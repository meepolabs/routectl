//! Config-row validation family + validation collection.

use super::build::ipv4_compatible_embedded;
use super::warnings::class_policy_warnings;
use crate::config::{Config, CredentialSource, ProviderEntry};
use routectl_core::Result;
use routectl_core::identity::anthropic::is_anthropic_api_host;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use std::collections::BTreeMap;

pub(super) fn validate_base_url_scheme(provider_name: &str, base_url: &str) -> Result<()> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        // A present-but-empty base_url is an operator typo, not a
        // "use the kind default" signal: the only default substitution
        // is for the OpenaiResponses `None` case, which is resolved to
        // a concrete URL BEFORE this fn is called. Reject here as
        // defense-in-depth for direct library consumers.
        return Err(routectl_core::Error::Config(format!(
            "provider `{provider_name}`: base_url is set but empty; \
             set an explicit endpoint or omit the field to use the kind default"
        )));
    }
    let url = match url::Url::parse(trimmed) {
        Ok(u) => u,
        Err(e) => {
            return Err(routectl_core::Error::Config(format!(
                "provider `{provider_name}`: base_url is not a valid URL: {e}"
            )));
        }
    };
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(routectl_core::Error::Config(format!(
            "provider `{provider_name}`: base_url scheme `{scheme}` is not allowed; \
             use https:// (or http:// for loopback only)"
        )));
    }

    // Require a non-empty host. A hostless authority cannot name a real
    // egress, and this is defense in depth against a malformed authority
    // entering config at all. The Anthropic host predicate does NOT rely on
    // this guard (core/provider APIs have direct callers) -- the two are
    // independent.
    if url.host_str().is_none_or(str::is_empty) {
        return Err(routectl_core::Error::Config(format!(
            "provider `{provider_name}`: base_url has no host; \
             set an explicit http(s) endpoint with a hostname"
        )));
    }

    // Link-local rejection (regardless of scheme). Covers cloud
    // metadata services. `Ipv4Addr::is_link_local` is stable since
    // 1.0 (covers 169.254.0.0/16). For IPv6 we check the fe80::/10
    // prefix manually since `is_unicast_link_local` was only
    // stabilized recently and we want to keep MSRV low.
    if let Some(host) = url.host() {
        let link_local = match host {
            url::Host::Ipv4(ip) => ip.is_link_local(),
            // Canonicalize an IPv4-mapped IPv6 address
            // (`::ffff:a.b.c.d`) to its embedded IPv4 before the
            // link-local test, otherwise `::ffff:169.254.169.254`
            // slips past the fe80::/10 segment check and can reach
            // cloud-metadata with credentials.
            url::Host::Ipv6(ip) => match ip.to_ipv4_mapped() {
                Some(v4) => v4.is_link_local(),
                // Not IPv4-mapped: catch the IPv4-COMPATIBLE form
                // (`::a.b.c.d`) too, then fall back to the fe80::/10
                // segment check for native IPv6.
                None => ipv4_compatible_embedded(&ip)
                    .map_or((ip.segments()[0] & 0xffc0) == 0xfe80, |v4| {
                        v4.is_link_local()
                    }),
            },
            url::Host::Domain(_) => false,
        };
        if link_local {
            return Err(routectl_core::Error::Config(format!(
                "provider `{provider_name}`: base_url targets a link-local \
                 address; cloud-metadata IPs (169.254.169.254 etc.) and IPv6 fe80::/10 \
                 are blocked at build time to prevent SSRF / credential leak"
            )));
        }
    }

    if scheme == "https" {
        return Ok(());
    }
    // http:// is permitted only for loopback hosts so local-dev and
    // integration tests work.
    let host = url.host_str().unwrap_or("");
    let is_loopback = host == "localhost"
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
        || host.starts_with("127.")
        || url
            .host()
            .and_then(|h| match h {
                url::Host::Ipv4(ip) => Some(ip.is_loopback()),
                // Canonicalize an IPv4-mapped IPv6 loopback
                // (`::ffff:127.0.0.1`) so it is accepted as loopback
                // http:// just like the bare `127.0.0.1`, rather than
                // misleadingly rejected as cleartext non-loopback.
                url::Host::Ipv6(ip) => Some(match ip.to_ipv4_mapped() {
                    Some(v4) => v4.is_loopback(),
                    None => ipv4_compatible_embedded(&ip)
                        .map_or(ip.is_loopback(), |v4| v4.is_loopback()),
                }),
                url::Host::Domain(_) => None,
            })
            .unwrap_or(false);
    if is_loopback {
        return Ok(());
    }
    Err(routectl_core::Error::Config(format!(
        "provider `{provider_name}`: base_url uses cleartext http:// for a \
         non-loopback host -- API keys and prompt content would be sent in \
         the clear. Use https:// (or bind a local proxy on 127.0.0.1)"
    )))
}

/// Validate the `account_id_ref` invariant for an openai-responses
/// entry. A misconfigured TOML surfaces here as a clean `Error::Config`
/// rather than a confusing upstream 401/403 at first request time.
///
/// Rules:
///   - ChatgptOauth + `oauth://<provider>` bearer: `account_id_ref` is
///     OPTIONAL. When omitted, the factory derives the account id from
///     the logged-in OAuth session (the `chatgpt_account_id` recorded
///     at `routectl login`). An explicit `account_id_ref` is still
///     accepted and wins as an override.
///   - ChatgptOauth + static bearer (`env://`/`file://`/`literal:`):
///     `account_id_ref` is REQUIRED. There is no OAuth session to read
///     the account id from, so the operator must supply it -- this is
///     the legacy chatgpt-oauth workflow, kept unchanged.
///   - ApiKey / BedrockMantle: `account_id_ref` is FORBIDDEN (the
///     account id is a ChatGPT-OAuth-only concept).
///
/// `bearer_is_oauth` mirrors `matches!(SecretRef::parse(api_key_ref),
/// Ok(SecretRef::OAuth { .. }))`; the caller computes it once and passes
/// it in so the validator and the downstream resolver do not each
/// reparse the same URI.
#[cfg(feature = "openai-responses")]
pub(super) fn validate_openai_responses_account_id(
    name: &str,
    auth_kind: OpenaiResponsesAuthKind,
    bearer_is_oauth: bool,
    account_id_ref: &Option<String>,
) -> Result<()> {
    use routectl_core::Error;

    let has_account = account_id_ref.is_some();
    let is_chatgpt_oauth = matches!(auth_kind, OpenaiResponsesAuthKind::ChatgptOauth);
    if !is_chatgpt_oauth {
        // ApiKey / BedrockMantle: account_id is a ChatGPT-OAuth-only
        // concept; reject it for the other surfaces.
        if has_account {
            return Err(Error::Config(format!(
                "openai-responses provider `{name}`: `account_id_ref` is only valid \
                 when auth_kind = \"chatgpt-oauth\"; remove it for {auth_kind:?}"
            )));
        }
        return Ok(());
    }

    // ChatgptOauth path. `oauth://` bearers may omit account_id_ref
    // (derived from the session); static bearers must supply it.
    if bearer_is_oauth || has_account {
        return Ok(());
    }
    Err(Error::Config(format!(
        "openai-responses provider `{name}`: auth_kind = \"chatgpt-oauth\" with a \
         static bearer requires `account_id_ref` (the ChatGPT account UUID). Use an \
         `oauth://<provider>` bearer to derive it from a logged-in session instead."
    )))
}

