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
/// validator and warning messages that name a class. Round-trips through
/// the type's own `Serialize` impl rather than a hand-duplicated match arm
/// list, so the two spellings cannot drift.
pub(super) fn class_token(class: crate::class_policy::ConfigFailureClass) -> String {
    serde_json::to_string(&class)
        .expect("ConfigFailureClass serialization is infallible")
        .trim_matches('"')
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
///     feature-unsupported}` ([`ALLOWED_REMAP_TARGETS`]). A remap may
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
///   fragment is rejected -- see [`MITM_REQUIRED_HOST`] for why this is
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
            ..
        } = entry
        else {
            continue;
        };

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
                         base_url's host to be exactly api.anthropic.com (got {base_url:?}) -- \
                         a path, port, or credentials prefix on that host is fine, but no other \
                         host is accepted. This is a hard containment guarantee: a forwarded \
                         provider carries the client's full-scope claude.ai bearer, which must \
                         never be sent to a non-Anthropic egress"
                    )));
                }
            }
            CredentialSource::Own => {
                if api_key_ref.is_empty() {
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
        let leaves: [(&str, Option<f32>); 3] = [
            ("wm", override_entry.wm),
            ("rm", override_entry.rm),
            ("storage_rent", override_entry.storage_rent),
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
        let is_empty = match entry {
            ProviderEntry::OpenaiCompat { base_url, .. }
            | ProviderEntry::AnthropicApi { base_url, .. } => base_url.trim().is_empty(),
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
    pub errors: Vec<String>,
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

    let warnings = class_policy_warnings(config);

    ConfigValidation { errors, warnings }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;
