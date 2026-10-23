// Ranked resolution of an effort refusal: the row of the level the refused
// attempt sent, and every case that names no row.
// An `include!`d FRAGMENT of `rejection_parser_tests.rs`.

mod ranked_resolution {
    //! Which ranked row an effort refusal names, given the level the egress
    //! body sent, with the levels it carries; and every reading that fails
    //! closed to no row.

    use routectl_core::UpstreamBody;
    use routectl_core::capability::SignalTier;

    use super::super::super::learn_engine::{EffortRank, SentEffort};
    use super::level_list::{HIGH, LOW, MAX, MEDIUM, XHIGH};
    use super::{
        ANTHROPIC, BEDROCK, Inferred, InvalidValue, OPENAI_COMPAT, OPENAI_RESPONSES, RejectionKind,
        RejectionValue, SelfIdentifying, UnsupportedField, anthropic_body, bedrock_body,
        captured_sent_effort, corpus_outcomes, fixture, openai_coded_body, outcome, outcome_sent,
        parse_rejection, rejections_dir,
    };

    /// What [`parse_rejection`] names: the row path, tier, kind and value.
    type Named = (
        &'static str,
        SignalTier,
        RejectionKind,
        Option<RejectionValue>,
    );

    fn sent(token: &str) -> SentEffort {
        SentEffort::Rank(EffortRank::of_token(token).expect("premise: a rank member"))
    }

