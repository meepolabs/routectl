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
        retire_all: false,
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
        retire_all: false,
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
        retire_all: false,
    }];

    let result = map_through(row(None, "broken", "web_search"), gapped, 3);

    assert_eq!(result.unwrap_err(), VocabSkip::UnknownVersion(1));
}

#[test]
fn a_retiring_step_skips_every_row_of_its_version_and_no_other() {
    let steps = &[
        VocabStep {
            from: 1,
            renames: &[],
            retire_all: true,
        },
        VocabStep {
            from: 2,
            renames: &[],
            retire_all: false,
        },
    ];
    let cases: &[(&str, Option<i64>, Result<(), VocabSkip>)] = &[
        ("null reads as v1", None, Err(VocabSkip::RetiredVersion(1))),
        ("explicit v1", Some(1), Err(VocabSkip::RetiredVersion(1))),
        ("v2 starts past the retiring step", Some(2), Ok(())),
        ("current", Some(3), Ok(())),
    ];

    for (name, version, want) in cases {
        let got = map_through(row(*version, "broken", "web_search"), steps, 3).map(|_| ());
        assert_eq!(&got, want, "{name}");
    }
}

/// Production: a v1 row keyed by a model nickname has no lane in the current
/// grammar, so it is skipped whole and the caller counts it; a v2 row keyed
/// `provider#upstream` maps through unchanged.
#[test]
fn the_production_ladder_skips_nickname_rows_and_replays_lane_rows() {
    let mut nickname_row = row(None, "broken", "web_search");
    nickname_row.state_key = "opus-nick".to_string();
    let mut lane_row = row(Some(CURRENT_VOCAB_VERSION), "broken", "web_search");
    lane_row.state_key = "anthropic#claude-opus-4".to_string();

    let skipped = map_to_current(nickname_row);
    let replayed = map_to_current(lane_row).expect("a current lane row maps");

    assert_eq!(
        skipped,
        Err(VocabSkip::RetiredVersion(LEGACY_VOCAB_VERSION))
    );
    assert_eq!(replayed.state_key, "anthropic#claude-opus-4");
    assert_eq!(replayed.capability, "web_search");
    assert_eq!(replayed.vocab_version, Some(CURRENT_VOCAB_VERSION));
}

#[test]
fn the_production_ladder_rejects_versions_outside_its_range() {
    let cases: &[(&str, Option<i64>, bool)] = &[
        ("legacy null", None, false),
        ("legacy v1", Some(LEGACY_VOCAB_VERSION), false),
        ("current", Some(CURRENT_VOCAB_VERSION), true),
        ("future", Some(CURRENT_VOCAB_VERSION + 1), false),
        ("zero", Some(0), false),
    ];

    for (name, version, mapped) in cases {
        let result = map_to_current(row(*version, "broken", "web_search"));
        assert_eq!(result.is_ok(), *mapped, "{name}: {result:?}");
    }
}

#[test]
fn the_legacy_vocab_step_retires_every_row() {
    let legacy_step = VOCAB_STEPS
        .iter()
        .find(|step| step.from == LEGACY_VOCAB_VERSION)
        .expect("the production ladder has a step out of the legacy version");

    assert!(
        legacy_step.retire_all,
        "the legacy capability-row data step in the usage ledger deletes \
         pre-boundary NULL-vocab rows because no legacy row can map forward; \
         revisit that data step before making the legacy vocabulary mappable"
    );
}
