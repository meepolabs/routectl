//! In-memory per-lane evidence store for the token-estimate correction.
//!
//! One bounded ring of recent ratio samples per lane, behind a single mutex.
//! Deliberately NOT the bounded-LRU shape the cache-reuse tracker uses: that
//! keyspace is inbound-session-keyed and therefore client-driven and
//! unbounded, whereas a lane is `(provider_kind, operator-declared nickname)`
//! and so bounded by the config the daemon loaded. A plain map cannot grow
//! past the declared model table.
//!
//! The store holds no request content and no session identifier: a sample
//! carries a timestamp, an integer ratio, and an opaque cohort tag.

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use parking_lot::Mutex;

use super::factor::{Factor, IDENTITY_PERMILLE, reduce};

/// Bound on the samples retained per lane. The cap is the memory bound: the
/// per-process floor is the declared lane count times this, regardless of
/// traffic shape. Comfortably above the reduction's sample and cohort floors
/// so a lane can hold several cohorts' worth of recent history.
pub const SAMPLES_PER_LANE: usize = 64;

/// Identity of one calibration lane.
///
/// The model dimension is the SERVED NICKNAME -- the operator-facing label --
/// and never the upstream wire id. Pricing legitimately keys on the wire id;
/// this does not, and a query keyed on it would silently never match a
/// written sample, holding every lane cold forever while looking healthy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LaneKey {
    /// Stable provider-kind token of the served target (`anthropic-api` |
    /// `openai-compat` | `bedrock` | `openai-responses`).
    pub provider_kind: String,
    /// Served model NICKNAME, not the upstream wire id.
    pub nickname: String,
}

/// One request's worth of evidence about a lane's estimator error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// When the observation was recorded, for the reduction's age bound.
    pub ts: SystemTime,
    /// `actual / estimate` as integer permille. Above
    /// [`IDENTITY_PERMILLE`](super::factor::IDENTITY_PERMILLE) means the
    /// estimator under-counted this request.
    pub permille: u32,
    /// Opaque grouping tag for the caller this observation came from. Used
    /// ONLY to give each caller one vote in the reduction, so no single
    /// high-volume caller defines a lane. Never part of [`LaneKey`].
    pub cohort: u64,
}

/// Bounded ring of a lane's recent samples, oldest first.
///
/// The cap is enforced inside [`LaneSamples::push`] so no caller can grow a
/// lane past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSamples {
    samples: VecDeque<Sample>,
}

impl Default for LaneSamples {
    /// An empty ring, pre-sized to the cap it will reach: a busy lane fills
    /// to `SAMPLES_PER_LANE` and stays there, so allocating once beats
    /// re-growing on the dispatch path.
    fn default() -> Self {
        Self {
            samples: VecDeque::with_capacity(SAMPLES_PER_LANE),
        }
    }
}

impl LaneSamples {
    /// Append a sample, dropping the oldest when the ring is full.
    pub fn push(&mut self, sample: Sample) {
        if self.samples.len() == SAMPLES_PER_LANE {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Retained samples in arrival order.
    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.samples.iter()
    }
}

/// Lane-keyed evidence map. Interior-locked, so it is read and written
/// through a shared reference on the dispatch path.
#[derive(Debug, Default)]
pub struct CalibrationStore {
    lanes: Mutex<HashMap<LaneKey, LaneSamples>>,
}

impl CalibrationStore {
    /// Turn one estimate/actual pair into a stored sample for `key`, reporting
    /// whether it was stored.
    ///
    /// A pair with a zero on either side is dropped rather than stored: a zero
    /// estimate has no ratio at all, and a zero actual is an upstream that
    /// reported nothing rather than a request that genuinely cost nothing --
    /// storing it would drag the lane's reduced ratio toward zero, which is
    /// the direction that makes the window gate admit oversized requests.
    ///
    /// The live dispatch path ignores the return value; the warm rebuild reads
    /// it to tally what it dropped, which is what keeps the two paths sharing
    /// ONE admission rule instead of each re-deciding it.
    pub fn record(
        &self,
        key: LaneKey,
        estimated: u64,
        actual: u64,
        cohort: u64,
        ts: SystemTime,
    ) -> bool {
        if estimated == 0 || actual == 0 {
            return false;
        }
        let Some(permille) = permille_ratio(estimated, actual) else {
            return false;
        };
        let sample = Sample {
            ts,
            permille,
            cohort,
        };
        let mut guard = self.lanes.lock();
        guard.entry(key).or_default().push(sample);
        true
    }

    /// The correction for `key`, or `None` when the lane has no usable one.
    ///
    /// `None` covers every refusal cause -- unseen lane, too little evidence,
    /// evidence too old, and a reduced ratio outside the sane band -- so the
    /// one decision site has exactly one fallback path.
    pub fn factor_for(&self, key: &LaneKey, now: SystemTime) -> Option<Factor> {
        let guard = self.lanes.lock();
        reduce(guard.get(key)?, now)
    }

    /// Number of lanes holding at least one sample. Test read surface today;
    /// ungate alongside a lane-count diagnostic.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lanes.lock().len()
    }

    /// True when no lane holds a sample. Test read surface today, paired with
    /// [`CalibrationStore::len`].
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.lanes.lock().is_empty()
    }

    /// How many lanes currently produce a correction, judged against `now`.
    ///
    /// Runs the SAME reduction the gate's lookup runs, so a lane counted here
    /// is a lane the gate would correct -- there is no second notion of
    /// "calibrated" to drift from it. Boot-observability read surface for the
    /// warm rebuild's summary.
    pub fn calibrated_lane_count(&self, now: SystemTime) -> usize {
        self.lanes
            .lock()
            .values()
            .filter(|samples| reduce(samples, now).is_some())
            .count()
    }

    /// Snapshot every lane and its samples, for carrying the learned state
    /// across a hot-reload rebuild.
    pub fn export_entries(&self) -> Vec<(LaneKey, LaneSamples)> {
        self.lanes
            .lock()
            .iter()
            .map(|(key, samples)| (key.clone(), samples.clone()))
            .collect()
    }

    /// Install snapshotted entries, replacing any lane of the same key.
    ///
    /// No ordering discipline is needed on the way in (unlike the LRU-shaped
    /// session stores): this map never evicts, so there is no eviction
    /// frontier a scattered replay could disturb.
    pub fn import_entries(&self, entries: Vec<(LaneKey, LaneSamples)>) {
        let mut guard = self.lanes.lock();
        for (key, samples) in entries {
            guard.insert(key, samples);
        }
    }
}

/// `actual / estimated` in permille, or `None` when the ratio does not fit a
/// `u32` (an actual over four million times the estimate -- unreachable from
/// a real pair, and refusing beats truncating into the sane band).
fn permille_ratio(estimated: u64, actual: u64) -> Option<u32> {
    let scaled = actual.checked_mul(u64::from(IDENTITY_PERMILLE))?;
    u32::try_from(scaled / estimated).ok()
}

/// The opaque cohort tag a caller's session key reduces to.
///
/// The ONE derivation, shared by the live write and the boot warm rebuild: two
/// derivations would let the same caller count as two cohorts across a
/// restart, which is exactly how a lane could clear the distinct-cohort floor
/// on evidence one caller produced. Every keyless request shares tag zero, so
/// a lane fed only keyless traffic never clears that floor at all.
///
/// The hash is process-salted, so a tag is never comparable across restarts --
/// which is fine, because nothing persists one: the rebuild re-derives every
/// tag from the session ids it just read.
pub fn cohort_of(session_key: Option<&str>) -> u64 {
    session_key.map_or(0, crate::log_hash::salted_log_hash)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
