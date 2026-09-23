//! Coverage for the fidelity INFO emitter: the counter-to-field mapping, the
//! bounded sanitized rows, the process-global fields, the probe state, and the
//! one-line-per-request rule.
//!
//! # Why the row tests plant a REAL verdict
//!
//! An earlier version of this file asserted that the rows field was emitted as
//! `[]` on a fresh router. That test was vacuous in the way that matters: a
//! projection that rendered nothing at all, or one wired to the wrong source,
//! passes it. So the row cases here plant a resident acting verdict into the
//! ACTUAL router behind the facade -- through the router's own gated planting
//! seam, which goes through the same `import_entries` and canary-seed paths a cold
//! boot uses -- and then assert the emitted line carries every required value.

use super::*;

use std::sync::Arc;

use super::super::router_view::StatusRouterHandle;
use arc_swap::ArcSwap;
use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, Router, plant_acting_field_verdict_for_tests,
};

/// The one grounded closed-table path, so a planted verdict resolves to a real
/// transform class rather than the unknown-class arm.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// The state key the fixture router resolves, and the one a planted verdict is
/// keyed on.
const STATE_KEY: &str = "sonnet";

/// A router over a config whose one model resolves, so a planted verdict's state
/// key has a provider kind to normalize against.
fn fixture_router() -> Router {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://WF_TEST_KEY"),
    );
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    // DURABLE PERSISTENCE IS ASSUMED, stated rather than defaulted: every verdict row
    // reports `capability_writer_unhealthy` while capability writes cannot be
    // guaranteed, and `Router::new` installs no health read. A fixture that said
    // nothing would make every blocked-reason assertion in this file read that one
    // token regardless of the state it was written to describe.
    Router::new(Arc::new(Config {
        providers,
        models,
        aliases,
        ..Config::default()
    }))
    .with_capability_writes_assumed_durable_for_tests()
}

/// A facade view over `router`, which is what the panels hold.
fn view_over(router: Router) -> StatusRouterView {
    StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router))).view()
}

fn fresh_view() -> StatusRouterView {
    view_over(fixture_router())
}

/// A view over a router carrying ONE resident acting verdict, acknowledged once.
fn view_with_planted_verdict() -> StatusRouterView {
    let router = fixture_router();
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, GROUNDED_PATH, 1);
    view_over(router)
}

/// Capture the one snapshot line `body` emits, failing loudly if there is none.
fn captured_snapshot(body: impl FnOnce()) -> routectl_testkit::CapturedEvent {
    let events = routectl_testkit::capture_events(body);
    let mut lines: Vec<routectl_testkit::CapturedEvent> = events
        .into_iter()
        .filter(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE)
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "exactly one fidelity snapshot line per emitting build",
    );
    lines.remove(0)
}

fn no_budgets() -> Vec<PaidProbeBudget> {
    Vec::new()
}

fn healthy_globals() -> AccountingGlobals {
    AccountingGlobals {
        writer_degraded: false,
        consumed_unauthorized_total: 0,
    }
}

// ---------------------------------------------------------------------------
// The counter-to-field mapping
// ---------------------------------------------------------------------------

