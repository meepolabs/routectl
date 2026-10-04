//! Pure, provenance-annotated derivation of the effective config view that
//! backs `config show --effective`.
//!
//! Three surfaces have more than one layer competing for a value, so only
//! those carry provenance: a model's catalog cell (the compiled-in baked
//! table vs an operator overlay), a retry class's policy (the baked class
//! default vs a `[retry.classes.<class>]` leaf), and a capability cell (the
//! config-derived override layer, tagged with the source that set it). Every
//! other config field is trivially the value in `config.toml`; annotating it
//! would be noise.
//!
//! The derivation is a PURE function over `(&Config, &CatalogOverlay)`. It
//! runs the SAME `(provider_kind, upstream)` catalog lookups the router's
//! chain-build merge runs, so an operator inspecting the view sees exactly the
//! cells dispatch prices against -- with no secret resolution, no provider
//! construction, and no network.
//!
//! The capability layer carries only the config-derived override cells (the
//! same read-model the routing filter consults). Learned-capability negatives
//! are in-memory runtime state on a live router, not config, so they are out
//! of scope for this pure view.

use routectl_core::failure_class::FailureClass;
use routectl_providers::anthropic_api::AuthKind;
#[cfg(feature = "gemini")]
use routectl_providers::gemini::GeminiAuthMode;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;

use crate::catalog::{EffectiveRow, resolve_effective_row};
use crate::catalog_overlay::CatalogOverlay;
use crate::class_policy::ConfigFailureClass;
#[cfg(feature = "bedrock")]
use crate::config::BedrockCredsConfig;
use crate::config::{Config, ProviderEntry};
use crate::override_registry::{OverrideRegistry, OverrideRow};

/// The catalog cell a single `[models.X]` entry resolves to, tagged with the
/// `(provider_kind, upstream)` selector it was looked up under.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCell {
    /// The `[models.<nickname>]` key.
    pub nickname: String,
    /// The `provider` this model references. May name a `[providers]` entry
    /// or a `[pools]` block -- both live in one namespace.
    pub provider: String,
    /// The kind token the catalog cell was looked up under: the referenced
    /// provider entry's own kind, or for a pool-backed model a member's kind
    /// (see [`Config::provider_kind_for_target`]). Empty when the reference
    /// resolves to neither -- the same fallback the chain-build merge uses.
    pub provider_kind: String,
    /// The upstream model id forwarded to the provider.
    pub upstream: String,
    /// The merged catalog row and its winning-layer provenance.
    pub row: EffectiveRow,
}

/// The resolved retry/fallback policy for one failure class, tagged with the
/// layer that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassPolicyCell {
    /// The failure class this policy governs.
    pub class: ConfigFailureClass,
    /// The resolved same-provider retry cap.
    pub retry_cap: u32,
    /// Whether this class falls back to the next provider in the chain.
    pub fallback: bool,
    /// Which layer won.
    pub source: ClassPolicySource,
}

/// Which layer supplied a [`ClassPolicyCell`]'s value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassPolicySource {
    /// A `[retry.classes.<class>]` leaf the operator set.
    Config,
    /// The compiled-in baked class default (no operator leaf for this class).
    BakedDefault,
}

/// One `[aliases]` entry flattened into its ordered fallback chain. A
/// `Single` alias yields a one-element chain; a `Chain` keeps the operator's
/// order verbatim, since that order IS the fallback sequence dispatch walks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasChain {
    /// The `[aliases]` table key (may be a suffix-glob pattern).
    pub alias: String,
    /// The model nicknames this alias falls back through, in order.
    pub chain: Vec<String>,
}

