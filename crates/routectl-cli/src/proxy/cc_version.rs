//! CC-version warn-and-proceed check for the MITM front-proxy.
//!
//! This check records the Claude Code CLI version this
//! `[mitm]` config was tested against, then WARNS -- never
//! hard-refuses -- when the version actually observed on the wire
//! differs. A hard refuse would break routectl on every Claude Code
//! release; skipping the check entirely would lose the only signal that
//! CC's wire shape moved out from under
//! `routectl_core::identity::anthropic`'s pinned defaults, which is
//! exactly the arms-race breakage this check exists to surface.
//!
//! Claude Code's own `User-Agent` carries its version:
//! `claude-cli/<version> (external, cli)` (see
//! `routectl_core::identity::anthropic::default_claude_code_user_agent`).
//! [`observed_cc_version`] lifts that header off a decrypted request and
//! reads the post-prefix token through the shared LOOSE core helper
//! (`claude_cli_ua_token`), which owns the prefix literal but decides
//! nothing about the token's shape. Loose is the right reading HERE and
//! not a shortcut: `tested_cc_version` is a string an operator typed and
//! it is compared verbatim, so a prerelease, build-suffixed, or opaque
//! token must still produce the warning they asked for. The sibling
//! `server::cc_pin_drift` guard keys durable state on what it observes and
//! therefore uses the STRICT parser instead.
//!
//! [`CcVersionWarnGuard`] dedups the resulting warning so a steady
//! mismatch warns exactly once and a version change re-warns (the newly
//! observed version is itself new information).
//!
//! MODULE VISIBILITY: `pub` FOR A CROSS-BINARY TEST, not for callers. The
//! version-warning log-contract test lives in its own integration binary (a
//! thread-local capture subscriber over a shared `warn!` callsite is
//! unreliable inside the lib test binary, where sibling tests poison
//! tracing's per-callsite `Interest` cache first), and an integration binary
//! cannot see a `pub(crate)` module. The only production caller is
//! `proxy::split`.

use std::sync::Mutex;

use http::HeaderMap;

/// Extracts the version token from a `claude-cli/<token> ...`
/// `User-Agent` header value on a decrypted MITM request. Returns
/// `None` for a missing header, a non-UTF8 value, or a `User-Agent` that
/// carries no post-prefix token -- callers treat all of those identically
/// (no warning, no panic). Any non-empty token is reported, INCLUDING an
/// unstable or opaque one; see the module doc for why.
pub fn observed_cc_version(headers: &HeaderMap) -> Option<String> {
    let ua = headers.get(http::header::USER_AGENT)?.to_str().ok()?;
    routectl_core::identity::anthropic::claude_cli_ua_token(ua).map(str::to_string)
}

/// Dedups the CC-version-mismatch warning: fires at most once for the
/// currently-mismatched version, not once per request. Deliberately
/// tracks only the single most-recently-warned version (an `O(1)`,
/// never-capped `Mutex<Option<String>>`) rather than a full historical
/// set of every version ever seen -- a version that reappears after the
/// mismatch changed away and back (e.g. `2.0.0 -> 2.0.1 -> 2.0.0`) warns
/// again. That is intentional: this check exists to surface an
/// arms-race signal (the wire shape moved out from under routectl's
/// pinned defaults), and a client presently sending a mismatched
/// version is itself the actionable fact, independent of whether that
/// exact string was warned about at some earlier point in the process's
/// lifetime. Never refuses the request either way -- this guard only
/// decides whether to log, and [`Self::check`]'s return value is the
/// testable seam callers assert against instead of scraping logs.
#[derive(Debug, Default)]
pub struct CcVersionWarnGuard {
    last_warned: Mutex<Option<String>>,
}