/// Routectl-mandatory body fields: keys routectl writes into every
/// Bedrock-Invoke body. If `[bedrock] allowed_body_fields` is non-empty
/// AND missing any of these, the egress drops them on send and the
/// upstream 400s the malformed body. Surfaces here as a clean startup
/// error instead. (Skipped entirely when `allowed_body_fields` is
/// empty -- that puts the filter in pass-through mode.)
///
/// Keep in sync with `is_bedrock_invoke_managed_key` in
/// `routectl-providers/src/bedrock/invoke.rs` -- that is the writer
/// side; this is the validator side.
#[cfg(feature = "bedrock")]
const BEDROCK_REQUIRED_BODY_FIELDS: &[&str] = &["anthropic_version", "max_tokens", "messages"];

/// Validate the per-deployment Bedrock allowlists.
///
/// Empty lists are PASS-THROUGH mode -- no filter applies, so no
/// validation is needed. The operator is in discovery mode (capturing
/// observed traffic via `ROUTECTL_LOG=routectl_providers::bedrock=trace`
/// to build their list) or has explicitly opted out of routectl-side
/// filtering. Either way, validation is only meaningful when the
/// operator has populated a non-empty list and we want to catch
/// configurations that would silently break their requests.
///
/// When `allowed_body_fields` is non-empty, validate:
///   - Routectl-mandatory keys (`messages`, `anthropic_version`,
///     `max_tokens`) are present -- but only when at least one provider
///     uses `api_shape = "invoke"`. Those keys live at the AWS top
///     level on Converse and never appear in
///     `additionalModelRequestFields`, so a Converse-only deployment
///     is unaffected by their absence from the allowlist.
///   - If any provider has a `[providers.X] anthropic_beta` floor,
///     `anthropic_beta` is on the list -- otherwise the filter
///     silently drops the operator-asserted always-send array. Applies
///     to both Invoke (top-level body) and Converse
///     (`additionalModelRequestFields` bag).
#[cfg(feature = "bedrock")]
pub(super) fn validate_bedrock_allowlists(
    has_invoke_provider: bool,
    has_provider_beta_floor: bool,
    _allowed_betas: &[String],
    allowed_body_fields: &[String],
) -> Result<()> {
    use routectl_core::Error;

    // Pass-through mode: nothing to validate.
    if allowed_body_fields.is_empty() {
        return Ok(());
    }

    if has_invoke_provider {
        let missing: Vec<&str> = BEDROCK_REQUIRED_BODY_FIELDS
            .iter()
            .copied()
            .filter(|required| !allowed_body_fields.iter().any(|s| s == required))
            .collect();
        if !missing.is_empty() {
            return Err(Error::Config(format!(
                "[bedrock] allowed_body_fields is missing routectl-mandatory keys \
                 {missing:?}. Without these, every Bedrock Invoke request 400s on \
                 the egress. See examples/bedrock.toml for the full baseline; or \
                 remove `[bedrock] allowed_body_fields` entirely to disable \
                 filtering and run in discovery mode."
            )));
        }
    }

    if has_provider_beta_floor && !allowed_body_fields.iter().any(|s| s == "anthropic_beta") {
        return Err(Error::Config(
            "[bedrock] allowed_body_fields is missing `anthropic_beta`, but at \
             least one [providers.X] bedrock entry sets anthropic_beta. The \
             per-provider floor is operator-asserted always-send; include \
             `anthropic_beta` in allowed_body_fields or remove the per-provider \
             floor. See examples/bedrock.toml for the baseline."
                .into(),
        ));
    }

    Ok(())
}

/// Validate that `[bedrock]` allowlists are coherent with the
/// configured providers. Returns Ok in two cases:
///
///   - No provider has `kind = "bedrock"` (no-op).
///   - `[bedrock] allowed_body_fields` is empty (pass-through mode --
///     routectl forwards the assembled body verbatim, so there is
///     nothing to validate). This is the discovery-mode default:
///     bring up routectl, observe traffic via
///     `ROUTECTL_LOG=routectl_providers::bedrock=trace`, then build
///     `allowed_betas` / `allowed_body_fields` from what you see.
///
/// Returns Err only when the operator has populated a non-empty
/// `allowed_body_fields` that:
///
///   - Is missing routectl-mandatory keys (`messages`,
///     `anthropic_version`, `max_tokens`), which would silently break
///     every Bedrock request.
///   - Is missing `anthropic_beta` while a `[providers.X]` entry sets
///     a `anthropic_beta` floor that the filter would then drop.
///
/// `allowed_betas` is independent: empty there just disables betas
/// filtering, allowed there just gates which betas survive. No
/// validation of `allowed_betas` shape is needed.
///
/// Call once per process startup BEFORE building any providers.
#[cfg(feature = "bedrock")]
pub fn validate_bedrock_global_config(config: &crate::config::Config) -> Result<()> {
    let mut bedrock_in_use = false;
    let mut has_invoke_provider = false;
    let mut has_provider_beta_floor = false;
    for entry in config.providers.values() {
        if let crate::config::ProviderEntry::Bedrock {
            api_shape,
            anthropic_beta,
            ..
        } = entry
        {
            bedrock_in_use = true;
            has_invoke_provider |=
                matches!(api_shape, crate::config::BedrockApiShapeConfig::Invoke);
            has_provider_beta_floor |= !anthropic_beta.is_empty();
        }
    }
    if !bedrock_in_use {
        return Ok(());
    }

    validate_bedrock_allowlists(
        has_invoke_provider,
        has_provider_beta_floor,
        &config.bedrock.allowed_betas,
        &config.bedrock.allowed_body_fields,
    )
}

