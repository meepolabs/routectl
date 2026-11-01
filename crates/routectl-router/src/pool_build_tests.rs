//! Pure-shape tests for the pool build report predicates.

use super::*;

fn placeholder(member: &str) -> PoolPlaceholder {
    PoolPlaceholder {
        member: member.to_string(),
        provider_kind: "anthropic-api",
        cause: UnusableCause::NotLoggedIn,
        account: Some(format!("anthropic#{member}")),
    }
}

fn report(usable: usize, placeholders: Vec<PoolPlaceholder>) -> PoolReport {
    PoolReport {
        pool: "anthropic-pool".to_string(),
        models: vec!["opus".to_string()],
        configured_members: usable + placeholders.len(),
        usable_members: usable,
        placeholders,
    }
}

#[test]
fn a_pool_with_a_placeholder_reads_as_degraded_not_unavailable() {
    for (label, usable) in [("a built survivor", 1), ("placeholders alone", 0)] {
        let r = report(usable, vec![placeholder("seat-b")]);

        assert!(r.is_degraded(), "{label}");
        assert!(!r.is_unavailable(), "{label}: the placeholder is a seat");
    }
}

#[test]
fn a_fully_built_pool_is_neither_degraded_nor_unavailable() {
    let r = report(2, Vec::new());

    assert!(!r.is_degraded());
    assert!(!r.is_unavailable());
}

#[test]
fn a_pool_with_no_seat_at_all_reads_as_unavailable() {
    let r = report(0, Vec::new());

    assert!(r.is_unavailable());
    assert!(!r.is_degraded());
}
