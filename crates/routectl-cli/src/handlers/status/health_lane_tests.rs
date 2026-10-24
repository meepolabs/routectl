//! The health panel's join key, end to end: a negative learned through real
//! dispatch serializes under the same lane every target on that lane carries
//! as `learned_lane`, so a client joining on it finds the row from each
//! nickname's card. The row's resolved cell agrees with the served doctor
//! matrix on the same router snapshot.

use super::*;

use std::collections::BTreeMap;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Result, Role,
    ToolDef,
};
use routectl_router::{Config, ResolvedModel, Router, RouterOptions};
use serde_json::{Value, json};

use crate::handlers::status::DaemonMeta;
use crate::server::AppState;

const PROVIDER: &str = "p";
const UPSTREAM: &str = "shared-upstream";
const LANE: &str = "p#shared-upstream";
const NICKNAMES: [&str; 2] = ["alpha", "beta"];

/// Rejects every request with an openai-compat 400 naming `web_search`, the
/// shape the classifier lifts to a self-identifying feature rejection.
struct RejectsWebSearch;

#[async_trait::async_trait]
impl Provider for RejectsWebSearch {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        let body = json!({
            "error": {
                "type": "invalid_request_error",
                "code": "unsupported_parameter",
                "param": "web_search",
                "message": "Unsupported parameter."
            }
        });
        Err(Error::upstream_full(
            "p",
            400,
            body.to_string(),
            None,
            Some("invalid_request_error".to_string()),
            Some("unsupported_parameter".to_string()),
        ))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

/// Two nicknames on one upstream of one provider entry: one lane, two
/// runtime targets.
fn router() -> Router {
    let config: Config = toml::from_str(&format!(
        "version = 3\n\
         [providers.{PROVIDER}]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid/v1\"\n\
         api_key_ref = \"env://ROUTECTL_HEALTH_LANE_TEST_KEY\"\n\
         [capability]\n\
         enabled = true\n"
    ))
    .expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(RejectsWebSearch);
    let models: BTreeMap<String, Arc<ResolvedModel>> = NICKNAMES
        .iter()
        .map(|nickname| {
            (
                (*nickname).to_string(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    PROVIDER,
                    Arc::clone(&provider),
                    UPSTREAM,
                )),
            )
        })
        .collect();
    router.install_resolved_models(models);
    router
}

fn web_search_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        tools: Some(vec![ToolDef::Other(
            json!({ "type": "web_search", "name": "t" }),
        )]),
        ..Default::default()
    }
}

#[tokio::test]
async fn each_target_learned_lane_is_the_state_key_of_the_row_learned_on_it() {
    // Arrange: learn a negative through real dispatch on one nickname.
    let router = router();
    let dispatched = router
        .complete_with_options(web_search_request("alpha"), RouterOptions::default())
        .await;
    assert!(dispatched.result.is_err(), "the only target rejects");
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = StatusState::from_app(&app, None, DaemonMeta::for_test());

    // Act
    let panel = build_from_view(
        &state.router.view(),
        false,
        FidelityEmission::always(),
        &FidelityGate::default(),
    );
    let wire = serde_json::to_value(&panel).expect("panel serializes");

    // Assert
    let rows = wire["learned_negatives"]
        .as_array()
        .expect("learned rows array");
    assert_eq!(rows.len(), 1, "one negative learned: {rows:?}");
    assert_eq!(rows[0]["verdict"], "broken");
    assert_eq!(rows[0]["state_key"], LANE);
    assert_eq!(rows[0]["layer"], "learned");
    assert_eq!(rows[0]["action"], "route_away");
    assert!(rows[0]["age_ms"].is_i64(), "{rows:?}");
    assert!(
        rows[0]["expires_at_ms"].is_i64(),
        "a negative carries its lapse deadline: {rows:?}"
    );
    let targets = wire["targets"].as_array().expect("targets array");
    let mut nicknames: Vec<&str> = targets
        .iter()
        .map(|t| t["nickname"].as_str().expect("nickname"))
        .collect();
    nicknames.sort_unstable();
    assert_eq!(nicknames, NICKNAMES);
    for target in targets {
        assert_eq!(
            target["learned_lane"], rows[0]["state_key"],
            "target {} joins its lane's row",
            target["nickname"]
        );
        assert_ne!(
            target["state_key"], rows[0]["state_key"],
            "the runtime key never matches a lane row"
        );
    }
}

