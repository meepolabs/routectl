//! Observability primitives for the MITM front-proxy.
//!
//! Realization note: these metrics were originally sketched as Prometheus-style
//! metrics (`rc_proxy_requests_total{leg,result_class,path_class}`, a
//! `rc_streams_open` gauge, and so on). routectl has no metrics
//! backend or exporter anywhere in the workspace, so this module does
//! NOT stand one up -- there is no `/metrics` HTTP endpoint and no
//! exporter dependency. Instead each named metric is a lock-free
//! `AtomicU64` counter (`Ordering::Relaxed`; never used for control
//! flow), mirroring the existing `routectl-usage` `UsageCounters`
//! pattern (see `routectl_usage::handle::UsageCounters`), and the
//! Prometheus-shaped names are kept only as label metadata surfaced
//! through structured `tracing` snapshots. If a real metrics backend
//! ever lands, [`ProxyMetrics::log_snapshot`] is the single seam to
//! swap for an exporter call.
//!
//! No token, credential, or request/response body ever flows into a
//! counter dimension or a log line here -- by construction, the only
//! inputs this module accepts are small closed enums plus the HTTP
//! method and path (for [`warn_once`]), never headers or payloads.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Which leg of the proxy a request/stream belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// Traffic classified as an inference call to an upstream model API.
    Inference,
    /// Traffic classified as a control-plane call (non-inference API
    /// surface on the same upstream host).
    ControlPlane,
    /// Traffic split-decided as an opaque blind tunnel (no MITM
    /// decryption applied).
    BlindTunnel,
}

impl Leg {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::Inference => 0,
            Self::ControlPlane => 1,
            Self::BlindTunnel => 2,
        }
    }
}

/// Coarse outcome class for a completed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultClass {
    Success,
    ClientError,
    ServerError,
    Unreachable,
}

impl ResultClass {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::ClientError => 1,
            Self::ServerError => 2,
            Self::Unreachable => 3,
        }
    }
}

/// Which path family a request landed in, independent of `Leg`
/// (`Leg` is the split decision; `PathClass` is what the path looked
/// like to the classifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Inference,
    ControlPlane,
    Unknown,
}

impl PathClass {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::Inference => 0,
            Self::ControlPlane => 1,
            Self::Unknown => 2,
        }
    }
}

/// Total distinct `(leg, result_class, path_class)` combinations, and
/// the flat-array size backing `requests_total`.
const REQUEST_DIMENSIONS: usize = Leg::COUNT * ResultClass::COUNT * PathClass::COUNT;

const fn request_index(leg: Leg, result_class: ResultClass, path_class: PathClass) -> usize {
    (leg.index() * ResultClass::COUNT + result_class.index()) * PathClass::COUNT
        + path_class.index()
}

/// Lock-free observability counters for the MITM front-proxy.
///
/// Every increment is a single relaxed atomic op: cheap, never blocks,
/// never panics. Safe to hold behind a shared `Arc` and clone-share
/// across the listener's per-connection tasks. See the module doc for
/// why these are plain atomics rather than a Prometheus registry.
#[derive(Debug)]
pub struct ProxyMetrics {
    /// `rc_proxy_requests_total{leg,result_class,path_class}`, flattened
    /// into one array indexed by [`request_index`].
    requests_total: [AtomicU64; REQUEST_DIMENSIONS],
    /// `rc_streams_open` (gauge-like: inc on open, dec on close).
    streams_open: AtomicU64,
    rc_stream_idle_aborts_total: AtomicU64,
    rc_unknown_forwarded_paths_total: AtomicU64,
    rc_tls_handshake_failures_total: AtomicU64,
    rc_tls_handshake_timeouts_total: AtomicU64,
}

impl Default for ProxyMetrics {
    fn default() -> Self {
        Self {
            requests_total: std::array::from_fn(|_| AtomicU64::new(0)),
            streams_open: AtomicU64::new(0),
            rc_stream_idle_aborts_total: AtomicU64::new(0),
            rc_unknown_forwarded_paths_total: AtomicU64::new(0),
            rc_tls_handshake_failures_total: AtomicU64::new(0),
            rc_tls_handshake_timeouts_total: AtomicU64::new(0),
        }
    }
}

