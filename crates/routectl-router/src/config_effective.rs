//! Pure, provenance-annotated derivation of the effective config view that
//! backs `config show --effective`.
//!
//! Several surfaces have more than one layer competing for a value, so only
//! those carry provenance: a model's facts (context window, output fill and
//! per-field prices, each tagged with its source), a retry class's
//! policy (the baked class default vs a `[retry.classes.<class>]` leaf), and
//! a capability cell (the config-derived override layer, tagged with the
//! source that set it). Every other config field is trivially the value in
//! `config.toml`; annotating it would be noise.
//!
//! The derivation is a PURE function over the config and the price tables it
//! is handed. It runs the same price precedence usage cost resolves through,
//! so an operator inspecting the view sees exactly the cells cost reads --
//! with no secret resolution, no provider construction, no file read, and no
//! network.
//!
//! The capability layer carries only the config-derived override cells (the
//! same read-model the routing filter consults). Learned-capability negatives
//! are in-memory runtime state on a live router, not config, so they are out
//! of scope for this pure view.

use std::borrow::Cow;

use routectl_auth::SecretScheme;
use routectl_core::failure_class::FailureClass;
use routectl_providers::anthropic_api::AuthKind;
#[cfg(feature = "gemini")]
use routectl_providers::gemini::GeminiAuthMode;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;

use crate::class_policy::ConfigFailureClass;
#[cfg(feature = "bedrock")]
use crate::config::BedrockCredsConfig;
use crate::config::{
    Config, ModelEntry, PricingConfig, ProviderEntry, ResolvedTuning, resolve_tuning,
};
use crate::override_registry::{OverrideRegistry, OverrideRow};
use crate::prices::{
    PriceSource, PriceSources, PriceSubject, PriceTable, TableDate, effective_pricing,
};
use crate::router::LearnedFills;
use crate::seat_pool::seat_state_key;

/// The effective facts a single `[models.X]` entry resolves to, tagged with
/// the `(provider_kind, upstream)` it targets.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCell {
    /// The `[models.<nickname>]` key.
    pub nickname: String,
    /// The `provider` this model references. May name a `[providers]` entry
    /// or a `[pools]` block -- both live in one namespace.
    pub provider: String,
    /// The kind token of the target: the referenced provider entry's own
    /// kind, or for a pool-backed model a member's kind (see
    /// [`Config::provider_kind_for_target`]). Empty when the reference
    /// resolves to neither.
    pub provider_kind: String,
    /// The upstream model id forwarded to the provider.
    pub upstream: String,
    /// `[models.X] context_window`, the one window source; `None` when the
    /// operator configured none (the window gate keeps the model and
    /// `/v1/models` omits it).
    pub context_window: Option<u32>,
    /// The `max_tokens` the model's egress sends when a client omits one.
    /// `None` when the egress forwards an omitted limit as omitted, whatever
    /// `max_output_tokens` says (that value then only sizes the window-gate
    /// reserve). A pure derivation has no learned layer, so this is the
    /// configured or default fill until [`EffectiveView::with_learned_fills`]
    /// lowers it.
    pub fill: Option<FillCeiling>,
    /// The effective per-million-token prices; a field left `None` is unknown.
    pub pricing: PricingConfig,
    /// The source of each field of [`Self::pricing`].
    pub price_sources: PriceSources,
}

impl ModelCell {
    /// Each price field as `(config key, value, source)`, in the
    /// `[models.X.pricing]` key order.
    pub const fn price_fields(&self) -> [(&'static str, Option<f64>, PriceSource); 5] {
        let p = &self.pricing;
        let s = &self.price_sources;
        [
            ("input_per_mtok", p.input_per_mtok, s.input),
            ("output_per_mtok", p.output_per_mtok, s.output),
            ("cache_read_per_mtok", p.cache_read_per_mtok, s.cache_read),
            (
                "cache_write_5m_per_mtok",
                p.cache_write_5m_per_mtok,
                s.cache_write_5m,
            ),
            (
                "cache_write_1h_per_mtok",
                p.cache_write_1h_per_mtok,
                s.cache_write_1h,
            ),
        ]
    }
}

/// A model's output fill and the layer that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillCeiling {
    /// The fill value in tokens.
    pub tokens: u32,
    /// Which layer supplied [`Self::tokens`].
    pub source: FillSource,
}

