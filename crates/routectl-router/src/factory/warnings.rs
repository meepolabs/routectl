//! Non-fatal config warnings.

use super::validate::{class_token, is_health_status};
use crate::class_policy::ConfigFailureClass;
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
/// Three checks, each producing zero or more warning lines:
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
///   - `[retry.classes.bad-request] fallback = false`. Valid config
///     (the override works as written), but the baked `bad-request`
///     fallback is what walks a capability-filter rejection to a capable
///     target: turning it off also turns off structured-output rescue,
///     so a request needing a capability the target lacks hard-fails
///     instead of falling over.
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

    if config
        .retry
        .classes
        .get(&ConfigFailureClass::BadRequest)
        .and_then(|policy| policy.fallback)
        == Some(false)
    {
        warnings.push(
            "[retry.classes.bad-request] fallback = false: disabling bad-request fallback \
             also disables capability-filter structured-output rescue -- a request needing \
             a capability the target lacks will hard-fail instead of walking to a capable \
             target"
                .to_string(),
        );
    }

    warnings
}

/// Advisory (never fatal) load-time check on the per-block cache
/// breakpoint surface: warn when `auto_emit_per_block_breakpoints` is set
/// on an entry whose EGRESS cannot carry a per-block marker
/// (`ProviderEntry::supports_per_block_breakpoints` is false). The key
/// parses and the entry loads, but the value changes nothing either way --
/// placement gates on wire support, so an opt-in there is inert rather than
/// honored.
///
/// Covers Bedrock `api_shape = "invoke"` (no front-marker path; its egress
/// lowers the TOP-LEVEL marker itself), `openai-compat` (the egress DROPS a
/// per-block marker, and 400s under `strict_translation`), and the
/// server-side-caching kinds `openai-responses` / `gemini`. Silent on
/// `anthropic-api` and Bedrock Converse, where the knob is live.
pub fn per_block_breakpoint_warnings(config: &crate::config::Config) -> Vec<String> {
    config
        .providers
        .iter()
        .filter(|(_, entry)| {
            entry.auto_emit_per_block_breakpoints().is_some()
                && !entry.supports_per_block_breakpoints()
        })
        .map(|(provider_name, entry)| {
            format!(
                "[providers.{provider_name}] auto_emit_per_block_breakpoints has no effect on \
                 {}: that egress cannot carry a per-block cache marker, so the key is inert. The \
                 knob gates front-marker emission on anthropic-api and on Bedrock api_shape = \
                 \"converse\" only. Remove the key.",
                inert_per_block_surface(entry),
            )
        })
        .collect()
}

/// Name the surface the inert key sits on, for
/// [`per_block_breakpoint_warnings`]. A Bedrock entry is named by its
/// `api_shape` (the shape, not the kind, is what decides per-block support
/// there, and naming `bedrock` alone would read as if no Bedrock entry
/// supported the knob); every other kind is named by its `kind` token.
fn inert_per_block_surface(entry: &ProviderEntry) -> String {
    #[cfg(feature = "bedrock")]
    if let ProviderEntry::Bedrock { api_shape, .. } = entry {
        use crate::config::BedrockApiShapeConfig;
        let shape = match api_shape {
            BedrockApiShapeConfig::Invoke => "invoke",
            BedrockApiShapeConfig::Converse => "converse",
        };
        return format!("api_shape = \"{shape}\"");
    }
    format!("kind = \"{}\"", entry.kind_str())
}

/// Advisory (never fatal) load-time check on the Cloud Code host surface:
/// warn when a `cloud-code` Gemini entry pins `base_url` at the PRODUCTION
/// Cloud Code host. The pin is honored verbatim -- routectl never rewrites
/// it and never issues a second request against the other host -- but the
/// lane default is the daily host, and a consumer seat (one not covered by
/// the GCP terms of service) serves on daily, so a production pin is a
/// plausible cause of a permission / quota rejection the operator would
/// otherwise read as an account problem.
///
/// The host test is [`routectl_providers::gemini::is_prod_host`]: an exact
/// match on the PARSED host, so a lookalike authority
/// (`<prod-host>.example.test`, `<prod-host>@evil.example`, the host inside
/// a path) does not fire the warning. Silent for an unset `base_url` (that
/// entry takes the daily default), an explicit daily or enterprise-mirror
/// pin, and every `api-key` entry -- the Cloud Code hosts are not on the
/// api-key lane at all.
///
/// Not `const`: with `gemini` enabled the body allocates and formats. Only
/// the reduced build collapses to the empty-`Vec` arm, and constness is
/// part of the recorded public API -- so it cannot vary by feature.
#[allow(clippy::missing_const_for_fn)]
pub fn cloudcode_host_warnings(config: &crate::config::Config) -> Vec<String> {
    #[cfg(feature = "gemini")]
    {
        use routectl_providers::gemini::GeminiAuthMode;

        config
            .providers
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry,
                    ProviderEntry::Gemini {
                        auth_mode: GeminiAuthMode::CloudCode,
                        base_url,
                        ..
                    } if routectl_providers::gemini::is_prod_host(base_url)
                )
            })
            .map(|(provider_name, _)| {
                format!(
                    "[providers.{provider_name}] base_url pins this cloud-code entry to the \
                     production cloud-code host; the lane default is the daily host, and \
                     consumer (non-GCP-ToS) seats serve on the daily host. If this entry is \
                     rejected on permission or quota, the pin is a candidate cause -- unset \
                     base_url to serve on the daily default."
                )
            })
            .collect()
    }
    #[cfg(not(feature = "gemini"))]
    {
        let _ = config;
        Vec::new()
    }
}

