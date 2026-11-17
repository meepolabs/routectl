//! Config-row validation family + validation collection.

use super::build::ipv4_compatible_embedded;
use super::plan::plan_coherent_entry;
use crate::config::entry_shape::{parse_base_url, shape_refusals};
use crate::config::provider_descriptor::{Format, vendor_containment, vendor_row};
use crate::config::{Config, PoolEntry, PoolKey, PoolKeyField, ProviderEntry};
use routectl_core::capability::{REASONING_REPLAY, WELL_KNOWN_CAPABILITY_KEYS};
use routectl_core::{Result, render_operator_name};
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use std::collections::BTreeMap;

/// Reject cleartext `http://` base_urls unless the parsed host is loopback:
/// an IPv4 literal in `127.0.0.0/8`, native `::1`, IPv4-mapped loopback, or
/// the exact name `localhost`. Every other DNS name needs `https://`, even
/// one whose text begins with `127.`.
///
/// Link-local hosts (IPv4 `169.254.0.0/16`, IPv6 `fe80::/10`) are rejected
/// regardless of scheme: they cover cloud-metadata services, and egress
/// there would forward credentials to an untrusted endpoint.
pub(super) fn validate_base_url_scheme(provider_name: &str, base_url: &str) -> Result<()> {
    let url = parse_base_url(provider_name, base_url).map_err(routectl_core::Error::Config)?;
    let scheme = url.scheme();

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
    if url.host().is_some_and(|h| is_cleartext_loopback_host(&h)) {
        return Ok(());
    }
    Err(routectl_core::Error::Config(format!(
        "provider `{provider_name}`: base_url uses cleartext http:// for a \
         non-loopback host -- API keys and prompt content would be sent in \
         the clear. Use https:// (or bind a local proxy on 127.0.0.1)"
    )))
}

/// Classify on the parsed host variant, never on host text: a DNS name
/// such as `127.evil.example` can resolve anywhere. The IPv4-compatible
/// form (`::a.b.c.d`) is deliberately not loopback -- it is not native
/// `::1` and may follow an ordinary IPv6 route.
fn is_cleartext_loopback_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(domain) => *domain == "localhost",
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Validate the `account_id_ref` invariant for an openai-responses
/// entry, by the dialect its destination selects. A misconfigured TOML surfaces here as a clean `Error::Config`
/// rather than a confusing upstream 401/403 at first request time.
///
/// Rules:
///   - Codex host + `oauth://<provider>` credential: `account_id_ref` is
///     OPTIONAL. When omitted, the factory derives the account id from
///     the logged-in OAuth session (the `chatgpt_account_id` recorded
///     at `routectl login`). An explicit `account_id_ref` is still
///     accepted and wins as an override.
///   - Codex host + static credential (`env://`/`file://`):
///     `account_id_ref` is REQUIRED. There is no OAuth session to read
///     the account id from, so the operator must supply it -- this is
///     the legacy chatgpt-oauth workflow, kept unchanged.
///   - any other destination: `account_id_ref` is FORBIDDEN (the account
///     id is a ChatGPT Codex concept).
///
/// `bearer_is_oauth` is whether the planned credential is an `oauth://`
/// reference; the caller computes it once and passes it in.
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
                "openai-responses provider `{name}`: `account_id_ref` is only valid with a \
                 base_url on the ChatGPT Codex host; remove it"
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
        "openai-responses provider `{name}`: the ChatGPT Codex host with a static \
         credential requires `account_id_ref` (the ChatGPT account UUID). Use an \
         `oauth://codex` credential to derive it from a logged-in session instead."
    )))
}

/// The Responses dialect an `openai-responses` entry over a base URL speaks:
/// the ChatGPT Codex dialect on the Codex host, the OpenAI API dialect
/// anywhere else. Destination picks the dialect; the credential does not.
#[cfg(feature = "openai-responses")]
pub(super) fn responses_base_url_dialect(entry: &ProviderEntry) -> OpenaiResponsesAuthKind {
    if entry.is_chatgpt_oauth_responses() {
        OpenaiResponsesAuthKind::ChatgptOauth
    } else {
        OpenaiResponsesAuthKind::ApiKey
    }
}

