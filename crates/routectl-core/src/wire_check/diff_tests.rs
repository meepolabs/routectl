use serde_json::{Value, json};

use super::{DiffOutcome, Finding, MAX_DIFF_DEPTH, MAX_DIFF_NODES, Walker, diff};
use crate::wire_check::edits::{DeclaredEdit, EditKind, EditSite, PathClass};
use crate::wire_check::lane::LaneClass;
use crate::wire_check::path::WirePath;

fn edit(pattern: &'static str, kind: EditKind) -> DeclaredEdit {
    DeclaredEdit::new(EditSite::FieldRepair, pattern, kind)
        .unwrap_or_else(|e| panic!("pattern {pattern:?} refused: {e}"))
}

fn finding(pointer: &str, class: PathClass) -> Finding {
    let path = if pointer.is_empty() {
        WirePath::root()
    } else {
        WirePath::from_segments(pointer.trim_start_matches('/').split('/'))
    };
    Finding { path, class }
}

fn fidelity(input: &Value, output: &Value, edits: &[DeclaredEdit]) -> DiffOutcome {
    diff(input, output, LaneClass::Fidelity, edits)
}

/// `0` nested inside `depth` single-element arrays, built iteratively.
fn nested(depth: usize) -> Value {
    (0..depth).fold(json!(0), |inner, _| Value::Array(vec![inner]))
}

/// Unwind a value built by [`nested`] one level at a time; the recursive
/// `Drop` of `Value` would overflow the test thread's stack at full depth.
fn dismantle(mut value: Value) {
    while let Value::Array(items) = &mut value {
        match items.pop() {
            Some(inner) => value = inner,
            None => return,
        }
    }
}

#[test]
fn fidelity_rules_classify_each_difference() {
    let rows: [(&str, Value, Value, Vec<Finding>); 9] = [
        (
            "key reorder",
            json!({"a": 1, "b": {"c": 2, "d": 3}}),
            json!({"b": {"d": 3, "c": 2}, "a": 1}),
            vec![],
        ),
        (
            "integer vs float",
            json!({"n": 1}),
            json!({"n": 1.0}),
            vec![],
        ),
        (
            "negative integer vs float",
            json!({"n": -3}),
            json!({"n": -3.0}),
            vec![],
        ),
        (
            "string vs number",
            json!({"n": "1"}),
            json!({"n": 1}),
            vec![finding("/n", PathClass::Changed)],
        ),
        (
            "absent key",
            json!({"a": 1, "b": 2}),
            json!({"a": 1}),
            vec![finding("/b", PathClass::Missing)],
        ),
        (
            "extra key",
            json!({"a": 1}),
            json!({"a": 1, "b": 2}),
            vec![finding("/b", PathClass::Added)],
        ),
        (
            "swapped array elements",
            json!({"t": [{"x": 1}, {"y": 2}]}),
            json!({"t": [{"y": 2}, {"x": 1}]}),
            vec![finding("/t/0", PathClass::Moved)],
        ),
        (
            "cross-parent move",
            json!({"a": [{"k": 1}], "b": []}),
            json!({"a": [], "b": [{"k": 1}]}),
            vec![
                finding("/a/0", PathClass::Missing),
                finding("/b/0", PathClass::Added),
            ],
        ),
        (
            "null is not false",
            json!({"f": null}),
            json!({"f": false}),
            vec![finding("/f", PathClass::Changed)],
        ),
    ];

    for (name, input, output, expected) in rows {
        let outcome = fidelity(&input, &output, &[]);

        assert_eq!(outcome.findings, expected, "row {name}");
        assert!(!outcome.truncated, "row {name}");
    }
}

#[test]
fn array_shift_after_an_undeclared_removal_is_one_missing_element() {
    let input = json!({"t": ["a", "b", "c", "d"]});
    let output = json!({"t": ["a", "c", "d"]});

    let outcome = fidelity(&input, &output, &[]);

    assert_eq!(outcome.findings, [finding("/t/1", PathClass::Missing)]);
}

