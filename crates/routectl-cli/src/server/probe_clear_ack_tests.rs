//! The probe-clear acknowledgement against a REAL usage writer: what it answers
//! for each writer outcome, the row it writes, and what a refusal leaves behind
//! on the router.

use super::*;

use std::collections::BTreeMap;
use std::path::Path;

use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};
use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, ResolvedModel, TEST_RECHECK_KWARG,
    TEST_RECHECK_PATH, plant_acting_field_verdict_for_tests,
};

use crate::server::test_support::{drain_usage_writer_strict, live_usage_writer};

const NICKNAME: &str = "sonnet";
const STATE_KEY: &str = "anthropic#claude-sonnet-4-5";

/// Accepts every free token count, so a queued re-check settles as ACCEPTED and
/// submits its clear.
struct AcceptingCounter;

#[async_trait::async_trait]
impl Provider for AcceptingCounter {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("anthropic", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("anthropic", 500, "unused"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        Ok(TokenCount {
            input_tokens: 5,
            extras: serde_json::Map::new(),
        })
    }
}

/// A Router on a remote-looking lane, resolving to [`AcceptingCounter`].
fn lane_router() -> Router {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://PROBE_CLEAR_ACK_TEST_KEY"),
    );
    let mut models = BTreeMap::new();
    models.insert(
        NICKNAME.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "default".to_string(),
        AliasValue::Single(NICKNAME.to_string()),
    );
    let mut router = Router::new(Arc::new(Config {
        providers,
        models,
        aliases,
        ..Config::default()
    }));
    let mut resolved = BTreeMap::new();
    resolved.insert(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME,
            "anthropic",
            Arc::new(AcceptingCounter) as Arc<dyn Provider>,
            "claude-sonnet-4-5",
        )),
    );
    router.install_resolved_models(resolved);
    router
}

/// A swap publishing `router` with the acknowledgement over `usage` installed,
/// wired the way the daemon's boot wires it.
fn published(router: Router, usage: &UsageHandle) -> Arc<ArcSwap<Router>> {
    Arc::new_cyclic(|live| {
        ArcSwap::from_pointee(install_probe_clear_ack(router, usage, live.clone()))
    })
}

/// An acknowledgement over `usage` reading revisions from `swap`, held directly
/// so a test can submit a clear without running a probe.
fn ack_over(usage: &UsageHandle, swap: &Arc<ArcSwap<Router>>) -> UsageProbeClearAck {
    UsageProbeClearAck {
        usage: usage.clone(),
        live: Arc::downgrade(swap),
    }
}

/// Queue one re-check on the published router: an acting negative on the
/// count-validated row, and an admitted request carrying that field.
async fn queue_recheck(swap: &Arc<ArcSwap<Router>>) {
    let live = swap.load_full();
    plant_acting_field_verdict_for_tests(&live, STATE_KEY, TEST_RECHECK_PATH, 0);
    let request = ChatRequest {
        chat_template_kwargs: Some(serde_json::json!({ TEST_RECHECK_KWARG: "summarized" })),
        ..Default::default()
    };
    let _ = live.complete(request).await;
    assert_eq!(
        live.probe_scheduler_snapshot().queued,
        1,
        "premise: the admitted request queued one re-check",
    );
}

fn is_acting(router: &Router) -> bool {
    router
        .fidelity_snapshot()
        .acting
        .iter()
        .any(|row| row.state_key == STATE_KEY && row.feature_key.ends_with(TEST_RECHECK_PATH))
}

/// A clear for the re-check row at the given stamps.
fn cleared(generation: u64, incarnation: u64) -> CapabilityClearedEvent {
    CapabilityClearedEvent {
        persistence_generation: generation,
        incarnation,
        state_key: STATE_KEY.to_string(),
        capability_key: "cap-under-test".to_string(),
        provider_kind: "anthropic-api".to_string(),
    }
}

