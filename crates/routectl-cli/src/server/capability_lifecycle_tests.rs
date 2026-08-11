//! Consolidated lifecycle acceptance scenarios for the capability-event
//! ledger and its warm rebuild, driving the REAL components together: a real
//! `UsageWriter` persisting to a temp DB, the real startup warm bridge, and a
//! real `Router`'s learned-capability registry read back through
//! `learned_capability_snapshot`. Each scenario is an independent AAA test.
//!
//! Events reach the ledger through the same `try_send_capability_event` call
//! the live drain and the reload seam use, so persistence is exercised end to
//! end rather than mocked. The one intentional test double is
//! [`LiveEventReader`] in the equivalence scenario: it stands in for the
//! pre-persistence, in-memory live event source (no ledger, no clock map),
//! which is the baseline the persisted round-trip must reproduce. It feeds a
//! real registry through the real replay engine.
//!
//! The stage-two admission determinism property -- identical observation
//! sequences plus identical `now` yield identical registry state -- is pinned
//! in the router crate (`capability_acceptance_tests::scenario_f_*`); this
//! module extends it across the persist -> read -> clock-map -> replay
//! lifecycle, including the probe-settlement (`cleared`) path a live re-probe
//! success takes.

use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::{
    COMPUTER_USE, EvidenceSource, FailurePhase, PROMPT_CACHING, SCHEMA_PARSE, STRUCTURED_OUTPUT,
    SignalTier, Verdict, WEB_SEARCH,
};
use routectl_router::{
    CapabilityEventRow as ReplayRow, CapabilityLedgerReader, Config, LearnedRegistryEntry,
    ReplayTombstone, Router,
};
use routectl_usage::{
    CHANNEL_CAPACITY, CapabilityEvent, UsageHandle, UsageWriter, latest_tombstone, open,
    read_capability_events_after,
};
use tempfile::TempDir;

use super::build_router_from_config;
use super::capability_rebuild::warm_capability_registry_from_ledger;

/// Milliseconds in one day, for building timestamps far outside the decay
/// window (default 48h) without pinning any calendar date.
const MS_PER_DAY: i64 = 86_400_000;

// --- shared harness ----------------------------------------------------

/// The default-config router this crate ships: baked catalog version, overlay
/// revision zero. Both feed the boot revision the warm stamps and compares.
async fn default_router(tmp: &TempDir) -> Router {
    let mut config = Config::default();
    config.usage.db_path = tmp.path().join("router-usage.db");
    let config = Arc::new(config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    build_router_from_config(config, secrets)
        .await
        .expect("build router")
}

/// A real writer at `path`, enabled, with no retention prune. Returns the
/// owning writer so the caller flushes it with `shutdown`.
fn writer_at(path: &Path) -> (UsageHandle, UsageWriter) {
    UsageWriter::start(path.to_path_buf(), CHANNEL_CAPACITY, 0, true)
}

/// A real writer whose startup runs the retention prune with `days` of
/// retention (the one-shot `prune_capability_events` fires on start).
fn writer_with_retention(path: &Path, days: u32) -> (UsageHandle, UsageWriter) {
    UsageWriter::start(path.to_path_buf(), CHANNEL_CAPACITY, days, true)
}

/// Current wall-clock epoch milliseconds; the basis the persisted `ts` column
/// carries and the warm clock map reads back.
fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits i64")
}

/// This boot's revision as the ledger's `i64` columns carry it.
fn revision_of(router: &Router) -> (i64, i64) {
    (
        i64::from(router.catalog_version()),
        i64::try_from(router.overlay_revision()).expect("overlay fits i64"),
    )
}

