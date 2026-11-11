use super::context_window::ContextWindowTokenStatus;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::compact::automatic_compaction_configured;
use codex_history::CompactionContextPause;
use codex_history::RolloutItem;
use codex_history::UserInputOrigin;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ContextPause;
use codex_protocol::protocol::EventMsg;
use codex_thread_store::PersistContext;

#[derive(Debug, Default)]
pub(crate) struct ContextPauseState {
    reached: bool,
    pub(crate) waiting_for_user: bool,
}

impl ContextPauseState {
    pub(crate) fn restore(thread_id: ThreadId, items: &[RolloutItem]) -> Self {
        let mut state = Self::default();
        let mut in_review = false;
        let mut input_origin = UserInputOrigin::User;
        for item in items {
            match item {
                RolloutItem::Compacted(compacted) => {
                    if let Some(pause) = compacted
                        .resume_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.context_pause)
                        .filter(|pause| pause.thread_id == thread_id)
                    {
                        state.reached = true;
                        state.waiting_for_user = pause.waiting_for_user;
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    if let Some(pause) = event
                        .context_pause
                        .as_ref()
                        .filter(|pause| pause.thread_id == thread_id)
                    {
                        state.reached = true;
                        state.waiting_for_user = !pause.resumed_by_user;
                    }
                }
                RolloutItem::ResponseItem(envelope) if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user") =>
                {
                    input_origin = UserInputOrigin::from_message(&envelope.item);
                }
                RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_)) => in_review = true,
                RolloutItem::EventMsg(EventMsg::ExitedReviewMode(_)) => in_review = false,
                RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => {
                    in_review = false;
                    input_origin = UserInputOrigin::User;
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    state.waiting_for_user &= in_review || !input_origin.is_user();
                    input_origin = UserInputOrigin::User;
                }
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.thread_id == thread_id =>
                {
                    match &event.item {
                        TurnItem::EnteredReviewMode(_) => in_review = true,
                        TurnItem::ExitedReviewMode(_) => in_review = false,
                        TurnItem::UserMessage(_) => {
                            state.waiting_for_user &= in_review || !input_origin.is_user();
                            input_origin = UserInputOrigin::User;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        state
    }

    pub(crate) fn checkpoint(&self, thread_id: ThreadId) -> Option<CompactionContextPause> {
        self.reached.then_some(CompactionContextPause {
            thread_id,
            waiting_for_user: self.waiting_for_user,
        })
    }
}

pub(super) async fn require_user_input(
    session: &Session,
    explicit_user: bool,
) -> Result<(), CodexErr> {
    if !explicit_user && session.state.lock().await.context_pause.waiting_for_user {
        return Err(CodexErr::InvalidRequest(
            "Context paused; explicit user input is required to continue.".to_string(),
        ));
    }
    Ok(())
}

pub(super) async fn acknowledge_user_input(session: &Session, explicit_user: bool) {
    if explicit_user {
        session.state.lock().await.context_pause.waiting_for_user = false;
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "suspension and marking the active task as pausing must be atomic"
)]
pub(crate) async fn maybe_pause(
    session: &Session,
    turn: &TurnContext,
    status: &ContextWindowTokenStatus,
) -> bool {
    if automatic_compaction_configured(turn) || status.full_context_window_limit_reached {
        return false;
    }
    let (Some(threshold_percent), Some(context_window)) = (
        turn.config.model_context_pause_percent,
        status.full_context_window_limit,
    ) else {
        return false;
    };
    let used_tokens = status.active_context_tokens;
    if context_window <= 0
        || i128::from(used_tokens) * 100
            < i128::from(context_window) * i128::from(threshold_percent)
    {
        return false;
    }

    // Suspension removes the task under this lock. It must see the pause marker
    // before it can hand the turn off for automatic recovery.
    let active = session.active_turn.lock().await;
    if active
        .as_ref()
        .and_then(|active| active.task.as_ref())
        .is_none_or(|task| task.turn_context.sub_id != turn.sub_id)
    {
        return false;
    }
    let mut state = session.state.lock().await;
    if state.context_pause.reached {
        return false;
    }
    state.context_pause.reached = true;
    state.context_pause.waiting_for_user = true;
    turn.extension_data.insert(ContextPause {
        thread_id: session.thread_id(),
        used_tokens,
        context_window,
        threshold_percent,
        resumed_by_user: false,
    });
    true
}

pub(crate) async fn completion(session: &Session, turn: &TurnContext) -> Option<ContextPause> {
    let pause = turn.extension_data.get::<ContextPause>()?;
    Some(ContextPause {
        resumed_by_user: !session.state.lock().await.context_pause.waiting_for_user,
        ..(*pause).clone()
    })
}

pub(super) async fn persist_completion(session: &Session, event: &EventMsg) -> anyhow::Result<()> {
    if let Some(thread) = session.live_thread() {
        thread
            .append_items(&[RolloutItem::EventMsg(event.clone())])
            .await?;
        thread.persist(PersistContext::Standard).await?;
        thread.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "context_pause_tests.rs"]
mod tests;
