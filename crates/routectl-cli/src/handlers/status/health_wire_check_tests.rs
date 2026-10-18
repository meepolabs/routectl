//! The health panel's wire-check section, end to end over a live router: a
//! loss found on a real dispatch renders by lane and class, never by value, the
//! panel's existing fields serialize exactly as before, and building the
//! panel reads the wire-check state without changing it.

use super::*;

use std::collections::BTreeMap;

use arc_swap::ArcSwap;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, header};
use futures::stream::BoxStream;
use routectl_core::wire_check::{EgressKind, IngressDialect, Lane, Leg, PathClass, WirePath};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, Router};
use serde_json::{Value, json};

use crate::handlers::ingress_handle::ingress_handle;
use crate::handlers::status::DaemonMeta;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

const BODY_SENTINEL: &str = "BODY-SENTINEL-must-never-render";

/// Answers every call and, like a real egress, deposits into the attempt's
/// tap the client's body with its top-level `metadata` removed.
struct DropsMetadata;

#[async_trait::async_trait]
impl Provider for DropsMetadata {
    fn id(&self) -> &'static str {
        "drops-metadata"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("drops-metadata", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let internal = &req.routectl_internal;
        if let (Some(tap), Some(ingress)) = (&internal.wire_tap, &internal.ingress_wire) {
            let mut body: Value = serde_json::from_slice(&ingress.bytes).expect("ingress json");
            body.as_object_mut()
                .expect("object body")
                .remove("metadata");
            tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
        }
        Ok(ChatResponse::default())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("drops-metadata", 501, "unused"))
    }
}

fn router_with(provider: Arc<dyn Provider>) -> Arc<ArcSwap<Router>> {
    let config = Config {
        providers: BTreeMap::from([("p".to_string(), ProviderEntry::anthropic_api("literal:k"))]),
        aliases: BTreeMap::from([("a".to_string(), AliasValue::Single("m".to_string()))]),
        ..Config::default()
    };
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream")),
    )]));
    Arc::new(ArcSwap::from_pointee(router))
}

fn status_over(swap: &Arc<ArcSwap<Router>>) -> (Arc<AppState>, StatusState) {
    let app = AppState::for_test(Arc::clone(swap));
    let status = StatusState::from_app(&app, None, DaemonMeta::for_test());
    (app, status)
}

fn panel_of(status: &StatusState) -> HealthPanel {
    build_from_view(
        &status.router.view(),
        false,
        FidelityEmission::always(),
        &FidelityGate::default(),
    )
}

#[tokio::test]
async fn a_loss_found_on_a_real_dispatch_renders_its_lane_and_class_and_never_the_body() {
    let swap = router_with(Arc::new(DropsMetadata));
    let (app, status) = status_over(&swap);
    let body = format!(
        r#"{{"model":"a","max_tokens":16,"metadata":{{"user_id":"{BODY_SENTINEL}"}},"messages":[{{"role":"user","content":"{BODY_SENTINEL}"}}]}}"#
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    ingress_handle(app, headers, None, Ok(Bytes::from(body)), AnthropicIngress).await;
    let text = serde_json::to_string(&panel_of(&status)).expect("panel serializes");

    let wire: Value = serde_json::from_str(&text).expect("panel json");
    assert_eq!(
        wire["wire_check"]["loss"][0]["lane"],
        json!("anthropic->anthropic-api"),
        "{text}"
    );
    assert_eq!(
        wire["wire_check"]["loss"][0]["path_class"],
        json!("missing")
    );
    assert!(
        !text.contains(BODY_SENTINEL),
        "the panel must carry no body content: {text}"
    );
}

/// The panel as it serialized before the wire-check section was added.
#[derive(Serialize)]
struct PanelWithoutWireCheck<'a> {
    targets: &'a Vec<TargetHealth>,
    learned_negatives: &'a Vec<LearnedNegative>,
}

#[test]
fn the_existing_panel_fields_serialize_byte_for_byte_as_before() {
    let swap = router_with(Arc::new(DropsMetadata));
    let (_app, status) = status_over(&swap);
    let state = swap.load();
    let path = WirePath::root().child("metadata");
    state.wire_check_state_for_tests().record_compared(
        Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi),
        Leg::Request,
        [(&path, PathClass::Missing)],
        false,
    );
    let panel = panel_of(&status);

    let text = serde_json::to_string(&panel).expect("panel serializes");
    let before = serde_json::to_string(&PanelWithoutWireCheck {
        targets: &panel.targets,
        learned_negatives: &panel.learned_negatives,
    })
    .expect("legacy shape serializes");
    let wire_check = serde_json::to_string(&panel.wire_check).expect("rows serialize");

    let legacy_prefix = before.strip_suffix('}').expect("an object");
    assert_eq!(
        text,
        format!("{legacy_prefix},\"wire_check\":{wire_check}}}")
    );
    assert!(
        !panel.targets.is_empty(),
        "the comparison covers a target row"
    );
}

#[test]
fn building_the_panel_changes_neither_the_wire_check_sampler_nor_its_counters() {
    let swap = router_with(Arc::new(DropsMetadata));
    let (_app, status) = status_over(&swap);
    let router = swap.load();
    let wire_check = router.wire_check_state_for_tests();
    let config: Config =
        toml::from_str("version = 5\n[wire_check]\nfirst_n = 2\nsample_one_in = 1000\n")
            .expect("wire_check config parses");
    let lane = Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi);
    assert!(wire_check.admit(lane, None, &config.wire_check));
    wire_check.record_local_refusal(lane, routectl_core::RefusalReason::CloakRelocation);
    let before = router.wire_check_snapshot();

    for _ in 0..5 {
        panel_of(&status);
    }

    assert_eq!(router.wire_check_snapshot(), before);
    assert!(
        wire_check.admit(lane, None, &config.wire_check),
        "the second success is still inside first_n, so no read advanced the sampler"
    );
    assert!(!wire_check.admit(lane, None, &config.wire_check));
}