/// Run [`validate_openai_responses_account_id`] over every
/// `openai-responses` entry over a base URL, so `config check`, `serve`
/// startup and hot reload refuse the same account-id shapes the provider
/// factory refuses. A cloud-carrier entry takes no account id; the cloud
/// rule below refuses one there.
///
/// Parses the credential only to classify it as an `oauth://` reference; no
/// secret is resolved or read.
#[cfg(feature = "openai-responses")]
fn validate_openai_responses_account_ids(config: &Config) -> Result<()> {
    use routectl_auth::{CredentialRef, SecretRef};

    for (name, entry) in &config.providers {
        if entry.format() != Format::OpenaiResponses {
            continue;
        }
        if entry.cloud.is_some() {
            if entry.account_id_ref.is_some() {
                return Err(routectl_core::Error::Config(format!(
                    "openai-responses provider `{name}`: `account_id_ref` is only valid with a \
                     base_url on the ChatGPT Codex host; remove it"
                )));
            }
            continue;
        }
        let bearer_is_oauth = matches!(
            CredentialRef::parse(&entry.credential),
            Ok(CredentialRef::Secret(SecretRef::OAuth { .. }))
        );
        validate_openai_responses_account_id(
            name,
            responses_base_url_dialect(entry),
            bearer_is_oauth,
            &entry.account_id_ref,
        )?;
    }
    Ok(())
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
/// and it would leave `format` with a configured value distinct from
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
    use routectl_core::Error;
    use routectl_providers::anthropic_family::{AnthropicFamily, anthropic_family};

    let mut errors: Vec<String> = Vec::new();
    for (nickname, model) in &config.models {
        let Some(provider) = config.providers.get(&model.provider) else {
            continue;
        };
        if provider.format() != Format::BedrockInvoke {
            continue;
        }
        if anthropic_family(&model.upstream) == AnthropicFamily::No {
            errors.push(format!(
                "[models.{nickname}] upstream `{upstream}` is not an Anthropic-family \
                 model, but its provider `{provider}` is configured with \
                 `format = \"bedrock-invoke\"`. The invoke lane sends and parses the \
                 Anthropic wire shape, so this model cannot work on it: set \
                 `format = \"bedrock-converse\"` on [providers.{provider}] (the \
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

/// Validate the `[retry.classes]` counts for a HARD policy violation the
/// loader's serde layer cannot catch on its own (syntactically valid TOML,
/// rejected by policy): a `feature-unsupported` count is present.
///
/// This class is reserved: the baked class defaults already govern it, and
/// an override here has no path to take effect yet. Rejected outright
/// (rather than silently accepted as a no-op) so the operator does not
/// carry dead config; see the `[retry]` section of `docs/CONFIGURATION.md`
/// for the classes that do accept an override. This reject is lifted by a
/// later, targeted removal once the class gains real override semantics.
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
            "[retry.classes] feature-unsupported: this class is reserved -- the built-in \
             default already governs it, so a count here is rejected; delete the line. \
             See the [retry] section of docs/CONFIGURATION.md for the classes that \
             accept a count."
                .into(),
        ));
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
/// (chain targets resolve to known models). Both must run.
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

/// Validate that every entry in `[aliases]` resolves to a known
/// `[models.X]` nickname OR another alias key (recursive
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
/// Call once per process startup BEFORE `build_resolved_models`. Glob keys
/// (`claude-*` etc.) are validated identically to exact keys -- the chain target must
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
            if !config.models.contains_key(nickname) {
                errors.push(format!(
                    "alias `{alias}`: target `{nickname}` is not a known \
                     model nickname in [models] and is not an alias key"
                ));
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

/// Validate the optional `[mitm]` block: `Ok` when the block is
/// absent (feature off, matching the `[server.auth]` presence-gates
/// convention). When present, reject a `listen_port` that collides with
/// `[server] port` (the two are separate bound sockets on the same host).
/// The block names no upstream origin or host, so there is nothing else
/// to check: both are fixed in routectl-cli's proxy module.
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

    if mitm.listen_port == config.server.port {
        return Err(Error::Config(format!(
            "[mitm] listen_port {} collides with [server] port {}; the MITM \
             listener and the routectl HTTP server must bind different ports",
            mitm.listen_port, config.server.port
        )));
    }

    Ok(())
}

/// Contain the managed Anthropic subscription token to the Anthropic API
/// host. Any reference parsing to `oauth://anthropic[#label]` is accepted only
/// on an `anthropic-messages` entry with `auth = "bearer"`, no `cloud`, and a
/// `base_url` that is `https://` on exactly `api.anthropic.com` with no
/// userinfo or explicit port; every other format and every other host
/// (loopback included) is rejected. Static `env://` / `file://` credentials
/// are unaffected.
///
/// Pure and self-sufficient: the factory runs the containment of every
/// managed vendor, this one included, before resolving any secret for the
/// entry, since direct callers reach the factory without config validation.
/// The message names the provider and the required shape only; the
/// `base_url`, the reference, and the seat label are withheld because each
/// can carry credentials or identify a seat.
pub fn validate_managed_anthropic_credential(name: &str, entry: &ProviderEntry) -> Result<()> {
    let row = vendor_row("anthropic").ok_or_else(|| {
        routectl_core::Error::Internal("the descriptor has no anthropic vendor row".to_string())
    })?;
    vendor_containment(row, name, entry)
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

    Ok(())
}

/// Reject a `payload_extras.store` key on an `openai-responses` entry over
/// the AWS cloud: the Responses `store` flag is forced off on Bedrock's
/// managed surface and cannot be configured there.
fn validate_cloud_responses_store(config: &Config) -> Result<()> {
    for (name, entry) in &config.providers {
        let carries_store = entry
            .payload_extras
            .as_ref()
            .and_then(|v| v.as_object())
            .is_some_and(|obj| obj.contains_key("store"));
        if entry.format() == Format::OpenaiResponses && entry.cloud.is_some() && carries_store {
            return Err(routectl_core::Error::Config(format!(
                "provider `{name}`: payload_extras carries a `store` key -- the Responses \
                 store flag is forced off on the AWS cloud and cannot be configured; remove it \
                 from payload_extras"
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

/// The maximum number of members one pool may declare. Bounds pool
/// construction cost, the per-request dispatch chain a pool expands into,
/// and the per-member WARN volume a degraded pool emits. Well above any
/// plausible account count for one provider family; a config that exceeds
/// it is far more likely a generated mistake than an intent.
pub const MAX_POOL_MEMBERS: usize = 32;

/// Reject every incoherent `[pools.<name>]` block. One function so `config
/// check`, the serve gate, the hot-reload gate and doctor's config section
/// all apply the identical rule set -- every rejection class below fires on
/// each of those surfaces with no per-surface plumbing.
///
/// Rejection classes, each with its own message:
///
///   - a pool naming a provider entry that does not exist,
///   - a member that differs from the pool's first member on format,
///     destination, auth, OAuth vendor or cloak settings, one error per
///     differing setting (members must be interchangeable seats of
///     one upstream, which is what keeps pools and alias chains from
///     becoming two overlapping fallback mechanisms),
///   - a member claimed by two pools (a provider entry belongs to at most
///     one pool),
///   - a member authenticating with an API key rather than an OAuth seat,
///   - a pool name colliding with a provider entry name or with a model
///     nickname (a `[models.X] provider` value resolves against providers
///     and pools in one namespace, and the session-affinity namespace is
///     pool-or-nickname keyed),
///   - an empty member list (a pool that serves nothing is a silent
///     no-serve rather than a configuration),
///   - more than [`MAX_POOL_MEMBERS`] members.
///
/// Every class is collected rather than short-circuited, so one pass names
/// every problem in the pool table.
pub fn validate_pools(config: &Config) -> Result<()> {
    use routectl_core::Error;

    let mut errors: Vec<String> = Vec::new();
    let mut claimed_by: BTreeMap<&str, &str> = BTreeMap::new();

    for (pool_key_name, pool) in &config.pools {
        let pool_name = render_operator_name(pool_key_name);
        if config.providers.contains_key(pool_key_name) {
            errors.push(format!(
                "pool `{pool_name}`: a provider entry of the same name exists; a model's \
                 `provider` value resolves against providers and pools in ONE namespace, so \
                 a name matching both is ambiguous -- rename the pool or the provider entry"
            ));
        }
        if config.models.contains_key(pool_key_name) {
            errors.push(format!(
                "pool `{pool_name}`: a model nickname of the same name exists; session \
                 affinity is keyed by pool name or model nickname, so a name matching both \
                 would share one session pin between unrelated targets -- rename the pool or \
                 the model"
            ));
        }
        if pool.members.is_empty() {
            errors.push(format!(
                "pool `{pool_name}`: `members` is empty; an empty pool serves no traffic, \
                 which fails every request routed to it -- list at least one provider entry \
                 or remove the pool"
            ));
        }
        if pool.members.len() > MAX_POOL_MEMBERS {
            errors.push(format!(
                "pool `{pool_name}`: {} members exceeds the {MAX_POOL_MEMBERS}-member cap; \
                 split the accounts across more than one pool",
                pool.members.len()
            ));
        }

        let mut pool_key: Option<(&str, PoolKey)> = None;
        for member_key in &pool.members {
            let member = render_operator_name(member_key);
            if let Some(previous) = claimed_by.insert(member_key.as_str(), pool_key_name.as_str())
                && previous != pool_key_name.as_str()
            {
                let previous = render_operator_name(previous);
                errors.push(format!(
                    "provider `{member}` is a member of both pool `{previous}` and pool \
                     `{pool_name}`; a provider entry belongs to at most one pool"
                ));
            }
            let Some(entry) = config.providers.get(member_key) else {
                errors.push(format!(
                    "pool `{pool_name}`: member `{member}` is not a known provider entry in \
                     [providers]"
                ));
                continue;
            };
            let key = PoolKey::of(entry);
            match &pool_key {
                None => pool_key = Some((member_key.as_str(), key)),
                Some((first_member, first_key)) => {
                    errors.extend(first_key.differing_fields(&key).into_iter().map(|field| {
                        pool_mismatch_message(pool_key_name, first_member, member_key, field)
                    }))
                }
            }
            if !member_authenticates_with_oauth(entry) {
                errors.push(format!(
                    "pool `{pool_name}`: member `{member}` does not authenticate with an \
                     `oauth://` credential; pool members are OAuth accounts this release \
                     (API-key members are a planned widening, not a permanent restriction)"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Config(errors.join("\n")))
    }
}

/// One line naming a pool, two of its members and the one setting they
/// disagree on. Only the names and the field are rendered, never either
/// member's value: a base URL, region or credential reference does not
/// belong in a refusal that reaches operator stdout and the reload WARN.
fn pool_mismatch_message(
    pool_name: &str,
    first_member: &str,
    member: &str,
    field: PoolKeyField,
) -> String {
    format!(
        "pool `{}`: member `{}` differs from member `{}` on {}; every member of a pool must \
         share format, destination, auth, OAuth vendor and cloak settings (only per-seat \
         tuning may differ) -- use an alias chain for ordered fallback across different \
         upstreams",
        routectl_core::render_operator_name(pool_name),
        routectl_core::render_operator_name(member),
        routectl_core::render_operator_name(first_member),
        field.as_str(),
    )
}

/// Whether a provider entry's `credential` is a routectl-managed OAuth
/// seat. Companion references such as `account_id_ref` do not decide it:
/// they are not the credential the seat authenticates with.
fn member_authenticates_with_oauth(entry: &ProviderEntry) -> bool {
    matches!(
        routectl_auth::CredentialRef::parse(&entry.credential),
        Ok(routectl_auth::CredentialRef::Secret(
            routectl_auth::SecretRef::OAuth { .. }
        ))
    )
}

/// The single `codex_version` value configured across the provider table,
/// or `None` when no provider sets one (the caller falls back to the
/// pinned default). Assumes [`validate_codex_version`] has already
/// rejected divergent values, so returning the first configured value is
/// authoritative.
pub fn resolved_codex_version(config: &Config) -> Option<String> {
    configured_codex_version(config).map(|(_, version)| version.to_owned())
}

/// The `[providers.X]` entry whose `codex_version` [`resolved_codex_version`]
/// returns, with that value: `(X, version)`. `None` when no entry sets one.
pub fn configured_codex_version(config: &Config) -> Option<(&str, &str)> {
    config
        .providers
        .iter()
        .find_map(|(name, entry)| entry.codex_version().map(|v| (name.as_str(), v)))
}

/// Reject a config whose `[capability] essential` list names a key the
/// strip-vs-route consult can never act on.
///
/// The accept set is every well-known capability key plus every key the
/// baked strip table maps to a `Strip` action (which is what makes the set
/// auto-extend when a droppable is added in code), MINUS
/// [`REASONING_REPLAY`]: its strip is lane-managed rather than key-only, so
/// replay negatives never reach the consult and listing the key would pin a
/// meaningful-looking no-op into an operator-facing vocabulary. Listing a
/// key that already routes away is accepted as an idempotent declaration.
///
/// Returns the first offending entry in config order, naming the key inside
/// backticks so an entry whose only defect is invisible in plain prose (a
/// stray space) still shows its exact bounds, plus a did-you-mean hint when
/// the entry is an exact whitespace/case variant of an accepted key.
fn validate_capability_essential(config: &Config) -> Result<(), String> {
    for key in &config.capability.essential {
        if key == REASONING_REPLAY {
            return Err(format!(
                "[capability] essential lists `{REASONING_REPLAY}`, which this list \
                 cannot govern: replayed reasoning artifacts are dropped by the \
                 target lane's replay path, not by the key-only strip consult -- \
                 remove the entry"
            ));
        }
        let known = WELL_KNOWN_CAPABILITY_KEYS.contains(&key.as_str())
            || crate::capability_strip::is_strippable(key);
        if !known {
            let hint = match essential_key_near_miss(key) {
                Some((candidate, difference)) => {
                    format!("; did you mean `{candidate}`? the entry differs only by {difference}")
                }
                None => String::new(),
            };
            return Err(format!(
                "[capability] essential names unknown capability `{key}`{hint}"
            ));
        }
    }
    Ok(())
}

/// The `[capability] essential` accept set as one iterator: every well-known
/// capability key plus every baked-strippable key, minus the
/// [`REASONING_REPLAY`] carve-out. Mirrors the membership test in
/// [`validate_capability_essential`] so a suggestion can never name a key
/// that validator would itself reject.
fn essential_accept_set() -> impl Iterator<Item = &'static str> {
    WELL_KNOWN_CAPABILITY_KEYS
        .iter()
        .copied()
        .chain(crate::capability_strip::strippable_keys())
        .filter(|key| *key != REASONING_REPLAY)
}

/// The accepted key a rejected entry is an invisible variant of, paired with
/// the phrase naming the difference -- or `None` when the entry is a genuine
/// unknown.
///
/// The match is exact-after-normalization rather than fuzzy on purpose: the
/// defects worth a hint here are precisely the ones an operator cannot see
/// in the rendered key (surrounding whitespace, letter case), and for those
/// the suggestion is certain. A misspelling is visible in the quoted key
/// already, so it gets no guess instead of a possibly-wrong one.
fn essential_key_near_miss(key: &str) -> Option<(&'static str, &'static str)> {
    let trimmed = key.trim();
    let whitespace_differs = trimmed.len() != key.len();
    essential_accept_set().find_map(|candidate| {
        let difference = match (
            whitespace_differs,
            trimmed == candidate,
            trimmed.eq_ignore_ascii_case(candidate),
        ) {
            (true, true, _) => "surrounding whitespace",
            (true, false, true) => "surrounding whitespace and letter case",
            (false, false, true) => "letter case",
            _ => return None,
        };
        Some((candidate, difference))
    })
}

/// What a `[models.X] provider` value names: a `[pools]` block or a
/// `[providers]` entry.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum ModelProviderTarget<'a> {
    /// A `[pools.<name>]` block.
    Pool(&'a PoolEntry),
    /// A `[providers.<name>]` entry.
    Provider(&'a ProviderEntry),
}

/// Resolve a `[models.X] provider` value against `[pools]` and `[providers]`,
/// which share ONE namespace. [`validate_pools`] rejects a name held by both,
/// so on a validated config at most one lookup can hit; on an unvalidated
/// one the pool wins. `None` means the name is in neither table.
pub fn resolve_model_provider<'a>(
    config: &'a Config,
    provider: &str,
) -> Option<ModelProviderTarget<'a>> {
    if let Some(pool) = config.pools.get(provider) {
        return Some(ModelProviderTarget::Pool(pool));
    }
    config
        .providers
        .get(provider)
        .map(ModelProviderTarget::Provider)
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
    /// Hard-fail conditions, one bare message per finding.
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

/// Push each line of `joined` as its own entry. The validators that report
/// several findings at once join them one per line; the collection keeps
/// one entry per finding so every consumer can count and render them.
fn push_each_finding(errors: &mut Vec<String>, joined: &str) {
    errors.extend(joined.lines().map(str::to_owned));
}

/// Every refusal for one provider entry: each broken shape rule, then the
/// first refusal of its plan unless that is one of the shape messages
/// already collected.
fn provider_entry_errors(name: &str, entry: &ProviderEntry) -> Vec<String> {
    let name = render_operator_name(name);
    let mut errors = shape_refusals(&name, entry);
    if let Err(e) = plan_coherent_entry(&name, entry) {
        let message = bare_validation_message(e);
        if !errors.contains(&message) {
            errors.push(message);
        }
    }
    errors
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

    if let Err(e) = crate::config::validate_model_token_limits(config) {
        push_each_finding(&mut errors, &e);
    }
    if let Err(e) = crate::config::validate_server_body_limit(config) {
        errors.push(e);
    }
    if let Err(e) = crate::config::validate_model_pricing(config) {
        push_each_finding(&mut errors, &e);
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = validate_bedrock_invoke_model_family(config) {
        push_each_finding(&mut errors, &bare_validation_message(e));
    }
    if let Err(e) = validate_alias_chain_targets(config) {
        push_each_finding(&mut errors, &bare_validation_message(e));
    }
    if let Err(e) = validate_alias_patterns(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_class_policy(config) {
        errors.push(bare_validation_message(e));
    }
    for (name, entry) in &config.providers {
        errors.extend(provider_entry_errors(name, entry));
    }
    if let Err(e) = validate_cloud_responses_store(config) {
        errors.push(bare_validation_message(e));
    }
    #[cfg(feature = "bedrock")]
    if let Err(e) = super::validate_region::validate_aws_regions(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = crate::override_registry::validate_capability_overrides(config) {
        errors.push(e);
    }
    if let Err(e) = validate_capability_essential(config) {
        errors.push(e);
    }
    if let Err(e) = validate_float_fields(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_codex_version(config) {
        errors.push(bare_validation_message(e));
    }
    #[cfg(feature = "openai-responses")]
    if let Err(e) = validate_openai_responses_account_ids(config) {
        errors.push(bare_validation_message(e));
    }
    if let Err(e) = validate_pools(config) {
        push_each_finding(&mut errors, &bare_validation_message(e));
    }
    if let Err(e) = super::validate_state_keys::validate_state_key_names(config) {
        push_each_finding(&mut errors, &bare_validation_message(e));
    }

    let mut warnings = super::warnings::codex_identity_warnings(config);
    warnings.extend(super::warnings::cloudcode_host_warnings(config));
    warnings.extend(super::warnings::cloudcode_model_warnings(config));
    warnings.extend(super::beta_floor_warnings::beta_floor_override_warnings(
        config,
    ));

    ConfigValidation { errors, warnings }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod validate_tests;

#[cfg(test)]
#[path = "validate_one_per_finding_tests.rs"]
mod one_per_finding_tests;

#[cfg(test)]
#[path = "validate_name_render_tests.rs"]
mod name_render_tests;

#[cfg(test)]
#[path = "validate_loopback_vectors_tests.rs"]
mod loopback_vectors_tests;

#[cfg(test)]
#[path = "validate_managed_oauth_tests.rs"]
mod managed_oauth_tests;

#[cfg(all(test, feature = "openai-responses"))]
#[path = "validate_account_id_tests.rs"]
mod account_id_tests;