/// Every counter field on the line carries the value of the counter it is NAMED
/// for, asserted on the CAPTURED EVENT.
///
/// This replaces a source-parsing guard, and the reason is that the old one pinned
/// how the code was WRITTEN rather than what it emits: a `tracing` macro change or a
/// renamed local broke the parse while the behavior was fine, and a genuinely wrong
/// mapping written a different way could slip past it.
///
/// What makes an event-level assertion sound here is that every counter is seeded to
/// a DIFFERENT number. Equal values make any permutation pass, which is exactly how
/// a swap survives -- and a swap is the defect that matters, because it emits a
/// well-formed line reporting one fact as another and no assertion on the values
/// alone could see it.
///
/// Mutation checks: swap any two right-hand sides in the `info!` call -> red on both
/// of their assertions; point one field at a constant -> red on that one.
#[test]
fn every_counter_field_carries_the_value_of_the_counter_it_is_named_for() {
    let router = fixture_router();
    let seeded = routectl_router::seed_distinct_fidelity_counters_for_tests(&router);
    let view = view_over(router);
    // The globals are seeded distinctly too, and to values no counter above holds --
    // so a field wired to a counter instead of a global is caught as well.
    let globals = AccountingGlobals {
        writer_degraded: true,
        consumed_unauthorized_total: 41,
    };

    let info = captured_snapshot(|| {
        log_field_verdict_snapshot(&view, &no_budgets(), globals, FidelityEmission::always());
    });

    // Asserted against what was SEEDED rather than against restated literals: a
    // literal here would be a second copy of the fixture's numbers, free to drift.
    for (field, expected) in [
        ("rc_field_repair_attempted_total", seeded.repair_attempted),
        ("rc_field_repair_succeeded_total", seeded.repair_succeeded),
        ("rc_field_verdicts_learned_total", seeded.verdicts_learned),
        (
            "rc_field_outstanding_unconfirmed_total",
            seeded.outstanding_unconfirmed,
        ),
        (
            "rc_field_disproved_requests_total",
            seeded.disproved_requests,
        ),
        ("rc_field_preflight_actions_total", seeded.preflight_actions),
        ("rc_parser_unlocalized_total", seeded.parser_unlocalized),
    ] {
        assert_eq!(
            info.field(field),
            Some(expected.to_string().as_str()),
            "{field} must carry its own counter's value ({expected}): every counter \
             here holds a DIFFERENT number, so a swap between two fields cannot pass",
        );
    }
    // The premise the whole test rests on: the seeded values really are distinct, so
    // no permutation of them satisfies the loop above.
    let mut values = vec![
        seeded.repair_attempted,
        seeded.repair_succeeded,
        seeded.verdicts_learned,
        seeded.outstanding_unconfirmed,
        seeded.disproved_requests,
        seeded.preflight_actions,
        seeded.parser_unlocalized,
    ];
    let planned = values.len();
    values.sort_unstable();
    values.dedup();
    assert_eq!(
        values.len(),
        planned,
        "premise: the seeded counter values are pairwise distinct, or a swap would \
         pass the assertions above: {values:?}",
    );
}

/// The two process-global fields carry the GLOBALS' values, not a counter's or a
/// row's.
///
/// Seeded to values no counter holds, so a field wired to the wrong source is caught
/// by the value rather than by reading the source. The scope claim is the point: a
/// process-wide fact in a per-provider position reads as a multiple of the truth to
/// anyone summing the column.
#[test]
fn the_global_fields_carry_the_globals_values() {
    let router = fixture_router();
    let _seeded = routectl_router::seed_distinct_fidelity_counters_for_tests(&router);
    let view = view_over(router);
    let globals = AccountingGlobals {
        writer_degraded: true,
        consumed_unauthorized_total: 41,
    };

    let info = captured_snapshot(|| {
        log_field_verdict_snapshot(&view, &no_budgets(), globals, FidelityEmission::always());
    });

    assert_eq!(
        info.field("rc_paid_probe_consumed_unauthorized_total"),
        Some("41"),
        "the unauthorized total carries the GLOBAL value -- 41 is held by no counter, \
         so a field wired to one instead is caught by the value",
    );
    assert_eq!(
        info.field("rc_usage_writer_degraded"),
        Some("true"),
        "and the writer-health field carries the global boolean rather than a default",
    );
}

// ---------------------------------------------------------------------------
// Presence at zero
// ---------------------------------------------------------------------------

/// Every count on the line is emitted even at zero.
///
/// An absent field on a status surface reads as an unavailable panel, not as
/// "nothing happened" -- and the states these report at zero are exactly the ones
/// an operator is debugging: a pre-flight that has not fired, a parser that has
/// seen nothing, a deployment with no configured cap.
#[test]
fn every_count_is_reported_even_at_zero() {
    let view = fresh_view();

    let info = captured_snapshot(|| {
        log_field_verdict_snapshot(
            &view,
            &no_budgets(),
            healthy_globals(),
            FidelityEmission::always(),
        );
    });

    assert_eq!(info.level, tracing::Level::INFO);
    for field in [
        "rc_field_repair_attempted_total",
        "rc_field_repair_succeeded_total",
        "rc_field_verdicts_learned_total",
        "rc_field_outstanding_unconfirmed_total",
        "rc_field_disproved_requests_total",
        "rc_field_preflight_actions_total",
        "rc_parser_unlocalized_total",
        "rc_acting_field_verdicts_total",
        "rc_acting_field_verdicts_omitted",
        "rc_field_verdict_rows_total",
        "rc_field_verdict_rows_omitted",
        "rc_paid_probe_budgets_total",
        "rc_paid_probe_budgets_omitted",
        "rc_paid_probe_consumed_unauthorized_total",
        "rc_probe_activations_total",
        "rc_probe_queued",
        "rc_probe_in_flight",
        "rc_probe_backing_off",
    ] {
        assert_eq!(
            info.field(field),
            Some("0"),
            "{field} is reported at zero rather than omitted",
        );
    }
    assert_eq!(
        info.field("rc_probe_last_settlement"),
        Some(PROBE_SETTLEMENT_NONE),
        "a scheduler nothing has settled on reports the explicit none token, not an \
         absent field -- a never-activated lane is a real state",
    );
    assert_eq!(
        info.field("rc_probe_next_retry_ms"),
        Some(NO_NEXT_RETRY.to_string().as_str()),
        "and nothing backing off reports the sentinel, which is distinct from a zero \
         wait (a backoff that already elapsed)",
    );
    assert_eq!(info.field("rc_usage_writer_degraded"), Some("false"));
}

