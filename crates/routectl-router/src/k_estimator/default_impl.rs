//! Default ledger-backed K estimator.
//!
//! Answers a [`KQuery`] from the in-memory [`KSessionStore`] populated by the
//! live dispatch path and the [`super::rebuild`] data path. No IO on the hot
//! path; a query that names an unknown triple gets a `Cold` default rather
//! than failing.
//!
//! The percentile/confidence math is isolated in [`estimate_from_runs`], a
//! pure free function with its own tests, because it is the swappable,
//! validation-gated part of the design: a later increment tunes its
//! thresholds against real ledger data via a calibration harness. The
//! threshold constants below are PROVISIONAL v1 values, not a public
//! contract.

use std::sync::Arc;
use std::time::Duration;

use super::store::{KSessionKey, KSessionStore};
use super::{Confidence, EstimateSource, KEstimate, KEstimator, KQuery};

/// Floor percentile -- the conservative lower bound the cost gate may consult
/// to authorize a cut. PROVISIONAL; tuned by the calibration harness later.
const P_FLOOR: u32 = 10;
/// Point percentile -- the advisory-display best estimate. PROVISIONAL.
const P_POINT: u32 = 50;
/// Ceiling percentile -- reserved for the misfire-envelope headroom check.
/// PROVISIONAL.
const P_CEILING: u32 = 80;
/// Minimum number of observed runs before an estimate is `Calibrated` (the
/// floor may then gate a cut). Below this the estimate stays advisory-only.
/// PROVISIONAL; tuned later against real ledger data.
const CALIBRATED_MIN_RUNS: usize = 8;

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

        let samples = window.len() as u32;
        let run_reuse_counts = split_runs(&window, q.ttl);
        let (k_floor, k_point, k_ceiling, confidence) = estimate_from_runs(&run_reuse_counts);

        let source = match confidence {
            Confidence::Cold => EstimateSource::ColdDefault,
            _ => EstimateSource::LiveLedger,
        };

        KEstimate {
            k_floor,
            k_point,
            k_ceiling,
            samples,
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

/// Split a window's samples (oldest->newest) into runs and return each run's
/// directly-measured reuse count.
///
/// A new run begins when the gap between consecutive samples exceeds `ttl`
/// (the prior cache prefix would have aged out, so a new prefix was written).
/// A run's observation is the count of its samples that observed reuse --
/// the measured re-read count, not the run length.
fn split_runs(window: &super::store::KSessionWindow, ttl: Duration) -> Vec<u32> {
    let mut runs: Vec<u32> = Vec::new();
    let mut current: u32 = 0;
    let mut started = false;
    let mut prev_ts: Option<std::time::SystemTime> = None;

    for sample in window.iter() {
        let is_new_run = match prev_ts {
            None => true,
            Some(prev) => sample
                .ts
                .duration_since(prev)
                .map(|gap| gap > ttl)
                .unwrap_or(false),
        };

        if is_new_run && started {
            runs.push(current);
            current = 0;
        }
        started = true;

        if sample.observed_reuse {
            current += 1;
        }
        prev_ts = Some(sample.ts);
    }

    if started {
        runs.push(current);
    }
    runs
}

/// Pure percentile/confidence reducer over per-run reuse counts.
///
/// Returns `(k_floor, k_point, k_ceiling, confidence)`. This is the
/// explicitly swappable, validation-gated core of the estimator; keep it
/// simple and auditable. Percentiles use the nearest-rank method on the
/// ascending-sorted counts.
///
/// Confidence by run count `n`:
/// - `n == 0` -> `Cold`, all bounds 0.
/// - `1 <= n < CALIBRATED_MIN_RUNS` -> `Low`, and `k_floor` is clamped to 0
///   (a thin sample must not authorize a future cut).
/// - `n >= CALIBRATED_MIN_RUNS` -> `Calibrated`.
fn estimate_from_runs(run_reuse_counts: &[u32]) -> (f64, f64, f64, Confidence) {
    let n = run_reuse_counts.len();
    if n == 0 {
        return (0.0, 0.0, 0.0, Confidence::Cold);
    }

    let mut sorted: Vec<u32> = run_reuse_counts.to_vec();
    sorted.sort_unstable();

    let mut k_floor = percentile(&sorted, P_FLOOR);
    let k_point = percentile(&sorted, P_POINT);
    let k_ceiling = percentile(&sorted, P_CEILING);

    let confidence = if n >= CALIBRATED_MIN_RUNS {
        Confidence::Calibrated
    } else {
        k_floor = 0.0;
        Confidence::Low
    };

    (k_floor, k_point, k_ceiling, confidence)
}

/// Nearest-rank percentile over an ascending-sorted, non-empty slice.
///
/// The rank is `ceil(p/100 * n)` clamped to `[1, n]`, indexed as `rank - 1`.
/// `p == 0` maps to the first element. Never indexes out of bounds for any
/// non-empty input.
fn percentile(sorted: &[u32], p: u32) -> f64 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    // ceil(p * n / 100) without floating point, then clamp into [1, n].
    let rank = (p as usize * n).div_ceil(100);
    let rank = rank.clamp(1, n);
    sorted[rank - 1] as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k_estimator::store::{KSessionWindow, Sample};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    #[test]
    fn estimate_from_runs_empty_is_cold() {
        // Arrange + Act
        let (floor, point, ceiling, conf) = estimate_from_runs(&[]);

        // Assert
        assert_eq!((floor, point, ceiling), (0.0, 0.0, 0.0));
        assert_eq!(conf, Confidence::Cold);
    }

    #[test]
    fn estimate_from_runs_thin_sample_is_low_with_clamped_floor() {
        // Arrange: n=1 and n=2 are both below the calibration threshold, so
        // they are Low and the floor is clamped to 0 even though the sole
        // observation is non-zero.
        for counts in [vec![5u32], vec![5u32, 9u32]] {
            // Act
            let (floor, _point, _ceiling, conf) = estimate_from_runs(&counts);

            // Assert
            assert_eq!(conf, Confidence::Low, "n={} must be Low", counts.len());
            assert_eq!(floor, 0.0, "thin-sample floor must clamp to 0");
        }
    }

    #[test]
    fn estimate_from_runs_calibrated_at_threshold() {
        // Arrange: exactly CALIBRATED_MIN_RUNS runs.
        let counts: Vec<u32> = (1..=CALIBRATED_MIN_RUNS as u32).collect();

        // Act
        let (floor, point, ceiling, conf) = estimate_from_runs(&counts);

        // Assert: calibrated, and the floor is no longer force-clamped.
        assert_eq!(conf, Confidence::Calibrated);
        assert!(floor >= 0.0);
        assert!(floor <= point);
        assert!(point <= ceiling);
    }

    #[test]
    fn estimate_from_runs_percentiles_on_known_vector() {
        // Arrange: 1..=10. Nearest-rank ceil(p*n/100):
        //   p10 -> rank 1 -> 1; p50 -> rank 5 -> 5; p80 -> rank 8 -> 8.
        let counts: Vec<u32> = (1..=10).collect();

        // Act
        let (floor, point, ceiling, conf) = estimate_from_runs(&counts);

        // Assert
        assert_eq!(floor, 1.0);
        assert_eq!(point, 5.0);
        assert_eq!(ceiling, 8.0);
        assert_eq!(conf, Confidence::Calibrated);
    }

    #[test]
    fn estimate_from_runs_floor_le_point_le_ceiling() {
        // Arrange: an unsorted, calibrated-size sample.
        let counts = vec![9u32, 1, 4, 7, 2, 8, 3, 6, 5, 0];

        // Act
        let (floor, point, ceiling, _conf) = estimate_from_runs(&counts);

        // Assert: monotonic bounds.
        assert!(floor <= point, "floor {floor} <= point {point}");
        assert!(point <= ceiling, "point {point} <= ceiling {ceiling}");
    }

    #[test]
    fn percentile_does_not_panic_on_tiny_samples() {
        // n=1 and n=2 must be in-bounds for every percentile we use.
        for sorted in [vec![3u32], vec![3u32, 9u32]] {
            for p in [P_FLOOR, P_POINT, P_CEILING] {
                let v = percentile(&sorted, p);
                assert!(v >= 0.0, "percentile {p} on {sorted:?} = {v}");
            }
        }
    }

    #[test]
    fn estimate_keyless_query_is_cold() {
        // Arrange
        let store = Arc::new(KSessionStore::new());
        let est = LedgerBackedK::new(store);

        // Act
        let out = est.estimate(&query(None, Duration::from_secs(300)));

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
        let out = est.estimate(&query(Some("unknown"), Duration::from_secs(300)));

        // Assert
        assert_eq!(out.confidence, Confidence::Cold);
        assert_eq!(out.source, EstimateSource::ColdDefault);
    }

    #[test]
    fn estimate_single_contiguous_run_counts_measured_reuses() {
        // Arrange: 10 samples within one TTL window, every other one a hit.
        // No TTL gap -> a single run; reuse count is the number of hits (5),
        // not the run length.
        let samples: Vec<(u64, bool)> = (0..10).map(|i| (i, i % 2 == 0)).collect();
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act: TTL large enough that no gap splits the run.
        let out = est.estimate(&query(Some("s1"), Duration::from_secs(300)));

        // Assert: one run -> Low (below calibration), floor clamped, and the
        // point reflects the single measured reuse count of 5.
        assert_eq!(out.confidence, Confidence::Low);
        assert_eq!(out.k_floor, 0.0);
        assert_eq!(out.k_point, 5.0);
        assert_eq!(out.samples, 10);
        assert_eq!(out.source, EstimateSource::LiveLedger);
    }

    #[test]
    fn estimate_splits_runs_on_ttl_gap() {
        // Arrange: two clusters separated by a gap larger than the TTL. Each
        // cluster is a run; within each, count the measured reuses.
        // Run A at ts 0,1,2 (3 hits), run B at ts 1000,1001 (2 hits).
        let samples = vec![
            (0u64, true),
            (1, true),
            (2, true),
            (1_000, true),
            (1_001, true),
        ];
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act: TTL of 60s splits at the 1000s gap into two runs.
        let store_clone = est.store.clone();
        let window = store_clone.get(&key("s1")).expect("present");
        let runs = split_runs(&window, Duration::from_secs(60));

        // Assert: two runs with reuse counts 3 and 2.
        assert_eq!(runs.len(), 2);
        assert!(runs.contains(&3));
        assert!(runs.contains(&2));

        // And the estimate is LiveLedger (samples fed it), Low (two runs).
        let out = est.estimate(&query(Some("s1"), Duration::from_secs(60)));
        assert_eq!(out.source, EstimateSource::LiveLedger);
        assert_eq!(out.confidence, Confidence::Low);
        assert_eq!(out.samples, 5);
    }

    #[test]
    fn estimate_misses_on_zero_gap_uses_now_irrelevant() {
        // Arrange: a window whose samples are all misses (no reuse). The run
        // split still yields runs, but each measured reuse count is 0.
        let samples = vec![(0u64, false), (1, false), (2, false)];
        let store = Arc::new(KSessionStore::new());
        store.put(key("s1"), window_from(&samples));
        let est = LedgerBackedK::new(store);

        // Act
        let _: SystemTime = UNIX_EPOCH; // documents that `now` does not gate this path
        let out = est.estimate(&query(Some("s1"), Duration::from_secs(300)));

        // Assert: one run with reuse count 0 -> Low, all-zero bounds, but the
        // source is LiveLedger because real samples were consulted.
        assert_eq!(out.confidence, Confidence::Low);
        assert_eq!(out.k_point, 0.0);
        assert_eq!(out.source, EstimateSource::LiveLedger);
    }
}
