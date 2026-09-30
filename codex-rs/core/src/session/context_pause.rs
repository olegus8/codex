use super::context_window::ContextWindowTokenStatus;
use super::context_window::automatic_compaction_enabled;
use super::session::Session;
use super::turn_context::TurnContext;
use codex_history::CompactionContextPause;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::ContextPause;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_thread_store::PersistContext;

#[derive(Debug, Default)]
pub(crate) struct ContextPauseState {
    reached: bool,
    pub(crate) waiting_for_user: bool,
}

pub(crate) async fn waiting_for_user(session: &Session) -> bool {
    session.state.lock().await.context_pause.waiting_for_user
}

impl ContextPauseState {
    /// Rebuilds the pause from a rollout, which may begin at a compaction.
    ///
    /// Only a user message ends the wait, matching explicit user input at runtime.
    pub(crate) fn restore(thread_id: ThreadId, items: &[RolloutItem]) -> Self {
        let mut state = Self::default();
        // A review forwards its own prompt, which is not the user answering the pause.
        let mut in_review = false;
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
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event
                        .context_pause
                        .as_ref()
                        .is_some_and(|pause| pause.thread_id == thread_id) =>
                {
                    state.reached = true;
                    state.waiting_for_user = true;
                }
                RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_)) => in_review = true,
                RolloutItem::EventMsg(EventMsg::ExitedReviewMode(_)) => in_review = false,
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) if !in_review => {
                    state.waiting_for_user = false;
                }
                RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                    thread_id: item_thread_id,
                    item: TurnItem::UserMessage(_),
                    ..
                })) if *item_thread_id == thread_id => {
                    state.waiting_for_user = false;
                }
                _ => {}
            }
        }
        state
    }

    /// State a compaction carries so a resume from it keeps the pause.
    pub(crate) fn checkpoint(&self, thread_id: ThreadId) -> Option<CompactionContextPause> {
        self.reached.then_some(CompactionContextPause {
            thread_id,
            waiting_for_user: self.waiting_for_user,
        })
    }
}

/// Pause once per thread when active context reaches the configured share.
pub(crate) async fn maybe_pause(
    session: &Session,
    turn: &TurnContext,
    status: &ContextWindowTokenStatus,
) -> bool {
    if automatic_compaction_enabled(turn) || status.full_context_window_limit_reached {
        return false;
    }
    let (Some(percent), Some(limit)) = (
        turn.config.model_context_pause_percent,
        status.full_context_window_limit,
    ) else {
        return false;
    };
    if i128::from(status.active_context_tokens) * 100 < i128::from(limit) * i128::from(percent) {
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
        used_tokens: status.active_context_tokens,
        context_window: limit,
        threshold_percent: percent,
    });
    true
}

#[cfg(test)]
#[path = "context_pause_tests.rs"]
mod tests;

/// Flush the pause marker before delivery.
pub(crate) async fn persist_completion(session: &Session, event: &EventMsg) -> anyhow::Result<()> {
    if let Some(thread) = session.live_thread() {
        thread
            .append_items(&[RolloutItem::EventMsg(event.clone())])
            .await?;
        thread.persist(PersistContext::Standard).await?;
        thread.flush().await?;
    }
    Ok(())
}