/// Build one capability event bound for the writer. `phase` / `tier` are empty
/// strings for verdicts that carry none (a `cleared` settlement); `evidence`
/// is the pinned observation token a positive / suspect row needs on replay.
#[allow(clippy::too_many_arguments)]
fn cap_event(
    ts: i64,
    lane: &str,
    capability: &str,
    verdict: &str,
    phase: &str,
    source: &str,
    tier: &str,
    evidence: Option<&str>,
    catalog_version: i64,
    overlay_revision: i64,
) -> CapabilityEvent {
    CapabilityEvent {
        ts,
        lane_key: lane.to_string(),
        capability: capability.to_string(),
        verdict: verdict.to_string(),
        phase: phase.to_string(),
        source: source.to_string(),
        tier: tier.to_string(),
        evidence_class: evidence.map(str::to_string),
        upstream_token: None,
        catalog_version,
        overlay_revision,
    }
}

/// A self-identifying `broken` live negative -- the common F1 wire-token shape.
fn broken(ts: i64, lane: &str, cap: &str, tier: &str, cat: i64, overlay: i64) -> CapabilityEvent {
    cap_event(
        ts, lane, cap, "broken", "f1", "live", tier, None, cat, overlay,
    )
}

/// Warm `router` from the ledger at `db_path` through the real bridge, backed
/// by a throwaway writer for the fail-closed enqueue seam, and return the
/// router's post-warm snapshot.
fn warm_and_snapshot(db_path: &Path, router: &Router, scratch: &Path) -> Vec<LearnedRegistryEntry> {
    let (handle, writer) = writer_at(scratch);
    warm_capability_registry_from_ledger(db_path, router, &handle);
    drop(handle);
    writer.shutdown();
    router.learned_capability_snapshot()
}

/// Look up one resident entry by its lane / capability keys.
fn find<'a>(
    snap: &'a [LearnedRegistryEntry],
    lane: &str,
    cap: &str,
) -> Option<&'a LearnedRegistryEntry> {
    snap.iter()
        .find(|e| e.state_key == lane && e.feature_key == cap)
}

/// The dispatch-path decision an entry would drive, derived from its snapshot
/// fields exactly as `LearnedCapabilityRegistry::acting_negative_for` derives
/// it: a non-acting entry, a verified positive, and an advisory F3+Live
/// suspect all allow; an acting negative routes away until its decay window
/// lapses, after which it lapses to a single re-probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActingDecision {
    Allow,
    RouteAway,
    Reprobe,
}

fn acting_decision(entry: &LearnedRegistryEntry, now: Instant) -> ActingDecision {
    let is_acting =
        matches!(entry.signal_tier, SignalTier::SelfIdentifying) || entry.observations >= 2;
    if !is_acting {
        return ActingDecision::Allow;
    }
    match entry.verdict {
        Verdict::VerifiedWorking => ActingDecision::Allow,
        Verdict::LearnedBroken(FailurePhase::F3) if entry.source == EvidenceSource::Live => {
            ActingDecision::Allow
        }
        Verdict::LearnedBroken(_) => {
            if now >= entry.expires_at {
                ActingDecision::Reprobe
            } else {
                ActingDecision::RouteAway
            }
        }
        _ => ActingDecision::Allow,
    }
}

/// The normalized comparison tuple for live-vs-rebuild equivalence: per
/// (lane, capability),
/// the verdict / phase / source / tier tokens, the observation count, and the
/// derived acting decision. Instants, in-flight, and backoff are deliberately
/// excluded -- they legitimately differ between a live registry and a rebuilt
/// one.
type Normalized = (
    String,
    String,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    u32,
    ActingDecision,
);

fn normalize(snap: &[LearnedRegistryEntry], now: Instant) -> Vec<Normalized> {
    let mut out: Vec<Normalized> = snap
        .iter()
        .map(|e| {
            (
                e.state_key.clone(),
                e.feature_key.clone(),
                e.verdict.as_str(),
                e.phase.as_str(),
                e.source.as_str(),
                e.signal_tier.as_str(),
                e.observations,
                acting_decision(e, now),
            )
        })
        .collect();
    out.sort();
    out
}

/// One equivalence-stream event: (lane, capability, verdict, phase, source,
/// tier, evidence). Named to keep the `specs` table off clippy's
/// type-complexity radar.
type EventSpec = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
);

