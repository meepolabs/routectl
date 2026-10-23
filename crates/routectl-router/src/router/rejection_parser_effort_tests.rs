// The effort value-list reader, the effort refusal templates and the
// Responses effort refusals.
// An `include!`d FRAGMENT of `rejection_parser_tests.rs`.

mod level_list {
    //! The value-list reader over each captured effort list grammar, and the
    //! rank mask it feeds.

    use serde_json::Value;

    use super::super::effort::{ListMembers, levels_mask, list_members};

    const EFFORT_CAPTURE: &str = include_str!("../../tests/fixtures/effort_capture.json");

    pub(super) const MINIMAL: u8 = 1 << 0;
    pub(super) const LOW: u8 = 1 << 1;
    pub(super) const MEDIUM: u8 = 1 << 2;
    pub(super) const HIGH: u8 = 1 << 3;
    pub(super) const XHIGH: u8 = 1 << 4;
    pub(super) const MAX: u8 = 1 << 5;

    /// The captured upstream message of `case` in the effort capture record.
    fn captured_message(case: &str) -> String {
        let capture: Value = serde_json::from_str(EFFORT_CAPTURE).unwrap();
        capture["cases"][case]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("no captured message for {case}"))
            .to_string()
    }

    /// The members the reader takes from the text after `lead_in` in the
    /// captured message of `case`.
    fn listed(case: &str, lead_in: &str, members: ListMembers) -> Vec<String> {
        let message = captured_message(case);
        let (_, list) = message
            .split_once(lead_in)
            .unwrap_or_else(|| panic!("{case} has no {lead_in:?}"));
        list_members(list, members)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn each_captured_list_grammar_reads_to_its_rank_mask() {
        let rows: [(&str, &str, ListMembers, u8); 8] = [
            (
                "bedrock/effort-unknown-variant-invoke-opus-4-8",
                "expected one of ",
                ListMembers::Quoted,
                LOW | MEDIUM | HIGH | XHIGH | MAX,
            ),
            (
                "bedrock/effort-unknown-variant-converse-opus-4-8",
                "expected one of ",
                ListMembers::Quoted,
                LOW | MEDIUM | HIGH | XHIGH | MAX,
            ),
            (
                "openai-responses/reasoning-effort-invalid-value-codex-gpt-5-6",
                "Supported values are: ",
                ListMembers::Quoted,
                MINIMAL | LOW | MEDIUM | HIGH | XHIGH | MAX,
            ),
            (
                "openai-responses/reasoning-effort-minimal-unsupported-codex",
                "Supported values are: ",
                ListMembers::Quoted,
                LOW | MEDIUM | HIGH | XHIGH | MAX,
            ),
            (
                "anthropic-api/effort-xhigh-unsupported-sonnet-4-6",
                "Supported levels: ",
                ListMembers::Bare,
                LOW | MEDIUM | HIGH | MAX,
            ),
            (
                "anthropic-api/effort-xhigh-unsupported-opus-4-5",
                "Supported levels: ",
                ListMembers::Bare,
                LOW | MEDIUM | HIGH,
            ),
            (
                "bedrock/effort-invalid-value-invoke-opus-4-5-beta",
                "Input should be ",
                ListMembers::Quoted,
                LOW | MEDIUM | HIGH,
            ),
            (
                "anthropic-api/effort-invalid-value-opus-4-8",
                "Input should be ",
                ListMembers::Quoted,
                LOW | MEDIUM | HIGH | XHIGH | MAX,
            ),
        ];
        for (case, lead_in, members, expected) in rows {
            let tokens = listed(case, lead_in, members);
            let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
            assert_eq!(levels_mask(&tokens), expected, "{case}: read {tokens:?}");
        }
    }

    #[test]
    fn serde_list_keeps_its_catch_all_variant_and_ends_before_the_position_tail() {
        assert_eq!(
            listed(
                "bedrock/effort-unknown-variant-invoke-opus-4-8",
                "expected one of ",
                ListMembers::Quoted,
            ),
            ["low", "medium", "high", "xhigh", "max", "Unhandled"],
        );
    }

    #[test]
    fn bare_list_ends_at_its_closing_period() {
        assert_eq!(
            listed(
                "anthropic-api/effort-xhigh-unsupported-sonnet-4-6",
                "Supported levels: ",
                ListMembers::Bare,
            ),
            ["high", "low", "max", "medium"],
        );
    }

    #[test]
    fn out_of_rank_members_set_no_bit() {
        let rows: [(&str, &[&str], u8); 4] = [
            ("in rank beside none", &["none", "low", "high"], LOW | HIGH),
            ("catch-all variant", &["max", "Unhandled"], MAX),
            ("only out of rank", &["none", "Unhandled", "bogus"], 0),
            ("empty", &[], 0),
        ];
        for (name, tokens, expected) in rows {
            assert_eq!(levels_mask(tokens), expected, "{name}");
        }
    }

    #[test]
    fn a_separator_with_no_member_after_it_names_nothing() {
        let rows: [(&str, ListMembers); 4] = [
            ("high, low, and ", ListMembers::Bare),
            ("high, low, and .", ListMembers::Bare),
            ("`low`, `high", ListMembers::Quoted),
            ("'low', and 'hi\\gh'.", ListMembers::Quoted),
        ];
        for (text, members) in rows {
            assert!(list_members(text, members).is_empty(), "{text:?}");
        }
    }
}

