//! `/status/config` panel: the provenance-annotated EFFECTIVE view (the LIVE,
//! in-effect config) plus the auto-activation inventory.
//!
//! The panel answers "what config is actually IN EFFECT", so its source is the
//! live router config -- reached ONLY through the read-only facade
//! ([`super::router_view`]), which derives the secret-free [`EffectiveView`]
//! internally. The raw `Config` is never imported or serialized here; a
//! regression that tried to would trip the forbidden-import seam test.
//!
//! The overlay half of that view is the generation the daemon ACCEPTED at
//! its last successful boot or reload: it is retained on the live router and
//! read from memory, never re-read from disk per request. Reading disk here
//! would let the panel render economics and provenance from a file the daemon
//! REJECTED (routing continues on the previous generation), and would make a
//! corrupt file on disk take the whole panel down while routing is healthy.
//! Because config and overlay now come from the SAME pinned router, one
//! render can never pair a config generation with a different overlay one.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_router::class_policy::ConfigFailureClass;
use routectl_router::{
    ActivationEntry, ActivationState, ActivationStatus, AliasChain, ClassPolicyCell,
    ClassPolicySource, EffectiveRow, EffectiveView, ModelCell, OverrideRow, OverrideVerdict,
    ProviderCell, Source, class_debits,
};

use super::daemon_meta::DaemonMetaSnapshot;
use super::vocabulary::{codes, provenance};
use super::{Panel, StatusState, guard_panel, utc_rfc3339};

/// Wire-shape version of the config panel payload.
pub const SCHEMA_VERSION: u32 = 3;

/// The provenance-annotated effective view plus the activation inventory. Every
/// field is a display-safe projection: model economics and capability verdicts
/// carry no secrets, and the activation entries carry only discriminants.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ConfigPanel {
    /// One entry per `[models.X]` table, with its winning catalog layer.
    models: Vec<ModelCellWire>,
    /// One entry per operator-nameable failure class, with its winning layer.
    classes: Vec<ClassPolicyWire>,
    /// Config-derived capability-override cells, with verdict + provenance.
    capabilities: Vec<CapabilityCellWire>,
    /// One entry per `[aliases]` entry, with its ordered fallback chain.
    aliases: Vec<AliasChainWire>,
    /// One entry per `[providers.X]` entry: the routing-shape facts only.
    providers: Vec<ProviderWire>,
    /// Provenance of the config in effect plus the daemon serving it.
    source: SourceWire,
    /// Auto-activation inventory: one entry per routectl-owned OAuth provider.
    activation: Vec<ActivationWire>,
}

/// One alias's ordered fallback chain. The chain order IS the sequence
/// dispatch walks, so it is carried verbatim -- never sorted, never deduped.
#[derive(Debug, Clone, Serialize)]
struct AliasChainWire {
    alias: String,
    chain: Vec<String>,
}

