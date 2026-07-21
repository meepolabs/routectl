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
    ActivationEntry, ActivationState, ActivationStatus, ClassPolicyCell, ClassPolicySource,
    EffectiveRow, EffectiveView, ModelCell, OverrideProvenance, OverrideRow, OverrideVerdict,
    Source,
};

use super::vocabulary::{codes, provenance};
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};
use crate::server::load_overlay_default;

/// Wire-shape version of the config panel payload.
const SCHEMA_VERSION: u32 = 1;

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
    /// Auto-activation inventory: one entry per routectl-owned OAuth provider.
    activation: Vec<ActivationWire>,
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
        source: class_source_token(cell.source),
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

/// Map the derived effective view and activation inventory into the wire DTO.
/// Pure over its inputs, so the mapping is unit-testable without the router or
/// a disk read.
fn build_panel(effective: EffectiveView, activation: &ActivationState) -> ConfigPanel {
    ConfigPanel {
        models: effective.models.into_iter().map(map_model).collect(),
        classes: effective.classes.into_iter().map(map_class).collect(),
        capabilities: effective
            .capabilities
            .into_iter()
            .map(map_capability)
            .collect(),
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
    let view = state.router.view();
    let activation = state.activation.load_full();
    // The router snapshot and activation are pinned now, so request time IS
    // the effective view's read time.
    let as_of = now_utc_rfc3339();
    let panel = guard_panel(SCHEMA_VERSION, codes::CONFIG_UNAVAILABLE, move || {
        let overlay = match load_overlay_default() {
            Ok(overlay) => overlay,
            Err(err) => return unavailable_from_overlay_error(&err),
        };
        let effective = view.effective_view(&overlay);
        let dto = build_panel(effective, &activation);
        Panel::available(SCHEMA_VERSION, as_of, dto)
    })
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
    use crate::server::AppState;
    use arc_swap::ArcSwap;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use routectl_router::{CatalogRow, Config, Router};
    use serde_json::Value;
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn test_state() -> Arc<StatusState> {
        let router = Router::new(Arc::new(Config::default()));
        // The config panel reads only the live router and the catalog overlay
        // (a missing overlay file loads as empty), never the usage ledger, so
        // the temp dir may drop immediately.
        let (app, _dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        Arc::new(StatusState::from_app(
            &app,
            Some(PathBuf::from("/nonexistent/config.toml")),
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
        };
        let activation = ActivationState::default();

        let panel = build_panel(effective, &activation);

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

    /// Test #7 (redaction): an overlay load failure whose raw error carries a
    /// filesystem path and a secret-shaped value must never reach the payload.
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
        assert!(json["data"]["activation"].is_array());
    }
}
