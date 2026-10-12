use std::collections::HashSet;

use super::{DeclaredEdit, EditKind, EditPatternError, EditSite, PathClass, RejectionProvenance};
use crate::safe_token::is_safe_token;
use crate::wire_check::path::WirePath;

/// A path where each all-digit segment is an array index and every other
/// segment is an object key.
fn path_of(segments: &[&str]) -> WirePath {
    segments.iter().fold(WirePath::root(), |path, segment| {
        match segment.parse::<usize>() {
            Ok(index) => path.index(index),
            Err(_) => path.child(segment),
        }
    })
}

fn edit(pattern: &'static str) -> DeclaredEdit {
    DeclaredEdit::new(EditSite::CachePlacement, pattern, EditKind::Add)
        .unwrap_or_else(|e| panic!("pattern {pattern:?} refused: {e}"))
}

#[test]
fn constructor_refuses_each_blanket_pattern() {
    let rows = [
        ("empty root", ""),
        ("root slash", "/"),
        ("bare top-level child", "/*"),
        ("messages", "/messages"),
        ("every message", "/messages/*"),
        ("system", "/system"),
        ("every system block", "/system/*"),
        ("tools", "/tools"),
        ("every tool", "/tools/*"),
    ];

    for (name, pattern) in rows {
        let refused = DeclaredEdit::new(EditSite::Cloak, pattern, EditKind::Remove);

        assert_eq!(refused, Err(EditPatternError::Blanket), "row {name}");
    }
}

#[test]
fn constructor_accepts_narrow_patterns() {
    let rows = [
        ("masked key under metadata", "/metadata/*"),
        ("32-byte key", "/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"),
        ("hyphenated key", "/client_metadata"),
        ("cache marker", "/messages/*/content/*/cache_control"),
        ("concrete tool", "/tools/1"),
        ("tool field", "/tools/*/name"),
        ("top-level key", "/stream"),
        ("system block marker", "/system/0/cache_control"),
        (
            "schema keyword below a property name",
            "/tools/0/input_schema/properties/*/additionalProperties",
        ),
        (
            "schema keyword below a wildcard tool",
            "/tools/*/function/parameters/additionalProperties",
        ),
        (
            "schema keyword below a pattern-property regex",
            "/output_config/format/schema/patternProperties/*/type",
        ),
    ];

    for (name, pattern) in rows {
        let accepted = DeclaredEdit::new(EditSite::Cloak, pattern, EditKind::Rewrite);

        assert!(accepted.is_ok(), "row {name}: {accepted:?}");
    }
}