    fn named(kind: &str, body: &str, sent: SentEffort) -> Option<Named> {
        let body = UpstreamBody::new(body);
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            kind,
            400,
            body.parsed(),
            sent,
        )
        .map(|named| (named.row.path, named.tier, named.kind, named.value))
    }

    fn supported_levels(echoed: &str, levels: &str) -> String {
        anthropic_body(&format!(
            "This model does not support effort level '{echoed}'. Supported levels: {levels}."
        ))
    }

    fn pydantic(list: &str) -> String {
        anthropic_body(&format!("output_config.effort: Input should be {list}"))
    }

    struct Row {
        name: &'static str,
        kind: &'static str,
        body: String,
        sent: SentEffort,
        expected: Option<Named>,
    }

    fn rows() -> Vec<Row> {
        vec![
            Row {
                name: "supported levels, the echo equal to the sent level",
                kind: ANTHROPIC,
                body: supported_levels("xhigh", "high, low, max, medium"),
                sent: sent("xhigh"),
                expected: Some((
                    "output_config.effort.xhigh",
                    SelfIdentifying,
                    InvalidValue,
                    Some(RejectionValue::Levels(HIGH | LOW | MAX | MEDIUM)),
                )),
            },
            Row {
                name: "pydantic list, which echoes nothing",
                kind: ANTHROPIC,
                body: pydantic("'low', 'medium' or 'high'"),
                sent: sent("xhigh"),
                expected: Some((
                    "output_config.effort.xhigh",
                    SelfIdentifying,
                    InvalidValue,
                    Some(RejectionValue::Levels(LOW | MEDIUM | HIGH)),
                )),
            },
            Row {
                name: "serde variant list keeps its inferred tier",
                kind: BEDROCK,
                body: bedrock_body(
                    "unknown variant `max`, expected one of `low`, `medium`, `high`, \
                     `Unhandled` at line 1 column 9",
                ),
                sent: sent("max"),
                expected: Some((
                    "output_config.effort.max",
                    Inferred,
                    InvalidValue,
                    Some(RejectionValue::Levels(LOW | MEDIUM | HIGH)),
                )),
            },
            Row {
                name: "a bare extra input refuses the field, with no levels",
                kind: BEDROCK,
                body: bedrock_body("output_config.effort: Extra inputs are not permitted"),
                sent: sent("high"),
                expected: Some((
                    "output_config.effort.high",
                    SelfIdentifying,
                    UnsupportedField,
                    None,
                )),
            },
            Row {
                name: "responses unsupported value",
                kind: OPENAI_RESPONSES,
                body: openai_coded_body(
                    "Unsupported value: 'minimal' is not supported with the 'm' model. \
                     Supported values are: 'low', 'medium', and 'high'.",
                    Some("reasoning.effort"),
                    "unsupported_value",
                ),
                sent: sent("minimal"),
                expected: Some((
                    "output_config.effort.minimal",
                    SelfIdentifying,
                    InvalidValue,
                    Some(RejectionValue::Levels(LOW | MEDIUM | HIGH)),
                )),
            },
            // The list leaves out the sent level, so only the echo check
            // refuses this one.
            Row {
                name: "echo xhigh with sent high",
                kind: ANTHROPIC,
                body: supported_levels("xhigh", "low, medium"),
                sent: sent("high"),
                expected: None,
            },
            // No echo to check, so only the sent-in-list check refuses it.
            Row {
                name: "sent high with high in the pydantic list",
                kind: ANTHROPIC,
                body: pydantic("'low', 'medium' or 'high'"),
                sent: sent("high"),
                expected: None,
            },
            Row {
                name: "sent high with high in the supported levels",
                kind: ANTHROPIC,
                body: supported_levels("high", "high, low"),
                sent: sent("high"),
                expected: None,
            },
            Row {
                name: "sent none",
                kind: ANTHROPIC,
                body: pydantic("'low', 'medium' or 'high'"),
                sent: SentEffort::OutOfRank,
                expected: None,
            },
            Row {
                name: "sent no effort",
                kind: ANTHROPIC,
                body: pydantic("'low', 'medium' or 'high'"),
                sent: SentEffort::Absent,
                expected: None,
            },
            Row {
                name: "no egress record",
                kind: ANTHROPIC,
                body: supported_levels("xhigh", "high, low, medium"),
                sent: SentEffort::Unknown,
                expected: None,
            },
            Row {
                name: "no egress record, field refused",
                kind: BEDROCK,
                body: bedrock_body("output_config.effort: Extra inputs are not permitted"),
                sent: SentEffort::Unknown,
                expected: None,
            },
            Row {
                name: "a list with no rank member left after filtering",
                kind: ANTHROPIC,
                body: supported_levels("high", "none"),
                sent: sent("high"),
                expected: None,
            },
            // The level segment is the egress's to name, never the envelope's.
            Row {
                name: "an envelope spelling a ranked row's own path",
                kind: ANTHROPIC,
                body: anthropic_body("output_config.effort.high: Extra inputs are not permitted"),
                sent: sent("high"),
                expected: None,
            },
            Row {
                name: "openai-compat carries no ranked row",
                kind: OPENAI_COMPAT,
                body: openai_coded_body(
                    "Unsupported parameter: 'output_config.effort' is not supported with \
                     this model.",
                    Some("output_config.effort"),
                    "unsupported_parameter",
                ),
                sent: sent("high"),
                expected: None,
            },
        ]
    }

    #[test]
    fn an_effort_refusal_names_the_sent_level_row_or_fails_closed() {
        for row in rows() {
            assert_eq!(
                named(row.kind, &row.body, row.sent),
                row.expected,
                "{}",
                row.name
            );
        }
    }

    /// Each effort fixture whose captured request sent a rank level names
    /// that level's row and carries the levels its envelope listed, the sent
    /// level read from the capture record.
    #[test]
    fn each_resolving_effort_fixture_carries_the_levels_it_listed() {
        for (kind, case, level, expected) in [
            (
                ANTHROPIC,
                "effort-xhigh-unsupported-opus-4-5",
                "xhigh",
                Some(RejectionValue::Levels(HIGH | LOW | MEDIUM)),
            ),
            (
                ANTHROPIC,
                "effort-xhigh-unsupported-sonnet-4-6",
                "xhigh",
                Some(RejectionValue::Levels(HIGH | LOW | MAX | MEDIUM)),
            ),
            (
                BEDROCK,
                "effort-invalid-value-invoke-opus-4-5-beta",
                "xhigh",
                Some(RejectionValue::Levels(LOW | MEDIUM | HIGH)),
            ),
            (BEDROCK, "effort-extra-input-invoke-opus-4-5", "xhigh", None),
            (
                BEDROCK,
                "effort-xhigh-unsupported-converse-sonnet-4-6",
                "xhigh",
                Some(RejectionValue::Levels(HIGH | LOW | MAX | MEDIUM)),
            ),
            (
                OPENAI_RESPONSES,
                "reasoning-effort-minimal-unsupported-codex",
                "minimal",
                Some(RejectionValue::Levels(LOW | MEDIUM | HIGH | XHIGH | MAX)),
            ),
        ] {
            let (_, body) = fixture(kind, case);
            let captured = captured_sent_effort(&format!("{kind}/{case}"));
            assert_eq!(captured, sent(level), "premise: {kind}/{case} sent {level}");

            let resolved = named(kind, &body, captured).map(|(path, .., value)| (path, value));

            let path = format!("output_config.effort.{level}");
            assert_eq!(
                resolved.map(|(row, value)| (row.to_string(), value)),
                Some((path, expected)),
                "{kind}/{case}"
            );
        }
    }

    /// No fixture outside the effort field reads the sent level: every one
    /// resolves exactly as with no egress record, whatever level was sent.
    #[test]
    fn only_effort_refusals_read_the_sent_level() {
        let corpus = corpus_outcomes();
        let mut effort_cases = 0;
        let mut other_cases = 0;
        for kind_dir in std::fs::read_dir(rejections_dir()).expect("rejections dir") {
            let kind_dir = kind_dir.expect("dir entry").path();
            let kind = kind_dir.file_name().unwrap().to_str().unwrap().to_string();
            for file in std::fs::read_dir(&kind_dir).expect("kind dir") {
                let case = file.expect("file entry").path();
                let case = case.file_stem().unwrap().to_str().unwrap().to_string();
                let (status, body) = fixture(&kind, &case);
                let unknown = outcome(&kind, status, &body);
                let changed = routectl_providers::effort::VALID_EFFORT_TOKENS
                    .iter()
                    .any(|&level| outcome_sent(&kind, status, &body, sent(level)) != unknown);
                let resolved = &corpus[&format!("{kind}/{case}")];
                let ranked = matches!(resolved, super::Outcome::Row(path, ..)
                    if path.starts_with("output_config.effort."));
                if changed {
                    effort_cases += 1;
                    assert!(
                        matches!(&unknown, super::Outcome::Unresolved(path, ..)
                            if path == super::EFFORT_PATH),
                        "{kind}/{case}: only an effort refusal reads the sent level"
                    );
                } else {
                    other_cases += 1;
                    assert!(
                        !ranked,
                        "{kind}/{case}: resolved ranked without the sent level"
                    );
                }
            }
        }
        assert!(
            effort_cases >= 6 && other_cases > 20,
            "premise: both kinds present"
        );
    }
}