/// One `[providers.X]` entry's routing shape, mapped field-by-field from the
/// secret-safe [`ProviderCell`].
///
/// Every field is either a closed-vocabulary token or a structurally
/// secret-free reduction the derivation already performed. Nothing here is
/// reconstructed or widened: the endpoint is carried as the ORIGIN the
/// derivation produced (a raw `base_url` may embed a credential in userinfo,
/// path, or query, and provider validation does not reject that), and the
/// credential ref contributes its SCHEME only, never its body. No field may
/// ever carry a secret-store name, an account id, or a resolved credential.
#[derive(Debug, Clone, Serialize)]
struct ProviderWire {
    /// The `[providers.<id>]` table key.
    provider_id: String,
    /// The entry's `kind = "..."` discriminant.
    provider_kind: String,
    /// The CONFIGURED endpoint origin (scheme + host + port), or `null` when
    /// the entry's config carries no endpoint or the value does not parse.
    /// `null` does NOT mean "no endpoint": several kinds derive theirs at
    /// factory time, which a pure derivation over the config cannot reach.
    endpoint_origin: Option<String>,
    /// The URI SCHEME of the entry's primary `api_key_ref` (`env`, `file`,
    /// `oauth`, ...), never the ref body.
    credential_ref_scheme: Option<String>,
    /// This variant's OWN auth-mechanism token, or `null` for a kind with no
    /// auth discriminant. A closed vocabulary from the router's single
    /// extraction point, never an operator-supplied value.
    auth_token: Option<String>,
    /// The configured requests-per-minute cap. Three states reach the client,
    /// none interchangeable: a positive number is a live limit, an explicit
    /// `0` means IMMEDIATELY rate-limited (it seeds a zero-capacity bucket),
    /// and an explicit `null` means unlimited.
    ///
    /// Deliberately NOT `skip_serializing_if = "Option::is_none"`: skipping
    /// would make "unlimited" indistinguishable from "the field never
    /// arrived", and the renderer reports those as different words. The field
    /// is therefore ALWAYS present on this wire, `null` included.
    rpm_limit: Option<u32>,
}

/// Where the effective config came from and which daemon is serving it.
///
/// `config_path` is the ONE deliberate filesystem path on this wire: the
/// dashboard's source strip exists to answer "which file is in effect", and
/// the whole status surface is already gated behind the host allowlist plus
/// listener auth. It is the resolved path only -- never config CONTENT.
#[derive(Debug, Clone, Serialize)]
struct SourceWire {
    /// Resolved `config.toml` path, or `None` when serving from a config
    /// with no on-disk backing (a programmatic / test bind).
    config_path: Option<String>,
    /// How long ago the live config was loaded, or `None` before any load
    /// has been stamped.
    loaded_age_ms: Option<i64>,
    /// Number of `[aliases]` entries in the effective view.
    alias_count: usize,
    /// Number of `[providers.X]` entries in the effective view.
    provider_count: usize,
    /// The address the daemon's listener is bound to.
    listen_addr: String,
    /// The running binary's version.
    version: &'static str,
}

/// One `[models.X]` entry's effective catalog cell.
#[derive(Debug, Clone, Serialize)]
struct ModelCellWire {
    nickname: String,
    provider: String,
    provider_kind: String,
    upstream: String,
    /// Winning layer token: `baked` / `import` / `user` for a present row,
    /// or `disabled` / `missing`.
    source: &'static str,
    verified_at: Option<String>,
    economics: Option<EconomicsWire>,
}

/// The secret-free economics fields of a present catalog row.
#[derive(Debug, Clone, Serialize)]
struct EconomicsWire {
    wm: f32,
    rm: f32,
    max_context_tokens: Option<u32>,
}

/// One failure class's resolved retry/fallback policy and winning layer.
#[derive(Debug, Clone, Serialize)]
struct ClassPolicyWire {
    /// Kebab-case class token (matches the `[retry.classes.<class>]` key).
    class: &'static str,
    retry_cap: u32,
    fallback: bool,
    /// Whether a failure of this class debits the per-seat circuit breaker's
    /// health accounting. Read from the router's own [`class_debits`] -- the
    /// transient-health set has exactly one definition, and this wire field
    /// must never restate it.
    debits_breaker: bool,
    /// `config` (operator leaf) or `baked-default`.
    source: &'static str,
}

/// One config-derived capability-override cell.
#[derive(Debug, Clone, Serialize)]
struct CapabilityCellWire {
    target_spec: String,
    capability_key: String,
    /// `route-away` or `force-supported`.
    verdict: &'static str,
    /// Provenance token from the shared routing-filter vocabulary; always
    /// `override` for a config-derived cell.
    provenance: &'static str,
}

