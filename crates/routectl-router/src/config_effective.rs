//! Pure, provenance-annotated derivation of the effective config view that
//! backs `config show --effective`.
//!
//! Several surfaces have more than one layer competing for a value, so only
//! those carry provenance: a model's facts (context window, output fill and
//! per-field prices, each tagged with its source), a retry class's
//! policy (the baked class default vs a `[retry.classes]` count), and
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

use routectl_auth::{CredentialRef, SecretScheme};
use routectl_core::failure_class::FailureClass;

use crate::class_policy::ConfigFailureClass;
use crate::config::{
    Config, ModelEntry, PricingConfig, ProviderEntry, ResolvedTuning, SeatSelection, resolve_tuning,
};
use crate::override_registry::{OverrideRegistry, OverrideRow};
use crate::prices::{
    PriceSource, PriceSources, PriceSubject, PriceTable, TableDate, effective_pricing,
};
use crate::router::{AccountIdentities, LearnedFills};
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
    /// Each price field as `(dimension, value, source)`: input, output and
    /// cache read, then one row per cache-write lifetime ordered by duration.
    /// The 5m and 1h rows are always present, priced or not; any other
    /// lifetime appears only when priced.
    pub fn price_fields(&self) -> Vec<(PriceDimension, Option<f64>, PriceSource)> {
        let p = &self.pricing;
        let s = &self.price_sources;
        let five_minutes = PricingConfig::CACHE_WRITE_5M_SECONDS;
        let one_hour = PricingConfig::CACHE_WRITE_1H_SECONDS;
        let mut writes: Vec<(u32, Option<f64>)> = vec![
            (five_minutes, p.cache_write_5m_per_mtok),
            (one_hour, p.cache_write_1h_per_mtok),
        ];
        writes.extend(
            p.cache_write_rates()
                .into_iter()
                .filter(|(seconds, _)| *seconds != five_minutes && *seconds != one_hour)
                .map(|(seconds, rate)| (seconds, Some(rate))),
        );
        writes.sort_by_key(|(seconds, _)| *seconds);
        let fixed = [
            (PriceDimension::Input, p.input_per_mtok, s.input),
            (PriceDimension::Output, p.output_per_mtok, s.output),
            (
                PriceDimension::CacheRead,
                p.cache_read_per_mtok,
                s.cache_read,
            ),
        ];
        fixed
            .into_iter()
            .chain(writes.into_iter().map(|(seconds, rate)| {
                (
                    PriceDimension::CacheWrite(seconds),
                    rate,
                    s.cache_write_at(seconds),
                )
            }))
            .collect()
    }
}

/// One priced dimension of a model: a token class, or a cache write of one
/// lifetime in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceDimension {
    /// Input tokens.
    Input,
    /// Output tokens.
    Output,
    /// Tokens read from the prompt cache.
    CacheRead,
    /// Tokens written to the prompt cache for this many seconds.
    CacheWrite(u32),
}

impl PriceDimension {
    /// The `[models.X.pricing]` key that prices this dimension:
    /// `cache_write_5m_per_mtok` and `cache_write_1h_per_mtok` for the 300 and
    /// 3600 second lifetimes, `cache_write_per_mtok.<duration>` for any other.
    pub fn config_key(self) -> Cow<'static, str> {
        match self {
            Self::Input => Cow::Borrowed("input_per_mtok"),
            Self::Output => Cow::Borrowed("output_per_mtok"),
            Self::CacheRead => Cow::Borrowed("cache_read_per_mtok"),
            Self::CacheWrite(PricingConfig::CACHE_WRITE_5M_SECONDS) => {
                Cow::Borrowed("cache_write_5m_per_mtok")
            }
            Self::CacheWrite(PricingConfig::CACHE_WRITE_1H_SECONDS) => {
                Cow::Borrowed("cache_write_1h_per_mtok")
            }
            Self::CacheWrite(seconds) => Cow::Owned(format!(
                "cache_write_per_mtok.{}",
                routectl_core::format_cache_lifetime(seconds)
            )),
        }
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
            PriceSource::ModelConfig | PriceSource::Derived | PriceSource::Unknown => None,
        }
    }
}

/// The resolved retry/fallback policy for one failure class, tagged with the
/// layer that supplied its retry cap. Fallback is always the built-in value.
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
    /// A `[retry.classes]` count the operator set.
    Config,
    /// The compiled-in baked class default (no operator count for this class).
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
    /// The entry's provider family token, from [`ProviderEntry::kind_str`].
    pub kind: String,
    /// The entry's `format = "..."` value.
    pub format: String,
    /// The CONFIGURED endpoint origin (scheme + host + port), or `None` when
    /// the entry's carrier is a cloud or the value does not parse. A cloud
    /// entry's endpoint is derived at factory time and is shown as
    /// [`Self::cloud`] instead.
    pub endpoint_origin: Option<String>,
    /// The cloud carrier: its name, then its region when the region is a
    /// canonical AWS region identifier (`aws us-east-1`); `None` for a
    /// base-URL entry.
    pub cloud: Option<String>,
    /// The SCHEME of the entry's `credential` (`env`, `file`, `oauth`,
    /// `aws-profile`, `aws-chain`, `forwarded`, `literal`), never the
    /// credential body. `unknown` for a present value off that allowlist (a
    /// pasted bare value included); `None` for a blank credential.
    pub credential_ref_scheme: Option<String>,
    /// The entry's `auth = "..."` value: a closed vocabulary (`x-api-key`,
    /// `bearer`, `x-goog-api-key`, `aws-sigv4`).
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
/// it does NOT reject userinfo. So
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

