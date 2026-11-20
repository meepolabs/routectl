//! Tests for the scope -> model family table and its once-per-tag log.
//!
//! The log set is process-global, so every test that reaches it through
//! [`binds`] uses a scope tag no other test in the binary uses: a shared tag
//! would let a sibling test claim the one line first.

use super::*;

use std::sync::{Arc, Barrier};

const IGNORED_EVENT: &str = "placement_scope_ignored";

/// The ignored-scope lines `f` emitted for `scope`.
fn ignored_lines_for(scope: &str, f: impl FnOnce()) -> usize {
    routectl_testkit::capture_events(f)
        .iter()
        .filter(|event| event.field("event") == Some(IGNORED_EVENT))
        .filter(|event| event.field("scope") == Some(scope))
        .count()
}

#[test]
fn a_known_scope_binds_only_its_own_model_family() {
    let rows = [
        ("opus on an opus model", "opus", "claude-opus-4-7", true),
        ("opus on a sonnet model", "opus", "claude-sonnet-4-5", false),
        (
            "sonnet on a sonnet model",
            "sonnet",
            "claude-sonnet-4-5",
            true,
        ),
        (
            "sonnet on an opus model",
            "sonnet",
            "claude-opus-4-7",
            false,
        ),
        (
            "opus on a model only containing the tag",
            "opus",
            "my-claude-opus-4",
            false,
        ),
        ("opus on a bare family name", "opus", "claude-opus", false),
        ("opus on an empty model", "opus", "", false),
        ("opus on a codex model", "opus", "gpt-5-codex", false),
    ];
    for (name, scope, model, expected) in rows {
        assert_eq!(binds(scope, model), expected, "row: {name}");
    }
}

#[test]
fn an_unknown_scope_never_binds_whatever_the_model() {
    let rows = [
        (
            "a scope containing a known tag",
            "opus_extra",
            "claude-opus-4-7",
        ),
        ("a codex extra limit", "codex_bengalfox", "gpt-5-codex"),
        (
            "a scope naming the model exactly",
            "claude_opus",
            "claude-opus-4-7",
        ),
    ];
    for (name, scope, model) in rows {
        assert!(!binds(scope, model), "row: {name}");
    }
}

#[test]
fn an_unknown_scope_is_logged_once_and_a_known_one_never() {
    let unknown = "scope_logged_once_probe";

    let first = ignored_lines_for(unknown, || {
        binds(unknown, "claude-opus-4-7");
        binds(unknown, "claude-opus-4-7");
    });
    let later = ignored_lines_for(unknown, || {
        binds(unknown, "claude-sonnet-4-5");
    });
    let known = routectl_testkit::capture_events(|| {
        binds("opus", "claude-sonnet-4-5");
    })
    .iter()
    .filter(|event| event.field("event") == Some(IGNORED_EVENT))
    .count();

    assert_eq!(first, 1, "the first sighting logs exactly once");
    assert_eq!(later, 0, "a later sighting stays silent");
    assert_eq!(known, 0, "a known scope is not ignored");
}

#[test]
fn concurrent_sightings_of_one_unknown_scope_claim_one_line() {
    // Arrange
    const THREADS: usize = 64;
    let log = Arc::new(ScopeLog::with_capacity(LOGGED_SCOPE_CAPACITY));
    let barrier = Arc::new(Barrier::new(THREADS));

    // Act
    let claims: usize = (0..THREADS)
        .map(|_| {
            let log = Arc::clone(&log);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                usize::from(log.claim("codex_racing"))
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().expect("a claiming thread"))
        .sum();

    // Assert
    assert_eq!(claims, 1);
}

#[test]
fn concurrent_sightings_through_the_global_log_emit_one_line() {
    const THREADS: usize = 32;
    let scope = "scope_global_race_probe";
    let barrier = Arc::new(Barrier::new(THREADS));

    let lines: usize = (0..THREADS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                ignored_lines_for(scope, || {
                    barrier.wait();
                    binds(scope, "claude-opus-4-7");
                })
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().expect("a sighting thread"))
        .sum();

    assert_eq!(lines, 1);
}

#[test]
fn the_log_set_stops_growing_at_its_capacity() {
    let log = ScopeLog::with_capacity(2);

    let claimed: Vec<bool> = ["s_a", "s_b", "s_c", "s_a"]
        .into_iter()
        .map(|scope| log.claim(scope))
        .collect();

    assert_eq!(claimed, vec![true, true, false, false]);
    assert_eq!(log.seen.lock().len(), 2);
}
