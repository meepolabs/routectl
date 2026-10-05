//! Positive traffic versus the bounded warm-rebuild window, end to end: real
//! non-streaming dispatches through a `Router`, the real capability drain, a
//! real `UsageWriter` persisting to a temp ledger, and the real startup warm
//! reading it back into a fresh `Router`.
//!
//! The rebuild reads only the newest `REBUILD_ROW_LIMIT` post-boundary rows,
//! so the ledger must grow with verdict transitions rather than with traffic:
//! otherwise steady verified traffic on one key pushes an older negative out
//! of the window and a restart silently forgets it.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use routectl_core::capability::{FailurePhase, STRUCTURED_OUTPUT, SignalTier, Verdict, WEB_SEARCH};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Provider, Result, test_utils,
};
use routectl_router::{Config, ResolvedModel, Router, RouterOptions};
use routectl_usage::{
    CHANNEL_CAPACITY, CapabilityEvent, UsageHandle, UsageWriter, open, read_capability_events_after,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::capability_rebuild::warm_capability_registry_from_ledger;
use super::ledger_reader::REBUILD_ROW_LIMIT;
use crate::handlers::usage_capture::drain_capability_events;

const DAYS: usize = 30;
const VERIFIED_REQUESTS_PER_DAY: usize = 200;
const LANE: &str = "m1";

/// A provider whose non-streaming arm returns a schema-conforming body, so
/// every dispatch reaches the success-arm observer with verified evidence.
struct SchemaConformingProvider;

#[async_trait::async_trait]
impl Provider for SchemaConformingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Ok(conforming_response())
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(conforming_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p1", 500, "unused"))
    }
}

fn conforming_response() -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message: test_utils::assistant_text_msg(r#"{"name":"ok"}"#),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        ..Default::default()
    }
}

/// A router with one `openai-compat` lane `m1` served by
/// [`SchemaConformingProvider`], the capability subsystem enabled.
fn router() -> Router {
    let toml_text = r#"
version = 3
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
[capability]
enabled = true
"#;
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(SchemaConformingProvider);
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        LANE.to_string(),
        Arc::new(ResolvedModel::new(LANE, "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

/// A strict structured-output request whose schema the canned body satisfies.
fn structured_output_request() -> ChatRequest {
    ChatRequest {
        model: LANE.into(),
        messages: vec![].into(),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": { "type": "object", "required": ["name"] }
                }
            }
        })),
        ..Default::default()
    }
}

fn revision_of(router: &Router) -> (i64, i64) {
    (
        i64::from(router.catalog_version()),
        i64::try_from(router.overlay_revision()).expect("overlay fits i64"),
    )
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits i64")
}

/// A self-identifying F1 negative on `capability`, stamped at `ts`.
fn broken(
    ts: i64,
    capability: &str,
    catalog_version: i64,
    overlay_revision: i64,
) -> CapabilityEvent {
    CapabilityEvent {
        ts,
        lane_key: LANE.to_string(),
        capability: capability.to_string(),
        verdict: Verdict::LearnedBroken(FailurePhase::F1)
            .as_str()
            .to_string(),
        phase: FailurePhase::F1.as_str().to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version,
        overlay_revision,
        provider_kind: None,
        vocab_version: None,
    }
}

/// Block until every capability event the handle accepted has been persisted
/// or superseded, so the bounded channel never drops a burst of rows.
fn wait_drained(handle: &UsageHandle) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let counters = handle.counters();
        let settled =
            counters.capability_events_persisted() + counters.capability_events_superseded();
        if settled >= counters.capability_events_enqueued() {
            return;
        }
        assert!(Instant::now() < deadline, "capability events never drained");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Dispatch one day of verified traffic on the lane, draining each request's
/// capability events into the ledger exactly as the serve path does.
async fn one_day_of_verified_traffic(router: &Router, handle: &UsageHandle) {
    for _ in 0..VERIFIED_REQUESTS_PER_DAY {
        let dispatched = router
            .complete_with_options(structured_output_request(), RouterOptions::default())
            .await;
        assert!(dispatched.result.is_ok(), "canned success dispatched");
        drain_capability_events(
            handle,
            &dispatched.meta,
            router.catalog_version(),
            router.overlay_revision(),
        );
    }
    wait_drained(handle);
}

/// Run the blocking startup warm off the runtime thread against `ledger`.
fn warm(ledger: &Path, router: &Router, scratch: &Path) {
    let (handle, writer) = UsageWriter::start(scratch.to_path_buf(), CHANNEL_CAPACITY, 0, true);
    std::thread::scope(|scope| {
        scope
            .spawn(|| warm_capability_registry_from_ledger(ledger, router, &handle))
            .join()
            .expect("warm thread");
    });
    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn a_month_of_positive_traffic_writes_one_row_and_keeps_older_negatives() {
    // Arrange: a matching boundary and a negative learned before any traffic,
    // then a month of verified requests on a different key of the same lane --
    // more requests than the rebuild window holds.
    const {
        assert!(
            DAYS * VERIFIED_REQUESTS_PER_DAY > REBUILD_ROW_LIMIT,
            "the traffic must be able to overflow the rebuild window"
        );
    }
    let tmp = TempDir::new().expect("tempdir");
    let live = router();
    let (cat, overlay) = revision_of(&live);
    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(ledger.clone(), CHANNEL_CAPACITY, 0, true);
    let ts = now_ms();
    handle.try_send_capability_event_in_generation(CapabilityEvent::tombstone(ts, cat, overlay), 1);
    handle.try_send_capability_event_in_generation(broken(ts, WEB_SEARCH, cat, overlay), 1);
    wait_drained(&handle);

    // Act: the traffic, then a restart that warms a fresh router.
    for _ in 0..DAYS {
        one_day_of_verified_traffic(&live, &handle).await;
    }
    drop(handle);
    writer.shutdown();
    let restarted = router();
    warm(&ledger, &restarted, &tmp.path().join("scratch.db"));

    // Assert: the negative learned before the traffic still replays and acts,
    // and the positive is restated beside it.
    let snap = restarted.learned_capability_snapshot();
    let entry = |cap: &str| {
        snap.iter()
            .find(|e| e.state_key == LANE && e.feature_key == cap)
            .unwrap_or_else(|| panic!("{cap} resident after restart"))
    };
    let negative = entry(WEB_SEARCH);
    assert_eq!(
        negative.verdict,
        Verdict::LearnedBroken(FailurePhase::F1),
        "the pre-traffic negative replays after the traffic"
    );
    assert_eq!(negative.signal_tier, SignalTier::SelfIdentifying);
    assert!(
        negative.expires_at > Instant::now(),
        "the replayed negative is still inside its decay window, so it acts"
    );
    assert_eq!(entry(STRUCTURED_OUTPUT).verdict, Verdict::VerifiedWorking);

    // Rows scale with transitions -- the one fresh positive -- not with the
    // month of requests behind it.
    let db = open(&ledger).expect("reopen ledger");
    // Room for every row the traffic could have written, tombstone and
    // negative included, so an overgrown ledger is counted in full.
    let every_row = DAYS * VERIFIED_REQUESTS_PER_DAY + 2;
    let rows = read_capability_events_after(db.conn(), 0, every_row).expect("read ledger");
    let verified = rows
        .iter()
        .filter(|r| r.verdict.as_deref() == Some(Verdict::VerifiedWorking.as_str()))
        .count();
    assert_eq!(verified, 1, "one positive row for one verdict transition");
    drop(db);
}
