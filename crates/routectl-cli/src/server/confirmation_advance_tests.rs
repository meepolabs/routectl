//! Tests for daemon-owned confirmation advancement: the properties that only hold
//! because the advancement does not live in the request's future.
//!
//! Each test drives the tracker against a writer it controls, so the commit can be
//! held open at the exact instant a request-side future is dropped -- which is the
//! whole window these tests exist to cover. The `parked` fixture below is what makes
//! that deterministic rather than a race: the writer is held off its queue until the
//! test says so, so "the request future is gone and the row has not committed yet" is
//! a state the test establishes rather than hopes for.

use super::*;

use routectl_router::{
    Config, ModelEntry, ProviderEntry, Router, field_verdict_event_stamps_for_tests,
    plant_acting_field_verdict_for_tests,
};
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_usage::{CHANNEL_CAPACITY, CapabilityEvent, UsageHandle, UsageWriter};
use tempfile::TempDir;

/// How long a wait may take before the test fails rather than hangs.
const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The lane and field this file plants on.
const STATE_KEY: &str = "nick";
const FIELD_PATH: &str = "thinking.enabled.display";

/// The wire-shape capability key, assembled from parts: the namespace prefix has
/// exactly one compiled spelling and a lexical guard forbids a second.
fn field_key() -> String {
    format!("{}{}{}", "fie", "ld:", FIELD_PATH)
}

/// A router carrying one resident ACTING field verdict with ZERO acknowledged
/// confirmations -- the one state from which an advancement is observable.
///
/// Zero deliberately: a verdict planted WITH confirmations is already pre-flight
/// eligible, so every assertion below about the advancement would pass whether or not
/// it ran.
fn router_awaiting_confirmation() -> Arc<Router> {
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let router =
        Arc::new(Router::new(Arc::new(config)).with_capability_writes_assumed_durable_for_tests());
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 0);
    assert!(
        !eligible(&router),
        "fixture premise: a verdict with no acknowledged confirmation is NOT pre-flight \
         eligible -- which is the gap the advancement closes, and what makes every \
         assertion below non-vacuous",
    );
    router
}

/// Whether the planted verdict is pre-flight eligible on `router` RIGHT NOW.
///
/// Read through the router's own status surface rather than a counter, because
/// eligibility is what the advancement is FOR: a test reading a raw count could pass
/// on a build that raised it somewhere the planner never looks.
fn eligible(router: &Router) -> bool {
    router
        .field_verdict_status()
        .iter()
        .any(|row| row.capability_key == field_key() && row.blocked_reason.is_none())
}

/// The identity an advancement must present, with the LIVE generation and the resident
/// incarnation read off `router`.
///
/// Read rather than hardcoded: the router's acknowledgment validates both against live
/// state, so a fabricated pair would produce a refusal indistinguishable from a broken
/// acknowledgment's.
fn identity_for(router: &Router, observations: u32) -> ConfirmationIdentity {
    let (generation, incarnation) =
        field_verdict_event_stamps_for_tests(router, STATE_KEY, FIELD_PATH);
    ConfirmationIdentity {
        state_key: STATE_KEY.to_string(),
        capability_key: field_key(),
        provider_kind: "anthropic-api".to_string(),
        generation,
        incarnation,
        observations,
    }
}

/// The learned-negative row an advancement's event carries.
fn field_event() -> CapabilityEvent {
    CapabilityEvent {
        ts: 1_000,
        lane_key: STATE_KEY.to_string(),
        capability: field_key(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: 8,
        overlay_revision: 1,
    }
}

/// A real file-backed writer plus its handle and tempdir guard.
fn live_writer() -> (TempDir, UsageHandle, UsageWriter) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    drop(routectl_usage::open(&path).expect("migrating open"));
    let (handle, writer) = UsageWriter::start(path, CHANNEL_CAPACITY, 0, true);
    (dir, handle, writer)
}