/// One fixture per class, each producing exactly one finding at `/b` or
/// `/b/0`.
fn class_fixtures() -> [(PathClass, &'static str, Value, Value); 4] {
    [
        (
            PathClass::Missing,
            "/b",
            json!({"a": 1, "b": 2}),
            json!({"a": 1}),
        ),
        (
            PathClass::Added,
            "/b",
            json!({"a": 1}),
            json!({"a": 1, "b": 2}),
        ),
        (PathClass::Changed, "/b", json!({"b": 1}), json!({"b": 2})),
        (
            PathClass::Moved,
            "/b/0",
            json!({"b": [1, 2, 3]}),
            json!({"b": [2, 3, 1]}),
        ),
    ]
}

const fn kind_for(class: PathClass) -> EditKind {
    match class {
        PathClass::Missing => EditKind::Remove,
        PathClass::Added => EditKind::Add,
        PathClass::Changed => EditKind::Rewrite,
        _ => EditKind::Relocate,
    }
}

#[test]
fn fixtures_each_produce_their_one_finding() {
    for (class, pointer, input, output) in class_fixtures() {
        let outcome = fidelity(&input, &output, &[]);

        assert_eq!(
            outcome.findings,
            [finding(pointer, class)],
            "fixture {}",
            class.label()
        );
    }
}

#[test]
fn declared_edit_suppresses_its_own_class_at_a_matching_pattern() {
    for (class, pointer, input, output) in class_fixtures() {
        let declared = [edit(pointer, kind_for(class))];

        let outcome = fidelity(&input, &output, &declared);

        assert_eq!(outcome.findings, [], "fixture {}", class.label());
    }
}

#[test]
fn declared_edit_suppresses_nothing_at_a_non_matching_path() {
    for (class, pointer, input, output) in class_fixtures() {
        let declared = [
            edit("/other", kind_for(class)),
            edit("/b/9", kind_for(class)),
        ];

        let outcome = fidelity(&input, &output, &declared);

        assert_eq!(
            outcome.findings,
            [finding(pointer, class)],
            "fixture {}",
            class.label()
        );
    }
}

#[test]
fn declared_edit_of_another_kind_leaves_the_difference_detected() {
    for (class, pointer, input, output) in class_fixtures() {
        for kind in EditKind::ALL.into_iter().filter(|k| *k != kind_for(class)) {
            let declared = [edit(pointer, kind)];

            let outcome = fidelity(&input, &output, &declared);

            assert!(
                !outcome.findings.is_empty(),
                "fixture {} with {}",
                class.label(),
                kind.label()
            );
        }
    }
}

#[test]
fn wildcard_pattern_suppresses_every_matching_element() {
    let input = json!({"t": [{"id": "a"}, {"id": "b"}]});
    let output = json!({"t": [{"id": "x"}, {"id": "y"}]});
    let declared = [edit("/t/*/id", EditKind::Rewrite)];

    let outcome = fidelity(&input, &output, &declared);

    assert_eq!(outcome.findings, []);
}

#[test]
fn declared_middle_remove_explains_the_shift_of_later_elements() {
    let rows: [(&str, Value, Value, Vec<Finding>); 3] = [
        (
            "removal alone",
            json!({"tools": ["a", "b", "c", "d"]}),
            json!({"tools": ["a", "c", "d"]}),
            vec![],
        ),
        (
            "later element changed",
            json!({"tools": [{"n": "a"}, {"n": "b"}, {"n": "c"}]}),
            json!({"tools": [{"n": "a"}, {"n": "c2"}]}),
            vec![finding("/tools/2/n", PathClass::Changed)],
        ),
        (
            "removed element also kept",
            json!({"tools": ["a", "b", "c"]}),
            json!({"tools": ["a", "b", "c"]}),
            vec![finding("/tools/1", PathClass::Added)],
        ),
    ];
    let declared = [edit("/tools/1", EditKind::Remove)];

    for (name, input, output, expected) in rows {
        let outcome = fidelity(&input, &output, &declared);

        assert_eq!(outcome.findings, expected, "row {name}");
    }
}

#[test]
fn undeclared_middle_remove_misattributes_a_later_change() {
    let input = json!({"tools": [{"n": "a"}, {"n": "b"}, {"n": "c"}]});
    let output = json!({"tools": [{"n": "a"}, {"n": "c2"}]});

    let outcome = fidelity(&input, &output, &[]);

    assert_eq!(
        outcome.findings,
        [
            finding("/tools/1/n", PathClass::Changed),
            finding("/tools/2", PathClass::Missing),
        ]
    );
}

#[test]
fn declared_add_explains_the_shift_of_later_elements() {
    let input = json!({"tools": ["a", "c", "d"]});
    let output = json!({"tools": ["a", "b", "c", "d"]});
    let declared = [edit("/tools/1", EditKind::Add)];

    let outcome = fidelity(&input, &output, &declared);

    assert_eq!(outcome.findings, []);
}

#[test]
fn relocate_pairs_the_two_ends_of_a_cross_parent_move() {
    let rows: [(&str, Value, Value, &'static str, Vec<Finding>); 4] = [
        (
            "declared at the source",
            json!({"a": {"k": 1}}),
            json!({"b": {"k": 1}}),
            "/a",
            vec![],
        ),
        (
            "declared at the destination",
            json!({"a": {"k": 1}}),
            json!({"b": {"k": 1}}),
            "/b",
            vec![],
        ),
        (
            "value changed on the way",
            json!({"a": {"k": 1}}),
            json!({"b": {"k": 2}}),
            "/a",
            vec![
                finding("/a", PathClass::Missing),
                finding("/b", PathClass::Added),
            ],
        ),
        (
            "nothing arrived",
            json!({"a": {"k": 1}}),
            json!({}),
            "/a",
            vec![finding("/a", PathClass::Missing)],
        ),
    ];

    for (name, input, output, pattern, expected) in rows {
        let declared = [edit(pattern, EditKind::Relocate)];

        let outcome = fidelity(&input, &output, &declared);

        assert_eq!(outcome.findings, expected, "row {name}");
    }
}

#[test]
fn translation_reports_only_unconsumed_top_level_extras() {
    let extras = json!({"consumed": {"n": 1}, "dropped": true});
    let egress = json!({"model": "m", "consumed": {"n": 2, "other": 3}});

    let outcome = diff(&extras, &egress, LaneClass::Translation, &[]);

    assert_eq!(
        outcome.findings,
        [finding("/dropped", PathClass::TranslationUnconsumed)]
    );
    assert!(!outcome.truncated);
}

#[test]
fn translation_tolerates_non_object_bodies() {
    let rows: [(&str, Value, Value, Vec<Finding>); 3] = [
        ("scalar extras", json!(1), json!({"a": 1}), vec![]),
        (
            "array egress",
            json!({"a": 1}),
            json!([1]),
            vec![finding("/a", PathClass::TranslationUnconsumed)],
        ),
        ("null both", Value::Null, Value::Null, vec![]),
    ];

    for (name, extras, egress, expected) in rows {
        let outcome = diff(&extras, &egress, LaneClass::Translation, &[]);

        assert_eq!(outcome.findings, expected, "row {name}");
    }
}

#[test]
fn non_object_roots_compare_without_panic() {
    let rows: [(&str, Value, Value, Vec<Finding>); 5] = [
        ("null roots", Value::Null, Value::Null, vec![]),
        (
            "scalar roots differ",
            json!("1"),
            json!(1),
            vec![finding("", PathClass::Changed)],
        ),
        ("array roots equal", json!([1, 2]), json!([1, 2.0]), vec![]),
        (
            "array vs object root",
            json!([1]),
            json!({"a": 1}),
            vec![finding("", PathClass::Changed)],
        ),
        (
            "empty arrays",
            json!([]),
            json!([1]),
            vec![finding("/0", PathClass::Added)],
        ),
    ];

    for (name, input, output, expected) in rows {
        let outcome = fidelity(&input, &output, &[]);

        assert_eq!(outcome.findings, expected, "row {name}");
        assert!(!outcome.truncated, "row {name}");
    }
}

#[test]
fn deep_nesting_stops_at_the_depth_cap() {
    let input = nested(100_000);
    let output = nested(100_000);

    let outcome = fidelity(&input, &output, &[]);

    assert!(outcome.truncated);
    dismantle(input);
    dismantle(output);
}

#[test]
fn nesting_at_the_depth_cap_is_compared_fully() {
    let input = nested(MAX_DIFF_DEPTH);
    let output = nested(MAX_DIFF_DEPTH);

    let outcome = fidelity(&input, &output, &[]);

    assert_eq!(outcome, DiffOutcome::default());
}

#[test]
fn node_budget_truncates_a_huge_array() {
    let rows = [
        ("over the node cap", 300_000_usize),
        ("one million", 1_000_000),
    ];

    for (name, len) in rows {
        let input = Value::Array((0..len).map(|n| json!(n)).collect());
        let output = Value::Array((0..len).map(|n| json!(n)).collect());

        let outcome = fidelity(&input, &output, &[]);

        assert!(outcome.truncated, "row {name}");
    }
}

#[test]
fn node_budget_keeps_findings_found_before_it_stopped() {
    let mut items: Vec<Value> = (0..MAX_DIFF_NODES).map(|n| json!(n)).collect();
    let input = json!({"a": 1, "z": Value::Array(items.clone())});
    items.reverse();
    let output = json!({"a": 2, "z": Value::Array(items)});

    let outcome = fidelity(&input, &output, &[]);

    assert!(outcome.truncated);
    assert_eq!(
        outcome.findings.first(),
        Some(&finding("/a", PathClass::Changed))
    );
}

#[test]
fn array_under_the_node_cap_is_not_truncated() {
    let len = MAX_DIFF_NODES / 4;
    let input = Value::Array((0..len).map(|n| json!(n)).collect());
    let output = Value::Array((0..len).map(|n| json!(n)).collect());

    let outcome = fidelity(&input, &output, &[]);

    assert_eq!(outcome, DiffOutcome::default());
}

#[test]
fn unsafe_keys_surface_as_the_wildcard_segment() {
    let input = json!({"ok": {"bad key\n": 1, "caf\u{e9}": 2}});
    let output = json!({"ok": {}});

    let outcome = fidelity(&input, &output, &[]);

    let pointers: Vec<&str> = outcome.findings.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(pointers, ["/ok/*", "/ok/*"]);
}

#[test]
fn findings_come_out_in_a_deterministic_order() {
    let input = json!({"z": 1, "m": [3, 1, 2], "a": {"q": 1, "b": 2}, "k": "v"});
    let output = json!({"a": {"b": 3}, "m": [1, 2, 3], "z": "1", "n": 0});

    let first = fidelity(&input, &output, &[]);
    let second = fidelity(&input, &output, &[]);

    assert_eq!(first, second);
    assert_eq!(
        first.findings,
        [
            finding("/a/b", PathClass::Changed),
            finding("/a/q", PathClass::Missing),
            finding("/k", PathClass::Missing),
            finding("/m/0", PathClass::Moved),
            finding("/z", PathClass::Changed),
            finding("/n", PathClass::Added),
        ]
    );
}

#[test]
fn integer_and_float_compare_exactly_at_the_64_bit_boundaries() {
    let two_pow_53 = 9_007_199_254_740_992_u64;
    let rows: [(&str, Value, Value, bool); 9] = [
        (
            "2^53 vs its float",
            json!(two_pow_53),
            json!(9_007_199_254_740_992.0),
            true,
        ),
        (
            "2^53 + 1 vs the float 2^53",
            json!(two_pow_53 + 1),
            json!(9_007_199_254_740_992.0),
            false,
        ),
        (
            "u64::MAX vs its nearest float",
            json!(u64::MAX),
            json!(u64::MAX as f64),
            false,
        ),
        (
            "i64::MIN vs its float",
            json!(i64::MIN),
            json!(i64::MIN as f64),
            true,
        ),
        (
            "i64::MIN + 1 vs the float i64::MIN",
            json!(i64::MIN + 1),
            json!(i64::MIN as f64),
            false,
        ),
        ("negative zero vs zero", json!(-0.0), json!(0), true),
        ("zero vs negative zero", json!(0), json!(-0.0), true),
        ("integer vs fractional float", json!(2), json!(2.5), false),
        ("float vs float", json!(0.5), json!(0.5), true),
    ];

    for (name, input, output, equal) in rows {
        let outcome = fidelity(&json!({"n": input}), &json!({"n": output}), &[]);

        let expected = if equal {
            vec![]
        } else {
            vec![finding("/n", PathClass::Changed)]
        };
        assert_eq!(outcome.findings, expected, "row {name}");
    }
}

#[test]
fn array_work_stays_inside_the_node_budget() {
    let rows = [
        ("one huge pair", 1_000_000_usize, 1),
        ("many arrays each under the cap", 60_000, 8),
    ];

    for (name, len, count) in rows {
        let array = Value::Array((0..len).map(|n| json!(n)).collect());
        let body = Value::Object((0..count).map(|k| (k.to_string(), array.clone())).collect());
        let mut walker = Walker::new(&[]);

        walker.compare(&body, &body, &WirePath::root(), 0);

        assert!(walker.truncated, "row {name}");
        assert!(
            walker.visited <= MAX_DIFF_NODES && walker.collected <= MAX_DIFF_NODES,
            "row {name}: visited {} collected {}",
            walker.visited,
            walker.collected
        );
    }
}

#[test]
fn declaration_at_a_coarsened_ancestor_suppresses_nothing() {
    let deep_in = json!({"a": {"b": {"c": {"d": {"e": {"f": {"g": {"h": {"i": 1}}}}}}}}});
    let deep_out = json!({"a": {"b": {"c": {"d": {"e": {"f": {"g": {"h": {"i": 2}}}}}}}}});
    let wide = "~".repeat(64);
    let long_in = json!({&wide: {&wide: {"x": 1}}});
    let long_out = json!({&wide: {&wide: {"x": 2}}});
    let long_ancestor: &'static str = Box::leak(format!("/{}", "~0".repeat(64)).into_boxed_str());
    let rows: [(&str, Value, Value, &'static str, &str); 2] = [
        (
            "depth cap",
            deep_in,
            deep_out,
            "/a/b/c/d/e/f/g/h",
            "/a/b/c/d/e/f/g/h",
        ),
        (
            "length cap",
            long_in,
            long_out,
            long_ancestor,
            long_ancestor,
        ),
    ];

    for (name, input, output, pattern, rendered) in rows {
        let declared = [edit(pattern, EditKind::Rewrite)];

        let outcome = fidelity(&input, &output, &declared);

        let pointers: Vec<&str> = outcome.findings.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(pointers, [rendered], "row {name}");
        assert!(
            outcome.findings.iter().all(|f| f.path.is_coarsened()),
            "row {name}"
        );
    }
}

#[test]
fn wildcard_declaration_spares_safe_sibling_keys() {
    let input = json!({"x": {"unsafe key": 1, "safe": 1}});
    let output = json!({"x": {"unsafe key": 2, "safe": 2}});
    let rows: [(&str, Vec<DeclaredEdit>, Vec<Finding>); 2] = [
        (
            "undeclared",
            vec![],
            vec![
                finding("/x/safe", PathClass::Changed),
                finding("/x/*", PathClass::Changed),
            ],
        ),
        (
            "wildcard declared",
            vec![edit("/x/*", EditKind::Rewrite)],
            vec![finding("/x/safe", PathClass::Changed)],
        ),
    ];

    for (name, declared, expected) in rows {
        let outcome = fidelity(&input, &output, &declared);

        assert_eq!(outcome.findings, expected, "row {name}");
    }
}