impl CcVersionWarnGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compares `observed` against `tested` and emits one
    /// `tracing::warn!` the first time a given `observed` value
    /// mismatches `tested`; returns `true` iff this call was the one
    /// that emitted. Never warns when `tested` is `None` (no tested
    /// version configured), when `observed` is `None` (no/unparseable
    /// `User-Agent`), or when the two already match.
    pub fn check(&self, tested: Option<&str>, observed: Option<&str>) -> bool {
        let (Some(tested), Some(observed)) = (tested, observed) else {
            return false;
        };
        if tested == observed {
            return false;
        }

        let mut last_warned = match self.last_warned.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if last_warned.as_deref() == Some(observed) {
            return false;
        }
        *last_warned = Some(observed.to_string());
        drop(last_warned);

        tracing::warn!(
            target: "routectl_cli::proxy::cc_version",
            tested_cc_version = tested,
            observed_cc_version = observed,
            "observed Claude Code version does not match the tested version -- \
             proceeding anyway (never hard-refused)"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_ua(ua: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::USER_AGENT, ua.parse().unwrap());
        headers
    }

    #[test]
    fn observed_cc_version_extracts_the_version_token() {
        let headers = headers_with_ua("claude-cli/2.1.169 (external, cli)");
        assert_eq!(observed_cc_version(&headers).as_deref(), Some("2.1.169"));
    }

    #[test]
    fn observed_cc_version_is_none_when_user_agent_header_is_absent() {
        let headers = HeaderMap::new();
        assert_eq!(observed_cc_version(&headers), None);
    }

    #[test]
    fn observed_cc_version_is_none_for_a_non_claude_cli_user_agent() {
        let headers = headers_with_ua("Mozilla/5.0 (X11; Linux x86_64)");
        assert_eq!(observed_cc_version(&headers), None);
    }

    #[test]
    fn observed_cc_version_is_none_for_a_prefix_with_no_version_token() {
        let headers = headers_with_ua("claude-cli/");
        assert_eq!(observed_cc_version(&headers), None);
    }

    #[test]
    fn observed_cc_version_is_none_for_a_non_utf8_header_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_bytes(&[0xC0, 0xAF]).unwrap(),
        );
        assert_eq!(observed_cc_version(&headers), None);
    }

    #[test]
    fn observed_cc_version_agrees_with_the_shared_loose_core_helper() {
        // One helper decides what the post-prefix token IS. This header
        // extractor's only job is getting the value out of the HeaderMap, so
        // its answer must be the shared helper's answer for the same string.
        for ua in [
            "claude-cli/2.1.246 (external, sdk-cli)",
            "claude-cli/2.1.169 (external, cli)",
            "claude-cli/2.1.246.1e8 (external, cli)",
            "claude-cli/",
            "Mozilla/5.0",
        ] {
            let headers = headers_with_ua(ua);
            assert_eq!(
                observed_cc_version(&headers).as_deref(),
                routectl_core::identity::anthropic::claude_cli_ua_token(ua),
                "header extraction must not decide token validity itself; ua={ua}"
            );
        }
    }

    #[test]
    fn observed_cc_version_still_reports_an_unstable_or_opaque_token() {
        // The long-standing contract, pinned so it cannot be narrowed by a
        // change elsewhere. `tested_cc_version` is a string an operator
        // typed, compared verbatim: if this extractor started refusing
        // prerelease, build-suffixed, or opaque tokens, an operator who
        // recorded one would silently stop being warned about the drift they
        // asked to hear about.
        assert_eq!(
            observed_cc_version(&headers_with_ua("claude-cli/2.1.246.1e8 (external, cli)"))
                .as_deref(),
            Some("2.1.246.1e8")
        );
        assert_eq!(
            observed_cc_version(&headers_with_ua("claude-cli/2.2.0-rc.1")).as_deref(),
            Some("2.2.0-rc.1")
        );
        assert_eq!(
            observed_cc_version(&headers_with_ua("claude-cli/nightly")).as_deref(),
            Some("nightly")
        );
    }

    #[test]
    fn the_guard_warns_on_an_unstable_observed_token_exactly_as_before() {
        let guard = CcVersionWarnGuard::new();

        assert!(guard.check(Some("2.1.246"), Some("2.1.246.1e8")));
        assert!(!guard.check(Some("2.1.246"), Some("2.1.246.1e8")));
        assert!(
            guard.check(Some("2.1.246"), Some("2.1.247-rc.1")),
            "an unstable token is still a mismatch worth one warning"
        );
    }

    #[test]
    fn check_does_not_warn_on_a_matching_version() {
        let guard = CcVersionWarnGuard::new();
        assert!(!guard.check(Some("2.1.169"), Some("2.1.169")));
    }

    #[test]
    fn check_warns_once_on_a_mismatch() {
        let guard = CcVersionWarnGuard::new();
        assert!(guard.check(Some("2.1.169"), Some("2.0.0")));
    }

    #[test]
    fn check_does_not_rewarn_the_same_mismatch() {
        let guard = CcVersionWarnGuard::new();
        assert!(guard.check(Some("2.1.169"), Some("2.0.0")));
        assert!(!guard.check(Some("2.1.169"), Some("2.0.0")));
    }

    #[test]
    fn check_rewarns_on_a_version_change() {
        let guard = CcVersionWarnGuard::new();
        assert!(guard.check(Some("2.1.169"), Some("2.0.0")));
        assert!(!guard.check(Some("2.1.169"), Some("2.0.0")));
        assert!(
            guard.check(Some("2.1.169"), Some("2.0.1")),
            "a newly observed version must re-warn even though a mismatch already fired"
        );
    }

    #[test]
    fn check_rewarns_on_a_version_that_reappears_after_a_flip_flop() {
        let guard = CcVersionWarnGuard::new();
        assert!(guard.check(Some("2.1.169"), Some("2.0.0")));
        assert!(guard.check(Some("2.1.169"), Some("2.0.1")));
        assert!(
            guard.check(Some("2.1.169"), Some("2.0.0")),
            "a version that reappears after the mismatch changed away and back must \
             re-warn -- the guard only remembers the single most-recently-warned \
             version, not a full historical set (see the struct doc)"
        );
    }

    #[test]
    fn check_never_warns_when_no_tested_version_is_configured() {
        let guard = CcVersionWarnGuard::new();
        assert!(!guard.check(None, Some("2.0.0")));
    }

    #[test]
    fn check_never_warns_when_no_version_was_observed() {
        let guard = CcVersionWarnGuard::new();
        assert!(!guard.check(Some("2.1.169"), None));
    }
}
