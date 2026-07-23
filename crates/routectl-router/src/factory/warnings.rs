//! Non-fatal config warnings.

use super::validate::{class_token, is_health_status};
use crate::config::ProviderEntry;

/// Returns `true` when `entry` is `ProviderEntry::AnthropicApi { context_management: true, .. }`.
/// `false` for any other shape. Used by the model-binding warning
/// path to scope the guard to the only provider kind where the
/// `context_management` emulation flag exists.
const fn anthropic_api_uses_context_management(entry: &ProviderEntry) -> bool {
    matches!(
        entry,
        ProviderEntry::AnthropicApi {
            context_management: true,
            ..
        }
    )
}

/// Emit a structured WARN when an anthropic-api provider declares
/// `context_management = true` but the model's `history_reasoning`
/// is missing or set to anything other than `Preserve`. The two
/// settings are complementary: `context_management` controls the
/// outgoing-request shaping for non-Anthropic anthropic-api endpoints
/// (DeepSeek `/anthropic`, vLLM, LM Studio) while `history_reasoning =
/// "preserve"` ensures thinking blocks ride back into the request
/// history so multi-turn continuity is preserved upstream.
///
/// Silent for any other shape: `context_management = false`,
/// `history_reasoning = Preserve`, or non-anthropic-api providers.
/// The literal strings `context_management` and `history_reasoning`
/// appear in the message body so operators can grep the runbook
/// without hunting for the exact wording.
pub(super) fn warn_context_management_needs_preserve(
    provider_name: &str,
    nickname: &str,
    entry: &ProviderEntry,
    history_reasoning: Option<crate::config::HistoryReasoning>,
) {
    if !anthropic_api_uses_context_management(entry) {
        return;
    }
    if matches!(
        history_reasoning,
        Some(crate::config::HistoryReasoning::Preserve)
    ) {
        return;
    }
    tracing::warn!(
        provider = provider_name,
        model = nickname,
        "context_management = true on this anthropic-api provider but \
         history_reasoning is not 'preserve' on the model; thinking \
         echo-back is required for multi-turn continuity. See \
         docs/PROVIDER-QUIRKS.md \"context_management\" for the \
         recommended config."
    );
}

/// Advisory (never fatal) checks over the same `[retry.classes]` +
/// `[providers.X.class_overrides]` surface `validate_class_policy`
/// hard-rejects on. Each finding here is a config smell the operator
/// probably didn't intend, not a misconfiguration the loader must refuse.
///
/// Two checks, each producing zero or more warning lines:
///
///   - A `class_overrides` remap whose SOURCE status is a health signal
///     (`is_health_status`: 408, 429, or any 500..=599). Since
///     `validate_class_policy` already restricts the target to a
///     terminal, non-retrying class, any such remap diverts a
///     breaker-relevant status into a class the breaker does not debit --
///     an outage-masking risk if the upstream is actually unhealthy.
///
///   - An empty `[retry.classes.<c>]` block (both `retry` and `fallback`
///     leaves `None`). Parses fine and does nothing; almost always a
///     leftover the operator forgot to fill in or clear out.
///
/// Call once per process startup (or `routectl config check`) alongside
/// `validate_class_policy`; unlike that function, warnings never fail
/// the load.
pub fn class_policy_warnings(config: &crate::config::Config) -> Vec<String> {
    let mut warnings = Vec::new();

    for (provider_name, entry) in &config.providers {
        for (status, target) in &entry.runtime().class_overrides {
            if is_health_status(*status) {
                warnings.push(format!(
                    "[providers.{provider_name}.class_overrides] {status} = {}: remapping \
                     a health status to a request-fault class disables breaker accounting \
                     for it (outage-masking risk)",
                    class_token(*target),
                ));
            }
        }
    }

    for (class, policy) in &config.retry.classes {
        if policy.retry.is_none() && policy.fallback.is_none() {
            warnings.push(format!(
                "[retry.classes.{}]: `retry` and `fallback` are both unset -- this block \
                 has no effect",
                class_token(*class),
            ));
        }
    }

    warnings
}