mod doctor_agreement {
    use std::time::Instant;

    use arc_swap::ArcSwap;
    use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
    use routectl_router::{CapabilityMatrixPanel, GenerationOutcome, Router, beta_capability_key};
    use serde_json::Value;

    use super::super::*;
    use crate::commands::doctor::{
        GatherSources, build_report_no_network, gather_context_no_network,
    };
    use crate::handlers::status::router_view::StatusRouterHandle;
    use crate::server::capability_rebuild::WarmReport;

    const PINNED_FLAG: &str = "fx-pinned";
    const CLIENT_FLAG: &str = "fx-client";
    const SEEDED_FLAG: &str = "advanced-tool-use-2025-11-20";
    const FEATURE: &str = "web_search";
    const BEDROCK_LANE: &str = "bed#anthropic.claude-test-v1:0";
    const CONVERSE_LANE: &str = "cv#us.amazon.nova-2-lite-v1:0";
    const KIND: &str = "bedrock";

    /// A Bedrock Messages entry whose `anthropic_beta` floor pins
    /// [`PINNED_FLAG`] and whose nickname force-supports [`FEATURE`], and a
    /// Converse entry serving a non-Claude model. Neither upstream is in the
    /// catalog, so the doctor's priors hold no opinion the status surface
    /// lacks.
    fn config_toml(db_path: &std::path::Path) -> String {
        format!(
            "version = {}\n\
             [usage]\n\
             db_path = \"{}\"\n\
             [providers.bed]\n\
             kind = \"bedrock\"\n\
             region = \"us-east-1\"\n\
             creds = {{ kind = \"default-chain\" }}\n\
             anthropic_beta = [\"{PINNED_FLAG}\"]\n\
             [providers.cv]\n\
             kind = \"bedrock\"\n\
             region = \"us-east-1\"\n\
             creds = {{ kind = \"default-chain\" }}\n\
             api_shape = \"converse\"\n\
             [models.claude]\n\
             provider = \"bed\"\n\
             upstream = \"anthropic.claude-test-v1:0\"\n\
             [models.nova]\n\
             provider = \"cv\"\n\
             upstream = \"us.amazon.nova-2-lite-v1:0\"\n\
             [capability.overrides.\"bed:claude\"]\n\
             force_supported = [\"{FEATURE}\"]\n",
            routectl_router::CURRENT_CONFIG_VERSION,
            db_path.display(),
        )
    }

    fn beta(flag: &str) -> String {
        beta_capability_key(flag).expect("a well-formed flag")
    }

    fn plant_negative(router: &Router, lane: &str, key: &str, now: Instant) {
        let outcome = router.observe_learned_capability(
            lane,
            key,
            KIND,
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        );
        assert!(
            matches!(outcome, GenerationOutcome::Applied { .. }),
            "premise: the negative on {lane} {key} is stored",
        );
    }

    fn plant_positive(router: &Router, lane: &str, key: &str, now: Instant) {
        let outcome =
            router.observe_verified_capability(lane, key, KIND, EvidenceSource::Live, None, now);
        assert!(
            matches!(outcome, GenerationOutcome::Applied { .. }),
            "premise: the positive on {lane} {key} is stored",
        );
    }

