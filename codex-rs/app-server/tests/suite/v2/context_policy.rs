use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;

#[test_case(120_000, "completed", false, ThreadHistoryMode::Legacy; "below threshold")]
#[test_case(134_000, "completed", true, ThreadHistoryMode::Legacy; "pause and handoff")]
#[test_case(134_000, "completed", true, ThreadHistoryMode::Paginated; "paginated pause")]
#[test_case(200_000, "failed", false, ThreadHistoryMode::Legacy; "exhaustion")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_public_outcomes(
    used: i64,
    status: &str,
    paused: bool,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let first = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_function_call(
                "retained",
                "update_plan",
                &json!({"plan": [{"step": "Save work", "status": "completed"}]}).to_string(),
            ),
            responses::ev_completed_with_tokens("r1", used),
        ]),
    )
    .await;
    let follow_up = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("m2", "Handoff"),
            responses::ev_completed_with_tokens("r2", used.min(140_000)),
        ]),
    )
    .await;
    let home = tempfile::tempdir()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config(concat!(
            "model_auto_compact_enabled = false\n",
            "model_context_pause_percent = 70\n",
            "model_context_window = 200000\n",
            "tools.update_plan.enabled = true\n",
        ))
        .with_provider_config("supports_websockets = false")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".into()),
            history_mode: Some(history_mode),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(start).await?;
    let input = |text: &str| TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    };
    let completed = app
        .start_turn_and_wait_for_completion(input("Save my work"))
        .await?;
    let turn = serde_json::to_value(&completed.turn)?;
    assert_eq!(turn["status"], status);
    assert_eq!(turn["contextPause"].is_object(), paused);
    assert_eq!(first.requests().len(), 1);
    assert_eq!(follow_up.requests().len(), usize::from(used < 134_000));
    if paused {
        let pause = &turn["contextPause"];
        assert_eq!(pause["contextWindow"], 190_000);
        assert_eq!(pause["thresholdPercent"], 70);
        assert!(pause["usedTokens"].as_i64().expect("usage") >= used);
        let usage = app
            .read_stream_until_matching_notification("runtime context usage", |event| {
                event.method == "thread/tokenUsage/updated"
                    && event.params.as_ref().is_some_and(|params| {
                        params["tokenUsage"]["contextWindowUsage"]["usedTokens"]
                            == pause["usedTokens"]
                    })
            })
            .await?;
        assert_eq!(
            usage.params.expect("usage")["tokenUsage"]["contextWindowUsage"],
            json!({"usedTokens": pause["usedTokens"], "contextWindow": 190_000}),
        );
        let saved_pause = if history_mode == ThreadHistoryMode::Paginated {
            let read = app
                .send_thread_turns_list_request(ThreadTurnsListParams {
                    thread_id: thread.id.clone(),
                    cursor: None,
                    limit: None,
                    sort_direction: None,
                    items_view: None,
                })
                .await?;
            let saved: Value = app.read_response(read).await?;
            saved["data"][0]["contextPause"].clone()
        } else {
            let read = app
                .send_thread_read_request(ThreadReadParams {
                    thread_id: thread.id.clone(),
                    include_turns: true,
                })
                .await?;
            let saved: Value = app.read_response(read).await?;
            saved["thread"]["turns"][0]["contextPause"].clone()
        };
        assert_eq!(saved_pause, *pause);
        let handoff = app
            .start_turn_and_wait_for_completion(input("Write a handoff"))
            .await?;
        assert!(serde_json::to_value(handoff.turn)?["contextPause"].is_null());
    } else if status == "failed" {
        assert_eq!(turn["error"]["codexErrorInfo"], "contextWindowExceeded");
    }
    if status == "completed" {
        let request = follow_up.single_request();
        assert_eq!(
            request.function_call_output_text("retained").as_deref(),
            Some("Plan updated"),
        );
        assert!(request.inputs_of_type("compaction").is_empty());
    }
    Ok(())
}
