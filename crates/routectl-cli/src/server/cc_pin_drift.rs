//! Compiled-pin drift guard: warns when an inbound client self-reports a
//! Claude Code CLI version that differs from the one THIS BUILD mints.
//!
//! Distinct from the MITM `proxy::cc_version` check in both question and
//! vocabulary. That one compares the wire against a version the OPERATOR
//! recorded as tested in `[mitm]`, is opt-in, and only sees the
//! front-proxy leg. This one compares the wire against
//! `routectl_core::identity::anthropic::compiled_claude_cli_version` --
//! the fingerprint routectl actually presents upstream -- and observes at
//! the HTTP ingress, so it fires in base-url mode too. Front-proxy traffic
//! can produce both warnings when both contracts drift; the log targets
//! and field names keep them separable.
//!
//! Owned by `AppState`, which outlives router hot-swaps, so a reload does
//! not re-warn a version already reported. A test builds its own
//! `AppState` and therefore its own guard, which is why the dedup state
//! lives on the instance rather than in a process global.

use axum::http::HeaderMap;

use crate::warn_dedup::{CappedWarnSet, WarnDecision};

/// Log target both events on this guard carry. Distinct from the MITM
/// check's target so an operator can filter the two questions apart.
const DRIFT_TARGET: &str = "routectl_cli::server::cc_pin_drift";

/// Fixed token naming WHERE the observed version came from. A static
/// class word, never request-derived: the header itself, the full
/// User-Agent, and any billing attribution text stay out of the log.
const OBSERVATION_SOURCE: &str = "ingress_user_agent";

/// Upper bound on the distinct mismatched versions this guard remembers.
/// Keyed on client-supplied data, so an unbounded set would grow for the
/// life of the process under a runaway or hostile client. Its own constant
/// rather than a shared one: this bounds a version population and the
/// proxy's `WarnOnce` bounds a request-path population, so a change in
/// either should not follow the other.
const CC_PIN_DRIFT_WARN_CAP: usize = 1024;

/// Warns once per DISTINCT observed version that differs from the
/// compiled pin. A set rather than a last-seen slot: two clients on
/// different versions alternating through one routectl would otherwise
/// re-warn on every request forever, each one making the other's record
/// stale. The bounded dedup decision itself lives in
/// `warn_dedup::CappedWarnSet`; this type owns the comparison,
/// the log target, and the field vocabulary.
#[derive(Debug)]
pub struct CcPinDriftGuard {
    seen: CappedWarnSet<String>,
}

impl Default for CcPinDriftGuard {
    fn default() -> Self {
        Self::with_cap(CC_PIN_DRIFT_WARN_CAP)
    }
}