/// Advisory (never fatal) load-time check on the Cloud Code model-id
/// surface: warn when a `[models.X]` entry served by a `cloud-code` Gemini
/// provider pins an `upstream` id Google has deprecated server-side. The id
/// still resolves upstream, so the request succeeds -- but it is served by
/// whatever Google now aliases the name to, which is not what the config
/// says. The message names the verified replacement so the fix is a single
/// edit.
///
/// The provider-mode gate is load-bearing, not a scoping nicety:
/// `gemini-2.5-flash` is a LIVE, current id on the native api-key REST
/// surface. An `api-key` entry pinning it is correct config and must stay
/// silent; only the `cloud-code` lane has the alias problem.
///
/// The alias list itself lives provider-side in
/// [`routectl_providers::gemini::deprecated_alias_replacement`] -- one home
/// to edit when Google churns the catalog again.
///
/// A `[models.X] provider` value naming a `[pools.X]` block rather than a
/// `[providers.X]` entry is not inspected: the join here is against
/// `config.providers` only.
///
/// Not `const`: with `gemini` enabled the body allocates and formats. Only
/// the reduced build collapses to the empty-`Vec` arm, and constness is
/// part of the recorded public API -- so it cannot vary by feature.
#[allow(clippy::missing_const_for_fn)]
pub fn cloudcode_model_warnings(config: &crate::config::Config) -> Vec<String> {
    #[cfg(feature = "gemini")]
    {
        use routectl_providers::gemini::{GeminiAuthMode, deprecated_alias_replacement};

        let mut warnings = Vec::new();
        for (nickname, model) in &config.models {
            if !matches!(
                config.providers.get(&model.provider),
                Some(ProviderEntry::Gemini {
                    auth_mode: GeminiAuthMode::CloudCode,
                    ..
                })
            ) {
                continue;
            }
            if let Some(replacement) = deprecated_alias_replacement(&model.upstream) {
                warnings.push(format!(
                    "[models.{nickname}] upstream `{upstream}` is a deprecated Cloud Code \
                     model id: Google still resolves it, but server-side it now aliases to \
                     another model, so this entry does not serve what its id says. Set \
                     upstream = \"{replacement}\" (verified end-to-end on the cloud-code \
                     lane). The id remains current on the api-key Gemini lane -- only \
                     cloud-code entries need the change.",
                    upstream = model.upstream,
                ));
            }
        }
        warnings
    }
    #[cfg(not(feature = "gemini"))]
    {
        let _ = config;
        Vec::new()
    }
}

/// Advisory (never fatal) load-time check on the codex identity surface:
/// warn when a chatgpt-oauth openai-responses provider overrides the
/// `version` or `user-agent` identity header via `header_extras` with a
/// value that diverges from the derived codex identity. The override still
/// WINS (the merge order is unchanged and settled) -- this only surfaces
/// that the operator is emitting a fingerprint other than the one routectl
/// derives, which the chatgpt.com backend may flag. A matching or absent
/// override is silent.
///
/// The comparison is against the identity routectl WOULD derive from the
/// resolved `codex_version` (config-level, independent of whether the
/// process-global identity has been installed yet), so it fires the same
/// way on the `config check` path as on the serve boot path.
///
/// Not `const`: with `openai-responses` enabled the body allocates and
/// formats. Only the reduced build collapses to the empty-`Vec` arm, and
/// constness is part of the recorded public API -- so it cannot vary by
/// feature.
#[allow(clippy::missing_const_for_fn)]
pub fn codex_identity_warnings(config: &crate::config::Config) -> Vec<String> {
    #[cfg(feature = "openai-responses")]
    {
        use routectl_core::identity::codex::{CodexIdentity, PINNED_CODEX_VERSION};

        let effective_version = super::validate::resolved_codex_version(config)
            .unwrap_or_else(|| PINNED_CODEX_VERSION.to_string());
        let identity = CodexIdentity::new(&effective_version);

        let mut warnings = Vec::new();
        for (provider_name, entry) in &config.providers {
            if !entry.is_chatgpt_oauth_responses() {
                continue;
            }
            for (key, value) in entry.header_extras() {
                match key.to_ascii_lowercase().as_str() {
                    "version" if value != identity.version() => warnings.push(format!(
                        "[providers.{provider_name}.header_extras] version = \"{value}\" overrides \
                         the derived codex identity version \"{}\"; the override wins but emits a \
                         fingerprint routectl did not derive",
                        identity.version(),
                    )),
                    "user-agent" if value != identity.user_agent() => warnings.push(format!(
                        "[providers.{provider_name}.header_extras] user-agent overrides the \
                         derived codex User-Agent; the override wins but emits a fingerprint \
                         routectl did not derive"
                    )),
                    _ => {}
                }
            }
        }
        warnings
    }
    #[cfg(not(feature = "openai-responses"))]
    {
        let _ = config;
        Vec::new()
    }
}

#[cfg(test)]
#[path = "warnings_tests.rs"]
mod warnings_tests;