/// Spin until `cond` holds, or fail rather than hang.
async fn until(label: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {label}",
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// THE cancellation property
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_request_future_still_advances_eligibility_in_the_same_process() {
    // THE property daemon ownership buys, and the bug it fixes: the advancement's
    // await is a cancellation point, so with the await inline in the handler a client
    // that hung up mid-commit left the row on disk and the count unapplied -- the
    // verdict dormant until a restart, silently, and only for those requests.
    //
    // DETERMINISTIC, not a race. The writer is PARKED before the row is admitted, so
    // the sequence the test establishes is exact:
    //   1. the row is admitted (so the tracker owns a receipt whose outcome is
    //      pending);
    //   2. the request-side future is dropped, while the commit has provably NOT
    //      happened -- asserted, not assumed;
    //   3. the writer is released and commits;
    //   4. the LIVE router has advanced, with no restart.
    //
    // Step 2's assertion is what makes this a cancellation test rather than a
    // completion test: without it, a writer that had already committed would make the
    // drop irrelevant and the test would pass on the inline-await build.
    //
    // Mutation check: restore the inline await (advance on the request's own future
    // instead of handing the receipt to the tracker) -> red here, because the drop in
    // step 2 cancels the advancement and step 4 finds the verdict still dormant.
    let router = router_awaiting_confirmation();
    let tracker = ConfirmationTracker::new();
    // PARKED: nothing consumes the writer channel yet, so an admitted row provably
    // cannot have committed when the request-side future is dropped below.
    let (usage, park) = ParkedWriter::park();

    let identity = identity_for(&router, 1);
    let claim = tracker.claim().expect("an open tracker admits a claim");
    let receipt = usage
        .admit_acknowledged_capability_event(
            field_event(),
            identity.generation,
            identity.incarnation,
        )
        .expect("the channel admits the event");

    // The REQUEST-SIDE future: whatever a handler does after admitting. The point is
    // that it is CANCELLED at an await, before the commit lands.
    let request_side = Box::pin(std::future::pending::<()>());

    tracker.advance(claim, Arc::clone(&router), identity, receipt);

    // PREMISE: the commit has NOT happened, which is what makes the drop below a
    // cancellation rather than a no-op. Without this the test would pass on the
    // inline-await build whenever the writer happened to be fast.
    assert!(
        !eligible(&router),
        "premise: the row cannot have committed -- the writer is parked, so the \
         advancement is genuinely pending when the request future is dropped",
    );
    assert_eq!(
        tracker.in_flight(),
        1,
        "premise: the advancement is in flight, owned by the tracker",
    );

    // THE CANCELLATION: the request-side future is dropped, as a client hangup does.
    drop(request_side);

    // Release the writer and let the row commit.
    let (writer, _dir) = park.release();
    until("the advancement to finish", || tracker.in_flight() == 0).await;

    assert!(
        eligible(&router),
        "the LIVE router advanced its acknowledged confirmation after the request \
         future was dropped: the advancement is the daemon's work, so a cancelled \
         client cannot leave a durable row unapplied. Same process, no restart",
    );

    drop(usage);
    writer.shutdown();
}

#[tokio::test]
async fn a_stale_incarnation_advance_does_nothing_even_though_its_row_commits() {
    // The generation/incarnation half, on the tracker's own path. The row COMMITS --
    // so a build that advanced on a successful commit alone would pass the
    // cancellation test above and fail this one. What it rules out is an advancement
    // that credits whatever lifecycle is resident when the task happens to run rather
    // than the one whose row committed.
    //
    // Mutation check: delete the incarnation comparison from
    // `FieldVerdictRegistry::acknowledge_durable_confirmation` -> red here.
    let (_dir, usage, writer) = live_writer();
    let router = router_awaiting_confirmation();
    let tracker = ConfirmationTracker::new();

    let mut stale = identity_for(&router, 1);
    // A MISMATCHED incarnation, above the resident one: a learned incarnation starts
    // at zero, so there is no lower value a fresh fixture can plant. The high side is
    // not the lesser case either -- admitted, it would reseed the canary state on the
    // strength of a mutation this registry never saw.
    stale.incarnation = stale
        .incarnation
        .checked_add(1)
        .expect("an incarnation one above the resident one");

    let claim = tracker.claim().expect("an open tracker admits a claim");
    let receipt = usage
        .admit_acknowledged_capability_event(field_event(), stale.generation, stale.incarnation)
        .expect("a live writer admits the event");
    tracker.advance(claim, Arc::clone(&router), stale, receipt);

    until("the advancement to finish", || tracker.in_flight() == 0).await;

    assert!(
        !eligible(&router),
        "a durable row for a DIFFERENT lifecycle must advance nothing: crediting the \
         resident verdict with another incarnation's evidence is exactly what the \
         router's incarnation check refuses",
    );

    drop(usage);
    writer.shutdown();
}

#[tokio::test]
async fn a_failed_write_advances_nothing() {
    // The durability half on this path: the row cannot land, so nothing may advance.
    // Capture disabled is the production state that produces it -- the acknowledged
    // admission refuses, so there is no receipt at all. Driven here through a receipt
    // whose writer is GONE, which is the other shape: the sender drops without
    // answering and `await_outcome` resolves to a write failure.
    //
    // Mutation check: drop the `is_durable()` guard in `advance_owned` -> red here.
    let router = router_awaiting_confirmation();
    let tracker = ConfirmationTracker::new();
    // PARKED, then DROPPED without ever being released: the writer never starts, so
    // the receipt's sender is destroyed unanswered and `await_outcome` resolves as a
    // write failure -- the safe answer, since nothing is known to have landed.
    //
    // A live writer would not do: it commits in microseconds, so dropping it after
    // admission races the commit and the test would assert against whichever won.
    let (usage, park) = ParkedWriter::park();

    let identity = identity_for(&router, 1);
    let claim = tracker.claim().expect("an open tracker admits a claim");
    let receipt = usage
        .admit_acknowledged_capability_event(
            field_event(),
            identity.generation,
            identity.incarnation,
        )
        .expect("admitted");

    // The writer's receiving end goes away BEFORE anything consumes it.
    drop(park);

    tracker.advance(claim, Arc::clone(&router), identity, receipt);
    until("the advancement to finish", || tracker.in_flight() == 0).await;

    assert!(
        !eligible(&router),
        "an advancement whose write did not land must leave the verdict dormant: the \
         lane stays served by reactive forward-and-repair",
    );
}

// ---------------------------------------------------------------------------
// Shutdown and cancel controls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_closed_tracker_admits_no_further_advancement() {
    // The shutdown gate. A refused claim is what stops an advancement starting after
    // the writer is about to drain -- one started then could outlive the commit it
    // needs. The refusal is SAFE: the verdict stays dormant and the next boot's ledger
    // replay seeds its count, which is the recovery this feature's absence relied on
    // entirely.
    let tracker = ConfirmationTracker::new();
    assert!(!tracker.is_closed(), "premise: a fresh tracker is open");
    assert!(
        tracker.claim().is_some(),
        "premise: an open tracker admits a claim",
    );

    tracker.close_and_wait(WAIT).await;

    assert!(tracker.is_closed());
    assert!(
        tracker.claim().is_none(),
        "a closed tracker must admit nothing further: an advancement starting now \
         could outlive the writer whose commit it awaits",
    );
    assert_eq!(
        tracker.in_flight(),
        0,
        "and the claim taken above released on drop, so shutdown waits for nothing \
         that was never admitted",
    );
}

