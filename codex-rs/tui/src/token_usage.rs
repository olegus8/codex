//! TUI token usage models and display formatting.

use std::fmt;

use codex_protocol::num_format::format_with_separators;
use serde::Deserialize;
use serde::Serialize;

const BASELINE_TOKENS: i64 = 12000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

impl TokenUsage {
    pub fn is_zero(&self) -> bool {
        self.total_tokens == 0
    }

    pub(crate) fn cached_input(&self) -> i64 {
        self.cached_input_tokens.max(0)
    }

    pub(crate) fn non_cached_input(&self) -> i64 {
        (self.input_tokens - self.cached_input()).max(0)
    }

    pub(crate) fn blended_total(&self) -> i64 {
        (self.non_cached_input() + self.output_tokens.max(0)).max(0)
    }

    /// Returns the raw `total_tokens` value. For `last_token_usage`, this is the latest active
    /// context size; for `total_token_usage`, this is the accumulated session total.
    pub(crate) fn tokens_in_context_window(&self) -> i64 {
        self.total_tokens
    }

    pub(crate) fn percent_of_context_window_remaining(&self, context_window: i64) -> i64 {
        if context_window <= BASELINE_TOKENS {
            return 0;
        }
        let effective_window = context_window - BASELINE_TOKENS;
        let used = (self.tokens_in_context_window() - BASELINE_TOKENS).max(0);
        let remaining = (effective_window - used).max(0);
        ((remaining as f64 / effective_window as f64) * 100.0)
            .clamp(0.0, 100.0)
            .round() as i64
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenUsageInfo {
    #[serde(default)]
    pub(crate) context_window_usage: Option<codex_app_server_protocol::ContextWindowUsage>,
    pub(crate) total_token_usage: TokenUsage,
    pub(crate) last_token_usage: TokenUsage,
    pub(crate) model_context_window: Option<i64>,
}

impl TokenUsageInfo {
    /// Prefers the runtime guard's usable limit when automatic compaction is disabled.
    pub(crate) fn context_window_size(&self) -> Option<i64> {
        self.context_window_usage
            .as_ref()
            .map(|usage| usage.context_window)
            .or(self.model_context_window)
    }

    pub(crate) fn context_remaining_percent(&self) -> Option<i64> {
        if let Some(usage) = &self.context_window_usage {
            let remaining = usage
                .context_window
                .saturating_sub(usage.used_tokens)
                .max(0);
            return Some(if usage.context_window <= 0 {
                0
            } else {
                ((remaining as f64 / usage.context_window as f64) * 100.0)
                    .clamp(0.0, 100.0)
                    .round() as i64
            });
        }
        self.model_context_window.map(|window| {
            self.last_token_usage
                .percent_of_context_window_remaining(window)
        })
    }
}

impl fmt::Display for TokenUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Token usage: total={} input={}{} output={}{}",
            format_with_separators(self.blended_total()),
            format_with_separators(self.non_cached_input()),
            if self.cached_input() > 0 {
                format!(
                    " (+ {} cached)",
                    format_with_separators(self.cached_input())
                )
            } else {
                String::new()
            },
            format_with_separators(self.output_tokens),
            if self.reasoning_output_tokens > 0 {
                format!(
                    " (reasoning {})",
                    format_with_separators(self.reasoning_output_tokens)
                )
            } else {
                String::new()
            }
        )
    }
}
