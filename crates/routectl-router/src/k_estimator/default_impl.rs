//! Default ledger-backed K estimator.
//!
//! Answers a [`KQuery`] from the in-memory [`KSessionStore`] populated by the
//! live dispatch path and the [`super::rebuild`] data path. No IO on the hot
//! path; a query that names an unknown triple gets a `Cold` default rather
//! than failing.
//!
//! The reuse math is a PER-TURN HAZARD (geometric) model, not the earlier
//! TTL-gap-run percentile model. Each retained sample is one Bernoulli trial:
//! did that turn observe cache reuse? Over the window's `n` trials with
//! `successes` reuses the per-turn continuation probability `p_hat =
//! successes / n` drives the geometric horizon `E[K] = p / (1 - p)`. The
//! floor comes from the Wilson one-sided LOWER bound on `p` (so a thin or
//! all-success window widens rather than over-promising), the ceiling from
//! the Wilson UPPER bound. Framing evidence as the TURN (not the run) means a
//! single long contiguous session is `n` observations, not one -- which is
//! what fixes v1's starvation of few-long-contiguous sessions.
//!
//! [`wilson_bound`] is the isolated, swappable, validation-gated core (D15):
//! a pure free function with its own tests, tuned against real ledger data by
//! the calibration harness. `Z_WILSON`, `P_CLAMP`, and `CALIBRATED_MIN_TRIALS`
//! are PROVISIONAL, harness-tunable values, not a public contract.

use std::sync::Arc;

use super::store::{KSessionKey, KSessionStore};
use super::{Confidence, EstimateSource, KEstimate, KEstimator, KQuery};

/// One-sided normal quantile for the Wilson bound. `~1.645` is the ~95%
/// one-sided level. HARNESS-TUNABLE: this is the single dial the calibration
/// harness turns to trade authorized cuts against the coverage safety gate;
/// raising it widens the interval (fewer, safer cuts), lowering it narrows it.
const Z_WILSON: f64 = 1.645;

/// Upper clamp on the per-turn continuation probability fed to `E[K]`. A
/// window of all-success turns yields `p_hat = 1.0`, and `1 / (1 - 1)` is
/// infinite; clamping `p` just below 1 keeps `E[K]` finite and non-NaN. At
/// `0.99` the maximum expressible horizon is `0.99 / 0.01 = 99` reuses --
/// far above any realistic break-even K while staying finite. PROVISIONAL.
const P_CLAMP: f64 = 0.99;

/// Minimum number of observed TRIALS (samples) before an estimate is
/// `Calibrated` (the floor may then gate a cut). Below this the estimate
/// stays advisory-only and the floor is forced to 0.
///
/// PROVISIONAL: carried at 8 from v1's `CALIBRATED_MIN_RUNS`, to be re-picked
/// from the first per-turn calibration run against real ledger data. 8
/// Bernoulli trials is few, but the Wilson lower bound self-widens at low `n`,
/// so it self-protects until the harness sets a data-driven value.
const CALIBRATED_MIN_TRIALS: u32 = 8;

/// Ledger-backed [`KEstimator`]. Reads from a shared [`KSessionStore`].
///
/// In this form the store `Arc` is supplied by the constructor; the router
/// shares its own store handle in additive follow-up work. Cheap to clone.
pub struct LedgerBackedK {
    store: Arc<KSessionStore>,
}

impl LedgerBackedK {
    /// Construct an estimator over the given shared session store.
    pub const fn new(store: Arc<KSessionStore>) -> Self {
        Self { store }
    }
}