/// One provider's activation record, mapped field-by-field from the
/// `#[non_exhaustive]` [`ActivationEntry`].
#[derive(Debug, Clone, Serialize)]
struct ActivationWire {
    provider_id: String,
    provider_kind: String,
    /// `activated`, `unresolved`, or `unknown` (an activation state this
    /// build does not recognize).
    status: &'static str,
    /// Machine-readable reason code, present only when `status` is
    /// `unresolved`.
    reason: Option<String>,
    referenced_by_aliases: bool,
}

/// The winning catalog-layer token for a present row.
const fn source_token(source: Source) -> &'static str {
    match source {
        Source::Baked => "baked",
        Source::Import => "import",
        Source::User => "user",
    }
}

/// The winning class-policy-layer token.
const fn class_source_token(source: ClassPolicySource) -> &'static str {
    match source {
        ClassPolicySource::Config => "config",
        ClassPolicySource::BakedDefault => "baked-default",
    }
}

/// The kebab-case class token, delegated to the canonical
/// [`FailureClass::class_token`] so the wire vocabulary has a single source.
/// Every config-facing class names a canonical class, so the token is always
/// present.
///
/// [`FailureClass::class_token`]: routectl_core::failure_class::FailureClass::class_token
fn class_token(class: ConfigFailureClass) -> &'static str {
    class
        .to_failure_class()
        .class_token()
        .expect("a config-facing failure class always has a canonical token")
}

/// The capability verdict token.
const fn verdict_token(verdict: OverrideVerdict) -> &'static str {
    match verdict {
        OverrideVerdict::RouteAway => "route-away",
        OverrideVerdict::ForceSupported => "force-supported",
    }
}

fn map_model(cell: ModelCell) -> ModelCellWire {
    let (source, verified_at, economics) = match cell.row {
        EffectiveRow::Present {
            row,
            source,
            verified_at,
        } => (
            source_token(source),
            Some(verified_at),
            Some(EconomicsWire {
                wm: row.wm,
                rm: row.rm,
                max_context_tokens: row.max_context_tokens,
            }),
        ),
        EffectiveRow::Disabled => ("disabled", None, None),
        EffectiveRow::Missing => ("missing", None, None),
    };
    ModelCellWire {
        nickname: cell.nickname,
        provider: cell.provider,
        provider_kind: cell.provider_kind,
        upstream: cell.upstream,
        source,
        verified_at,
        economics,
    }
}

fn map_class(cell: ClassPolicyCell) -> ClassPolicyWire {
    ClassPolicyWire {
        class: class_token(cell.class),
        retry_cap: cell.retry_cap,
        fallback: cell.fallback,
        debits_breaker: class_debits(&cell.class.to_failure_class()),
        source: class_source_token(cell.source),
    }
}

fn map_alias(chain: AliasChain) -> AliasChainWire {
    AliasChainWire {
        alias: chain.alias,
        chain: chain.chain,
    }
}

/// Map one secret-safe provider cell onto the wire. A pure field-for-field
/// move: every reduction (the endpoint origin, the ref scheme, the auth token)
/// already happened in the router's derivation, and re-deriving any of them
/// here would mint a second source of truth for a secret boundary.
fn map_provider(cell: ProviderCell) -> ProviderWire {
    ProviderWire {
        provider_id: cell.id,
        provider_kind: cell.kind,
        endpoint_origin: cell.endpoint_origin,
        credential_ref_scheme: cell.credential_ref_scheme,
        auth_token: cell.auth_token,
        rpm_limit: cell.rpm_limit,
    }
}

fn map_capability(row: OverrideRow) -> CapabilityCellWire {
    CapabilityCellWire {
        target_spec: row.target_spec,
        capability_key: row.capability_key,
        verdict: verdict_token(row.verdict),
        // Every config-derived cell comes from `[capability.overrides]`; the
        // token is the routing filter's own skip-log `source` value, so the
        // effective view and a route-away log read one dialect.
        provenance: provenance::OVERRIDE,
    }
}

