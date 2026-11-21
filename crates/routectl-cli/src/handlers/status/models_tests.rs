//! `/status/models`: one row per learned lane, the lane key against the
//! ledger row a served request writes, refusals as the shared resolver
//! resolves them, transit bounds, one cause per passthrough hop, the closed
//! state tokens, and that the builder reads no ledger and runs no doctor
//! gather.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use routectl_core::capability::{FailurePhase, Verdict};
use routectl_core::wire_check::WirePath;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, EligibilityReason, Error, ForwardedBearer,
    Message, MessageContent, Provider, Role, Usage,
};
use routectl_router::config::Auth;
use routectl_router::wire_check_state::{
    CheckOutcome, TRANSIT_LANE_CAP, TRANSIT_PATH_CAP, WireCheckState,
};
use routectl_router::{
    BetaSeedScope, CURRENT_CONFIG_VERSION, Config, OverrideRegistry, ProviderEntry, ResolvedModel,
    Router, StateKey,
};
use routectl_usage::{CHANNEL_CAPACITY, Outcome, UsageWriter};
use serde_json::Value;
use tower::ServiceExt;

use super::fixture::{learned_entry, populated_models, resolved_lanes};
use super::*;
use crate::handlers::status::panel_fixtures::{FIXTURE_NOW_MS, fixture_config, fixture_targets};
use crate::handlers::status::production_source::production_source;
use crate::handlers::status::{DaemonMeta, status_router};
use crate::handlers::usage_capture::{UsageCapture, build_usage_draft};
use crate::lane_cell::{CellSources, LearnedSnapshot, lane_metas, resolve_cell};
use crate::server::AppState;

// ------------------------------------------------------------------ rigs

/// Answers every completion with a one-token reply.
struct Answers;

#[async_trait::async_trait]
impl Provider for Answers {
    fn id(&self) -> &'static str {
        "answers"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        Err(Error::normalize_response("answers", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> routectl_core::Result<ChatResponse> {
        Ok(ChatResponse {
            model: req.model,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: message("ok"),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 1,
                total_tokens: 4,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<BoxStream<'static, routectl_core::Result<ChatChunk>>> {
        Err(Error::upstream("answers", 500, "unused"))
    }
}

/// Fails every request with a server error, never dialing anything.
struct ServerError;

#[async_trait::async_trait]
impl Provider for ServerError {
    fn id(&self) -> &'static str {
        "hop"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        Err(Error::normalize_response("hop", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> routectl_core::Result<ChatResponse> {
        Err(Error::upstream("hop", 500, "unused"))
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<BoxStream<'static, routectl_core::Result<ChatChunk>>> {
        Err(Error::upstream("hop", 500, "unused"))
    }
}

/// Resolves every reference, `oauth://` included, to a fixed test bearer,
/// so a pool of login seats builds without a login.
struct TestSecrets;

#[async_trait::async_trait]
impl routectl_auth::SecretStore for TestSecrets {
    async fn get(&self, _: &routectl_auth::SecretRef) -> routectl_core::Result<String> {
        Ok("test-bearer".to_string())
    }
    async fn set(&self, _: &routectl_auth::SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &routectl_auth::SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
}

fn message(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
        extras: serde_json::Map::new(),
    }
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        max_tokens: Some(16),
        messages: vec![message("hi")].into(),
        ..Default::default()
    }
}

fn status_state(router: Router) -> Arc<StatusState> {
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()))
}

async fn get(state: &Arc<StatusState>, path: &str) -> Value {
    let resp = status_router()
        .with_state(Arc::clone(state))
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{path}");
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).expect("json body");
    assert!(json["unavailable"].is_null(), "{path}: {json}");
    json
}

fn rows(json: &Value) -> &Vec<Value> {
    json["data"]["rows"].as_array().expect("rows array")
}

fn row<'j>(json: &'j Value, lane: &str) -> &'j Value {
    rows(json)
        .iter()
        .find(|row| row["lane"] == lane)
        .unwrap_or_else(|| panic!("no row {lane}: {json:#}"))
}

fn lanes_of(json: &Value) -> Vec<&str> {
    rows(json)
        .iter()
        .map(|row| row["lane"].as_str().expect("lane"))
        .collect()
}

// ------------------------------------------------------------ one row per lane

const ANTHROPIC_SEAT: &str = "format = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\nauth = \"bearer\"\n";

