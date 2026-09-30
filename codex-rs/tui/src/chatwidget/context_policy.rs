//! Presents the once-per-thread context pause.

use super::*;
use codex_app_server_protocol::ContextPause;
use codex_protocol::num_format::format_with_separators;

impl ChatWidget {
    pub(super) fn show_context_pause(&mut self, pause: &ContextPause) {
        self.add_to_history(history_cell::new_info_event(
            format!(
                "Paused at the {}% context threshold ({} of {} tokens used).",
                pause.threshold_percent,
                format_with_separators(pause.used_tokens),
                format_with_separators(pause.context_window),
            ),
            Some("Send a message to continue or request a handoff.".into()),
        ));
        self.request_redraw();
    }
}
