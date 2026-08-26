//! Structural JSON comparator + header bag comparator. Both return
//! [`crate::common::replay::DiffMessage`] on mismatch so test output
//! reads the same shape.

use std::collections::BTreeSet;

use serde_json::Value;

use super::DiffMessage;

/// One structural difference between an actual and an expected JSON
/// value, located by the dot/bracket `path` convention documented on
/// [`diff_all`].
///
/// `actual` / `expected` carry the value from the side(s) that has it:
/// [`DivergenceKind::Added`] fills only `actual`,
/// [`DivergenceKind::Removed`] fills only `expected`, and
/// [`DivergenceKind::Changed`] fills both.
///
/// [`Display`](std::fmt::Display) renders the one-line form the
/// comparator's own failure messages use, so a consumer that reports
/// unexplained divergences never hand-rolls a second match over
/// [`DivergenceKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub path: String,
    pub kind: DivergenceKind,
    pub actual: Option<Value>,
    pub expected: Option<Value>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_divergence(self).0)
    }
}

/// Which side of the comparison a [`Divergence`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceKind {
    /// Present on the actual side only.
    Added,
    /// Present on the expected side only.
    Removed,
    /// Present on both sides with different values.
    Changed,
}

/// Walk two JSON values and return EVERY structural divergence between
/// them, not just the first. Object key order is irrelevant; array
/// element order is significant. Subtrees whose path matches one of
/// `ignore_paths` are excluded from the comparison, including from key
/// membership (see the comment inside `walk_objects`).
///
/// Path syntax: root-level object keys are named without a leading dot
/// (e.g. `"model"`); nested keys are joined with `.` (e.g.
/// `"usage.input_tokens"`); array indices use bracket notation (e.g.
/// `"messages[0].role"`). The empty path denotes the root.
///
/// # Array length
///
/// There is no length-specific kind and no [`DivergenceKind::Changed`]
/// on the array path itself. A length difference is reported as one
/// membership divergence PER SURPLUS ELEMENT, at that element's indexed
/// path: a longer actual side yields [`DivergenceKind::Added`] at
/// `arr[i]`, a longer expected side yields
/// [`DivergenceKind::Removed`]. Elements present on both sides are
/// still compared, so a length change never suppresses the
/// element-level divergences underneath it. Consumers therefore see
/// which elements went missing rather than only that a count moved.
///
/// # Result ordering
///
/// Divergences arrive in TRAVERSAL order: depth-first, and lexicographic
/// only among the keys of one object (array indices ascend). This is not
/// a global sort of the path strings. Sibling keys `a` (an object holding
/// `z`) and `a-b` emit `["a.z", "a-b"]`, because `a` sorts before `a-b`
/// at their shared level and `a`'s subtree is walked before moving on; a
/// global sort would give `["a-b", "a.z"]` since `-` (0x2D) precedes `.`
/// (0x2E). Ordering is deterministic, so a consumer may assert an exact
/// path vector -- but it must order its expectation by traversal, not by
/// `sort()`.
///
/// # Why a set and not the first mismatch
///
/// Reporting one mismatch makes a per-path whitelist unsound. The only
/// thing `ignore_paths` can express is "exclude this whole subtree", so
/// a conservation whitelist layered on a first-mismatch comparator would
/// have to exclude `.messages` wholesale -- and `.messages` dominates a
/// captured request body by bytes. Every real wire loss sitting behind
/// the first whitelisted divergence would be structurally invisible. A
/// complete divergence set lets a consumer account for each path
/// individually and fail on the ones it cannot explain.
///
/// # Array pairing is POSITIONAL -- normalize element removals away, do
/// not whitelist them
///
/// Elements are paired by index, so removing one element from the MIDDLE
/// of an array shifts every element after it and reports as a divergence
/// at each subsequent index. Whitelisting such a transform per index is
/// not viable: the dominant real transform (the system-turn lift) removes
/// turns from the middle of `.messages`, leaving almost no message pair
/// aligned, so one explained transform would emit a divergence for nearly
/// every element and the whitelist would have to cover essentially the
/// whole array -- muting the very subtree this comparator exists to keep
/// visible.
///
/// A caller with a known element-removing transform therefore applies it
/// to its own inputs BEFORE calling `diff_all`, and diffs the normalized
/// pair. There is deliberately no normalization hook here: normalization
/// is the caller's domain knowledge, and a pre-diff `Value` transform
/// needs no API from the comparator. Applying the system-turn removal to
/// the ingress side first realigns the arrays element-for-element in 238
/// of 250 corpus fixtures -- every fixture the lift touched -- with zero
/// residual divergences; the
/// `diff_all_returns_empty_after_the_caller_normalizes_away_a_middle_removal`
/// test pins that pattern.
///
/// ## Other count-changing transforms a normalizer must weigh
///
/// Reachability differs per lane, so a normalizer is only correct for the
/// lane it was measured on. On the anthropic-ingress lane, against the
/// current corpus:
///
/// - **`SystemTurnPolicy::Forward` emits system turns IN PLACE**
///   (`anthropic_api/messages.rs:1336`). The policy is resolved
///   per-request at `anthropic_api/request.rs:577` from
///   `forward_system_turns && filtered_system.is_some()`, so a fixture
///   captured under Forward keeps its system turns on the wire and
///   removing them from the ingress side would INTRODUCE a misalignment
///   that is not there. No current fixture was captured under Forward, so
///   this is a caveat for future captures rather than a present bug --
///   re-derive it before trusting a system-turn normalizer, do not assume
///   it still holds.
/// - **Unsigned-thinking stripping can drop whole turns**
///   (`anthropic_api/messages.rs:248-250`). The corpus does carry
///   thinking blocks, but none was observed dropping a turn, so a
///   normalizer for it would be unexercised on the present corpus.
/// - **Tool-run folding CANNOT fire on this lane**
///   (`collect_tool_run`, `anthropic_api/messages.rs:1366-1370`) even
///   though it folds N consecutive `Role::Tool` turns into one. `Role::Tool`
///   is the OpenAI-shaped canonical form; on anthropic ingress tool
///   results ride as `tool_result` content blocks inside user turns, and
///   `role=tool` turns do not occur anywhere in the corpus. A normalizer
///   for it would be dead config that no fixture exercises.
///
/// Each bullet is an OBSERVATION over a per-contributor corpus, not a
/// settled invariant: re-derive it against the fixtures at hand before
/// relying on it.
pub fn diff_all(actual: &Value, expected: &Value, ignore_paths: &[&str]) -> Vec<Divergence> {
    let mut out = Vec::new();
    walk(actual, expected, "", ignore_paths, &mut out);
    out
}

