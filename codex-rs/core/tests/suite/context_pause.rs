use anyhow::Result;
use codex_config::LoaderOverrides;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::config::ConfigBuilder;
use codex_features::Feature;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
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

fn builder() -> TestCodexBuilder {
    test_codex()
        .with_pre_build_hook(|home| {
            std::fs::write(
                home.join("config.toml"),
                concat!(
                    "model_auto_compact_enabled = false\n",
                    "model_context_pause_percent = 70\n",
                ),
            )
            .expect("write context policy");
        })
        .with_config(|config| {
            config.model_context_window = Some(200_000);
            config.update_plan_enabled = true;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config
                .features
                .disable(Feature::TokenBudget)
                .expect("disable token budget");
        })
}

fn plan_call(id: &str) -> Value {
    ev_function_call(
        id,
        "update_plan",
        &json!({
            "plan": [{"step": "Retain this step", "status": "completed"}]
        })
        .to_string(),
    )
}

async fn turn(test: &TestCodex, text: &str) -> Result<Value> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("unexpected error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(serde_json::to_value(event)?)
}

#[test_case(120_000, 70, false; "below threshold")]
#[test_case(134_000, 70, true; "uses usable window")]
#[test_case(134_000, 90, false; "higher threshold")]
#[test_case(96_000, 50, true; "lower threshold")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_stops_before_the_next_model_request(
    used: i64,
    percent: u8,
    paused: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![
            plan_call("retained"),
            ev_completed_with_tokens("r1", used),
        ]),
    )
    .await;
    let second = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Finished"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let test = builder()
        .with_config(move |config| {
            config.model_context_pause_percent = Some(percent);
        })
        .build_with_auto_env(&server)
        .await?;
    let event = turn(&test, "Do the work").await?;
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), usize::from(!paused));
    assert_eq!(event["context_pause"].is_object(), paused);
    if paused {
        assert_eq!(event["context_pause"]["context_window"], 190_000);
        assert_eq!(event["context_pause"]["threshold_percent"], percent);
        assert!(
            event["context_pause"]["used_tokens"]
                .as_i64()
                .expect("context usage")
                >= used
        );
        let rollout = std::fs::read_to_string(
            test.session_configured
                .rollout_path
                .as_ref()
                .expect("rollout path"),
        )?;
        assert!(
            rollout.contains("context_pause"),
            "pause is durable before delivery"
        );
        assert!(rollout.contains("Plan updated"));
    }
    Ok(())
}

#[test_case(125_000, false; "below threshold")]
#[test_case(134_000, true; "at threshold")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_counts_past_reasoning_once(used: i64, paused: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let reasoning = "x".repeat(400_000);
    let first = mount_sse_once(
        &server,
        sse(vec![
            ev_reasoning_item("reasoning", &[], &[reasoning.as_str()]),
            plan_call("first-tool"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 100_000),
        ]),
    )
    .await;
    let second = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Done"),
            ev_completed_with_tokens("r2", /*total_tokens*/ 120_000),
        ]),
    )
    .await;
    let third = mount_sse_once(
        &server,
        sse(vec![
            plan_call("second-tool"),
            ev_completed_with_tokens("r3", used),
        ]),
    )
    .await;
    let fourth = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m4", "Merged"),
            ev_completed("r4"),
        ]),
    )
    .await;
    let test = builder().build_with_auto_env(&server).await?;
    assert!(turn(&test, "Do the work").await?["context_pause"].is_null());
    assert_eq!(second.requests().len(), 1);
    let event = turn(&test, "merge it").await?;
    assert_eq!(first.requests().len(), 1);
    assert_eq!(third.requests().len(), 1);
    assert_eq!(fourth.requests().len(), usize::from(!paused));
    assert_eq!(event["context_pause"].is_object(), paused);
    if paused {
        let reported = event["context_pause"]["used_tokens"]
            .as_i64()
            .expect("context usage");
        assert!((used..used + 1_000).contains(&reported), "{reported}");
    }
    Ok(())
}