/// `(verdict, source, catalog_version, overlay_revision)` of every row for
/// `capability`, read from the ledger file.
fn rows_for(db_path: &Path, capability: &str) -> Vec<(String, String, i64, i64)> {
    let db = routectl_usage::open_readonly(db_path).expect("read-only open");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT verdict, source, catalog_version, overlay_revision \
             FROM capability_events WHERE capability = ?1 ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([capability], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .expect("query")
    .collect::<std::result::Result<Vec<_>, _>>()
    .expect("rows")
}

/// A closed writer channel: the adapter answers NOT durable, so the router keeps
/// the negative acting and the re-check job retryable.
///
/// Mutation check: make `acknowledge_clear` return `true` unconditionally, or on
/// an admission refusal -> red on every assertion below.
#[tokio::test]
async fn a_closed_channel_reports_not_durable_and_the_job_stays_retryable() {
    let usage = routectl_usage::handle_with_closed_channel();
    let swap = published(lane_router(), &usage);
    queue_recheck(&swap).await;
    let live = swap.load_full();

    let summary = live.run_probe_pass().await;

    assert_eq!(summary.free_validators_run, 1, "premise: the re-check ran");
    assert!(is_acting(&live), "the negative stays acting");
    let snap = live.probe_scheduler_snapshot();
    assert_eq!(
        snap.resolved_total, 0,
        "not resolved without a durable clear"
    );
    assert_eq!(snap.backing_off, 1, "the job stays retryable");
    assert_eq!(snap.clear_persistence_failures_total, 1);
    assert_eq!(
        usage.counters().capability_writer_unavailable(),
        1,
        "premise: the clear reached the closed channel and was refused there",
    );
}

/// A healthy writer: the adapter answers durable, the router clears and
/// resolves, and the ledger carries one LIVE-source `cleared` row.
///
/// The positive control for the closed-channel test above.
#[tokio::test]
async fn a_committed_clear_is_durable_and_lands_one_live_cleared_row() {
    let (_dir, path, usage, writer) = live_usage_writer().await;
    let swap = published(lane_router(), &usage);
    queue_recheck(&swap).await;
    let live = swap.load_full();
    let capability = live
        .fidelity_snapshot()
        .acting
        .iter()
        .find(|row| row.state_key == STATE_KEY)
        .map(|row| row.feature_key.clone())
        .expect("premise: the planted negative is acting");

    live.run_probe_pass().await;

    assert!(!is_acting(&live), "the disproved negative is cleared");
    let snap = live.probe_scheduler_snapshot();
    assert_eq!(snap.resolved_total, 1);
    assert_eq!(snap.clear_persistence_failures_total, 0);
    let rows = rows_for(&path, &capability);
    assert_eq!(
        rows.len(),
        1,
        "exactly one row for the cleared key: {rows:?}"
    );
    assert_eq!(rows[0].0, "cleared");
    assert_eq!(
        rows[0].1, "live",
        "live source: the warm rebuild skips a field row of any other source",
    );

    drop((live, swap, usage));
    drain_usage_writer_strict(writer).await;
}

/// The row is stamped with the revision pair of the router PUBLISHED when the
/// clear is submitted, not the one live when the acknowledgement was built.
///
/// Mutation check: capture the revision pair at install time -> red.
#[tokio::test]
async fn the_clear_is_stamped_with_the_published_router_revision() {
    let (_dir, path, usage, writer) = live_usage_writer().await;
    let swap = Arc::new(ArcSwap::from_pointee(lane_router()));
    let ack = ack_over(&usage, &swap);
    let mut replacement = lane_router();
    replacement.install_catalog_overlay(crate::server::test_support::overlay_at_revision(9));
    let expected_catalog = i64::from(replacement.catalog_version());
    swap.store(Arc::new(replacement));

    assert!(
        ack.acknowledge_clear(cleared(1, 1)).await,
        "the clear commits"
    );

    assert_eq!(
        rows_for(&path, "cap-under-test"),
        vec![(
            "cleared".to_string(),
            "live".to_string(),
            expected_catalog,
            9
        )],
        "stamped with the replacement's overlay revision",
    );
    drop((ack, swap, usage));
    drain_usage_writer_strict(writer).await;
}

/// Only a COMMITTED write is durable: a row the writer drops as superseded by a
/// purge of the same key answers not durable, though nothing failed.
///
/// Mutation check: answer `outcome != WriteFailed` instead of `is_durable()` ->
/// red.
#[tokio::test]
async fn a_clear_superseded_by_a_purge_is_not_durable() {
    let (_dir, path, usage, writer) = live_usage_writer().await;
    let swap = Arc::new(ArcSwap::from_pointee(lane_router()));
    let floor = usage
        .admit_capability_batch_at(
            vec![cleared_capability_row(&cleared(1, 5), 1_000, 0, 0)],
            1,
            5,
        )
        .expect("the purge batch is admitted")
        .await_outcome()
        .await;
    assert!(
        matches!(floor, routectl_usage::BatchCommit::Committed { .. }),
        "premise: the purge floor is raised: {floor:?}",
    );
    let ack = ack_over(&usage, &swap);

    let durable = ack.acknowledge_clear(cleared(1, 5)).await;

    assert!(!durable, "a superseded row is not on disk");
    assert_eq!(
        rows_for(&path, "cap-under-test").len(),
        1,
        "premise: only the purge's own row landed",
    );
    drop((ack, swap, usage));
    drain_usage_writer_strict(writer).await;
}

/// With the daemon's swap gone, nothing is submitted and the answer is not
/// durable.
#[tokio::test]
async fn a_dropped_swap_submits_nothing() {
    let usage = routectl_usage::handle_with_closed_channel();
    let ack = UsageProbeClearAck {
        usage: usage.clone(),
        live: Weak::new(),
    };

    assert!(!ack.acknowledge_clear(cleared(1, 1)).await);
    assert_eq!(
        usage.counters().capability_writer_unavailable(),
        0,
        "no admission was attempted",
    );
}
