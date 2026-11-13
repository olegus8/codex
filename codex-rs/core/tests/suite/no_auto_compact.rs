use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::TokenBudgetConfig;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

fn no_auto_compact() -> TestCodexBuilder {
    test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_pre_build_hook(|home| {
            std::fs::write(
                home.join("config.toml"),
                "model_auto_compact_enabled = false\n",
            )
            .expect("write compaction config");
        })
        .with_config(|config| {
            config.features.disable(Feature::TokenBudget).unwrap();
        })
}

#[test_case(false; "context downshift")]
#[test_case(true; "compaction hash change")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_model_change_preserves_history(hash_change: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("before", "Before model change"),
                ev_completed_with_tokens("before", /*total_tokens*/ 80_000),
            ]),
            sse(vec![
                ev_assistant_message("after", "After model change"),
                ev_completed("after"),
            ]),
        ],
    )
    .await;
    let test = no_auto_compact()
        .with_model_info_override("gpt-5.4", |model| {
            model.context_window = Some(250_000);
            model.comp_hash = Some("old-hash".to_string());
        })
        .with_model_info_override("gpt-5.2", move |model| {
            let (window, hash) = if hash_change {
                (250_000, "new-hash")
            } else {
                (100_000, "old-hash")
            };
            model.context_window = Some(window);
            model.comp_hash = Some(hash.to_string());
        })
        .with_model("gpt-5.4")
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("First model").await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "New model handoff".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                model: Some("gpt-5.2".to_string()),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let request = &requests[1];
    assert_eq!(request.body_json()["model"], "gpt-5.2");
    assert!(request.body_contains_text("Before model change"));
    assert!(request.body_contains_text("New model handoff"));
    assert!(request.inputs_of_type("compaction_trigger").is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_keeps_manual_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("before", "Before manual compaction"),
                ev_completed("before"),
            ]),
            sse(vec![
                json!({"type": "response.output_item.done", "item": {
                    "type": "compaction", "encrypted_content": "requested-summary"
                }}),
                ev_completed("compact"),
            ]),
        ],
    )
    .await;
    let test = no_auto_compact().build_with_auto_env(&server).await?;
    test.submit_text_turn("Keep until I compact").await?;
    test.codex.submit(Op::Compact).await?;
    let mut completed_compaction = false;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::ItemCompleted(event) = event {
            completed_compaction |= matches!(event.item, TurnItem::ContextCompaction(_));
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].inputs_of_type("compaction_trigger").is_empty());
    assert!(completed_compaction);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_refuses_model_requested_reset() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call("remaining", "get_context_remaining", "{}"),
                ev_completed_with_tokens("before-reset", /*total_tokens*/ 9_500),
            ]),
            sse(vec![
                ev_function_call("reset", "new_context", "{}"),
                ev_completed_with_tokens("reset", /*total_tokens*/ 13_500),
            ]),
            sse(vec![
                ev_assistant_message("after", "Continuing with original history"),
                ev_completed("after"),
            ]),
        ],
    )
    .await;
    let test = no_auto_compact()
        .with_config(|config| {
            config.model_context_window = Some(50_000);
            config.model_auto_compact_token_limit = Some(9_000);
            config.token_budget = Some(TokenBudgetConfig {
                auto_compact_fallback_prompt: Some("Prepare automatic reset".to_string()),
                auto_compact_fallback_buffer_tokens: Some(4_000),
                ..TokenBudgetConfig::default()
            });
            config.features.enable(Feature::TokenBudget).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Retain this original request")
        .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[1].body_contains_text("Prepare automatic reset"));
    assert_eq!(
        requests[2].function_call_output_text("reset").as_deref(),
        Some("Context resets are disabled for this thread.")
    );
    assert!(requests[2].body_contains_text("Retain this original request"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_keeps_history_over_turn_limits() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call(
                    "plan",
                    "update_plan",
                    r#"{"plan":[{"step":"Keep this step","status":"completed"}]}"#,
                ),
                ev_completed_with_tokens("plan", /*total_tokens*/ 70_000),
            ]),
            sse(vec![
                ev_assistant_message("first", "First turn complete"),
                ev_completed_with_tokens("first", /*total_tokens*/ 70_100),
            ]),
            sse(vec![
                ev_assistant_message("second", "Second turn complete"),
                ev_completed("second"),
            ]),
        ],
    )
    .await;
    let test = no_auto_compact()
        .with_config(|config| {
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(60_000);
            config.model_post_turn_compact_threshold_percent = 50;
            config.update_plan_enabled = true;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Retain my plan").await?;
    test.submit_text_turn("Continue the same plan").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(request.inputs_of_type("compaction_trigger").is_empty());
        assert_eq!(
            request.function_call_output_text("plan").as_deref(),
            Some("Plan updated")
        );
    }
    assert!(requests[2].body_contains_text("First turn complete"));
    Ok(())
}

#[test_case(false; "unset preserves background compaction")]
#[test_case(true; "disabled skips background compaction")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_background_compaction(disabled: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mut bodies = vec![sse(vec![
        ev_assistant_message("answer", "First turn complete"),
        ev_completed_with_tokens("answer", /*total_tokens*/ 70_000),
    ])];
    if !disabled {
        bodies.push(sse(vec![
            json!({"type": "response.output_item.done", "item": {
                "type": "compaction", "encrypted_content": "background-summary"
            }}),
            ev_completed("background"),
        ]));
    }
    let responses = mount_sse_sequence(&server, bodies).await;
    let builder = if disabled {
        no_auto_compact()
    } else {
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
    };
    let test = builder
        .with_model("gpt-5.4")
        .with_config(|config| {
            config.features.disable(Feature::TokenBudget).unwrap();
            config.features.enable(Feature::AsyncCompaction).unwrap();
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(90_000);
            config.model_post_turn_compact_threshold_percent = 0;
            config.async_compaction_threshold_percent = 65;
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Keep this history").await?;
    test.codex.wait_for_background_compaction_for_test().await;

    let requests = responses.requests();
    assert_eq!(requests.len(), if disabled { 1 } else { 2 });
    if !disabled {
        assert!(!requests[1].inputs_of_type("compaction_trigger").is_empty());
    }
    Ok(())
}
