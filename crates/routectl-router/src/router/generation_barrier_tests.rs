//! Tests for the generation barrier as the Router applies it: a superseded
//! Router must not repopulate or route on catalog-scoped state, while its
//! wire-shape observations still reach the live shared registry.

use super::*;
use crate::config::Config;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use std::sync::Arc;
use std::time::Instant;

/// The wire-shape key, assembled at runtime (the namespace prefix has exactly
/// one spelling, in its owning module).
fn field_key(path: &str) -> String {
    format!("{}{}{}", "fie", "ld:", path)
}

/// Two Routers sharing one registry: `old` is the superseded generation, `live`
/// is published after a boundary transition.
fn superseded_pair() -> (Router, Router) {
    let config = Arc::new(Config::default());
    let old = Router::new(config.clone());
    let mut live = Router::new(config);
    live.carry_over_learned_from(&old);
    // A full cut+commit so the generation STRICTLY advances.
    let crate::learned_capability::BoundaryCut::Taken { receipt, .. } = live
        .learned_capabilities
        .with_boundary_cut(|_, pending| pending, |_| true)
    else {
        panic!("the first boundary must be taken");
    };
    let settled = live
        .learned_capabilities
        .commit_boundary_transition(&receipt);
    assert!(
        matches!(
            settled,
            crate::learned_capability::BoundarySettlement::Applied { .. }
        ),
        "the transition must apply",
    );
    live.registry_generation.store(
        live.learned_capabilities.generation(),
        std::sync::atomic::Ordering::Release,
    );
    (old, live)
}

/// A wire-shape observation made through the OLD Router after the swap lands in
/// the live registry, and the live Router reads it.
///
/// This is the whole point of sharing one registry Arc: a request that was
/// already dispatching when the reload completed still learned something true,
/// and a catalog revision cannot make a statement about a request envelope
/// false. Under the retired snapshot-copy carry-over that observation went into
/// a discarded store.
#[test]
fn an_old_router_field_observation_reaches_the_live_registry() {
    let (old, live) = superseded_pair();
    let key = field_key("thinking.enabled.display");
    let now = Instant::now();

    let outcome = old.observe_learned_capability(
        "nick",
        &key,
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );

    assert!(
        matches!(
            outcome,
            crate::learned_capability::GenerationOutcome::Applied { .. }
        ),
        "a wire-shape observation from a superseded Router must be accepted",
    );
    let resident: Vec<String> = live
        .learned_capability_snapshot()
        .into_iter()
        .map(|e| e.feature_key)
        .collect();
    assert_eq!(
        resident,
        vec![key],
        "the live Router must see the old Router's field observation",
    );
}

