//! Structural JSON comparator + header bag comparator. Both return
//! [`crate::common::replay::DiffMessage`] on mismatch so test output
//! reads the same shape.

use std::collections::BTreeSet;

use serde_json::Value;

use super::DiffMessage;

/// Compare two JSON values for structural equality. Object key order
/// is irrelevant; array element order is significant. Subtrees whose
/// dot-path matches one of `ignore_paths` are skipped.
///
/// Path syntax: root-level object keys are named without a leading dot
/// (e.g. `"model"`); nested keys are joined with `.` (e.g.
/// `"usage.input_tokens"`); array indices use bracket notation (e.g.
/// `"messages[0].role"`).
pub fn assert_json_equal_structural(
    actual: &Value,
    expected: &Value,
    ignore_paths: &[&str],
) -> Result<(), DiffMessage> {
    json_eq_inner(actual, expected, "", ignore_paths)
}

fn json_eq_inner(
    actual: &Value,
    expected: &Value,
    current_path: &str,
    ignore_paths: &[&str],
) -> Result<(), DiffMessage> {
    if !current_path.is_empty() && ignore_paths.contains(&current_path) {
        return Ok(());
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => json_eq_objects(a, e, current_path, ignore_paths),
        (Value::Array(a), Value::Array(e)) => json_eq_arrays(a, e, current_path, ignore_paths),
        (a, e) if a == e => Ok(()),
        (a, e) => Err(DiffMessage(format!(
            "value mismatch at {}: actual={}, expected={}",
            display_path(current_path),
            a,
            e
        ))),
    }
}

fn json_eq_objects(
    a: &serde_json::Map<String, Value>,
    e: &serde_json::Map<String, Value>,
    current_path: &str,
    ignore_paths: &[&str],
) -> Result<(), DiffMessage> {
    // Filter out keys whose dot-path matches an ignore entry. The
    // semantics of `ignore_paths` are "this subtree is not part of the
    // comparison" -- which has to apply to KEY MEMBERSHIP too, not
    // just to descendant value comparison. Per-provider body flips
    // (e.g. anthropic-api stripping `anthropic_beta`, openai-compat
    // injecting `stream_options`) make a key appear on exactly one
    // side of the diff; the test driver legitimately wants to ignore
    // those without flagging the absence as a key mismatch.
    let key_in_scope = |key: &str| -> bool {
        let next = if current_path.is_empty() {
            key.to_string()
        } else {
            format!("{current_path}.{key}")
        };
        !ignore_paths.contains(&next.as_str())
    };
    let a_keys: BTreeSet<&String> = a.keys().filter(|k| key_in_scope(k)).collect();
    let e_keys: BTreeSet<&String> = e.keys().filter(|k| key_in_scope(k)).collect();
    if a_keys != e_keys {
        let only_actual: Vec<&String> = a_keys.difference(&e_keys).copied().collect();
        let only_expected: Vec<&String> = e_keys.difference(&a_keys).copied().collect();
        return Err(DiffMessage(format!(
            "key mismatch at {}: only_actual={:?}, only_expected={:?}",
            display_path(current_path),
            only_actual,
            only_expected
        )));
    }
    for k in a_keys {
        let next = if current_path.is_empty() {
            k.clone()
        } else {
            format!("{current_path}.{k}")
        };
        json_eq_inner(&a[k], &e[k], &next, ignore_paths)?;
    }
    Ok(())
}

fn json_eq_arrays(
    a: &[Value],
    e: &[Value],
    current_path: &str,
    ignore_paths: &[&str],
) -> Result<(), DiffMessage> {
    if a.len() != e.len() {
        return Err(DiffMessage(format!(
            "array length mismatch at {}: actual={}, expected={}",
            display_path(current_path),
            a.len(),
            e.len()
        )));
    }
    for (i, (av, ev)) in a.iter().zip(e.iter()).enumerate() {
        let next = format!("{current_path}[{i}]");
        json_eq_inner(av, ev, &next, ignore_paths)?;
    }
    Ok(())
}