mod effort_templates {
    //! The effort refusal templates: which committed effort capture localizes
    //! the effort field, at what tier, with what echoed level and level mask,
    //! and which wording near them localizes nothing.

    use std::collections::BTreeMap;

    use routectl_core::UpstreamBody;
    use routectl_core::capability::SignalTier;

    use super::level_list::{HIGH, LOW, MAX, MEDIUM, XHIGH};
    use super::{
        ANTHROPIC, BEDROCK, CONVERSE_WRAPPER, EFFORT_PATH, Inferred, InvalidValue, Outcome,
        RejectionKind, SelfIdentifying, THINKING_DISABLED_PATH, UnsupportedField, anthropic_body,
        bedrock_body, fixture, localize, outcome, rejections_dir,
    };

    /// The sonnet-4-6 `Supported levels` refusal, as captured.
    const SUPPORTED_LEVELS: &str = "This model does not support effort level 'xhigh'. Supported levels: high, low, max, medium.";
    /// The Invoke serde unknown-variant refusal, as captured.
    const UNKNOWN_VARIANT: &str = "unknown variant `bogus`, expected one of `low`, `medium`, \
         `high`, `xhigh`, `max`, `Unhandled` at line 1 column 94";

    /// What `localize` reads from one effort capture: the canonical path,
    /// tier, kind, echoed level and level mask.
    type Reading = (String, SignalTier, RejectionKind, Option<String>, u8);

    fn reading(kind: &str, status: u16, body: &str) -> Option<Reading> {
        let body = UpstreamBody::new(body);
        let localized = localize(kind, status, body.parsed()?)?;
        Some((
            localized.segments.join("."),
            localized.tier,
            localized.kind,
            localized.echoed.map(str::to_owned),
            localized.levels,
        ))
    }

    fn effort(
        tier: SignalTier,
        kind: RejectionKind,
        echoed: Option<&str>,
        levels: u8,
    ) -> Option<Reading> {
        Some((
            EFFORT_PATH.to_string(),
            tier,
            kind,
            echoed.map(str::to_owned),
            levels,
        ))
    }

    /// Every committed `effort-*` capture under anthropic-api and bedrock,
    /// keyed `<kind>/<case>`, read on the lane its directory names.
    fn effort_corpus_readings() -> BTreeMap<String, Option<Reading>> {
        let mut readings = BTreeMap::new();
        for kind in [ANTHROPIC, BEDROCK] {
            for file in std::fs::read_dir(rejections_dir().join(kind)).expect("kind dir") {
                let path = file.expect("file entry").path();
                let case = path.file_stem().unwrap().to_str().unwrap().to_string();
                if !case.starts_with("effort-") {
                    continue;
                }
                let (status, body) = fixture(kind, &case);
                readings.insert(format!("{kind}/{case}"), reading(kind, status, &body));
            }
        }
        readings
    }

