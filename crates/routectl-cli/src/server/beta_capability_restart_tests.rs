//! Cross-restart proof for the two durable beta-flag facts: a learned beta
//! negative and an operator seed lift.
//!
//! Each piece is pinned in isolation elsewhere: the router mints a beta
//! negative from a provider's repair report, the control route commits a seed
//! lift's `cleared` row, and the warm rebuild replays both. None of those pins
//! spans the seam from a live dispatch or a live purge, through the production
//! writer, to the decision a SECOND Router makes after reading the ledger back.
//! A fact that changes behavior only in memory passes every one of them and is
//! gone at the next boot.
//!
//! Like `canary_span_tests`, this drives a real Router rather than a spawned
//! daemon. A real Bedrock provider derives its endpoint from the region, so it
//! cannot be aimed at an in-process mock; the wire behavior (stripping a
//! withheld flag, recording a rejected one) is pinned in the providers crate.
//! The seat below stands in at exactly that seam: it records what the router
//! handed it and, when told to, reports a flag as stripped.
//!
//! Every assertion reads the `withheld_betas` a rebuilt Router hands its
//! provider -- what would reach the upstream, not what a registry snapshot
//! claims.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::routing::post;
use parking_lot::Mutex;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};
use routectl_router::{Config, ResolvedModel, Router, RouterOptions};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};
use tower::ServiceExt;

use crate::handlers::control::purge_capability;
use crate::handlers::usage_capture::drain_capability_events;
use crate::server::AppState;
use crate::server::capability_rebuild::warm_capability_registry_from_ledger;
use crate::server::test_support::{isolate_usage_db, overlay_at_revision};

const PURGE_PATH: &str = "/control/capability/purge";

/// The configured `bedrock` entry, its model nickname, and the upstream id the
/// nickname resolves to.
const PROVIDER: &str = "bed";
const NICKNAME: &str = "m";
const UPSTREAM: &str = "anthropic.claude-test-v1:0";

/// The learned lane [`NICKNAME`] dispatches on.
const LANE: &str = "bed#anthropic.claude-test-v1:0";

/// A flag no seed names: only a learned negative can withhold it.
const LEARNED_FLAG: &str = "zz-flag-a";

/// The flag the fixture seed withholds on a bedrock lane.
const SEEDED_FLAG: &str = "fx-seeded";

/// Installed through the test-utils setter on every Router here, so the
/// shipped seed's contents never decide an assertion.
const FIXTURE_SEED: &[&str] = &[SEEDED_FLAG];

/// An overlay revision other than the default zero every first session boots
/// at.
const BUMPED_REVISION: u64 = 7;

fn beta_key(flag: &str) -> String {
    routectl_router::beta_capability_key(flag).expect("fixture flags are well formed")
}

/// One provider call as the seat received it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenCall {
    client_betas: Vec<String>,
    withheld_betas: Vec<String>,
}

/// The stand-in for a Bedrock provider: records each call's client betas and
/// withheld set, and optionally reports one flag as stripped after an upstream
/// rejection, the way the real provider's repair does.
#[derive(Default)]
struct BetaSeat {
    reports_stripped: Option<&'static str>,
    seen: Mutex<Vec<SeenCall>>,
}

impl BetaSeat {
    fn only_call(&self) -> SeenCall {
        let seen = self.seen.lock();
        assert_eq!(seen.len(), 1, "exactly one attempt reached the provider");
        seen[0].clone()
    }
}

#[async_trait::async_trait]
impl Provider for BetaSeat {
    fn id(&self) -> &'static str {
        "beta-seat"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("beta-seat", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().push(SeenCall {
            client_betas: req.anthropic_beta.clone(),
            withheld_betas: req.routectl_internal.withheld_betas.to_vec(),
        });
        if let (Some(flag), Some(report)) = (
            self.reports_stripped,
            req.routectl_internal.beta_repair_report.as_ref(),
        ) {
            report.record(&[flag.to_string()]);
        }
        Ok(ChatResponse {
            model: UPSTREAM.to_string(),
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("beta-seat", 500, "unused"))
    }
}

/// One bedrock entry, its usage DB isolated to a tempdir the caller keeps
/// alive. Every session of a test boots from this same config.
fn fixture_config() -> (Arc<Config>, tempfile::TempDir) {
    let mut config: Config = toml::from_str(&format!(
        "[providers.{PROVIDER}]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\n",
    ))
    .expect("fixture config parses");
    let dir = isolate_usage_db(&mut config);
    (Arc::new(config), dir)
}

/// A Router as one daemon session builds it: the fixture seed, the overlay
/// revision this session boots at, and `seat` resolved for [`NICKNAME`].
fn boot(config: &Arc<Config>, overlay_revision: u64, seat: Arc<BetaSeat>) -> Router {
    let mut router = Router::new(Arc::clone(config));
    router.set_beta_seed_for_tests(FIXTURE_SEED);
    if overlay_revision != 0 {
        router.install_catalog_overlay(overlay_at_revision(overlay_revision));
    }
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME,
            PROVIDER,
            seat as Arc<dyn Provider>,
            UPSTREAM,
        )),
    );
    router.install_resolved_models(models);
    router
}

