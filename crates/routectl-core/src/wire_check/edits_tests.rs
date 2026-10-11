use std::collections::HashSet;

use super::{DeclaredEdit, EditKind, EditPatternError, EditSite, PathClass, RejectionProvenance};
use crate::safe_token::is_safe_token;
use crate::wire_check::path::WirePath;

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
        ("cache marker", "/messages/*/content/*/cache_control"),
        ("concrete tool", "/tools/1"),
        ("tool field", "/tools/*/name"),
        ("top-level key", "/stream"),
        ("system block marker", "/system/0/cache_control"),
    ];

    for (name, pattern) in rows {
        let accepted = DeclaredEdit::new(EditSite::Cloak, pattern, EditKind::Rewrite);

        assert!(accepted.is_ok(), "row {name}: {accepted:?}");
    }
}

#[test]
fn constructor_refuses_patterns_that_cannot_match() {
    let nine_deep = "/a/b/c/d/e/f/g/h/i";
    let rows: [(&str, &'static str, EditPatternError); 5] = [
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
    ];

    for (name, pattern, expected) in rows {
        let refused = DeclaredEdit::new(EditSite::Cloak, pattern, EditKind::Remove);

        assert_eq!(refused, Err(expected), "row {name}");
    }
}

#[test]
fn wildcard_matches_exactly_one_segment() {
    let rows: [(&str, &'static str, &[&str], bool); 9] = [
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
            "unsafe key surfaces as wildcard",
            "/metadata/*",
            &["metadata", "a b"],
            true,
        ),
    ];

    for (name, pattern, segments, expected) in rows {
        let path = WirePath::from_segments(segments.iter().copied());

        assert_eq!(edit(pattern).matches(&path), expected, "row {name}");
    }
}

#[test]
fn tools_child_pattern_never_matches_a_grandchild() {
    let pattern = edit("/tools/1");

    assert!(!pattern.matches(&WirePath::from_segments(["tools", "1", "name"])));
}

#[test]
fn escaped_segments_match_their_escaped_pattern() {
    let path = WirePath::from_segments(["metadata", "a/b~c"]);

    assert!(edit("/metadata/a~1b~0c").matches(&path));
    assert!(!edit("/metadata/a/b~c").matches(&path));
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