    #[test]
    fn every_effort_capture_reads_to_its_tier_echo_and_level_mask() {
        let five = LOW | MEDIUM | HIGH | XHIGH | MAX;
        let expected: BTreeMap<String, Option<Reading>> = [
            // The accepted set is only inside `metadata.raw`.
            ("anthropic-api/effort-invalid-value-openrouter", None),
            (
                "anthropic-api/effort-invalid-value-opus-4-8",
                effort(SelfIdentifying, InvalidValue, None, five),
            ),
            ("anthropic-api/effort-parameter-unsupported-haiku-4-5", None),
            (
                "anthropic-api/effort-xhigh-thinking-disabled-opus-5-5",
                Some((
                    THINKING_DISABLED_PATH.to_string(),
                    SelfIdentifying,
                    UnsupportedField,
                    None,
                    0,
                )),
            ),
            (
                "anthropic-api/effort-xhigh-unsupported-opus-4-5",
                effort(
                    SelfIdentifying,
                    InvalidValue,
                    Some("xhigh"),
                    LOW | MEDIUM | HIGH,
                ),
            ),
            (
                "anthropic-api/effort-xhigh-unsupported-sonnet-4-6",
                effort(
                    SelfIdentifying,
                    InvalidValue,
                    Some("xhigh"),
                    LOW | MEDIUM | HIGH | MAX,
                ),
            ),
            // Refuses the field itself and lists nothing.
            (
                "bedrock/effort-extra-input-invoke-opus-4-5",
                effort(SelfIdentifying, UnsupportedField, None, 0),
            ),
            (
                "bedrock/effort-invalid-value-invoke-opus-4-5-beta",
                effort(SelfIdentifying, InvalidValue, None, LOW | MEDIUM | HIGH),
            ),
            // The catch-all `Unhandled` variant sets no bit.
            (
                "bedrock/effort-unknown-variant-converse-opus-4-8",
                effort(Inferred, InvalidValue, Some("bogus"), five),
            ),
            (
                "bedrock/effort-unknown-variant-invoke-opus-4-8",
                effort(Inferred, InvalidValue, Some("bogus"), five),
            ),
            (
                "bedrock/effort-xhigh-unsupported-converse-sonnet-4-6",
                effort(
                    SelfIdentifying,
                    InvalidValue,
                    Some("xhigh"),
                    LOW | MEDIUM | HIGH | MAX,
                ),
            ),
            (
                "bedrock/effort-xhigh-unsupported-invoke-sonnet-4-6",
                effort(
                    SelfIdentifying,
                    InvalidValue,
                    Some("xhigh"),
                    LOW | MEDIUM | HIGH | MAX,
                ),
            ),
        ]
        .into_iter()
        .map(|(case, reading)| (case.to_string(), reading))
        .collect();

        assert_eq!(effort_corpus_readings(), expected);
    }