/// The pre-persistence live event source for the equivalence scenario: it
/// yields router-side replay rows at live instants, bypassing the ledger and
/// the clock map. Feeding a real registry through the real replay engine, it
/// is the baseline the persisted round-trip must reproduce.
struct LiveEventReader {
    tombstone: ReplayTombstone,
    rows: Vec<ReplayRow>,
}

impl CapabilityLedgerReader for LiveEventReader {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(self.tombstone)
    }

    fn read_events(&self) -> Vec<ReplayRow> {
        self.rows.clone()
    }
}

// --- Scenario 1: live-vs-rebuild equivalence incl. cleared -------------

#[tokio::test]
async fn live_and_rebuild_registries_match_on_normalized_state() {
    // Arrange: one identical event stream expressed two ways. Each spec is
    // (lane, capability, verdict, phase, source, tier, evidence). A resident
    // negative, a verified positive, a corroborated inferred negative, and a
    // negative later settled by a probe-success `cleared` event.
    let tmp = TempDir::new().expect("tempdir");
    let live_router = default_router(&tmp).await;
    let rebuild_router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&rebuild_router);
    let cat_u32 = rebuild_router.catalog_version();
    let overlay_u64 = rebuild_router.overlay_revision();

    let specs: &[EventSpec] = &[
        (
            "gpt-nick",
            WEB_SEARCH,
            "broken",
            "f1",
            "live",
            "self-identifying",
            None,
        ),
        (
            "claude-nick",
            STRUCTURED_OUTPUT,
            "verified",
            "f3",
            "live",
            "self-identifying",
            Some(SCHEMA_PARSE),
        ),
        (
            "gpt-nick",
            COMPUTER_USE,
            "broken",
            "f1",
            "live",
            "inferred",
            None,
        ),
        (
            "gpt-nick",
            COMPUTER_USE,
            "broken",
            "f1",
            "live",
            "inferred",
            None,
        ),
        (
            "gpt-nick",
            PROMPT_CACHING,
            "broken",
            "f1",
            "live",
            "self-identifying",
            None,
        ),
        ("gpt-nick", PROMPT_CACHING, "cleared", "", "live", "", None),
    ];

    // Persisted side: a matching tombstone then every event through the real
    // writer, all stamped at the same recent instant.
    let ts = now_ms();
    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(ts, cat, overlay));
    for (lane, capability, verdict, phase, source, tier, evidence) in specs {
        handle.try_send_capability_event(cap_event(
            ts, lane, capability, verdict, phase, source, tier, *evidence, cat, overlay,
        ));
    }
    drop(handle);
    writer.shutdown();

    // Live side: the same stream as in-memory replay rows at one shared live
    // instant, oldest-first by rowid (the tombstone sits at rowid 0).
    let base = Instant::now();
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let live_rows: Vec<ReplayRow> = specs
        .iter()
        .enumerate()
        .map(
            |(i, (lane, capability, verdict, phase, source, tier, evidence))| {
                ReplayRow::new(
                    i as i64 + 1,
                    base,
                    (*verdict).to_string(),
                    opt(phase),
                    (*source).to_string(),
                    opt(tier),
                    evidence.map(str::to_string),
                    (*capability).to_string(),
                    (*lane).to_string(),
                    String::new(),
                    cat_u32,
                    overlay_u64,
                )
            },
        )
        .collect();
    let live_reader = LiveEventReader {
        tombstone: ReplayTombstone::new(0, cat_u32, overlay_u64),
        rows: live_rows,
    };

    // Act: build both real registries -- one from the live source, one warmed
    // from the persisted ledger through the real bridge.
    live_router.rebuild_learned_from_ledger(&live_reader);
    let scratch = tmp.path().join("scratch.db");
    let rebuilt = warm_and_snapshot(&ledger, &rebuild_router, &scratch);
    let live = live_router.learned_capability_snapshot();

    // Assert: identical normalized state, and the concrete outcomes the live
    // stream produces (so the equality is not vacuous). The settled
    // `prompt_caching` negative is gone on BOTH sides -- `cleared` reproduces
    // the live re-probe removal.
    let now = Instant::now();
    assert_eq!(
        normalize(&live, now),
        normalize(&rebuilt, now),
        "the persisted round-trip must reproduce the live registry's normalized state",
    );
    assert_eq!(live.len(), 3, "web_search, structured_output, computer_use");
    assert_eq!(rebuilt.len(), 3);
    assert!(
        find(&rebuilt, "gpt-nick", PROMPT_CACHING).is_none(),
        "a probe-settled negative must not resurrect after rebuild",
    );

    let ws = find(&rebuilt, "gpt-nick", WEB_SEARCH).expect("web_search resident");
    assert_eq!(ws.verdict, Verdict::LearnedBroken(FailurePhase::F1));
    assert_eq!(acting_decision(ws, now), ActingDecision::RouteAway);

    let so = find(&rebuilt, "claude-nick", STRUCTURED_OUTPUT).expect("structured_output resident");
    assert_eq!(so.verdict, Verdict::VerifiedWorking);
    assert_eq!(acting_decision(so, now), ActingDecision::Allow);

    let cu = find(&rebuilt, "gpt-nick", COMPUTER_USE).expect("computer_use resident");
    assert_eq!(
        cu.observations, 2,
        "the corroborated inferred negative acts on two"
    );
    assert_eq!(acting_decision(cu, now), ActingDecision::RouteAway);
}