impl FillCeiling {
    /// This fill lowered to `learned` when that ceiling is below it, the rule
    /// the dispatch clamp applies; unchanged otherwise.
    #[must_use]
    pub const fn lowered_by(self, learned: Option<u32>) -> Self {
        match learned {
            Some(tokens) if tokens < self.tokens => Self {
                tokens,
                source: FillSource::Learned,
            },
            _ => self,
        }
    }
}

/// Where a [`FillCeiling`] came from. Each variant renders as one fixed token
/// through [`Self::as_str`]; this is the one vocabulary every surface that
/// names a fill source uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillSource {
    /// `[models.X] max_output_tokens`.
    ModelConfig,
    /// A ceiling learned from an upstream refusal, below what would
    /// otherwise be sent.
    Learned,
    /// The built-in fill of an egress that must send `max_tokens`.
    Default,
}

impl FillSource {
    /// The fixed token for this source.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelConfig => "model-config",
            Self::Learned => "learned",
            Self::Default => "default",
        }
    }
}

/// The price tables a view was derived against, by date. A `refreshed` or
/// `shipped` price source is read beside the matching date.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PriceTableDates {
    /// The refreshed table's date, or `None` when no refreshed table was given.
    pub refreshed: Option<TableDate>,
    /// The shipped table's date, or `None` when no shipped table was given.
    pub shipped: Option<TableDate>,
}

impl PriceTableDates {
    /// The date of the table a price `source` names: the refreshed or the
    /// shipped table's date, `None` for every other source.
    pub const fn date_for(&self, source: PriceSource) -> Option<TableDate> {
        match source {
            PriceSource::Refreshed => self.refreshed,
            PriceSource::Shipped => self.shipped,
            PriceSource::ModelConfig
            | PriceSource::Registry
            | PriceSource::Derived
            | PriceSource::Unknown => None,
        }
    }
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
    /// `oauth`, `literal`), never the ref body. `unknown` for a present ref
    /// off that allowlist (a pasted bare value included); `None` for a
    /// variant with no single canonical key slot, or a blank ref.
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
    /// The entry's OWN configured requests-per-minute cap. Three states, none
    /// interchangeable: `Some(n)` with `n > 0` is a live limit, `Some(0)` seeds
    /// a zero-capacity bucket and therefore means IMMEDIATELY rate-limited, and
    /// `None` means the entry sets none. A pool's `rpm_limit` inherited by a
    /// member does not appear here; the resolved cap, pool tier included, is
    /// the member's [`EffectiveView::tuning`] row.
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
/// Three crate-internal consumers depend on this being the ONLY base_url
/// projection: the effective-config view here,
/// [`crate::config::ProviderEntry::redact_secrets`] via `redact_base_url`, and
/// [`normalized_origin`] (the dispatch walk's endpoint key and the price
/// table's provider selection). Its
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

/// `scheme://host:port` for `base_url`: the crate's one origin normalization
/// for comparing endpoints. Host lowercased with any trailing DNS root dot
/// folded, port explicit or the scheme default, so `https://API.example.` and
/// `https://api.example:443/v1` normalize alike.
///
/// Built on [`endpoint_origin`], whose guard withholds any URL whose written
/// authority could put credential bytes in the host slot, so the result never
/// carries userinfo, a path or a query, and inherits its fail-safe `None`.
pub fn normalized_origin(base_url: &str) -> Option<String> {
    let origin = endpoint_origin(base_url)?;
    let url = url::Url::parse(&origin).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = url.port_or_known_default()?;
    Some(format!("{}://{host}:{port}", url.scheme()))
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
/// its first colon would publish the key id. A present ref off the list
/// (colon or not) collapses to the fixed `unknown`, echoing none of its bytes;
/// only an absent or blank ref is `None`.
///
/// The token match is deliberately looser than [`SecretScheme::classify`]
/// (case-insensitive, `env:VAR` counts as `env`): this surface reports which
/// credential class the operator MEANT, not whether the ref parses.
fn credential_ref_scheme(api_key_ref: Option<&str>) -> Option<String> {
    let raw = api_key_ref?.trim();
    if raw.is_empty() {
        return None;
    }
    let token = raw.split_once(':').map_or("", |(scheme, _)| scheme);
    let label = SecretScheme::ALL
        .iter()
        .map(|scheme| scheme.as_token())
        .find(|known| token.eq_ignore_ascii_case(known))
        .unwrap_or(UNKNOWN_CREDENTIAL_REF_SCHEME);
    Some(label.to_string())
}

/// What a present `api_key_ref` off the scheme allowlist renders as.
const UNKNOWN_CREDENTIAL_REF_SCHEME: &str = "unknown";

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
fn configured_base_url(entry: &ProviderEntry) -> Option<Cow<'_, str>> {
    match entry {
        ProviderEntry::OpenaiCompat { base_url, .. }
        | ProviderEntry::AnthropicApi { base_url, .. } => Some(Cow::Borrowed(base_url)),
        #[cfg(feature = "openai-responses")]
        ProviderEntry::OpenaiResponses { base_url, .. } => base_url.as_deref().map(Cow::Borrowed),
        #[cfg(feature = "gemini")]
        ProviderEntry::Gemini { .. } => entry.classifiable_egress_base_url(),
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
        endpoint_origin: configured_base_url(entry).and_then(|base| endpoint_origin(&base)),
        credential_ref_scheme: credential_ref_scheme(entry.api_key_ref()),
        auth_token: auth_token(entry).map(str::to_string),
        rpm_limit: entry.runtime().rpm_limit,
    }
}

/// What a [`TuningRow`]'s `target` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuningTargetKind {
    /// A `[providers.X]` entry, pool members included.
    Provider,
    /// A `[models.X]` entry that sets its own
    /// `stream_first_byte_timeout_ms`, or one seat of such a pooled model.
    Model,
}