/// Validate that every `[models.X]` routed at a Bedrock provider on the
/// InvokeModel lane names an Anthropic-family model.
///
/// The InvokeModel body is assembled by the Anthropic-shape normalizer
/// (hardcoded `anthropic_version`, `max_tokens`, Anthropic message
/// blocks), and the response is parsed back through the same shape, so a
/// non-Anthropic model on that lane cannot work -- both halves of the
/// wire contract are wrong. The Converse lane is vendor-neutral and is
/// never rejected here regardless of model family.
///
/// Rejection is deliberate rather than silently switching the entry to
/// Converse: the shape selects the response translation too, so an
/// inferred switch would move behavior the operator did not ask for,
/// and it would leave `api_shape` with a configured value distinct from
/// its effective one for every downstream consumer to reconcile.
///
/// Because the model id lives at `[models.X] upstream` rather than on
/// the provider entry, this walks `config.models` and joins each entry
/// to its `[providers]` row.
///
/// A model id that proves nothing -- an inference-profile ARN, which may
/// carry no vendor token -- PASSES. This gate is an ergonomics guard,
/// not a proof obligation, and rejecting an ARN would break working
/// Claude-on-ARN deployments.
///
/// A model whose `provider` names no configured provider is ignored
/// here; `build_resolved_models` owns that diagnostic.
///
/// Call once per process startup BEFORE building any providers.
#[cfg(feature = "bedrock")]
pub fn validate_bedrock_invoke_model_family(config: &crate::config::Config) -> Result<()> {
    use crate::anthropic_family::{AnthropicFamily, anthropic_family};
    use routectl_core::Error;

    let mut errors: Vec<String> = Vec::new();
    for (nickname, model) in &config.models {
        let Some(crate::config::ProviderEntry::Bedrock { api_shape, .. }) =
            config.providers.get(&model.provider)
        else {
            continue;
        };
        if !matches!(api_shape, crate::config::BedrockApiShapeConfig::Invoke) {
            continue;
        }
        if anthropic_family(&model.upstream) == AnthropicFamily::No {
            errors.push(format!(
                "[models.{nickname}] upstream `{upstream}` is not an Anthropic-family \
                 model, but its provider `{provider}` is configured with \
                 `api_shape = \"invoke\"`. The invoke lane sends and parses the \
                 Anthropic wire shape, so this model cannot work on it: set \
                 `api_shape = \"converse\"` on [providers.{provider}] (the \
                 vendor-neutral lane), or point this model at an Anthropic upstream.",
                upstream = model.upstream,
                provider = model.provider,
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors.join("\n")))
    }
}

/// Validate the `[models.X] effort_levels` allowlist across every
/// configured model.
///
/// Each element must be one of the six known effort vocabulary tokens
/// (the union of the Anthropic-shape and OpenAI-shape vocabularies):
/// `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Individual
/// egresses clamp to their own subset at dispatch time; the validator
/// here catches operator typos before any request is processed.
///
/// An empty `effort_levels` list is valid (means pass-through -- the
/// egress accepts whatever effort the caller supplied).
///
/// Returns `Err(Error::Config(...))` on the first model entry that
/// contains an unknown effort token, naming the model nickname and
/// the offending token.
///
/// Call once per process startup BEFORE building any providers.
pub fn validate_reasoning_defaults(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;
    use routectl_providers::effort::VALID_EFFORT_TOKENS;

    for (nickname, entry) in &config.models {
        for level in &entry.effort_levels {
            if !VALID_EFFORT_TOKENS.contains(&level.as_str()) {
                return Err(Error::Config(format!(
                    "[models.{nickname}] effort_levels contains unknown value {level:?}; \
                     valid values are: minimal, low, medium, high, xhigh, max"
                )));
            }
        }
    }
    Ok(())
}

/// The four config-facing classes a `[providers.X.class_overrides]` remap
/// may target. All four are terminal, non-retrying classes under the
/// baked defaults (see `class_policy::baked_class_defaults`) -- a remap
/// may only move a status INTO one of these, never toward a class the
/// router retries or uses for breaker/health accounting. Consumed by
/// [`validate_class_policy`].
const ALLOWED_REMAP_TARGETS: [crate::class_policy::ConfigFailureClass; 4] = [
    crate::class_policy::ConfigFailureClass::BadRequest,
    crate::class_policy::ConfigFailureClass::ContentPolicy,
    crate::class_policy::ConfigFailureClass::ContextWindow,
    crate::class_policy::ConfigFailureClass::FeatureUnsupported,
];

/// Render a [`crate::class_policy::ConfigFailureClass`] as the kebab-case
/// token it parses from / serializes to in TOML (e.g. `bad-request`), for
/// validator and warning messages that name a class. Delegates to the
/// canonical [`routectl_core::failure_class::FailureClass::class_token`] via
/// the config-to-canonical adapter, so validator, migrator, and `/status`
/// share one token vocabulary and cannot drift. Every
/// [`ConfigFailureClass`](crate::class_policy::ConfigFailureClass) maps to a
/// canonical class that yields a token (only `Unknown`, which the closed
/// config set never names, returns `None`).
pub(super) fn class_token(class: crate::class_policy::ConfigFailureClass) -> String {
    class
        .to_failure_class()
        .class_token()
        .expect("every ConfigFailureClass maps to a canonical class with a token")
        .to_string()
}

/// A status code the router's circuit breaker treats as a health signal:
/// 408 and 429 (explicit named rows) plus the whole 500..=599 range.
/// Distinct from "every 4xx/5xx is fallbackable" -- this is the narrower
/// set the breaker accounting cares about, used by
/// [`class_policy_warnings`] to flag a remap that would divert a health
/// signal away from that accounting.
pub(super) fn is_health_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

/// Validate the `[retry.classes]` overlay and every
/// `[providers.X.class_overrides]` remap for HARD policy violations that
/// the loader's serde layer cannot catch on its own (both are
/// syntactically valid TOML; these are policy rejects).
///
/// Two rejects:
///
///   - `[retry.classes.feature-unsupported]` is present. This class is
///     reserved: the baked class defaults already govern it, and an
///     override here has no path to take effect yet. Rejected outright
///     (rather than silently accepted as a no-op) so the operator does
///     not carry dead config; see the `[retry]` section of
///     `docs/CONFIGURATION.md` for the classes that do accept an
///     override. This reject is lifted by a later, targeted removal once
///     the class gains real override semantics.
///
///   - Any `[providers.X.class_overrides]` entry whose remap TARGET falls
///     outside `{bad-request, content-policy, context-window,
///     feature-unsupported}` (`ALLOWED_REMAP_TARGETS`). A remap may
///     only make behavior less aggressive -- move a status into one of
///     the terminal, non-retrying classes -- never toward a class the
///     router retries or debits for health. The error names the
///     provider, the source status, and the offending target so the
///     operator can fix the one line.
///
/// Call once per process startup alongside the other validators.
pub fn validate_class_policy(config: &crate::config::Config) -> Result<()> {
    use crate::class_policy::ConfigFailureClass;
    use routectl_core::Error;

    if config
        .retry
        .classes
        .contains_key(&ConfigFailureClass::FeatureUnsupported)
    {
        return Err(Error::Config(
            "[retry.classes.feature-unsupported]: this class is reserved -- the baked \
             defaults already govern it, so an override here is rejected. See the \
             [retry] section of docs/CONFIGURATION.md for the classes that accept an \
             override."
                .into(),
        ));
    }

    for (provider_name, entry) in &config.providers {
        for (status, target) in &entry.runtime().class_overrides {
            if !ALLOWED_REMAP_TARGETS.contains(target) {
                return Err(Error::Config(format!(
                    "[providers.{provider_name}.class_overrides] {status} = {}: a remap \
                     may only make behavior less aggressive -- the target must be one of \
                     bad-request, content-policy, context-window, feature-unsupported",
                    class_token(*target),
                )));
            }
        }
    }

    Ok(())
}

/// Validate that every `[registry]` key is a well-formed upstream-id
/// glob -- an exact id or a single trailing-`*` prefix. Embedded or bare
/// asterisks are rejected here at startup so the cost resolver
/// (`Config::pricing_for`) never silently skips a malformed key at query
/// time. The error names the offending key verbatim so an operator
/// running `routectl config check` sees exactly which key to fix.
///
/// Call once per process startup alongside the other validators.
pub fn validate_registry_patterns(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    for key in config.registry.keys() {
        crate::glob::AliasPattern::parse(key)
            .map_err(|e| Error::Config(format!("[registry.{key}]: invalid pattern: {e}")))?;
    }
    Ok(())
}

/// Validate that every `[aliases]` table key is a well-formed pattern --
/// an exact wire-model id or a single trailing-`*` prefix. Embedded or
/// bare asterisks are rejected here at startup; otherwise `Router::new`
/// warn-and-drops the malformed key and the request mis-routes (the
/// config check would still report "ok"). The error names the offending
/// key verbatim so an operator running `routectl config check` sees
/// exactly which key to fix.
///
/// This validates the alias KEYS (the patterns). It is distinct from
/// `validate_alias_chain_targets`, which validates the alias VALUES
/// (chain targets resolve to known, selectable models). Both must run.
///
/// Every key is parsed -- exact keys parse as `AliasPattern::Exact` and
/// always pass, so there is no need to gate on `contains('*')`.
pub fn validate_alias_patterns(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    for key in config.aliases.keys() {
        crate::glob::AliasPattern::parse(key)
            .map_err(|e| Error::Config(format!("[aliases.{key}]: invalid pattern: {e}")))?;
    }
    Ok(())
}

/// Validate that every entry in `[aliases]` resolves to a known and
/// selectable `[models.X]` nickname OR another alias key (recursive
/// expansion). Walks both `AliasValue::Single` and `AliasValue::Chain`;
/// accumulates every offending nickname into one consolidated startup
/// error so the operator gets the full list in one shot.
///
/// Failure modes:
///
///   - alias references a nickname that doesn't exist in `[models]`
///     and is not another alias key. Common cause: typo, or the
///     operator deleted a model row but forgot to update the alias.
///
///   - alias references a `selectable = false` nickname. The model
///     parses but the router refuses to dispatch to it; passing it
///     through as a route silently breaks at request time.
///
///   - empty `AliasValue::Chain([])`. An alias with no targets
///     resolves to `UnknownAlias` at request time, which is identical
///     to the alias not being declared at all -- surface the
///     misconfiguration at startup.
///
///   - cycle in the alias graph (e.g. `A = ["B"]`, `B = ["A"]`).
///     Detected via DFS over the alias keys; the error message names
///     the cycle path so the operator can break the loop. The
///     dispatch path also carries a runtime depth cap as belt-and-
///     suspenders, but cycles caught here never reach it.
///
/// Call once per process startup AFTER `validate_reasoning_defaults`
/// and BEFORE `build_resolved_models`. Glob keys (`claude-*` etc.)
/// are validated identically to exact keys -- the chain target must
/// still be a known nickname or alias key even though the alias key
/// matches a pattern.
pub fn validate_alias_chain_targets(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    let mut errors: Vec<String> = Vec::new();

    // Pass 1: empty-chain check + per-entry resolves-to-something
    // check (must be either a known model nickname OR another alias
    // key). Cycle detection is a separate pass below; it walks the
    // graph structure rather than per-entry semantics.
    for (alias, value) in &config.aliases {
        if value.is_empty() {
            errors.push(format!(
                "alias `{alias}`: chain is empty -- an alias with no targets \
                 resolves to UnknownAlias at request time, which is the same \
                 as not declaring the alias at all"
            ));
            continue;
        }
        for nickname in value.nicknames() {
            // An entry may either be another alias key (recursive
            // expansion) OR a model nickname. Alias keys win on
            // collision (matches the dispatch-time shadowing rule).
            if config.aliases.contains_key(nickname) {
                continue;
            }
            match config.models.get(nickname) {
                None => {
                    errors.push(format!(
                        "alias `{alias}`: target `{nickname}` is not a known \
                         model nickname in [models] and is not an alias key"
                    ));
                }
                Some(model) if !model.selectable => {
                    errors.push(format!(
                        "alias `{alias}`: target `{nickname}` is declared but \
                         `selectable = false`; alias chains must reference \
                         selectable models"
                    ));
                }
                Some(_) => {}
            }
        }
    }

    // Pass 2: cycle detection via DFS. Each connected component is
    // walked once -- `globally_visited` short-circuits keys that have
    // already been fully explored from another start point. Cycles
    // are recorded with the offending path so the operator can break
    // the loop.
    let mut globally_visited: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for start in config.aliases.keys() {
        if globally_visited.contains(start) {
            continue;
        }
        let mut path: Vec<String> = Vec::new();
        let mut path_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        detect_alias_cycles_dfs(
            &config.aliases,
            start,
            &mut path,
            &mut path_set,
            &mut globally_visited,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors.join("\n")))
    }
}

