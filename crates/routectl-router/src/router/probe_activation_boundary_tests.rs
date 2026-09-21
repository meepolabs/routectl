//! Where a lane activates, and where it must not.
//!
//! The boundary under test is ADMITTED traffic: past the gate, every
//! request-shaping step run, about to dial. Each test names that boundary
//! rather than the function it calls.
//!
//! Every negative assertion here is paired with a positive control on the
//! same fixture -- `an_admitted_anthropic_target_on_a_remote_base_activates`
//! proves the fixture DOES activate once the refused condition is removed.
//! Without that pairing a refusal test passes on any router whose activation
//! is broken outright.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use routectl_core::{ChatRequest, Provider};

use super::Router;
use super::probe_test_support::{
    FailingProvider, GROUNDED_PATH, OkProvider, grounding_request, plant_eligible_verdict,
    remote_router, router_on_base, router_on_base_with_failure_threshold,
};
use crate::config::Config;
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbeValidator;

#[tokio::test]
async fn an_acting_preflight_strips_before_activation_on_a_real_dispatch_walk() {
    // THE composition, end to end through `complete_with_options` rather than
    // by calling the activation seam directly. The two arms meet on one walk:
    // the pre-flight planner rewrites the per-target body first, and activation
    // reads THAT body. So an eligible verdict strips the closed-table surface
    // and the lane must activate nothing -- a probe asking about a field this
    // request no longer sends would produce evidence about bytes the upstream
    // never saw.
    //
    // A source-text guard already pins that each call site passes
    // `attempt_req`. It cannot pin ORDER: moving activation above
    // `plan_field_preflight`, or handing it a clone taken before the rewrite,
    // still passes the per-target variable and still reads the guard as
    // satisfied. Only driving the real walk distinguishes those.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);
    plant_eligible_verdict(&router, "m1");

    let dispatched = router
        .complete_with_options(grounding_request(), Default::default())
        .await;

    // THE PREMISE, asserted rather than assumed: the pre-flight planner must
    // actually have ACTED on this walk. A fixture whose planting silently
    // failed, or a planner that declined for any of its own gates, would leave
    // the surface intact -- and then zero activations would be measuring a
    // broken activation path while claiming to measure the composition.
    assert!(
        dispatched
            .meta
            .field_preflight
            .iter()
            .any(|record| record.acted),
        "premise: the pre-flight planner must have acted, or zero activations \
         proves nothing about the composition: {:?}",
        dispatched.meta.field_preflight
    );

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.activations_total, 0,
        "an acting pre-flight strips the surface, so the walk grounds no lane"
    );
    assert_eq!(snap.queued, 0);
}

#[tokio::test]
async fn the_same_dispatch_walk_without_an_eligible_verdict_activates_exactly_one_lane() {
    // The POSITIVE CONTROL for the test above, on the same walk and the same
    // fixture with the ONE difference that no verdict is planted: nothing
    // strips, so the surface survives to activation and exactly one lane
    // enters the queue. Without this, the silence above would pass on a router
    // whose activation is broken outright.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);

    let _ = router
        .complete_with_options(grounding_request(), Default::default())
        .await;

    assert_eq!(
        router.probe_scheduler_snapshot().activations_total,
        1,
        "with nothing stripping the surface, the walk activates its lane"
    );
}