/// The tuning in force for one target, every cell carrying its value and
/// the tier that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuningRow {
    /// The `[providers.X]` key, a `[models.X]` nickname, or for a pooled
    /// model one seat's runtime state key (`nickname#member`).
    pub target: String,
    /// Which table `target` names.
    pub kind: TuningTargetKind,
    /// The resolved cells, from [`resolve_tuning`].
    pub tuning: ResolvedTuning,
}

/// One row per `[providers.X]` entry, in key order.
fn provider_tuning_rows(config: &Config) -> Vec<TuningRow> {
    config
        .providers
        .keys()
        .map(|id| TuningRow {
            target: id.clone(),
            kind: TuningTargetKind::Provider,
            tuning: resolve_tuning(config, Some(id), None),
        })
        .collect()
}

/// The rows for every `[models.X]` entry that sets a non-zero
/// first-content override, in nickname order. A model on a provider gets one
/// row under its nickname, resolved through that provider. A pooled model
/// gets one row per seat, in member order, each keyed by the seat's runtime
/// state key and resolved through its member entry, because dispatch stamps
/// each seat from its member: a member's own breaker or RPM key beats the
/// pool's.
fn model_tuning_rows(config: &Config) -> Vec<TuningRow> {
    let mut rows = Vec::new();
    for (nickname, entry) in &config.models {
        let Some(override_ms) = entry.stream_first_byte_timeout_ms.filter(|&ms| ms > 0) else {
            continue;
        };
        let row = |target: String, resolve_through: &str| TuningRow {
            target,
            kind: TuningTargetKind::Model,
            tuning: resolve_tuning(config, Some(resolve_through), Some(override_ms)),
        };
        match config.pools.get(&entry.provider) {
            Some(pool) => rows.extend(
                pool.members
                    .iter()
                    .map(|member| row(seat_state_key(nickname, Some(member)), member)),
            ),
            None => rows.push(row(nickname.clone(), &entry.provider)),
        }
    }
    rows
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
    /// The resolved tuning table: one row per `[providers.X]` entry, then the
    /// rows of each `[models.X]` first-content override, one per seat for a
    /// pooled model (see [`TuningRow`]).
    pub tuning: Vec<TuningRow>,
    /// The dates of the price tables the model prices were resolved against.
    pub price_tables: PriceTableDates,
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

/// Derive the provenance-annotated effective view from a parsed config and
/// the refreshed and shipped price tables (either may be absent). Pure: the
/// prices come from [`effective_pricing`]; the class rows from
/// [`crate::config::RetryPolicy::resolved_class`].
#[must_use]
pub fn derive_effective_view(
    config: &Config,
    refreshed: Option<&PriceTable>,
    shipped: Option<&PriceTable>,
) -> EffectiveView {
    let models = config
        .models
        .iter()
        .map(|(nickname, entry)| {
            let provider_kind = config.provider_kind_for_target(&entry.provider);
            // A pool prices through the member entry usage rows bill under,
            // so a `[registry]` row scoped to that entry applies here too.
            let billed_provider = config
                .provider_entry_for_target(&entry.provider)
                .map_or(entry.provider.as_str(), |(key, _)| key);
            let subject = PriceSubject {
                model: Some(nickname),
                provider: billed_provider,
                upstream: &entry.upstream,
                persisted_kind: None,
            };
            let (pricing, price_sources) = effective_pricing(config, &subject, refreshed, shipped)
                .unwrap_or((PricingConfig::default(), PriceSources::UNKNOWN));
            ModelCell {
                nickname: nickname.clone(),
                provider: entry.provider.clone(),
                provider_kind: provider_kind.to_string(),
                upstream: entry.upstream.clone(),
                context_window: entry.context_window.filter(|tokens| *tokens > 0),
                fill: fill_ceiling(config, entry),
                pricing,
                price_sources,
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
    let tuning = provider_tuning_rows(config)
        .into_iter()
        .chain(model_tuning_rows(config))
        .collect();

    EffectiveView {
        models,
        classes,
        capabilities,
        aliases,
        providers,
        tuning,
        price_tables: PriceTableDates {
            refreshed: refreshed.map(PriceTable::generated_at),
            shipped: shipped.map(PriceTable::generated_at),
        },
    }
}

/// The `max_tokens` a model's egress sends when the client omits one: none
/// when the egress forwards an omitted limit as omitted, else the configured
/// `max_output_tokens`, else the built-in default. A pool is read through
/// its first resolvable member alone: members agree on kind, not on egress,
/// so a `bedrock` pool mixing Invoke and Converse members reports the first
/// member's egress for the whole pool. A learned ceiling is not applied
/// here; see [`FillCeiling::lowered_by`].
#[must_use]
pub fn fill_ceiling(config: &Config, entry: &ModelEntry) -> Option<FillCeiling> {
    let (_, provider) = config.provider_entry_for_target(&entry.provider)?;
    if !routectl_providers::anthropic_api::egress_fills_max_tokens(provider.egress_kind()) {
        return None;
    }
    Some(entry.max_output_tokens.filter(|tokens| *tokens > 0).map_or(
        FillCeiling {
            tokens: routectl_providers::anthropic_api::DEFAULT_MAX_OUTPUT_TOKENS,
            source: FillSource::Default,
        },
        |tokens| FillCeiling {
            tokens,
            source: FillSource::ModelConfig,
        },
    ))
}

impl EffectiveView {
    /// This view with each model's fill lowered by the learned ceiling
    /// `learned` holds for it, for a surface that reads a learned layer.
    #[must_use]
    pub fn with_learned_fills(mut self, learned: &LearnedFills) -> Self {
        for cell in &mut self.models {
            cell.fill = cell
                .fill
                .map(|fill| fill.lowered_by(learned.for_model(&cell.nickname)));
        }
        self
    }
}

#[cfg(test)]
#[path = "config_effective_tests.rs"]
mod tests;