    /// The `Supported levels` template on every lane that sends it: bare on
    /// anthropic-api and Bedrock Invoke, wrapped on Converse.
    fn on_each_lane(message: &str) -> [(String, &'static str, String); 3] {
        [
            (
                "anthropic-api".to_string(),
                ANTHROPIC,
                anthropic_body(message),
            ),
            ("bedrock invoke".to_string(), BEDROCK, bedrock_body(message)),
            (
                "bedrock converse".to_string(),
                BEDROCK,
                bedrock_body(&format!("{CONVERSE_WRAPPER}{message}")),
            ),
        ]
    }

    fn swap(from: &str, to: &str) -> String {
        assert!(
            SUPPORTED_LEVELS.contains(from),
            "premise: {from:?} is in the template"
        );
        SUPPORTED_LEVELS.replace(from, to)
    }

    #[test]
    fn supported_levels_near_misses_localize_nothing() {
        for (lane, kind, body) in on_each_lane(SUPPORTED_LEVELS) {
            assert_eq!(
                outcome(kind, 400, &body),
                super::unresolved(EFFORT_PATH, SelfIdentifying, InvalidValue),
                "premise: the same harness localizes the exact template on {lane}",
            );
        }
        let near_misses = [
            (
                "one changed word in the lead-in",
                swap("does not support", "does not allow"),
            ),
            ("missing closing period", swap("medium.", "medium")),
            (
                "trailing prose",
                format!("{SUPPORTED_LEVELS} See the docs."),
            ),
            (
                "separator with no member after it",
                swap("max, medium.", "max, ."),
            ),
            ("echoed level is not a name", swap("'xhigh'", "'x high'")),
            ("empty level list", swap("high, low, max, medium.", ".")),
            (
                "the bare effort-parameter refusal",
                "This model does not support the effort parameter.".to_string(),
            ),
        ];

        for (name, message) in near_misses {
            for (lane, kind, body) in on_each_lane(&message) {
                assert_eq!(
                    outcome(kind, 400, &body),
                    Outcome::NotLocalized,
                    "{name} on {lane}"
                );
            }
        }
    }

    #[test]
    fn the_openrouter_metadata_list_localizes_nothing() {
        let (status, body) = fixture(ANTHROPIC, "effort-invalid-value-openrouter");
        assert!(
            body.contains("\\\"values\\\"") && body.contains("output_config"),
            "premise: the capture carries the accepted list and the path in metadata.raw",
        );

        assert_eq!(outcome(ANTHROPIC, status, &body), Outcome::NotLocalized);
    }

    #[test]
    fn unknown_variant_near_misses_localize_nothing() {
        assert_eq!(
            outcome(BEDROCK, 400, &bedrock_body(UNKNOWN_VARIANT)),
            super::unresolved(EFFORT_PATH, Inferred, InvalidValue),
            "premise: the same harness localizes the exact template",
        );
        let swap = |from: &str, to: &str| {
            assert!(
                UNKNOWN_VARIANT.contains(from),
                "premise: {from:?} is in the template"
            );
            UNKNOWN_VARIANT.replace(from, to)
        };
        let cases = [
            (
                "another enum's variant list",
                BEDROCK,
                bedrock_body(&swap(
                    "`low`, `medium`, `high`, `xhigh`, `max`, `Unhandled`",
                    "`adaptive`, `enabled`, `disabled`",
                )),
            ),
            (
                "another enum's list naming one effort level",
                BEDROCK,
                bedrock_body(&swap(
                    "`low`, `medium`, `high`, `xhigh`, `max`, `Unhandled`",
                    "`auto`, `low`",
                )),
            ),
            (
                "only the catch-all variant",
                BEDROCK,
                bedrock_body(&swap(
                    "`low`, `medium`, `high`, `xhigh`, `max`, `Unhandled`",
                    "`Unhandled`",
                )),
            ),
            (
                "unquoted echoed variant",
                BEDROCK,
                bedrock_body(&swap("`bogus`", "bogus")),
            ),
            (
                "one changed word in the lead-in",
                BEDROCK,
                bedrock_body(&swap("unknown", "unexpected")),
            ),
            (
                "same message on anthropic-api",
                ANTHROPIC,
                anthropic_body(UNKNOWN_VARIANT),
            ),
        ];

        for (name, kind, body) in cases {
            assert_eq!(outcome(kind, 400, &body), Outcome::NotLocalized, "{name}");
        }
    }
}

mod responses_effort {
    //! The Responses effort refusals: the OpenAI envelope naming
    //! `reasoning.effort` in `error.param`, read onto the one canonical effort
    //! field with its echoed level and level mask, and only under that param.

    use std::collections::BTreeMap;

    use routectl_core::UpstreamBody;
    use serde_json::json;

    use super::level_list::{HIGH, LOW, MAX, MEDIUM, MINIMAL, XHIGH};
    use super::{
        EFFORT_PATH, InvalidValue, OPENAI_RESPONSES, Outcome, SelfIdentifying, fixture, localize,
        openai_coded_body, outcome, rejected_wire_path, rejections_dir,
    };

    const RESPONSES_EFFORT_PARAM: &str = "reasoning.effort";
    /// The codex `Invalid value` refusal, as captured.
    const INVALID_VALUE: &str = "Invalid value: 'bogus'. Supported values are: 'none', \
         'minimal', 'low', 'medium', 'high', 'xhigh', and 'max'.";
    /// The codex `Unsupported value` refusal, as captured.
    const UNSUPPORTED_VALUE: &str = "Unsupported value: 'minimal' is not supported with the \
         'gpt-5.6-terra' model. Supported values are: 'none', 'low', 'medium', 'high', 'xhigh', \
         and 'max'.";

    /// What `localize` reads: canonical path, raw path, echoed level, level
    /// mask, and whether the reason presupposes the field was sent.
    type Reading = (String, String, Option<String>, u8, bool);

