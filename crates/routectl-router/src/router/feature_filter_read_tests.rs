//! The feature filter consults the learned registry through ONE guarded read
//! per feature key per target, and that read alone decides strip, route-away,
//! probe admission and the verified-positive mask on a catalog prior.

use super::super::generation_barrier_tests::superseded_pair_with;
use super::capability_precedence_matrix_tests::{
    KIND, base_config, lane_of, seed_learned, seed_verified, target_with_priors,
};
use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

use crate::learned_capability::{BoundaryCut, EntryVerdict, ExportedEntry};

/// A lapsed learned negative: `expires_at == now`, which the filter's later
/// `Instant::now()` reads as expired, so its re-probe slot is claimable.
fn seed_lapsed(router: &Router, nickname: &str, feature: &str) {
    let base = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: KIND.into(),
            state_key: lane_of(nickname),
            feature_key: normalize_capability_key(feature, KIND),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base,
            evidence_class: None,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);
}

/// Seed one feature in each learned shape on `nickname`'s lane: an F1
/// strippable negative (`advisor`), a route-away negative (`web_search`)
/// when `with_route_away`, a verified positive (`structured_output`), and a
/// lapsed negative (`context_management`). `computer_use` stays unseeded.
fn seed_shapes(router: &Router, nickname: &str, with_route_away: bool) -> Vec<String> {
    seed_learned(router, nickname, "advisor", FailurePhase::F1);
    seed_verified(router, nickname, "structured_output");
    seed_lapsed(router, nickname, "context_management");
    let mut features = vec![
        "advisor".to_string(),
        "structured_output".to_string(),
        "computer_use".to_string(),
        "context_management".to_string(),
    ];
    if with_route_away {
        seed_learned(router, nickname, "web_search", FailurePhase::F1);
        features.push("web_search".to_string());
    }
    features
}

/// Count every `entries` lock acquisition on the router's registry.
fn count_entries_locks(router: &Router) -> Arc<AtomicUsize> {
    let count = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&count);
    router
        .learned_capabilities
        .set_lock_acquire_hook(Box::new(move |lock| {
            if lock == "entries" {
                sink.fetch_add(1, Ordering::SeqCst);
            }
        }));
    count
}

fn filter(
    router: &Router,
    target: &DispatchTarget,
    features: &[String],
) -> (
    Option<(FeatureKey, FilterSource)>,
    Vec<ProbeAdmission>,
    Vec<String>,
) {
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict =
        router.unsupported_feature_for_target(target, features, &mut admissions, &mut strip_keys);
    (verdict, admissions, strip_keys)
}

#[test]
fn every_learned_feature_takes_the_registry_entries_lock_once_per_target() {
    // Each row: (name, router after a generation change, route-away present).
    // Without a route-away the prior pass runs, so a second read there would
    // show in the count.
    let rows = [
        ("fresh router, all five shapes", false, true),
        ("fresh router, prior pass reached", false, false),
        ("live router after a boundary, all five shapes", true, true),
        (
            "live router after a boundary, prior pass reached",
            true,
            false,
        ),
    ];
    for (name, after_boundary, with_route_away) in rows {
        // Arrange
        let config = Arc::new(base_config(""));
        let router = if after_boundary {
            superseded_pair_with(config).1
        } else {
            Router::new(config)
        };
        let target = target_with_priors(&router, "nick", &[]);
        let features = seed_shapes(&router, "nick", with_route_away);
        let count = count_entries_locks(&router);

        // Act
        let (verdict, admissions, _) = filter(&router, &target, &features);

        // Assert
        let expected_verdict =
            with_route_away.then(|| ("web_search".to_string(), FilterSource::Learned));
        assert_eq!(verdict, expected_verdict, "{name}");
        assert_eq!(admissions.len(), 1, "{name}: the lapsed slot is claimed");
        assert_eq!(
            count.load(Ordering::SeqCst),
            features.len(),
            "{name}: one entries acquisition per learned feature",
        );
    }
}

#[test]
fn verified_positive_masks_prior_false_beside_other_shapes() {
    // Arrange -- the verified feature carries `prior=false`.
    let router = Router::new(Arc::new(base_config("")));
    let target = target_with_priors(&router, "nick", &[("structured_output", false)]);
    let features = seed_shapes(&router, "nick", false);

    // Act
    let (verdict, admissions, strip_keys) = filter(&router, &target, &features);

    // Assert -- not demoted; the strip and the admission still happen.
    assert_eq!(verdict, None);
    assert_eq!(strip_keys, vec!["advisor".to_string()]);
    assert_eq!(admissions.len(), 1);
}

#[test]
fn prior_false_on_an_unseeded_feature_demotes_and_keeps_learned_strips() {
    // Arrange -- the unseeded feature carries `prior=false`.
    let router = Router::new(Arc::new(base_config("")));
    let target = target_with_priors(&router, "nick", &[("computer_use", false)]);
    let features = seed_shapes(&router, "nick", false);

    // Act
    let (verdict, _, strip_keys) = filter(&router, &target, &features);

    // Assert
    assert_eq!(
        verdict,
        Some(("computer_use".to_string(), FilterSource::Prior))
    );
    assert_eq!(strip_keys, vec!["advisor".to_string()]);
}

#[test]
fn a_lapsed_negative_admits_once_under_the_generation_its_read_saw() {
    // Arrange -- an admitted-but-uncommitted boundary makes the effective
    // generation differ from the one the Router was published at, so a
    // generation sampled outside the read would not match.
    let router = Router::new(Arc::new(base_config("")));
    let target = target_with_priors(&router, "nick", &[]);
    seed_lapsed(&router, "nick", "context_management");
    let BoundaryCut::Taken { receipt, .. } = router
        .learned_capabilities
        .with_boundary_cut(|_, _| (), |()| true)
    else {
        panic!("the first boundary must be taken");
    };
    assert_ne!(receipt.generation(), router.registry_generation());

    // Act
    let (verdict, admissions, strip_keys) =
        filter(&router, &target, &["context_management".to_string()]);

    // Assert
    assert_eq!(verdict, None);
    assert!(strip_keys.is_empty(), "an admitted probe is never stripped");
    assert_eq!(admissions.len(), 1);
    assert_eq!(admissions[0].generation, receipt.generation());
    assert_eq!(admissions[0].learned_key, lane_of("nick"));
}

#[test]
fn a_superseded_router_acts_on_no_learned_verdict_but_still_demotes_on_prior() {
    // Arrange -- every shape resident, plus `prior=false` on the verified
    // feature, which only a learned read could mask.
    let (old, live) = superseded_pair_with(Arc::new(base_config("")));
    let priors = [("structured_output", false)];
    let old_target = target_with_priors(&old, "nick", &priors);
    let live_target = target_with_priors(&live, "nick", &priors);
    let features = seed_shapes(&live, "nick", true);

    // Act
    let stale = filter(&old, &old_target, &features);
    let current = filter(&live, &live_target, &features);

    // Assert -- the stale Router sees no verdict: no strip, no admission, no
    // route-away, and the unmasked prior demotes.
    assert_eq!(
        stale.0,
        Some(("structured_output".to_string(), FilterSource::Prior))
    );
    assert!(stale.1.is_empty(), "a stale read claims no re-probe slot");
    assert!(stale.2.is_empty(), "a stale read strips nothing");
    // Positive control -- the live Router reads the same entries.
    assert_eq!(
        current.0,
        Some(("web_search".to_string(), FilterSource::Learned))
    );
    assert_eq!(current.1.len(), 1);
}