impl ProxyMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bumps `rc_proxy_requests_total` for the given dimension triple.
    pub fn incr_request(&self, leg: Leg, result_class: ResultClass, path_class: PathClass) {
        self.requests_total[request_index(leg, result_class, path_class)]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Current count for one `(leg, result_class, path_class)` triple.
    pub fn request_count(&self, leg: Leg, result_class: ResultClass, path_class: PathClass) -> u64 {
        self.requests_total[request_index(leg, result_class, path_class)].load(Ordering::Relaxed)
    }

    /// Sum of `rc_proxy_requests_total` across every dimension.
    pub fn requests_total(&self) -> u64 {
        self.requests_total
            .iter()
            .map(|counter| counter.load(Ordering::Relaxed))
            .sum()
    }

    /// A stream opened: bumps the `rc_streams_open` gauge up by one.
    pub fn stream_opened(&self) {
        self.streams_open.fetch_add(1, Ordering::Relaxed);
    }

    /// A stream closed: bumps the `rc_streams_open` gauge down by one,
    /// saturating at zero so a close that races an open (or an
    /// unmatched close) can never underflow the counter or panic.
    pub fn stream_closed(&self) {
        let mut current = self.streams_open.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(1);
            match self.streams_open.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Current value of the `rc_streams_open` gauge.
    pub fn streams_open(&self) -> u64 {
        self.streams_open.load(Ordering::Relaxed)
    }

    pub fn incr_stream_idle_aborts(&self) {
        self.rc_stream_idle_aborts_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn stream_idle_aborts_total(&self) -> u64 {
        self.rc_stream_idle_aborts_total.load(Ordering::Relaxed)
    }

    pub fn incr_unknown_forwarded_paths(&self) {
        self.rc_unknown_forwarded_paths_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn unknown_forwarded_paths_total(&self) -> u64 {
        self.rc_unknown_forwarded_paths_total
            .load(Ordering::Relaxed)
    }

    /// Bumps `rc_tls_handshake_failures_total` and returns the
    /// post-increment count, atomically (`fetch_add`'s own return
    /// value, not a separate load). Callers that gate a loud log on
    /// "every Nth cumulative failure" (see `proxy::mitm`) MUST use this
    /// return value rather than incrementing and then calling
    /// [`Self::tls_handshake_failures_total`] separately -- under
    /// concurrent handshake failures from multiple connections, a
    /// separate load can race and either double-fire or skip the
    /// threshold entirely, exactly during the failure burst the
    /// threshold exists to surface.
    pub fn incr_tls_handshake_failures(&self) -> u64 {
        self.rc_tls_handshake_failures_total
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }

    pub fn tls_handshake_failures_total(&self) -> u64 {
        self.rc_tls_handshake_failures_total.load(Ordering::Relaxed)
    }

    pub fn incr_tls_handshake_timeouts(&self) {
        self.rc_tls_handshake_timeouts_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn tls_handshake_timeouts_total(&self) -> u64 {
        self.rc_tls_handshake_timeouts_total.load(Ordering::Relaxed)
    }

    /// Emits one structured `tracing::debug!` line carrying every
    /// counter's current value. Fields are all counter names + numeric
    /// values -- no token, header, or body content ever reaches this
    /// call.
    pub fn log_snapshot(&self) {
        tracing::debug!(
            target: "routectl_cli::proxy::metrics",
            rc_proxy_requests_total = self.requests_total(),
            rc_streams_open = self.streams_open(),
            rc_stream_idle_aborts_total = self.stream_idle_aborts_total(),
            rc_unknown_forwarded_paths_total = self.unknown_forwarded_paths_total(),
            rc_tls_handshake_failures_total = self.tls_handshake_failures_total(),
            rc_tls_handshake_timeouts_total = self.tls_handshake_timeouts_total(),
            "proxy metrics snapshot"
        );
    }
}

/// Dedups repeated WARN emissions for the same `(method, path)` pair.
///
/// Backed by a plain `Mutex<HashSet<...>>` (std-only, no new deps):
/// the MITM proxy's WARN-worthy events (an unknown forwarded path, for
/// instance) are cheap enough and rare enough on the hot path that a
/// mutex is simpler and just as safe as a lock-free structure here --
/// unlike `ProxyMetrics`'s per-request counters, this is not called
/// once per request in steady state, only once per newly-seen pair.
///
/// The set is capped at [`WARN_ONCE_CAP`] distinct pairs: it is keyed on
/// request-derived data (the exact path a client sends), so without a
/// bound a client that sends many distinct unrecognized paths -- an
/// attacker or a runaway client bug -- would grow this set without
/// limit for the life of the process. Past the cap, this degrades to
/// **never warn** for a newly-seen pair rather than warning on every
/// request for it: the one-time cap-reached log below is the signal an
/// operator needs that something pathological is happening, and staying
/// quiet after that avoids turning the degradation itself into a second
/// unbounded log-volume problem.
#[derive(Debug, Default)]
pub struct WarnOnce {
    seen: Mutex<HashSet<(String, String)>>,
    cap_reached: AtomicBool,
}

/// Upper bound on the number of distinct `(method, path)` pairs
/// [`WarnOnce`] tracks. Not a tuning knob -- 1024 distinct
/// never-before-seen proxy paths is already far past what any
/// legitimate Claude Code / Anthropic surface would ever produce.
const WARN_ONCE_CAP: usize = 1024;

/// Outcome of checking one `(method, path)` pair against the tracked
/// set, decided while holding the lock so the check-then-act is atomic.
enum WarnDecision {
    NewlyTracked,
    AlreadyTracked,
    CapReached,
}

fn decide_and_track(seen: &mut HashSet<(String, String)>, key: (String, String)) -> WarnDecision {
    if seen.contains(&key) {
        return WarnDecision::AlreadyTracked;
    }
    if seen.len() >= WARN_ONCE_CAP {
        return WarnDecision::CapReached;
    }
    seen.insert(key);
    WarnDecision::NewlyTracked
}

impl WarnOnce {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emits exactly one `tracing::warn!` for a given `(method, path)`
    /// pair for the lifetime of this `WarnOnce`, up to [`WARN_ONCE_CAP`]
    /// distinct pairs. Returns `true` if this call was the one that
    /// emitted (a fresh pair, tracked below the cap), `false` otherwise
    /// (an already-seen pair, or a fresh pair arriving once the cap has
    /// been reached). Never panics: a poisoned mutex (only reachable if
    /// a prior holder panicked while holding the lock, which nothing in
    /// this method does) falls back to treating the pair as unseen and
    /// still warns, rather than propagating a panic onto the proxy hot
    /// path.
    pub fn warn_once(&self, method: &str, path: &str) -> bool {
        let key = (method.to_string(), path.to_string());
        let decision = match self.seen.lock() {
            Ok(mut seen) => decide_and_track(&mut seen, key),
            Err(poisoned) => decide_and_track(&mut poisoned.into_inner(), key),
        };
        match decision {
            WarnDecision::NewlyTracked => {
                tracing::warn!(
                    target: "routectl_cli::proxy::metrics",
                    method,
                    path,
                    "unrecognized proxy request path (first occurrence)"
                );
                true
            }
            WarnDecision::AlreadyTracked => false,
            WarnDecision::CapReached => {
                if !self.cap_reached.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        target: "routectl_cli::proxy::metrics",
                        cap = WARN_ONCE_CAP,
                        "WarnOnce dedup set reached its cap -- further distinct \
                         unrecognized paths will no longer be individually warned"
                    );
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incr_request_bumps_only_the_matching_dimension() {
        let metrics = ProxyMetrics::new();

        metrics.incr_request(Leg::Inference, ResultClass::Success, PathClass::Inference);

        assert_eq!(
            metrics.request_count(Leg::Inference, ResultClass::Success, PathClass::Inference),
            1
        );
        assert_eq!(
            metrics.request_count(
                Leg::ControlPlane,
                ResultClass::Success,
                PathClass::Inference
            ),
            0
        );
        assert_eq!(
            metrics.request_count(
                Leg::Inference,
                ResultClass::ClientError,
                PathClass::Inference
            ),
            0
        );
        assert_eq!(metrics.requests_total(), 1);
    }

    #[test]
    fn incr_request_accumulates_across_calls_and_dimensions() {
        let metrics = ProxyMetrics::new();

        metrics.incr_request(Leg::Inference, ResultClass::Success, PathClass::Inference);
        metrics.incr_request(Leg::Inference, ResultClass::Success, PathClass::Inference);
        metrics.incr_request(
            Leg::BlindTunnel,
            ResultClass::Unreachable,
            PathClass::Unknown,
        );

        assert_eq!(
            metrics.request_count(Leg::Inference, ResultClass::Success, PathClass::Inference),
            2
        );
        assert_eq!(
            metrics.request_count(
                Leg::BlindTunnel,
                ResultClass::Unreachable,
                PathClass::Unknown
            ),
            1
        );
        assert_eq!(metrics.requests_total(), 3);
    }

    #[test]
    fn streams_open_inc_dec_round_trips() {
        let metrics = ProxyMetrics::new();

        metrics.stream_opened();
        metrics.stream_opened();
        assert_eq!(metrics.streams_open(), 2);

        metrics.stream_closed();
        assert_eq!(metrics.streams_open(), 1);
    }

    #[test]
    fn streams_open_close_never_underflows_or_panics() {
        let metrics = ProxyMetrics::new();

        // No matching open -- an unmatched close must saturate at zero,
        // not wrap around or panic.
        metrics.stream_closed();
        metrics.stream_closed();
        metrics.stream_closed();

        assert_eq!(metrics.streams_open(), 0);

        metrics.stream_opened();
        metrics.stream_closed();
        metrics.stream_closed();
        assert_eq!(metrics.streams_open(), 0);
    }

    #[test]
    fn simple_counters_inc_and_load() {
        let metrics = ProxyMetrics::new();

        metrics.incr_stream_idle_aborts();
        metrics.incr_stream_idle_aborts();
        metrics.incr_unknown_forwarded_paths();
        metrics.incr_tls_handshake_failures();
        metrics.incr_tls_handshake_timeouts();

        assert_eq!(metrics.stream_idle_aborts_total(), 2);
        assert_eq!(metrics.unknown_forwarded_paths_total(), 1);
        assert_eq!(metrics.tls_handshake_failures_total(), 1);
        assert_eq!(metrics.tls_handshake_timeouts_total(), 1);
    }

    #[test]
    fn log_snapshot_does_not_panic() {
        let metrics = ProxyMetrics::new();
        metrics.incr_request(Leg::Inference, ResultClass::Success, PathClass::Inference);
        metrics.stream_opened();

        metrics.log_snapshot();
    }

    #[test]
    fn warn_once_emits_exactly_once_per_pair() {
        let warn_once = WarnOnce::new();

        let first = warn_once.warn_once("GET", "/v1/unknown");
        let second = warn_once.warn_once("GET", "/v1/unknown");

        assert!(first, "first call for a new pair must emit");
        assert!(!second, "second call for the same pair must not re-emit");
    }

    #[test]
    fn warn_once_treats_distinct_pairs_independently() {
        let warn_once = WarnOnce::new();

        assert!(warn_once.warn_once("GET", "/v1/a"));
        assert!(
            warn_once.warn_once("POST", "/v1/a"),
            "different method is a distinct pair"
        );
        assert!(
            warn_once.warn_once("GET", "/v1/b"),
            "different path is a distinct pair"
        );
        assert!(
            !warn_once.warn_once("GET", "/v1/a"),
            "repeat of first pair must not re-emit"
        );
    }

    #[test]
    fn warn_once_stops_growing_and_warning_once_the_cap_is_reached() {
        let warn_once = WarnOnce::new();

        for i in 0..WARN_ONCE_CAP {
            assert!(
                warn_once.warn_once("GET", &format!("/v1/distinct-path-{i}")),
                "every one of the first {WARN_ONCE_CAP} distinct pairs must emit"
            );
        }
        assert_eq!(warn_once.seen.lock().unwrap().len(), WARN_ONCE_CAP);
        assert!(!warn_once.cap_reached.load(Ordering::Relaxed));

        // The (CAP + 1)th distinct pair pushes past the cap: it must not
        // emit its own "first occurrence" warning, but must be the one
        // call that flips the one-time cap-reached warning.
        assert!(!warn_once.warn_once("GET", "/v1/one-too-many"));
        assert!(warn_once.cap_reached.load(Ordering::Relaxed));

        // Further distinct pairs past the cap never warn again (the
        // never-warn-past-cap degradation), and the set never grows
        // past the cap.
        assert!(!warn_once.warn_once("GET", "/v1/still-more"));
        assert!(!warn_once.warn_once("POST", "/v1/yet-another"));
        assert_eq!(warn_once.seen.lock().unwrap().len(), WARN_ONCE_CAP);

        // Pairs tracked before the cap was reached keep their existing
        // dedup behavior unaffected by the cap.
        assert!(!warn_once.warn_once("GET", "/v1/distinct-path-0"));
    }
}