/// Compare two JSON values for structural equality, reporting the FIRST
/// divergence [`diff_all`] finds. Semantics and path syntax are
/// [`diff_all`]'s; this is only a different presentation of the same
/// walk.
pub fn assert_json_equal_structural(
    actual: &Value,
    expected: &Value,
    ignore_paths: &[&str],
) -> Result<(), DiffMessage> {
    match diff_all(actual, expected, ignore_paths).first() {
        None => Ok(()),
        Some(divergence) => Err(format_divergence(divergence)),
    }
}

fn format_divergence(divergence: &Divergence) -> DiffMessage {
    let path = display_path(&divergence.path);
    // An indexed path names an array element, not an object key, so a
    // surplus element must not read as a key mismatch.
    let noun = if divergence.path.ends_with(']') {
        "element mismatch"
    } else {
        "key mismatch"
    };
    let text = match divergence.kind {
        DivergenceKind::Changed => format!(
            "value mismatch at {}: actual={}, expected={}",
            path,
            render_side(divergence.actual.as_ref()),
            render_side(divergence.expected.as_ref())
        ),
        DivergenceKind::Added => format!(
            "{noun} at {}: present in actual only, actual={}",
            path,
            render_side(divergence.actual.as_ref())
        ),
        DivergenceKind::Removed => format!(
            "{noun} at {}: present in expected only, expected={}",
            path,
            render_side(divergence.expected.as_ref())
        ),
    };
    DiffMessage(text)
}

fn render_side(value: Option<&Value>) -> String {
    value.map_or_else(|| "<absent>".to_string(), ToString::to_string)
}

fn walk(
    actual: &Value,
    expected: &Value,
    current_path: &str,
    ignore_paths: &[&str],
    out: &mut Vec<Divergence>,
) {
    if !current_path.is_empty() && ignore_paths.contains(&current_path) {
        return;
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => walk_objects(a, e, current_path, ignore_paths, out),
        (Value::Array(a), Value::Array(e)) => walk_arrays(a, e, current_path, ignore_paths, out),
        (a, e) if a == e => {}
        (a, e) => out.push(Divergence {
            path: current_path.to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(a.clone()),
            expected: Some(e.clone()),
        }),
    }
}