// --- Scenario 2: learn a negative, restart, act without a fresh attempt ---

#[tokio::test]
async fn learned_negative_survives_restart_and_acts_without_a_fresh_attempt() {
    // Arrange: a matching tombstone and one self-identifying negative persisted
    // through the real writer at a recent instant.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&router);
    let ts = now_ms();

    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(ts, cat, overlay));
    handle.try_send_capability_event(broken(
        ts,
        "gpt-nick",
        WEB_SEARCH,
        "self-identifying",
        cat,
        overlay,
    ));
    drop(handle);
    writer.shutdown();

    // Act: a fresh process warms the registry from the ledger.
    let scratch = tmp.path().join("scratch.db");
    let snap = warm_and_snapshot(&ledger, &router, &scratch);

    // Assert: the negative is resident and already acts -- a first dispatch
    // routes away without paying a fresh doomed attempt to re-learn it.
    assert_eq!(snap.len(), 1);
    let entry = find(&snap, "gpt-nick", WEB_SEARCH).expect("negative resident after restart");
    let now = Instant::now();
    assert!(
        entry.expires_at > now,
        "a freshly-learned negative is still within its window"
    );
    assert_eq!(acting_decision(entry, now), ActingDecision::RouteAway);
}

// --- Scenario 3: bump revision, restart, negative gone, re-learn ---------

#[tokio::test]
async fn bumped_revision_across_restart_drops_negative_then_relearns_at_new_revision() {
    // Arrange: a tombstone + negative learned under overlay revision 0.
    let tmp = TempDir::new().expect("tempdir");
    let ts = now_ms();
    let ledger = tmp.path().join("usage.db");

    let seed_router = default_router(&tmp).await;
    let cat = i64::from(seed_router.catalog_version());
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(ts, cat, 0));
    handle.try_send_capability_event(broken(
        ts,
        "gpt-nick",
        WEB_SEARCH,
        "self-identifying",
        cat,
        0,
    ));
    drop(handle);
    writer.shutdown();

    // Act 1: restart at a bumped overlay revision. The ledger's tombstone
    // revision no longer matches, so warm fails closed to empty and writes a
    // fresh boot tombstone at the new revision (into the same ledger).
    let mut bumped = default_router(&tmp).await;
    bumped.install_catalog_overlay(crate::server::test_support::overlay_at_revision(7));
    let (h2, w2) = writer_at(&ledger);
    warm_capability_registry_from_ledger(&ledger, &bumped, &h2);
    drop(h2);
    w2.shutdown();

    // Assert 1: the pre-bump negative is gone (fail-closed to empty).
    assert!(
        bumped.learned_capability_snapshot().is_empty(),
        "a revision bump drops every pre-bump negative",
    );

    // Act 2: fresh traffic under the new revision learns a new negative, which
    // sits after the fresh boot tombstone. A later restart at the new revision
    // replays it -- re-learning against the new catalog's priors.
    let (h3, w3) = writer_at(&ledger);
    h3.try_send_capability_event(broken(
        now_ms(),
        "claude-nick",
        COMPUTER_USE,
        "self-identifying",
        cat,
        7,
    ));
    drop(h3);
    w3.shutdown();

    let mut relearned = default_router(&tmp).await;
    relearned.install_catalog_overlay(crate::server::test_support::overlay_at_revision(7));
    let scratch = tmp.path().join("scratch.db");
    let snap = warm_and_snapshot(&ledger, &relearned, &scratch);

    // Assert 2: the new-revision negative replays; the pre-bump one stays gone.
    assert!(
        find(&snap, "claude-nick", COMPUTER_USE).is_some(),
        "a negative learned under the new revision replays across restart",
    );
    assert!(
        find(&snap, "gpt-nick", WEB_SEARCH).is_none(),
        "the pre-bump negative never crosses the new boundary",
    );
}