#[test]
fn constructor_refuses_patterns_that_cannot_match() {
    let nine_deep = "/a/b/c/d/e/f/g/h/i";
    let rows: [(&str, &'static str, EditPatternError); 13] = [
        ("escaped key", "/a~0b~1c", EditPatternError::Unmatchable),
        (
            "33-byte key",
            "/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk",
            EditPatternError::Unmatchable,
        ),
        (
            "verbatim key under metadata",
            "/metadata/user_id",
            EditPatternError::Unmatchable,
        ),
        (
            "non-keyword key in a tool schema",
            "/tools/0/input_schema/account_id",
            EditPatternError::Unmatchable,
        ),
        (
            "verbatim property name",
            "/tools/0/input_schema/properties/account_id",
            EditPatternError::Unmatchable,
        ),
        (
            "keyword-named property name",
            "/output_config/format/schema/properties/additionalProperties",
            EditPatternError::Unmatchable,
        ),
        (
            "verbatim key under an instance-valued keyword",
            "/output_config/format/schema/default/type",
            EditPatternError::Unmatchable,
        ),
        (
            "no leading slash",
            "messages/0",
            EditPatternError::NotAPointer,
        ),
        (
            "empty middle segment",
            "/messages//content",
            EditPatternError::Unmatchable,
        ),
        ("trailing slash", "/stream/", EditPatternError::Unmatchable),
        (
            "whitespace segment",
            "/two words",
            EditPatternError::Unmatchable,
        ),
        (
            "deeper than a path",
            nine_deep,
            EditPatternError::Unmatchable,
        ),
        ("leading digit key", "/1st", EditPatternError::Unmatchable),
    ];

    for (name, pattern, expected) in rows {
        let refused = DeclaredEdit::new(EditSite::Cloak, pattern, EditKind::Remove);

        assert_eq!(refused, Err(expected), "row {name}");
    }
}

#[test]
fn wildcard_matches_exactly_one_segment() {
    let rows: [(&str, &'static str, &[&str], bool); 12] = [
        ("concrete exact", "/tools/1", &["tools", "1"], true),
        ("concrete other index", "/tools/1", &["tools", "2"], false),
        (
            "concrete prefix only",
            "/tools/1",
            &["tools", "1", "name"],
            false,
        ),
        (
            "concrete longer than path",
            "/tools/1/name",
            &["tools", "1"],
            false,
        ),
        (
            "wildcard one segment",
            "/tools/*/name",
            &["tools", "7", "name"],
            true,
        ),
        (
            "wildcard is not two segments",
            "/tools/*/name",
            &["tools", "1", "x", "name"],
            false,
        ),
        (
            "wildcard needs a segment",
            "/tools/*/name",
            &["tools", "name"],
            false,
        ),
        (
            "nested wildcards",
            "/messages/*/content/*/cache_control",
            &["messages", "3", "content", "0", "cache_control"],
            true,
        ),
        (
            "wildcard matches a non-identifier key",
            "/x/*",
            &["x", "a b"],
            true,
        ),
        (
            "wildcard matches a key masked under metadata",
            "/metadata/*",
            &["metadata", "user_id"],
            true,
        ),
        (
            "wildcard never matches a verbatim key",
            "/x/*",
            &["x", "user_id"],
            false,
        ),
        (
            "concrete index matches an index",
            "/tools/7/name",
            &["tools", "7", "name"],
            true,
        ),
    ];

    for (name, pattern, segments, expected) in rows {
        let path = path_of(segments);

        assert_eq!(edit(pattern).matches(&path), expected, "row {name}");
    }
}

#[test]
fn tools_child_pattern_never_matches_a_grandchild() {
    let pattern = edit("/tools/1");

    assert!(!pattern.matches(&path_of(&["tools", "1", "name"])));
}

#[test]
fn coarsened_path_matches_no_pattern() {
    let deep = path_of(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
    let rows = [
        ("exact rendered text", "/a/b/c/d/e/f/g/h"),
        ("wildcard tail", "/a/b/c/d/e/f/g/*"),
    ];

    for (name, pattern) in rows {
        assert!(!edit(pattern).matches(&deep), "row {name}");
    }
}

#[test]
fn every_label_is_a_unique_safe_token() {
    let labels: Vec<&str> = EditSite::ALL
        .iter()
        .map(|s| s.label())
        .chain(EditKind::ALL.iter().map(|k| k.label()))
        .chain(PathClass::ALL.iter().map(|c| c.label()))
        .chain(RejectionProvenance::ALL.iter().map(|p| p.label()))
        .collect();

    for label in &labels {
        assert!(is_safe_token(label), "{label:?}");
    }
    let sites: HashSet<&str> = EditSite::ALL.iter().map(|s| s.label()).collect();
    assert_eq!(sites.len(), EditSite::ALL.len());
}

#[test]
fn closed_label_sets_spell_the_contract_vocabulary() {
    let classes: Vec<&str> = PathClass::ALL.iter().map(|c| c.label()).collect();
    let provenances: Vec<&str> = RejectionProvenance::ALL.iter().map(|p| p.label()).collect();

    assert_eq!(
        classes,
        [
            "missing",
            "moved",
            "changed",
            "added",
            "translation_unconsumed"
        ]
    );
    assert_eq!(
        provenances,
        [
            "client_unchanged",
            "client_changed",
            "routectl_added",
            "unlocalized",
            "n_a"
        ]
    );
}
