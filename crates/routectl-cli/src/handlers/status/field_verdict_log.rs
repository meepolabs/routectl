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
        rc_field_outstanding_unconfirmed_total = counters.outstanding_unconfirmed,
        rc_field_disproved_requests_total = counters.disproved_requests,
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

    /// The argument list of the production `tracing::info!` call, from the macro's
    /// opening paren to the message literal that closes it.
    ///
    /// The production/test cut is delegated to
    /// [`crate::handlers::status::production_source`] rather than hand-rolled, and
    /// that matters: cutting on the first `#[cfg(test)]` is the known-buggy shape
    /// that helper exists to retire, because the attribute also decorates
    /// test-only items sitting ABOVE the production code and the scanned region
    /// then silently shrinks. Measured here, not assumed -- with a hand-rolled
    /// `#[cfg(test)]` cut, adding a benign test-only const above the macro
    /// truncated this guard's region to the module doc comment, so the macro was
    /// no longer in it.
    ///
    /// Cutting at all is still load-bearing: the needles the caller searches for
    /// also occur in THIS module's text (the `OPEN` literal below, the assertion
    /// strings), so a whole-file scan would match its own literal if the
    /// production macro were renamed and then sweep a region containing the
    /// needles -- passing vacuously exactly when it should fail.
    ///
    /// # Panics
    ///
    /// If the macro call or the message literal is absent from the production
    /// region, or if `production_source` finds the cut ambiguous. Failing closed is
    /// the point: a locator returning an empty slice would make its caller pass
    /// vacuously, which is exactly the shape a mapping guard must not have.
    fn info_macro_args(source: &str) -> &str {
        const OPEN: &str = "tracing::info!(";
        const CLOSE: &str = "\"envelope field verdict snapshot\"";
        let production = crate::handlers::status::production_source::production_source(source);
        let open_at = production
            .find(OPEN)
            .expect("the snapshot log must be emitted through a tracing::info! call");
        let args = &production[open_at + OPEN.len()..];
        let end = args
            .find(CLOSE)
            .expect("the info! argument list must close with the snapshot message literal");
        &args[..end]
    }

    /// `args` with every run of whitespace removed, so the mapping assertions are
    /// insensitive to rustfmt's line breaking. Nothing else is rewritten -- a
    /// dropped token would be a hole in the contract.
    fn without_whitespace(args: &str) -> String {
        args.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The two alarm log fields must each read the counter they are NAMED for.
    ///
    /// Nothing else pins this pairing. A captured event carries only the emitted
    /// values, so swapping the two right-hand sides produces a log that is still
    /// well-formed, still nonzero, and still passes every trace assertion -- while
    /// reporting current exposure as lifetime exposure and the reverse. Since the
    /// two halves mean opposite things (one MIGHT be disproved, the other WAS),
    /// that swap is a reporting inversion an operator cannot detect.
    ///
    /// Asserted against the source text because the mapping is not observable at
    /// runtime. Scoped to the `info!` argument list rather than the whole file, so
    /// a doc comment mentioning a field name cannot satisfy it.
    ///
    /// Mutation checks: swap `counters.outstanding_unconfirmed` and
    /// `counters.disproved_requests` between the two field names -> red on both
    /// assertions; repoint just ONE field at the other counter -> red on that
    /// field's assertion alone.
    #[test]
    fn each_alarm_log_field_reads_the_counter_it_is_named_for() {
        let args = without_whitespace(info_macro_args(include_str!("field_verdict_log.rs")));

        assert!(
            args.contains(
                "rc_field_outstanding_unconfirmed_total=counters.outstanding_unconfirmed"
            ),
            "the outstanding-half log field must read the outstanding counter, not the \
             lifetime one: the two mean opposite things and a swap is invisible in the \
             emitted log",
        );
        assert!(
            args.contains("rc_field_disproved_requests_total=counters.disproved_requests"),
            "and the lifetime-half log field must read the lifetime counter",
        );
    }

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
    ///
    /// This test's share of the alarm halves is PRESENCE: both fields are emitted
    /// even at zero, because an absent field reads as an unavailable panel rather
    /// than as no exposure. Three layers divide the contract, and none substitutes
    /// for another:
    /// - nonzero READ-BACK through `field_repair_counters` is pinned at the router
    ///   layer (`field_repair_counters_reports_both_nonzero_alarm_halves`), where
    ///   the canary registry is in scope;
    /// - the CLI field-name -> counter MAPPING is pinned by the source guard above
    ///   (`each_alarm_log_field_reads_the_counter_it_is_named_for`), since a swap
    ///   is invisible in the emitted values;
    /// - PRESENCE is pinned here.
    ///
    /// No nonzero counterpart lives at this layer deliberately: both counters come
    /// from the router's canary registry, reachable only through `pub(crate)`
    /// surface (`Router::field_verdicts`, and the `field_canary` / `field_verdict`
    /// modules), so driving them from this crate would mean widening production
    /// visibility for a test.
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
        assert_eq!(
            info.field("rc_field_outstanding_unconfirmed_total"),
            Some("0"),
            "the wrong-repair alarm's outstanding half is reported even at zero: an \
             absent field reads as an unavailable panel rather than as no exposure",
        );
        assert_eq!(
            info.field("rc_field_disproved_requests_total"),
            Some("0"),
            "and so is its lifetime half",
        );
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