impl CcPinDriftGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// A guard with an explicit dedup bound, so a test can reach the cap
    /// boundary in a handful of calls instead of a thousand. Production
    /// builds one through [`Self::new`].
    ///
    /// `pub` FOR A CROSS-BINARY TEST, not for callers: the log-contract test
    /// lives in its own integration binary (a thread-local capture
    /// subscriber over a shared `warn!` callsite is unreliable inside the
    /// lib test binary, where sibling tests poison tracing's per-callsite
    /// `Interest` cache first), and an integration binary cannot see a
    /// `pub(crate)` item. Hidden from the docs so the widened visibility
    /// does not read as an offered API.
    #[doc(hidden)]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            seen: CappedWarnSet::new(cap),
        }
    }

    /// Observe the inbound `User-Agent`, if it carries a stable Claude
    /// Code version. Returns `true` iff this call emitted the drift
    /// warning.
    ///
    /// SILENCE CEILING: a missing, non-UTF-8, non-Claude-Code, or
    /// unstable-token User-Agent is no observation at all, so no warning
    /// says nothing about whether drift exists. A direct library caller
    /// sends no headers and is unobservable by construction.
    ///
    /// `pub` for the same cross-binary-test reason as [`Self::with_cap`];
    /// the production call sites are both in `crate::handlers`.
    #[doc(hidden)]
    pub fn observe_headers(&self, headers: &HeaderMap) -> bool {
        let observed = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .and_then(routectl_core::identity::anthropic::parse_claude_cli_version);
        self.observe_version(observed)
    }

    /// Compare `observed` against the compiled pin and emit at most one
    /// warning per distinct mismatched value for this guard's lifetime,
    /// plus at most one cap-reached line. Returns `true` iff this call was
    /// the one that emitted the per-version warning, which is the seam
    /// tests assert on instead of scraping logs. Never refuses a request --
    /// this guard only decides whether to log.
    ///
    /// `observed` must come from the STRICT parser: every value stored here
    /// becomes a dedup key and a log field, and a per-request-varying token
    /// would defeat the dedup and grow the set.
    ///
    /// `pub` for the same cross-binary-test reason as [`Self::with_cap`].
    #[doc(hidden)]
    pub fn observe_version(&self, observed: Option<&str>) -> bool {
        let pinned = routectl_core::identity::anthropic::compiled_claude_cli_version();
        let Some(observed) = observed else {
            return false;
        };
        if observed == pinned {
            return false;
        }

        match self.seen.admit(observed.to_string()) {
            WarnDecision::Emit => {
                tracing::warn!(
                    target: DRIFT_TARGET,
                    pinned_cc_version = pinned,
                    ingress_cc_version = observed,
                    source = OBSERVATION_SOURCE,
                    "client self-reports a Claude Code version this build does not mint -- \
                     the outbound fingerprint is behind the client (never hard-refused)"
                );
                true
            }
            WarnDecision::AlreadyWarned => false,
            WarnDecision::CapReached { notice } => {
                if notice {
                    tracing::warn!(
                        target: DRIFT_TARGET,
                        cap = self.seen.cap(),
                        source = OBSERVATION_SOURCE,
                        "Claude Code version dedup set reached its cap -- further distinct \
                         mismatched versions will no longer be individually warned"
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

    const PINNED: &str = routectl_core::identity::anthropic::compiled_claude_cli_version();

    fn headers_with_ua(ua: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::USER_AGENT, ua.parse().unwrap());
        headers
    }

    #[test]
    fn a_client_matching_the_compiled_pin_never_warns() {
        let guard = CcPinDriftGuard::new();

        assert!(!guard.observe_version(Some(PINNED)));
    }

    #[test]
    fn a_mismatched_version_warns_exactly_once() {
        let guard = CcPinDriftGuard::new();

        assert!(guard.observe_version(Some("9.9.9")));

        assert!(
            !guard.observe_version(Some("9.9.9")),
            "a steady mismatch must not warn on every request"
        );
    }

    #[test]
    fn each_distinct_mismatched_version_warns_once_under_alternation() {
        let guard = CcPinDriftGuard::new();

        assert!(guard.observe_version(Some("9.9.9")));
        assert!(guard.observe_version(Some("9.9.8")));
        let realternated = [
            guard.observe_version(Some("9.9.9")),
            guard.observe_version(Some("9.9.8")),
            guard.observe_version(Some("9.9.9")),
        ];

        assert_eq!(
            realternated,
            [false, false, false],
            "two clients alternating versions must warn once EACH, not forever"
        );
    }

    #[test]
    fn a_missing_or_unparseable_version_never_warns() {
        let guard = CcPinDriftGuard::new();

        assert!(!guard.observe_version(None));

        assert!(!guard.observe_version(None), "still silent on repeat");
    }

    #[test]
    fn the_dedup_set_stops_growing_at_its_cap() {
        let guard = CcPinDriftGuard::new();

        for n in 0..CC_PIN_DRIFT_WARN_CAP {
            assert!(
                guard.observe_version(Some(&format!("9.9.{n}"))),
                "every distinct version below the cap warns"
            );
        }
        let past_cap = guard.observe_version(Some("8.0.0"));
        let past_cap_again = guard.observe_version(Some("8.0.1"));

        assert!(
            !past_cap && !past_cap_again,
            "past the cap the guard degrades to never-warn, not warn-every-request"
        );
        assert_eq!(guard.seen.len(), CC_PIN_DRIFT_WARN_CAP);
    }

    #[test]
    fn the_cap_reached_notice_is_claimed_exactly_once_at_the_boundary() {
        // The whole boundary in one place, at the guard's own seam: notice
        // unclaimed while the set fills, claimed by the first arrival past
        // the cap, refused to the second, and a key recorded BEFORE the cap
        // still deduping after it. Small explicit cap so this is five calls
        // rather than a thousand.
        let guard = CcPinDriftGuard::with_cap(2);

        assert!(guard.observe_version(Some("9.9.1")));
        assert!(guard.observe_version(Some("9.9.2")));
        assert!(
            !guard.seen.cap_noted(),
            "a full set has not yet reached the cap for any key"
        );

        assert!(!guard.observe_version(Some("8.0.0")));
        assert!(
            guard.seen.cap_noted(),
            "the first arrival past the cap claims the one-shot notice"
        );

        assert!(!guard.observe_version(Some("8.0.1")));
        assert!(
            !guard.observe_version(Some("9.9.1")),
            "a version recorded before the cap must still dedup, not re-warn"
        );
        assert_eq!(guard.seen.len(), 2, "the set never grew past its cap");
    }

    #[test]
    fn a_poisoned_lock_still_decides_rather_than_panicking() {
        let guard = std::sync::Arc::new(CcPinDriftGuard::new());
        let poisoner = std::sync::Arc::clone(&guard);
        let _ = std::thread::spawn(move || {
            poisoner.seen.poison_for_test();
        })
        .join();

        assert!(
            guard.observe_version(Some("9.9.9")),
            "a poisoned dedup lock must be recovered, not propagated onto the request path"
        );
        assert!(!guard.observe_version(Some("9.9.9")));
    }

    #[test]
    fn concurrent_observations_of_one_version_warn_exactly_once() {
        let guard = std::sync::Arc::new(CcPinDriftGuard::new());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let guard = std::sync::Arc::clone(&guard);
            handles.push(std::thread::spawn(move || {
                usize::from(guard.observe_version(Some("9.9.9")))
            }));
        }

        let warned: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

        assert_eq!(
            warned, 1,
            "the check-then-act must be atomic under concurrent ingress"
        );
    }

    #[test]
    fn a_user_agent_header_is_observed_through_the_shared_core_parser() {
        let guard = CcPinDriftGuard::new();

        assert!(
            guard.observe_headers(&headers_with_ua("claude-cli/9.9.9 (external, sdk-cli)")),
            "a drifted client User-Agent is one observation"
        );
        assert!(
            !guard.observe_headers(&headers_with_ua(&format!(
                "claude-cli/{PINNED} (external, cli)"
            ))),
            "a client on the compiled pin is not drift"
        );
        assert!(!guard.observe_headers(&HeaderMap::new()));
        assert!(!guard.observe_headers(&headers_with_ua("Mozilla/5.0")));
    }

    #[test]
    fn a_build_suffixed_version_is_not_treated_as_an_observation() {
        let guard = CcPinDriftGuard::new();

        // The per-request build suffix a billing attribution block carries
        // would otherwise re-warn as it changes. The stable User-Agent is
        // the only source, so this parses as no observation at all.
        assert!(!guard.observe_headers(&headers_with_ua("claude-cli/9.9.9.1e8 (external, cli)")));
    }
}