fn walk_objects(
    a: &serde_json::Map<String, Value>,
    e: &serde_json::Map<String, Value>,
    current_path: &str,
    ignore_paths: &[&str],
    out: &mut Vec<Divergence>,
) {
    // Filter out keys whose dot-path matches an ignore entry. The
    // semantics of `ignore_paths` are "this subtree is not part of the
    // comparison" -- which has to apply to KEY MEMBERSHIP too, not
    // just to descendant value comparison. Per-provider body flips
    // (e.g. anthropic-api stripping `anthropic_beta`, openai-compat
    // injecting `stream_options`) make a key appear on exactly one
    // side of the diff; the test driver legitimately wants to ignore
    // those without flagging the absence as a key mismatch.
    let key_in_scope =
        |key: &str| -> bool { !ignore_paths.contains(&child_path(current_path, key).as_str()) };
    let keys: BTreeSet<&String> = a
        .keys()
        .chain(e.keys())
        .filter(|k| key_in_scope(k))
        .collect();
    for k in keys {
        let next = child_path(current_path, k);
        match (a.get(k), e.get(k)) {
            (Some(av), Some(ev)) => walk(av, ev, &next, ignore_paths, out),
            (Some(av), None) => out.push(Divergence {
                path: next,
                kind: DivergenceKind::Added,
                actual: Some(av.clone()),
                expected: None,
            }),
            (None, Some(ev)) => out.push(Divergence {
                path: next,
                kind: DivergenceKind::Removed,
                actual: None,
                expected: Some(ev.clone()),
            }),
            (None, None) => unreachable!("key came from one of the two maps"),
        }
    }
}

fn walk_arrays(
    a: &[Value],
    e: &[Value],
    current_path: &str,
    ignore_paths: &[&str],
    out: &mut Vec<Divergence>,
) {
    for i in 0..a.len().max(e.len()) {
        let next = format!("{current_path}[{i}]");
        if ignore_paths.contains(&next.as_str()) {
            continue;
        }
        match (a.get(i), e.get(i)) {
            (Some(av), Some(ev)) => walk(av, ev, &next, ignore_paths, out),
            (Some(av), None) => out.push(Divergence {
                path: next,
                kind: DivergenceKind::Added,
                actual: Some(av.clone()),
                expected: None,
            }),
            (None, Some(ev)) => out.push(Divergence {
                path: next,
                kind: DivergenceKind::Removed,
                actual: None,
                expected: Some(ev.clone()),
            }),
            (None, None) => unreachable!("index is below the longer of the two lengths"),
        }
    }
}

