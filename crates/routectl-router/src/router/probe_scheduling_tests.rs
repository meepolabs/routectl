//! Router-owned lazy probe activation: no probe work at startup, install,
//! config parse, or reload, and no probe work on a status read.

use std::sync::Arc;
use std::time::Instant;

use routectl_core::ChatRequest;

use super::Router;
use super::probe_lifecycle::PROBE_ACTIVATION_REFUSED_EVENT;
use super::probe_test_support::{GROUNDED_PATH, grounding_request};
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{PROBE_QUEUE_DEPTH, ProbeActivation, ProbeValidator};

/// A router over one anthropic-api provider reachable through an alias --
/// the shape a real admitted request dispatches against.
fn router() -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.models.insert(
        "sonnet".to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single("sonnet".to_string()),
    );
    Router::new(Arc::new(config))
}

fn key(n: usize) -> FieldVerdictKey {
    FieldVerdictKey::new(
        &format!("anthropic:model-{n}"),
        GROUNDED_PATH,
        "anthropic-api",
    )
    .expect("the grounded path mints an identity on this lane")
}

#[test]
fn constructing_a_router_schedules_no_probe_work() {
    // Arrange / Act: construction is the whole of startup, install, and
    // config parsing as far as this crate is concerned.
    let router = router();

    // Assert
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.queued, 0, "startup must perform zero probe calls");
    assert_eq!(snap.in_flight, 0);
    assert_eq!(snap.activations_total, 0);
}

#[test]
fn a_reload_carry_over_schedules_no_probe_work() {
    // Arrange
    let previous = router();
    let mut next = router();

    // Act: the hot-reload coordinator's carry-over, with no traffic.
    next.carry_over_learned_from(&previous);

    // Assert
    let snap = next.probe_scheduler_snapshot();
    assert_eq!(snap.queued, 0, "reload must perform zero probe calls");
    assert_eq!(snap.activations_total, 0);
}

#[test]
fn the_first_admitted_request_activates_the_lane_once() {
    // Arrange
    let router = router();

    // Act: three admitted requests on the same lane and capability.
    let first = router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);
    let second = router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);
    let third = router.activate_probe_lane(&key(0), ProbeValidator::ExpectedRejection);

    // Assert
    assert_eq!(first, ProbeActivation::Queued);
    assert_eq!(second, ProbeActivation::Deduped);
    assert_eq!(third, ProbeActivation::Deduped);
    assert_eq!(router.probe_scheduler_snapshot().queued, 1);
}

#[test]
fn activation_is_refused_while_the_capability_kill_switch_is_off() {
    // Arrange: the operator's learned-capability kill switch is the same
    // switch that governs every other learned-verdict arm.
    let mut config = Config::default();
    config.capability.enabled = false;
    let router = Router::new(Arc::new(config));

    // Act
    let outcome = router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);

    // Assert
    assert_eq!(outcome, ProbeActivation::Retired);
    assert_eq!(router.probe_scheduler_snapshot().queued, 0);
}

#[test]
fn a_reload_carries_the_scheduler_across_rather_than_dropping_its_work() {
    // Arrange: work activated against the outgoing router.
    let previous = router();
    previous.activate_probe_lane(&key(0), ProbeValidator::CountTokens);
    let mut next = router();

    // Act
    next.carry_over_learned_from(&previous);

    // Assert: an in-flight probe settles against the SAME table the
    // replacement leases from, so the job is visible on both.
    assert_eq!(
        next.probe_scheduler_snapshot().queued,
        1,
        "a reload must not silently drop queued probe work"
    );
    assert_eq!(
        next.activate_probe_lane(&key(0), ProbeValidator::CountTokens),
        ProbeActivation::Deduped,
        "the carried job still holds its lane/capability slot"
    );
}

#[test]
fn shutdown_cancels_every_queued_probe() {
    // Arrange
    let router = router();
    router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);
    router.activate_probe_lane(&key(1), ProbeValidator::CountTokens);

    // Act
    let cancelled = router.shutdown_probe_work();

    // Assert
    assert_eq!(cancelled, 2);
    assert_eq!(router.probe_scheduler_snapshot().queued, 0);
}

