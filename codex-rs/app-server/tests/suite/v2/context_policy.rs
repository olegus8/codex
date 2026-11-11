use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::CodexErrorInfo;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadTimelineEntry;
use codex_app_server_protocol::ThreadTimelineListParams;
use codex_app_server_protocol::ThreadTimelineListResponse;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(Some(120_000), false, ThreadHistoryMode::Legacy; "below threshold")]
#[test_case(Some(134_000), true, ThreadHistoryMode::Legacy; "pause and handoff")]
#[test_case(Some(134_000), true, ThreadHistoryMode::Paginated; "paginated pause")]
#[test_case(None, false, ThreadHistoryMode::Legacy; "provider exhaustion")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_public_outcomes(
    used: Option<i64>,
    paused: bool,
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let first_stream = match used {
        Some(used) => responses::sse(vec![
            responses::ev_function_call(
                "retained",
                "update_plan",
                r#"{"plan":[{"step":"Save work","status":"completed"}]}"#,
            ),
            responses::ev_completed_with_tokens("r1", used),
        ]),
        None => responses::sse_failed("r1", "context_length_exceeded", "Context limit exceeded"),
    };
    let first = responses::mount_sse_once(&server, first_stream).await;
    let follow_up = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("m2", "Handoff"),
            responses::ev_completed_with_tokens("r2", 125_000),
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
    let completed = app.start_turn_and_wait_for_completion(input("Save my work")).await?;
    assert_eq!(first.requests().len(), 1);
    assert_eq!(follow_up.requests().len(), usize::from(used.is_some() && !paused));
    assert_eq!(completed.turn.context_pause.is_some(), paused);

    let expected_usage = completed
        .turn
        .context_pause
        .as_ref()
        .map(|pause| pause.used_tokens)
        .unwrap_or_else(|| used.unwrap_or(190_000));
    let notification = app
        .read_stream_until_matching_notification("runtime context usage", |event| {
            event.method == "thread/tokenUsage/updated"
                && event.params.as_ref().is_some_and(|params| {
                    params["tokenUsage"]["contextWindowUsage"]["usedTokens"]
                        .as_i64()
                        .is_some_and(|tokens| tokens >= expected_usage)
                })
        })
        .await?;
    let usage: ThreadTokenUsageUpdatedNotification =
        serde_json::from_value(notification.params.expect("token usage"))?;
    let usage = usage.token_usage.context_window_usage.expect("usable window usage");
    assert_eq!(usage.context_window, 190_000);

    if used.is_none() {
        assert_eq!(completed.turn.status, TurnStatus::Failed);
        assert_eq!(usage.used_tokens, 190_000);
        assert_eq!(
            completed.turn.error.expect("exhaustion").codex_error_info,
            Some(CodexErrorInfo::ContextWindowExceeded)
        );
        return Ok(());
    }

    assert_eq!(completed.turn.status, TurnStatus::Completed);
    if let Some(pause) = completed.turn.context_pause {
        assert_eq!(usage.used_tokens, pause.used_tokens);
        assert_eq!(usage.context_window, pause.context_window);
        assert_eq!(pause.threshold_percent, 70);
        if history_mode == ThreadHistoryMode::Paginated {
            let read = app
                .send_thread_turns_list_request(ThreadTurnsListParams {
                    thread_id: thread.id.clone(),
                    cursor: None,
                    limit: None,
                    sort_direction: None,
                    items_view: None,
                })
                .await?;
            let saved: ThreadTurnsListResponse = app.read_response(read).await?;
            assert_eq!(saved.data[0].context_pause.as_ref(), Some(&pause));
            let timeline = app
                .send_thread_timeline_list_request(ThreadTimelineListParams {
                    thread_id: thread.id.clone(),
                    cursor: None,
                    limit: None,
                })
                .await?;
            let timeline: ThreadTimelineListResponse = app.read_response(timeline).await?;
            assert!(timeline.data.iter().any(|entry| matches!(
                entry,
                ThreadTimelineEntry::TurnCompleted {
                    turn_id,
                    context_pause: Some(saved),
                    ..
                } if turn_id == &completed.turn.id && saved == &pause
            )));
        } else {
            let read = app
                .send_thread_read_request(ThreadReadParams {
                    thread_id: thread.id.clone(),
                    include_turns: true,
                })
                .await?;
            let saved: ThreadReadResponse = app.read_response(read).await?;
            assert_eq!(saved.thread.turns[0].context_pause.as_ref(), Some(&pause));
        }
        let handoff = app
            .start_turn_and_wait_for_completion(input("Write a handoff"))
            .await?;
        assert!(handoff.turn.context_pause.is_none());
    }
    let request = follow_up.single_request();
    assert_eq!(
        request.function_call_output_text("retained").as_deref(),
        Some("Plan updated"),
    );
    assert!(request.inputs_of_type("compaction").is_empty());
    Ok(())
}