#[test_case("Write a handoff"; "handoff")]
#[test_case("Continue the work"; "ordinary request")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_preserves_all_tools_and_only_pauses_once(prompt: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![
            plan_call("first-tool"),
            plan_call("second-tool"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let second = mount_sse_once(
        &server,
        sse(vec![
            plan_call("third-tool"),
            ev_completed_with_tokens("r2", /*total_tokens*/ 140_000),
        ]),
    )
    .await;
    let third = mount_sse_once(
        &server,
        sse(vec![ev_assistant_message("m3", "Done"), ev_completed("r3")]),
    )
    .await;
    let test = builder().build_with_auto_env(&server).await?;
    assert!(turn(&test, "Retain my work").await?["context_pause"].is_object());
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), 0);
    assert!(turn(&test, prompt).await?["context_pause"].is_null());
    let resumed = second.single_request();
    for id in ["first-tool", "second-tool"] {
        assert_eq!(
            resumed.function_call_output_text(id).as_deref(),
            Some("Plan updated")
        );
    }
    assert!(
        resumed
            .message_input_texts("user")
            .ends_with(&["Retain my work".to_string(), prompt.to_string(),])
    );
    let last = third.single_request();
    assert_eq!(
        last.function_call_output_text("third-tool").as_deref(),
        Some("Plan updated")
    );
    assert!(last.inputs_of_type("compaction").is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_survives_restart_and_is_independent_per_session() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            plan_call("retained"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let mut factory = builder();
    let test = factory.build_with_auto_env(&server).await?;
    assert!(turn(&test, "First session").await?["context_pause"].is_object());
    let restarted = factory.restart(&server, &test).await?;
    assert!(
        restarted
            .codex
            .start_turn_if_idle(TurnInputRequest::user_input(Vec::new()),)
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit user input")
    );
    assert!(
        restarted
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(Vec::new()),)
            .await
            .unwrap_err()
            .to_string()
            .contains("explicit user input")
    );
    let after_restart = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Handoff"),
            ev_completed_with_tokens("r2", /*total_tokens*/ 140_000),
        ]),
    )
    .await;
    assert!(turn(&restarted, "Write a handoff").await?["context_pause"].is_null());
    assert_eq!(
        after_restart
            .single_request()
            .function_call_output_text("retained")
            .as_deref(),
        Some("Plan updated")
    );
    mount_sse_once(
        &server,
        sse(vec![
            plan_call("fresh"),
            ev_completed_with_tokens("r3", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let fresh = builder().build_with_auto_env(&server).await?;
    assert!(turn(&fresh, "Independent session").await?["context_pause"].is_object());
    Ok(())
}

async fn assert_idle_start_refused(test: &TestCodex) {
    let error = test
        .codex
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Queued work".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .expect_err("a paused thread refuses unattended starts");
    assert!(error.to_string().contains("explicit user input"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_outlasts_idle_starts_manual_compaction_and_restart() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            plan_call("retained"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let compact = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("summary", "Requested summary"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let mut factory = builder();
    let test = factory.build_with_auto_env(&server).await?;
    assert!(turn(&test, "Save my work").await?["context_pause"].is_object());
    assert_idle_start_refused(&test).await;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(compact.requests().len(), 1);
    assert_idle_start_refused(&test).await;
    let restarted = factory.restart(&server, &test).await?;
    assert_idle_start_refused(&restarted).await;
    let handoff = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m3", "Handoff"),
            ev_completed_with_tokens("r3", /*total_tokens*/ 20_000),
        ]),
    )
    .await;
    assert!(turn(&restarted, "Write a handoff").await?["context_pause"].is_null());
    assert_eq!(handoff.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_runs_stop_hooks_without_continuing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![
            plan_call("retained"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let continuation = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Continued"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let test = builder()
        .with_pre_build_hook(|home| {
            let script = home.join("stop_hook.py");
            std::fs::write(
                &script,
                concat!(
                    "from pathlib import Path\n",
                    "import json, sys\n",
                    "json.load(sys.stdin)\n",
                    "Path(__file__).with_suffix('.called').touch()\n",
                    "print(json.dumps({'decision': 'block', 'reason': 'Keep going'}))\n",
                ),
            )
            .expect("write stop hook");
            let python = if cfg!(windows) { "python" } else { "python3" };
            std::fs::write(
                home.join("hooks.json"),
                json!({"hooks": {"Stop": [{"hooks": [{
                    "type": "command",
                    "command": format!("{python} \"{}\"", script.display()),
                }]}]}})
                .to_string(),
            )
            .expect("write hooks");
        })
        .with_config(trust_discovered_hooks)
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Do the work".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut warnings = Vec::new();
    let event = wait_for_event(&test.codex, |event| {
        if let EventMsg::Warning(warning) = event {
            warnings.push(warning.message.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(serde_json::to_value(event)?["context_pause"].is_object());
    assert!(test.codex_home_path().join("stop_hook.called").exists());
    assert_eq!(
        warnings,
        vec!["Context paused; the Stop hook's continuation was not run.".to_string()]
    );
    assert_eq!(first.requests().len(), 1);
    assert!(continuation.requests().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_finishes_running_tools_and_preserves_accepted_input() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![
            ev_function_call("running", "record", "{}"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let follow_up = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Handoff"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let mut test = builder().build_with_auto_env(&server).await?;
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "record".into(),
                description: "Record work".into(),
                input_schema: json!({"type": "object", "properties": {}}),
                defer_loading: false,
            })],
            environments: Some(test.codex.environment_selections().await),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    test.codex = thread.thread;
    test.session_configured = thread.session_configured;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Do the work".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let EventMsg::DynamicToolCallRequest(request) = wait_for_event(&test.codex, |event| {
        assert!(
            !matches!(event, EventMsg::TurnComplete(_)),
            "tool must finish first"
        );
        matches!(event, EventMsg::DynamicToolCallRequest(_))
    })
    .await
    else {
        unreachable!()
    };
    assert!(matches!(
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Keep this accepted request".into(),
                text_elements: Vec::new()
            }]))
            .await?,
        TurnInputSubmission::Steered { .. }
    ));
    test.codex
        .submit(Op::DynamicToolResponse {
            id: request.call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "Saved result".into(),
                }],
                success: true,
            },
        })
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(serde_json::to_value(event)?["context_pause"].is_object());
    assert_eq!(first.requests().len(), 1);
    assert!(follow_up.requests().is_empty());
    assert!(turn(&test, "Write a handoff").await?["context_pause"].is_null());
    let request = follow_up.single_request();
    assert!(request.body_contains_text("Saved result"));
    assert!(request.message_input_texts("user").ends_with(&[
        "Do the work".to_string(),
        "Keep this accepted request".to_string(),
        "Write a handoff".to_string(),
    ]));
    Ok(())
}

#[test_case(0)]
#[test_case(100)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_rejects_invalid_threshold(percent: u8) -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!("model_context_pause_percent = {percent}\n"),
    )?;
    let error = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .expect_err("invalid threshold");
    assert!(error.to_string().contains("model_context_pause_percent"));
    Ok(())
}

#[test_case(true; "automatic compaction enabled")]
#[test_case(false; "pause not configured")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_pause_is_opt_in(auto_compact: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            plan_call("retained"),
            ev_completed_with_tokens("r1", /*total_tokens*/ 134_000),
        ]),
    )
    .await;
    let next = mount_sse_once(
        &server,
        sse(vec![ev_assistant_message("m2", "Done"), ev_completed("r2")]),
    )
    .await;
    let test = builder()
        .with_config(move |config| {
            config.model_auto_compact_enabled = auto_compact;
            if !auto_compact {
                config.model_context_pause_percent = None;
            }
        })
        .build_with_auto_env(&server)
        .await?;
    assert!(turn(&test, "Do the work").await?["context_pause"].is_null());
    assert_eq!(
        next.single_request()
            .function_call_output_text("retained")
            .as_deref(),
        Some("Plan updated")
    );
    Ok(())
}