// ---------------------------------------------------------------------------
// The planted verdict: a NON-VACUOUS row assertion
// ---------------------------------------------------------------------------

/// A resident acting verdict planted in the REAL router appears on the line as one
/// row carrying every required value.
///
/// THE non-vacuous row test, and the reason it plants rather than asserting on an
/// empty set: a projection that rendered nothing, or one wired to the wrong source,
/// passes an empty-set assertion. Here the row must be present AND carry each
/// field, so the same defects are red.
///
/// The verdict goes in through the router's own gated seam, which uses the same
/// `import_entries` and canary-seed paths a cold-boot ledger replay uses -- so this
/// is a state real traffic produces, not a shape only a test can make.
///
/// Mutation checks: render zero rows -> red on the count; drop any single rendered
/// field -> red on that field's assertion; source the rows from a fresh learned
/// read instead of the snapshot's -> the row is still present, which is why the
/// coherence property is pinned in the router crate instead.
#[test]
fn a_planted_resident_verdict_is_emitted_as_one_fully_populated_row() {
    let view = view_with_planted_verdict();

    let info = captured_snapshot(|| {
        log_field_verdict_snapshot(
            &view,
            &no_budgets(),
            healthy_globals(),
            FidelityEmission::always(),
        );
    });

    assert_eq!(
        info.field("rc_field_verdict_rows_total"),
        Some("1"),
        "the planted verdict is resident, so the surface reports exactly one row",
    );
    assert_eq!(
        info.field("rc_field_verdict_rows_omitted"),
        Some("0"),
        "and one row is well inside the render ceiling",
    );
    let rows = info
        .field("rc_field_verdict_rows")
        .expect("the rows field is emitted");
    for required in [
        // The identity: target and capability key.
        STATE_KEY,
        GROUNDED_PATH,
        // The class and its prefix-impact cost.
        "envelope",
        "prefix_impacting: false",
        // Provenance.
        "live",
        "f1",
        // The confirmation count BESIDE its required quorum -- neither is readable
        // as sufficient or short without the other.
        "confirmations: 1",
        "required_quorum: \"1\"",
        // Nothing is blocking: this verdict is eligible, which is the state in
        // which pre-flight rewrites traffic.
        "blocked_reason: \"none\"",
        // Canary position and its never-settled outcome.
        "canary: \"counting\"",
        "canary_last_outcome: \"none\"",
        // Both exposure counts.
        "requests_in_flight: 0",
        "unconfirmed_requests: 0",
    ] {
        assert!(
            rows.contains(required),
            "the emitted row must carry {required:?}, got {rows}",
        );
    }
    assert_eq!(
        info.field("rc_acting_field_verdicts_total"),
        Some("1"),
        "and the same verdict is reported as ACTING -- both derivations come from \
         one learned read, so they agree by construction rather than by luck",
    );
}

/// The acting row carries its identity too, not merely a count.
///
/// A count alone cannot tell an operator WHICH verdict is steering traffic, which
/// is the first thing they need. Asserted separately from the detailed row because
/// the two fields have different producers.
#[test]
fn the_acting_row_names_the_verdict_it_reports() {
    let view = view_with_planted_verdict();

    let info = captured_snapshot(|| {
        log_field_verdict_snapshot(
            &view,
            &no_budgets(),
            healthy_globals(),
            FidelityEmission::always(),
        );
    });

    let acting = info
        .field("rc_acting_field_verdicts")
        .expect("the acting field is emitted");
    assert!(
        acting.contains(GROUNDED_PATH) && acting.contains(STATE_KEY),
        "the acting entry names its capability key and its target, got {acting}",
    );
}

// ---------------------------------------------------------------------------
// One line per request
// ---------------------------------------------------------------------------