#[test]
fn reading_the_probe_snapshot_activates_nothing() {
    // Arrange: status reads are pure -- they never activate a lane.
    let router = router();

    // Act
    for _ in 0..5 {
        let _ = router.probe_scheduler_snapshot();
    }

    // Assert
    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[test]
fn a_leased_job_carries_the_identity_and_validator_it_was_activated_for() {
    // Arrange
    let router = router();
    let now = Instant::now();
    router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);

    // Act
    let lease = router.lease_due_probe(now).expect("the queued job is due");

    // Assert
    assert_eq!(lease.key(), &key(0));
    assert_eq!(lease.validator(), ProbeValidator::CountTokens);
    assert_eq!(
        lease.generation(),
        router.probe_incarnation(),
        "a lease is stamped with the scheduler incarnation that activated it"
    );
}

#[test]
fn retirement_cancels_probe_work_from_a_superseded_generation() {
    // Arrange: work activated at the current generation.
    let router = router();
    router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);

    // Act: the generation advances past it and the router retires.
    let cancelled = router.publish_probe_incarnation();

    // Assert
    assert_eq!(cancelled, 1);
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.queued, 0, "no work survives on retired state");
    assert_eq!(snap.retired_total, 1);
}

#[test]
fn work_activated_after_a_publication_survives_it() {
    // Publication always advances the incarnation, so the property that
    // holds is not "retiring twice is a no-op" but "work belonging to the
    // CURRENT incarnation is untouched by the publication that created it".
    let router = router();
    router.publish_probe_incarnation();
    router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);

    assert_eq!(router.probe_scheduler_snapshot().queued, 1);
    assert!(
        router.lease_due_probe(Instant::now()).is_some(),
        "live-incarnation work must remain leasable"
    );
}

#[test]
fn a_queue_full_refusal_emits_a_bounded_closed_set_diagnostic() {
    // Arrange: saturate the queue, then one more.
    let router = router();
    for n in 0..PROBE_QUEUE_DEPTH {
        router.activate_probe_lane(&key(n), ProbeValidator::CountTokens);
    }

    // Act
    let events = routectl_testkit::capture_events(|| {
        let outcome =
            router.activate_probe_lane(&key(PROBE_QUEUE_DEPTH), ProbeValidator::CountTokens);
        assert_eq!(outcome, ProbeActivation::QueueFull);
    });

    // Assert: exactly one line, carrying closed-set tokens and counters
    // only -- no request bytes, no upstream text, no capability key.
    let refusals: Vec<_> = events
        .iter()
        .filter(|e| e.message == PROBE_ACTIVATION_REFUSED_EVENT)
        .collect();
    assert_eq!(refusals.len(), 1, "one bounded line per refusal");
    let event = refusals[0];
    assert_eq!(event.field("outcome"), Some("queue_full"));
    assert_eq!(
        event.field("queued"),
        Some(PROBE_QUEUE_DEPTH.to_string().as_str())
    );
    assert_eq!(event.field("queue_full_total"), Some("1"));
}

#[test]
fn a_deduped_activation_emits_no_diagnostic() {
    // Arrange: dedupe is the common case on every request after the
    // first, so it must not log per request.
    let router = router();
    router.activate_probe_lane(&key(0), ProbeValidator::CountTokens);

    // Act
    let events = routectl_testkit::capture_events(|| {
        for _ in 0..50 {
            assert_eq!(
                router.activate_probe_lane(&key(0), ProbeValidator::CountTokens),
                ProbeActivation::Deduped
            );
        }
    });

    // Assert
    assert!(
        !events
            .iter()
            .any(|e| e.message == PROBE_ACTIVATION_REFUSED_EVENT),
        "the steady-state dedupe path must emit no per-request line"
    );
}

#[test]
fn a_successful_activation_emits_no_refusal_diagnostic() {
    // Positive control for the two negative assertions above: the fixture
    // provably reaches the emitting call, and the refusal line has a
    // distinct producer, so their silence is about the outcome and not
    // about the capture never seeing this module at all.
    let router = router();

    // Act
    let events = routectl_testkit::capture_events(|| {
        assert_eq!(
            router.activate_probe_lane(&key(0), ProbeValidator::CountTokens),
            ProbeActivation::Queued
        );
    });

    // Assert
    assert!(
        !events
            .iter()
            .any(|e| e.message == PROBE_ACTIVATION_REFUSED_EVENT)
    );
}

#[test]
fn an_admitted_request_grounding_the_closed_table_activates_its_lane() {
    // Arrange
    let router = router();
    let req = grounding_request();

    // Act
    router.activate_probe_lanes_for_admitted_request(
        "anthropic:sonnet",
        "anthropic",
        Some("anthropic-api"),
        false,
        &req,
    );

    // Assert
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.activations_total, 1);
    assert_eq!(snap.queued, 1);
}