fn start_writer(config: &Config) -> (UsageHandle, UsageWriter) {
    UsageWriter::start(config.usage.db_path.clone(), CHANNEL_CAPACITY, 0, true)
}

/// Stop the writer so every queued row is on disk. The handle goes first: a
/// live producer clone keeps the channel open and the drain would wait out its
/// deadline and abandon what is queued.
fn stop_writer(usage: UsageHandle, writer: UsageWriter) {
    drop(usage);
    writer.shutdown();
}

/// The boot warm, off the runtime thread: it commits its boundary through the
/// blocking acknowledged path, and Tokio panics if a worker blocks.
fn warm_off_runtime(config: &Config, router: &Router, usage: &UsageHandle) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| warm_capability_registry_from_ledger(&config.usage.db_path, router, usage))
            .join()
            .expect("warm thread");
    });
}

fn request_with(flag: &str) -> ChatRequest {
    ChatRequest {
        model: NICKNAME.to_string(),
        anthropic_beta: vec![flag.to_string()],
        ..Default::default()
    }
}

async fn dispatch(router: &Router, flag: &str) -> routectl_router::Dispatched {
    let dispatched =
        Box::pin(router.complete_with_options(request_with(flag), RouterOptions::new())).await;
    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result);
    dispatched
}

/// A fresh session over the ledger: start a writer, boot at `overlay_revision`,
/// warm from the ledger, dispatch one request carrying `flag`, stop the writer.
/// Returns the withheld set the provider was handed.
async fn withheld_after_restart(
    config: &Arc<Config>,
    overlay_revision: u64,
    flag: &str,
) -> Vec<String> {
    let (usage, writer) = start_writer(config);
    let seat = Arc::new(BetaSeat::default());
    let router = boot(config, overlay_revision, Arc::clone(&seat));
    warm_off_runtime(config, &router, &usage);
    let _ = dispatch(&router, flag).await;
    stop_writer(usage, writer);
    let call = seat.only_call();
    assert_eq!(
        call.client_betas,
        vec![flag.to_string()],
        "premise: the client sent the flag, so an empty withheld set means it was forwarded",
    );
    call.withheld_betas
}

/// Purge the seeded flag's cell through the real control route, as the CLI
/// would. Consumes the router: the route holds the only handle afterwards.
async fn lift_seed_through_the_control_route(router: Router, usage: &UsageHandle) {
    let state =
        AppState::for_test_with_usage(Arc::new(ArcSwap::from_pointee(router)), usage.clone());
    let app = axum::Router::new()
        .route(PURGE_PATH, post(purge_capability))
        .with_state(state);
    let body = serde_json::json!({
        "state_key": LANE,
        "capability_key": beta_key(SEEDED_FLAG),
    })
    .to_string();
    let peer: std::net::SocketAddr = "127.0.0.1:54321".parse().expect("loopback peer");
    let request = HttpRequest::builder()
        .method("POST")
        .uri(PURGE_PATH)
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer))
        .body(Body::from(body))
        .expect("build request");

    let response = app.oneshot(request).await.expect("route responds");

    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(status, StatusCode::OK, "body was {json}");
    assert_eq!(json["seed_lifted"].as_bool(), Some(true), "body was {json}");
}

/// The first daemon session of the seed scenarios: boot, warm, show the seed
/// withholding the flag on a fresh Router, optionally lift it, stop.
async fn first_seed_session(config: &Arc<Config>, lift: bool) {
    let (usage, writer) = start_writer(config);
    let seat = Arc::new(BetaSeat::default());
    let router = boot(config, 0, Arc::clone(&seat));
    warm_off_runtime(config, &router, &usage);
    let _ = dispatch(&router, SEEDED_FLAG).await;
    assert_eq!(
        seat.only_call().withheld_betas,
        vec![SEEDED_FLAG.to_string()],
        "premise: the seed withholds the flag on a fresh Router",
    );
    if lift {
        lift_seed_through_the_control_route(router, &usage).await;
    }
    stop_writer(usage, writer);
}

