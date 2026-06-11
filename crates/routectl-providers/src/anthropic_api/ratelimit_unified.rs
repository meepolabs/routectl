//! Parser for the Anthropic `anthropic-ratelimit-unified-*` response-header
//! family (subscription quota / overage observability). Tolerant by
//! design: a missing family yields `None`, a non-UTF8 header value is
//! skipped silently, and an unrecognized suffix lands in `extras` for
//! forward-compat. A weird header value must NEVER fail a request.
//!
//! Header reference: the unified family is emitted on the OAuth
//! subscription path. BARE suffixes: `-status`, `-reset`,
//! `-representative-claim`, `-overage-status`, `-overage-utilization`,
//! `-overage-reset`, `-fallback-percentage`. WINDOWED suffixes:
//! `-5h-status`, `-5h-utilization`, `-5h-reset`, `-7d-status`,
//! `-7d-utilization`, `-7d-reset`. There is NO bare `-utilization`
//! header. `quota.utilization` is sourced from the 5h window (the
//! operational subscription signal); the 7d window and the per-window
//! status/reset suffixes land in `extras`. The api-key path does not
//! emit the family, so `parse_unified_quota` returns `None` there.

use reqwest::header::HeaderMap;

use routectl_core::AnthropicUnifiedQuota;

/// Common prefix shared by every header in the unified family.
const UNIFIED_PREFIX: &str = "anthropic-ratelimit-unified-";

/// Parse the `anthropic-ratelimit-unified-*` family out of an upstream
/// response's headers. Returns `None` when NO header of the family is
/// present (the api-key path, or any non-subscription response).
/// Non-UTF8 header values are skipped silently -- the family carries
/// only ASCII quota strings, so a non-UTF8 value is upstream
/// misbehavior, not data routectl should surface or fail on.
pub(crate) fn parse_unified_quota(headers: &HeaderMap) -> Option<AnthropicUnifiedQuota> {
    let mut quota = AnthropicUnifiedQuota::default();
    let mut saw_any = false;

    for (name, value) in headers.iter() {
        // `HeaderName::as_str()` is documented to always return lowercase
        // (http 1.x), so a borrow suffices -- no per-header allocation.
        let Some(suffix) = name.as_str().strip_prefix(UNIFIED_PREFIX) else {
            continue;
        };
        // Skip non-UTF8 values silently: never fail the request.
        let Ok(val) = value.to_str() else {
            continue;
        };
        saw_any = true;
        assign_suffix(&mut quota, suffix, val.to_string());
    }

    saw_any.then_some(quota)
}

/// The billing-attribution transition observed between the
/// previous-seen `representative-claim` and the current one. Drives the
/// once-per-flip log: steady state is `None` (silent), a flip into
/// overage warns, a flip back out informs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverageTransition {
    /// Billing attribution flipped INTO overage on this response.
    EnteredOverage,
    /// Billing attribution flipped back OUT of overage on this response.
    RecoveredFromOverage,
}

/// Compute the billing-attribution transition between the previous-seen
/// claim and the current one. Pure: the caller owns the persisted
/// `prior` state. Returns `None` for steady state (both overage or
/// both non-overage), so the caller stays silent and emits no
/// per-request log flood.
///
/// `prior == None` is the first observation for this provider instance;
/// only an entry INTO overage is worth a log there (a first observation
/// that is already non-overage is the silent normal case).
pub(crate) fn classify_overage_transition(
    prior: Option<&str>,
    current: Option<&str>,
) -> Option<OverageTransition> {
    let was_overage = prior == Some(OVERAGE_CLAIM);
    let is_overage = current == Some(OVERAGE_CLAIM);
    match (was_overage, is_overage) {
        (false, true) => Some(OverageTransition::EnteredOverage),
        (true, false) => Some(OverageTransition::RecoveredFromOverage),
        _ => None,
    }
}

/// `representative-claim` value that signals overage billing. Re-exported
/// from core's canonical constant so the state machine and the parser
/// agree on the literal.
use routectl_core::upstream_meta::OVERAGE_CLAIM;