/// A two-member login pool behind `pooled`, and two nicknames on one entry
/// and upstream (`alpha`, `beta`) beside a third on another upstream.
fn lanes_config() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat-a]\n{ANTHROPIC_SEAT}credential = \"oauth://anthropic#seat-a\"\n\
         [providers.seat-b]\n{ANTHROPIC_SEAT}credential = \"oauth://anthropic#seat-b\"\n\
         [providers.api]\nformat = \"openai-chat\"\nbase_url = \"https://api.example.invalid/v1\"\n\
         credential = \"env://MODELS_LANE_TEST_KEY\"\nauth = \"bearer\"\n\
         [pools.the-pool]\nmembers = [\"seat-a\", \"seat-b\"]\n\
         [models.pooled]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n\
         [models.alpha]\nprovider = \"api\"\nupstream = \"shared\"\n\
         [models.beta]\nprovider = \"api\"\nupstream = \"shared\"\n\
         [models.gamma]\nprovider = \"api\"\nupstream = \"other\"\n"
    )
}

#[tokio::test]
async fn one_row_per_lane_joins_nicknames_and_splits_pool_members() {
    // Arrange
    let config = routectl_router::parse_config(&lanes_config()).expect("fixture parses");
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(TestSecrets);
    let router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("router builds");
    assert_eq!(
        router.status_targets(Instant::now()).len(),
        5,
        "premise: two seat targets and three nickname targets"
    );
    let state = status_state(router);

    // Act
    let json = get(&state, "/status/models").await;

    // Assert
    let mut lanes = lanes_of(&json);
    lanes.sort_unstable();
    assert_eq!(
        lanes,
        [
            "api#other",
            "api#shared",
            "seat-a#claude-sonnet-4-6",
            "seat-b#claude-sonnet-4-6",
        ]
    );
    assert_eq!(
        row(&json, "api#shared")["nicknames"],
        serde_json::json!(["alpha", "beta"])
    );
    for (lane, entry, label) in [
        ("seat-a#claude-sonnet-4-6", "seat-a", "seat-a"),
        ("seat-b#claude-sonnet-4-6", "seat-b", "seat-b"),
    ] {
        let seat = row(&json, lane);
        assert_eq!(seat["nicknames"], serde_json::json!(["pooled"]), "{lane}");
        assert_eq!(seat["provider"], entry, "{lane}");
        assert_eq!(seat["upstream"], "claude-sonnet-4-6", "{lane}");
        assert_eq!(seat["account"], format!("anthropic#{label}"), "{lane}");
        assert_eq!(seat["login_label"], label, "{lane}");
    }
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
}

/// Two nicknames share one lane but keep their own breakers: the lane reads
/// the worse target's state and the item that names it, whichever nickname
/// sorts first.
#[tokio::test]
async fn a_shared_lane_reads_its_worst_targets_state_and_cause() {
    // Arrange
    let config = routectl_router::parse_config(&lanes_config()).expect("fixture parses");
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(TestSecrets);
    let router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("router builds");
    assert!(router.force_open_breaker("beta", std::time::Duration::from_hours(1)));
    let state = status_state(router);

    // Act
    let json = get(&state, "/status/models").await;

    // Assert
    let shared = row(&json, "api#shared");
    assert_eq!(shared["state"], "breaker_open", "{shared}");
    assert_eq!(shared["cause_key"], "route_circuit_open:beta", "{shared}");
    assert!(shared["since_ms"].is_i64(), "{shared}");
    assert_eq!(row(&json, "api#other")["state"], "ok");
    assert_eq!(lanes_of(&json)[0], "api#shared", "the problem row leads");
}

// --------------------------------------------- lane key against the ledger row

/// `own` serves `own-upstream` on its own credential; `fwd` forwards the
/// client's credential and is configured with `fwd-upstream`.
fn ledger_router() -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "own-entry".into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.providers.insert(
        "fwd-entry".into(),
        ProviderEntry::anthropic_api("forwarded").with_auth(Auth::Bearer),
    );
    let provider: Arc<dyn Provider> = Arc::new(Answers);
    let models: BTreeMap<String, Arc<ResolvedModel>> = [
        ResolvedModel::new("own", "own-entry", Arc::clone(&provider), "own-upstream"),
        ResolvedModel::new("fwd", "fwd-entry", provider, "fwd-upstream"),
    ]
    .into_iter()
    .map(|model| (model.nickname.clone(), Arc::new(model)))
    .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

