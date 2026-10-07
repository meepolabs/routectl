use std::path::Path;

use routectl_core::capability::{REASONING_REPLAY, normalize_capability_key};
use routectl_core::{ChatRequest, MAX_SAFE_TOKEN_LEN};

use super::*;
use crate::capability_strip::{
    CapabilityAction, Outcome, RequestInterceptor, StripContext, StripInterceptor, StripKind,
    action_for, is_strippable, strippable_keys,
};
use crate::field_capability::{capability_key_is_catalog_scoped, capability_key_is_field_verdict};
use crate::key_literal_scan;

/// A real dated `anthropic_beta` flag, the shape an upstream rejection names.
const DATED_FLAG: &str = "context-1m-2025-08-07";

fn dated_key() -> String {
    beta_capability_key(DATED_FLAG).expect("a dated flag is accepted")
}

// --- namespace ownership ---

/// The prefix is permanent. The expectation is an explicit byte array so an
/// edit to the const cannot drag the assertion along, and so this file does
/// not become the second prefix literal the scan below forbids.
#[test]
fn the_prefix_is_exactly_the_five_ascii_bytes_of_the_permanent_namespace() {
    // b, e, t, a, colon.
    let permanent: [u8; 5] = [0x62, 0x65, 0x74, 0x61, 0x3a];

    assert_eq!(BETA_CAPABILITY_PREFIX.as_bytes(), permanent.as_slice());
    assert_eq!(
        &dated_key().as_bytes()[..permanent.len()],
        permanent.as_slice()
    );
}

/// A second compiled spelling of the prefix anywhere under `crates/` could
/// drift from this module's and re-partition persisted keys. Same lexer-backed,
/// fail-closed scan the field namespace uses, with this namespace's predicate.
#[test]
fn the_beta_prefix_literal_occurs_exactly_once_under_crates() {
    // Arrange
    let crates_dir = key_literal_scan::workspace_crates_dir();
    let sources = key_literal_scan::rust_sources_under(&crates_dir);
    assert!(
        sources.len() > 100,
        "the scan must actually walk the workspace; found {} sources under {}",
        sources.len(),
        crates_dir.display()
    );

    // Act
    let carriers: Vec<(std::path::PathBuf, usize)> = sources
        .into_iter()
        .filter_map(|path| {
            let count = count_beta_literals_in(&path);
            (count > 0).then_some((path, count))
        })
        .collect();

    // Assert
    let owner_occurrences: usize = carriers
        .iter()
        .filter(|(path, _)| path.ends_with("routectl-router/src/beta_capability.rs"))
        .map(|(_, count)| *count)
        .sum();
    assert_eq!(
        owner_occurrences, 1,
        "the owning module must carry the prefix literal exactly once; carriers: {carriers:?}"
    );
    let total: usize = carriers.iter().map(|(_, count)| *count).sum();
    assert_eq!(
        total, 1,
        "the prefix literal must occur exactly once under crates/; carriers: {carriers:?}"
    );
}

/// The positive control for the uniqueness scan: the predicate recognizes a
/// bare prefix and a full key in any literal spelling, and refuses prose that
/// merely opens with the prefix. Fixtures are built from the const so this
/// file is never a carrier.
#[test]
fn the_scan_counts_beta_keys_and_ignores_prose_with_the_same_opening() {
    let p = BETA_CAPABILITY_PREFIX;
    let escaped = p.replace(':', "\\u{3a}");
    for source in [
        format!("const P: &str = \"{p}\";"),
        format!("const P: &str = \"{p}{DATED_FLAG}\";"),
        format!("const P: &str = r#\"{p}x\"#;"),
        format!("const P: &str = \"{escaped}x\";"),
    ] {
        assert_eq!(
            key_literal_scan::count_key_literals_in_source(&source, is_beta_key),
            1,
            "{source}"
        );
    }
    for source in [
        format!("const P: &str = \"{p} not a flag\";"),
        format!("const P: &str = \"{p}a.b\";"),
        format!("///{p}{DATED_FLAG}\nstruct S;"),
        "fn f() { let beta:u8 = 0; }".to_string(),
    ] {
        assert_eq!(
            key_literal_scan::count_key_literals_in_source(&source, is_beta_key),
            0,
            "{source}"
        );
    }
}

// --- grammar ---

/// Accept control for the reject table: a dated flag, a short flag, and a
/// flag exactly at the shared token cap.
#[test]
fn dated_and_at_cap_flags_are_accepted() {
    let at_cap = "a".repeat(MAX_SAFE_TOKEN_LEN);
    for flag in [
        DATED_FLAG,
        "interleaved-thinking-2025-05-14",
        "x",
        at_cap.as_str(),
    ] {
        assert_eq!(
            beta_capability_key(flag),
            Some(format!("{BETA_CAPABILITY_PREFIX}{flag}")),
            "{flag}"
        );
    }
}