    /// The doctor matrix cell's `(layer, overrode_layer, action)`.
    fn doctor_cell(
        panel: &CapabilityMatrixPanel,
        lane: &str,
        key: &str,
    ) -> (Option<String>, Option<String>, String) {
        let column = panel
            .columns
            .iter()
            .position(|c| c == key)
            .unwrap_or_else(|| panic!("doctor column {key} missing"));
        let cell = &panel
            .lanes
            .iter()
            .find(|l| l.lane == lane)
            .unwrap_or_else(|| panic!("doctor lane {lane} missing"))
            .cells[column];
        (
            cell.layer.map(str::to_string),
            cell.overrode_layer.map(str::to_string),
            cell.action.to_string(),
        )
    }

    /// The status learned row's `(layer, overrode_layer, action)`.
    fn status_row(
        rows: &[Value],
        lane: &str,
        key: &str,
    ) -> (Option<String>, Option<String>, String) {
        let row = rows
            .iter()
            .find(|r| r["state_key"] == lane && r["capability_key"] == key)
            .unwrap_or_else(|| panic!("status row {lane} {key} missing: {rows:?}"));
        let token = |field: &str| row[field].as_str().map(str::to_string);
        (
            token("layer"),
            token("overrode_layer"),
            token("action").expect("action is always a token"),
        )
    }

    /// Status and the served doctor read one router snapshot and show the same
    /// layer, beaten tier and action on every learned cell, including a pinned
    /// beta flag with an acting learned negative, which both show as the send
    /// dispatch takes.
    ///
    /// Mutation check: map the status `action` through its own token in
    /// `map_learned` (e.g. `route_away` for every acting negative) -> red at
    /// the pinned and seed rows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn status_and_the_served_doctor_agree_on_every_learned_cell() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        let absent_ledger = dir.path().join("absent").join("usage.db");
        std::fs::write(&config_path, config_toml(&absent_ledger)).unwrap();
        let config = crate::server::parse_config_only(&config_path).expect("fixture config parses");
        let router = Router::new(Arc::new(config));
        let now = Instant::now();
        let pinned = beta(PINNED_FLAG);
        let client = beta(CLIENT_FLAG);
        let seeded = beta(SEEDED_FLAG);
        plant_negative(&router, BEDROCK_LANE, &pinned, now);
        plant_negative(&router, BEDROCK_LANE, FEATURE, now);
        plant_negative(&router, BEDROCK_LANE, &client, now);
        plant_positive(&router, BEDROCK_LANE, &seeded, now);
        plant_positive(&router, CONVERSE_LANE, &client, now);
        let view = StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router))).view();

        // Act
        let served = gather_context_no_network(
            &config_path,
            GatherSources::Served(Box::new(view.served_doctor_inputs(WarmReport::not_run()))),
            &[],
        )
        .await;
        let doctor = build_report_no_network(&served)
            .panels
            .capability_matrix
            .expect("the served report carries the matrix");
        let status = serde_json::to_value(build_from_view(
            &view,
            false,
            FidelityEmission::always(),
            &FidelityGate::default(),
        ))
        .expect("panel serializes");
        let rows = status["learned_negatives"]
            .as_array()
            .expect("learned rows array");

        // Assert
        let s = |v: &str| Some(v.to_string());
        let expected = [
            (
                BEDROCK_LANE,
                pinned.as_str(),
                s("pin"),
                s("learned"),
                "allow",
            ),
            (BEDROCK_LANE, FEATURE, s("override"), s("learned"), "allow"),
            (
                BEDROCK_LANE,
                client.as_str(),
                s("learned"),
                None,
                "withhold",
            ),
            (
                BEDROCK_LANE,
                seeded.as_str(),
                s("learned"),
                s("seed"),
                "allow",
            ),
            (
                CONVERSE_LANE,
                client.as_str(),
                s("representability"),
                s("learned"),
                "withhold",
            ),
        ];
        assert_eq!(rows.len(), expected.len(), "one row per planted entry");
        for (lane, key, layer, overrode, action) in expected {
            let shown = status_row(rows, lane, key);
            assert_eq!(
                shown,
                doctor_cell(&doctor, lane, key),
                "status and doctor differ on {lane} {key}",
            );
            assert_eq!(shown, (layer, overrode, action.to_string()), "{lane} {key}");
        }
    }
}