/// Serve `req` through `router` and persist its row through the capture
/// path the non-streaming handler runs; return the row's `(provider,
/// upstream)` read back from the ledger.
async fn served_ledger_identity(router: &Router, req: ChatRequest) -> (String, String) {
    let dir = tempfile::tempdir().expect("ledger dir");
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0);
    let dispatched = router
        .complete_with_options(req.clone(), Default::default())
        .await;
    let resp = dispatched.result.expect("the stub upstream answers");
    let draft = build_usage_draft(
        "anthropic",
        &req,
        "models-ledger-row".to_string(),
        routectl_usage::Harness::Other,
    );
    let mut capture = UsageCapture::new(
        draft,
        handle,
        "ingress-1".to_string(),
        routectl_core::RequestObservation::new(),
    );
    capture.observe_meta(&dispatched.meta, 0, 0);
    capture.observe_response(&resp);
    capture.finalize(Outcome::Ok);
    drop(capture);
    writer.shutdown();
    let db = routectl_usage::open_readonly(&db_path).expect("open ledger");
    db.conn()
        .query_row(
            "SELECT provider, upstream FROM requests WHERE request_id = 'models-ledger-row'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the row landed")
}

#[tokio::test]
async fn an_own_credential_lane_is_the_ledger_rows_provider_and_upstream() {
    // Arrange
    let router = ledger_router();
    let ledger = served_ledger_identity(&router, request("own")).await;
    let state = status_state(router);

    // Act
    let json = get(&state, "/status/models").await;

    // Assert
    let own = row(&json, "own-entry#own-upstream");
    assert_eq!(
        (own["provider"].as_str(), own["upstream"].as_str()),
        (Some(ledger.0.as_str()), Some(ledger.1.as_str()))
    );
}

/// The documented exception: a forwarded lane's ledger row records the
/// client's requested model, so its `upstream` is not the lane's.
#[tokio::test]
async fn a_forwarded_lane_records_the_clients_model_not_the_lanes_upstream() {
    // Arrange
    let router = ledger_router();
    let mut req = request("fwd");
    req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("sk-test".to_string()));
    let ledger = served_ledger_identity(&router, req).await;
    let state = status_state(router);

    // Act
    let json = get(&state, "/status/models").await;

    // Assert
    let fwd = row(&json, "fwd-entry#fwd-upstream");
    assert_eq!(fwd["provider"], ledger.0.as_str());
    assert_eq!(fwd["upstream"], "fwd-upstream");
    assert_eq!(ledger.1, "fwd", "the ledger keeps the client's model");
    assert!(fwd["account"].is_null(), "a forwarded lane owns no account");
}

// ------------------------------------------------------------------ refusals

