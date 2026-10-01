use super::session::Session;
use super::turn_context::TurnContext;
use crate::config::Config;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::ContextWindowUsage;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;

/// Guardian reviews keep their own compaction regardless of the user setting.
pub(crate) fn automatic_compaction_enabled(turn_context: &TurnContext) -> bool {
    turn_context.config.model_auto_compact_enabled
        || crate::guardian::is_basic_session_source(&turn_context.session_source)
}

#[derive(Debug)]
pub(crate) struct ContextWindowTokenStatus {
    // Full active context usage, independent of the configured auto-compact scope.
    pub(crate) active_context_tokens: i64,
    // Usage counted against `model_auto_compact_token_limit` for the current scope.
    pub(crate) auto_compact_scope_tokens: i64,
    pub(crate) auto_compact_scope_limit: Option<i64>,
    pub(crate) full_context_window_limit: Option<i64>,
    pub(crate) base_window_tokens_remaining: Option<i64>,
    pub(crate) auto_compact_window_prefill_tokens: Option<i64>,
    pub(crate) full_context_window_limit_reached: bool,
    pub(crate) token_limit_reached: bool,
    pub(crate) turn_end_compaction_threshold_reached: bool,
}

fn tokens_remaining(limit: Option<i64>, used: i64) -> Option<i64> {
    limit.map(|limit| limit.saturating_sub(used).max(0))
}

/// Report runtime usage against the usable limit when automatic compaction is disabled.
pub(crate) async fn publish_usage(
    sess: &Session,
    turn_context: &TurnContext,
    status: &ContextWindowTokenStatus,
) {
    if automatic_compaction_enabled(turn_context) {
        return;
    }
    let Some(context_window) = status.full_context_window_limit else {
        return;
    };
    let (info, rate_limits) = {
        let mut state = sess.state.lock().await;
        let (info, rate_limits) = state.token_info_and_rate_limits();
        let mut info = info.unwrap_or(TokenUsageInfo {
            context_window_usage: None,
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            model_context_window: Some(context_window),
        });
        info.context_window_usage = Some(ContextWindowUsage {
            used_tokens: status.active_context_tokens,
            context_window,
        });
        state.set_token_info(Some(info.clone()));
        (Some(info), rate_limits)
    };
    sess.send_event(
        turn_context,
        EventMsg::TokenCount(TokenCountEvent { info, rate_limits }),
    )
    .await;
}

pub(crate) async fn context_window_token_status(
    sess: &Session,
    turn_context: &TurnContext,
) -> ContextWindowTokenStatus {
    context_window_token_status_with_config(
        sess,
        turn_context.config.as_ref(),
        turn_context.model_info().as_ref(),
        !automatic_compaction_enabled(turn_context),
    )
    .await
}

pub(crate) async fn context_window_token_status_for_model(
    sess: &Session,
    config: &Config,
    turn_context: &TurnContext,
    model_info: &ModelInfo,
) -> ContextWindowTokenStatus {
    let config = config_for_model(config, turn_context, model_info);
    context_window_token_status_with_config(
        sess, &config, model_info, /*measure_provider_usage*/ false,
    )
    .await
}

/// Usage checked before each request of a turn without automatic compaction.
pub(crate) async fn usable_context_token_status(
    sess: &Session,
    turn_context: &TurnContext,
    model_info: &ModelInfo,
) -> ContextWindowTokenStatus {
    let config = config_for_model(turn_context.config.as_ref(), turn_context, model_info);
    context_window_token_status_with_config(
        sess, &config, model_info, /*measure_provider_usage*/ true,
    )
    .await
}

fn config_for_model(config: &Config, turn_context: &TurnContext, model_info: &ModelInfo) -> Config {
    let mut config = config.clone();
    config.token_budget = super::token_budget::resolve_token_budget(
        turn_context.configured_token_budget.as_ref(),
        turn_context.use_model_token_budget_defaults,
        model_info,
    );
    config
}

async fn context_window_token_status_with_config(
    sess: &Session,
    config: &Config,
    model_info: &ModelInfo,
    measure_provider_usage: bool,
) -> ContextWindowTokenStatus {
    // Without compaction, measure the provider's reported usage, estimating the whole prompt
    // until the provider reports any.
    let active_context_tokens = if !measure_provider_usage {
        sess.get_total_token_usage().await
    } else if sess
        .token_usage_info()
        .await
        .is_none_or(|info| info.last_token_usage.total_tokens == 0)
    {
        let base_instructions = sess.get_prompt_base_instructions().await;
        sess.clone_history()
            .await
            .estimate_token_count_with_base_instructions(&base_instructions)
            .unwrap_or(0)
    } else {
        sess.get_reported_token_usage().await
    };

    // Count either the full active context or only the tokens added after the initial prefix.
    let (auto_compact_scope_tokens, auto_compact_scope_limit, auto_compact_window_prefill_tokens) =
        match config.model_auto_compact_token_limit_scope {
            AutoCompactTokenLimitScope::Total => (
                active_context_tokens,
                model_info.auto_compact_token_limit(),
                None,
            ),
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                let window = sess.auto_compact_window_snapshot().await;
                let baseline = window.prefill_input_tokens.unwrap_or(active_context_tokens);

                let scope_limit = config
                    .model_auto_compact_token_limit
                    .or_else(|| model_info.auto_compact_token_limit());
                (
                    active_context_tokens.saturating_sub(baseline),
                    scope_limit,
                    window.prefill_input_tokens,
                )
            }
        };

    // The model's full context window is a hard cap, independent of the auto-compaction scope.
    let full_context_window_limit = model_info.resolved_context_window().map(|context_window| {
        context_window.saturating_mul(model_info.effective_context_window_percent) / 100
    });

    // Report remaining tokens against the base (unbuffered) window, capped by the full context.
    let base_window_tokens_remaining = [
        tokens_remaining(auto_compact_scope_limit, auto_compact_scope_tokens),
        tokens_remaining(full_context_window_limit, active_context_tokens),
    ]
    .into_iter()
    .flatten()
    .min();

    // Only reserve the fallback buffer when there is a fallback prompt to use it.
    let auto_compact_fallback_buffer_tokens = config
        .token_budget
        .as_ref()
        .map_or(0, crate::config::TokenBudgetConfig::fallback_buffer_tokens);
    let buffered_auto_compact_limit = auto_compact_scope_limit
        .map(|limit| limit.saturating_add(auto_compact_fallback_buffer_tokens));

    // Force compaction once the buffered window or the model's full context window is reached.
    let full_context_window_limit_reached =
        full_context_window_limit.is_some_and(|limit| active_context_tokens >= limit);
    let token_limit_reached = buffered_auto_compact_limit
        .is_some_and(|limit| auto_compact_scope_tokens >= limit)
        || full_context_window_limit_reached;
    let post_turn_percent = config.model_post_turn_compact_threshold_percent;
    let turn_end_compaction_threshold_reached = post_turn_percent > 0
        && (token_limit_reached
            || full_context_window_limit.is_some_and(|limit| {
                i128::from(active_context_tokens) * 100
                    >= i128::from(limit) * i128::from(post_turn_percent)
            }));

    ContextWindowTokenStatus {
        active_context_tokens,
        auto_compact_scope_tokens,
        auto_compact_scope_limit,
        full_context_window_limit,
        base_window_tokens_remaining,
        auto_compact_window_prefill_tokens,
        full_context_window_limit_reached,
        token_limit_reached,
        turn_end_compaction_threshold_reached,
    }
}