/// A build whose claim was already taken emits NO line.
///
/// The exactly-once half, expressed through the claim: a sibling in the same request
/// got there first. Emitting again would put two identical snapshots in the log per
/// poll, which makes a reader counting lines read double the poll rate while the two
/// lines carry different timestamps for one moment.
#[test]
fn a_build_whose_claim_is_taken_emits_no_line() {
    let view = fresh_view();
    let emission = FidelityEmission::shared();
    // A sibling claims it first.
    let events_first = routectl_testkit::capture_events(|| {
        log_field_verdict_snapshot(&view, &no_budgets(), healthy_globals(), emission.clone());
    });
    assert_eq!(
        events_first
            .iter()
            .filter(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE)
            .count(),
        1,
        "premise: the first builder took the claim",
    );

    let events = routectl_testkit::capture_events(|| {
        log_field_verdict_snapshot(&view, &no_budgets(), healthy_globals(), emission);
    });

    assert!(
        !events
            .iter()
            .any(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE),
        "the second builder finds the claim taken and emits nothing",
    );
}

/// And an emitting build emits exactly one.
///
/// The paired control: without it, an emitter that never emitted at all would
/// satisfy the suppression test.
#[test]
fn an_emitting_build_emits_exactly_one_line() {
    let view = fresh_view();

    let events = routectl_testkit::capture_events(|| {
        log_field_verdict_snapshot(
            &view,
            &no_budgets(),
            healthy_globals(),
            FidelityEmission::always(),
        );
    });

    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE)
            .count(),
        1,
    );
}

// ---------------------------------------------------------------------------
// Content discipline
// ---------------------------------------------------------------------------

/// Every closed-set count on the line renders as a bare non-negative integer.
///
/// A vocabulary check rather than a sentinel hunt, because the surface's contract
/// is POSITIVE: these fields are counts. A sentinel test can only refuse the leaks
/// it happened to imagine; this refuses anything that is not a count -- which is
/// the shape a value-carrying regression takes here.
#[test]
fn every_count_renders_as_a_bare_integer() {
    let view = view_with_planted_verdict();

    let info = captured_snapshot(|| {
        log_field_verdict_snapshot(
            &view,
            &no_budgets(),
            healthy_globals(),
            FidelityEmission::always(),
        );
    });

    for field in [
        "rc_field_repair_attempted_total",
        "rc_field_preflight_actions_total",
        "rc_parser_unlocalized_total",
        "rc_acting_field_verdicts_total",
        "rc_acting_field_verdicts_omitted",
        "rc_field_verdict_rows_total",
        "rc_field_verdict_rows_omitted",
        "rc_paid_probe_budgets_total",
        "rc_paid_probe_consumed_unauthorized_total",
        "rc_probe_queued",
        "rc_probe_in_flight",
        "rc_probe_backing_off",
    ] {
        let value = info
            .field(field)
            .unwrap_or_else(|| panic!("{field} must be emitted"));
        assert!(
            value.bytes().all(|b| b.is_ascii_digit()),
            "{field} must render as a bare count, got {value:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// The request-scoped claim
// ---------------------------------------------------------------------------

/// Two builders sharing one claim emit exactly ONE line between them.
///
/// The aggregate's contract: both panels carry this surface, and a request must
/// produce one snapshot rather than two identical ones with different timestamps.
#[test]
fn two_builders_sharing_a_claim_emit_exactly_one_line() {
    let view = fresh_view();
    let emission = FidelityEmission::shared();

    let events = routectl_testkit::capture_events(|| {
        log_field_verdict_snapshot(&view, &no_budgets(), healthy_globals(), emission.clone());
        log_field_verdict_snapshot(&view, &no_budgets(), healthy_globals(), emission);
    });

    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE)
            .count(),
        1,
        "the claim is one-shot per request, so the second builder finds it taken",
    );
}

/// When the FIRST builder never reaches the logger, the second one still emits.
///
/// THE reason this is a claim rather than a pre-assigned role. Each panel is built
/// through `guard_panel`, which degrades a failing data source to an unavailable
/// panel -- and an unavailable build never reaches its logger. Under the previous
/// pre-assigned shape a failing health panel took the whole observability floor down
/// with it for that request, silently, while the doctor panel that built fine stayed
/// designated-suppressed.
///
/// Modeled by simply not calling the first builder's logger, which is exactly what a
/// degraded build does.
///
/// Mutation check: pre-assign the roles again (first caller emits, second is told to
/// suppress) -> red here, zero lines.
#[test]
fn a_builder_that_never_reaches_the_logger_leaves_the_claim_for_its_sibling() {
    let view = fresh_view();
    let emission = FidelityEmission::shared();
    // The first builder's clone is taken and DROPPED unused: its panel degraded, so
    // it never reached the logger.
    let degraded = emission.clone();
    drop(degraded);

    let events = routectl_testkit::capture_events(|| {
        log_field_verdict_snapshot(&view, &no_budgets(), healthy_globals(), emission);
    });

    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE)
            .count(),
        1,
        "a failing sibling must not remove the observability floor from the request: \
         the surviving builder claims the line",
    );
}