/// Every row's refusals and capability cells equal what `resolve_cell`
/// resolves for the lane, over learned negatives one of which an override
/// outranks: the outranked negative is no refusal, the other is.
#[test]
fn refusals_are_the_cells_the_shared_resolver_shows_as_learned_unsupported() {
    // Arrange
    let now = Instant::now();
    let config = fixture_config();
    let entries = vec![
        learned_entry(
            "anthropic#claude-sonnet-4-5",
            "structured_output".to_string(),
            Verdict::LearnedBroken(FailurePhase::F1),
            now,
        ),
        learned_entry(
            "compat#glm-5",
            "web_search".to_string(),
            Verdict::LearnedBroken(FailurePhase::F1),
            now,
        ),
    ];

    // Act
    let panel = models_panel(ModelInputs {
        now_ms: FIXTURE_NOW_MS,
        targets: fixture_targets(),
        credentials: BTreeMap::new(),
        lanes: resolved_lanes(&config, &entries, now),
        transit: WireCheckState::default().transit_snapshot(),
    });
    let json = serde_json::to_value(&panel).expect("panel serializes");

    // Assert
    let refusals = |lane: &str| -> Vec<&str> {
        json["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["lane"] == lane)
            .expect("lane row")["refusals"]
            .as_array()
            .expect("refusals")
            .iter()
            .map(|r| r["capability"].as_str().expect("capability"))
            .collect()
    };
    assert_eq!(refusals("compat#glm-5"), ["web_search"]);
    assert!(
        refusals("anthropic#claude-sonnet-4-5").is_empty(),
        "an override outranks the learned negative"
    );
    let overrides = OverrideRegistry::build(&config);
    let sources = CellSources {
        config: &config,
        overrides: &overrides,
        learned: LearnedSnapshot {
            entries: &entries,
            now: Some((now, FIXTURE_NOW_MS)),
            seed_clears: &[],
        },
        beta_seed: BetaSeedScope::EMPTY,
    };
    for meta in lane_metas(&config, &entries) {
        let Some(row) = json["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["lane"] == meta.lane.as_str())
        else {
            continue;
        };
        for cell in row["capabilities"].as_array().expect("capabilities") {
            let capability = cell["capability"].as_str().expect("capability");
            let resolved = resolve_cell(&meta, capability, &sources);
            let shown = resolved.shown().expect("the fixture's nicknames agree");
            assert_eq!(cell["verdict"], shown.verdict, "{} {capability}", meta.lane);
            assert_eq!(cell["layer"].as_str(), resolved.layer(), "{capability}");
            assert_eq!(cell["action"].as_str(), resolved.action(), "{capability}");
            let is_refusal = resolved.layer() == Some("learned") && shown.supported == Some(false);
            let listed = row["refusals"]
                .as_array()
                .expect("refusals")
                .iter()
                .find(|r| r["capability"] == capability);
            assert_eq!(listed.is_some(), is_refusal, "{} {capability}", meta.lane);
            if let Some(listed) = listed {
                assert_eq!(listed["verdict"], shown.verdict);
                assert_eq!(listed["action"].as_str(), resolved.action());
            }
        }
    }
}

// ------------------------------------------------------------------- transit

/// A lane the wire check never sampled, and one first seen after the tally
/// named its lane cap, both read `checked == 0` with no paths; a sampled
/// lane's paths stop at the cap and one `other` entry sums the rest.
#[test]
fn transit_reads_unmeasured_for_an_unsampled_lane_and_bounds_its_paths() {
    // Arrange
    let state = WireCheckState::default();
    for i in 0..TRANSIT_LANE_CAP {
        let filler = StateKey::parse(&format!("filler-{i:03}#u")).expect("lane key");
        state.record_transit(Some(&filler), CheckOutcome::Clean, std::iter::empty());
    }
    let sonnet = StateKey::parse("anthropic#claude-sonnet-4-5").expect("lane key");
    let glm = StateKey::parse("compat#glm-5").expect("lane key");
    let paths: Vec<WirePath> = (0..TRANSIT_PATH_CAP + 3)
        .map(|i| WirePath::from_segments([format!("f{i:02}").as_str()]))
        .collect();
    state.record_transit(Some(&glm), CheckOutcome::Loss, paths.iter());
    let snapshot = state.transit_snapshot();
    assert!(
        snapshot.lanes.iter().any(|row| row.lane
            == routectl_router::wire_check_state::TransitLane::Other
            && row.checked == 1),
        "premise: the glm lane counted only in the overflow bucket"
    );
    let named = WireCheckState::default();
    named.record_transit(Some(&sonnet), CheckOutcome::Loss, paths.iter());
    let mut transit = named.transit_snapshot();
    transit.lanes.extend(snapshot.lanes);

    // Act
    let panel = models_panel(ModelInputs {
        now_ms: FIXTURE_NOW_MS,
        targets: fixture_targets(),
        credentials: BTreeMap::new(),
        lanes: Vec::new(),
        transit,
    });
    let json = serde_json::to_value(&panel).expect("panel serializes");
    let find = |lane: &str| {
        json["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["lane"] == lane)
            .expect("lane row")
            .clone()
    };

    // Assert
    for lane in ["claude-seat#claude-sonnet-4-5", "compat#glm-5"] {
        assert_eq!(
            find(lane)["transit"],
            serde_json::json!({"checked": 0, "changed": 0, "top_paths": []}),
            "{lane}"
        );
    }
    let sampled = find("anthropic#claude-sonnet-4-5")["transit"].clone();
    assert_eq!(
        (sampled["checked"].as_u64(), sampled["changed"].as_u64()),
        (Some(1), Some(1))
    );
    let top = sampled["top_paths"].as_array().expect("top paths");
    assert_eq!(top.len(), TRANSIT_PATH_CAP + 1, "{top:?}");
    assert!(
        top[..TRANSIT_PATH_CAP]
            .iter()
            .all(|p| p["path"] != OTHER_PATHS)
    );
    assert_eq!(
        top[TRANSIT_PATH_CAP],
        serde_json::json!({"path": OTHER_PATHS, "count": 3})
    );
}

// --------------------------------------------------------------- one cause

/// Three nicknames on one provider entry, each its own upstream, all
/// failing with a server error after one attempt.
fn hop_router() -> Router {
    let config: Config = toml::from_str(&format!(
        "version = {CURRENT_CONFIG_VERSION}\n[retry]\nmax_attempts = 1\n\n\
         [providers.hop]\nformat = \"openai-chat\"\nbase_url = \"https://hop.example.invalid/v1\"\n\
         credential = \"env://MODELS_HOP_TEST_KEY\"\nauth = \"bearer\"\ncircuit_failures = 1\n\n\
         [providers.calm]\nformat = \"openai-chat\"\nbase_url = \"https://calm.example.invalid/v1\"\n\
         credential = \"env://MODELS_CALM_TEST_KEY\"\nauth = \"bearer\"\n"
    ))
    .expect("fixture config parses");
    let failing: Arc<dyn Provider> = Arc::new(ServerError);
    let answering: Arc<dyn Provider> = Arc::new(Answers);
    let models: BTreeMap<String, Arc<ResolvedModel>> = [
        ResolvedModel::new("hop-a", "hop", Arc::clone(&failing), "up-a"),
        ResolvedModel::new("hop-b", "hop", Arc::clone(&failing), "up-b"),
        ResolvedModel::new("hop-c", "hop", failing, "up-c"),
        ResolvedModel::new("calm", "calm", answering, "up-calm"),
    ]
    .into_iter()
    .map(|model| (model.nickname.clone(), Arc::new(model)))
    .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn every_passthrough_hop_row_carries_the_attention_items_key() {
    // Arrange
    let router = hop_router();
    for nickname in ["hop-a", "hop-b", "hop-c"] {
        let dispatched = router
            .complete_with_options(request(nickname), Default::default())
            .await;
        assert!(dispatched.result.is_err(), "premise: {nickname} fails");
    }
    let state = status_state(router);

    // Act
    let models = get(&state, "/status/models").await;
    let attention = get(&state, "/status/attention").await;

    // Assert
    let folds: Vec<&Value> = attention["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["kind"] == "provider_failing")
        .collect();
    assert_eq!(folds.len(), 1, "{attention:#}");
    let key = folds[0]["key"].as_str().expect("key");
    for lane in ["hop#up-a", "hop#up-b", "hop#up-c"] {
        let hop = row(&models, lane);
        assert_eq!(hop["cause_key"], key, "{lane}");
        assert_eq!(hop["state"], "breaker_open", "{lane}");
    }
    let calm = row(&models, "calm#up-calm");
    assert!(calm["cause_key"].is_null(), "{calm}");
    assert_eq!(calm["state"], "ok");
    assert_eq!(
        lanes_of(&models),
        ["hop#up-a", "hop#up-b", "hop#up-c", "calm#up-calm"],
        "problems first, then by lane"
    );
}

// -------------------------------------------------------------- the contract

/// The closed `state` tokens. Each refusal state spells its eligibility
/// reason's token, and every gate reason maps onto a listed state.
#[test]
fn the_state_tokens_are_closed_and_spell_the_eligibility_reasons() {
    let tokens: Vec<&str> = ModelState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        tokens,
        [
            "ok",
            "credential_unusable",
            "account_parked",
            "breaker_open",
            "rate_limited",
        ]
    );
    for reason in EligibilityReason::ALL {
        let state = ModelState::of(Some(reason));
        assert!(ModelState::ALL.contains(&state), "{reason:?}");
        if reason.is_request_only() {
            assert_eq!(state, ModelState::Ok, "{reason:?}");
        } else {
            assert_eq!(state.as_str(), reason.as_str(), "{reason:?}");
        }
    }
    assert_eq!(ModelState::of(None), ModelState::Ok);
}

/// What a read of the usage ledger or a doctor gather has to spell: the
/// ledger path, the usage crate, the doctor inputs and gather entry points
/// and the doctor panel's own report read. Kept in pieces so this test's
/// own text never matches.
fn ledger_and_gather_tokens() -> [String; 6] {
    [
        format!("{}{}", "usage_db", "_path"),
        format!("{}{}", "routectl", "_usage"),
        format!("{}{}", "served_doctor", "_inputs"),
        format!("{}{}", "gather_context", "_no_network"),
        format!("{}{}", "report_fail", "_findings"),
        format!("{}{}", "open_read", "only"),
    ]
}

/// `build_from_view` takes the router view and the pinned clock only, and no
/// production line of models.rs spells a ledger or doctor-gather read.
/// Control: every token is found in doctor.rs, which does both.
#[test]
fn the_builder_reads_no_ledger_and_runs_no_doctor_gather() {
    // Arrange
    let models = production_source(include_str!("models.rs"));
    let doctor = production_source(include_str!("doctor.rs"));
    let _signature: fn(&StatusRouterView, (Instant, i64)) -> ModelsPanel = build_from_view;

    // Act / Assert
    for token in ledger_and_gather_tokens() {
        assert!(!models.contains(&token), "models.rs spells `{token}`");
        assert!(
            doctor.contains(&token),
            "control: doctor.rs spells `{token}`"
        );
    }
}

#[test]
fn the_populated_fixture_carries_every_row_field_and_the_start_stamp() {
    let json = populated_models();
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
    assert_eq!(
        json["data"]["started_ms"],
        super::fixture::FIXTURE_STARTED_MS
    );
    let mut want = [
        "lane",
        "provider",
        "upstream",
        "nicknames",
        "account",
        "login_label",
        "state",
        "since_ms",
        "cause_key",
        "refusals",
        "capabilities",
        "transit",
    ];
    want.sort_unstable();
    for row in rows(&json) {
        let keys: Vec<&str> = row
            .as_object()
            .expect("a row")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, want, "{row}");
    }
    assert_eq!(
        lanes_of(&json),
        [
            "claude-seat#claude-sonnet-4-5",
            "compat#glm-5",
            "anthropic#claude-sonnet-4-5",
        ],
        "problem rows first"
    );
}

// ------------------------------------------------------------- the aggregate

/// The envelope with its read stamp removed: the one field two reads of the
/// same state may differ in.
fn without_as_of(mut envelope: Value) -> Value {
    envelope
        .as_object_mut()
        .expect("an envelope object")
        .remove("as_of");
    envelope
}

/// One daemon state read through `panels.models` on `/status` and then
/// through `/status/models` serializes to the same bytes, read stamp aside:
/// a parked seat (a problem row with a cause), a sampled lane's transit and
/// two nicknames on one lane. The state carries no clock-derived field but
/// `as_of`, so the comparison needs no other tolerance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_aggregate_member_serializes_the_standalone_payload() {
    // Arrange
    let config = routectl_router::parse_config(&lanes_config()).expect("fixture parses");
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(TestSecrets);
    let router = crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("router builds");
    let seat_a = router
        .status_targets(Instant::now())
        .into_iter()
        .find(|target| target.provider_name == "seat-a")
        .expect("premise: a seat-a target")
        .state_key;
    assert!(router.park_credential_for_tests(
        &seat_a,
        routectl_core::UpstreamReason::CredentialInvalid,
        Instant::now() + std::time::Duration::from_hours(1),
    ));
    let shared = StateKey::parse("api#shared").expect("lane key");
    let path = WirePath::from_segments(["choices"]);
    router.wire_check_state_for_tests().record_transit(
        Some(&shared),
        CheckOutcome::Loss,
        std::iter::once(&path),
    );
    let state = status_state(router);

    // Act
    let aggregate = get(&state, "/status").await;
    let standalone = get(&state, "/status/models").await;

    // Assert
    let member = without_as_of(aggregate["panels"]["models"].clone());
    let route = without_as_of(standalone);
    let parked = row(&member, "seat-a#claude-sonnet-4-6");
    assert_eq!(parked["state"], "account_parked", "{member:#}");
    assert!(parked["cause_key"].is_string(), "{parked}");
    assert_eq!(row(&member, "api#shared")["transit"]["changed"], 1);
    assert_eq!(member["schema_version"], SCHEMA_VERSION);
    assert_eq!(
        serde_json::to_string(&member).expect("member serializes"),
        serde_json::to_string(&route).expect("route serializes"),
    );
}