/// The secret-safe projection of one `[providers.X]` entry: the routing-shape
/// facts a read surface may display, and nothing else.
///
/// Every field is either a closed-vocabulary token or a structurally
/// secret-free reduction of a config string. No field carries a resolved
/// credential, a secret-ref body, or a raw `base_url` -- the endpoint in
/// particular is reduced to its origin rather than copied, because a
/// `base_url` may legitimately embed a credential (see `endpoint_origin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCell {
    /// The `[providers.<id>]` table key.
    pub id: String,
    /// The entry's `kind = "..."` discriminant, from
    /// [`ProviderEntry::kind_str`].
    pub kind: String,
    /// The CONFIGURED endpoint origin (scheme + host + port), or `None` when
    /// the entry's config carries no endpoint or the value does not parse.
    ///
    /// `None` does NOT mean "no endpoint": Bedrock derives its host from
    /// `region`, both mantle lanes require `base_url` be left unset and derive
    /// it from `bedrock_mantle.region`, and an unset-`base_url`
    /// `openai-responses` entry gets an auth-kind-appropriate default. All
    /// three are resolved at FACTORY time, which a pure derivation over
    /// `&Config` cannot reach without duplicating factory logic and minting a
    /// second source of truth. This field reports what the CONFIG says.
    ///
    /// The one exception is a `cloud-code` Gemini entry with `base_url` left
    /// unset: reporting the api-key public base there would be a WRONG fact,
    /// so such an entry reports the effective cloud-code host instead.
    pub endpoint_origin: Option<String>,
    /// The URI SCHEME of the entry's primary `api_key_ref` (`env`, `file`,
    /// `oauth`, ...), never the ref body. `None` for a variant with no single
    /// canonical key slot, or a ref with no `scheme:` prefix.
    pub credential_ref_scheme: Option<String>,
    /// This variant's OWN existing serde auth token (`api-key`,
    /// `oauth-bearer`, `chatgpt-oauth`, `bedrock-mantle`, `cloud-code`,
    /// `bearer-key`, `static`, `profile`, `default-chain`), or `None` for
    /// `openai-compat`, which has no auth discriminant at all.
    ///
    /// There is deliberately NO cross-kind vocabulary here: each provider kind
    /// carries the token its own config already uses, so the projection adds no
    /// vocabulary of its own. The extraction point states the prohibition that
    /// keeps this field secret-free.
    pub auth_token: Option<String>,
    /// The configured requests-per-minute cap. Three states, none
    /// interchangeable: `Some(n)` with `n > 0` is a live limit, `Some(0)` seeds
    /// a zero-capacity bucket and therefore means IMMEDIATELY rate-limited, and
    /// `None` means unlimited.
    pub rpm_limit: Option<u32>,
}

