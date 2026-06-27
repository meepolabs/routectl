//! Per-session cache-reuse tracker for the cost gate's advisory output.
//!
//! Exposes a `KEstimator` trait that, given a (session, provider_kind, model)
//! triple, returns a confidence-bracketed estimate of how many times a
//! freshly-written cache prefix will be re-read before it ages out. The
//! advisory steady-state trimmer consults this to price its would-cut
//! candidates: a low-confidence cold estimate must stay advisory-only, while
//! a calibrated estimate can in the future gate a live trim.
//!
//! This module ships the contract (trait, types, session-keyed store) plus
//! its carry-over discipline. A default implementation that reads the live
//! usage ledger, and the dispatch helper that consults it, land in additive
//! follow-up work; until then nothing in the router calls into here.

pub mod default_impl;
pub mod rebuild;
pub mod store;

pub use default_impl::LedgerBackedK;
pub use rebuild::{rebuild_into, LedgerReader, LedgerSampleRow};
pub use store::{KSessionKey, KSessionStore, KSessionWindow, Sample, K_SESSION_CAPACITY};

use std::time::{Duration, SystemTime};

/// Query parameters for a single K estimation. Borrowed so the call site
/// never pays an allocation on the hot path.
///
/// `session_key` is optional: a request that arrives without a session
/// identifier (a one-shot count_tokens probe, an unauthenticated dev call)
/// still gets a `Cold` estimate, just keyed only by (provider_kind, model).
///
/// `ttl` is the cache prefix's expected lifetime as configured for the
/// served target; `now` is the dispatch-time wall clock. Both are passed in
/// rather than read inside the estimator so tests can pin them.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct KQuery<'a> {
    /// Inbound session identifier (typically the conversation-scoped token
    /// that also keys [`crate::seat_pool::StickyPins`]). `None` for one-shot
    /// requests that carry no session.
    pub session_key: Option<&'a str>,
    /// Stable provider-kind token of the served target (`anthropic-api` |
    /// `openai-compat` | `bedrock` | `openai-responses`).
    pub provider_kind: &'a str,
    /// Served model nickname -- the operator-facing label, not the upstream
    /// wire id.
    pub model: &'a str,
    /// Configured TTL of the cache prefix the trimmer is pricing.
    pub ttl: Duration,
    /// Dispatch-time wall clock used to age live ledger samples.
    pub now: SystemTime,
}

/// A bracketed estimate of expected cache re-reads for a single (session,
/// provider_kind, model) triple.
///
/// Three bounds rather than a bare point estimate: the floor is the only
/// bound the cost gate may consult to actually CUT a prefix (a misfire
/// there is irreversible cold-miss), the point estimate is for advisory
/// display, and the ceiling reserves headroom for an upcoming
/// misfire-envelope check.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct KEstimate {
    /// Lower confidence bound. The cost gate may consult ONLY this bound
    /// when deciding whether to actually cut a prefix; the point and
    /// ceiling are advisory-only.
    pub k_floor: f64,
    /// Best point estimate. Display only -- never gates a cut.
    pub k_point: f64,
    /// Upper confidence bound. Reserved for the misfire envelope check
    /// that lands with the live-cut wiring; advisory-only until then.
    pub k_ceiling: f64,
    /// Number of live samples that fed the estimate. Zero for a cold
    /// default.
    pub samples: u32,
    /// Confidence class. The cost gate refuses to cut on anything below
    /// `Calibrated`.
    pub confidence: Confidence,
    /// Provenance: where the numbers came from. Surfaced in advisory
    /// columns so an operator reading the ledger can tell a learned
    /// estimate from a cold default at a glance.
    pub source: EstimateSource,
}

/// Confidence class for a [`KEstimate`].
///
/// `#[non_exhaustive]` because additive ranks (e.g. `Stale` for a session
/// whose ledger samples are older than the TTL) will land without bumping
/// a major version.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// No samples observed for this triple. The floor is a hard 0; the
    /// point and ceiling fall back to a conservative default.
    Cold,
    /// Some samples, but below the calibration threshold. Advisory display
    /// only; the cost gate must not cut.
    Low,
    /// Enough samples to set a defensible floor. The cost gate may consult
    /// the floor to authorize a cut.
    Calibrated,
}

/// Provenance of the estimate. Surfaced for diagnostic display only.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EstimateSource {
    /// Derived from the live ledger window for this triple.
    LiveLedger,
    /// Derived from a longer-window rebuild (snapshot) when the live
    /// window held too few samples.
    RebuildOnly,
    /// No samples available; cold default.
    ColdDefault,
}

/// Abstract K estimator. The default implementation reads the usage ledger;
/// tests substitute a stub. `Send + Sync` so the router can hold one behind
/// an `Arc` and share it across dispatch tasks.
pub trait KEstimator: Send + Sync {
    /// Return an estimate for `q`. Implementations must be cheap on the
    /// hot path (no IO, no allocation in the steady state) and must never
    /// panic on a query that names an unknown triple -- a cold default is
    /// always a valid answer.
    fn estimate(&self, q: &KQuery<'_>) -> KEstimate;
}