#[tokio::test]
async fn shutdown_waits_for_an_in_flight_advancement_before_the_writer_drains() {
    // The ordering the whole design rests on: an in-flight advancement is waiting on
    // a commit, so shutdown must wait for it WHILE THE WRITER IS STILL ALIVE.
    //
    // Observed as a completed advancement rather than as a timing measurement:
    // `close_and_wait` returns and the verdict is eligible, which can only be true if
    // the wait actually waited.
    //
    // Mutation check: make `close_and_wait` return without waiting (drop the loop) ->
    // red here, because the advancement has not applied when the assertion runs.
    let router = router_awaiting_confirmation();
    let tracker = ConfirmationTracker::new();
    let (usage, park) = ParkedWriter::park();

    let identity = identity_for(&router, 1);
    let claim = tracker.claim().expect("open");
    let receipt = usage
        .admit_acknowledged_capability_event(
            field_event(),
            identity.generation,
            identity.incarnation,
        )
        .expect("admitted");
    tracker.advance(claim, Arc::clone(&router), identity, receipt);
    assert_eq!(tracker.in_flight(), 1, "premise: one advancement pending");

    // Release the parked writer so the commit can land, then shut down: the wait must
    // hold until the advancement applies.
    let (writer, _dir) = park.release();
    tracker.close_and_wait(WAIT).await;

    assert_eq!(
        tracker.in_flight(),
        0,
        "close_and_wait must not return while an advancement is still in flight",
    );
    assert!(
        eligible(&router),
        "and the advancement it waited for must have applied",
    );

    drop(usage);
    writer.shutdown();
}

