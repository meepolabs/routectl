//! `/status/config` panel: the provenance-annotated EFFECTIVE view (the LIVE,
//! in-effect config) plus the auto-activation inventory.
//!
//! The panel answers "what config is actually IN EFFECT", so its source is the
//! live router config -- reached ONLY through the read-only facade
//! ([`super::router_view`]), which derives the secret-free [`EffectiveView`]
//! internally. The raw `Config` is never imported or serialized here; a
//! regression that tried to would trip the forbidden-import seam test.
//!
//! Per request the panel loads a fresh catalog overlay from disk (a SYNC
//! loader, so it runs inside the `spawn_blocking` builder [`guard_panel`]
//! wraps it in) and folds it against the live config into the effective view.
//! No overlay is retained on the router. On an overlay load/parse failure the
//! raw loader error -- which can carry a filesystem path or a config value --
//! is dropped entirely: the wire carries only a fixed reason code, and the
//! panel's availability edge is logged centrally by that fixed code (see
//! [`super::PanelCounters`]), never per poll and never with the raw error.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_router::class_policy::ConfigFailureClass;
use routectl_router::{
    ActivationEntry, ActivationState, ActivationStatus, AliasChain, ClassPolicyCell,
    ClassPolicySource, EffectiveRow, EffectiveView, ModelCell, OverrideProvenance, OverrideRow,
    OverrideVerdict, ProviderCell, Source, class_debits,
};

use super::daemon_meta::DaemonMetaSnapshot;
use super::vocabulary::{codes, provenance};
use super::{Panel, StatusState, guard_panel, utc_rfc3339};
use crate::server::load_overlay_default;

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
/// listener auth. It is the resolved path only -- never config CONTENT, and
/// never a loader error string (see [`unavailable_from_overlay_error`]).
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
    /// Provenance token from the shared routing-filter vocabulary
    /// (`provider` / `model` / `override`).
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