// Every test here joins the default serial group: sibling tests in this crate
// deliver SIGTERM / SIGHUP to their own process, and a test in flight when one
// fires dies with the binary. See `canary_span_tests` for the measurement.

/// A beta the upstream rejected, learned through the production drain, is
/// withheld by a Router rebuilt from the ledger.
#[tokio::test]
#[serial_test::serial]
async fn a_learned_beta_negative_survives_a_restart() {
    // Arrange -- session 1 learns the flag from a reported strip.
    let (config, _dir) = fixture_config();
    let (usage, writer) = start_writer(&config);
    let seat = Arc::new(BetaSeat {
        reports_stripped: Some(LEARNED_FLAG),
        ..BetaSeat::default()
    });
    let router = boot(&config, 0, Arc::clone(&seat));
    warm_off_runtime(&config, &router, &usage);
    let dispatched = dispatch(&router, LEARNED_FLAG).await;
    assert!(
        seat.only_call().withheld_betas.is_empty(),
        "premise: nothing withholds the unseeded flag before it is learned",
    );
    let learned: Vec<&str> = dispatched
        .meta
        .learned_capabilities
        .iter()
        .map(|ev| ev.capability_key.as_str())
        .collect();
    assert_eq!(learned, vec![beta_key(LEARNED_FLAG).as_str()]);
    drain_capability_events(
        &usage,
        &dispatched.meta,
        router.catalog_version(),
        router.overlay_revision(),
    );
    stop_writer(usage, writer);

    // Act
    let withheld = withheld_after_restart(&config, 0, LEARNED_FLAG).await;

    // Assert
    assert_eq!(
        withheld,
        vec![LEARNED_FLAG.to_string()],
        "the learned negative was replayed from the ledger and withholds the flag",
    );
}

/// Control for the learned-beta restart: with nothing in the ledger, the same
/// restart forwards the flag, so the withholding above came from the ledger.
#[tokio::test]
#[serial_test::serial]
async fn a_restart_over_an_empty_ledger_forwards_the_unlearned_beta() {
    let (config, _dir) = fixture_config();

    let withheld = withheld_after_restart(&config, 0, LEARNED_FLAG).await;

    assert!(withheld.is_empty(), "withheld: {withheld:?}");
}

/// A seed lift committed by the control route is replayed at the next boot,
/// and the rebuilt Router forwards the seeded flag.
#[tokio::test]
#[serial_test::serial]
async fn a_seed_lift_survives_a_restart() {
    let (config, _dir) = fixture_config();
    first_seed_session(&config, true).await;

    let withheld = withheld_after_restart(&config, 0, SEEDED_FLAG).await;

    assert!(
        withheld.is_empty(),
        "the lift's cleared row restored the marker; withheld: {withheld:?}",
    );
}

/// Control for the seed-lift restart: without the purge, the rebuilt Router
/// still withholds the seeded flag.
#[tokio::test]
#[serial_test::serial]
async fn without_a_lift_a_restart_still_withholds_the_seeded_beta() {
    let (config, _dir) = fixture_config();
    first_seed_session(&config, false).await;

    let withheld = withheld_after_restart(&config, 0, SEEDED_FLAG).await;

    assert_eq!(withheld, vec![SEEDED_FLAG.to_string()]);
}

/// A seed lift is a catalog-scoped fact: a boot under a different overlay
/// revision drops its row on replay and the seed withholds the flag again.
#[tokio::test]
#[serial_test::serial]
async fn a_seed_lift_lapses_when_the_overlay_revision_changes() {
    // Arrange -- the lift is durable at the revision it was made under.
    let (config, _dir) = fixture_config();
    first_seed_session(&config, true).await;
    assert!(
        withheld_after_restart(&config, 0, SEEDED_FLAG)
            .await
            .is_empty(),
        "premise: the lift survives a same-revision restart",
    );

    // Act
    let withheld = withheld_after_restart(&config, BUMPED_REVISION, SEEDED_FLAG).await;

    // Assert
    assert_eq!(withheld, vec![SEEDED_FLAG.to_string()]);
}