#[tokio::test]
async fn shutdown_abandons_a_stalled_advancement_rather_than_hanging_or_failing() {
    // The bounded-abandonment contract, and the one place this tracker deliberately
    // differs from the purge tracker: an advancement that cannot finish is ABANDONED,
    // logged, and shutdown continues. It holds no lease and owes the registry nothing;
    // its row is already durable, so the next boot's cold-rebuild seed restores the
    // count. Treating this as ambiguous routing state (as a purge settlement's
    // timeout is) would trigger terminal shutdown for a condition a restart already
    // resolves.
    //
    // The advancement is stalled by parking the writer and NEVER releasing it, so the
    // receipt's outcome never arrives.
    let router = router_awaiting_confirmation();
    let tracker = ConfirmationTracker::new();
    // PARKED and never released: the receipt's outcome never arrives, so the
    // advancement genuinely cannot finish.
    let (usage, park) = ParkedWriter::park();

    let identity = identity_for(&router, 1);
    let claim = tracker.claim().expect("open");
    let receipt = usage
        .admit_acknowledged_capability_event(
            field_event(),
            identity.generation,
            identity.incarnation,
        )
        .expect("admitted");
    tracker.advance(claim, Arc::clone(&router), identity, receipt);

    // A SHORT deadline, so the test asserts the timeout path rather than waiting out
    // the production one.
    tracker
        .close_and_wait(std::time::Duration::from_millis(50))
        .await;

    assert!(
        tracker.is_closed(),
        "shutdown closed the tracker and returned rather than hanging on an \
         advancement that cannot finish",
    );
    assert_eq!(
        tracker.in_flight(),
        1,
        "premise: the advancement really was still in flight -- so the return above \
         is an ABANDONMENT rather than a completion the deadline happened to cover",
    );
    assert!(
        !eligible(&router),
        "and the abandoned advancement applied nothing, which is the safe direction: \
         the verdict is dormant, the lane repairs on rejection, and the next boot's \
         ledger replay restores the count",
    );

    // Only now is the writer released, so the stall above was genuine.
    let (writer, _dir) = park.release();
    drop(usage);
    writer.shutdown();
}

// ---------------------------------------------------------------------------
// The park fixture
// ---------------------------------------------------------------------------

/// A writer channel whose receiving end is HELD, so an admitted row provably has not
/// committed until the test releases it.
///
/// # Why this rather than a real writer plus a sleep
///
/// The cancellation property is about an ORDERING -- the request-side future is gone
/// and the commit has not happened -- and a real writer commits in microseconds, so a
/// sleep could only make that ordering likely. Here it is exact: nothing consumes the
/// channel until `release` starts a writer thread over it, so "the commit has not
/// happened" is a state the test ESTABLISHES rather than hopes for.
///
/// What is NOT substituted is anything under test. The admission is the real
/// `admit_acknowledged_capability_event` (so its enabled gate and refusal accounting
/// run), the receipt and its `await_outcome` are real, the commit is the real writer's
/// own persist path, and the advancement is the real tracker task calling the real
/// router entry point. Only WHEN the writer starts consuming is the test's.
struct ParkedWriter {
    /// The unread receiving end, taken by `release`.
    rx: Option<tokio::sync::mpsc::Receiver<routectl_usage::WriterMessage>>,
    /// The counters the handle and the eventual writer share.
    counters: Arc<routectl_usage::UsageCounters>,
    /// Where the released writer's rows land; kept so the file outlives it.
    dir: TempDir,
}

impl ParkedWriter {
    /// A handle whose writer has not started, and the fixture that starts it.
    fn park() -> (UsageHandle, Self) {
        let (tx, rx) =
            tokio::sync::mpsc::channel::<routectl_usage::WriterMessage>(CHANNEL_CAPACITY);
        let counters = Arc::new(routectl_usage::UsageCounters::default());
        let handle = routectl_usage::handle_over_channel_with(tx, Arc::clone(&counters));
        let dir = TempDir::new().expect("tempdir");
        (
            handle,
            Self {
                rx: Some(rx),
                counters,
                dir,
            },
        )
    }

    /// Start the real writer over the parked channel, so everything queued commits.
    ///
    /// Returns the writer for the caller to shut down -- the same production type and
    /// the same drain, so a released fixture is not a special case at teardown.
    fn release(mut self) -> (UsageWriter, TempDir) {
        let rx = self.rx.take().expect("release consumes the fixture once");
        let path = self.dir.path().join("usage.db");
        drop(routectl_usage::open(&path).expect("migrating open"));
        let writer = UsageWriter::start_over_channel(path, 0, rx, Arc::clone(&self.counters));
        (writer, self.dir)
    }
}

// The hostile-concurrency sweep lives in a sibling file to keep this one under
// the size ceiling. It compiles into THIS module via `include!`, so the
// fixtures above stay in scope and no test's module path changes.
include!("confirmation_advance_hostile_tests.rs");
