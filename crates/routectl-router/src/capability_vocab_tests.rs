use std::time::Instant;

use super::*;

/// A minimal live negative carrying `vocab_version`.
fn row(vocab_version: Option<i64>, verdict: &str, capability: &str) -> CapabilityEventRow {
    CapabilityEventRow::new(
        9,
        Instant::now(),
        verdict.to_string(),
        Some("f1".to_string()),
        "live".to_string(),
        Some("self-identifying".to_string()),
        None,
        capability.to_string(),
        "lane".to_string(),
        String::new(),
        7,
        3,
    )
    .with_vocab_version(vocab_version)
}

/// A three-version ladder: v1 -> v2 renames a capability, v2 -> v3 renames a
/// verdict and retires another capability. Exercises the rename, chaining, and
/// retirement paths the identity production ladder cannot.
const TEST_STEPS: &[VocabStep] = &[
    VocabStep {
        from: 1,
        renames: &[Rename {
            field: TokenField::Capability,
            from: "old_search",
            to: Some("web_search"),
        }],
    },
    VocabStep {
        from: 2,
        renames: &[
            Rename {
                field: TokenField::Verdict,
                from: "busted",
                to: Some("broken"),
            },
            Rename {
                field: TokenField::Capability,
                from: "dropped_feature",
                to: None,
            },
        ],
    },
];
const TEST_CURRENT: i64 = 3;

#[derive(Debug)]
enum Expect {
    Mapped {
        verdict: &'static str,
        capability: &'static str,
    },
    Skipped(VocabSkip),
}

#[test]
fn map_through_maps_known_versions_and_skips_unknown_ones_whole() {
    let cases: &[(&str, Option<i64>, &str, &str, Expect)] = &[
        (
            "null version reads as v1 and chains every step",
            None,
            "busted",
            "old_search",
            Expect::Mapped {
                verdict: "broken",
                capability: "web_search",
            },
        ),
        (
            "explicit v1 is the same as null",
            Some(1),
            "broken",
            "old_search",
            Expect::Mapped {
                verdict: "broken",
                capability: "web_search",
            },
        ),
        (
            "v2 applies only the v2 -> v3 step",
            Some(2),
            "busted",
            "old_search",
            Expect::Mapped {
                verdict: "broken",
                capability: "old_search",
            },
        ),
        (
            "current version is untouched",
            Some(3),
            "busted",
            "old_search",
            Expect::Mapped {
                verdict: "busted",
                capability: "old_search",
            },
        ),
        (
            "an unrenamed token passes through",
            Some(1),
            "cleared",
            "thinking",
            Expect::Mapped {
                verdict: "cleared",
                capability: "thinking",
            },
        ),
        (
            "a version newer than current is unknown",
            Some(4),
            "broken",
            "web_search",
            Expect::Skipped(VocabSkip::UnknownVersion(4)),
        ),
        (
            "version zero is unknown",
            Some(0),
            "broken",
            "web_search",
            Expect::Skipped(VocabSkip::UnknownVersion(0)),
        ),
        (
            "a negative version is unknown",
            Some(-1),
            "broken",
            "web_search",
            Expect::Skipped(VocabSkip::UnknownVersion(-1)),
        ),
        (
            "a retired token skips the whole row even after an earlier rename",
            None,
            "busted",
            "dropped_feature",
            Expect::Skipped(VocabSkip::RetiredToken {
                field: TokenField::Capability,
                version: 2,
            }),
        ),
    ];

    for (name, version, verdict, capability, expect) in cases {
        let result = map_through(row(*version, verdict, capability), TEST_STEPS, TEST_CURRENT);
        match (expect, result) {
            (
                Expect::Mapped {
                    verdict: want_verdict,
                    capability: want_capability,
                },
                Ok(mapped),
            ) => {
                assert_eq!(mapped.verdict, *want_verdict, "{name}");
                assert_eq!(mapped.capability, *want_capability, "{name}");
                assert_eq!(mapped.vocab_version, Some(TEST_CURRENT), "{name}");
                assert_eq!(mapped.phase.as_deref(), Some("f1"), "{name}");
                assert_eq!(mapped.state_key, "lane", "{name}");
            }
            (Expect::Skipped(want), Err(got)) => assert_eq!(&got, want, "{name}"),
            (expect, got) => panic!("{name}: expected {expect:?}, got {got:?}"),
        }
    }
}

#[test]
fn a_gap_in_the_ladder_is_an_unknown_version() {
    let gapped = &[VocabStep {
        from: 2,
        renames: &[],
    }];

    let result = map_through(row(None, "broken", "web_search"), gapped, 3);

    assert_eq!(result.unwrap_err(), VocabSkip::UnknownVersion(1));
}

#[test]
fn the_production_ladder_maps_legacy_and_current_rows_and_rejects_others() {
    let cases: &[(&str, Option<i64>, bool)] = &[
        ("legacy null", None, true),
        ("legacy v1", Some(LEGACY_VOCAB_VERSION), true),
        ("current", Some(CURRENT_VOCAB_VERSION), true),
        ("future", Some(CURRENT_VOCAB_VERSION + 1), false),
        ("zero", Some(0), false),
    ];

    for (name, version, mapped) in cases {
        let result = map_to_current(row(*version, "broken", "web_search"));
        assert_eq!(result.is_ok(), *mapped, "{name}: {result:?}");
        if let Ok(row) = result {
            assert_eq!(row.capability, "web_search", "{name}");
            assert_eq!(row.vocab_version, Some(CURRENT_VOCAB_VERSION), "{name}");
        }
    }
}