/// Route one unified-family header (identified by its suffix after
/// `anthropic-ratelimit-unified-`) to its typed field. Unknown suffixes
/// fall to `extras` so a future header is observable without a code
/// change.
fn assign_suffix(quota: &mut AnthropicUnifiedQuota, suffix: &str, value: String) {
    match suffix {
        "status" => quota.status = Some(value),
        "overage-status" => quota.overage_status = Some(value),
        // The 5h window is the operational subscription signal; route it
        // to `utilization`. There is no bare `-utilization` header on the
        // OAuth path. The 7d window and per-window status/reset stay in
        // `extras` for forward-compat.
        "5h-utilization" => quota.utilization = Some(value),
        "overage-utilization" => quota.overage_utilization = Some(value),
        "representative-claim" => quota.representative_claim = Some(value),
        "reset" => quota.reset = Some(value),
        other => quota.extras.push((other.to_string(), value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(v).expect("valid header value"),
            );
        }
        map
    }

    #[test]
    fn parses_full_unified_family_into_typed_fields() {
        // Arrange
        let map = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-overage-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.10"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-reset", "2026-06-09T12:00:00Z"),
        ]);

        // Act
        let quota = parse_unified_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.status.as_deref(), Some("allowed"));
        assert_eq!(quota.overage_status.as_deref(), Some("allowed"));
        assert_eq!(quota.utilization.as_deref(), Some("0.42"));
        assert_eq!(quota.overage_utilization.as_deref(), Some("0.10"));
        assert_eq!(quota.representative_claim.as_deref(), Some("five_hour"));
        assert_eq!(quota.reset.as_deref(), Some("2026-06-09T12:00:00Z"));
        assert!(quota.extras.is_empty());
    }

    #[test]
    fn sources_utilization_from_5h_window_and_extras_the_7d_window() {
        // Arrange: the real OAuth shape -- a bare status, a 5h
        // utilization, and a 7d utilization. The 5h window is the
        // operational subscription signal routed to `utilization`; the
        // 7d window stays in `extras`.
        let map = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.21"),
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.30"),
        ]);

        // Act
        let quota = parse_unified_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.utilization.as_deref(), Some("0.21"));
        assert_eq!(quota.status.as_deref(), Some("allowed"));
        assert_eq!(
            quota.extras,
            vec![("7d-utilization".to_string(), "0.30".to_string())],
            "7d window must land in extras, not utilization"
        );
    }

    #[test]
    fn bare_suffixes_map_to_their_typed_fields() {
        // Arrange: a regression guard that the BARE suffixes still route
        // to their typed fields after the 5h-utilization remap.
        let map = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-reset", "2026-06-09T12:00:00Z"),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "five_hour",
            ),
            ("anthropic-ratelimit-unified-overage-utilization", "0.0"),
        ]);

        // Act
        let quota = parse_unified_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.status.as_deref(), Some("allowed"));
        assert_eq!(quota.reset.as_deref(), Some("2026-06-09T12:00:00Z"));
        assert_eq!(quota.representative_claim.as_deref(), Some("five_hour"));
        assert_eq!(quota.overage_utilization.as_deref(), Some("0.0"));
        assert!(quota.extras.is_empty());
    }

    #[test]
    fn returns_none_when_family_absent() {
        // Arrange: unrelated headers only.
        let map = headers(&[
            ("content-type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("x-request-id", "req_123"),
        ]);

        // Act + Assert
        assert!(parse_unified_quota(&map).is_none());
    }

    #[test]
    fn returns_none_for_empty_headers() {
        // Arrange + Act + Assert
        assert!(parse_unified_quota(&HeaderMap::new()).is_none());
    }

    #[test]
    fn parses_partial_family_leaving_absent_fields_none() {
        // Arrange: only the representative-claim + reset are present.
        let map = headers(&[
            (
                "anthropic-ratelimit-unified-representative-claim",
                "overage",
            ),
            ("anthropic-ratelimit-unified-reset", "2026-06-09T13:00:00Z"),
        ]);

        // Act
        let quota = parse_unified_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.representative_claim.as_deref(), Some("overage"));
        assert_eq!(quota.reset.as_deref(), Some("2026-06-09T13:00:00Z"));
        assert!(quota.status.is_none());
        assert!(quota.utilization.is_none());
        assert!(quota.is_overage());
    }

    #[test]
    fn skips_non_utf8_value_but_keeps_other_family_members() {
        // Arrange: a valid family header plus one whose value is not
        // valid UTF-8. The non-UTF8 one must be skipped silently while
        // the valid one is still captured.
        let mut map = headers(&[("anthropic-ratelimit-unified-status", "allowed")]);
        map.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-5h-utilization"),
            HeaderValue::from_bytes(&[0xff, 0xfe]).expect("non-utf8 header value"),
        );

        // Act
        let quota = parse_unified_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.status.as_deref(), Some("allowed"));
        assert!(
            quota.utilization.is_none(),
            "non-UTF8 value must be skipped, not surfaced"
        );
    }

    // -- overage flip state machine ---------------------------------------

    #[test]
    fn none_to_overage_signals_entered_overage() {
        // Arrange + Act + Assert: first observation that is already in
        // overage is worth a single warn.
        assert_eq!(
            classify_overage_transition(None, Some("overage")),
            Some(OverageTransition::EnteredOverage)
        );
    }

    #[test]
    fn non_overage_to_overage_signals_entered_overage() {
        assert_eq!(
            classify_overage_transition(Some("five_hour"), Some("overage")),
            Some(OverageTransition::EnteredOverage)
        );
    }

    #[test]
    fn overage_to_overage_is_steady_state_silent() {
        // Arrange + Act + Assert: no per-request flood while in overage.
        assert_eq!(
            classify_overage_transition(Some("overage"), Some("overage")),
            None
        );
    }

    #[test]
    fn overage_to_non_overage_signals_recovery() {
        assert_eq!(
            classify_overage_transition(Some("overage"), Some("five_hour")),
            Some(OverageTransition::RecoveredFromOverage)
        );
    }

    #[test]
    fn overage_prior_with_absent_current_claim_is_treated_as_recovery() {
        // Pins current behavior: an absent current claim is treated as
        // not-overage, so a prior-overage observation flips to recovery.
        assert_eq!(
            classify_overage_transition(Some("overage"), None),
            Some(OverageTransition::RecoveredFromOverage)
        );
    }

    #[test]
    fn non_overage_to_non_overage_is_steady_state_silent() {
        assert_eq!(
            classify_overage_transition(Some("five_hour"), Some("five_hour")),
            None
        );
    }

    #[test]
    fn first_observation_non_overage_is_silent() {
        assert_eq!(classify_overage_transition(None, Some("five_hour")), None);
    }

    #[test]
    fn captures_unknown_unified_suffix_in_extras() {
        // Arrange: a future/unknown suffix routectl doesn't model yet.
        let map = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            (
                "anthropic-ratelimit-unified-overage-disabled-reason",
                "spend_cap",
            ),
        ]);

        // Act
        let quota = parse_unified_quota(&map).expect("family present");

        // Assert: the typed field is set; the unknown suffix lands in
        // extras as (suffix, value) for forward-compat.
        assert_eq!(quota.status.as_deref(), Some("allowed"));
        assert_eq!(
            quota.extras,
            vec![(
                "overage-disabled-reason".to_string(),
                "spend_cap".to_string()
            )]
        );
    }
}