/// A standalone builder always emits, with no claim to arbitrate.
///
/// The paired control for both cases above: without it, a claim that never granted
/// would satisfy the exactly-once assertion while emitting nothing anywhere.
#[test]
fn a_standalone_builder_always_emits() {
    let view = fresh_view();

    let events = routectl_testkit::capture_events(|| {
        log_field_verdict_snapshot(
            &view,
            &no_budgets(),
            healthy_globals(),
            FidelityEmission::always(),
        );
    });

    assert_eq!(
        events
            .iter()
            .filter(|e| e.message == FIDELITY_SNAPSHOT_MESSAGE)
            .count(),
        1,
    );
}

// ---------------------------------------------------------------------------
// Per-call event observer
// ---------------------------------------------------------------------------

/// The per-call observer UNIT test, proving the emitter boundary directly.
///
/// Driven at the emitter function rather than through the daemon, because the
/// observer is installed on the `FidelityEmission` the builder uses and the daemon's
/// own builds construct their own. What this covers that no daemon test can: the
/// exact event value the emitter produces, and the exactly-one guarantee through the
/// claim.
#[test]
fn the_emitter_sends_one_populated_event_through_the_per_call_observer() {
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://K"),
    );
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        "sonnet".to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert(
        "default".to_string(),
        AliasValue::Single("sonnet".to_string()),
    );
    let router = Router::new(std::sync::Arc::new(Config {
        providers,
        aliases,
        models,
        ..Config::default()
    }));
    plant_acting_field_verdict_for_tests(&router, "sonnet", "thinking.enabled.display", 1);
    let view = StatusRouterHandle::new(std::sync::Arc::new(ArcSwap::from_pointee(router))).view();
    let globals = AccountingGlobals {
        writer_degraded: false,
        consumed_unauthorized_total: 0,
    };

    let mut emission = FidelityEmission::always();
    let mut rx = emission.observe();

    log_field_verdict_snapshot(&view, &[], globals, emission);

    let event = rx.try_recv().expect("the emitter sent exactly one event");
    assert_eq!(
        event.verdict_rows_total, 1,
        "the event carries the planted verdict row count",
    );
    assert!(
        !event.snapshot.acting.is_empty(),
        "and the snapshot inside it is populated -- this is what the INFO line would \
         carry",
    );
}

/// A SUPPRESSED builder (claim already taken) sends NO event.
///
/// The exactly-once half, verified through the observer: the second builder finds
/// the claim taken and does not reach the emitter, so no event is sent.
#[test]
fn a_suppressed_builder_sends_no_event_to_the_observer() {
    let router = Router::new(std::sync::Arc::new(Config::default()));
    let view = StatusRouterHandle::new(std::sync::Arc::new(ArcSwap::from_pointee(router))).view();
    let globals = AccountingGlobals {
        writer_degraded: false,
        consumed_unauthorized_total: 0,
    };

    let mut emission = FidelityEmission::shared();
    let mut rx = emission.observe();
    // First builder takes the claim.
    log_field_verdict_snapshot(&view, &[], globals, emission.clone());
    let first = rx.try_recv();
    assert!(first.is_ok(), "the first builder sent an event");

    // Second builder: claim is taken.
    let mut emission2 = emission;
    let mut rx2 = emission2.observe();
    log_field_verdict_snapshot(&view, &[], globals, emission2);
    assert!(
        rx2.try_recv().is_err(),
        "the second builder found the claim taken and sent nothing",
    );
}