#[test]
fn malformed_flags_are_rejected() {
    let over_cap = "a".repeat(MAX_SAFE_TOKEN_LEN + 1);
    let key_passed_back = dated_key();
    for (label, flag) in [
        ("a dot", "context-1m.2025-08-07"),
        ("a leading dot", ".context-1m"),
        ("a colon", "context:1m"),
        ("empty", ""),
        ("over the cap", over_cap.as_str()),
        ("a space", "context 1m"),
        ("a tab", "context-1m\t"),
        ("a newline", "context-1m\n"),
        ("non-ASCII", "context-\u{e9}"),
        ("a zero-width space", "context\u{200b}-1m"),
        ("a key passed back in", key_passed_back.as_str()),
        ("the bare prefix", BETA_CAPABILITY_PREFIX),
    ] {
        assert_eq!(beta_capability_key(flag), None, "{label}: {flag:?}");
    }
}

#[test]
fn a_key_round_trips_to_its_flag() {
    let key = dated_key();

    assert_eq!(beta_flag_of(&key), Some(DATED_FLAG));
    assert!(capability_key_is_beta(&key));
}

#[test]
fn keys_outside_the_namespace_are_not_beta() {
    for key in [
        "web_search",
        "context_management",
        "reasoning_replay:codex",
        "betax",
        "",
    ] {
        assert_eq!(beta_flag_of(key), None, "{key:?}");
        assert!(!capability_key_is_beta(key), "{key:?}");
    }
}

// --- normalization and scope ---

/// The registry stores keys through the shared normalizer. A dot-free flag
/// passes through unchanged on Bedrock (whose branch keeps only the first dot
/// segment) and on a pass-through lane.
#[test]
fn the_normalizer_leaves_a_beta_key_unchanged() {
    let key = dated_key();

    assert_eq!(normalize_capability_key(&key, "bedrock"), key);
    assert_eq!(normalize_capability_key(&key, "anthropic-api"), key);
}

#[test]
fn beta_keys_are_catalog_scoped_and_not_field_verdicts() {
    let key = dated_key();

    assert!(capability_key_is_catalog_scoped(&key));
    assert!(!capability_key_is_field_verdict(&key));
}

// --- strip policy ---

#[test]
fn every_beta_key_strips_as_a_beta_flag() {
    for flag in [DATED_FLAG, "x"] {
        let key = beta_capability_key(flag).expect("accepted");

        assert_eq!(
            action_for(&key),
            CapabilityAction::Strip(StripKind::BetaFlag),
            "{key}"
        );
        assert!(!is_strippable(&key), "{key}");
    }
}

/// The beta arm must not widen the enumerated droppable table, which is the
/// operator-facing `[capability] essential` accept set.
#[test]
fn strippable_keys_are_unchanged_by_the_beta_namespace() {
    let enumerated: Vec<&str> = strippable_keys().collect();

    assert_eq!(
        enumerated,
        ["advisor", "context_management", REASONING_REPLAY]
    );
    assert!(enumerated.iter().all(|key| !capability_key_is_beta(key)));
}

/// The provider withholds a beta flag at egress, so the interceptor has no
/// transform for it: it reports Unchanged in both modes, leaves the request
/// untouched even when the flag is present, and never reaches the missing-plan
/// debug assertion.
#[test]
fn the_interceptor_leaves_a_beta_key_unchanged_without_panicking() {
    // Arrange
    let key = dated_key();
    let mut req = ChatRequest {
        anthropic_beta: vec![DATED_FLAG.to_string()],
        ..Default::default()
    };
    let before = serde_json::to_value(&req).expect("serializes");
    let ctx = |strict| StripContext {
        keys: vec![key.clone()],
        strict,
    };

    // Act
    let lenient = StripInterceptor.apply(&mut req, &ctx(false));
    let strict = StripInterceptor.apply(&mut req, &ctx(true));

    // Assert
    assert!(matches!(lenient, Outcome::Unchanged), "{lenient:?}");
    assert!(matches!(strict, Outcome::Unchanged), "{strict:?}");
    assert_eq!(serde_json::to_value(&req).expect("serializes"), before);
}

// --- scan adapters ---

fn count_beta_literals_in(path: &Path) -> usize {
    key_literal_scan::count_key_literals_in(path, is_beta_key)
}

/// True when `value` is the bare prefix or a key the production grammar could
/// mint. The production predicate is the authority so this test cannot carry
/// a drifting copy of the grammar.
fn is_beta_key(value: &str) -> bool {
    value
        .strip_prefix(BETA_CAPABILITY_PREFIX)
        .is_some_and(|flag| flag.is_empty() || is_beta_flag(flag))
}
