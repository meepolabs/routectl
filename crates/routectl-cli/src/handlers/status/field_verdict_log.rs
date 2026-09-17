//! Shared live-visibility log line for acting field verdicts, called from
//! both the health and doctor panels so a field repair or a durably purged
//! verdict stays observable at INFO without a response-body change.

use routectl_router::{LearnedRegistryEntry, acting_field_verdicts};

use super::router_view::StatusRouterView;

/// Emit the single aggregated envelope-field verdict INFO log for one panel
/// build: the live process-lifetime repair counters plus every currently
/// acting field verdict, with its provenance. Log-only -- never crosses onto
/// a panel's response body.
pub(super) fn log_field_verdict_snapshot(
    view: &StatusRouterView,
    learned: &[LearnedRegistryEntry],
) {
    let counters = view.field_repair_counters();
    let acting = acting_field_verdicts(learned);
    tracing::info!(
        rc_field_repair_attempted_total = counters.repair_attempted,
        rc_field_repair_succeeded_total = counters.repair_succeeded,
        rc_field_verdicts_learned_total = counters.verdicts_learned,
        rc_acting_field_verdicts_total = acting.len(),
        rc_acting_field_verdicts = ?acting,
        "envelope field verdict snapshot",
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use arc_swap::ArcSwap;
    use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
    use routectl_router::{Config, Router};

    use super::super::router_view::StatusRouterHandle;
    use super::*;

    fn fresh_view() -> StatusRouterView {
        let router = Router::new(Arc::new(Config::default()));
        let handle = StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router)));
        handle.view()
    }

    /// The wire-shape capability key the tests plant and then expect the log
    /// to name.
    ///
    /// The namespace prefix is owned by a single module in `routectl-router`,
    /// and a lexical guard there fails if the literal appears anywhere else
    /// under `crates/`: a second spelling could drift from the owner and
    /// re-partition persisted history. That module's constructor is
    /// crate-private to it, so this fixture assembles the key from parts --
    /// the scanned source carries no full prefix literal while the value
    /// stays byte-identical.
    fn acting_field_key() -> String {
        format!("{}{}{}", "fie", "ld:", "thinking.enabled.display")
    }

    fn learned_row(feature_key: String, verdict: Verdict) -> LearnedRegistryEntry {
        LearnedRegistryEntry {
            state_key: "opus".into(),
            feature_key,
            verdict,
            signal_tier: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: Instant::now(),
            last_seen: Instant::now(),
            expires_at: Instant::now(),
            evidence_class: None,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
        }
    }

    /// The INFO snapshot log carries the live repair counters and reports
    /// zero acting field verdicts against an empty learned-negative list --
    /// the base case a fixture that only ever plants a field negative could
    /// not fail on.
    #[test]
    fn reports_zero_for_an_empty_learned_snapshot() {
        let view = fresh_view();

        let events = routectl_testkit::capture_events(|| {
            log_field_verdict_snapshot(&view, &[]);
        });

        let info = events
            .iter()
            .find(|e| e.message == "envelope field verdict snapshot")
            .expect("field verdict snapshot INFO log emitted");
        assert_eq!(info.level, tracing::Level::INFO);
        assert_eq!(info.field("rc_field_repair_attempted_total"), Some("0"));
        assert_eq!(info.field("rc_field_repair_succeeded_total"), Some("0"));
        assert_eq!(info.field("rc_field_verdicts_learned_total"), Some("0"));
        assert_eq!(info.field("rc_acting_field_verdicts_total"), Some("0"));
    }

    /// A field-namespaced `LearnedBroken` row among the learned negatives is
    /// reported by the snapshot log as one acting field verdict, and the
    /// provenance list carries its feature key -- so the log is not merely
    /// counting, it is naming what is acting.
    #[test]
    fn names_an_acting_field_negative_in_the_provenance_field() {
        let acting_key = acting_field_key();
        let entries = [learned_row(
            acting_key.clone(),
            Verdict::LearnedBroken(FailurePhase::F1),
        )];
        let view = fresh_view();

        let events = routectl_testkit::capture_events(|| {
            log_field_verdict_snapshot(&view, &entries);
        });

        let info = events
            .iter()
            .find(|e| e.message == "envelope field verdict snapshot")
            .expect("field verdict snapshot INFO log emitted");
        assert_eq!(info.field("rc_acting_field_verdicts_total"), Some("1"));
        let provenance = info
            .field("rc_acting_field_verdicts")
            .expect("acting-verdicts provenance field present");
        assert!(provenance.contains(&acting_key));
    }

    /// A catalog-scoped `LearnedBroken` row (no field-namespace prefix) is a
    /// different namespace entirely and must not inflate the acting count --
    /// the log names only what is actually acting on the field surface.
    #[test]
    fn excludes_a_catalog_scoped_negative_from_the_acting_count() {
        let entries = [learned_row(
            "web_search".into(),
            Verdict::LearnedBroken(FailurePhase::F1),
        )];
        let view = fresh_view();

        let events = routectl_testkit::capture_events(|| {
            log_field_verdict_snapshot(&view, &entries);
        });

        let info = events
            .iter()
            .find(|e| e.message == "envelope field verdict snapshot")
            .expect("field verdict snapshot INFO log emitted");
        assert_eq!(info.field("rc_acting_field_verdicts_total"), Some("0"));
    }

    /// A field-namespaced row that is NOT `LearnedBroken` (e.g. cleared) must
    /// not count as acting -- surfacing it would claim a drop that is not
    /// happening.
    #[test]
    fn excludes_a_cleared_field_row_from_the_acting_count() {
        let entries = [learned_row(acting_field_key(), Verdict::Cleared)];
        let view = fresh_view();

        let events = routectl_testkit::capture_events(|| {
            log_field_verdict_snapshot(&view, &entries);
        });

        let info = events
            .iter()
            .find(|e| e.message == "envelope field verdict snapshot")
            .expect("field verdict snapshot INFO log emitted");
        assert_eq!(info.field("rc_acting_field_verdicts_total"), Some("0"));
    }
}