/// DFS helper for cycle detection in the alias graph. `path` /
/// `path_set` track the currently-active recursion stack; a chain
/// entry that hits the stack means we have a back-edge (cycle).
/// `globally_visited` is the standard DFS "fully explored" set so
/// each connected component is traversed once.
///
/// Errors are pushed into `errors` and accumulate alongside the
/// per-entry diagnostics from pass 1 of
/// `validate_alias_chain_targets`. Reuses `Error::Config` rather than
/// introducing a new variant -- the message carries the cycle path
/// (e.g. `alias `foo`: cycle detected: foo -> bar -> baz -> foo`).
fn detect_alias_cycles_dfs(
    aliases: &BTreeMap<String, crate::config::AliasValue>,
    current: &str,
    path: &mut Vec<String>,
    path_set: &mut std::collections::BTreeSet<String>,
    globally_visited: &mut std::collections::BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    if path_set.contains(current) {
        // Back-edge: cycle. The cycle starts at the position in
        // `path` where `current` first appears and closes by re-
        // visiting `current`. Attribute the diagnostic to the FIRST
        // alias actually in the cycle (`path[idx]`), not the DFS
        // root, so the operator's eye lands on the alias that
        // closes the loop. With external feeders like
        // `c -> a -> b -> a`, the report names `a` (the cycle's
        // entry) rather than `c` (which merely points into it).
        let idx = path
            .iter()
            .position(|p| p == current)
            .expect("path_set/path invariant: current must be present in path");
        let mut cycle_path: Vec<&str> = path[idx..].iter().map(String::as_str).collect();
        cycle_path.push(current);
        let entry_alias = path[idx].clone();
        errors.push(format!(
            "alias `{entry_alias}`: cycle detected: {}",
            cycle_path.join(" -> ")
        ));
        return;
    }
    if globally_visited.contains(current) {
        return;
    }
    let Some(value) = aliases.get(current) else {
        // Not an alias key; either a model nickname (handled in
        // pass 1) or a dangling reference (also handled in pass 1).
        // Either way, no cycle can pass through a non-alias leaf.
        return;
    };
    path.push(current.to_string());
    path_set.insert(current.to_string());
    for entry in value.nicknames() {
        detect_alias_cycles_dfs(aliases, entry, path, path_set, globally_visited, errors);
    }
    path.pop();
    path_set.remove(current);
    globally_visited.insert(current.to_string());
}