fn child_path(current_path: &str, key: &str) -> String {
    if current_path.is_empty() {
        key.to_string()
    } else {
        format!("{current_path}.{key}")
    }
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

    #[test]
    fn json_equal_fails_on_array_length_mismatch_naming_the_surplus_index() {
        let a = json!({"messages": [1, 2]});
        let e = json!({"messages": [1]});
        let err = assert_json_equal_structural(&a, &e, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("element mismatch"), "got: {msg}");
        assert!(msg.contains("messages[1]"), "got: {msg}");
    }

    #[test]
    fn json_equal_reports_only_the_first_of_several_divergences() {
        let a = json!({"one": 1, "two": 2});
        let e = json!({"one": 9, "two": 8});
        let err = assert_json_equal_structural(&a, &e, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("at one:"), "got: {msg}");
        assert!(!msg.contains("at two:"), "got: {msg}");
    }

    // ---------- diff_all ----------

    fn paths(divergences: &[Divergence]) -> Vec<&str> {
        divergences.iter().map(|d| d.path.as_str()).collect()
    }

    fn at<'a>(divergences: &'a [Divergence], path: &str) -> &'a Divergence {
        divergences
            .iter()
            .find(|d| d.path == path)
            .unwrap_or_else(|| panic!("no divergence at {path}; got {:?}", paths(divergences)))
    }

    #[test]
    fn diff_all_returns_empty_on_identical_values() {
        let v = json!({"model": "x", "messages": [{"role": "user"}]});

        let divergences = diff_all(&v, &v, &[]);

        assert!(divergences.is_empty(), "got: {divergences:?}");
    }

    #[test]
    fn diff_all_returns_every_divergence_among_sibling_keys() {
        let a = json!({"one": 1, "two": 2, "three": 3, "same": 0});
        let e = json!({"one": 9, "two": 8, "three": 7, "same": 0});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 3, "got: {divergences:?}");
        assert_eq!(paths(&divergences), vec!["one", "three", "two"]);
    }

    #[test]
    fn diff_all_reports_divergences_past_a_first_key_mismatch() {
        // The first-mismatch comparator stopped at the `a` key-set
        // difference and never descended into `b`.
        let a = json!({"a": 1, "b": {"c": 1, "d": 2}});
        let e = json!({"b": {"c": 99, "d": 2}});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 2, "got: {divergences:?}");
        assert_eq!(at(&divergences, "a").kind, DivergenceKind::Added);
        assert_eq!(at(&divergences, "b.c").kind, DivergenceKind::Changed);
    }

    #[test]
    fn diff_all_reports_divergences_at_multiple_depths() {
        let a = json!({"top": 1, "mid": {"leaf": "a", "deep": {"x": 1}}});
        let e = json!({"top": 2, "mid": {"leaf": "b", "deep": {"x": 2}}});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 3, "got: {divergences:?}");
        assert_eq!(
            paths(&divergences),
            vec!["mid.deep.x", "mid.leaf", "top"],
            "got: {divergences:?}"
        );
    }

    #[test]
    fn diff_all_reports_every_array_element_divergence_with_indexed_paths() {
        let a = json!({"messages": [{"role": "user"}, {"role": "assistant"}]});
        let e = json!({"messages": [{"role": "system"}, {"role": "tool"}]});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 2, "got: {divergences:?}");
        assert_eq!(
            paths(&divergences),
            vec!["messages[0].role", "messages[1].role"]
        );
        assert_eq!(
            at(&divergences, "messages[0].role").actual,
            Some(json!("user"))
        );
        assert_eq!(
            at(&divergences, "messages[0].role").expected,
            Some(json!("system"))
        );
    }

    #[test]
    fn diff_all_reports_surplus_array_elements_without_hiding_element_divergences() {
        let a = json!({"messages": [{"role": "user"}, {"role": "assistant"}]});
        let e = json!({"messages": [{"role": "system"}]});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 2, "got: {divergences:?}");
        assert_eq!(
            at(&divergences, "messages[0].role").kind,
            DivergenceKind::Changed
        );
        let surplus = at(&divergences, "messages[1]");
        assert_eq!(surplus.kind, DivergenceKind::Added);
        assert_eq!(surplus.actual, Some(json!({"role": "assistant"})));
        assert_eq!(surplus.expected, None);
    }

    #[test]
    fn diff_all_marks_a_missing_array_element_as_removed() {
        let a = json!([1]);
        let e = json!([1, 2]);

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 1, "got: {divergences:?}");
        let missing = at(&divergences, "[1]");
        assert_eq!(missing.kind, DivergenceKind::Removed);
        assert_eq!(missing.actual, None);
        assert_eq!(missing.expected, Some(json!(2)));
    }

    #[test]
    fn diff_all_orients_added_and_removed_to_the_side_that_has_the_key() {
        let a = json!({"only_actual": "from-actual"});
        let e = json!({"only_expected": "from-expected"});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 2, "got: {divergences:?}");
        let added = at(&divergences, "only_actual");
        assert_eq!(added.kind, DivergenceKind::Added);
        assert_eq!(added.actual, Some(json!("from-actual")));
        assert_eq!(added.expected, None);
        let removed = at(&divergences, "only_expected");
        assert_eq!(removed.kind, DivergenceKind::Removed);
        assert_eq!(removed.actual, None);
        assert_eq!(removed.expected, Some(json!("from-expected")));
    }

    #[test]
    fn diff_all_suppresses_an_ignored_path_but_keeps_its_siblings() {
        let a = json!({"id": "abc", "nested": {"drift": 1, "keep": 1}, "other": 1});
        let e = json!({"id": "xyz", "nested": {"drift": 2, "keep": 1}, "other": 2});

        // Positive control: without the ignore entries both drifting
        // paths are reported, so the suppression below is not vacuous.
        let unfiltered = diff_all(&a, &e, &[]);
        assert_eq!(
            paths(&unfiltered),
            vec!["id", "nested.drift", "other"],
            "got: {unfiltered:?}"
        );

        let filtered = diff_all(&a, &e, &["id", "nested.drift"]);

        assert_eq!(paths(&filtered), vec!["other"], "got: {filtered:?}");
    }

    #[test]
    fn diff_all_ignores_a_key_present_on_only_one_side() {
        let a = json!({"model": "x", "anthropic_beta": ["foo"]});
        let e = json!({"model": "x"});

        // Positive control: the asymmetric key IS a divergence when the
        // path is not ignored.
        let unfiltered = diff_all(&a, &e, &[]);
        assert_eq!(paths(&unfiltered), vec!["anthropic_beta"]);
        assert_eq!(
            at(&unfiltered, "anthropic_beta").kind,
            DivergenceKind::Added
        );

        assert!(diff_all(&a, &e, &["anthropic_beta"]).is_empty());
    }

    #[test]
    fn diff_all_orders_paths_by_traversal_not_by_global_sort() {
        // `-` (0x2D) precedes `.` (0x2E), so a global sort of the path
        // strings would put `a-b` first. Traversal order walks `a`'s
        // subtree before its sibling.
        let a = json!({"a": {"z": 1}, "a-b": 1});
        let e = json!({"a": {"z": 2}, "a-b": 2});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(
            paths(&divergences),
            vec!["a.z", "a-b"],
            "got: {divergences:?}"
        );
    }

    #[test]
    fn divergence_display_matches_the_comparator_failure_message() {
        let a = json!({"model": "x"});
        let e = json!({"model": "y"});
        let divergence = diff_all(&a, &e, &[]).remove(0);

        let rendered = divergence.to_string();

        assert_eq!(
            rendered,
            assert_json_equal_structural(&a, &e, &[])
                .unwrap_err()
                .to_string()
        );
    }

    #[test]
    fn diff_all_ignores_an_indexed_array_element_path() {
        let a = json!({"messages": [{"role": "user"}, {"id": "req-1"}]});
        let e = json!({"messages": [{"role": "user"}, {"id": "req-2"}]});

        // Positive control: the element IS a divergence when its indexed
        // path is not ignored.
        let unfiltered = diff_all(&a, &e, &[]);
        assert_eq!(paths(&unfiltered), vec!["messages[1].id"]);

        assert!(diff_all(&a, &e, &["messages[1]"]).is_empty());
    }

    #[test]
    fn diff_all_ignores_a_surplus_array_element_at_an_indexed_path() {
        // The old comparator returned on the length mismatch before any
        // per-index ignore could apply, so this branch is new behavior.
        let a = json!({"messages": [{"role": "user"}, {"role": "system"}]});
        let e = json!({"messages": [{"role": "user"}]});

        // Positive control: the surplus element IS a divergence when its
        // indexed path is not ignored.
        let unfiltered = diff_all(&a, &e, &[]);
        assert_eq!(paths(&unfiltered), vec!["messages[1]"]);
        assert_eq!(at(&unfiltered, "messages[1]").kind, DivergenceKind::Added);

        assert!(diff_all(&a, &e, &["messages[1]"]).is_empty());
    }

    /// A caller's normalization step: drop every `role:"system"` turn
    /// from `.messages`, matching the system-turn lift under
    /// `SystemTurnPolicy::Lift`. That policy is resolved per-request at
    /// `crates/routectl-providers/src/anthropic_api/request.rs:577` from
    /// `forward_system_turns && filtered_system.is_some()`, so this
    /// removal is only the right normalization for a Lift-policy
    /// capture -- under `Forward` the turns ride the wire in place and
    /// removing them would introduce a misalignment.
    fn without_system_turns(body: &Value) -> Value {
        let mut out = body.clone();
        let kept: Vec<Value> = out["messages"]
            .as_array()
            .expect("fixture has a messages array")
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
            .cloned()
            .collect();
        out["messages"] = Value::Array(kept);
        out
    }

    #[test]
    fn diff_all_returns_empty_after_the_caller_normalizes_away_a_middle_removal() {
        // The system-turn lift removes turns from the MIDDLE of
        // `.messages`, so every later element shifts index. Pins the
        // normalize-then-diff seam: the caller applies its known
        // transform first, then diffs.
        let ingress = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "system", "content": "lifted"},
                {"role": "assistant", "content": "two"},
                {"role": "system", "content": "also lifted"},
                {"role": "user", "content": "three"},
            ],
        });
        let outgoing = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "user", "content": "three"},
            ],
        });

        // Positive control: diffed raw, the middle removal shifts every
        // surviving element and the divergence set is broad enough that
        // a per-index whitelist would cover the whole array.
        let raw = diff_all(&ingress, &outgoing, &[]);
        assert_eq!(
            paths(&raw),
            vec![
                "messages[1].content",
                "messages[1].role",
                "messages[2].content",
                "messages[2].role",
                "messages[3]",
                "messages[4]",
            ],
            "got: {raw:?}"
        );

        let normalized = diff_all(&without_system_turns(&ingress), &outgoing, &[]);

        assert!(normalized.is_empty(), "got: {normalized:?}");
    }

    #[test]
    fn diff_all_reports_a_type_change_as_changed_on_the_container_path() {
        let a = json!({"content": "text"});
        let e = json!({"content": [{"type": "text"}]});

        let divergences = diff_all(&a, &e, &[]);

        assert_eq!(divergences.len(), 1, "got: {divergences:?}");
        assert_eq!(at(&divergences, "content").kind, DivergenceKind::Changed);
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