#[test]
fn a_burst_of_admitted_requests_on_one_lane_activates_exactly_once() {
    // Arrange
    let router = router();
    let req = grounding_request();

    // Act: the shape real traffic takes -- many requests, one lane.
    for _ in 0..25 {
        router.activate_probe_lanes_for_admitted_request(
            "anthropic:sonnet",
            "anthropic",
            Some("anthropic-api"),
            false,
            &req,
        );
    }

    // Assert
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.activations_total, 1, "one job per lane/capability");
    assert_eq!(snap.deduped_total, 24);
}

#[test]
fn an_admitted_request_grounding_nothing_activates_no_lane() {
    // Arrange: no closed-table surface on the request, so there is no
    // capability identity to probe.
    let router = router();

    // Act
    router.activate_probe_lanes_for_admitted_request(
        "anthropic:sonnet",
        "anthropic",
        Some("anthropic-api"),
        false,
        &ChatRequest::default(),
    );

    // Assert
    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[test]
fn a_forwarded_credential_target_activates_no_lane() {
    // Arrange: a forwarded target authenticates with the CLIENT's bearer,
    // so routectl may neither mint nor probe a verdict for it -- the same
    // refusal the reactive repair arm applies.
    let router = router();

    // Act
    router.activate_probe_lanes_for_admitted_request(
        "anthropic:sonnet",
        "anthropic",
        Some("anthropic-api"),
        true,
        &grounding_request(),
    );

    // Assert
    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[test]
fn a_target_off_the_acting_lane_activates_no_lane() {
    // Arrange: only the one lane this stage acts on may probe.
    let router = router();

    // Act
    for kind in [None, Some("openai-compat"), Some("bedrock")] {
        router.activate_probe_lanes_for_admitted_request(
            "other:model",
            "anthropic",
            kind,
            false,
            &grounding_request(),
        );
    }

    // Assert
    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[test]
fn an_admitted_request_activates_a_free_validator_first() {
    // Arrange
    let router = router();
    let now = Instant::now();

    // Act
    router.activate_probe_lanes_for_admitted_request(
        "anthropic:sonnet",
        "anthropic",
        Some("anthropic-api"),
        false,
        &grounding_request(),
    );
    let lease = router.lease_due_probe(now).expect("the queued job is due");

    // Assert: no paid call may be the first thing a lane does.
    assert!(
        lease.validator().is_free(),
        "activation queued a paid validator: {:?}",
        lease.validator()
    );
}

#[test]
fn a_body_an_acting_preflight_already_stripped_activates_no_lane() {
    // The two arms COMPOSE on the same dispatch walk: the pre-flight planner
    // runs first and, when it acts, hands the walk a body with the closed-table
    // surface removed. Activation reads that same per-target body, so it must
    // see no grounded field and queue nothing -- a probe asking about a field
    // this request no longer sends would produce evidence about bytes the
    // upstream never saw. Nothing is lost by declining: pre-flight only acts on
    // an ALREADY-eligible verdict, so there is no open question to probe.
    let router = router();
    // DERIVED by running the real transforms, not by hand-clearing the
    // carriers. A hand-written "stripped" body is a claim about what
    // `drop_from` removes, and the claim rots the moment the surface grows a
    // third carrier: the fixture would still look stripped while the real
    // transform left a carrier behind, and this test would pass on a body
    // activation should have refused. Every present row is dropped rather
    // than only the first, so the premise holds as the closed table grows.
    let mut req = grounding_request();
    let rows: Vec<_> = super::field_repair::present_rows(&req).collect();
    assert!(
        !rows.is_empty(),
        "premise: the fixture grounds at least one closed-table surface"
    );
    for row in rows {
        assert!(
            row.surface.drop_from(&mut req),
            "premise: the transform must report it removed the surface"
        );
        assert!(
            !row.surface.present_in(&req),
            "premise: this is the post-condition the pre-flight planner enforces \
             before adopting a transformed body"
        );
    }

    // Act
    router.activate_probe_lanes_for_admitted_request(
        "anthropic:sonnet",
        "anthropic",
        Some("anthropic-api"),
        false,
        &req,
    );

    // Assert. The positive control is
    // `an_admitted_request_grounding_the_closed_table_activates_its_lane`:
    // the SAME call on the un-stripped fixture activates exactly one lane, so
    // this silence is about the stripped body rather than an inert fixture.
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.activations_total, 0,
        "a stripped body grounds no closed-table surface"
    );
    assert_eq!(snap.queued, 0);
}
