use super::ContextPauseState;
use codex_history::CompactedItem;
use codex_history::CompactionContextPause;
use codex_history::CompactionResumeMetadata;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::ContextPause;
use codex_protocol::protocol::EnteredReviewModeEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExitedReviewModeEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;

fn compaction(pause: Option<CompactionContextPause>) -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: String::new(),
        replacement_history: Some(Vec::new()),
        retained_context: None,
        guardian_history: None,
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: Some(CompactionResumeMetadata {
            multi_agent_version: None,
            last_started_turn_id: None,
            previous_turn_settings: None,
            context_pause: pause,
        }),
    })
}

fn paused_turn(thread_id: ThreadId) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "paused".to_string(),
        context_pause: Some(ContextPause {
            thread_id,
            used_tokens: 133_000,
            context_window: 190_000,
            threshold_percent: 70,
        }),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn turn_started() -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "compact".to_string(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn user_message(thread_id: ThreadId) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "handoff".to_string(),
        item: TurnItem::UserMessage(UserMessageItem::new(&[])),
        started_at_ms: None,
        completed_at_ms: 0,
    }))
}

fn restored(thread_id: ThreadId, items: &[RolloutItem]) -> (Option<CompactionContextPause>, bool) {
    let state = ContextPauseState::restore(thread_id, items);
    (state.checkpoint(thread_id), state.waiting_for_user)
}

#[test]
fn only_a_user_message_ends_a_restored_pause() {
    let thread_id = ThreadId::new();
    let reached = |waiting_for_user| {
        Some(CompactionContextPause {
            thread_id,
            waiting_for_user,
        })
    };
    assert_eq!(
        restored(thread_id, &[paused_turn(thread_id), turn_started()]),
        (reached(true), true)
    );
    assert_eq!(
        restored(
            thread_id,
            &[
                paused_turn(thread_id),
                turn_started(),
                user_message(thread_id)
            ]
        ),
        (reached(false), false)
    );
}

#[test]
fn a_review_prompt_does_not_end_a_restored_pause() {
    let thread_id = ThreadId::new();
    let review_prompt = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "Review the change".to_string(),
        ..Default::default()
    }));
    let items = [
        paused_turn(thread_id),
        RolloutItem::EventMsg(EventMsg::EnteredReviewMode(EnteredReviewModeEvent {
            target: ReviewTarget::UncommittedChanges,
            user_facing_hint: None,
            turn_id: None,
            item_id: None,
        })),
        review_prompt,
        user_message(ThreadId::new()),
        RolloutItem::EventMsg(EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
            turn_id: None,
            item_id: None,
            review_output: None,
        })),
    ];
    let state = ContextPauseState::restore(thread_id, &items);
    assert!(state.waiting_for_user);
}

#[test]
fn a_compaction_carries_the_pause_into_a_bounded_resume() {
    let thread_id = ThreadId::new();
    let reached = |waiting_for_user| {
        Some(CompactionContextPause {
            thread_id,
            waiting_for_user,
        })
    };
    assert_eq!(
        restored(thread_id, &[compaction(reached(true))]),
        (reached(true), true)
    );
    assert_eq!(
        restored(thread_id, &[compaction(reached(false))]),
        (reached(false), false)
    );
    assert_eq!(restored(thread_id, &[compaction(None)]), (None, false));
}

#[test]
fn a_fork_does_not_inherit_its_parent_pause() {
    let parent = ThreadId::new();
    let fork = ThreadId::new();
    let parent_pause = Some(CompactionContextPause {
        thread_id: parent,
        waiting_for_user: true,
    });
    assert_eq!(
        restored(fork, &[compaction(parent_pause), paused_turn(parent)]),
        (None, false)
    );
}