/// Reduce a configured `base_url` to its ORIGIN: scheme, host, and port only.
///
/// A `base_url` may legitimately carry a credential. `validate_base_url_scheme`
/// rejects bad schemes, link-local targets, and cleartext-on-non-loopback, but
/// it does NOT reject userinfo -- only the `[mitm]` origin validator does. So
/// `https://user:<secret>@upstream.example/v1` is an ACCEPTED provider config,
/// and that raw string is what sits in the entry. Path and query are equally
/// unsafe: some compat gateways carry the key there.
///
/// So this reduction is the security boundary, not a formatting nicety:
/// userinfo, path, query, and fragment are DROPPED, and a value that does not
/// parse yields `None`. There is deliberately no raw-string fallback -- a
/// fallback would publish exactly the string this function exists to withhold.
///
/// The boundary is enforced HERE and deliberately does not lean on any
/// validator, because `validate_base_url_scheme` is reached only from the
/// per-model factory loop while this projection walks every `[providers.X]`
/// entry: a provider that no `[models.X]` references is projected having never
/// been scheme-checked at all. Two failure modes are therefore handled
/// explicitly rather than assumed away.
///
/// **Only `http` and `https` project.** For any other scheme the url crate
/// gives an OPAQUE host, which it does not percent-decode, so
/// `weird://h.example%40<secret>` would carry credential bytes straight through
/// `host_str()`. A scheme with no `//` authority (`mailto:`, `unix:/path`)
/// yields `None` merely because there is no host to read -- NOT because the
/// scheme was checked; the explicit gate is what covers the `//` cases.
///
/// **A `@` the parser did not read as userinfo yields `None`.** For a special
/// scheme the authority ends at the first `/`, so
/// `https://<key>:8080/x@h.example/v1` parses as host `<key>`, port 8080, with
/// the real host demoted into the path -- and a base64 secret routinely
/// contains a `/`. When the credential is in the USERNAME position (a real
/// pattern on some relays) that puts it directly into the projected origin.
/// Trusting `host_str()` to be the operator's intended host is the leak, so a
/// remainder carrying an unconsumed `@` is withheld whole.
///
/// Two crate-internal consumers depend on this being the ONLY base_url
/// projection: the effective-config view here, and
/// [`crate::config::ProviderEntry::redact_secrets`] via `redact_base_url`. Its
/// fail-safe `None` is the contract, not an inconvenience -- it must never be
/// softened into a raw-string or best-effort fallback for a caller that would
/// rather always have something to print.
///
/// `pub` only within this `pub(crate) mod`, matching `derive_effective_view`:
/// the crate's re-export list is explicit and omits this function, so it is not
/// part of the public API.
pub fn endpoint_origin(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Reconstructed field-by-field rather than by clearing components on the
    // parsed url: `Url::set_username`/`set_password` fail silently on
    // cannot-be-a-base URLs, which would leave userinfo in the output.
    let url = url::Url::parse(trimmed).ok()?;
    let scheme = url.scheme();
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    // The AUTHORITY as written: everything after `://` up to the first `/`,
    // `?`, or `#`. The parser's authority scan stops at that same first `/`,
    // and it takes userinfo from the LAST `@` before it -- so a SECOND `@`
    // lets a benign leading userinfo satisfy any "was userinfo consumed?"
    // question while the host slot is filled from the bytes between the two.
    // `https://x@<secret-with-a-slash>/y@h.example/v1` is the vector, and it is
    // why this predicate asks about the WRITTEN AUTHORITY rather than about
    // what the parse happened to consume: at most one `@` in the authority, and
    // none demoted past it into the path.
    let remainder = trimmed.split_once("://").map(|(_, rest)| rest)?;
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or(remainder);
    // BOTH clauses are load-bearing and neither subsumes the other, verified by
    // probe against the real parser:
    //   - two `@` INSIDE the authority: the parse takes userinfo from the LAST
    //     one, so a benign leading `x@` cannot be used to vouch for the rest.
    //   - any `@` AFTER the authority ends: in
    //     `https://x@<secret-with-a-slash>/y@h.example/v1` the written authority
    //     is just `x@<secret-prefix>` (one `@`, so the first clause passes) and
    //     the real host is demoted into the path -- the secret becomes the host.
    // Dropping the second clause reopens that leak; it was measured, not
    // assumed. The cost is that a legitimate `@` in a path or query withholds
    // the origin too (rendered as "derived at startup"): a fail-SAFE
    // imprecision, deliberately preferred over publishing credential bytes.
    let rest_after_authority = &remainder[authority.len()..];
    if authority.matches('@').count() > 1 || rest_after_authority.contains('@') {
        return None;
    }
    Some(match url.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

/// The leak-safe scheme of a secret ref (`env://VAR` -> `env`), never the body.
///
/// The body names a variable, a filesystem path, or an OAuth provider id; only
/// the scheme says WHERE the credential class lives, which is the whole of what
/// a read surface needs.
///
/// The vocabulary is an ALLOWLIST, matching the decision this codebase already
/// made for the doctor surface's `scheme_label`: an unrecognized ref is not a
/// ref with an exotic scheme, it is a bare value someone pasted into the field,
/// and a bare value IS secret material. `api_key_ref` is a plain `String` and
/// `SecretRef::parse` never runs for a provider no model references, so a
/// pasted `AKIA<id>:<secret>` reaches here intact -- returning the text before
/// its first colon would publish the key id. Anything off the list collapses to
/// `None`, echoing none of its bytes.
fn credential_ref_scheme(api_key_ref: Option<&str>) -> Option<String> {
    let raw = api_key_ref?.trim();
    let (scheme, _) = raw.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    matches!(scheme.as_str(), "env" | "file" | "oauth" | "literal").then_some(scheme)
}

/// Extract the entry's own auth-mechanism token.
///
/// PROHIBITION -- this is the single extraction point, so the rule lives here:
/// the returned token is a CLOSED-VOCABULARY auth-MECHANISM name. It must never
/// encode a credential path, a secret-store name, an OAuth provider id, an
/// account id, a region, or any other operator-supplied value. Every token
/// below is a compile-time `&'static str` from a closed enum, which is what
/// makes that hold today.
///
/// Both matches are deliberately EXHAUSTIVE with no wildcard arm, so a new
/// provider variant or a new auth discriminant breaks the build HERE rather
/// than silently defaulting. Whoever fixes that break must supply another
/// closed-vocabulary token -- or, if the new discriminant cannot be reduced to
/// one, return `Some("redacted")` instead of passing its value through. Adding
/// a wildcard to make the break go away would turn a display field into a
/// credential-shape disclosure.
const fn auth_token(entry: &ProviderEntry) -> Option<&'static str> {
    match entry {
        // No auth discriminant exists on this variant: the credential ref is
        // the only auth fact, and its scheme is reported separately.
        ProviderEntry::OpenaiCompat { .. } => None,
        ProviderEntry::AnthropicApi { auth_kind, .. } => Some(match auth_kind {
            AuthKind::ApiKey => "api-key",
            AuthKind::OauthBearer => "oauth-bearer",
        }),
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses { auth_kind, .. } => Some(match auth_kind {
            OpenaiResponsesAuthKind::ChatgptOauth => "chatgpt-oauth",
            OpenaiResponsesAuthKind::ApiKey => "api-key",
            OpenaiResponsesAuthKind::BedrockMantle => "bedrock-mantle",
        }),
        #[cfg(feature = "gemini")]
        ProviderEntry::Gemini { auth_mode, .. } => Some(match auth_mode {
            GeminiAuthMode::ApiKey => "api-key",
            GeminiAuthMode::CloudCode => "cloud-code",
        }),
        #[cfg(feature = "bedrock")]
        ProviderEntry::Bedrock { creds, .. } => Some(match creds {
            BedrockCredsConfig::BearerKey { .. } => "bearer-key",
            BedrockCredsConfig::Static { .. } => "static",
            BedrockCredsConfig::Profile { .. } => "profile",
            BedrockCredsConfig::DefaultChain => "default-chain",
        }),
    }
}

/// The configured `base_url` for an entry, or `None` for a variant whose
/// endpoint is derived at factory time rather than carried in config. A
/// cloud-code Gemini entry with no pin reports its effective cloud-code host.
fn configured_base_url(entry: &ProviderEntry) -> Option<&str> {
    match entry {
        ProviderEntry::OpenaiCompat { base_url, .. }
        | ProviderEntry::AnthropicApi { base_url, .. } => Some(base_url),
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses { base_url, .. } => base_url.as_deref(),
        // A cloud-code entry that never set `base_url` carries the api-key
        // public default, which is not a host it ever talks to: the factory
        // leaves the constructor's cloud-code default in place. Report that
        // effective host rather than a value the lane cannot use. An explicit
        // pin (production, enterprise mirror, staging) is reported verbatim.
        #[cfg(feature = "gemini")]
        ProviderEntry::Gemini {
            base_url,
            auth_mode: GeminiAuthMode::CloudCode,
            ..
        } if *base_url == crate::config::default_gemini_base() => {
            Some(routectl_providers::gemini::DAILY_BASE_URL)
        }
        #[cfg(feature = "gemini")]
        ProviderEntry::Gemini { base_url, .. } => Some(base_url),
        // Bedrock carries no base_url at all: the factory derives the host
        // from `region`.
        #[cfg(feature = "bedrock")]
        ProviderEntry::Bedrock { .. } => None,
    }
}

/// Project one `[providers.X]` entry into its secret-safe cell.
fn provider_cell(id: &str, entry: &ProviderEntry) -> ProviderCell {
    ProviderCell {
        id: id.to_string(),
        kind: entry.kind_str().to_string(),
        endpoint_origin: configured_base_url(entry).and_then(endpoint_origin),
        credential_ref_scheme: credential_ref_scheme(entry.api_key_ref()),
        auth_token: auth_token(entry).map(str::to_string),
        rpm_limit: entry.runtime().rpm_limit,
    }
}

/// The provenance-annotated effective view: the layered surfaces only.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveView {
    /// One entry per `[models.X]` table, in nickname order.
    pub models: Vec<ModelCell>,
    /// One entry per operator-nameable failure class, in a stable order.
    pub classes: Vec<ClassPolicyCell>,
    /// One entry per config-derived capability-override cell, in a stable
    /// `(target_spec, capability_key)` order. Empty when the config carries
    /// no capability overrides. Learned-capability negatives are runtime
    /// state on a live router, not config, so they are not part of this
    /// pure view.
    pub capabilities: Vec<OverrideRow>,
    /// One entry per `[aliases]` table entry, in alias-key order.
    pub aliases: Vec<AliasChain>,
    /// One secret-safe cell per `[providers.X]` entry, in provider-key order.
    pub providers: Vec<ProviderCell>,
}