/// A HEALTH-FAILURE FALLBACK: when health's builder does not reach the emitter, the
/// shared claim stays free and doctor's builder sends the event through the observer.
///
/// This exercises the real failure path: the claim is shared, health never calls the
/// emitter (it degraded), and doctor is the builder that takes the claim. What makes
/// it non-vacuous is that the observer receives a populated event from the SECOND
/// builder -- a test that inferred fallback from "one event was emitted" could not
/// tell which builder produced it.
#[test]
fn health_failure_fallback_lets_doctor_emit_through_the_observer() {
    let router = fixture_router();
    routectl_router::plant_acting_field_verdict_for_tests(&router, STATE_KEY, GROUNDED_PATH, 1);
    let view = view_over(router);
    let globals = AccountingGlobals {
        writer_degraded: false,
        consumed_unauthorized_total: 0,
    };

    let mut emission = FidelityEmission::shared();
    let mut rx = emission.observe();

    // Health's builder DOES NOT CALL the emitter: it degraded to unavailable, so it
    // never reached the fidelity read. Modeled by simply not calling it, which is
    // exactly what a degraded guard_panel produces.
    let _ = emission.clone(); // health took a clone and dropped it

    // Doctor's builder: it reaches the emitter and finds the claim free.
    log_field_verdict_snapshot(&view, &no_budgets(), globals, emission);

    let event = rx.try_recv().expect(
        "the doctor builder sent the event: it found the claim free because \
                 health never reached the emitter",
    );
    assert!(
        event.verdict_rows_total > 0,
        "and the event is POPULATED: the planted verdict row is present, so the line \
         carries real data rather than an empty snapshot that passes vacuously",
    );
}

// ---------------------------------------------------------------------------
// Release-path allocation shape
// ---------------------------------------------------------------------------

/// The emitter's PRODUCTION region constructs no `FidelityEvent` and clones no
/// snapshot: both live only inside the `cfg(test)` observer function.
///
/// # Why this is a source guard rather than a behavioral one
///
/// The property is about what a RELEASE build allocates, and no test can observe
/// that from inside a test build -- the very `cfg(test)` code the guard is about is
/// compiled in here. What can be checked is the structural fact that produces it:
/// the construction and the clone appear in this file only below the `cfg(test)`
/// gate. A counting or timing assertion would measure the test build and say nothing
/// about the shipped one.
///
/// The regression it pins is a real one that shipped in this file: the event was
/// built unconditionally in the emitter body, so every status poll of every daemon
/// deep-cloned a `FidelitySnapshot` -- whose verdict and acting vectors grow with the
/// (target, field) identities a deployment has learned, unbounded and traffic-driven
/// -- for a consumer no release build has, on a surface polled every few seconds.
///
/// The cut is the shared `production_source` helper, which keys on the `mod tests {`
/// opener and refuses an ambiguous one. This module's tests are a SIDECAR
/// (`#[path] mod tests;`), so its body is not in this file's text at all and the
/// whole emitter source is scanned -- which is why the scan below must exclude the
/// gated function by name rather than relying on the cut.
#[test]
fn the_emitter_allocates_no_observer_event_outside_a_test_build() {
    const EMITTER_SRC: &str = include_str!("field_verdict_log.rs");

    let production = super::super::production_source::production_source(EMITTER_SRC);
    // Everything from the `cfg(test)` observer helper onward is test-only by
    // construction; the region ABOVE it is what a release build compiles.
    let gate = "#[cfg(test)]\nfn emit_observer_event(";
    let split = production.find(gate).expect(
        "the gated observer helper must be present and spelled exactly like this, or \
         this guard is scanning a file it no longer understands",
    );
    let release_region = &production[..split];

    // The ASSIGNMENT form, not the bare type name: the struct DECLARATION also reads
    // `FidelityEvent {`, and matching it would fail this guard for the mere existence
    // of the type rather than for an allocation on the production path.
    assert!(
        !release_region.contains("= FidelityEvent {"),
        "the emitter's release region must construct no FidelityEvent: the event owns \
         a cloned FidelitySnapshot whose size grows with the identities a deployment \
         has learned, and building it on the production path makes every status poll \
         pay for a deep copy no release build can observe",
    );
    assert!(
        !release_region.contains("snapshot.clone()"),
        "and it must clone no snapshot there, for the same reason -- the ONE clone \
         lives inside the cfg(test) helper",
    );
    // POSITIVE CONTROL: the strings the assertions above look for are really present
    // in this file, below the gate. Without this the two checks would pass just as
    // well against a rename that put the allocation somewhere they cannot see.
    let gated_region = &production[split..];
    assert!(
        gated_region.contains("= FidelityEvent {") && gated_region.contains("snapshot.clone()"),
        "control: the construction and the clone DO exist in this file, inside the \
         gated helper -- so the assertions above are locating a real allocation rather \
         than matching nothing at all",
    );
}
