use super::context_window::ContextWindowTokenStatus;
use super::context_window::automatic_compaction_enabled;
use super::session::Session;
use super::turn_context::TurnContext;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ContextPause;
use codex_protocol::protocol::EventMsg;
use codex_thread_store::PersistContext;

#[derive(Default)]
pub(crate) struct ContextPauseState {
    reached: bool,
    pub(crate) waiting_for_user: bool,
}

pub(crate) async fn waiting_for_user(session: &Session) -> bool {
    session.state.lock().await.context_pause.waiting_for_user
}

impl ContextPauseState {
    pub(crate) fn restore(thread_id: ThreadId, items: &[RolloutItem]) -> Self {
        let mut state = Self::default();
        for item in items {
            match item {
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event
                        .context_pause
                        .as_ref()
                        .is_some_and(|pause| pause.thread_id == thread_id) =>
                {
                    state.reached = true;
                    state.waiting_for_user = true;
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(_)) => {
                    state.waiting_for_user = false;
                }
                _ => {}
            }
        }
        state
    }
}

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

/// Append the pause marker and flush completed history before publishing it.
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