/// The capability provenance token, reusing the shared routing-filter
/// vocabulary so the effective view and a route-away log read one dialect.
const fn provenance_token(source: OverrideProvenance) -> &'static str {
    match source {
        OverrideProvenance::ProviderStatic => provenance::PROVIDER,
        OverrideProvenance::ModelStatic => provenance::MODEL,
        OverrideProvenance::Override => provenance::OVERRIDE,
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
        provenance: provenance_token(row.provenance),
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

/// Fold an overlay load failure into an unavailable panel. The raw loader
/// error (`_err`) can carry a filesystem path or a config value, so it is
/// dropped entirely -- only the fixed [`codes::CONFIG_UNAVAILABLE`] reaches
/// the wire, and the availability edge is logged centrally by that code (see
/// [`super::PanelCounters`]), never per poll and never with the raw error.
fn unavailable_from_overlay_error(_err: &str) -> Panel<ConfigPanel> {
    Panel::unavailable(SCHEMA_VERSION, codes::CONFIG_UNAVAILABLE)
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
            let overlay = match load_overlay_default() {
                Ok(overlay) => overlay,
                Err(err) => return unavailable_from_overlay_error(&err),
            };
            let effective = view.effective_view(&overlay);
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
mod tests {
    use super::*;
    use crate::handlers::status::DaemonMeta;
    use crate::server::AppState;
    use arc_swap::ArcSwap;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use routectl_router::config::AliasValue;
    use routectl_router::{CatalogOverlay, CatalogRow, Config, Router, derive_effective_view};
    use serde_json::Value;
    use std::path::PathBuf;
    use tower::ServiceExt;

    /// A fixed daemon snapshot so the source-strip assertions pin exact
    /// values instead of racing a live clock.
    fn sample_daemon() -> DaemonMetaSnapshot {
        DaemonMetaSnapshot {
            listen_addr: "127.0.0.1:9000".to_string(),
            version: env!("CARGO_PKG_VERSION"),
            config_loaded_age_ms: Some(4_000),
        }
    }

    /// Build the wire DTO through the SAME derivation the handler uses, so a
    /// config-shaped assertion exercises the real path end to end.
    fn panel_from_config(config: &Config) -> ConfigPanel {
        let effective = derive_effective_view(config, &CatalogOverlay::default());
        build_panel(
            effective,
            &ActivationState::default(),
            None,
            sample_daemon(),
        )
    }

    fn test_state() -> Arc<StatusState> {
        let router = Router::new(Arc::new(Config::default()));
        // The config panel reads only the live router and the catalog overlay
        // (a missing overlay file loads as empty), never the usage ledger, so
        // the temp dir may drop immediately.
        let (app, _dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        Arc::new(StatusState::from_app(
            &app,
            Some(PathBuf::from("/nonexistent/config.toml")),
            DaemonMeta::for_test(),
        ))
    }

    fn present_model(nickname: &str, source: Source) -> ModelCell {
        ModelCell {
            nickname: nickname.to_string(),
            provider: "anthropic".to_string(),
            provider_kind: "anthropic-api".to_string(),
            upstream: "claude-opus-4-8".to_string(),
            row: EffectiveRow::Present {
                row: CatalogRow::sentinel(),
                source,
                verified_at: "2026-07-01".to_string(),
            },
        }
    }

    #[test]
    fn source_tokens_pin_every_catalog_layer() {
        assert_eq!(source_token(Source::Baked), "baked");
        assert_eq!(source_token(Source::Import), "import");
        assert_eq!(source_token(Source::User), "user");
    }

    #[test]
    fn class_source_and_class_tokens_are_kebab_case() {
        assert_eq!(class_source_token(ClassPolicySource::Config), "config");
        assert_eq!(
            class_source_token(ClassPolicySource::BakedDefault),
            "baked-default"
        );
        assert_eq!(class_token(ConfigFailureClass::RateLimited), "rate-limited");
        assert_eq!(
            class_token(ConfigFailureClass::FeatureUnsupported),
            "feature-unsupported"
        );
    }

    #[test]
    fn capability_tokens_reuse_the_routing_filter_vocabulary() {
        assert_eq!(verdict_token(OverrideVerdict::RouteAway), "route-away");
        assert_eq!(
            verdict_token(OverrideVerdict::ForceSupported),
            "force-supported"
        );
        assert_eq!(
            provenance_token(OverrideProvenance::ProviderStatic),
            "provider"
        );
        assert_eq!(provenance_token(OverrideProvenance::ModelStatic), "model");
        assert_eq!(provenance_token(OverrideProvenance::Override), "override");
    }

    #[test]
    fn build_panel_maps_every_effective_surface_with_provenance() {
        let effective = EffectiveView {
            models: vec![
                present_model("opus", Source::User),
                ModelCell {
                    nickname: "ghost".to_string(),
                    provider: "ghost".to_string(),
                    provider_kind: String::new(),
                    upstream: "nope".to_string(),
                    row: EffectiveRow::Missing,
                },
            ],
            classes: vec![ClassPolicyCell {
                class: ConfigFailureClass::RateLimited,
                retry_cap: 7,
                fallback: true,
                source: ClassPolicySource::Config,
            }],
            capabilities: vec![OverrideRow {
                target_spec: "anthropic".to_string(),
                capability_key: "web_search".to_string(),
                verdict: OverrideVerdict::RouteAway,
                provenance: OverrideProvenance::Override,
            }],
            aliases: Vec::new(),
            providers: Vec::new(),
        };
        let activation = ActivationState::default();

        let panel = build_panel(effective, &activation, None, sample_daemon());

        let opus = &panel.models[0];
        assert_eq!(opus.source, "user");
        assert_eq!(opus.verified_at.as_deref(), Some("2026-07-01"));
        assert!(opus.economics.is_some());
        assert_eq!(panel.models[1].source, "missing");
        assert!(panel.models[1].economics.is_none());

        assert_eq!(panel.classes[0].class, "rate-limited");
        assert_eq!(panel.classes[0].retry_cap, 7);
        assert_eq!(panel.classes[0].source, "config");

        assert_eq!(panel.capabilities[0].verdict, "route-away");
        assert_eq!(panel.capabilities[0].provenance, "override");

        assert!(panel.activation.is_empty());
    }

    /// A `Single` alias serializes as a one-element chain and a `Chain` keeps
    /// the operator's order verbatim -- the chain order IS the fallback
    /// sequence dispatch walks, so a sort or a dedupe would misreport routing.
    #[test]
    fn alias_chains_preserve_single_and_chain_order() {
        let mut config = Config::default();
        config
            .aliases
            .insert("solo".to_string(), AliasValue::Single("opus".to_string()));
        config.aliases.insert(
            "fast".to_string(),
            AliasValue::Chain(vec![
                "small".to_string(),
                "smaller".to_string(),
                "smallest".to_string(),
            ]),
        );

        let panel = panel_from_config(&config);

        let solo = panel
            .aliases
            .iter()
            .find(|a| a.alias == "solo")
            .expect("single alias present");
        assert_eq!(solo.chain, vec!["opus".to_string()]);

        let fast = panel
            .aliases
            .iter()
            .find(|a| a.alias == "fast")
            .expect("chain alias present");
        assert_eq!(
            fast.chain,
            vec![
                "small".to_string(),
                "smaller".to_string(),
                "smallest".to_string()
            ],
            "the fallback chain must keep the configured order verbatim"
        );
    }

    /// The source strip's counts derive from the SAME effective view the
    /// tables render from, and every declared field reaches the wire.
    #[test]
    fn source_object_carries_every_declared_field() {
        let mut config = Config::default();
        config
            .aliases
            .insert("solo".to_string(), AliasValue::Single("opus".to_string()));
        config.aliases.insert(
            "fast".to_string(),
            AliasValue::Chain(vec!["small".to_string()]),
        );

        let effective = derive_effective_view(&config, &CatalogOverlay::default());
        let panel = build_panel(
            effective,
            &ActivationState::default(),
            Some("/etc/routectl/config.toml"),
            sample_daemon(),
        );

        let value = serde_json::to_value(&panel.source).unwrap();
        let obj = value.as_object().unwrap();
        for key in [
            "config_path",
            "loaded_age_ms",
            "alias_count",
            "provider_count",
            "listen_addr",
            "version",
        ] {
            assert!(obj.contains_key(key), "source strip must carry `{key}`");
        }
        assert_eq!(
            panel.source.config_path.as_deref(),
            Some("/etc/routectl/config.toml")
        );
        assert_eq!(panel.source.loaded_age_ms, Some(4_000));
        assert_eq!(panel.source.alias_count, 2);
        assert_eq!(panel.source.provider_count, 0);
        assert_eq!(panel.source.listen_addr, "127.0.0.1:9000");
        assert_eq!(panel.source.version, env!("CARGO_PKG_VERSION"));
    }

    /// A server bound without an on-disk config reports `None`, never an
    /// invented placeholder path.
    #[test]
    fn source_object_reports_no_path_without_an_on_disk_config() {
        let panel = panel_from_config(&Config::default());
        assert!(panel.source.config_path.is_none());
    }

    /// `debits_breaker` is read from the router's own transient-health set, so
    /// a recoverable class debits the breaker and a caller-shaped one does not.
    /// The set itself is never restated here -- only its verdict is asserted.
    #[test]
    fn debits_breaker_is_true_for_transient_and_false_for_caller_shaped_classes() {
        let panel = panel_from_config(&Config::default());
        let debits = |class: &str| {
            panel
                .classes
                .iter()
                .find(|c| c.class == class)
                .unwrap_or_else(|| panic!("class `{class}` present in the effective view"))
                .debits_breaker
        };

        assert!(debits("rate-limited"));
        assert!(debits("server-error"));
        assert!(debits("timeout"));
        assert!(debits("network-error"));
        assert!(debits("overloaded"));

        assert!(!debits("auth"));
        assert!(!debits("bad-request"));
        assert!(!debits("content-policy"));
        assert!(!debits("context-window"));
        assert!(!debits("feature-unsupported"));
    }

    #[test]
    fn activation_wire_carries_the_contract_fields() {
        let value = serde_json::to_value(ActivationWire {
            provider_id: "anthropic".to_string(),
            provider_kind: "anthropic-api".to_string(),
            status: "unresolved",
            reason: Some("oauth_missing".to_string()),
            referenced_by_aliases: true,
        })
        .unwrap();
        let obj = value.as_object().unwrap();
        assert!(obj.contains_key("provider_id"));
        assert!(obj.contains_key("provider_kind"));
        assert!(obj.contains_key("status"));
        assert!(obj.contains_key("reason"));
        assert!(obj.contains_key("referenced_by_aliases"));
    }

    /// The three-provider fixture the RPM contract is stated over: an
    /// immediately-throttled entry, an unlimited one, and a capped one.
    fn config_with_three_rpm_states() -> Config {
        toml::from_str(
            r#"
version = 2

[providers.throttled]
kind = "anthropic-api"
api_key_ref = "env://THROTTLED_KEY"
base_url = "https://throttled.example/v1"
rpm_limit = 0

[providers.unlimited]
kind = "anthropic-api"
api_key_ref = "env://UNLIMITED_KEY"
base_url = "https://unlimited.example:8443/v1"

[providers.capped]
kind = "openai-compat"
api_key_ref = "file:///nonexistent/capped.key"
base_url = "https://capped.example/v1"
rpm_limit = 60
"#,
        )
        .expect("valid config")
    }

    fn provider_json(config: &Config, id: &str) -> Value {
        let panel = panel_from_config(config);
        let value = serde_json::to_value(&panel.providers).unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["provider_id"] == id)
            .unwrap_or_else(|| panic!("provider `{id}` on the wire"))
            .clone()
    }

    /// The RPM field's three states must be DISTINGUISHABLE on the wire, not
    /// merely distinguishable in Rust. An immediately-throttled provider
    /// carries an explicit numeric `0`; an unlimited one carries an explicit
    /// `null`. Skipping the absent case (a `skip_serializing_if`) would
    /// collapse "unlimited" into "the field never arrived", which the client
    /// reports as a different word -- so the key is always present.
    #[test]
    fn zero_rpm_reaches_the_wire_as_an_explicit_zero_and_unlimited_as_null() {
        let config = config_with_three_rpm_states();

        let throttled = provider_json(&config, "throttled");
        assert_eq!(
            throttled["rpm_limit"],
            Value::from(0),
            "a zero cap must reach the wire as an explicit numeric 0"
        );

        let unlimited = provider_json(&config, "unlimited");
        assert!(
            unlimited.as_object().unwrap().contains_key("rpm_limit"),
            "the unlimited case must carry the key explicitly, never omit it"
        );
        assert_eq!(unlimited["rpm_limit"], Value::Null);

        assert_eq!(
            provider_json(&config, "capped")["rpm_limit"],
            Value::from(60)
        );
    }

    /// Every declared provider field reaches the wire, and the endpoint is the
    /// derivation's ORIGIN -- never widened back toward a fuller URL here.
    #[test]
    fn provider_wire_carries_the_contract_fields_and_only_the_origin() {
        let config = config_with_three_rpm_states();
        let unlimited = provider_json(&config, "unlimited");
        let obj = unlimited.as_object().unwrap();
        for key in [
            "provider_id",
            "provider_kind",
            "endpoint_origin",
            "credential_ref_scheme",
            "auth_token",
            "rpm_limit",
        ] {
            assert!(obj.contains_key(key), "provider row must carry `{key}`");
        }
        assert_eq!(unlimited["provider_kind"], "anthropic-api");
        assert_eq!(
            unlimited["endpoint_origin"],
            "https://unlimited.example:8443"
        );
        assert_eq!(unlimited["credential_ref_scheme"], "env");
        assert_eq!(unlimited["auth_token"], "api-key");

        // The variant with no auth discriminant reports absence rather than a
        // spanning token, and its ref contributes the scheme only.
        let capped = provider_json(&config, "capped");
        assert_eq!(capped["auth_token"], Value::Null);
        assert_eq!(capped["credential_ref_scheme"], "file");
    }

    /// A `base_url` carrying a credential in userinfo, query, and fragment is
    /// an ACCEPTED provider config, so the panel is the last line before it
    /// would reach a browser. Nothing secret-shaped may survive the whole
    /// serialized provider list.
    #[test]
    fn provider_rows_never_carry_a_credential_or_a_ref_body() {
        let config: Config = toml::from_str(
            r#"
version = 2

[providers.leaky]
kind = "anthropic-api"
api_key_ref = "env://LEAKY_SECRET_VAR"
base_url = "https://user:sk-live-LEAKED@internal.example/v1?key=sk-live-LEAKED#sk-live-LEAKED"
"#,
        )
        .expect("valid config");

        let panel = panel_from_config(&config);
        let text = serde_json::to_string(&panel.providers).unwrap();

        for forbidden in ["sk-live-LEAKED", "user:", "LEAKY_SECRET_VAR", "/v1"] {
            assert!(
                !text.contains(forbidden),
                "provider rows leaked `{forbidden}`: {text}"
            );
        }
        assert!(text.contains("https://internal.example"));
    }

    /// The source strip's provider count and the provider table derive from the
    /// SAME effective view, so a rendered row can never be missing from the
    /// count.
    #[test]
    fn provider_count_matches_the_rendered_rows() {
        let panel = panel_from_config(&config_with_three_rpm_states());
        assert_eq!(panel.source.provider_count, panel.providers.len());
        assert_eq!(panel.providers.len(), 3);
    }

    /// Test #7 (redaction): an overlay load failure whose raw error carries a    /// filesystem path and a secret-shaped value must never reach the payload.
    /// The failure folds to an unavailable panel with the fixed code and no
    /// data, so nothing to leak survives.
    #[test]
    fn overlay_failure_yields_code_only_unavailable_panel() {
        let leaky = "catalog overlay load error: catalog overlay \
                     /home/someone/.config/routectl/catalog_overlay.json: corrupt or \
                     invalid: literal:sk-live-LEAKED at env://SECRET file:///etc/passwd \
                     oauth://anthropic";
        let panel = unavailable_from_overlay_error(leaky);
        assert_eq!(panel.unavailable.as_deref(), Some("config_unavailable"));
        assert!(panel.data.is_none());
        assert!(panel.as_of.is_none());

        let text = serde_json::to_string(&panel).unwrap();
        for forbidden in [
            "LEAKED",
            "literal:",
            "env://",
            "file://",
            "oauth://",
            "/home/",
            "/etc/",
            "catalog_overlay.json",
            "config.toml",
        ] {
            assert!(
                !text.contains(forbidden),
                "config unavailable payload leaked `{forbidden}`: {text}"
            );
        }
    }

    /// A read that lands mid-reload must skew CONSERVATIVELY: the source strip
    /// reports the NEW alias counts beside an age still measured from the
    /// PREVIOUS config load. The inverse -- a fresh age beside pre-reload
    /// counts -- reads as "my edit was ignored", so it must be unreachable.
    ///
    /// Sequenced writes, no threads: the daemon snapshot is pinned FIRST (as
    /// `build` pins it), the reload's router swap lands SECOND, and the panel
    /// is built from both. That `build` actually reads in that order is pinned
    /// by `build_reads_the_daemon_stamp_before_the_router_snapshot`.
    #[test]
    fn a_reload_after_the_daemon_read_skews_the_age_and_never_the_counts() {
        let router_swap = Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
            Config::default(),
        ))));
        let (app, _dir) = AppState::for_test(router_swap.clone());
        let state = StatusState::from_app(&app, None, DaemonMeta::for_test());
        let pinned_daemon = DaemonMetaSnapshot {
            listen_addr: "127.0.0.1:9000".to_string(),
            version: env!("CARGO_PKG_VERSION"),
            config_loaded_age_ms: Some(60_000),
        };

        let mut reloaded = Config::default();
        reloaded
            .aliases
            .insert("fresh".to_string(), AliasValue::Single("opus".to_string()));
        router_swap.store(Arc::new(Router::new(Arc::new(reloaded))));
        let effective = state
            .router
            .view()
            .effective_view(&CatalogOverlay::default());
        let panel = build_panel(effective, &ActivationState::default(), None, pinned_daemon);

        assert_eq!(
            panel.source.alias_count, 1,
            "the counts must come from the post-reload router, never the pre-reload one"
        );
        assert_eq!(
            panel.source.loaded_age_ms,
            Some(60_000),
            "the age must stay the pre-reload (conservative) one, never be refreshed \
             to sit beside stale counts"
        );
    }

    /// Structural guard on `build`'s read ORDER, which IS the mechanism: the
    /// daemon stamp is snapshotted BEFORE the router view is pinned. Reachable
    /// reload states are ordered `gen(router) >= gen(stamp)`, so the reverse
    /// order yields the harmful skew -- a fresh age beside pre-reload counts.
    /// A refactor back to the intuitive router-first shape lands here.
    #[test]
    fn build_reads_the_daemon_stamp_before_the_router_snapshot() {
        let src = include_str!("config.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("a split yields at least one segment");

        let stamp_read = production
            .find("daemon_meta.snapshot(")
            .expect("`build` snapshots the daemon meta");
        let router_read = production
            .find("router.view()")
            .expect("`build` pins the router view");

        assert!(
            stamp_read < router_read,
            "the daemon stamp must be read before the router view, or the source \
             strip reports a fresh load age beside pre-reload counts"
        );
    }

    #[tokio::test]
    async fn handler_returns_available_panel_with_effective_view() {
        let app = super::super::status_router().with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/status/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert!(json["unavailable"].is_null());
        let as_of = json["as_of"].as_str().expect("as_of present");
        assert!(
            chrono::DateTime::parse_from_rfc3339(as_of).is_ok(),
            "as_of must be RFC3339: {as_of}"
        );
        // A default config has no models and no capability overrides, but every
        // failure class is represented in the effective class policy view.
        assert!(json["data"]["models"].is_array());
        assert!(json["data"]["classes"].as_array().unwrap().len() >= 10);
        assert!(json["data"]["capabilities"].is_array());
        assert!(json["data"]["aliases"].is_array());
        assert!(json["data"]["providers"].is_array());
        assert!(json["data"]["source"].is_object());
        assert!(json["data"]["activation"].is_array());
    }
}
