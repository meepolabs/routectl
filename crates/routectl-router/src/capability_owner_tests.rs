use super::*;

fn providers(entries: &[(&str, ProviderEntry)]) -> BTreeMap<String, ProviderEntry> {
    entries
        .iter()
        .map(|(name, entry)| ((*name).to_string(), entry.clone()))
        .collect()
}

fn compat() -> ProviderEntry {
    ProviderEntry::openai_compat("https://compat.example.test/v1", "literal:k")
}

fn anthropic() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k")
}

/// `(name, configured entries, recorded kind, expected)`.
type OwnerCase = (
    &'static str,
    Vec<(&'static str, ProviderEntry)>,
    &'static str,
    OwnerDecision,
);

#[test]
fn owner_decision_compares_the_lane_entry_and_the_recorded_kind() {
    let lane = StateKey::new("p", "model-x").expect("a separator-free entry name");
    let cases: &[OwnerCase] = &[
        (
            "same entry, same kind",
            vec![("p", compat())],
            "openai-compat",
            OwnerDecision::Owned,
        ),
        (
            "entry removed",
            vec![("other", compat())],
            "openai-compat",
            OwnerDecision::EntryRemoved,
        ),
        (
            "entry kind flipped",
            vec![("p", anthropic())],
            "openai-compat",
            OwnerDecision::KindChanged,
        ),
        (
            "kind not recorded",
            vec![("p", compat())],
            "",
            OwnerDecision::KindUnrecorded,
        ),
        (
            "removed wins over an unrecorded kind",
            vec![],
            "",
            OwnerDecision::EntryRemoved,
        ),
    ];
    for (name, entries, recorded, expected) in cases {
        assert_eq!(
            owner_decision(&lane, recorded, &providers(entries)),
            *expected,
            "{name}",
        );
    }
}

#[test]
fn a_same_kind_repoint_keeps_ownership() {
    let lane = StateKey::new("p", "model-x").expect("a separator-free entry name");
    let repointed = ProviderEntry::openai_compat("https://elsewhere.example.test/v1", "literal:k");

    let decision = owner_decision(&lane, "openai-compat", &providers(&[("p", repointed)]));

    assert_eq!(decision, OwnerDecision::Owned);
}

#[test]
fn only_owned_has_no_skip_reason() {
    assert_eq!(OwnerDecision::Owned.skip_reason(), None);
    for decision in [
        OwnerDecision::EntryRemoved,
        OwnerDecision::KindChanged,
        OwnerDecision::KindUnrecorded,
    ] {
        assert!(decision.skip_reason().is_some(), "{decision:?}");
    }
}

#[test]
fn recorded_kind_decision_matches_owner_decision_for_a_present_entry() {
    let lane = StateKey::new("p", "model-x").expect("a separator-free entry name");
    for recorded in ["openai-compat", "anthropic-api", ""] {
        for (current, entry) in [("openai-compat", compat()), ("anthropic-api", anthropic())] {
            assert_eq!(
                recorded_kind_decision(recorded, current),
                owner_decision(&lane, recorded, &providers(&[("p", entry)])),
                "recorded={recorded:?} current={current:?}",
            );
        }
    }
}

#[test]
fn recorded_kind_decision_compares_the_writer_and_reader_kinds() {
    let cases: &[(&str, &str, &str, OwnerDecision)] = &[
        (
            "same kind",
            "openai-compat",
            "openai-compat",
            OwnerDecision::Owned,
        ),
        (
            "kind flipped",
            "openai-compat",
            "anthropic-api",
            OwnerDecision::KindChanged,
        ),
        (
            "reader has no kind",
            "openai-compat",
            "",
            OwnerDecision::KindChanged,
        ),
        (
            "writer had no kind",
            "",
            "anthropic-api",
            OwnerDecision::KindUnrecorded,
        ),
        ("neither has a kind", "", "", OwnerDecision::Owned),
    ];
    for (name, recorded, current, expected) in cases {
        assert_eq!(
            recorded_kind_decision(recorded, current),
            *expected,
            "{name}"
        );
    }
}