impl KEstimator for LedgerBackedK {
    fn estimate(&self, q: &KQuery<'_>) -> KEstimate {
        // q.ttl and q.now are accepted by the KQuery contract but unused by
        // this constant-hazard model; reserved for a future additive
        // age-conditioning refinement.
        let Some(session_key) = q.session_key else {
            return cold_default();
        };

        let key = KSessionKey {
            session_key: session_key.to_string(),
            provider_kind: q.provider_kind.to_string(),
            model: q.model.to_string(),
        };

        let Some(window) = self.store.get(&key) else {
            return cold_default();
        };
        if window.is_empty() {
            return cold_default();
        }

        let n = window.len() as u32;
        let successes = window.iter().filter(|s| s.observed_reuse).count() as u32;
        let (k_floor, k_point, k_ceiling, confidence) = hazard_estimate(successes, n);

        let source = match confidence {
            Confidence::Cold => EstimateSource::ColdDefault,
            _ => EstimateSource::LiveLedger,
        };

        KEstimate {
            k_floor,
            k_point,
            k_ceiling,
            samples: n,
            confidence,
            source,
        }
    }
}

/// The all-zero cold estimate returned for a missing session, an unknown
/// triple, or an empty window.
const fn cold_default() -> KEstimate {
    KEstimate {
        k_floor: 0.0,
        k_point: 0.0,
        k_ceiling: 0.0,
        samples: 0,
        confidence: Confidence::Cold,
        source: EstimateSource::ColdDefault,
    }
}

/// Pure per-turn-hazard reducer over a window's Bernoulli reuse outcomes.
///
/// `successes` = samples that observed reuse, `n` = total samples. Returns
/// `(k_floor, k_point, k_ceiling, confidence)`:
/// - `n == 0` -> `Cold`, all bounds 0.
/// - `k_point = E[K]` from the observed continuation rate `successes / n`.
/// - `k_floor = E[K]` from the Wilson LOWER bound on `p`; `k_ceiling` from
///   the Wilson UPPER bound. Because `p_lcb <= p_hat <= p_ucb` and
///   [`expected_k`] is monotone non-decreasing, `k_floor <= k_point <=
///   k_ceiling` always holds.
/// - `successes == 0` collapses `p_lcb` to 0, so the floor is 0 (fail-closed
///   KEEP: an all-miss window never authorizes a cut).
/// - `n < CALIBRATED_MIN_TRIALS` -> `Low`, and `k_floor` is force-clamped to
///   0 (a thin sample must never gate a cut). `n >= CALIBRATED_MIN_TRIALS` ->
///   `Calibrated`.
fn hazard_estimate(successes: u32, n: u32) -> (f64, f64, f64, Confidence) {
    if n == 0 {
        return (0.0, 0.0, 0.0, Confidence::Cold);
    }

    let p_hat = f64::from(successes) / f64::from(n);
    let k_point = expected_k(p_hat);

    let p_lcb = wilson_bound(successes, n, Z_WILSON, false);
    let mut k_floor = expected_k(p_lcb);

    let p_ucb = wilson_bound(successes, n, Z_WILSON, true);
    let k_ceiling = expected_k(p_ucb);

    let confidence = if n >= CALIBRATED_MIN_TRIALS {
        Confidence::Calibrated
    } else {
        k_floor = 0.0;
        Confidence::Low
    };

    (k_floor, k_point, k_ceiling, confidence)
}

/// One-sided Wilson score bound on a Bernoulli success probability.
///
/// This is the swappable, validation-gated core (D15): closed-form, no
/// dependency, and -- unlike the Wald interval -- its LOWER bound is strictly
/// below 1.0 at any finite `n` even when `successes == trials`, so `E[K]`
/// built on it never goes infinite.
///
/// With `n = trials` and `phat = successes / n`:
/// ```text
/// denom  = 1 + z^2 / n
/// center = (phat + z^2 / 2n) / denom
/// margin = (z / denom) * sqrt( phat(1 - phat)/n + z^2 / 4n^2 )
/// bound  = center + margin  (upper)  |  center - margin  (lower)
/// ```
/// The result is clamped to `[0.0, 1.0]`. `trials == 0` returns `0.0`
/// (fail-closed: no evidence authorizes nothing, never NaN).
fn wilson_bound(successes: u32, trials: u32, z: f64, upper: bool) -> f64 {
    if trials == 0 {
        return 0.0;
    }
    let n = f64::from(trials);
    let phat = f64::from(successes) / n;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = (phat + z2 / (2.0 * n)) / denom;
    let margin = (z / denom) * (phat * (1.0 - phat) / n + z2 / (4.0 * n * n)).sqrt();
    let bound = if upper {
        center + margin
    } else {
        center - margin
    };
    bound.clamp(0.0, 1.0)
}

