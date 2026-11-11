use super::*;
use codex_app_server_protocol::ContextPause;
use codex_app_server_protocol::ContextWindowUsage;
use codex_app_server_protocol::ThreadItem as AppServerThreadItem;
use pretty_assertions::assert_eq;

fn paused_turn() -> AppServerTurn {
    let mut turn = app_server_turn(
        "pause",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    turn.context_pause = Some(ContextPause {
        used_tokens: 133_000,
        context_window: 190_000,
        threshold_percent: 70,
    });
    turn
}

fn complete_turn(chat: &mut ChatWidget, turn: AppServerTurn) {
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: chat.thread_id.unwrap().to_string(),
            turn,
        }),
        /*replay_kind*/ None,
    );
}

fn submit(chat: &mut ChatWidget, text: &str) {
    chat.handle_composer_input_result(
        InputResult::Submitted {
            text: text.into(),
            text_elements: Vec::new(),
        },
        /*had_modal_or_popup*/ false,
    );
}

#[tokio::test]
async fn context_policy_pause_preserves_queue_and_accepts_handoff() {
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "pause");
    chat.queue_user_message("Queued work".into());
    complete_turn(&mut chat, paused_turn());
    assert!(!chat.is_user_turn_pending_or_running());
    assert_eq!(chat.queued_user_message_texts(), ["Queued work"]);
    assert!(ops.try_recv().is_err());
    assert!(!chat.maybe_send_next_queued_input());
    let history: Vec<_> = drain_insert_history_normalized(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    assert_chatwidget_snapshot!("context_policy_paused", lines_to_single_string(&history));
    submit(&mut chat, "Write a handoff");
    assert_matches!(next_submit_op(&mut ops), Op::UserTurn { .. });
    assert_eq!(chat.queued_user_message_texts(), ["Queued work"]);
}

#[tokio::test]
async fn context_policy_usage_uses_runtime_accounting() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.status_line = Some(vec!["context-remaining".into()]);
    let mut info = make_token_info(/*total_tokens*/ 120_000, /*context_window*/ 200_000);
    info.context_window_usage = Some(ContextWindowUsage {
        used_tokens: 133_000,
        context_window: 190_000,
    });
    handle_token_count(&mut chat, Some(info));
    assert_eq!(chat.status_line_context_remaining_percent(), Some(30));
    assert_eq!(chat.status_line_context_used_percent(), Some(70));
    assert_eq!(chat.status_line_context_window_size(), Some(190_000));
    chat.refresh_status_line();
    assert_chatwidget_snapshot!(
        "context_policy_usage_footer",
        render_bottom_popup(&chat, /*width*/ 80),
    );
    chat.dispatch_command(SlashCommand::Status);
    let history: Vec<_> = drain_insert_history(&mut rx).into_iter().flatten().collect();
    let status = lines_to_single_string(&history);
    assert!(status.contains("30% left"), "{status}");
    assert!(status.contains("133K used / 190K"), "{status}");
}

#[tokio::test]
async fn context_policy_exhaustion_keeps_queued_work() {
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "turn-1");
    let mut info = make_token_info(/*total_tokens*/ 200_000, /*context_window*/ 190_000);
    info.context_window_usage = Some(ContextWindowUsage {
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
    assert_eq!(chat.queued_user_message_texts(), ["Queued work"]);
    let history: Vec<_> = drain_insert_history(&mut rx).into_iter().flatten().collect();
    assert_chatwidget_snapshot!("context_policy_exhausted", lines_to_single_string(&history));
}

#[tokio::test]
async fn context_policy_pause_replays_until_a_later_user_message() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let mut pause = paused_turn();
    pause.items_view = codex_app_server_protocol::TurnItemsView::NotLoaded;
    let mut compaction = app_server_turn(
        "compaction",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    compaction.items_view = codex_app_server_protocol::TurnItemsView::NotLoaded;
    chat.replay_thread_turns(vec![pause, compaction], ReplayKind::ResumeInitialMessages);
    assert!(chat.input_queue.context_input_required);
    let history: Vec<_> = drain_insert_history(&mut rx).into_iter().flatten().collect();
    assert!(lines_to_single_string(&history).contains("request a handoff"));

    let mut handoff = app_server_turn(
        "handoff",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    handoff.items = vec![AppServerThreadItem::untimed(AppServerThreadItemPayload::UserMessage {
        id: "handoff-request".into(),
        client_id: None,
        content: Vec::new(),
    })];
    chat.replay_thread_turns(vec![handoff], ReplayKind::ThreadSnapshot);
    assert!(!chat.input_queue.context_input_required);
}

#[tokio::test]
async fn context_policy_pause_holds_queue_through_shell_commands() {
    let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "pause");
    chat.queue_user_message("Queued work".into());
    complete_turn(&mut chat, paused_turn());

    submit(&mut chat, "!pwd");
    assert_matches!(ops.try_recv(), Ok(Op::RunUserShellCommand { command }) if command == "pwd");
    handle_turn_started(&mut chat, "shell");
    complete_turn(
        &mut chat,
        app_server_turn(
            "shell",
            AppServerTurnStatus::Completed,
            /*duration_ms*/ None,
            /*error*/ None,
        ),
    );
    assert!(!chat.maybe_send_next_queued_input());
    assert!(ops.try_recv().is_err());
    assert_eq!(chat.queued_user_message_texts(), ["Queued work"]);
}