const fn display_path(p: &str) -> &str {
    if p.is_empty() { "<root>" } else { p }
}

// ---------------------------------------------------------------------
// Header bag comparator
// ---------------------------------------------------------------------

/// Header names allowed to differ silently. Comparison is
/// case-insensitive on the header NAME; the value is what we skip.
pub const DEFAULT_HEADER_ALLOW_SKIP: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-amz-security-token",
    "x-amz-date",
    "x-amz-content-sha256",
    "user-agent",
];

/// Compare two header bag lists. Each pair is matched by
/// case-insensitive name; if both sides agree on a header, values
/// must match. Headers whose lowercased name appears in `allow_skip`
/// are exempt.
pub fn assert_headers_equal(
    actual: &[(String, String)],
    expected: &[(String, String)],
    allow_skip: &[&str],
) -> Result<(), DiffMessage> {
    let allow: BTreeSet<String> = allow_skip.iter().map(|s| s.to_ascii_lowercase()).collect();
    let actual_map = headers_to_map(actual);
    let expected_map = headers_to_map(expected);

    let names: BTreeSet<&String> = actual_map.keys().chain(expected_map.keys()).collect();
    for name in names {
        if allow.contains(name) {
            continue;
        }
        match (actual_map.get(name), expected_map.get(name)) {
            (Some(a), Some(e)) if a == e => {}
            (Some(a), Some(e)) => {
                return Err(DiffMessage(format!(
                    "header value mismatch on {}: actual={}, expected={}",
                    name,
                    redact_for_diff(a),
                    redact_for_diff(e)
                )));
            }
            (Some(_), None) => {
                return Err(DiffMessage(format!(
                    "header {name} present in actual, missing from expected"
                )));
            }
            (None, Some(_)) => {
                return Err(DiffMessage(format!(
                    "header {name} present in expected, missing from actual"
                )));
            }
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}

fn headers_to_map(pairs: &[(String, String)]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (name, value) in pairs {
        out.insert(name.to_ascii_lowercase(), value.clone());
    }
    out
}

/// Redact a header value for diff output: keep the first 6 characters
/// plus a length tag. Any token longer than the prefix stays
/// unreadable in CI logs even when the comparator screams.
fn redact_for_diff(value: &str) -> String {
    let prefix: String = value.chars().take(6).collect();
    format!("{}...(len={})", prefix, value.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- JSON ----------

    #[test]
    fn json_equal_passes_on_identical() {
        let v = json!({"a": 1, "b": [1, 2, 3]});
        assert!(assert_json_equal_structural(&v, &v, &[]).is_ok());
    }

    #[test]
    fn json_equal_passes_on_object_key_reorder() {
        let a = json!({"a": 1, "b": 2});
        let e = json!({"b": 2, "a": 1});
        assert!(assert_json_equal_structural(&a, &e, &[]).is_ok());
    }

    #[test]
    fn json_equal_fails_on_key_mismatch() {
        let a = json!({"a": 1});
        let e = json!({"b": 1});
        let err = assert_json_equal_structural(&a, &e, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("key mismatch"), "got: {msg}");
    }

    #[test]
    fn json_equal_fails_on_value_mismatch() {
        let a = json!({"a": 1});
        let e = json!({"a": 2});
        let err = assert_json_equal_structural(&a, &e, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("value mismatch"), "got: {msg}");
        assert!(msg.contains("at a:"), "got: {msg}");
    }

    #[test]
    fn json_equal_fails_on_array_order_mismatch() {
        let a = json!([1, 2, 3]);
        let e = json!([3, 2, 1]);
        let err = assert_json_equal_structural(&a, &e, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("value mismatch"), "got: {msg}");
    }

    #[test]
    fn json_equal_skips_ignored_path() {
        let a = json!({"id": "abc", "data": 1});
        let e = json!({"id": "xyz", "data": 1});
        assert!(assert_json_equal_structural(&a, &e, &["id"]).is_ok());
    }

    #[test]
    fn json_equal_skips_ignored_key_present_in_only_one_side() {
        // Per-provider body flip: actual carries an extra key (e.g.
        // `anthropic_beta` before the post-normalize strip) that the
        // captured outgoing body has lost. With the path ignored the
        // comparator must not flag the asymmetric membership.
        let a = json!({"model": "x", "anthropic_beta": ["foo"]});
        let e = json!({"model": "x"});
        assert!(assert_json_equal_structural(&a, &e, &["anthropic_beta"]).is_ok());

        let a = json!({"model": "x"});
        let e = json!({"model": "x", "stream": true});
        assert!(assert_json_equal_structural(&a, &e, &["stream"]).is_ok());
    }

    #[test]
    fn json_equal_still_fails_on_unrelated_unique_key() {
        // Sanity: an ignored path must not silence ALL key mismatches.
        let a = json!({"model": "x", "extra": 1});
        let e = json!({"model": "x"});
        let err = assert_json_equal_structural(&a, &e, &["stream"]).unwrap_err();
        assert!(err.to_string().contains("key mismatch"), "got: {err}");
    }

    #[test]
    fn json_equal_recurses_nested_structural() {
        let a = json!({"outer": {"inner": [{"x": 1}, {"x": 2}]}});
        let e = json!({"outer": {"inner": [{"x": 1}, {"x": 99}]}});
        let err = assert_json_equal_structural(&a, &e, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("outer.inner[1].x"), "got: {msg}");
    }

    // ---------- Headers ----------

    #[test]
    fn headers_equal_passes_on_identical() {
        let a = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-extra".to_string(), "1".to_string()),
        ];
        let e = a.clone();
        assert!(assert_headers_equal(&a, &e, DEFAULT_HEADER_ALLOW_SKIP).is_ok());
    }

    #[test]
    fn headers_equal_skips_allowlisted_value_drift() {
        let a = vec![
            ("authorization".to_string(), "Bearer abc-123".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let e = vec![
            ("authorization".to_string(), "<REDACTED>".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        assert!(assert_headers_equal(&a, &e, DEFAULT_HEADER_ALLOW_SKIP).is_ok());
    }

    #[test]
    fn headers_equal_uses_case_insensitive_name_match() {
        let a = vec![("Content-Type".to_string(), "application/json".to_string())];
        let e = vec![("content-type".to_string(), "application/json".to_string())];
        assert!(assert_headers_equal(&a, &e, DEFAULT_HEADER_ALLOW_SKIP).is_ok());
    }

    #[test]
    fn headers_equal_fails_on_non_allowlisted_value_mismatch() {
        let a = vec![("x-custom".to_string(), "secret-value-xyz".to_string())];
        let e = vec![("x-custom".to_string(), "different-secret-abc".to_string())];
        let err = assert_headers_equal(&a, &e, DEFAULT_HEADER_ALLOW_SKIP).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("header value mismatch on x-custom"),
            "got: {msg}"
        );
        // Redacted values must not leak the full mismatched value.
        assert!(!msg.contains("secret-value-xyz"), "leaked actual: {msg}");
        assert!(
            !msg.contains("different-secret-abc"),
            "leaked expected: {msg}"
        );
        assert!(msg.contains("(len="), "diff missing length tag: {msg}");
    }

    #[test]
    fn headers_equal_fails_when_present_in_one_side_only() {
        let a = vec![("x-custom".to_string(), "1".to_string())];
        let e: Vec<(String, String)> = Vec::new();
        let err = assert_headers_equal(&a, &e, DEFAULT_HEADER_ALLOW_SKIP).unwrap_err();
        assert!(err.to_string().contains("missing from expected"));
    }
}