#[tokio::test]
async fn an_upstream_failure_still_activates_the_lane() {
    // The boundary is ADMITTED (past the gate, about to dial), not
    // SUCCEEDED: a lane whose upstream is erroring is exactly the lane a
    // probe should investigate, so gating activation on a 2xx would keep
    // the scheduler dark on the traffic that most needs it.
    let provider = Arc::new(FailingProvider {
        status: 500,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());

    let _ = router.complete(grounding_request()).await;

    assert!(
        provider.calls.load(Ordering::SeqCst) > 0,
        "fixture must actually reach the upstream"
    );
    assert_eq!(
        router.probe_scheduler_snapshot().activations_total,
        1,
        "an admitted request whose upstream failed must still activate"
    );
}

#[tokio::test]
async fn a_gate_refused_request_activates_nothing() {
    // The converse boundary: a request the runtime gate refuses never
    // reaches the outbound call, so it is not admitted traffic and must
    // not mark the lane in use.
    let provider = Arc::new(FailingProvider {
        status: 500,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    router.force_open_breaker("m1", Duration::from_hours(1));

    let _ = router.complete(grounding_request()).await;

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "the gate must have refused before any dial"
    );
    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[tokio::test]
async fn the_streaming_walk_activates_at_the_same_boundary() {
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);

    let _ = router.stream(grounding_request()).await;

    assert_eq!(router.probe_scheduler_snapshot().activations_total, 1);
}

#[tokio::test]
async fn the_count_tokens_walk_activates_the_lane() {
    // count_tokens is a real admitted request on this lane and its own
    // free validator class, so it must activate like the other two walks.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());

    let _ = router.count_tokens(grounding_request()).await;

    assert!(provider.count_calls.load(Ordering::SeqCst) > 0);
    assert_eq!(router.probe_scheduler_snapshot().activations_total, 1);
}

#[tokio::test]
async fn all_three_walks_share_one_lane_slot() {
    // The three surfaces resolve to the same lane and capability, so a
    // session mixing them queues exactly one job.
    //
    // Breaker DISABLED (`None`) on purpose: this test's subject is the dedupe
    // count across walks, and `OkProvider::stream` returns a stream whose
    // first-content failure debits the lane. With a threshold of 1 that trip
    // would refuse the third walk's gate and the test would measure breaker
    // behavior instead of dedupe.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base_with_failure_threshold(
        "https://api.anthropic.com",
        provider as Arc<dyn Provider>,
        None,
    );

    let _ = router.complete(grounding_request()).await;
    let _ = router.stream(grounding_request()).await;
    let _ = router.count_tokens(grounding_request()).await;

    let snap = router.probe_scheduler_snapshot();
    assert_eq!(snap.activations_total, 1);
    assert_eq!(snap.deduped_total, 2);
}

/// Every activation call site in the dispatch and count_tokens walks,
/// read out of the production source.
///
/// A SOURCE-TEXT guard, and the reason is that the overlay set gives
/// no behavioral discriminator: `apply_layered_overlays` rebuilds
/// `routectl_internal` from default and carries the closed-table carrier
/// across verbatim, and `payload_extras` merges into `provider_extras`
/// rather than the carrier, so on every fixture available here the
/// ingress and per-target requests ground the SAME path. A behavioral
/// test would therefore pass on both inputs -- it could not fail if the
/// wrong one were passed, which is the definition of a vacuous check.
/// This guard CAN fail: swapping any call site back to the ingress
/// request turns it red immediately.
fn activation_call_site_arguments() -> Vec<String> {
    const DISPATCH: &str = include_str!("dispatch.rs");
    const COUNT_TOKENS: &str = include_str!("count_tokens.rs");
    const CALL: &str = "self.on_admitted_request(";
    let mut args = Vec::new();
    for source in [DISPATCH, COUNT_TOKENS] {
        // Scanned WHOLE deliberately: both files declare their tests as
        // `#[path]` sidecars, so no test text lives in them and there is
        // nothing to cut -- a `#[cfg(test)]` split would truncate at the
        // sidecar declaration and hide the very call sites this reads.
        for (_, rest) in source.match_indices(CALL).map(|(i, _)| (i, &source[i..])) {
            let open = rest.find('(').expect("the matched call has an open paren");
            let close = rest.find(')').expect("a single-line call site");
            args.push(rest[open + 1..close].to_string());
        }
    }
    args
}

#[test]
fn every_activation_call_site_passes_the_per_target_request() {
    let args = activation_call_site_arguments();

    assert_eq!(
        args.len(),
        3,
        "expected the complete, stream, and count_tokens call sites, found {args:?}"
    );
    // NAME-based, and that is the limit of what this guard proves: any binding
    // whose name contains `attempt_req` satisfies it, including a clone taken
    // BEFORE the pre-flight rewrite (`attempt_req_before_preflight`). Measured --
    // that mutation passes here. What closes the gap is the end-to-end
    // composition test
    // `an_acting_preflight_strips_before_activation_on_a_real_dispatch_walk`,
    // which drives the real walk and reds on exactly that clone. This guard's job
    // is narrower: catch a site wired to the INGRESS request, which no behavioral
    // fixture here discriminates (the overlay set gives no observable difference).
    for arg in &args {
        assert!(
            arg.contains("attempt_req"),
            "an activation call site passes something other than the per-target \
             request, which would probe bytes no upstream sees: {arg}"
        );
        assert!(
            !arg.contains("&req,") && !arg.contains("&req)"),
            "an activation call site passes the ingress request: {arg}"
        );
    }
}

// ---------------------------------------------------------------------
// Refusals, mirroring the reactive arm entry-for-entry
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_admitted_anthropic_target_on_a_remote_base_activates() {
    // POSITIVE CONTROL for every refusal test below: the same fixture,
    // with none of the refused conditions present, DOES activate. Without
    // this the refusals could all pass on a router that activates nothing.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);

    let _ = router.complete(grounding_request()).await;

    assert_eq!(router.probe_scheduler_snapshot().activations_total, 1);
}

#[tokio::test]
async fn a_loopback_target_activates_no_lane() {
    // A local hop's rejection is not attributable to the wire format it
    // was configured with, so it may mint no verdict -- and therefore
    // must not be probed. Same predicate the reactive arm uses.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base("http://127.0.0.1:8899", provider);

    let _ = router.complete(grounding_request()).await;

    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[tokio::test]
async fn a_local_hostname_target_activates_no_lane() {
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = router_on_base("http://localhost:8899", provider);

    let _ = router.complete(grounding_request()).await;

    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[test]
fn a_target_whose_base_url_is_not_attributable_activates_no_lane() {
    // A provider entry that yields NO anthropic base url -- the Bedrock
    // Mantle shape is exactly this case: the entry reads `anthropic-api`
    // but authenticates and egresses through Mantle, so the configured
    // base names nothing a rejection can be attributed to. The reactive
    // arm refuses on the same `None`, and this refusal must be reached
    // through the SAME accessor rather than a second base-url read.
    let router = Router::new(Arc::new(Config::default()));

    // No `[providers.p1]` block exists, so the accessor answers None.
    router.activate_probe_lanes_for_admitted_request(
        "m1",
        "p1",
        Some("anthropic-api"),
        false,
        &grounding_request(),
    );

    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

#[test]
fn a_request_grounding_nothing_activates_no_lane() {
    // Converse of the fixture premise: proves the grounding request is
    // what drives every activation above, so none of them is passing on
    // an unconditional activate.
    let provider: Arc<dyn Provider> = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider);

    router.activate_probe_lanes_for_admitted_request(
        "m1",
        "p1",
        Some("anthropic-api"),
        false,
        &ChatRequest::default(),
    );

    assert_eq!(router.probe_scheduler_snapshot().activations_total, 0);
}

// ---------------------------------------------------------------------
// Scheduler incarnation, reload retirement, and shutdown
// ---------------------------------------------------------------------

#[test]
fn the_scheduler_incarnation_advances_on_every_publication() {
    // The registry generation only moves on a catalog/overlay boundary, so
    // it cannot express "this router was republished". A scheduler-owned
    // incarnation must advance on EVERY successful publication.
    let router = Router::new(Arc::new(Config::default()));
    let first = router.probe_incarnation();

    router.publish_probe_incarnation();
    let second = router.probe_incarnation();
    router.publish_probe_incarnation();
    let third = router.probe_incarnation();

    assert!(second > first, "publication must advance the incarnation");
    assert!(
        third > second,
        "every publication advances it, not just one"
    );
}

#[test]
fn a_reload_publication_retires_the_previous_incarnation_work() {
    let previous = Router::new(Arc::new(Config::default()));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    previous.activate_probe_lane(&key, ProbeValidator::CountTokens);
    assert_eq!(previous.probe_scheduler_snapshot().queued, 1);

    let mut next = Router::new(Arc::new(Config::default()));
    next.carry_over_learned_from(&previous);
    let cancelled = next.publish_probe_incarnation();

    assert_eq!(cancelled, 1, "reload publication retires the old work");
    let snap = next.probe_scheduler_snapshot();
    assert_eq!(snap.queued, 0);
    assert_eq!(snap.retired_total, 1);
}

#[test]
fn work_from_a_retired_incarnation_can_neither_lease_nor_retry() {
    let previous = Router::new(Arc::new(Config::default()));
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    previous.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let mut next = Router::new(Arc::new(Config::default()));
    next.carry_over_learned_from(&previous);
    next.publish_probe_incarnation();

    assert!(
        next.lease_due_probe(std::time::Instant::now()).is_none(),
        "retired work must not lease"
    );
    // And a fresh activation from the retired incarnation is refused.
    assert_eq!(
        previous.activate_probe_lane(&key, ProbeValidator::CountTokens),
        crate::probe_scheduler::ProbeActivation::Retired
    );
    assert_eq!(next.probe_scheduler_snapshot().queued, 0);
}

#[test]
fn shutdown_cancels_all_probe_work() {
    let router = Router::new(Arc::new(Config::default()));
    for n in 0..3 {
        let key = FieldVerdictKey::new(&format!("m{n}"), GROUNDED_PATH, "anthropic-api")
            .expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    }

    let cancelled = router.shutdown_probe_work();

    assert_eq!(cancelled, 3);
    assert_eq!(router.probe_scheduler_snapshot().queued, 0);
    assert!(router.lease_due_probe(std::time::Instant::now()).is_none());
}
