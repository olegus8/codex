use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_core::config::TokenBudgetConfig;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;

#[derive(Clone, Copy)]
enum Provider {
    Local,
    Remote,
}

fn no_auto_compact(provider: Provider) -> TestCodexBuilder {
    test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_pre_build_hook(|home| {
            std::fs::write(
                home.join("config.toml"),
                "model_auto_compact_enabled = false\n",
            )
            .expect("write context policy");
        })
        .with_config(move |config| {
            config.model_provider.name = match provider {
                Provider::Local => "Local context policy test",
                Provider::Remote => "OpenAI",
            }
            .to_string();
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_context_window = Some(200_000);
            config.update_plan_enabled = true;
            config.model_auto_compact_token_limit = Some(100_000);
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config
                .features
                .disable(Feature::TokenBudget)
                .expect("disable token budget");
        })
}

fn plan_call() -> Value {
    ev_function_call(
        "retained-plan",
        "update_plan",
        &json!({"plan": [{"step": "Retain this step", "status": "completed"}]}).to_string(),
    )
}

async fn submit(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    Ok(())
}

async fn expect_exhaustion(test: &TestCodex) {
    let mut errors = Vec::new();
    wait_for_event(&test.codex, |event| {
        match event {
            EventMsg::Error(error) => errors.push(error.codex_error_info.clone()),
            EventMsg::ItemStarted(event) => assert!(
                !matches!(
                    event.item,
                    codex_protocol::items::TurnItem::ContextCompaction(_)
                ),
                "exhaustion must not start compaction"
            ),
            _ => {}
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(errors, vec![Some(CodexErrorInfo::ContextWindowExceeded)]);
}

async fn retained_rollout(test: &TestCodex) -> Result<String> {
    test.codex.shutdown_and_wait().await?;
    let text = std::fs::read_to_string(
        test.session_configured
            .rollout_path
            .as_ref()
            .expect("rollout path"),
    )?;
    for line in text.lines() {
        let value: Value = serde_json::from_str(line)?;
        assert_ne!(value["type"], "compacted");
    }
    Ok(text)
}

#[test_case(Provider::Local, AutoCompactTokenLimitScope::Total; "local total")]
#[test_case(Provider::Remote, AutoCompactTokenLimitScope::Total; "remote total")]
#[test_case(Provider::Local, AutoCompactTokenLimitScope::BodyAfterPrefix; "local body")]
#[test_case(Provider::Remote, AutoCompactTokenLimitScope::BodyAfterPrefix; "remote body")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_retains_tools_and_accepts_handoff(
    provider: Provider,
    scope: AutoCompactTokenLimitScope,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![
            plan_call(),
            ev_completed_with_tokens("r1", /*total_tokens*/ 185_000),
        ]),
    )
    .await;
    let second = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Work preserved"),
            ev_completed_with_tokens("r2", /*total_tokens*/ 185_100),
        ]),
    )
    .await;
    let third = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m3", "Handoff"),
            ev_completed("r3"),
        ]),
    )
    .await;
    let test = no_auto_compact(provider)
        .with_pre_build_hook(|home| {
            let script = home.join("observe_compact.py");
            std::fs::write(&script, concat!(
                "from pathlib import Path\n",
                "Path(__file__).with_suffix('.called').touch()\n",
                "print('{\"continue\":false,\"stopReason\":\"Unexpected automatic compaction\"}')\n",
            )).expect("write hook");
            let python = if cfg!(windows) { "python" } else { "python3" };
            std::fs::write(home.join("hooks.json"), json!({"hooks": {
                "PreCompact": [{"matcher": "auto", "hooks": [{
                    "type": "command", "command": format!("{python} \"{}\"", script.display())
                }]}]
            }}).to_string()).expect("write hooks");
        })
        .with_config(trust_discovered_hooks)
        .with_config(move |config| {
            config.model_auto_compact_token_limit_scope = scope;
            config.model_auto_compact_token_limit = Some(10);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep my work").await?;
    test.submit_turn("Write my handoff").await?;

    assert_eq!(first.requests().len(), 1);
    let second_request = second.single_request();
    let third_request = third.single_request();
    assert_eq!(
        second_request
            .function_call_output_text("retained-plan")
            .as_deref(),
        Some("Plan updated")
    );
    assert_eq!(
        third_request.function_call_output("retained-plan"),
        second_request.function_call_output("retained-plan")
    );
    assert!(
        third_request
            .message_input_texts("user")
            .contains(&"Keep my work".to_string())
    );
    assert!(
        third_request
            .message_input_texts("user")
            .contains(&"Write my handoff".to_string())
    );
    assert!(third_request.body_contains_text("Work preserved"));
    assert!(third_request.inputs_of_type("compaction").is_empty());
    assert!(
        !third_request
            .body_json()
            .to_string()
            .contains(SUMMARIZATION_PROMPT)
    );
    assert!(
        !test
            .codex_home_path()
            .join("observe_compact.called")
            .exists()
    );
    retained_rollout(&test).await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Exhaustion {
    NextTurn,
    ToolResult,
    Provider,
    IncomingInput,
}

#[test_case(Exhaustion::NextTurn; "next turn")]
#[test_case(Exhaustion::ToolResult; "tool result")]
#[test_case(Exhaustion::Provider; "provider error")]
#[test_case(Exhaustion::IncomingInput; "incoming input")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_stops_at_exhaustion_and_preserves_input(cause: Exhaustion) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let response = match cause {
        Exhaustion::NextTurn => sse(vec![
            ev_assistant_message("m1", "Last answer"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 190_000),
        ]),
        Exhaustion::ToolResult => sse(vec![
            plan_call(),
            ev_completed_with_tokens("r1", /*total_tokens*/ 189_999),
        ]),
        Exhaustion::Provider => sse_failed("r1", "context_length_exceeded", "Context is full"),
        Exhaustion::IncomingInput => sse(vec![ev_completed("unexpected")]),
    };
    let mock = mount_sse_once(&server, response).await;
    let test = no_auto_compact(Provider::Remote)
        .build_with_auto_env(&server)
        .await?;
    match cause {
        Exhaustion::NextTurn => test.submit_turn("Fill the window").await?,
        Exhaustion::ToolResult | Exhaustion::Provider => {
            submit(&test, "Fill the window").await?;
            expect_exhaustion(&test).await;
        }
        Exhaustion::IncomingInput => {
            submit(&test, &"large input ".repeat(100_000)).await?;
            expect_exhaustion(&test).await;
        }
    }
    for prompt in ["Preserve this handoff request", "Preserve this retry too"] {
        submit(&test, prompt).await?;
        expect_exhaustion(&test).await;
    }
    assert_eq!(
        mock.requests().len(),
        match cause {
            Exhaustion::IncomingInput => 0,
            Exhaustion::NextTurn | Exhaustion::ToolResult | Exhaustion::Provider => 1,
        }
    );
    let rollout = retained_rollout(&test).await?;
    assert!(rollout.contains("Preserve this handoff request"));
    assert!(rollout.contains("Preserve this retry too"));
    if matches!(cause, Exhaustion::ToolResult) {
        assert!(rollout.contains("retained-plan"));
        assert!(rollout.contains("Plan updated"));
    }
    Ok(())
}

