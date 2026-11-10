use codex_history::CompactionContextPause;
use codex_history::RolloutItem;
use codex_history::UserInputOrigin;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;

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