/// The only host the MITM front is ever allowed to terminate or egress
/// to. The milestone constraint is a HARD "the forwarded full-scope
/// claude.ai token must NEVER be sent to a non-Anthropic egress"; both
/// checks below pin to this exact string (never a suffix/subdomain
/// match) so a mistyped or poisoned config cannot widen the blast
/// radius of that token.
const MITM_REQUIRED_HOST: &str = "api.anthropic.com";

/// Validate the optional `[mitm]` block: `Ok` when the block is
/// absent (feature off, matching the `[server.auth]` presence-gates
/// convention). When present, reject:
/// - an `upstream_origin` that is not EXACTLY `https://api.anthropic.com`
///   (any other scheme, host, explicit port, userinfo, path, query, or
///   fragment is rejected -- see `MITM_REQUIRED_HOST` for why this is
///   pinned rather than pattern-matched);
/// - a `listen_port` that collides with `[server] port` (the two are
///   separate bound sockets on the same host);
/// - a `mitm_host` that is not EXACTLY `api.anthropic.com`.
///
/// Call once per process startup alongside the other validators. This
/// function only validates the config schema -- building and spawning
/// the proxy listener itself lives in `routectl_cli::proxy` /
/// `routectl_cli::server::serve_on_listener`, not here.
pub fn validate_mitm_config(config: &crate::config::Config) -> Result<()> {
    use routectl_core::Error;

    let Some(mitm) = &config.mitm else {
        return Ok(());
    };

    let is_pinned_origin = url::Url::parse(&mitm.upstream_origin).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(MITM_REQUIRED_HOST)
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none()
    });
    if !is_pinned_origin {
        return Err(Error::Config(format!(
            "[mitm] upstream_origin {:?} must be exactly \
             https://{MITM_REQUIRED_HOST} -- no userinfo, path, query, or fragment, and no \
             other host. This is a hard containment guarantee: the MITM proxy forwards the \
             client's full-scope claude.ai token, which must never be sent to a \
             non-Anthropic egress",
            mitm.upstream_origin
        )));
    }

    if mitm.listen_port == config.server.port {
        return Err(Error::Config(format!(
            "[mitm] listen_port {} collides with [server] port {}; the MITM \
             listener and the routectl HTTP server must bind different ports",
            mitm.listen_port, config.server.port
        )));
    }

    if mitm.mitm_host != MITM_REQUIRED_HOST {
        return Err(Error::Config(format!(
            "[mitm] mitm_host {:?} must be exactly {MITM_REQUIRED_HOST:?} -- no other host \
             (including a subdomain) is accepted. This is a hard containment guarantee: the \
             MITM proxy forwards the client's full-scope claude.ai token, which must never be \
             presented to a non-Anthropic host",
            mitm.mitm_host
        )));
    }

    Ok(())
}