/// Geometric reuse horizon `E[K] = p / (1 - p)` for a per-turn continuation
/// probability `p`, made finite and non-negative.
///
/// `p <= 0` -> `0.0`; `p` is clamped to [`P_CLAMP`] just below 1.0 so an
/// all-success window can never produce `Inf`/`NaN`. Monotonic
/// non-decreasing in `p`.
fn expected_k(p: f64) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    let p = p.min(P_CLAMP);
    p / (1.0 - p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k_estimator::store::{KSessionWindow, Sample};
    use std::time::{Duration, UNIX_EPOCH};

    fn window_from(samples: &[(u64, bool)]) -> KSessionWindow {
        let mut w = KSessionWindow::new();
        for (secs, reuse) in samples {
            w.push(Sample {
                ts: UNIX_EPOCH + Duration::from_secs(*secs),
                observed_reuse: *reuse,
            });
        }
        w
    }

    fn query(session: Option<&str>, ttl: Duration) -> KQuery<'_> {
        KQuery {
            session_key: session,
            provider_kind: "anthropic-api",
            model: "opus",
            ttl,
            now: UNIX_EPOCH,
        }
    }

    fn key(session: &str) -> KSessionKey {
        KSessionKey {
            session_key: session.into(),
            provider_kind: "anthropic-api".into(),
            model: "opus".into(),
        }
    }

    // --- wilson_bound ---

    #[test]
    fn wilson_bound_zero_trials_returns_zero() {
        // Arrange + Act + Assert: no evidence -> 0 for both directions, never
        // NaN from a divide-by-zero.
        assert_eq!(wilson_bound(0, 0, Z_WILSON, false), 0.0);
        assert_eq!(wilson_bound(0, 0, Z_WILSON, true), 0.0);
    }

    #[test]
    fn wilson_bound_zero_successes_lower_is_zero() {
        // Arrange: an all-miss window over several trials.
        // Act + Assert: the lower bound collapses to exactly 0 (fail-closed).
        for trials in [1u32, 4, 8, 32] {
            assert_eq!(
                wilson_bound(0, trials, Z_WILSON, false),
                0.0,
                "lower bound at 0 successes / {trials} trials must be 0",
            );
        }
    }

    #[test]
    fn wilson_bound_lower_below_one_at_full_success() {
        // Arrange: successes == trials for every window size we can hold.
        // Act + Assert: the lower bound is strictly < 1, so E[K] stays finite
        // -- the whole reason Wilson is chosen over Wald.
        for trials in 1u32..=32 {
            let lower = wilson_bound(trials, trials, Z_WILSON, false);
            assert!(
                lower < 1.0,
                "lower bound at {trials}/{trials} must be < 1, got {lower}",
            );
        }
    }

    #[test]
    fn wilson_bound_matches_hand_computed_value() {
        // Arrange: successes=1, trials=1, z=1.0. By hand:
        //   denom  = 1 + 1/1 = 2
        //   center = (1 + 1/2) / 2 = 0.75
        //   margin = (1/2) * sqrt(0 + 1/4) = 0.5 * 0.5 = 0.25
        //   lower  = 0.75 - 0.25 = 0.50 ; upper = 1.00 (clamped from 1.00)
        // Act
        let lower = wilson_bound(1, 1, 1.0, false);
        let upper = wilson_bound(1, 1, 1.0, true);

        // Assert
        assert!((lower - 0.5).abs() < 1e-12, "lower {lower} != 0.5");
        assert!((upper - 1.0).abs() < 1e-12, "upper {upper} != 1.0");
    }

    #[test]
    fn wilson_bound_brackets_the_observed_rate() {
        // Arrange: a spread of (successes, trials) pairs.
        // Act + Assert: lower <= phat <= upper for every pair (the score
        // interval always contains the point estimate).
        for (s, t) in [
            (0u32, 4u32),
            (1, 4),
            (2, 4),
            (3, 4),
            (4, 4),
            (5, 10),
            (8, 10),
        ] {
            let phat = f64::from(s) / f64::from(t);
            let lower = wilson_bound(s, t, Z_WILSON, false);
            let upper = wilson_bound(s, t, Z_WILSON, true);
            assert!(
                lower <= phat && phat <= upper,
                "({s}/{t}) phat={phat} not in [{lower}, {upper}]",
            );
        }
    }

    // --- expected_k ---

    #[test]
    fn expected_k_zero_and_negative_are_zero() {
        // Arrange + Act + Assert
        assert_eq!(expected_k(0.0), 0.0);
        assert_eq!(expected_k(-0.5), 0.0);
    }

    #[test]
    fn expected_k_half_is_one() {
        // Arrange + Act + Assert: p=0.5 -> 0.5 / 0.5 = 1.
        assert!((expected_k(0.5) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn expected_k_is_monotonic_non_decreasing() {
        // Arrange: an ascending sweep of probabilities.
        let ps = [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99, 1.0];

        // Act + Assert: each E[K] is >= the previous.
        let mut prev = expected_k(ps[0]);
        for &p in &ps[1..] {
            let cur = expected_k(p);
            assert!(cur >= prev, "E[K] dropped from {prev} to {cur} at p={p}");
            prev = cur;
        }
    }

    #[test]
    fn expected_k_at_full_success_is_finite_and_clamped() {
        // Arrange + Act: p=1.0 would be infinite without the clamp.
        let k = expected_k(1.0);

        // Assert: finite, and equal to the clamped horizon P_CLAMP/(1-P_CLAMP).
        assert!(k.is_finite(), "E[K] at p=1 must be finite, got {k}");
        let expected = P_CLAMP / (1.0 - P_CLAMP);
        assert!(
            (k - expected).abs() < 1e-9,
            "E[K] {k} != clamped {expected}"
        );
    }

    // --- estimate() ---

    #[test]
    fn estimate_keyless_query_is_cold() {
        // Arrange
        let store = Arc::new(KSessionStore::new());
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(None, Duration::from_mins(5)));

        // Assert
        assert_eq!(out.confidence, Confidence::Cold);
        assert_eq!(out.source, EstimateSource::ColdDefault);
        assert_eq!(out.samples, 0);
        assert_eq!((out.k_floor, out.k_point, out.k_ceiling), (0.0, 0.0, 0.0));
    }

    #[test]
    fn estimate_missing_key_is_cold() {
        // Arrange: a populated store, but the query names a different session.
        let store = Arc::new(KSessionStore::new());
        store.put(key("known"), window_from(&[(100, true)]));
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(Some("unknown"), Duration::from_mins(5)));

        // Assert
        assert_eq!(out.confidence, Confidence::Cold);
        assert_eq!(out.source, EstimateSource::ColdDefault);
    }

    #[test]
    fn estimate_thin_sample_is_low_with_forced_zero_floor() {
        // Arrange: fewer than CALIBRATED_MIN_TRIALS samples, all hits.
        let n = (CALIBRATED_MIN_TRIALS - 1) as u64;
        let samples: Vec<(u64, bool)> = (0..n).map(|i| (i, true)).collect();
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(Some("s1"), Duration::from_mins(5)));

        // Assert: Low, and the floor is force-clamped to 0 even though every
        // observed turn reused (a thin sample must never authorize a cut).
        assert_eq!(out.confidence, Confidence::Low);
        assert_eq!(out.k_floor, 0.0, "thin-sample floor must clamp to 0");
        assert!(out.k_point > 0.0, "point should reflect the observed reuse");
        assert_eq!(out.source, EstimateSource::LiveLedger);
    }

    #[test]
    fn estimate_at_threshold_is_calibrated_with_monotonic_bounds() {
        // Arrange: exactly CALIBRATED_MIN_TRIALS samples, a mix of hits/misses
        // so p_hat is strictly interior and the floor is non-trivial.
        let n = CALIBRATED_MIN_TRIALS as u64;
        let samples: Vec<(u64, bool)> = (0..n).map(|i| (i, i % 2 == 0)).collect();
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(Some("s1"), Duration::from_mins(5)));

        // Assert: Calibrated, floor no longer force-clamped, bounds ordered.
        assert_eq!(out.confidence, Confidence::Calibrated);
        assert!(out.k_floor >= 0.0);
        assert!(
            out.k_floor <= out.k_point,
            "floor {} <= point {}",
            out.k_floor,
            out.k_point
        );
        assert!(
            out.k_point <= out.k_ceiling,
            "point {} <= ceiling {}",
            out.k_point,
            out.k_ceiling
        );
        assert_eq!(out.samples, CALIBRATED_MIN_TRIALS);
    }

    #[test]
    fn estimate_all_miss_window_has_zero_floor_from_live_ledger() {
        // Arrange: a calibrated-size window where no turn observed reuse.
        let n = CALIBRATED_MIN_TRIALS as u64;
        let samples: Vec<(u64, bool)> = (0..n).map(|i| (i, false)).collect();
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(Some("s1"), Duration::from_mins(5)));

        // Assert: fail-closed -- the floor and point are 0, and the source is
        // LiveLedger because real samples were consulted.
        assert_eq!(out.confidence, Confidence::Calibrated);
        assert_eq!(out.k_floor, 0.0);
        assert_eq!(out.k_point, 0.0);
        assert_eq!(out.source, EstimateSource::LiveLedger);
    }

    #[test]
    fn estimate_all_hit_window_point_is_finite() {
        // Arrange: a full window of all-reuse turns -- p_hat = 1.0, which
        // would be an infinite horizon without the P_CLAMP guard.
        let samples: Vec<(u64, bool)> = (0..16u64).map(|i| (i, true)).collect();
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(Some("s1"), Duration::from_mins(5)));

        // Assert: Calibrated, and every bound is finite (clamped).
        assert_eq!(out.confidence, Confidence::Calibrated);
        assert!(out.k_point.is_finite() && out.k_point > 0.0);
        assert!(out.k_ceiling.is_finite());
        assert!(out.k_floor.is_finite());
        assert!(out.k_floor <= out.k_point && out.k_point <= out.k_ceiling);
    }

    #[test]
    fn estimate_single_contiguous_window_is_calibrated_starvation_fix() {
        // Arrange: one CONTIGUOUS run of >= CALIBRATED_MIN_TRIALS turns with
        // no TTL gaps. The v1 run-splitter would have scored this a SINGLE run
        // (n_runs = 1 < 8) and returned Low; the per-turn model counts each
        // turn as a trial, so n = 12 >= 8 and the estimate is Calibrated.
        // This is the starvation fix for few-long-contiguous sessions.
        let samples: Vec<(u64, bool)> = (0..12u64).map(|i| (i, i % 3 != 0)).collect();
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act: a large TTL -- the model no longer splits on gaps at all.
        let out = est.estimate(&query(Some("s1"), Duration::from_mins(5)));

        // Assert: the contiguous run is now Calibrated with a usable floor.
        assert_eq!(
            out.confidence,
            Confidence::Calibrated,
            "a single contiguous >= threshold window must now be Calibrated"
        );
        assert_eq!(out.samples, 12);
        assert!(out.k_floor >= 0.0 && out.k_floor <= out.k_point);
        assert_eq!(out.source, EstimateSource::LiveLedger);
    }
}
