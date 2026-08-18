//! Tests for the cache-decision vocabulary.
//!
//! The spellings are asserted against INLINE LITERALS on purpose: this file
//! is the wire-spec pin for a frozen contract, so it must fail if a const's
//! value is edited. Everywhere else in the workspace consumes the consts.

use super::*;

#[test]
fn every_token_keeps_its_frozen_spelling() {
    // Arrange + Act + Assert: one literal per token, byte for byte. A
    // renamed token silently re-classifies every historical ledger row, so
    // this test exists to make that edit loud.
    assert_eq!(AUTO_EMITTED, "auto_emitted");
    assert_eq!(CALLER_SUPPLIED, "caller_supplied");
    assert_eq!(VOLATILE_VETOED, "volatile_vetoed");
    assert_eq!(AUTO_SKIPPED_GLOBAL_DISABLED, "auto_skipped:global_disabled");
    assert_eq!(
        AUTO_SKIPPED_PROVIDER_DISABLED,
        "auto_skipped:provider_disabled"
    );
    assert_eq!(AUTO_SKIPPED_NO_CAPABILITY, "auto_skipped:no_capability");
    assert_eq!(AUTO_SKIPPED_BREAKPOINT_CAP, "auto_skipped:breakpoint_cap");
    assert_eq!(
        AUTO_SKIPPED_NO_PLACEMENT_REGION,
        "auto_skipped:no_placement_region"
    );
    assert_eq!(
        AUTO_SKIPPED_K_BELOW_BREAK_EVEN,
        "auto_skipped:k_below_break_even"
    );
    assert_eq!(
        AUTO_SKIPPED_VALIDATION_ROLLED_BACK,
        "auto_skipped:validation_rolled_back"
    );
}

#[test]
fn the_token_list_holds_every_token_exactly_once() {
    // Arrange: the list is meant to be the whole closed contract.
    let mut seen = CACHE_DECISION_TOKENS.to_vec();
    let before = seen.len();

    // Act
    seen.sort_unstable();
    seen.dedup();

    // Assert
    assert_eq!(before, 10, "ten tokens in the vocabulary");
    assert_eq!(seen.len(), before, "no duplicate token in the list");
}

#[test]
fn only_the_skip_reasons_carry_the_auto_skipped_prefix() {
    // The prefix is load-bearing: it groups the variants where auto-emit ran
    // but declined, and operators filter logs on it.
    for token in [AUTO_EMITTED, CALLER_SUPPLIED, VOLATILE_VETOED] {
        assert!(
            !token.starts_with("auto_skipped:"),
            "{token} is not a skip reason",
        );
    }
    let skips = CACHE_DECISION_TOKENS
        .iter()
        .filter(|t| t.starts_with("auto_skipped:"))
        .count();
    assert_eq!(skips, 7, "seven decline reasons carry the prefix");
}

#[test]
fn recognizes_known_tokens_and_rejects_unknown_ones() {
    // Arrange + Act + Assert: readers depend on this to skip rather than
    // panic on a token written by a newer build.
    for token in CACHE_DECISION_TOKENS {
        assert!(is_known_cache_decision(token), "{token} must be known");
    }
    assert!(!is_known_cache_decision(""));
    assert!(!is_known_cache_decision("auto_skipped:"));
    assert!(!is_known_cache_decision("auto_emitted "));
    assert!(!is_known_cache_decision("AUTO_EMITTED"));
    assert!(!is_known_cache_decision("auto_skipped:not_a_real_reason"));
}