/// Reject an incoherent provider-level `credential_source` on any
/// `[providers.X]` `anthropic-api` entry, for every provider in
/// `config.providers` (not just the ones a request happens to route
/// through). Runs on every config-validation path, not only serve
/// startup -- see `commands::config::check`, `commands::test::run`, and
/// `commands::prompt_size::run` in routectl-cli.
///
/// - `Forwarded` requires an EMPTY `api_key_ref` AND `base_url` pinned
///   to `api.anthropic.com` (`is_anthropic_api_host`). This is a hard
///   containment guarantee: a forwarded provider carries the client's
///   full-scope claude.ai bearer, which must never be sent to a
///   non-Anthropic egress -- pinning the host at config-validation time
///   makes containment true by construction, mirroring
///   `validate_mitm_config`'s host pin.
/// - `Own` requires a non-empty `api_key_ref`, exactly as before this
///   field existed on the variant.
pub fn validate_provider_credential_sources(config: &Config) -> Result<()> {
    use routectl_core::Error;

    for (name, entry) in &config.providers {
        let ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url,
            credential_source,
            #[cfg(feature = "bedrock")]
            bedrock_mantle,
            ..
        } = entry
        else {
            continue;
        };

        // The Bedrock mantle lane authenticates with `bedrock_mantle.creds`,
        // not `api_key_ref`, so it legitimately runs `own` with an empty
        // `api_key_ref`. `validate_provider_bedrock_mantle` owns the
        // coherence checks for that lane; the `own`-requires-a-key rule
        // below must not fire on it.
        #[cfg(feature = "bedrock")]
        let is_mantle_lane = bedrock_mantle.is_some();
        #[cfg(not(feature = "bedrock"))]
        let is_mantle_lane = false;

        match credential_source {
            CredentialSource::Forwarded => {
                if !api_key_ref.is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: credential_source = \"forwarded\" must not set \
                         api_key_ref (got a non-empty value) -- a forwarded provider \
                         authenticates with the client's captured bearer, never a configured key"
                    )));
                }
                if !is_anthropic_api_host(base_url) {
                    return Err(Error::Config(format!(
                        "provider `{name}`: credential_source = \"forwarded\" requires \
                         base_url's host to be exactly api.anthropic.com -- a path, port, or \
                         credentials prefix on that host is fine, but no other host is \
                         accepted. This is a hard containment guarantee: a forwarded provider \
                         carries the client's full-scope claude.ai bearer, which must never be \
                         sent to a non-Anthropic egress. The configured base_url is withheld \
                         from this message because it may carry credentials in userinfo, a \
                         path, or a query; see your own config.toml for the value"
                    )));
                }
            }
            CredentialSource::Own => {
                if !is_mantle_lane && api_key_ref.is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: credential_source = \"own\" (the default) requires \
                         a non-empty api_key_ref"
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Reject an incoherent `bedrock_mantle` sub-config on any
/// `[providers.X]` `anthropic-api` entry. The mere PRESENCE of the
/// sub-table selects the Bedrock mantle lane, on which every other
/// credential/endpoint knob is derived from the sub-table itself --
/// `region` yields the endpoint host and SigV4 scope, `creds` carries the
/// credential. So the standard direct-to-Anthropic knobs must be left at
/// their neutral defaults; any operator value on them is a
/// misconfiguration that would silently contradict the lane. Runs on every
/// config-validation path (not only serve startup), same as
/// [`validate_provider_credential_sources`].
///
/// With `bedrock_mantle` set, this rejects:
///   - `auth_kind = "oauth-bearer"` -- the mantle lane authenticates via
///     the bearer key or SigV4 in `creds`, never a Claude-Code OAuth token
///     (whose identity headers / UA must never reach AWS).
///   - a non-default `credential_source` -- the credential comes from
///     `creds`, so `own` (the default) is the only coherent value.
///   - a non-empty `api_key_ref` -- `creds` is the single credential
///     source; a stray `api_key_ref` is dead config.
///   - a non-default `base_url` -- `region` is the single source of truth
///     for the endpoint; the factory derives the URL, so a manual
///     `base_url` would drift from the region.
///   - an empty / whitespace-only `region`.
#[cfg(feature = "bedrock")]
pub fn validate_provider_bedrock_mantle(config: &Config) -> Result<()> {
    use crate::config::default_anthropic_base;
    use routectl_core::Error;
    use routectl_providers::anthropic_api::AuthKind;

    for (name, entry) in &config.providers {
        let ProviderEntry::AnthropicApi {
            api_key_ref,
            base_url,
            auth_kind,
            credential_source,
            bedrock_mantle: Some(mantle),
            ..
        } = entry
        else {
            continue;
        };

        if *auth_kind == AuthKind::OauthBearer {
            return Err(Error::Config(format!(
                "provider `{name}`: bedrock_mantle is set but auth_kind = \"oauth-bearer\" -- \
                 the mantle lane authenticates with bedrock_mantle.creds (bearer key or SigV4), \
                 never a Claude Code OAuth token; remove auth_kind or set it to \"api-key\""
            )));
        }
        if *credential_source != CredentialSource::Own {
            return Err(Error::Config(format!(
                "provider `{name}`: bedrock_mantle is set but credential_source is not \"own\" \
                 -- the mantle lane's credential comes from bedrock_mantle.creds; remove \
                 credential_source or set it to \"own\""
            )));
        }
        if !api_key_ref.is_empty() {
            return Err(Error::Config(format!(
                "provider `{name}`: bedrock_mantle is set but api_key_ref is non-empty -- the \
                 mantle lane's credential comes from bedrock_mantle.creds; remove api_key_ref"
            )));
        }
        if base_url != &default_anthropic_base() {
            return Err(Error::Config(format!(
                "provider `{name}`: bedrock_mantle is set but base_url is not its default -- \
                 bedrock_mantle.region is the single source of truth for the endpoint (the \
                 factory derives the URL from it); remove base_url"
            )));
        }
        if mantle.region.trim().is_empty() {
            return Err(Error::Config(format!(
                "provider `{name}`: bedrock_mantle.region is empty -- it must name an AWS \
                 region (e.g. \"us-east-1\"); the endpoint host and SigV4 scope derive from it"
            )));
        }
    }

    Ok(())
}

/// Reject an incoherent Bedrock mantle lane on either OpenAI-shape provider
/// (`openai-compat` / `openai-responses`) and close the legacy bearer-only
/// surface. The sibling of [`validate_provider_bedrock_mantle`] for the
/// OpenAI lanes: the PRESENCE of a `bedrock_mantle` sub-table selects the
/// lane, on which the endpoint (`region`) and credential (`creds`) come from
/// the sub-table, so every other credential/endpoint knob must be neutral.
///
/// With `bedrock_mantle` set, this rejects on either lane:
///   - a non-empty `api_key_ref` -- `creds` is the single credential source.
///   - a non-empty `base_url` -- `region` derives the endpoint.
///   - an empty / whitespace-only `region`.
///
/// On `openai-responses` additionally:
///   - a set `account_id_ref` -- it belongs to the ChatGPT-OAuth surface,
///     never the mantle lane.
///   - a `store` key in `payload_extras` -- the Responses `store` flag is
///     forced off on the mantle lane, so operator config must not carry it.
///
/// LEGACY CLOSURE (`openai-responses`): `auth_kind = "bedrock-mantle"` with
/// NO `bedrock_mantle` block is a hard error naming the block form. The
/// bearer-only lane cannot meet the SigV4 posture and has no known
/// production user; the mantle lane is selected exclusively by the block.
/// When the block IS present the factory sets the runtime marker itself, so
/// `auth_kind` may be stated redundantly or omitted -- it never selects the
/// lane alone.
#[cfg(feature = "bedrock")]
pub fn validate_provider_openai_mantle(config: &Config) -> Result<()> {
    use routectl_core::Error;

    for (name, entry) in &config.providers {
        match entry {
            ProviderEntry::OpenaiCompat {
                api_key_ref,
                base_url,
                bedrock_mantle: Some(mantle),
                ..
            } => {
                if !api_key_ref.is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle is set but api_key_ref is non-empty -- \
                         the mantle lane's credential comes from bedrock_mantle.creds; remove \
                         api_key_ref"
                    )));
                }
                if !base_url.trim().is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle is set but base_url is non-empty -- \
                         bedrock_mantle.region is the single source of truth for the endpoint \
                         (the factory derives the URL from it); remove base_url"
                    )));
                }
                if mantle.region.trim().is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle.region is empty -- it must name an AWS \
                         region (e.g. \"us-east-1\"); the endpoint host and SigV4 scope derive \
                         from it"
                    )));
                }
            }
            #[cfg(feature = "openai-responses")]
            ProviderEntry::OpenaiResponses {
                api_key_ref,
                account_id_ref,
                base_url,
                auth_kind,
                payload_extras,
                bedrock_mantle,
                ..
            } => {
                if *auth_kind == OpenaiResponsesAuthKind::BedrockMantle && bedrock_mantle.is_none()
                {
                    return Err(Error::Config(format!(
                        "provider `{name}`: auth_kind = \"bedrock-mantle\" but no bedrock_mantle \
                         block is set -- the legacy bearer-only surface is closed; set \
                         [providers.{name}.bedrock_mantle] with region and creds to select the \
                         mantle lane"
                    )));
                }
                let Some(mantle) = bedrock_mantle else {
                    continue;
                };
                if !api_key_ref.is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle is set but api_key_ref is non-empty -- \
                         the mantle lane's credential comes from bedrock_mantle.creds; remove \
                         api_key_ref"
                    )));
                }
                if account_id_ref.is_some() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle is set but account_id_ref is set -- it \
                         belongs to the chatgpt-oauth surface, not the mantle lane; remove \
                         account_id_ref"
                    )));
                }
                if base_url.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle is set but base_url is non-empty -- \
                         bedrock_mantle.region is the single source of truth for the endpoint \
                         (the factory derives the URL from it); remove base_url"
                    )));
                }
                if payload_extras
                    .as_ref()
                    .and_then(|v| v.as_object())
                    .is_some_and(|obj| obj.contains_key("store"))
                {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle is set but payload_extras carries a \
                         `store` key -- the Responses store flag is forced off on the mantle lane \
                         and cannot be configured; remove it from payload_extras"
                    )));
                }
                if mantle.region.trim().is_empty() {
                    return Err(Error::Config(format!(
                        "provider `{name}`: bedrock_mantle.region is empty -- it must name an AWS \
                         region (e.g. \"us-east-1\"); the endpoint host and SigV4 scope derive \
                         from it"
                    )));
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Reject a present-but-empty required credential ref inside a
/// [`crate::config::BedrockCredsConfig`]. The config-check secret-ref parse walk skips
/// empty ref slots (an empty string is not a parseable secret URI), so
/// without this check an operator typo like `key_ref = ""` on a required
/// slot would pass config validation and only surface as a confusing
/// failure at provider build / first request time.
///
/// Rejected (after trim):
///   - `BearerKey.key_ref` empty.
///   - `Static.access_key_ref` or `Static.secret_key_ref` empty.
///   - `Static.session_token_ref` present-but-empty (`Some("")`) -- an
///     optional slot that is explicitly set to empty is still a typo.
///     Omitting it entirely (`None`) is valid.
///
/// `Profile` and `DefaultChain` carry no secret-ref slots to check. The
/// error names the provider and the offending field; no secret value is
/// echoed.
#[cfg(feature = "bedrock")]
fn validate_bedrock_creds(name: &str, creds: &crate::config::BedrockCredsConfig) -> Result<()> {
    use crate::config::BedrockCredsConfig;
    use routectl_core::Error;

    let empty = |field: &str| {
        Error::Config(format!(
            "provider `{name}`: bedrock creds `{field}` is set but empty; \
             give it a secret ref (env://, file://, or literal:) or remove the field \
             where it is optional"
        ))
    };

    match creds {
        BedrockCredsConfig::BearerKey { key_ref } => {
            if key_ref.trim().is_empty() {
                return Err(empty("key_ref"));
            }
        }
        BedrockCredsConfig::Static {
            access_key_ref,
            secret_key_ref,
            session_token_ref,
        } => {
            if access_key_ref.trim().is_empty() {
                return Err(empty("access_key_ref"));
            }
            if secret_key_ref.trim().is_empty() {
                return Err(empty("secret_key_ref"));
            }
            if session_token_ref
                .as_deref()
                .is_some_and(|s| s.trim().is_empty())
            {
                return Err(empty("session_token_ref"));
            }
        }
        BedrockCredsConfig::Profile { .. } | BedrockCredsConfig::DefaultChain => {}
    }

    Ok(())
}

/// Reject a present-but-empty required Bedrock credential ref wherever a
/// [`crate::config::BedrockCredsConfig`] appears: the native Bedrock lane (`creds`) and
/// all three `bedrock_mantle` lanes (`bedrock_mantle.creds` on
/// `anthropic-api` / `openai-compat` / `openai-responses`). One shared
/// per-descriptor check, so the four lanes cannot drift. Runs on every
/// config-validation path via [`collect_config_validation`], not only
/// serve startup.
#[cfg(feature = "bedrock")]
pub fn validate_bedrock_creds_refs(config: &Config) -> Result<()> {
    for (name, entry) in &config.providers {
        match entry {
            ProviderEntry::Bedrock { creds, .. } => validate_bedrock_creds(name, creds)?,
            ProviderEntry::AnthropicApi {
                bedrock_mantle: Some(mantle),
                ..
            }
            | ProviderEntry::OpenaiCompat {
                bedrock_mantle: Some(mantle),
                ..
            } => validate_bedrock_creds(name, &mantle.creds)?,
            #[cfg(feature = "openai-responses")]
            ProviderEntry::OpenaiResponses {
                bedrock_mantle: Some(mantle),
                ..
            } => validate_bedrock_creds(name, &mantle.creds)?,
            _ => {}
        }
    }
    Ok(())
}

/// Reject any float config leaf that is non-finite (NaN or +/-inf), plus a
/// non-positive `retry.backoff_multiplier`. Non-finite is the latent hole:
/// NaN and inf both slip past the `wm < sentinel` / `rm <= 0.0` overlay
/// checks, and a non-finite multiplier turns backoff duration math into a
/// runaway sleep. The covered leaves are pinned against the schema's
/// `type: number` set by `float_leaf_coverage_matches_schema`.
fn validate_float_fields(config: &Config) -> Result<()> {
    use routectl_core::Error;

    let mul = config.retry.backoff_multiplier;
    if !mul.is_finite() {
        return Err(Error::Config(format!(
            "retry.backoff_multiplier is `{mul}`; must be a finite number"
        )));
    }
    if mul <= 0.0 {
        return Err(Error::Config(format!(
            "retry.backoff_multiplier is `{mul}`; must be greater than 0"
        )));
    }

    for (key, entry) in &config.registry {
        let Some(pricing) = entry.pricing.as_ref() else {
            continue;
        };
        let leaves: [(&str, Option<f64>); 5] = [
            ("input_per_mtok", pricing.input_per_mtok),
            ("output_per_mtok", pricing.output_per_mtok),
            ("cache_read_per_mtok", pricing.cache_read_per_mtok),
            ("cache_write_5m_per_mtok", pricing.cache_write_5m_per_mtok),
            ("cache_write_1h_per_mtok", pricing.cache_write_1h_per_mtok),
        ];
        for (field, value) in leaves {
            let Some(v) = value else { continue };
            if !v.is_finite() {
                return Err(Error::Config(format!(
                    "registry.`{key}`.pricing.{field} is `{v}`; must be a finite number"
                )));
            }
        }
    }

    for (key, override_entry) in &config.cache_pricing {
        let leaves: [(&str, Option<f32>); 5] = [
            ("wm", override_entry.wm),
            ("rm", override_entry.rm),
            ("storage_rent", override_entry.storage_rent),
            ("input_cost_per_token", override_entry.input_cost_per_token),
            (
                "output_cost_per_token",
                override_entry.output_cost_per_token,
            ),
        ];
        for (field, value) in leaves {
            let Some(v) = value else { continue };
            if !v.is_finite() {
                return Err(Error::Config(format!(
                    "cache_pricing.`{key}`.{field} is `{v}`; must be a finite number"
                )));
            }
        }
    }

    Ok(())
}

/// Reject a present-but-empty (or whitespace-only) `base_url` on any
/// provider entry. An explicit `base_url = ""` is an operator typo;
/// silently routing to a vendor default they did not name is the
/// surprising behavior. A field that is omitted entirely keeps its
/// serde default (`None` / kind-default) and is left untouched.
fn validate_base_urls(config: &Config) -> Result<()> {
    use routectl_core::Error;

    for (name, entry) in &config.providers {
        // openai-compat base_url is REQUIRED (non-empty) on the standard
        // lane -- it has no kind default. It is defaulted (empty) in the
        // schema only so the mantle lane may omit it; there the factory
        // derives the URL from bedrock_mantle.region, so an empty value is
        // valid and this check is skipped (validate_provider_openai_mantle
        // owns the mantle-lane coherence instead).
        if let ProviderEntry::OpenaiCompat { base_url, .. } = entry {
            #[cfg(feature = "bedrock")]
            if matches!(
                entry,
                ProviderEntry::OpenaiCompat {
                    bedrock_mantle: Some(_),
                    ..
                }
            ) {
                continue;
            }
            if base_url.trim().is_empty() {
                return Err(Error::Config(format!(
                    "provider `{name}`: openai-compat base_url is required and must be non-empty; \
                     set an explicit endpoint URL"
                )));
            }
            continue;
        }

        let is_empty = match entry {
            ProviderEntry::OpenaiCompat { .. } => false,
            ProviderEntry::AnthropicApi { base_url, .. } => base_url.trim().is_empty(),
            #[cfg(feature = "gemini")]
            ProviderEntry::Gemini { base_url, .. } => base_url.trim().is_empty(),
            #[cfg(feature = "openai-responses")]
            ProviderEntry::OpenaiResponses { base_url, .. } => {
                base_url.as_deref().is_some_and(|s| s.trim().is_empty())
            }
            #[cfg(feature = "bedrock")]
            ProviderEntry::Bedrock { .. } => false,
        };
        if is_empty {
            return Err(Error::Config(format!(
                "provider `{name}`: base_url is set but empty; \
                 set an explicit endpoint or omit the field to use the kind default"
            )));
        }
    }

    Ok(())
}

/// Explicit upper bound on an operator-supplied `codex_version`. Real
/// codex CLI versions are short (`X.Y.Z` plus at most a pre-release
/// suffix); a value past this is a fat-finger or an injection attempt and
/// fails fast rather than reaching the wire.
const CODEX_VERSION_MAX_LEN: usize = 64;

/// Validate every `codex_version` knob across the provider table. Two
/// hard-fail conditions: a syntactically illegal value (empty, too long,
/// or carrying a byte that is not printable ASCII -- whitespace, control,
/// and non-ASCII are all rejected, and the value is never sanitized), and
/// two providers setting DIFFERENT values (the codex identity is
/// process-global, so a silent winner is forbidden -- the error names both
/// providers). Providers that omit the knob inherit the resolved value.
pub fn validate_codex_version(config: &Config) -> Result<()> {
    use routectl_core::Error;

    let mut seen: Option<(&str, &str)> = None;
    for (name, entry) in &config.providers {
        let Some(version) = entry.codex_version() else {
            continue;
        };
        validate_codex_version_syntax(name, version)?;
        match seen {
            None => seen = Some((name.as_str(), version)),
            Some((prev_name, prev_version)) if prev_version != version => {
                return Err(Error::Config(format!(
                    "providers `{prev_name}` and `{name}` set different codex_version values \
                     (`{prev_version}` vs `{version}`); the codex identity is process-global, so \
                     every openai-responses provider that sets codex_version must agree on one \
                     value"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

pub(super) fn validate_codex_version_syntax(provider_name: &str, version: &str) -> Result<()> {
    use routectl_core::Error;

    if version.is_empty() {
        return Err(Error::Config(format!(
            "provider `{provider_name}`: codex_version is empty; omit the field to use the \
             pinned default or set a non-empty version"
        )));
    }
    if version.len() > CODEX_VERSION_MAX_LEN {
        return Err(Error::Config(format!(
            "provider `{provider_name}`: codex_version is {} bytes; the maximum is \
             {CODEX_VERSION_MAX_LEN}",
            version.len()
        )));
    }
    // Printable ASCII only (0x21..=0x7e): excludes space, tab, control
    // bytes, DEL, and every non-ASCII byte. The value is stamped verbatim
    // into an HTTP header and the User-Agent, so anything outside this set
    // is either header-illegal or a fingerprint the operator did not
    // intend.
    if let Some(bad) = version.bytes().find(|b| !(0x21..=0x7e).contains(b)) {
        return Err(Error::Config(format!(
            "provider `{provider_name}`: codex_version contains an illegal byte {bad:#04x}; \
             only printable ASCII (no whitespace or control characters) is allowed"
        )));
    }
    Ok(())
}

/// The single `codex_version` value configured across the provider table,
/// or `None` when no provider sets one (the caller falls back to the
/// pinned default). Assumes [`validate_codex_version`] has already
/// rejected divergent values, so returning the first configured value is
/// authoritative.
pub fn resolved_codex_version(config: &Config) -> Option<String> {
    config
        .providers
        .values()
        .find_map(|entry| entry.codex_version().map(str::to_owned))
}

/// Collected outcome of the shared config-validation suite:
/// `errors` are hard-fail conditions, `warnings` are advisory.
///
/// Every string is a BARE message with no leading error-kind prefix: the
/// `Error::Config` Display prefix (`config: `) of the `Error`-returning
/// validators is stripped here once, and the `String`-returning validators
/// are already bare. Each caller owns its own wrapping -- `config check`
/// and the `serve` pre-parse gate re-add a `config: ` prefix for their
/// rendered output, while `test` / `prompt-size` re-wrap in `Error::Config`
/// (whose Display re-adds the same prefix). Keeping the collection uniform
/// means no caller has to normalize the prefix itself.
#[derive(Debug, Default, Clone)]
pub struct ConfigValidation {
    /// Hard-fail conditions, each a bare message.
    pub errors: Vec<String>,
    /// Advisory conditions, each a bare message.
    pub warnings: Vec<String>,
}

/// Reduce a validator `Error` to its bare message. The suite's
/// `Error`-returning validators all produce `Error::Config`, whose Display
/// carries a `config: ` prefix; take the inner message directly so every
/// collected string is uniformly bare. Any other variant (none expected on
/// this path) falls back to its full Display.
fn bare_validation_message(e: routectl_core::Error) -> String {
    match e {
        routectl_core::Error::Config(msg) => msg,
        other => other.to_string(),
    }
}

/// Run the whole `validate_*` suite in ONE deterministic order and
/// collect every error + warning. This is the single ordered invocation
/// point every config surface (`config check`, `test`, `prompt-size`, and
/// the `serve` pre-parse gate) routes through, so a validator can never be
/// silently missing from one path while present in another. The error
/// taxonomy is unchanged: each check rejects exactly the configs it
/// rejected before; only the invocation is centralized.
///
/// The `[mitm]` validator is intentionally NOT part of this suite -- it is
/// specific to the router build path and stays there.
pub fn collect_config_validation(config: &Config) -> ConfigValidation {
    let mut errors: Vec<String> = Vec::new();

    if let Err(e) = crate::config::validate_cache_pricing_retired(config) {
        errors.push(e);
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = validate_bedrock_global_config(config) {
        errors.push(bare_validation_message(e));
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = validate_bedrock_invoke_model_family(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_reasoning_defaults(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_alias_chain_targets(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_alias_patterns(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_registry_patterns(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_class_policy(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_provider_credential_sources(config) {
        errors.push(bare_validation_message(e));
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = validate_provider_bedrock_mantle(config) {
        errors.push(bare_validation_message(e));
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = validate_provider_openai_mantle(config) {
        errors.push(bare_validation_message(e));
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = validate_bedrock_creds_refs(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = crate::catalog::validate_overrides(&config.cache_pricing) {
        errors.push(e);
    }
    if let Err(e) = crate::override_registry::validate_capability_overrides(config) {
        errors.push(e);
    }
    if let Err(e) = validate_float_fields(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_base_urls(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_codex_version(config) {
        errors.push(bare_validation_message(e));
    }

    let mut warnings = class_policy_warnings(config);
    warnings.extend(super::warnings::codex_identity_warnings(config));

    ConfigValidation { errors, warnings }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;
