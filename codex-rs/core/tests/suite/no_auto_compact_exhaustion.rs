use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
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
use test_case::test_case;
use wiremock::MockServer;

const USABLE_WINDOW: i64 = 190_000;

fn no_auto_compact() -> TestCodexBuilder {
    test_codex()
        .with_model("gpt-5.5")
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_pre_build_hook(|home| {
            std::fs::write(
                home.join("config.toml"),
                "model_auto_compact_enabled = false\n",
            )
            .expect("write compaction config");
        })
        .with_config(|config| {
            config.model_context_window = Some(200_000);
            config.model_auto_compact_token_limit = Some(10);
            config.model_post_turn_compact_threshold_percent = 50;
            config.update_plan_enabled = true;
            config.features.disable(Feature::TokenBudget).unwrap();
        })
}

fn plan_call() -> Value {
    ev_function_call(
        "retained-plan",
        "update_plan",
        r#"{"plan":[{"step":"Retain this step","status":"completed"}]}"#,
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
            EventMsg::ItemStarted(event) => {
                assert!(!matches!(event.item, TurnItem::ContextCompaction(_)));
            }
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
        let entry: Value = serde_json::from_str(line)?;
        assert_ne!(entry["type"], "compacted");
    }
    Ok(text)
}

async fn sampling_request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .count()
}

#[test_case(AutoCompactTokenLimitScope::Total; "total")]
#[test_case(AutoCompactTokenLimitScope::BodyAfterPrefix; "body after prefix")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auto_compact_retains_tools_and_accepts_handoff(
    scope: AutoCompactTokenLimitScope,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                plan_call(),
                ev_completed_with_tokens("r1", /*total_tokens*/ 185_000),
            ]),
            sse(vec![
                ev_assistant_message("m2", "Work preserved"),
                ev_completed_with_tokens("r2", /*total_tokens*/ 185_100),
            ]),
            sse(vec![
                ev_assistant_message("m3", "Handoff"),
                ev_completed("r3"),
            ]),
        ],
    )
    .await;
    let test = no_auto_compact()
        .with_config(move |config| config.model_auto_compact_token_limit_scope = scope)
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("Keep my work").await?;
    test.submit_text_turn("Write my handoff").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1]
            .function_call_output_text("retained-plan")
            .as_deref(),
        Some("Plan updated"),
    );
    assert_eq!(
        requests[2].function_call_output("retained-plan"),
        requests[1].function_call_output("retained-plan"),
    );
    for text in ["Keep my work", "Work preserved", "Write my handoff"] {
        assert!(requests[2].body_contains_text(text));
    }
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
            ev_completed_with_tokens("r1", USABLE_WINDOW),
        ]),
        Exhaustion::ToolResult => sse(vec![
            plan_call(),
            ev_completed_with_tokens("r1", USABLE_WINDOW - 1),
        ]),
        Exhaustion::Provider => sse_failed("r1", "context_length_exceeded", "Context is full"),
        Exhaustion::IncomingInput => sse(vec![ev_completed("unexpected")]),
    };
    mount_sse_once(&server, response).await;
    let test = no_auto_compact().build_with_auto_env(&server).await?;
    match cause {
        Exhaustion::NextTurn => test.submit_text_turn("Fill the window").await?,
        Exhaustion::ToolResult | Exhaustion::Provider => {
            submit(&test, "Fill the window").await?;
            expect_exhaustion(&test).await;
        }
        Exhaustion::IncomingInput => {
            submit(&test, &"large input ".repeat(100_000)).await?;
            expect_exhaustion(&test).await;
        }
    }
    for prompt in ["Preserve this handoff request", "Preserve this follow-up too"] {
        submit(&test, prompt).await?;
        expect_exhaustion(&test).await;
    }
    assert_eq!(
        sampling_request_count(&server).await,
        usize::from(!matches!(cause, Exhaustion::IncomingInput)),
    );
    let rollout = retained_rollout(&test).await?;
    assert!(rollout.contains("Preserve this handoff request"));
    assert!(rollout.contains("Preserve this follow-up too"));
    if matches!(cause, Exhaustion::ToolResult) {
        assert!(rollout.contains("retained-plan"));
        assert!(rollout.contains("Plan updated"));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_provider_exhaustion_after_usage_stays_full() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("m1", "Keep working"),
                ev_completed_with_tokens("r1", /*total_tokens*/ 100_000),
            ]),
            sse(vec![
                ev_assistant_message("m2", "Still working"),
                ev_completed_with_tokens("r2", /*total_tokens*/ 110_000),
            ]),
            sse_failed("r3", "context_length_exceeded", "Context is full"),
        ],
    )
    .await;
    let test = no_auto_compact().build_with_auto_env(&server).await?;
    test.submit_text_turn("First turn").await?;
    test.submit_text_turn("Second turn").await?;
    let reported = test.codex.token_usage_info().await.expect("reported usage");

    for prompt in ["Fill the window", "Preserve this handoff"] {
        submit(&test, prompt).await?;
        expect_exhaustion(&test).await;
        let usage = test.codex.token_usage_info().await.expect("usage");
        assert_eq!(usage.total_token_usage, reported.total_token_usage);
        assert_eq!(usage.model_context_window, Some(USABLE_WINDOW));
        assert!(usage.last_token_usage.total_tokens >= USABLE_WINDOW);
    }
    assert_eq!(sampling_request_count(&server).await, 3);
    assert!(
        retained_rollout(&test)
            .await?
            .contains("Preserve this handoff")
    );
    Ok(())
}