    fn reading(status: u16, body: &str) -> Option<Reading> {
        let body = UpstreamBody::new(body);
        let localized = localize(OPENAI_RESPONSES, status, body.parsed()?)?;
        Some((
            localized.segments.join("."),
            localized.raw.to_string(),
            localized.echoed.map(str::to_owned),
            localized.levels,
            localized.presupposes_presence,
        ))
    }

    fn effort(echoed: &str, levels: u8) -> Option<Reading> {
        Some((
            EFFORT_PATH.to_string(),
            RESPONSES_EFFORT_PARAM.to_string(),
            Some(echoed.to_string()),
            levels,
            true,
        ))
    }

    fn body(message: &str, param: &str, code: &str) -> String {
        openai_coded_body(message, Some(param), code)
    }

    /// Mutation checks: drop the `Unsupported value` head -> red on the
    /// minimal case; drop the `reasoning.effort` canonical mapping -> red on
    /// every case (the path reads `reasoning.effort`, the mask `0`).
    #[test]
    fn every_responses_effort_capture_reads_to_its_echo_and_level_mask() {
        let mut readings = BTreeMap::new();
        for file in std::fs::read_dir(rejections_dir().join(OPENAI_RESPONSES)).expect("kind dir") {
            let path = file.expect("file entry").path();
            let case = path.file_stem().unwrap().to_str().unwrap().to_string();
            let (status, body) = fixture(OPENAI_RESPONSES, &case);
            readings.insert(case, reading(status, &body));
        }
        let six = MINIMAL | LOW | MEDIUM | HIGH | XHIGH | MAX;
        let without_minimal = LOW | MEDIUM | HIGH | XHIGH | MAX;
        let expected: BTreeMap<String, Option<Reading>> = [
            ("reasoning-effort-invalid-value-codex", effort("bogus", six)),
            (
                "reasoning-effort-invalid-value-codex-gpt-5-6",
                effort("bogus", six),
            ),
            (
                "reasoning-effort-invalid-value-mantle",
                effort("bogus", without_minimal),
            ),
            // The refused level is echoed and absent from the list.
            (
                "reasoning-effort-minimal-unsupported-codex",
                effort("minimal", without_minimal),
            ),
        ]
        .into_iter()
        .map(|(case, reading)| (case.to_string(), reading))
        .collect();

        assert_eq!(readings, expected);
    }

    /// The same envelope under any other `error.param` localizes nothing:
    /// its list is another field's, and the parameter is not the effort one.
    ///
    /// Mutation check: drop the `error.param == reasoning.effort` gate in
    /// `localize_responses` -> red (an echo and the listed values appear).
    #[test]
    fn a_value_list_is_read_only_under_the_effort_param() {
        for (name, message, code, refused) in [
            ("invalid value", INVALID_VALUE, "invalid_value", "bogus"),
            (
                "unsupported value",
                UNSUPPORTED_VALUE,
                "unsupported_value",
                "minimal",
            ),
        ] {
            assert_eq!(
                reading(400, &body(message, RESPONSES_EFFORT_PARAM, code))
                    .and_then(|reading| reading.2)
                    .as_deref(),
                Some(refused),
                "premise: {name} reads its echo under the effort param",
            );

            assert_eq!(
                reading(400, &body(message, "temperature", code)),
                None,
                "{name}"
            );
        }
    }

    /// A rejection status is a 400 or a 422; the status a failed codex
    /// stream event surfaces on is 0, and it localizes nothing.
    #[test]
    fn the_codex_refusal_at_status_zero_localizes_nothing() {
        let (status, body) = fixture(
            OPENAI_RESPONSES,
            "reasoning-effort-minimal-unsupported-codex",
        );
        assert_eq!(
            outcome(OPENAI_RESPONSES, status, &body),
            super::unresolved(EFFORT_PATH, SelfIdentifying, InvalidValue),
            "premise: the same text localizes at its captured status",
        );

        assert_eq!(outcome(OPENAI_RESPONSES, 0, &body), Outcome::NotLocalized);
        assert_eq!(
            rejected_wire_path(
                OPENAI_RESPONSES,
                0,
                UpstreamBody::new(body.as_str()).parsed()
            ),
            None
        );
    }

