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
    /// The `provider` this model references.
    pub provider: String,
    /// The referenced provider's kind token (empty when the provider is
    /// unknown -- the same fallback the chain-build merge uses).
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
            let provider_kind = config
                .providers
                .get(&entry.provider)
                .map_or("", ProviderEntry::kind_str);
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
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

    use crate::catalog::Source;
    use crate::catalog_overlay::CatalogOverlay;
    use crate::class_policy::ClassPolicy;
    use crate::config::Config;
    use crate::factory::apply_catalog_overlay;
    use crate::override_registry::{OverrideProvenance, OverrideVerdict};
    use crate::resolved::ResolvedModel;

    struct StubProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response(&self.id, "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    fn config_with_opus_model() -> Config {
        toml::from_str(
            r#"
version = 2

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-8"
"#,
        )
        .expect("valid config")
    }

    #[test]
    fn model_row_matches_apply_catalog_overlay_stamp() {
        // Arrange: same (provider_kind, upstream) fed to both paths.
        let config = config_with_opus_model();
        let overlay = CatalogOverlay::default();

        let provider: Arc<dyn Provider> = Arc::new(StubProvider { id: "stub".into() });
        let mut resolved = BTreeMap::new();
        resolved.insert(
            "opus".to_string(),
            Arc::new(ResolvedModel::new(
                "opus",
                "anthropic",
                provider,
                "claude-opus-4-8",
            )),
        );

        // Act
        let stamped = apply_catalog_overlay(resolved, &config, &overlay);
        let view = derive_effective_view(&config, &overlay);

        // Assert: the derivation reproduces the stamped merge exactly.
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        assert_eq!(derived.row, stamped["opus"].effective_row);
        assert!(matches!(
            derived.row,
            EffectiveRow::Present {
                source: Source::Baked,
                ..
            }
        ));
    }

    #[test]
    fn overlay_user_cell_tags_user_source() {
        // Arrange: a user overlay cell for the opus selector.
        let config = config_with_opus_model();
        let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "cells": {
                "anthropic-api:claude-opus-4-8": {
                    "source": "user",
                    "verified_at": "2026-07-01",
                    "rm": 0.05
                }
            }
        }))
        .expect("valid overlay");

        // Act
        let view = derive_effective_view(&config, &overlay);

        // Assert
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        match &derived.row {
            EffectiveRow::Present { source, row, .. } => {
                assert_eq!(*source, Source::User);
                assert_eq!(row.rm, 0.05);
            }
            other => panic!("expected Present/User, got {other:?}"),
        }
    }

    #[test]
    fn disable_overlay_cell_yields_disabled_row() {
        let config = config_with_opus_model();
        let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "cells": { "anthropic-api:claude-opus-4-8": null }
        }))
        .expect("valid overlay");

        let view = derive_effective_view(&config, &overlay);
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        assert_eq!(derived.row, EffectiveRow::Disabled);
    }

    #[test]
    fn unpriced_selector_yields_missing_row() {
        // A model whose provider is unknown resolves to an empty
        // provider_kind, which matches no baked cell -- the same fallback the
        // chain-build merge uses -- so neither layer has a row.
        let mut config = config_with_opus_model();
        config.models.get_mut("opus").unwrap().provider = "ghost".to_string();

        let view = derive_effective_view(&config, &CatalogOverlay::default());
        let derived = view.models.iter().find(|m| m.nickname == "opus").unwrap();
        assert_eq!(derived.provider_kind, "");
        assert_eq!(derived.row, EffectiveRow::Missing);
    }

    #[test]
    fn class_with_operator_leaf_tags_config_others_baked_default() {
        // Arrange: an operator leaf on rate-limited only.
        let mut config = Config::default();
        config.retry.classes.insert(
            ConfigFailureClass::RateLimited,
            ClassPolicy {
                retry: Some(7),
                fallback: None,
            },
        );

        // Act
        let view = derive_effective_view(&config, &CatalogOverlay::default());

        // Assert: rate-limited tagged config with the overridden cap; a class
        // with no leaf tags baked-default.
        let rate_limited = view
            .classes
            .iter()
            .find(|c| c.class == ConfigFailureClass::RateLimited)
            .unwrap();
        assert_eq!(rate_limited.source, ClassPolicySource::Config);
        assert_eq!(rate_limited.retry_cap, 7);

        let auth = view
            .classes
            .iter()
            .find(|c| c.class == ConfigFailureClass::Auth)
            .unwrap();
        assert_eq!(auth.source, ClassPolicySource::BakedDefault);

        // Every class is represented exactly once.
        assert_eq!(view.classes.len(), ALL_CONFIG_CLASSES.len());
    }

    #[test]
    fn seeded_override_appears_as_capability_cell_with_override_source() {
        // Arrange: an operator capability override force-marks web_search
        // unsupported for the opus model's provider.
        let mut config = config_with_opus_model();
        config.capability.overrides.insert(
            "anthropic".to_string(),
            crate::config::OverrideEntry {
                unsupported: vec!["web_search".to_string()],
                force_supported: Vec::new(),
            },
        );

        // Act
        let view = derive_effective_view(&config, &CatalogOverlay::default());

        // Assert: the capability layer carries the cell, tagged with the
        // Override provenance (the "override" token of the routing filter's
        // source contract) and the route-away verdict.
        let cell = view
            .capabilities
            .iter()
            .find(|c| c.target_spec == "anthropic" && c.capability_key == "web_search")
            .expect("seeded override must surface as a capability cell");
        assert_eq!(cell.verdict, OverrideVerdict::RouteAway);
        assert_eq!(cell.provenance, OverrideProvenance::Override);
    }

    /// The exact fixture `factory::validate_tests` uses to pin that a REJECTED
    /// base_url's embedded credential never reaches an error message. Here it
    /// pins the ACCEPTED case: a config carrying userinfo passes validation, so
    /// the projection is the only thing standing between it and a read surface.
    const LEAKY_BASE_URL: &str =
        "https://user:sk-live-LEAKED@internal.example/v1?key=sk-live-LEAKED#sk-live-LEAKED";

    fn config_with_provider_base_url(base_url: &str) -> Config {
        toml::from_str(&format!(
            r#"
version = 2

[providers.acme]
kind = "anthropic-api"
api_key_ref = "env://ACME_KEY"
base_url = "{base_url}"
"#
        ))
        .expect("valid config")
    }

    #[test]
    fn endpoint_origin_strips_userinfo_path_query_and_fragment() {
        // Arrange: an accepted config whose base_url embeds a credential in
        // userinfo, in the query, and in the fragment.
        let config = config_with_provider_base_url(LEAKY_BASE_URL);

        // Act
        let view = derive_effective_view(&config, &CatalogOverlay::default());

        // Assert: only the origin survives.
        let cell = &view.providers[0];
        assert_eq!(
            cell.endpoint_origin.as_deref(),
            Some("https://internal.example")
        );
    }

    #[test]
    fn credential_never_appears_anywhere_in_the_projection() {
        // Arrange
        let config = config_with_provider_base_url(LEAKY_BASE_URL);

        // Act: scan the WHOLE projection, not just the endpoint field -- a
        // future field that copies config text must fail this too.
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        let rendered = format!("{:?}", view.providers);

        // Assert
        assert!(
            !rendered.contains("sk-live-LEAKED"),
            "credential must not surface; got: {rendered}"
        );
        assert!(
            !rendered.contains("user:"),
            "userinfo must not surface; got: {rendered}"
        );
    }

    #[test]
    fn endpoint_origin_keeps_a_nondefault_port() {
        let config = config_with_provider_base_url("http://127.0.0.1:8080/v1");
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        assert_eq!(
            view.providers[0].endpoint_origin.as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }

    #[test]
    fn unparseable_endpoint_yields_none_not_a_raw_fallback() {
        // A non-URL base_url must project to None. A raw-string fallback here
        // would republish whatever the operator wrote, which is the entire
        // hazard this projection exists to close.
        let config = config_with_provider_base_url("sk-live-LEAKED-not-a-url");
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        assert_eq!(view.providers[0].endpoint_origin, None);
        assert!(!format!("{:?}", view.providers).contains("sk-live-LEAKED"));
    }

    #[test]
    fn credential_ref_scheme_carries_the_scheme_only() {
        let config = config_with_provider_base_url("https://upstream.example");
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        let cell = &view.providers[0];
        assert_eq!(cell.credential_ref_scheme.as_deref(), Some("env"));
        assert!(
            !format!("{cell:?}").contains("ACME_KEY"),
            "the ref body must not surface"
        );
    }

    /// An unrecognized ref is a bare value someone pasted into the field, not a
    /// ref with an exotic scheme -- and a bare value IS secret material, so the
    /// text before its first colon is a key id, not a scheme.
    #[test]
    fn unrecognized_credential_ref_publishes_none_of_its_bytes() {
        for pasted in [
            "AKIAEXAMPLE:wJalrXUtnFEMI",
            "sk-live-abc:def",
            "sk-live-no-colon-at-all",
            ":sk-live-leading",
        ] {
            assert_eq!(
                credential_ref_scheme(Some(pasted)),
                None,
                "`{pasted}` is not an allowlisted ref scheme; it must project nothing"
            );
        }
        // The allowlisted four still project, case-insensitively.
        for (raw, expected) in [
            ("env://VAR", "env"),
            ("FILE:///etc/k", "file"),
            ("oauth://anthropic", "oauth"),
            ("literal:k", "literal"),
        ] {
            assert_eq!(credential_ref_scheme(Some(raw)).as_deref(), Some(expected));
        }
    }

    /// Only `http`/`https` project. Every other scheme carries an OPAQUE host,
    /// which the url crate does NOT percent-decode, so credential bytes would
    /// ride straight through `host_str()`. This cannot lean on
    /// `validate_base_url_scheme`: that runs only from the per-model factory
    /// loop, while this projection walks every provider entry, so a provider no
    /// model references arrives here unvalidated.
    #[test]
    fn non_http_scheme_projects_no_origin() {
        for raw in [
            "weird://h.example%40sk-live-LEAKED/v1",
            "gopher://user:sk-live-P@h.example/x",
            "ftp://h.example/v1",
            // These two have no `//` authority, so they would yield None even
            // without the gate -- pinned so a future reader does not mistake
            // that for scheme checking.
            "mailto:a@b.example",
            "unix:/var/run/x.sock",
        ] {
            assert_eq!(
                endpoint_origin(raw),
                None,
                "`{raw}` is not http(s); it must project nothing"
            );
            assert!(!format!("{:?}", endpoint_origin(raw)).contains("sk-live"));
        }
    }

    /// For a special scheme the authority ends at the first `/`, so a `/` inside
    /// a credential truncates it and the parser reads the pre-`@` text as the
    /// HOST. When the credential sits in the username position that puts it
    /// straight into the projected origin.
    #[test]
    fn at_sign_the_parser_did_not_consume_projects_no_origin() {
        // Parses as host "sk-live-KEY", port 8080 -- the real host is demoted
        // into the path, so the origin would have BEEN the credential.
        assert_eq!(
            endpoint_origin("https://sk-live-KEY:8080/x@h.example/v1"),
            None
        );
        assert_eq!(
            endpoint_origin("https://user:8080/sk-live-REST@h.example/v1"),
            None
        );
        // A genuine userinfo authority still projects its real origin: the
        // parser consumed the `@`, so host_str() is the operator's host.
        assert_eq!(
            endpoint_origin("https://user:sk-live-X@h.example/v1").as_deref(),
            Some("https://h.example")
        );
        // No `@` at all is the ordinary case and is unaffected.
        assert_eq!(
            endpoint_origin("https://h.example:8443/v1").as_deref(),
            Some("https://h.example:8443")
        );
    }

    /// TWO `@` is a distinct vector from one, and defeats any guard that asks
    /// "did the parse consume userinfo?": the parser takes userinfo from the
    /// LAST `@` before the first `/`, so a benign leading `x@` vouches for a
    /// secret that then lands in the host slot. Both of these projected
    /// credential-derived origins before the authority-scoped predicate.
    #[test]
    fn a_second_at_sign_cannot_vouch_for_a_demoted_credential() {
        assert_eq!(
            endpoint_origin("https://x@sk-live-AB/CD@h.example/v1"),
            None
        );
        assert_eq!(
            endpoint_origin("https://x@sk-live-KEY:8080/y@h.example/v1"),
            None
        );
        // Neither may leak even a lowercased fragment of the credential.
        for raw in [
            "https://x@sk-live-AB/CD@h.example/v1",
            "https://x@sk-live-KEY:8080/y@h.example/v1",
        ] {
            assert!(!format!("{:?}", endpoint_origin(raw)).contains("sk-live"));
        }
    }

    /// The withholding is deliberately fail-SAFE rather than precise: an `@`
    /// that is legitimately in a path or query also suppresses the origin,
    /// which the panel renders as "derived at startup". Pinned so the
    /// imprecision is a recorded decision rather than a surprise -- tightening
    /// it must not reopen `a_second_at_sign_cannot_vouch_for_a_demoted_credential`.
    #[test]
    fn an_at_sign_past_the_authority_withholds_fail_safe() {
        assert_eq!(endpoint_origin("https://gw.example/v1?tenant=a@b"), None);
        assert_eq!(endpoint_origin("https://h.example/v1@beta"), None);
    }

    #[test]
    fn anthropic_entry_carries_its_own_auth_token_and_kind() {
        let config = config_with_provider_base_url("https://upstream.example");
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        let cell = &view.providers[0];
        assert_eq!(cell.id, "acme");
        assert_eq!(cell.kind, "anthropic-api");
        assert_eq!(cell.auth_token.as_deref(), Some("api-key"));
    }

    #[test]
    fn openai_compat_has_no_auth_token() {
        // The variant carries no auth discriminant, so the projection reports
        // absence rather than inventing a spanning token.
        let config: Config = toml::from_str(
            r#"
version = 2

[providers.compat]
kind = "openai-compat"
api_key_ref = "env://COMPAT_KEY"
base_url = "https://compat.example/v1"
"#,
        )
        .expect("valid config");

        let view = derive_effective_view(&config, &CatalogOverlay::default());
        assert_eq!(view.providers[0].auth_token, None);
    }

    /// Every auth arm the build carries, asserted against the token its own
    /// config vocabulary already uses. The `auth_token` match is exhaustive
    /// with no wildcard, so a new variant breaks the BUILD -- but a typo'd or
    /// swapped token in an existing arm compiles fine and would publish a
    /// wrong mechanism name on the panel. Only these tests catch that, and
    /// each is cfg-gated to the feature that compiles its arm.
    fn only_provider(toml_text: &str) -> ProviderCell {
        let config: Config = toml::from_str(toml_text).expect("valid config");
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        view.providers
            .into_iter()
            .next()
            .expect("one provider projected")
    }

    #[test]
    fn anthropic_oauth_bearer_carries_its_own_token() {
        let cell = only_provider(
            r#"
version = 2

[providers.oauth]
kind = "anthropic-api"
api_key_ref = "oauth://anthropic"
auth_kind = "oauth-bearer"
"#,
        );
        assert_eq!(cell.kind, "anthropic-api");
        assert_eq!(cell.auth_token.as_deref(), Some("oauth-bearer"));
        assert_eq!(cell.credential_ref_scheme.as_deref(), Some("oauth"));
    }

    #[cfg(feature = "openai-responses")]
    #[test]
    fn openai_responses_carries_each_of_its_own_auth_tokens() {
        for (auth_kind, expected) in [
            ("api-key", "api-key"),
            ("chatgpt-oauth", "chatgpt-oauth"),
            ("bedrock-mantle", "bedrock-mantle"),
        ] {
            let cell = only_provider(&format!(
                r#"
version = 2

[providers.r]
kind = "openai-responses"
api_key_ref = "env://K"
auth_kind = "{auth_kind}"
"#
            ));
            assert_eq!(cell.kind, "openai-responses");
            assert_eq!(
                cell.auth_token.as_deref(),
                Some(expected),
                "auth_kind {auth_kind} must project its own token"
            );
        }
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_carries_each_of_its_own_auth_modes() {
        for (auth_mode, expected) in [("api-key", "api-key"), ("cloud-code", "cloud-code")] {
            let cell = only_provider(&format!(
                r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
auth_mode = "{auth_mode}"
"#
            ));
            assert_eq!(cell.kind, "gemini");
            assert_eq!(
                cell.auth_token.as_deref(),
                Some(expected),
                "auth_mode {auth_mode} must project its own token"
            );
        }
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_cloud_code_reports_its_effective_host_not_the_api_key_default() {
        // An unset `base_url` on a cloud-code entry keeps the api-key public
        // default in config, but the lane never talks to that host: the
        // factory leaves the constructor's cloud-code default in place.
        let unset = only_provider(
            r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
"#,
        );
        assert_eq!(
            unset.endpoint_origin.as_deref(),
            endpoint_origin(routectl_providers::gemini::DAILY_BASE_URL).as_deref(),
        );

        // An explicit pin is reported verbatim -- never rewritten.
        let pinned = only_provider(&format!(
            r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
base_url = "{}"
"#,
            routectl_providers::gemini::PROD_BASE_URL
        ));
        assert_eq!(
            pinned.endpoint_origin.as_deref(),
            endpoint_origin(routectl_providers::gemini::PROD_BASE_URL).as_deref(),
        );

        // An api-key entry keeps reporting the public REST default.
        let api_key = only_provider(
            r#"
version = 2

[providers.g]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
"#,
        );
        assert_eq!(
            api_key.endpoint_origin.as_deref(),
            endpoint_origin(&crate::config::default_gemini_base()).as_deref(),
        );
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn bedrock_carries_its_creds_kind_and_no_configured_endpoint() {
        let cell = only_provider(
            r#"
version = 2

[providers.b]
kind = "bedrock"
region = "us-east-1"

[providers.b.creds]
kind = "default-chain"
"#,
        );
        assert_eq!(cell.kind, "bedrock");
        assert_eq!(cell.auth_token.as_deref(), Some("default-chain"));
        // Bedrock carries no `base_url` at all -- the factory derives the host
        // from `region`. A pure derivation reports absence rather than
        // duplicating that factory logic.
        assert_eq!(cell.endpoint_origin, None);
    }

    #[test]
    fn zero_rpm_limit_stays_distinct_from_unlimited() {
        // `Some(0)` seeds a zero-capacity bucket -- immediately rate-limited.
        // Collapsing it into `None` (unlimited) would invert the meaning.
        let limited: Config = toml::from_str(
            r#"
version = 2

[providers.zero]
kind = "anthropic-api"
api_key_ref = "env://K"
rpm_limit = 0

[providers.unlimited]
kind = "anthropic-api"
api_key_ref = "env://K"

[providers.capped]
kind = "anthropic-api"
api_key_ref = "env://K"
rpm_limit = 60
"#,
        )
        .expect("valid config");

        let view = derive_effective_view(&limited, &CatalogOverlay::default());
        let rpm = |id: &str| {
            view.providers
                .iter()
                .find(|p| p.id == id)
                .expect("provider projected")
                .rpm_limit
        };
        assert_eq!(rpm("zero"), Some(0));
        assert_eq!(rpm("unlimited"), None);
        assert_eq!(rpm("capped"), Some(60));
    }

    #[test]
    fn providers_are_projected_in_provider_key_order() {
        let config: Config = toml::from_str(
            r#"
version = 2

[providers.zeta]
kind = "anthropic-api"
api_key_ref = "env://K"

[providers.alpha]
kind = "anthropic-api"
api_key_ref = "env://K"
"#,
        )
        .expect("valid config");

        let view = derive_effective_view(&config, &CatalogOverlay::default());
        let ids: Vec<&str> = view.providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "zeta"]);
    }

    #[test]
    fn capability_layer_empty_when_no_overrides() {
        // A config with no capability overrides yields an empty capability
        // layer -- rendered empty, never an error.
        let config = config_with_opus_model();
        let view = derive_effective_view(&config, &CatalogOverlay::default());
        assert!(view.capabilities.is_empty());
    }

    #[test]
    fn derivation_source_is_pure_no_router_build() {
        // Structural guard: the production portion of this module never calls
        // into the secret-resolving / provider-constructing build path. The
        // pure `(&Config, &CatalogOverlay)` signature is the real invariant;
        // this scan tripwires an accidental reintroduction.
        let src = include_str!("config_effective.rs");
        // Cut at the inline test module, NOT at the first `#[cfg(test)]`: that
        // attribute also decorates test-only items which can sit above the real
        // module, which would silently shrink the scanned region while this
        // guard stayed green. The uniqueness assert is what closes that -- a
        // second test module has to be dealt with here, deliberately.
        //
        // Twin of `production_source` in routectl-cli's `handlers::status`,
        // duplicated rather than shared: crossing the crate boundary for four
        // lines would mean a public-API change plus a baseline regeneration
        // plus a cross-crate test dependency.
        //
        // The needle is assembled from fragments so THIS code's own source
        // lines are not counted as test-module openers.
        let needle = concat!("mod ", "tests {");
        let occurrences = src.matches(needle).count();
        assert_eq!(
            occurrences, 1,
            "the production cut is ambiguous with {occurrences} test-module openers; \
             decide explicitly what this guard must cover"
        );
        let production = &src[..src.find(needle).expect("an inline test module")];

        assert!(!production.contains("build_resolved_models"));
        assert!(!production.contains("build_provider"));
    }
}