fn map_activation((id, entry): (&str, &ActivationEntry)) -> ActivationWire {
    let (status, reason) = match entry.status {
        ActivationStatus::Activated => ("activated", None),
        ActivationStatus::Unresolved { reason } => {
            ("unresolved", Some(reason.as_str().to_string()))
        }
        _ => ("unknown", None),
    };
    ActivationWire {
        provider_id: id.to_string(),
        provider_kind: entry.provider_kind.to_string(),
        status,
        reason,
        referenced_by_aliases: entry.referenced_by_aliases,
    }
}

/// Fold the effective view's own counts together with the daemon facts into
/// the source strip. The counts come from the SAME view the tables render
/// from, so a rendered alias can never be missing from the count.
fn build_source(
    effective: &EffectiveView,
    config_path: Option<&str>,
    daemon: DaemonMetaSnapshot,
) -> SourceWire {
    SourceWire {
        config_path: config_path.map(str::to_string),
        loaded_age_ms: daemon.config_loaded_age_ms,
        alias_count: effective.aliases.len(),
        provider_count: effective.providers.len(),
        listen_addr: daemon.listen_addr,
        version: daemon.version,
    }
}

/// Map the derived effective view and activation inventory into the wire DTO.
/// Pure over its inputs, so the mapping is unit-testable without the router or
/// a disk read.
fn build_panel(
    effective: EffectiveView,
    activation: &ActivationState,
    config_path: Option<&str>,
    daemon: DaemonMetaSnapshot,
) -> ConfigPanel {
    let source = build_source(&effective, config_path, daemon);
    ConfigPanel {
        models: effective.models.into_iter().map(map_model).collect(),
        classes: effective.classes.into_iter().map(map_class).collect(),
        capabilities: effective
            .capabilities
            .into_iter()
            .map(map_capability)
            .collect(),
        aliases: effective.aliases.into_iter().map(map_alias).collect(),
        providers: effective.providers.into_iter().map(map_provider).collect(),
        source,
        activation: activation.iter().map(map_activation).collect(),
    }
}

pub(super) async fn build(state: &StatusState) -> Panel<ConfigPanel> {
    // Read order is load-bearing. ONE clock read anchors both the `as_of` and
    // the source strip's config-load age, so a request crossing a second
    // boundary cannot report an age inconsistent with the `as_of` beside it.
    //
    // A reload writes three cells independently (router, then the config-load
    // stamp, then activation), so a read landing mid-reload cannot see one
    // generation and no ordering can give it one -- ordering only picks the
    // skew DIRECTION. Reachable states are ordered
    // `gen(router) >= gen(stamp) >= gen(activation)`, and that holds ONLY
    // because `crate::server::reload` swaps the new router in BEFORE it stamps
    // the load. Given that, taking the stamp FIRST skews conservatively: a
    // mid-reload read shows the NEW alias/provider counts beside an age still
    // measured from the previous load, which self-corrects on the next poll.
    // Pinning the router first produces the harmful skew instead -- "loaded 0s
    // ago" beside PRE-reload counts, which an operator reads as their edit
    // having been ignored. Reordering these lines back to the intuitive
    // router-first shape reintroduces that lie; the source-order guard in this
    // module's tests fails if it does.
    //
    // Residual, accepted: router-vs-activation can still skew either way and
    // no ordering fixes it. The dashboard already footnotes those as different
    // sets.
    let now = chrono::Utc::now();
    let as_of = utc_rfc3339(now);
    let daemon = state.daemon_meta.snapshot(now.timestamp_millis());
    let view = state.router.view();
    let activation = state.activation.load_full();
    let config_path = state
        .config_path
        .as_ref()
        .map(|path| path.display().to_string());
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::CONFIG_UNAVAILABLE,
        move || {
            let effective = view.effective_view();
            let dto = build_panel(effective, &activation, config_path.as_deref(), daemon);
            Panel::available(SCHEMA_VERSION, as_of, dto)
        },
    )
    .await;
    state.observability.config.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<ConfigPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
