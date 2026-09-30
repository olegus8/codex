use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn context_policy_pause_preserves_queue_and_accepts_handoff() {
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "turn-1");
    chat.queue_user_message("Queued work".into());
    let mut turn = serde_json::to_value(app_server_turn(
        "turn-1",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    ))
    .unwrap();
    turn["contextPause"] = serde_json::json!({
        "usedTokens": 133000,
        "contextWindow": 190000,
        "thresholdPercent": 70,
    });
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".into(),
            turn: serde_json::from_value(turn).unwrap(),
        }),
        /*replay_kind*/ None,
    );
    assert!(!chat.is_user_turn_pending_or_running());
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);
    assert!(ops.try_recv().is_err());
    assert!(!chat.maybe_send_next_queued_input());
    let history: Vec<_> = drain_insert_history_normalized(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    assert_chatwidget_snapshot!("context_policy_paused", lines_to_single_string(&history));
    chat.handle_composer_input_result(
        InputResult::Submitted {
            text: "Write a handoff".into(),
            text_elements: Vec::new(),
        },
        /*had_modal_or_popup*/ false,
    );
    assert_matches!(next_submit_op(&mut ops), Op::UserTurn { .. });
}

#[tokio::test]
async fn context_policy_usage_uses_runtime_accounting() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.status_line = Some(vec!["context-remaining".into()]);
    let mut info = serde_json::to_value(make_token_info(
        /*total_tokens*/ 120_000, /*context_window*/ 200_000,
    ))
    .unwrap();
    info["context_window_usage"] = serde_json::json!({
        "usedTokens": 133000, "contextWindow": 190000,
    });
    chat.set_token_info(Some(serde_json::from_value(info).unwrap()));
    assert_eq!(chat.status_line_context_remaining_percent(), Some(30));
    assert_eq!(chat.status_line_context_used_percent(), Some(70));
    assert_eq!(chat.status_line_context_window_size(), Some(190_000));
    chat.refresh_status_line();
    assert_chatwidget_snapshot!(
        "context_policy_usage_footer",
        render_bottom_popup(&chat, /*width*/ 80),
    );
    chat.dispatch_command(SlashCommand::Status);
    let history: Vec<_> = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    let status = lines_to_single_string(&history);
    assert!(status.contains("30% left"), "{status}");
    assert!(status.contains("133K used / 190K"), "{status}");
}

#[tokio::test]
async fn context_policy_exhaustion_keeps_queued_work() {
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "turn-1");
    let mut info = make_token_info(
        /*total_tokens*/ 200_000, /*context_window*/ 190_000,
    );
    info.context_window_usage = Some(codex_app_server_protocol::ContextWindowUsage {
        used_tokens: 200_000,
        context_window: 190_000,
    });
    chat.set_token_info(Some(info));
    chat.queue_user_message("Queued work".into());
    chat.handle_non_retry_error(
        "Context exhausted".into(),
        Some(CodexErrorInfo::ContextWindowExceeded),
    );
    assert!(!chat.maybe_send_next_queued_input());
    assert!(ops.try_recv().is_err());
    assert_eq!(chat.input_queue.queued_user_messages.len(), 1);
    let history: Vec<_> = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    assert_chatwidget_snapshot!("context_policy_exhausted", lines_to_single_string(&history));
}

#[tokio::test]
async fn context_policy_pause_replays_until_a_later_turn() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut turn = app_server_turn(
        "pause",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    turn.context_pause = Some(codex_app_server_protocol::ContextPause {
        used_tokens: 133_000,
        context_window: 190_000,
        threshold_percent: 70,
    });
    chat.replay_thread_turns(vec![turn], ReplayKind::ThreadSnapshot);
    assert!(chat.input_queue.context_input_required);
    let history: Vec<_> = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    assert!(lines_to_single_string(&history).contains("request a handoff"));
    let mut later = app_server_turn(
        "later",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    later.items_view = codex_app_server_protocol::TurnItemsView::NotLoaded;
    chat.replay_thread_turns(vec![later], ReplayKind::ThreadSnapshot);
    assert!(!chat.input_queue.context_input_required);
}