// --- Scenario 4: decay across restart lapses to a single re-probe --------

#[tokio::test]
async fn stale_event_lapses_to_a_single_reprobe_across_restart() {
    // Arrange: a negative whose event predates the decay window (default 48h)
    // by well over a month, alongside a fresh negative for contrast.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&router);
    let recent = now_ms();
    let stale = recent - 60 * MS_PER_DAY;

    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(stale, cat, overlay));
    handle.try_send_capability_event(broken(
        stale,
        "gpt-nick",
        WEB_SEARCH,
        "self-identifying",
        cat,
        overlay,
    ));
    handle.try_send_capability_event(broken(
        recent,
        "gpt-nick",
        COMPUTER_USE,
        "self-identifying",
        cat,
        overlay,
    ));
    drop(handle);
    writer.shutdown();

    // Act
    let scratch = tmp.path().join("scratch.db");
    let snap = warm_and_snapshot(&ledger, &router, &scratch);

    // Assert: the stale negative is resident but its window has already lapsed,
    // so the next dispatch admits a single re-probe rather than routing away
    // forever; the recent negative still acts.
    let now = Instant::now();
    let stale_entry = find(&snap, "gpt-nick", WEB_SEARCH).expect("stale negative resident");
    assert!(
        stale_entry.expires_at <= now,
        "a stale negative maps to an expired window"
    );
    assert_eq!(acting_decision(stale_entry, now), ActingDecision::Reprobe);

    let recent_entry = find(&snap, "gpt-nick", COMPUTER_USE).expect("recent negative resident");
    assert_eq!(
        acting_decision(recent_entry, now),
        ActingDecision::RouteAway
    );
}

// --- Scenario 5: reload-then-restart replays post-reload negatives -------

#[tokio::test]
async fn reload_then_restart_replays_only_post_reload_negatives() {
    // Arrange: a boot boundary at revision 0 with a pre-reload negative, then a
    // hot reload advances the overlay revision (mirroring the reload seam's
    // tombstone enqueue) with a post-reload negative after it.
    let tmp = TempDir::new().expect("tempdir");
    let ts = now_ms();
    let ledger = tmp.path().join("usage.db");

    let probe_router = default_router(&tmp).await;
    let cat = i64::from(probe_router.catalog_version());
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(ts, cat, 0));
    handle.try_send_capability_event(broken(
        ts,
        "pre-lane",
        WEB_SEARCH,
        "self-identifying",
        cat,
        0,
    ));
    handle.try_send_capability_event(CapabilityEvent::tombstone(ts, cat, 1));
    handle.try_send_capability_event(broken(
        ts,
        "post-lane",
        WEB_SEARCH,
        "self-identifying",
        cat,
        1,
    ));
    drop(handle);
    writer.shutdown();

    // Act: restart at the post-reload revision.
    let mut router = default_router(&tmp).await;
    router.install_catalog_overlay(crate::server::test_support::overlay_at_revision(1));
    let scratch = tmp.path().join("scratch.db");
    let snap = warm_and_snapshot(&ledger, &router, &scratch);

    // Assert: only the post-reload negative (after the latest tombstone, at the
    // matching revision) replays; the pre-reload one sits before the boundary.
    assert_eq!(snap.len(), 1);
    assert!(
        find(&snap, "post-lane", WEB_SEARCH).is_some(),
        "post-reload negative replays"
    );
    assert!(
        find(&snap, "pre-lane", WEB_SEARCH).is_none(),
        "pre-reload negative is behind the boundary"
    );
}