/// Every operator-nameable failure class, in a stable render order.
const ALL_CONFIG_CLASSES: [ConfigFailureClass; 10] = [
    ConfigFailureClass::RateLimited,
    ConfigFailureClass::Auth,
    ConfigFailureClass::BadRequest,
    ConfigFailureClass::ContentPolicy,
    ConfigFailureClass::ContextWindow,
    ConfigFailureClass::ServerError,
    ConfigFailureClass::Timeout,
    ConfigFailureClass::NetworkError,
    ConfigFailureClass::Overloaded,
    ConfigFailureClass::FeatureUnsupported,
];

/// Derive the provenance-annotated effective view from a parsed config and its
/// catalog overlay. Pure: the model rows come from the same lookups the router
/// stamps onto `ResolvedModel::effective_row` at chain-build; the class rows
/// come from [`crate::config::RetryPolicy::resolved_class`].
#[must_use]
pub fn derive_effective_view(config: &Config, overlay: &CatalogOverlay) -> EffectiveView {
    let models = config
        .models
        .iter()
        .map(|(nickname, entry)| {
            let provider_kind = config.provider_kind_for_target(&entry.provider);
            ModelCell {
                nickname: nickname.clone(),
                provider: entry.provider.clone(),
                provider_kind: provider_kind.to_string(),
                upstream: entry.upstream.clone(),
                row: resolve_effective_row(
                    provider_kind,
                    &entry.upstream,
                    None,
                    &config.cache_pricing,
                    overlay,
                ),
            }
        })
        .collect();

    let classes = ALL_CONFIG_CLASSES
        .iter()
        .map(|&class| {
            let canonical: FailureClass = class.to_failure_class();
            let (retry_cap, fallback) = config.retry.resolved_class(&canonical);
            let source = if config.retry.classes.contains_key(&class) {
                ClassPolicySource::Config
            } else {
                ClassPolicySource::BakedDefault
            };
            ClassPolicyCell {
                class,
                retry_cap,
                fallback,
                source,
            }
        })
        .collect();

    let mut capabilities = OverrideRegistry::build(config).snapshot();
    capabilities.sort_by(|a, b| {
        a.target_spec
            .cmp(&b.target_spec)
            .then_with(|| a.capability_key.cmp(&b.capability_key))
    });

    let aliases = config
        .aliases
        .iter()
        .map(|(alias, value)| AliasChain {
            alias: alias.clone(),
            chain: value.nicknames().map(str::to_string).collect(),
        })
        .collect();

    let providers = config
        .providers
        .iter()
        .map(|(id, entry)| provider_cell(id, entry))
        .collect();

    EffectiveView {
        models,
        classes,
        capabilities,
        aliases,
        providers,
    }
}

#[cfg(test)]
#[path = "config_effective_tests.rs"]
mod tests;
