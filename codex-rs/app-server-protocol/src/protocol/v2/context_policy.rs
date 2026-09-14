use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Runtime accounting against the usable context limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextWindowUsage {
    #[ts(type = "number")]
    pub used_tokens: i64,
    #[ts(type = "number")]
    pub context_window: i64,
}

/// Pauses once per thread until explicit user input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextPause {
    #[ts(type = "number")]
    pub used_tokens: i64,
    #[ts(type = "number")]
    pub context_window: i64,
    pub threshold_percent: u8,
}

impl From<codex_protocol::protocol::ContextPause> for ContextPause {
    fn from(value: codex_protocol::protocol::ContextPause) -> Self {
        Self {
            used_tokens: value.used_tokens,
            context_window: value.context_window,
            threshold_percent: value.threshold_percent,
        }
    }
}