// --- Scenario 6a: missing tombstone fails closed to empty ----------------

#[tokio::test]
async fn missing_tombstone_fails_closed_to_empty_registry() {
    // Arrange: a ledger holding a negative but NO tombstone boundary.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&router);

    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(broken(
        now_ms(),
        "gpt-nick",
        WEB_SEARCH,
        "self-identifying",
        cat,
        overlay,
    ));
    drop(handle);
    writer.shutdown();

    // Act: warm reads the same ledger and must fail closed, writing a fresh
    // boot tombstone into it.
    let (h2, w2) = writer_at(&ledger);
    warm_capability_registry_from_ledger(&ledger, &router, &h2);
    drop(h2);
    w2.shutdown();

    // Assert: nothing replayed, and a fresh boundary now stamps this revision.
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "no tombstone replays nothing"
    );
    let db = open(&ledger).expect("reopen ledger");
    let boundary = latest_tombstone(db.conn())
        .expect("read tombstone")
        .expect("a fresh boot tombstone exists");
    assert_eq!(boundary.catalog_version, Some(cat));
    assert_eq!(boundary.overlay_revision, Some(overlay));
}

// --- Scenario 6b: probe-source rows replay, unknown-token rows skip -------

#[tokio::test]
async fn probe_source_replays_and_unknown_token_rows_skip_without_panic() {
    // Arrange: after a matching tombstone, a probe-source row (now replays
    // through the shared arms), an unknown-verdict row, an unknown-source row,
    // and one valid live negative.
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&router);
    let ts = now_ms();

    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(ts, cat, overlay));
    handle.try_send_capability_event(cap_event(
        ts,
        "probe-lane",
        WEB_SEARCH,
        "broken",
        "f1",
        "probe",
        "self-identifying",
        None,
        cat,
        overlay,
    ));
    handle.try_send_capability_event(cap_event(
        ts,
        "wobble-lane",
        WEB_SEARCH,
        "wobbled",
        "f1",
        "live",
        "self-identifying",
        None,
        cat,
        overlay,
    ));
    handle.try_send_capability_event(cap_event(
        ts,
        "telepathy-lane",
        WEB_SEARCH,
        "broken",
        "f1",
        "telepathy",
        "self-identifying",
        None,
        cat,
        overlay,
    ));
    handle.try_send_capability_event(broken(
        ts,
        "valid-lane",
        WEB_SEARCH,
        "self-identifying",
        cat,
        overlay,
    ));
    drop(handle);
    writer.shutdown();

    // Act: warm under a capture subscriber -- it must not panic on the odd rows.
    let scratch = tmp.path().join("scratch.db");
    let (h2, w2) = writer_at(&scratch);
    let events = routectl_testkit::capture_events(|| {
        warm_capability_registry_from_ledger(&ledger, &router, &h2);
    });
    drop(h2);
    w2.shutdown();

    // Assert: the probe negative and the valid live negative both replayed;
    // the unknown-verdict and unknown-source rows were skipped with counters
    // and a WARN, and nothing panicked.
    let snap = router.learned_capability_snapshot();
    assert_eq!(snap.len(), 2);
    assert!(
        find(&snap, "valid-lane", WEB_SEARCH).is_some(),
        "the valid negative replays"
    );
    assert!(
        find(&snap, "probe-lane", WEB_SEARCH).is_some(),
        "the probe negative replays through the shared arms"
    );

    let info = events
        .iter()
        .find(|e| e.level == tracing::Level::INFO && e.field("replayed_probe").is_some())
        .expect("rebuild summary emitted");
    assert_eq!(
        info.field("replayed_probe"),
        Some("1"),
        "one probe-source row replayed"
    );
    let skipped_unknown: u32 = info
        .field("skipped_unknown")
        .and_then(|v| v.parse().ok())
        .expect("skipped_unknown counter present");
    assert!(
        skipped_unknown >= 2,
        "the unknown-verdict and unknown-source rows both skip"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN),
        "an unrecognized token emits a WARN",
    );
}

