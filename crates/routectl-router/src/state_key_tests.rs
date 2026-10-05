use std::collections::HashSet;

use super::*;

#[test]
fn a_lane_round_trips_through_its_serialized_form() {
    let cases: &[(&str, &str, &str)] = &[
        ("plain", "anthropic", "claude-x"),
        (
            "upstream with a hash",
            "bedrock",
            "arn:aws:bedrock#model#v2",
        ),
        (
            "upstream with slashes",
            "openrouter",
            "vendor/model/variant",
        ),
        ("upstream with hash and slash", "p", "a/b#c/d#"),
        ("upstream led by a hash", "p", "#lead"),
        ("empty upstream", "p", ""),
    ];

    for (name, provider_entry, upstream) in cases {
        let key = StateKey::new(provider_entry, upstream).expect(name);

        let parsed = StateKey::parse(key.as_lane_key()).expect(name);

        assert_eq!(parsed, key, "{name}");
        assert_eq!(parsed.provider_entry(), *provider_entry, "{name}");
        assert_eq!(parsed.upstream(), *upstream, "{name}");
    }
}

#[test]
fn distinct_provider_upstream_pairs_never_serialize_equal() {
    let pairs: &[(&str, &str)] = &[
        ("a", "b#c"),
        ("a", "b"),
        ("a", "#b"),
        ("ab", "c"),
        ("a", "bc"),
        ("a", "b/c"),
        ("a/b", "c"),
        ("p", ""),
        ("", "p"),
    ];

    let serialized: Vec<String> = pairs
        .iter()
        .map(|(p, u)| StateKey::new(p, u).expect("no separator in the provider half"))
        .map(|key| key.as_lane_key().to_string())
        .collect();

    let unique: HashSet<&String> = serialized.iter().collect();
    assert_eq!(unique.len(), pairs.len(), "{serialized:?}");
}

#[test]
fn a_provider_entry_carrying_the_separator_mints_no_lane() {
    assert!(StateKey::new("pro#vider", "model").is_none());
}

#[test]
fn a_string_without_a_separator_is_not_a_lane() {
    assert!(StateKey::parse("nickname").is_none());
}

#[test]
fn the_log_view_and_debug_render_sanitized() {
    let key = StateKey::new("p", "up\u{1b}[31mstream").expect("valid");

    assert!(!key.for_log().contains('\u{1b}'));
    assert!(!format!("{key:?}").contains('\u{1b}'));
    assert!(key.as_lane_key().contains('\u{1b}'));
}
