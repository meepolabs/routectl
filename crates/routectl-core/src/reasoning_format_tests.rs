//! Tests for [`super`] -- see `reasoning_format.rs`.

use super::*;

#[test]
fn compatibility_tag_value_is_pinned() {
    assert_eq!(OPENAI_RESPONSES_V1, "openai-responses-v1");
    assert_eq!(CODEX_OAUTH, "codex-oauth");
    assert_eq!(OPENAI_APIKEY, "openai-apikey");
    assert_eq!(BEDROCK_MANTLE, "bedrock-mantle");
}

#[test]
fn is_responses_family_accepts_every_recognized_tag() {
    assert!(is_responses_family(Some(OPENAI_RESPONSES_V1)));
    assert!(is_responses_family(Some(CODEX_OAUTH)));
    assert!(is_responses_family(Some(OPENAI_APIKEY)));
    assert!(is_responses_family(Some(BEDROCK_MANTLE)));
}

#[test]
fn is_responses_family_rejects_absent_and_foreign_tags() {
    assert!(!is_responses_family(None));
    assert!(!is_responses_family(Some("anthropic-claude-v1")));
    assert!(!is_responses_family(Some("deepseek-v1")));
    assert!(!is_responses_family(Some("")));
    assert!(!is_responses_family(Some("OPENAI-RESPONSES-V1")));
}

#[test]
fn scheme_of_maps_both_openai_lanes_to_codex() {
    assert_eq!(scheme_of(Some(CODEX_OAUTH)), ReplayScheme::Codex);
    assert_eq!(scheme_of(Some(OPENAI_APIKEY)), ReplayScheme::Codex);
}

#[test]
fn scheme_of_maps_mantle_to_its_own_family() {
    assert_eq!(scheme_of(Some(BEDROCK_MANTLE)), ReplayScheme::Mantle);
}

#[test]
fn scheme_of_maps_compatibility_tag_and_unknowns_to_gray() {
    assert_eq!(scheme_of(Some(OPENAI_RESPONSES_V1)), ReplayScheme::Gray);
    assert_eq!(scheme_of(None), ReplayScheme::Gray);
    assert_eq!(scheme_of(Some("anthropic-claude-v1")), ReplayScheme::Gray);
    assert_eq!(
        scheme_of(Some("responses-v9-from-the-future")),
        ReplayScheme::Gray
    );
}

#[test]
fn is_replayable_carries_within_a_proven_family() {
    assert_eq!(
        is_replayable(ReplayScheme::Codex, ReplayScheme::Codex),
        Replayability::Carry
    );
    assert_eq!(
        is_replayable(ReplayScheme::Mantle, ReplayScheme::Mantle),
        Replayability::Carry
    );
}

#[test]
fn is_replayable_strips_across_proven_families() {
    assert_eq!(
        is_replayable(ReplayScheme::Codex, ReplayScheme::Mantle),
        Replayability::Strip
    );
    assert_eq!(
        is_replayable(ReplayScheme::Mantle, ReplayScheme::Codex),
        Replayability::Strip
    );
}

#[test]
fn is_replayable_stays_gray_whenever_either_side_is_gray() {
    for other in [
        ReplayScheme::Codex,
        ReplayScheme::Mantle,
        ReplayScheme::Gray,
    ] {
        assert_eq!(
            is_replayable(ReplayScheme::Gray, other),
            Replayability::Gray
        );
        assert_eq!(
            is_replayable(other, ReplayScheme::Gray),
            Replayability::Gray
        );
    }
}

#[test]
fn reasoning_format_vocabulary_reexported_from_crate_root() {
    assert!(crate::is_responses_family(Some(crate::CODEX_OAUTH)));
    assert_eq!(
        crate::scheme_of(Some(crate::BEDROCK_MANTLE)),
        crate::ReplayScheme::Mantle
    );
    assert_eq!(
        crate::is_replayable(crate::ReplayScheme::Codex, crate::ReplayScheme::Codex),
        crate::Replayability::Carry
    );
    assert_eq!(crate::OPENAI_RESPONSES_V1, "openai-responses-v1");
    assert_eq!(crate::OPENAI_APIKEY, "openai-apikey");
}