    /// The wire locator keeps the Responses spelling, so it is looked up in
    /// the body that went out; only the canonical field is the effort one.
    ///
    /// Mutation check: canonicalize `raw` as well -> red on the locator.
    #[test]
    fn the_locator_keeps_the_wire_spelling_and_the_field_is_canonical() {
        let (status, body) = fixture(OPENAI_RESPONSES, "reasoning-effort-invalid-value-codex");
        let body = UpstreamBody::new(body);

        let rejected = rejected_wire_path(OPENAI_RESPONSES, status, body.parsed())
            .expect("the codex refusal localizes");

        assert!(rejected.locator.segments().eq(["reasoning", "effort"]));
        assert!(rejected.field.segments().eq(["output_config", "effort"]));
        assert!(rejected.presupposes_presence);
    }

    /// The canonical mapping is the Responses spelling's alone: the same
    /// param on openai-compat stays `reasoning.effort`, and on Responses a
    /// longer or shorter path under `reasoning` is not the effort field and
    /// localizes nothing.
    #[test]
    fn only_the_responses_effort_param_maps_onto_the_effort_field() {
        let rows = [
            (
                "openai-compat",
                RESPONSES_EFFORT_PARAM,
                RESPONSES_EFFORT_PARAM,
            ),
            (OPENAI_RESPONSES, RESPONSES_EFFORT_PARAM, EFFORT_PATH),
        ];
        for (kind, param, canonical) in rows {
            let body = UpstreamBody::new(body("anything", param, "invalid_value"));
            let localized = localize(kind, 400, body.parsed().unwrap())
                .unwrap_or_else(|| panic!("{kind} {param}: localizes"));

            assert_eq!(localized.segments.join("."), canonical, "{kind} {param}");
        }
        for param in ["reasoning.effort.level", "reasoning"] {
            let body = UpstreamBody::new(body("anything", param, "invalid_value"));

            assert!(
                localize(OPENAI_RESPONSES, 400, body.parsed().unwrap()).is_none(),
                "{param}"
            );
        }
    }

    #[test]
    fn responses_value_refusal_near_misses_carry_no_echo_or_levels() {
        let swap = |template: &str, from: &str, to: &str| {
            assert!(
                template.contains(from),
                "premise: {from:?} is in the template"
            );
            template.replace(from, to)
        };
        let near_misses = [
            (
                "missing closing period",
                swap(INVALID_VALUE, "'max'.", "'max'"),
            ),
            ("trailing prose", format!("{INVALID_VALUE} See the docs.")),
            (
                "one changed word",
                swap(INVALID_VALUE, "Supported values", "Allowed values"),
            ),
            (
                "echoed value is not a name",
                swap(INVALID_VALUE, "'bogus'", "'bo gus'"),
            ),
            (
                "separator with no member",
                swap(INVALID_VALUE, ", and 'max'.", ", and ."),
            ),
            (
                "bare list",
                swap(INVALID_VALUE, "'none', 'minimal'", "none, minimal"),
            ),
            (
                "model id with a space",
                swap(UNSUPPORTED_VALUE, "'gpt-5.6-terra'", "'gpt 5.6'"),
            ),
            (
                "unsupported wording without the model",
                swap(UNSUPPORTED_VALUE, " with the 'gpt-5.6-terra' model", ""),
            ),
        ];

        for (name, message) in near_misses {
            let reading = reading(
                400,
                &body(&message, RESPONSES_EFFORT_PARAM, "invalid_value"),
            );

            assert_eq!(
                reading,
                Some((
                    EFFORT_PATH.to_string(),
                    RESPONSES_EFFORT_PARAM.to_string(),
                    None,
                    0,
                    false,
                )),
                "{name}: the param still names the field, with no value read",
            );
        }
    }

    #[test]
    fn the_codex_minimal_refusal_echoes_a_level_its_list_omits() {
        let body = json!({"error": {
            "message": UNSUPPORTED_VALUE,
            "type": "invalid_request_error",
            "param": RESPONSES_EFFORT_PARAM,
            "code": "unsupported_value",
        }})
        .to_string();

        let (_, _, echoed, levels, _) = reading(400, &body).expect("localizes");

        assert_eq!(echoed.as_deref(), Some("minimal"));
        assert_eq!(levels & MINIMAL, 0);
        assert_eq!(levels, LOW | MEDIUM | HIGH | XHIGH | MAX);
    }
}