/// The leak-safe scheme of a credential (`env://VAR` -> `env`,
/// `aws://profile/x` -> `aws-profile`), never the body.
///
/// The body names a variable, a filesystem path, an OAuth provider id or an
/// AWS profile; only the scheme says WHERE the credential class lives, which
/// is the whole of what a read surface needs.
///
/// A credential that parses reports its closed-vocabulary scheme label. One
/// that does not falls back to an ALLOWLIST over its text: an unrecognized
/// value is not a ref with an exotic scheme, it is a bare value someone
/// pasted into the field, and a bare value IS secret material. A pasted
/// `AKIA<id>:<secret>` reaches here intact, so returning the text before its
/// first colon would publish the key id. A present value off the list
/// (colon or not) collapses to the fixed `unknown`, echoing none of its
/// bytes; only a blank value is `None`.
///
/// The fallback match is deliberately looser than [`SecretScheme::classify`]
/// (case-insensitive, `env:VAR` counts as `env`): this surface reports which
/// credential class the operator MEANT, not whether the ref parses.
fn credential_ref_scheme(credential: &str) -> Option<String> {
    let raw = credential.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(parsed) = CredentialRef::parse(raw) {
        return Some(parsed.scheme_label().to_string());
    }
    let token = raw.split_once(':').map_or("", |(scheme, _)| scheme);
    let label = SecretScheme::ALL
        .iter()
        .map(|scheme| scheme.as_token())
        .find(|known| token.eq_ignore_ascii_case(known))
        .unwrap_or(UNKNOWN_CREDENTIAL_REF_SCHEME);
    Some(label.to_string())
}

/// What a present credential off the scheme allowlist renders as.
const UNKNOWN_CREDENTIAL_REF_SCHEME: &str = "unknown";

/// The cloud cell: the cloud name, plus the region only when it is a
/// canonical AWS region identifier (operator text that fails admission is
/// not echoed).
fn cloud_cell(entry: &ProviderEntry) -> Option<String> {
    let region = entry.cloud.as_ref()?.aws_region();
    Some(if routectl_providers::validate_aws_region(region).is_ok() {
        format!("aws {region}")
    } else {
        "aws".to_string()
    })
}

/// Project one `[providers.X]` entry into its secret-safe cell.
fn provider_cell(id: &str, entry: &ProviderEntry) -> ProviderCell {
    ProviderCell {
        id: id.to_string(),
        kind: entry.kind_str().to_string(),
        format: entry.format().as_str().to_string(),
        endpoint_origin: entry.base_url.as_deref().and_then(endpoint_origin),
        cloud: cloud_cell(entry),
        credential_ref_scheme: credential_ref_scheme(&entry.credential),
        auth_token: Some(entry.auth.as_str().to_string()),
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

/// One `[pools.<name>]` block: its seat-selection strategy and every
/// configured member, whether or not the member's provider built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolCell {
    /// The `[pools]` table key.
    pub pool: String,
    /// The strategy dispatch picks among the members with.
    pub seat_selection: SeatSelection,
    /// One entry per configured member, in the order the block lists them.
    pub members: Vec<PoolMemberCell>,
}

/// One configured pool member and the credential account it reads, named
/// by the [`AccountIdentities`] rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMemberCell {
    /// The member's `[providers]` table key.
    pub entry: String,
    /// The member's credential account key; `None` for an unknown or
    /// forwarded entry, which names no account.
    pub account: Option<String>,
    /// The `<label>` of the account's `oauth://<provider>#<label>` login;
    /// `None` for an unlabeled login, any other credential, and an account
    /// no model dispatches on.
    pub login_label: Option<String>,
}

/// Every `[pools]` block in key order, walked from config so a member whose
/// provider failed to build is still listed.
fn pool_cells(config: &Config) -> Vec<PoolCell> {
    let identities = AccountIdentities::from_config(config);
    config
        .pools
        .iter()
        .map(|(pool, entry)| PoolCell {
            pool: pool.clone(),
            seat_selection: entry.seat_selection,
            members: entry
                .members
                .iter()
                .map(|member| {
                    let account = AccountIdentities::account_of_entry(config, member);
                    let login_label = account
                        .as_deref()
                        .and_then(|account| identities.get(account))
                        .and_then(|identity| identity.login_label.clone());
                    PoolMemberCell {
                        entry: member.clone(),
                        account,
                        login_label,
                    }
                })
                .collect(),
        })
        .collect()
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
    /// One entry per `[pools.X]` block, in pool-key order.
    pub pools: Vec<PoolCell>,
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
            // A pool prices through the member entry usage rows bill under.
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
        pools: pool_cells(config),
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