#[test_case(185_000; "smaller window still fits")]
#[test_case(195_000; "smaller window exhausted")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_model_change_preserves_history(used: i64) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m1", "Before model change"),
            ev_completed_with_tokens("r1", used),
        ]),
    )
    .await;
    let second = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "After model change"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let test = no_auto_compact(Provider::Remote)
        .with_model_info_override("gpt-5.5", |model| {
            model.context_window = Some(250_000);
            model.comp_hash = Some("original-hash".to_string());
        })
        .with_model_info_override("gpt-5.4", |model| {
            model.context_window = Some(200_000);
            model.comp_hash = Some("changed-hash".to_string());
        })
        .with_config(|config| {
            config.model = Some("gpt-5.5".to_string());
            config.model_context_window = None;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("First model").await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "New model handoff".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: "gpt-5.4".to_string(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    if used >= 190_000 {
        expect_exhaustion(&test).await;
        assert!(second.requests().is_empty());
    } else {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let request = second.single_request();
        assert_eq!(request.body_json()["model"], "gpt-5.4");
        assert!(request.body_contains_text("Before model change"));
        assert!(
            request
                .message_input_texts("user")
                .contains(&"New model handoff".to_string())
        );
        assert!(request.inputs_of_type("compaction").is_empty());
    }
    assert_eq!(first.requests().len(), 1);
    let rollout = retained_rollout(&test).await?;
    assert!(rollout.contains("New model handoff"));
    Ok(())
}

#[test_case(Provider::Local; "local")]
#[test_case(Provider::Remote; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_keeps_manual_compaction(provider: Provider) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m1", "Before manual compaction"),
            ev_completed("r1"),
        ]),
    )
    .await;
    let summary = match provider {
        Provider::Local => ev_assistant_message("summary", "Requested summary"),
        Provider::Remote => json!({"type": "response.output_item.done", "item": {
            "type": "compaction", "encrypted_content": "requested-summary"
        }}),
    };
    let compact = mount_sse_once(&server, sse(vec![summary, ev_completed("r2")])).await;
    let test = no_auto_compact(provider)
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep until I compact").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(compact.requests().len(), 1);
    test.codex.shutdown_and_wait().await?;
    let rollout = std::fs::read_to_string(test.session_configured.rollout_path.expect("rollout"))?;
    assert!(rollout.lines().any(|line| {
        let value: Value = serde_json::from_str(line).expect("parse rollout line");
        value["type"] == "compacted"
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_refuses_model_requested_reset() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_function_call("reset", "new_context", "{}"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 185_000),
        ]),
    )
    .await;
    let continuation = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Continuing with original history"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let test = no_auto_compact(Provider::Remote)
        .with_config(|config| {
            config
                .features
                .enable(Feature::TokenBudget)
                .expect("enable token budget");
            config.token_budget = Some(TokenBudgetConfig {
                auto_compact_fallback_prompt: Some("Prepare automatic reset".to_string()),
                auto_compact_fallback_buffer_tokens: Some(20_000),
                ..TokenBudgetConfig::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Retain this original request").await?;
    let request = continuation.single_request();
    assert_eq!(
        request.function_call_output_text("reset").as_deref(),
        Some("Automatic context resets are disabled.")
    );
    assert!(
        request
            .message_input_texts("user")
            .contains(&"Retain this original request".to_string())
    );
    assert!(
        !request
            .body_json()
            .to_string()
            .contains("Prepare automatic reset")
    );
    retained_rollout(&test).await?;
    Ok(())
}