// --- Scenario 7: retention prune never crosses the tombstone -------------

#[tokio::test]
async fn retention_prune_never_crosses_the_tombstone() {
    // Arrange: an old PRE-tombstone negative, the tombstone, and an old
    // POST-tombstone negative -- all persisted with no prune (retention 0).
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&router);
    let old = now_ms() - 60 * MS_PER_DAY;

    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(broken(
        old,
        "ancient-pre",
        WEB_SEARCH,
        "self-identifying",
        cat,
        overlay,
    ));
    handle.try_send_capability_event(CapabilityEvent::tombstone(old, cat, overlay));
    handle.try_send_capability_event(broken(
        old,
        "protected-post",
        COMPUTER_USE,
        "self-identifying",
        cat,
        overlay,
    ));
    drop(handle);
    writer.shutdown();

    // Act 1: a restart with a 30-day retention window runs the startup prune.
    let (h2, w2) = writer_with_retention(&ledger, 30);
    drop(h2);
    w2.shutdown();

    // Assert 1: the pre-tombstone old row is gone; the tombstone and the
    // post-tombstone old row survive regardless of age.
    let db = open(&ledger).expect("reopen ledger");
    let rows = read_capability_events_after(db.conn(), 0, 100).expect("read ledger");
    let lanes: Vec<String> = rows.iter().filter_map(|r| r.lane_key.clone()).collect();
    assert!(
        !lanes.iter().any(|l| l == "ancient-pre"),
        "the old pre-tombstone row is pruned"
    );
    assert!(
        lanes.iter().any(|l| l == "protected-post"),
        "the post-tombstone row survives the prune"
    );
    drop(db);

    // Act 2: warm still replays the protected survivor.
    let scratch = tmp.path().join("scratch.db");
    let snap = warm_and_snapshot(&ledger, &router, &scratch);

    // Assert 2: the survivor is resident (old, so lapsed to re-probe); the
    // pruned pre-tombstone row is absent.
    assert!(
        find(&snap, "protected-post", COMPUTER_USE).is_some(),
        "the survivor replays after prune"
    );
    assert!(
        find(&snap, "ancient-pre", WEB_SEARCH).is_none(),
        "the pruned row cannot replay"
    );
}

// --- Scenario 8: clock-skew future ts clamps to now, replays fresh -------

#[tokio::test]
async fn future_dated_event_clamps_to_now_and_replays_fresh() {
    // Arrange: a negative stamped ten days in the FUTURE (a skewed writer clock).
    let tmp = TempDir::new().expect("tempdir");
    let router = default_router(&tmp).await;
    let (cat, overlay) = revision_of(&router);
    let now = now_ms();
    let future = now + 10 * MS_PER_DAY;

    let ledger = tmp.path().join("usage.db");
    let (handle, writer) = writer_at(&ledger);
    handle.try_send_capability_event(CapabilityEvent::tombstone(now, cat, overlay));
    handle.try_send_capability_event(broken(
        future,
        "gpt-nick",
        WEB_SEARCH,
        "self-identifying",
        cat,
        overlay,
    ));
    drop(handle);
    writer.shutdown();

    // Act
    let scratch = tmp.path().join("scratch.db");
    let snap = warm_and_snapshot(&ledger, &router, &scratch);

    // Assert: the clock map clamps the future timestamp to now, so the negative
    // replays as fresh -- resident, within its window, and acting.
    let clock = Instant::now();
    let entry = find(&snap, "gpt-nick", WEB_SEARCH).expect("future-dated negative resident");
    assert!(
        entry.expires_at > clock,
        "a clamped future event maps to a fresh window"
    );
    assert_eq!(acting_decision(entry, clock), ActingDecision::RouteAway);
}