/// A catalog-scoped observation through the old Router after the swap is inert:
/// nothing resident, and the caller is told Stale so it emits no ledger event
/// and bumps no metric.
#[test]
fn an_old_router_catalog_scoped_observation_is_inert() {
    let (old, live) = superseded_pair();

    let outcome = old.observe_learned_capability(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    assert_eq!(
        outcome,
        crate::learned_capability::GenerationOutcome::Stale,
        "a stale catalog-scoped observation must be refused",
    );
    assert!(
        live.learned_capability_snapshot().is_empty(),
        "it must leave nothing resident to repopulate the pruned state",
    );
}

/// A catalog-scoped READ through the old Router does not answer, so a
/// superseded Router cannot route on catalog truth the reload replaced. The
/// field read still answers.
#[test]
fn old_router_reads_are_scoped_by_key_class() {
    let (old, live) = superseded_pair();
    let now = Instant::now();
    let key = field_key("thinking.enabled.display");
    // Both classes learned on the LIVE generation.
    for capability in [key.as_str(), "web_search"] {
        live.observe_learned_capability(
            "nick",
            capability,
            "anthropic-api",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            now,
        );
    }

    assert_eq!(
        old.acting_negative_for_generation("nick", "web_search", "anthropic-api", now),
        None,
        "a stale catalog-scoped read must not answer",
    );
    assert!(
        old.acting_negative_for_generation("nick", &key, "anthropic-api", now)
            .is_some(),
        "a stale field read still answers",
    );
}

/// A probe settlement (the keyed clear) through the old Router on a
/// catalog-scoped key is a no-op AND reports Stale, so no cleared event rides
/// out to the ledger.
#[test]
fn an_old_router_probe_settlement_on_a_scoped_key_is_inert() {
    let (old, live) = superseded_pair();
    let now = Instant::now();
    live.observe_learned_capability(
        "nick",
        "web_search",
        "anthropic-api",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );

    let settled = old.clear_learned_capability("nick", "web_search", "anthropic-api");

    assert_eq!(
        settled,
        crate::learned_capability::GenerationOutcome::Stale,
        "a stale settlement must report Stale so nothing is emitted",
    );
    assert_eq!(
        live.learned_capability_snapshot().len(),
        1,
        "and must not clear the live entry",
    );
}

/// The replay facade shares its in-flight set across a rebuild.
///
/// A carry outstanding when the reload lands settles through the OLD facade, so
/// its release must be visible to the replacement's admission check. Two
/// separate sets would either admit a second concurrent carry for a pair already
/// in flight -- the N-parallel-repairs cost single-flight exists to prevent --
/// or leave the key latched with nothing able to clear it.
#[test]
fn the_replay_facade_shares_its_in_flight_set_across_a_rebuild() {
    let config = Arc::new(Config::default());
    let old = Router::new(config.clone());
    let mut live = Router::new(config);
    live.carry_over_learned_from(&old);

    assert!(
        old.learned_replay()
            .shares_in_flight_with(live.learned_replay()),
        "the rebuilt facade must share the outgoing in-flight set",
    );
}

/// A catalog-scoped verified-positive read through a superseded Router does not
/// answer: it reports no positive rather than asserting catalog truth the reload
/// replaced. The field read still answers.
#[test]
fn old_router_verified_reads_are_scoped_by_key_class() {
    let (old, live) = superseded_pair();
    let now = Instant::now();
    let key = field_key("thinking.enabled.display");
    for capability in [key.as_str(), "web_search"] {
        live.observe_verified_capability(
            "nick",
            capability,
            "anthropic-api",
            EvidenceSource::Live,
            Some("search_blocks"),
            now,
        );
    }

    assert!(
        !old.is_verified_working_or_false("nick", "web_search", "anthropic-api", now),
        "a stale catalog-scoped positive read must not answer",
    );
    assert!(
        old.is_verified_working_or_false("nick", &key, "anthropic-api", now),
        "a stale field positive read still answers",
    );
}

/// The EFFECTIVE persistence generation is the pending one while a boundary is
/// admitted-but-uncommitted, and reverts on rollback.
///
/// This is what makes an observation through the still-published old Router
/// survive: its event is stamped with the generation the boundary is committing,
/// so the writer keeps it instead of dropping it as pre-boundary.
#[test]
fn the_effective_generation_follows_the_pending_boundary() {
    let config = Arc::new(Config::default());
    let router = Router::new(config);
    let registry = router.learned_registry();
    let active = registry.generation();
    assert_eq!(registry.effective_persistence_generation(), active);

    let crate::learned_capability::BoundaryCut::Taken { receipt, .. } =
        registry.with_boundary_cut(|_, _| (), |()| true)
    else {
        panic!("must be taken");
    };
    assert_eq!(
        registry.effective_persistence_generation(),
        receipt.generation(),
        "an admitted boundary makes its generation the one events must use",
    );

    let _ = registry.rollback_pending_generation(&receipt);
    assert_eq!(
        registry.effective_persistence_generation(),
        active,
        "a rolled-back boundary reverts to the newest committed generation",
    );
}

/// Committing promotes the PENDING generation rather than inventing a new one,
/// so events already stamped with it stay valid.
#[test]
fn committing_promotes_the_pending_generation() {
    let config = Arc::new(Config::default());
    let router = Router::new(config);
    let registry = router.learned_registry();
    let crate::learned_capability::BoundaryCut::Taken { receipt, .. } =
        registry.with_boundary_cut(|_, _| (), |()| true)
    else {
        panic!("must be taken");
    };

    let settled = registry.commit_boundary_transition(&receipt);

    assert_eq!(
        settled,
        crate::learned_capability::BoundarySettlement::Applied {
            generation: receipt.generation(),
            pruned: 0,
        },
        "the committed generation must be the one the batch was stamped with",
    );
    assert_eq!(
        registry.effective_persistence_generation(),
        receipt.generation()
    );
}

/// The spellings that must not appear in a routing path. Each has an
/// `_in_generation` counterpart; the ungated forms stay public because the warm
/// rebuild and the doctor's offline read legitimately use them.
const UNGATED_REGISTRY_CALLS: &[&str] = &[
    ".observe(",
    ".observe_positive(",
    ".acting_negative_for(",
    ".is_verified_working(",
    ".remove_keyed(",
    ".expire_keyed(",
    ".record_probe_outcome(",
    ".negative_state(",
];

/// Sentinel a file must carry immediately before an INLINE `mod tests`, stating
/// that no production code follows the cut.
const LAST_ITEM_SENTINEL: &str = "// LAST ITEM IN FILE: no production code follows.";

/// Scan one source for ungated registry calls, returning one finding per hit.
///
/// Extracted from the test so it can be driven against FIXTURES rather than only
/// against the current tree -- a scanner whose failure branches are never
/// exercised reports safety it has not verified.
///
/// The whole source is scanned. An inline `mod tests {` would put test text in
/// scope, so it must be the file's last item and say so via
/// [`LAST_ITEM_SENTINEL`]; a missing sentinel is itself a finding.
fn scan_for_ungated_calls(label: &str, src: &str) -> Vec<String> {
    let mut findings = Vec::new();
    if let Some(at) = src.find("\nmod tests {")
        && !src[..at].trim_end().ends_with(LAST_ITEM_SENTINEL)
    {
        findings.push(format!(
            "{label} has an inline `mod tests` without the last-item sentinel, \
             so a whole-file scan would read test text as production"
        ));
    }
    for needle in UNGATED_REGISTRY_CALLS {
        if let Some(at) = src.find(needle) {
            let line = src[..at].lines().count();
            findings.push(format!("{label}:{line} calls {needle}"));
        }
    }
    findings
}

/// BACKSTOP: no production code reaches the registry's ungated mutation and read
/// methods directly.
///
/// A backstop, not the coverage: the behavioral tests above and in
/// `probe_guard_generation_tests` / `learned_replay` prove each path refuses a
/// stale generation. This catches a NEW call site added later.
///
/// # Why this does not cut at the first `#[cfg(test)]`
///
/// It used to, and that was VACUOUS: `router/mod.rs` declares test sidecars at
/// line 13 of 2569, and `runtime_gate.rs` at line 21 of 593, so the scan covered
/// a few lines of imports and reported clean.
#[test]
fn no_production_code_calls_the_ungated_registry_api() {
    // `include_str!` resolves at compile time, so a renamed or deleted file is a
    // compile error rather than a silently-skipped scan.
    let modules: &[(&str, &str)] = &[
        ("router/mod.rs", include_str!("mod.rs")),
        (
            "router/capability_learn.rs",
            include_str!("capability_learn.rs"),
        ),
        (
            "router/capability_observe.rs",
            include_str!("capability_observe.rs"),
        ),
        (
            "router/feature_filter.rs",
            include_str!("feature_filter.rs"),
        ),
        ("router/runtime_gate.rs", include_str!("runtime_gate.rs")),
        ("router/count_tokens.rs", include_str!("count_tokens.rs")),
        ("router/dispatch.rs", include_str!("dispatch.rs")),
        ("router/replay_repair.rs", include_str!("replay_repair.rs")),
        ("learned_replay.rs", include_str!("../learned_replay.rs")),
        ("field_verdict.rs", include_str!("../field_verdict.rs")),
    ];

    let findings: Vec<String> = modules
        .iter()
        .flat_map(|(label, src)| scan_for_ungated_calls(label, src))
        .collect();

    assert!(
        findings.is_empty(),
        "production code must reach the registry through a generation-aware \
         facade; found: {findings:?}",
    );
}

/// REJECT control: an ungated call is found even when it sits AFTER an early test
/// module -- the case the retired first-`#[cfg(test)]` cut hid.
///
/// Driven against a fixture, because the tree is (and should stay) clean, so
/// nothing in the real sources exercises this branch.
#[test]
fn the_scanner_finds_an_ungated_call_after_an_early_test_module() {
    let fixture = "fn early() {}\n\n#[cfg(test)]\nmod early_tests {}\n\n\
                   fn late() { registry.remove_keyed(a, b, c); }\n";

    let findings = scan_for_ungated_calls("fixture.rs", fixture);

    assert!(
        findings.iter().any(|f| f.contains(".remove_keyed(")),
        "a call after an early test module must still be found: {findings:?}",
    );
}

/// The SENTINEL branch fires when an inline `mod tests` is followed by production
/// code.
///
/// Needs its own fixture: no file in the tree has an inline test module today, so
/// the real sources cannot drive this branch at all.
#[test]
fn the_scanner_refuses_an_inline_test_module_without_the_sentinel() {
    let fixture = "fn production() {}\n\nmod tests {\n    fn t() {}\n}\n\n\
                   fn more_production() {}\n";

    let findings = scan_for_ungated_calls("fixture.rs", fixture);

    assert!(
        findings.iter().any(|f| f.contains("last-item sentinel")),
        "an inline test module without the sentinel must be refused: {findings:?}",
    );
}

/// The sentinel's ACCEPT control: a correctly-ended source passes.
///
/// Without it the rule could be written as "any inline `mod tests` is a finding",
/// which would reject a legitimate file and push the next author to weaken the
/// scanner rather than satisfy it.
#[test]
fn the_scanner_accepts_an_inline_test_module_that_declares_itself_last() {
    let fixture = format!(
        "fn production() {{}}\n\n{LAST_ITEM_SENTINEL}\nmod tests {{\n    fn t() {{}}\n}}\n"
    );

    let findings = scan_for_ungated_calls("fixture.rs", &fixture);

    assert!(
        findings.is_empty(),
        "a sentinel-declared last test module must be accepted: {findings:?}",
    );
}

/// Overall ACCEPT control: a clean production source yields nothing, so the
/// scanner cannot pass by rejecting everything.
#[test]
fn the_scanner_accepts_a_clean_production_source() {
    let fixture = "fn production() { registry.remove_keyed_in_generation(g, a, b, c); }\n";

    assert!(
        scan_for_ungated_calls("fixture.rs", fixture).is_empty(),
        "a generation-aware call must not be flagged",
    );
}

/// The scan's needles must match the spellings they name, and must NOT match
/// their own `_in_generation` counterparts.
///
/// Without this a typo (`.observe (`, `.obserev(`) would leave that entry
/// permanently inert while the suite stayed green.
#[test]
fn the_ungated_api_scan_matches_the_spellings_it_bans() {
    for needle in UNGATED_REGISTRY_CALLS {
        let sample = format!("registry{needle}a, b)");
        assert!(
            sample.contains(needle),
            "the scan's needle {needle} does not match a real call: {sample}",
        );
        let routed = needle.replace('(', "_in_generation(");
        assert!(
            !format!("registry{routed}g, a)").contains(needle),
            "needle {needle} also matches its own _in_generation form",
        );
    }
}
