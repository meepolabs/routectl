//! Tests for the lane -> tag / lane -> scheme mappings in `mod.rs`.

use super::*;
use routectl_core::{BEDROCK_MANTLE, CODEX_OAUTH, OPENAI_APIKEY, scheme_of};

#[test]
fn lane_format_tag_covers_every_auth_kind() {
    assert_eq!(lane_format_tag(AuthKind::ChatgptOauth), CODEX_OAUTH);
    assert_eq!(lane_format_tag(AuthKind::ApiKey), OPENAI_APIKEY);
    assert_eq!(lane_format_tag(AuthKind::BedrockMantle), BEDROCK_MANTLE);
}

#[test]
fn lane_format_tag_is_distinct_per_lane() {
    let tags = [
        lane_format_tag(AuthKind::ChatgptOauth),
        lane_format_tag(AuthKind::ApiKey),
        lane_format_tag(AuthKind::BedrockMantle),
    ];
    for (i, a) in tags.iter().enumerate() {
        for b in &tags[i + 1..] {
            assert_ne!(a, b, "lane tags must stay one-to-one with lanes");
        }
    }
}

#[test]
fn lane_format_tag_never_emits_the_compatibility_tag() {
    for kind in [
        AuthKind::ChatgptOauth,
        AuthKind::ApiKey,
        AuthKind::BedrockMantle,
    ] {
        assert_ne!(lane_format_tag(kind), routectl_core::OPENAI_RESPONSES_V1);
    }
}

#[test]
fn lane_scheme_groups_both_first_party_lanes_together() {
    assert_eq!(lane_scheme(AuthKind::ChatgptOauth), ReplayScheme::Codex);
    assert_eq!(lane_scheme(AuthKind::ApiKey), ReplayScheme::Codex);
}

#[test]
fn lane_scheme_keeps_mantle_in_its_own_family() {
    assert_eq!(lane_scheme(AuthKind::BedrockMantle), ReplayScheme::Mantle);
}

#[test]
fn no_lane_maps_to_gray() {
    for kind in [
        AuthKind::ChatgptOauth,
        AuthKind::ApiKey,
        AuthKind::BedrockMantle,
    ] {
        assert_ne!(lane_scheme(kind), ReplayScheme::Gray);
    }
}

/// A lane's own emitted tag must resolve back to that lane's scheme,
/// otherwise an artifact would be stripped on replay to the very lane
/// that produced it.
#[test]
fn emitted_tag_round_trips_to_the_lane_scheme() {
    for kind in [
        AuthKind::ChatgptOauth,
        AuthKind::ApiKey,
        AuthKind::BedrockMantle,
    ] {
        assert_eq!(scheme_of(Some(lane_format_tag(kind))), lane_scheme(kind));
    }
}
