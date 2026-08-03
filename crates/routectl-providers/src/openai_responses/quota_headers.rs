//! Parser for the Codex `x-codex-*` response-header quota family
//! (ChatGPT subscription quota observability). Tolerant by design: a
//! missing family yields `None`, a non-UTF8 header value is skipped
//! silently, and an unrecognized suffix lands in `extras` for
//! forward-compat. A weird header value must NEVER fail a request --
//! extraction runs on an already-successful response, so there is no
//! `Result` here.
//!
//! Header reference: the family is emitted on the chatgpt-oauth
//! subscription surface. Only three suffixes are typed, because only
//! those three feed a shared `quota_*` ledger column:
//! `active-limit`, `primary-used-percent` (integer percent 0-100),
//! `primary-reset-at` (epoch SECONDS). Everything else -- the whole
//! `secondary-*` family, `plan-type`, `primary-window-minutes`,
//! `primary-reset-after-seconds`,
//! `primary-over-secondary-limit-percent`, `credits-*`, `bengalfox-*`
//! and `safety-buffering-*` -- lands in `extras` observable but
//! unmodeled. The api-key and mantle lanes do not emit the family, so
//! `parse_codex_quota` returns `None` there without an auth-kind gate.

use reqwest::header::HeaderMap;

use routectl_core::CodexQuota;

/// Common prefix shared by every header in the Codex quota family.
const CODEX_PREFIX: &str = "x-codex-";

/// Parse the `x-codex-*` family out of an upstream response's headers.
/// Returns `None` when NO header of the family is present (the api-key
/// path, or any non-subscription response). Non-UTF8 header values are
/// skipped silently -- the family carries only ASCII quota strings, so a
/// non-UTF8 value is upstream misbehavior, not data routectl should
/// surface or fail on.
pub fn parse_codex_quota(headers: &HeaderMap) -> Option<CodexQuota> {
    let mut quota = CodexQuota::default();
    let mut saw_any = false;

    for (name, value) in headers {
        // `HeaderName::as_str()` is documented to always return lowercase
        // (http 1.x), so a borrow suffices -- no per-header allocation.
        let Some(suffix) = name.as_str().strip_prefix(CODEX_PREFIX) else {
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

/// Route one Codex-family header (identified by its suffix after
/// `x-codex-`) to its typed field. Only the three shared-column feeders
/// are typed; every other suffix falls to `extras` so a future header is
/// observable without a code change.
fn assign_suffix(quota: &mut CodexQuota, suffix: &str, value: String) {
    match suffix {
        "active-limit" => quota.active_limit = Some(value),
        "primary-used-percent" => quota.primary_used_percent = Some(value),
        "primary-reset-at" => quota.primary_reset_at = Some(value),
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

    /// The full captured success-path family from a live chatgpt-oauth
    /// response, verbatim field names and example values.
    fn captured_family() -> HeaderMap {
        headers(&[
            ("x-codex-active-limit", "premium"),
            ("x-codex-plan-type", "prolite"),
            ("x-codex-primary-used-percent", "16"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-after-seconds", "468887"),
            ("x-codex-primary-reset-at", "1786210114"),
            ("x-codex-primary-over-secondary-limit-percent", "0"),
            ("x-codex-secondary-used-percent", "0"),
            ("x-codex-secondary-window-minutes", "0"),
            ("x-codex-secondary-reset-after-seconds", "0"),
            ("x-codex-secondary-reset-at", ""),
            ("x-codex-credits-has-credits", "False"),
            ("x-codex-credits-balance", "0"),
            ("x-codex-credits-unlimited", "False"),
            ("x-codex-bengalfox-primary-used-percent", "0"),
            ("x-codex-bengalfox-primary-window-minutes", "10080"),
            ("x-codex-bengalfox-primary-reset-after-seconds", "604800"),
            ("x-codex-bengalfox-primary-reset-at", "1786346027"),
            ("x-codex-bengalfox-limit-name", "GPT-5.3-Codex-Spark"),
            ("x-codex-safety-buffering-enabled", "False"),
        ])
    }

    #[test]
    fn types_only_the_three_shared_column_feeders() {
        // Arrange
        let map = captured_family();

        // Act
        let quota = parse_codex_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.active_limit.as_deref(), Some("premium"));
        assert_eq!(quota.primary_used_percent.as_deref(), Some("16"));
        assert_eq!(quota.primary_reset_at.as_deref(), Some("1786210114"));
    }

    #[test]
    fn captures_every_other_captured_suffix_in_extras_verbatim() {
        // Arrange
        let map = captured_family();

        // Act
        let quota = parse_codex_quota(&map).expect("family present");

        // Assert: header iteration order is unspecified, so compare as a
        // set of (suffix, value) pairs.
        let mut got = quota.extras.clone();
        got.sort();
        let mut want: Vec<(String, String)> = [
            ("plan-type", "prolite"),
            ("primary-window-minutes", "10080"),
            ("primary-reset-after-seconds", "468887"),
            ("primary-over-secondary-limit-percent", "0"),
            ("secondary-used-percent", "0"),
            ("secondary-window-minutes", "0"),
            ("secondary-reset-after-seconds", "0"),
            // Empty value preserved: the upstream sends "" when the
            // secondary window is unused.
            ("secondary-reset-at", ""),
            ("credits-has-credits", "False"),
            ("credits-balance", "0"),
            ("credits-unlimited", "False"),
            ("bengalfox-primary-used-percent", "0"),
            ("bengalfox-primary-window-minutes", "10080"),
            ("bengalfox-primary-reset-after-seconds", "604800"),
            ("bengalfox-primary-reset-at", "1786346027"),
            ("bengalfox-limit-name", "GPT-5.3-Codex-Spark"),
            ("safety-buffering-enabled", "False"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn returns_none_when_family_absent() {
        // Arrange: unrelated headers only.
        let map = headers(&[
            ("content-type", "text/event-stream"),
            ("x-request-id", "req_123"),
            ("openai-processing-ms", "42"),
        ]);

        // Act + Assert
        assert!(parse_codex_quota(&map).is_none());
    }

    #[test]
    fn returns_none_for_empty_headers() {
        // Arrange + Act + Assert
        assert!(parse_codex_quota(&HeaderMap::new()).is_none());
    }

    #[test]
    fn skips_non_utf8_value_but_keeps_other_family_members() {
        // Arrange: a valid family header plus one whose value is not
        // valid UTF-8. The non-UTF8 one must be skipped silently while
        // the valid one is still captured.
        let mut map = headers(&[("x-codex-active-limit", "premium")]);
        map.insert(
            HeaderName::from_static("x-codex-primary-used-percent"),
            HeaderValue::from_bytes(&[0xff, 0xfe]).expect("non-utf8 header value"),
        );

        // Act
        let quota = parse_codex_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.active_limit.as_deref(), Some("premium"));
        assert!(
            quota.primary_used_percent.is_none(),
            "non-UTF8 value must be skipped, not surfaced"
        );
        assert!(quota.extras.is_empty());
    }

    #[test]
    fn parses_partial_family_leaving_absent_fields_none() {
        // Arrange: only the reset-at is present.
        let map = headers(&[("x-codex-primary-reset-at", "1786210114")]);

        // Act
        let quota = parse_codex_quota(&map).expect("family present");

        // Assert
        assert_eq!(quota.primary_reset_at.as_deref(), Some("1786210114"));
        assert!(quota.active_limit.is_none());
        assert!(quota.primary_used_percent.is_none());
        assert!(quota.extras.is_empty());
    }
}
