//! Welds the rejection parser to the committed rejection corpus: every
//! captured envelope has a stated outcome (a row, a localized path with no
//! row, or no localization), every grounded row is reached by a capture, and
//! near-miss wording resolves to nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use routectl_core::capability::SignalTier;
use routectl_core::{MAX_ERROR_BODY_BYTES, ParsedErrorBody, UpstreamBody};
use serde_json::{Value, json};

use super::super::field_repair::{closed_table, grounded_table};
use super::{
    LocalizedPath, RejectionKind, RejectionValue, THINKING_TYPES, localize, parse_rejection,
    rejected_wire_path,
};

const BEDROCK_CAPTURE: &str = include_str!("../../tests/fixtures/bedrock_validation_capture.json");
const FORCED_TOOL_CHOICE_CAPTURE: &str =
    include_str!("../../tests/fixtures/forced_tool_choice_capture.json");

const ANTHROPIC: &str = "anthropic-api";
const BEDROCK: &str = "bedrock";
const OPENAI_COMPAT: &str = "openai-compat";
const OPENAI_RESPONSES: &str = "openai-responses";

const DISPLAY_PATH: &str = "thinking.enabled.display";
const DISPLAY_MESSAGE: &str = "thinking.enabled.display: Input should be 'summarized', 'omitted'";
const CONVERSE_WRAPPER: &str = "The model returned the following errors: ";
const THINKING_DISABLED_PATH: &str = "thinking.type.disabled";
const TEMPERATURE_PATH: &str = "temperature";
const MAX_TOKENS_PATH: &str = "max_tokens";
/// The off-switch template exactly as the committed Bedrock capture spells it.
const THINKING_SWITCH_MESSAGE: &str = "To turn thinking off on this model, send \"thinking\": \
     {\"type\": \"between_tools\"} instead of {\"type\": \"disabled\"}. The model does not \
     think before responding.";

use RejectionKind::{InvalidValue, UnsupportedField};
use SignalTier::{Inferred, SelfIdentifying};

/// What the parser makes of one captured envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// Resolved to the closed-table row with this path.
    Row(&'static str, SignalTier, RejectionKind),
    /// Localized a path that has no row, shown canonicalized with a trailing
    /// `[]` when the raw path ended in an index.
    Unresolved(String, SignalTier, RejectionKind),
    /// No arm localized a path.
    NotLocalized,
}

fn outcome(kind: &str, status: u16, body: &str) -> Outcome {
    let body = UpstreamBody::new(body);
    if let Some(named) = parse_rejection(
        crate::router::FieldRepairTable::default(),
        kind,
        status,
        body.parsed(),
    ) {
        return Outcome::Row(named.row.path, named.tier, named.kind);
    }
    match body.parsed().and_then(|p| localize(kind, status, p)) {
        Some(LocalizedPath {
            segments,
            ends_in_index,
            tier,
            kind,
            ..
        }) => {
            let marker = if ends_in_index { "[]" } else { "" };
            Outcome::Unresolved(format!("{}{marker}", segments.join(".")), tier, kind)
        }
        None => Outcome::NotLocalized,
    }
}

fn unresolved(path: &str, tier: SignalTier, kind: RejectionKind) -> Outcome {
    Outcome::Unresolved(path.to_string(), tier, kind)
}

fn anthropic_body(message: &str) -> String {
    json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
        .to_string()
}

fn bedrock_body(message: &str) -> String {
    json!({ "message": message }).to_string()
}

fn openai_body(message: &str, param: Option<&str>) -> String {
    json!({"error": {"message": message, "type": "invalid_request_error", "param": param}})
        .to_string()
}

fn openai_coded_body(message: &str, param: Option<&str>, code: &str) -> String {
    json!({
        "error": {"message": message, "type": "invalid_request_error", "param": param, "code": code}
    })
    .to_string()
}

fn rejections_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rejections")
}

/// The status and body of the committed `rejections/<kind>/<case>.json`.
fn fixture(kind: &str, case: &str) -> (u16, String) {
    let file = rejections_dir().join(kind).join(format!("{case}.json"));
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let status = u16::try_from(fixture["status"].as_u64().unwrap()).unwrap();
    (status, fixture["body"].as_str().unwrap().to_string())
}

/// The replacements `localize` reads from `body`, or `None` when it
/// localizes nothing.
fn replacements(kind: &str, status: u16, body: &str) -> Option<Vec<String>> {
    let body = UpstreamBody::new(body);
    let localized = localize(kind, status, body.parsed()?)?;
    Some(
        localized
            .replacements
            .iter()
            .map(|token| (*token).to_string())
            .collect(),
    )
}

/// Every `rejections/<kind>/<case>.json` fixture, keyed `<kind>/<case>`, with
/// the outcome the parser reaches for it on the lane kind its directory names.
fn corpus_outcomes() -> BTreeMap<String, Outcome> {
    let mut outcomes = BTreeMap::new();
    for kind_dir in std::fs::read_dir(rejections_dir()).expect("rejections dir") {
        let kind_dir = kind_dir.expect("dir entry").path();
        let kind = kind_dir.file_name().unwrap().to_str().unwrap().to_string();
        for file in std::fs::read_dir(&kind_dir).expect("kind dir") {
            let file = file.expect("file entry").path();
            let case = file.file_stem().unwrap().to_str().unwrap().to_string();
            let fixture: Value =
                serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let status = u16::try_from(fixture["status"].as_u64().unwrap()).unwrap();
            let body = fixture["body"].as_str().unwrap();
            outcomes.insert(format!("{kind}/{case}"), outcome(&kind, status, body));
        }
    }
    outcomes
}

/// Every Bedrock validation canary, keyed by its capture name.
fn bedrock_canary_outcomes() -> BTreeMap<String, Outcome> {
    let capture: Value = serde_json::from_str(BEDROCK_CAPTURE).unwrap();
    capture["canaries"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, canary)| {
            let status = u16::try_from(canary["status"].as_u64().unwrap()).unwrap();
            let body = canary["body"].as_str().unwrap();
            (name.clone(), outcome(BEDROCK, status, body))
        })
        .collect()
}

#[test]
fn every_committed_rejection_fixture_resolves_to_a_row_or_is_reported_unresolved() {
    let expected: BTreeMap<String, Outcome> = [
        (
            "anthropic-api/beta-prompt-caching-scope-mantle",
            Outcome::NotLocalized,
        ),
        ("anthropic-api/beta-two-flags-mantle", Outcome::NotLocalized),
        (
            "anthropic-api/max-tokens-above-model-limit-messages",
            Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, InvalidValue),
        ),
        (
            "anthropic-api/thinking-display-enum",
            Outcome::Row(DISPLAY_PATH, SelfIdentifying, InvalidValue),
        ),
        (
            "anthropic-api/thinking-type-disabled-unsupported",
            Outcome::Row(THINKING_DISABLED_PATH, SelfIdentifying, UnsupportedField),
        ),
        (
            "bedrock/beta-bag-key-extraneous-converse",
            Outcome::NotLocalized,
        ),
        (
            "bedrock/beta-bag-key-unknown-parameter-converse",
            Outcome::NotLocalized,
        ),
        (
            "bedrock/beta-redact-thinking-converse",
            Outcome::NotLocalized,
        ),
        ("bedrock/beta-redact-thinking-invoke", Outcome::NotLocalized),
        (
            "bedrock/max-tokens-above-model-limit",
            Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, InvalidValue),
        ),
        (
            "bedrock/max-tokens-above-model-limit-invoke",
            Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, InvalidValue),
        ),
        (
            "bedrock/temperature-field-unsupported",
            Outcome::Row(TEMPERATURE_PATH, SelfIdentifying, UnsupportedField),
        ),
        (
            "bedrock/thinking-type-disabled-unsupported",
            Outcome::Row(THINKING_DISABLED_PATH, SelfIdentifying, UnsupportedField),
        ),
        (
            "bedrock/thinking-type-enabled-unsupported",
            Outcome::Row("thinking.type.enabled", SelfIdentifying, UnsupportedField),
        ),
        (
            "openai-compat/unsupported-parameter-temperature",
            Outcome::Row(TEMPERATURE_PATH, SelfIdentifying, UnsupportedField),
        ),
        (
            "openai-compat/unsupported-parameter-top-p",
            Outcome::Row("top_p", SelfIdentifying, UnsupportedField),
        ),
        (
            "openai-compat/unsupported-parameter-presence-penalty",
            Outcome::Row("presence_penalty", SelfIdentifying, UnsupportedField),
        ),
        (
            "openai-compat/unsupported-parameter-frequency-penalty",
            Outcome::Row("frequency_penalty", SelfIdentifying, UnsupportedField),
        ),
    ]
    .into_iter()
    .map(|(case, outcome)| (case.to_string(), outcome))
    .collect();

    assert_eq!(corpus_outcomes(), expected);
}

#[test]
fn every_bedrock_validation_canary_resolves_to_a_row_or_is_reported_unresolved() {
    let expected: BTreeMap<String, Outcome> = [
        ("advisor-tool", Outcome::NotLocalized),
        ("computer-aws-sonnet-4-6", Outcome::NotLocalized),
        ("computer-aws-sonnet-5", Outcome::NotLocalized),
        ("converse-advisor-composed", Outcome::NotLocalized),
        (
            "converse-field-probe",
            unresolved(
                "thinking.adaptive.routectl_probe_garbage",
                SelfIdentifying,
                UnsupportedField,
            ),
        ),
        ("converse-web-search-composed", Outcome::NotLocalized),
        (
            "schema-error-converse-sonnet-4-6",
            unresolved("output_config.format.schema", Inferred, InvalidValue),
        ),
        (
            "schema-error-invoke-sonnet-4-6",
            unresolved("output_config.format.schema", Inferred, InvalidValue),
        ),
        (
            "structured-output-converse-opus-5-5",
            unresolved("output_config.format", SelfIdentifying, UnsupportedField),
        ),
        (
            "structured-output-invoke-opus-5-5",
            unresolved("output_config.format", SelfIdentifying, UnsupportedField),
        ),
        ("supp-bad-model-id", Outcome::NotLocalized),
        ("tool-changes-sonnet-5", Outcome::NotLocalized),
        (
            "tool-removal-messages-opus-5-5",
            unresolved("messages.content[]", Inferred, InvalidValue),
        ),
        (
            "toolset-name-opus-5-5",
            unresolved("tools.computer_toolset_20260801", Inferred, InvalidValue),
        ),
        ("unknown-beta", Outcome::NotLocalized),
        (
            "unknown-body-field",
            unresolved(
                "routectl_envelope_probe_field",
                SelfIdentifying,
                UnsupportedField,
            ),
        ),
        (
            "web-search-pydantic-sonnet-4-6",
            unresolved("tools[]", Inferred, InvalidValue),
        ),
        ("web-search-template-sonnet-5", Outcome::NotLocalized),
    ]
    .into_iter()
    .map(|(case, outcome)| (case.to_string(), outcome))
    .collect();

    assert_eq!(bedrock_canary_outcomes(), expected);
}

#[test]
fn the_forced_tool_choice_location_prefix_is_an_inferred_unresolved_path() {
    let capture: Value = serde_json::from_str(FORCED_TOOL_CHOICE_CAPTURE).unwrap();
    let anthropic = &capture["anthropic_api"];
    let status = u16::try_from(anthropic["status"].as_u64().unwrap()).unwrap();

    let reached = outcome(ANTHROPIC, status, anthropic["body"].as_str().unwrap());

    assert_eq!(reached, unresolved("tool_choice", Inferred, InvalidValue));
}

#[test]
fn every_grounded_row_is_reached_by_a_committed_fixture() {
    let reached: Vec<&'static str> = corpus_outcomes()
        .into_values()
        .chain(bedrock_canary_outcomes().into_values())
        .filter_map(|outcome| match outcome {
            Outcome::Row(path, ..) => Some(path),
            _ => None,
        })
        .collect();

    for row in grounded_table() {
        assert!(
            reached.contains(&row.path),
            "grounded row {} is reached by no committed rejection fixture",
            row.path,
        );
    }
}

#[test]
fn near_miss_messages_resolve_to_no_row() {
    let forced: Value = serde_json::from_str(FORCED_TOOL_CHOICE_CAPTURE).unwrap();
    let bedrock: Value = serde_json::from_str(BEDROCK_CAPTURE).unwrap();
    let bodies = forced["near_miss_messages"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(name, message)| {
            let message = message.as_str().unwrap();
            [
                (
                    format!("forced/{name}/anthropic"),
                    ANTHROPIC,
                    anthropic_body(message),
                ),
                (
                    format!("forced/{name}/bedrock"),
                    BEDROCK,
                    bedrock_body(message),
                ),
            ]
        })
        .chain(
            bedrock["near_miss_messages"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, message)| {
                    let body = bedrock_body(message.as_str().unwrap());
                    (format!("bedrock/{name}"), BEDROCK, body)
                }),
        );

    let mut checked = 0;
    for (name, kind, body) in bodies {
        let parsed = UpstreamBody::new(body);
        assert!(
            parse_rejection(
                crate::router::FieldRepairTable::default(),
                kind,
                400,
                parsed.parsed()
            )
            .is_none(),
            "near miss {name} resolved to a row",
        );
        checked += 1;
    }

    assert!(checked > 20, "premise: both near-miss sets were read");
    let control = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            control.parsed()
        )
        .is_some(),
        "the same harness resolves a grounded message",
    );
}

#[test]
fn each_arm_assigns_its_tier() {
    let display = |tier| Outcome::Row(DISPLAY_PATH, tier, UnsupportedField);
    let cases = [
        (
            "grounded location prefix",
            ANTHROPIC,
            anthropic_body(DISPLAY_MESSAGE),
            Outcome::Row(DISPLAY_PATH, SelfIdentifying, InvalidValue),
        ),
        (
            "prose location prefix",
            ANTHROPIC,
            anthropic_body("thinking.enabled.display: is not available for this request"),
            Outcome::Row(DISPLAY_PATH, Inferred, InvalidValue),
        ),
        (
            "grounded quoted template",
            BEDROCK,
            bedrock_body(&format!(
                "{CONVERSE_WRAPPER}\"thinking.enabled.display\" is not supported for this \
                 model. Use \"thinking.type.adaptive\" instead."
            )),
            display(SelfIdentifying),
        ),
        (
            "prose quoted not-supported",
            ANTHROPIC,
            anthropic_body("\"thinking.enabled.display\" is not supported on this lane"),
            display(Inferred),
        ),
        (
            "thinking off-switch template",
            BEDROCK,
            bedrock_body(THINKING_SWITCH_MESSAGE),
            Outcome::Row(THINKING_DISABLED_PATH, SelfIdentifying, UnsupportedField),
        ),
        (
            "structured param",
            OPENAI_COMPAT,
            openai_body("anything at all", Some(DISPLAY_PATH)),
            Outcome::Row(DISPLAY_PATH, SelfIdentifying, InvalidValue),
        ),
        (
            "unsupported-parameter template",
            OPENAI_COMPAT,
            openai_body(
                "Unsupported parameter: 'thinking.enabled.display' is not supported with this \
                 model.",
                None,
            ),
            display(SelfIdentifying),
        ),
    ];

    for (name, kind, body, expected) in cases {
        assert_eq!(outcome(kind, 400, &body), expected, "{name}");
        let body = UpstreamBody::new(body);
        if let Some(named) = parse_rejection(
            crate::router::FieldRepairTable::default(),
            kind,
            400,
            body.parsed(),
        ) {
            let rewrite =
                (named.row.path == THINKING_DISABLED_PATH).then_some("thinking.type.between_tools");
            assert_eq!(named.value, rewrite.map(RejectionValue::Shape), "{name}");
        }
    }
}

#[test]
fn each_classification_rule_assigns_its_kind() {
    let template =
        format!("Unsupported parameter: '{DISPLAY_PATH}' is not supported with this model.");
    let other_template =
        "Unsupported parameter: 'temperature' is not supported with this model.".to_string();
    let cases = [
        (
            "location prefix, Input should be (display fixture)",
            ANTHROPIC,
            anthropic_body(DISPLAY_MESSAGE),
            (DISPLAY_PATH, InvalidValue),
        ),
        (
            "location prefix, Extra inputs are not permitted",
            BEDROCK,
            bedrock_body(&format!("{DISPLAY_PATH}: Extra inputs are not permitted")),
            (DISPLAY_PATH, UnsupportedField),
        ),
        (
            "location prefix, any other reason",
            ANTHROPIC,
            anthropic_body(&format!(
                "{DISPLAY_PATH}: is not available for this request"
            )),
            (DISPLAY_PATH, InvalidValue),
        ),
        (
            "quoted is not supported, grounded continuation",
            BEDROCK,
            bedrock_body(&format!(
                "{CONVERSE_WRAPPER}\"{DISPLAY_PATH}\" is not supported for this model. Use \
                 \"thinking.type.adaptive\" instead."
            )),
            (DISPLAY_PATH, UnsupportedField),
        ),
        (
            "quoted is not supported, prose continuation",
            ANTHROPIC,
            anthropic_body(&format!("\"{DISPLAY_PATH}\" is not supported on this lane")),
            (DISPLAY_PATH, UnsupportedField),
        ),
        (
            "bare error.param",
            OPENAI_COMPAT,
            openai_body("anything at all", Some(DISPLAY_PATH)),
            (DISPLAY_PATH, InvalidValue),
        ),
        (
            "error.param with another code",
            OPENAI_COMPAT,
            openai_coded_body("anything at all", Some(DISPLAY_PATH), "invalid_value"),
            (DISPLAY_PATH, InvalidValue),
        ),
        (
            "error.param with code unsupported_parameter",
            OPENAI_COMPAT,
            openai_coded_body(
                "anything at all",
                Some(DISPLAY_PATH),
                "unsupported_parameter",
            ),
            (DISPLAY_PATH, UnsupportedField),
        ),
        (
            "exact singular template, no param",
            OPENAI_COMPAT,
            openai_body(&template, None),
            (DISPLAY_PATH, UnsupportedField),
        ),
        (
            "exact singular template naming error.param",
            OPENAI_COMPAT,
            openai_body(&template, Some(DISPLAY_PATH)),
            (DISPLAY_PATH, UnsupportedField),
        ),
        (
            "exact singular template naming another parameter",
            OPENAI_COMPAT,
            openai_body(&other_template, Some(DISPLAY_PATH)),
            (DISPLAY_PATH, InvalidValue),
        ),
        (
            "thinking off-switch template",
            BEDROCK,
            bedrock_body(THINKING_SWITCH_MESSAGE),
            (THINKING_DISABLED_PATH, UnsupportedField),
        ),
    ];

    for (name, provider_kind, body, (path, kind)) in cases {
        let reached = match outcome(provider_kind, 400, &body) {
            Outcome::Row(path, _, kind) => Some((path.to_string(), kind, true)),
            Outcome::Unresolved(path, _, kind) => Some((path, kind, false)),
            Outcome::NotLocalized => None,
        };
        let has_row = path == DISPLAY_PATH || path == THINKING_DISABLED_PATH;
        assert_eq!(reached, Some((path.to_string(), kind, has_row)), "{name}");
    }
}

#[test]
fn a_path_ending_in_an_index_names_no_row() {
    let message = |path: &str| anthropic_body(&format!("{path}: Extra inputs are not permitted"));
    let control = UpstreamBody::new(message("thinking.enabled.0.display"));
    assert_eq!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            control.parsed()
        )
        .map(|n| n.row.path),
        Some(DISPLAY_PATH),
        "premise: an interior index still canonicalizes onto the row",
    );

    for path in [
        "thinking.enabled.display.0",
        "thinking.enabled.display[0]",
        "thinking.enabled.display[0][1]",
    ] {
        let body = UpstreamBody::new(message(path));

        assert!(
            parse_rejection(
                crate::router::FieldRepairTable::default(),
                ANTHROPIC,
                400,
                body.parsed()
            )
            .is_none(),
            "{path}",
        );
        assert_eq!(
            outcome(ANTHROPIC, 400, body.text()),
            unresolved(
                "thinking.enabled.display[]",
                SelfIdentifying,
                UnsupportedField
            ),
            "{path}: still reported as a localized path",
        );
    }
}

#[test]
fn the_plural_unsupported_parameters_wording_is_not_localized() {
    for message in [
        "Validation: Unsupported parameter(s): system",
        "Validation: Unsupported parameter(s): thinking.enabled.display",
    ] {
        let body = openai_body(message, None);

        assert_eq!(
            outcome(OPENAI_COMPAT, 400, &body),
            Outcome::NotLocalized,
            "{message}",
        );
    }
}

#[test]
fn openai_responses_has_no_parser_and_no_committed_rejection_fixture() {
    let structured = UpstreamBody::new(openai_coded_body(
        "anything at all",
        Some(DISPLAY_PATH),
        "unsupported_parameter",
    ));
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            OPENAI_COMPAT,
            400,
            structured.parsed()
        )
        .is_some(),
        "premise: the same envelope resolves on openai-compat",
    );

    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            OPENAI_RESPONSES,
            400,
            structured.parsed()
        )
        .is_none()
    );
    assert!(
        rejections_dir().is_dir() && !rejections_dir().join(OPENAI_RESPONSES).exists(),
        "a committed Responses rejection fixture must come with a deliberate parser arm",
    );
}

#[test]
fn the_unsupported_parameter_template_must_match_exactly() {
    for message in [
        "Unsupported parameter: 'thinking.enabled.display' is not supported with this model",
        "Unsupported parameters: 'thinking.enabled.display' is not supported with this model.",
        "Error. Unsupported parameter: 'thinking.enabled.display' is not supported with this \
         model.",
    ] {
        let body = UpstreamBody::new(openai_body(message, None));

        assert!(
            parse_rejection(
                crate::router::FieldRepairTable::default(),
                OPENAI_COMPAT,
                400,
                body.parsed()
            )
            .is_none(),
            "{message}",
        );
    }
}

#[test]
fn indices_and_the_converse_field_bag_canonicalize_onto_the_row() {
    let cases = [
        (ANTHROPIC, "thinking.enabled.0.display"),
        (ANTHROPIC, "thinking.enabled[0].display"),
        (
            BEDROCK,
            "additionalModelRequestFields.thinking.enabled.display",
        ),
    ];

    for (kind, path) in cases {
        let message = format!("{path}: Input should be 'summarized', 'omitted'");
        let body = if kind == BEDROCK {
            bedrock_body(&message)
        } else {
            anthropic_body(&message)
        };
        let body = UpstreamBody::new(body);

        let named = parse_rejection(
            crate::router::FieldRepairTable::default(),
            kind,
            400,
            body.parsed(),
        );

        assert_eq!(
            named.map(|n| n.row.path),
            Some(DISPLAY_PATH),
            "{kind} {path}"
        );
    }
}

#[test]
fn the_converse_field_bag_is_only_removed_on_bedrock() {
    let message = format!(
        "additionalModelRequestFields.{DISPLAY_PATH}: Input should be 'summarized', 'omitted'"
    );
    let body = UpstreamBody::new(anthropic_body(&message));

    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            body.parsed()
        )
        .is_none()
    );
}

#[test]
fn a_non_ascii_or_over_long_param_resolves_to_none() {
    let over_long = format!("thinking.enabled{}.display", ".0".repeat(30));
    assert!(over_long.len() > routectl_core::MAX_SAFE_TOKEN_LEN);
    let short_with_index = "thinking.enabled.0.display";

    for param in ["thinking.enabled.displ\u{e9}y", over_long.as_str()] {
        let body = UpstreamBody::new(openai_body("rejected", Some(param)));

        assert!(
            parse_rejection(
                crate::router::FieldRepairTable::default(),
                OPENAI_COMPAT,
                400,
                body.parsed()
            )
            .is_none(),
            "{param:?}",
        );
    }
    let control = UpstreamBody::new(openai_body("rejected", Some(short_with_index)));
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            OPENAI_COMPAT,
            400,
            control.parsed()
        )
        .is_some(),
        "the same path, bounded, resolves",
    );
}

#[test]
fn the_named_row_is_a_table_entry_holding_no_body_bytes() {
    let body = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));
    let body_bytes = body.text().as_bytes().as_ptr_range();

    let named = parse_rejection(
        crate::router::FieldRepairTable::default(),
        ANTHROPIC,
        400,
        body.parsed(),
    )
    .expect("display row");

    assert!(
        closed_table()
            .iter()
            .any(|row| std::ptr::eq(row, named.row)),
        "the named row is an entry of the closed table, not a copy",
    );
    assert!(
        !body_bytes.contains(&named.row.path.as_ptr()),
        "the row path points into the table, not into the upstream body",
    );
    drop(body);
    assert_eq!(
        named.row.path, DISPLAY_PATH,
        "the rejection outlives the body"
    );
}

#[test]
fn an_over_cap_or_absent_parse_resolves_to_none() {
    let filler = " ".repeat(MAX_ERROR_BODY_BYTES);
    let over_cap = UpstreamBody::new(format!("{}{filler}", anthropic_body(DISPLAY_MESSAGE)));
    assert!(over_cap.parsed().is_none(), "premise: over cap is unparsed");

    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            over_cap.parsed()
        )
        .is_none()
    );
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            None::<&ParsedErrorBody>
        )
        .is_none()
    );
}

#[test]
fn a_kind_or_status_without_a_parser_resolves_to_none() {
    let anthropic = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));
    let openai = UpstreamBody::new(openai_body("rejected", Some(DISPLAY_PATH)));
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            422,
            anthropic.parsed()
        )
        .is_some(),
        "premise: the body resolves on a parsed kind and rejection status",
    );

    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            "gemini",
            400,
            anthropic.parsed()
        )
        .is_none()
    );
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            "gemini",
            400,
            openai.parsed()
        )
        .is_none()
    );
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            500,
            anthropic.parsed()
        )
        .is_none()
    );
    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            429,
            anthropic.parsed()
        )
        .is_none()
    );
}

#[test]
fn an_anthropic_message_outside_an_invalid_request_error_resolves_to_none() {
    let body =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": DISPLAY_MESSAGE}});
    let body = UpstreamBody::new(body.to_string());

    assert!(
        parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            body.parsed()
        )
        .is_none()
    );
}

/// Mutation check: read candidates from every quoted token in the message
/// instead of the template's replacement position -> red (the echoed and the
/// mixed rows).
#[test]
fn the_accepted_value_fills_only_for_exactly_one_named_member() {
    use super::super::field_repair::{TEST_REWRITE_ACCEPTED, TEST_REWRITE_PATH};
    let [first, second] = TEST_REWRITE_ACCEPTED else {
        panic!("premise: the test rewrite row's closed set has two members");
    };
    let outside = "routectl-test-not-a-member";
    let listed = |reason: &str| format!("{TEST_REWRITE_PATH}: Input should be {reason}");
    let cases: [(&str, String, RejectionKind, Option<&str>); 14] = [
        (
            "one single-quoted member",
            listed(&format!("'{first}'")),
            InvalidValue,
            Some(first),
        ),
        (
            "one double-quoted member",
            listed(&format!("\"{second}\"")),
            InvalidValue,
            Some(second),
        ),
        (
            "one member named twice",
            listed(&format!("'{first}' or '{first}'")),
            InvalidValue,
            Some(first),
        ),
        (
            "a list naming one member among outside values",
            listed(&format!("'{outside}', '{first}', or '{outside}'")),
            InvalidValue,
            Some(first),
        ),
        (
            "two members",
            listed(&format!("'{first}', '{second}'")),
            InvalidValue,
            None,
        ),
        (
            "a value outside the set",
            listed(&format!("'{outside}'")),
            InvalidValue,
            None,
        ),
        ("an unquoted member", listed(first), InvalidValue, None),
        (
            "a member echoed after the list as the refused input",
            listed(&format!("'{outside}'. Received input: '{first}'")),
            InvalidValue,
            None,
        ),
        (
            "a list naming one member, the other echoed as the refused input",
            listed(&format!("'{second}'. Received input: '{first}'")),
            InvalidValue,
            Some(second),
        ),
        (
            "a truncated list ending in a separator",
            listed(&format!("'{first}' or ")),
            InvalidValue,
            None,
        ),
        (
            "a separator followed by an unquoted word",
            listed(&format!("'{first}', or something else")),
            InvalidValue,
            None,
        ),
        (
            "an escaped delimiter inside a member",
            listed(&format!("'{first}\\', '{second}'")),
            InvalidValue,
            None,
        ),
        (
            "a member echoed in prose with no list",
            format!("{TEST_REWRITE_PATH}: '{first}' is not a permitted value"),
            InvalidValue,
            None,
        ),
        (
            "a replacement clause naming one member after the refused one",
            format!(
                "\"{TEST_REWRITE_PATH}\" is not supported for this model. Use \"{second}\" \
                 instead of \"{first}\"."
            ),
            UnsupportedField,
            Some(second),
        ),
    ];

    for (name, message, kind, expected) in cases {
        let body = UpstreamBody::new(anthropic_body(&message));

        let named = parse_rejection(
            crate::router::FieldRepairTable::default(),
            ANTHROPIC,
            400,
            body.parsed(),
        )
        .unwrap_or_else(|| panic!("{name}: resolves the rewrite row"));

        assert_eq!(named.row.path, TEST_REWRITE_PATH, "{name}");
        assert_eq!(named.kind, kind, "{name}");
        assert_eq!(named.value, expected.map(RejectionValue::Shape), "{name}");
    }
}

#[test]
fn a_drop_row_never_carries_an_accepted_value() {
    let body = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));

    let named = parse_rejection(
        crate::router::FieldRepairTable::default(),
        ANTHROPIC,
        400,
        body.parsed(),
    )
    .expect("display row");

    assert_eq!(
        named.value, None,
        "the display envelope quotes values, but its row drops rather than rewrites",
    );
}

/// Mutation check: drop the quoted arm's `Use "..."` replacement read -> red
/// (no accepted member, so the rewrite row repairs nothing).
#[test]
fn the_thinking_enabled_fixture_names_the_adaptive_rewrite() {
    let (status, body) = fixture(BEDROCK, "thinking-type-enabled-unsupported");
    let body = UpstreamBody::new(body);

    let named = parse_rejection(
        crate::router::FieldRepairTable::grounded_only(),
        BEDROCK,
        status,
        body.parsed(),
    )
    .expect("the captured envelope names the grounded row");

    assert_eq!(named.row.path, "thinking.type.enabled");
    assert_eq!(named.tier, SelfIdentifying);
    assert_eq!(named.kind, UnsupportedField);
    assert_eq!(
        named.value,
        Some(RejectionValue::Shape("thinking.type.adaptive"))
    );
}

/// Mutation check: remove either member from the disabled row's accepted set
/// -> red on that member's fixture (no accepted member, so nothing rewrites).
#[test]
fn each_thinking_disabled_fixture_names_its_rewrite_on_the_disabled_row() {
    let cases = [
        (ANTHROPIC, "thinking.type.adaptive"),
        (BEDROCK, "thinking.type.between_tools"),
    ];

    for (kind, accepted) in cases {
        let (status, body) = fixture(kind, "thinking-type-disabled-unsupported");
        let body = UpstreamBody::new(body);

        let named = parse_rejection(
            crate::router::FieldRepairTable::grounded_only(),
            kind,
            status,
            body.parsed(),
        )
        .unwrap_or_else(|| panic!("{kind}: the captured envelope names the grounded row"));

        assert_eq!(named.row.path, THINKING_DISABLED_PATH, "{kind}");
        assert_eq!(named.tier, SelfIdentifying, "{kind}");
        assert_eq!(named.kind, UnsupportedField, "{kind}");
        assert_eq!(named.value, Some(RejectionValue::Shape(accepted)), "{kind}");
    }
}

#[test]
fn each_thinking_disabled_fixture_names_its_one_replacement() {
    let cases = [
        (ANTHROPIC, "thinking.type.adaptive"),
        (BEDROCK, "thinking.type.between_tools"),
    ];

    for (kind, replacement) in cases {
        let (status, body) = fixture(kind, "thinking-type-disabled-unsupported");

        assert_eq!(
            replacements(kind, status, &body),
            Some(vec![replacement.to_string()]),
            "{kind}",
        );
    }
}

#[test]
fn the_thinking_off_switch_presupposes_the_refused_type_was_sent() {
    let (status, body) = fixture(BEDROCK, "thinking-type-disabled-unsupported");
    let body = UpstreamBody::new(body);

    let rejected = rejected_wire_path(BEDROCK, status, body.parsed()).expect("localized");

    assert!(rejected.presupposes_presence);
    assert!(
        rejected
            .field
            .segments()
            .eq(THINKING_DISABLED_PATH.split('.'))
    );
}

#[test]
fn a_converse_wrapped_thinking_off_switch_localizes_like_the_bare_one() {
    let bare = bedrock_body(THINKING_SWITCH_MESSAGE);
    let wrapped = bedrock_body(&format!("{CONVERSE_WRAPPER}{THINKING_SWITCH_MESSAGE}"));
    let captured: Value =
        serde_json::from_str(&fixture(BEDROCK, "thinking-type-disabled-unsupported").1).unwrap();
    assert!(
        captured["message"]
            .as_str()
            .unwrap()
            .starts_with(THINKING_SWITCH_MESSAGE),
        "premise: the template constant is the committed capture's message",
    );
    assert_eq!(
        outcome(BEDROCK, 400, &bare),
        Outcome::Row(THINKING_DISABLED_PATH, SelfIdentifying, UnsupportedField),
        "premise: the bare template localizes",
    );

    assert_eq!(
        outcome(BEDROCK, 400, &wrapped),
        outcome(BEDROCK, 400, &bare)
    );
    assert_eq!(
        replacements(BEDROCK, 400, &wrapped),
        replacements(BEDROCK, 400, &bare),
    );
}

#[test]
fn thinking_off_switch_near_misses_localize_nothing() {
    let exact = THINKING_SWITCH_MESSAGE;
    let swap = |from: &str, to: &str| {
        assert!(exact.contains(from), "premise: {from:?} is in the template");
        exact.replacen(from, to, 1)
    };
    let cases = [
        (
            "altered lead-in",
            BEDROCK,
            400,
            bedrock_body(&swap("turn thinking off", "turn thinking of")),
        ),
        (
            "lead-in not at the start",
            BEDROCK,
            400,
            bedrock_body(&format!("Error. {exact}")),
        ),
        (
            "accepted token outside the closed set",
            BEDROCK,
            400,
            bedrock_body(&swap("between_tools", "between_turns")),
        ),
        (
            "refused token outside the closed set",
            BEDROCK,
            400,
            bedrock_body(&swap("\"disabled\"}", "\"off\"}")),
        ),
        (
            "swapped closing brace and quote",
            BEDROCK,
            400,
            bedrock_body(&swap("\"disabled\"}.", "\"disabled}\".")),
        ),
        (
            "missing close",
            BEDROCK,
            400,
            bedrock_body(&swap("\"disabled\"}.", "\"disabled\"")),
        ),
        (
            "escaped quote inside a token",
            BEDROCK,
            400,
            bedrock_body(&swap("between_tools", "between_tools\\")),
        ),
        (
            "extra whitespace",
            BEDROCK,
            400,
            bedrock_body(&swap("{\"type\": \"between", "{\"type\":  \"between")),
        ),
        ("status 500", BEDROCK, 500, bedrock_body(exact)),
        (
            "openai-compat kind",
            OPENAI_COMPAT,
            400,
            openai_body(exact, None),
        ),
    ];
    assert_eq!(
        outcome(BEDROCK, 400, &bedrock_body(exact)),
        Outcome::Row(THINKING_DISABLED_PATH, SelfIdentifying, UnsupportedField),
        "premise: the same harness localizes the exact template",
    );

    for (name, kind, status, body) in cases {
        assert_eq!(
            outcome(kind, status, &body),
            Outcome::NotLocalized,
            "{name}"
        );
    }
}

#[test]
fn the_thinking_off_switch_returns_only_table_entries() {
    let body = UpstreamBody::new(bedrock_body(THINKING_SWITCH_MESSAGE));
    let body_bytes = body.text().as_bytes().as_ptr_range();
    let entry = |token: &str| {
        THINKING_TYPES
            .iter()
            .find(|known| known.token == token)
            .unwrap()
    };

    let localized = localize(BEDROCK, 400, body.parsed().unwrap()).expect("localized");

    assert!(std::ptr::eq(localized.raw, entry("disabled").path));
    let [replacement] = localized.replacements[..] else {
        panic!("one replacement");
    };
    assert!(std::ptr::eq(replacement, entry("between_tools").path));
    for returned in [localized.raw, replacement] {
        assert!(!body_bytes.contains(&returned.as_ptr()), "{returned}");
    }
}

/// The Bedrock temperature refusal exactly as the committed capture spells it.
const TEMPERATURE_MESSAGE: &str =
    "This model doesn't support the temperature field. Remove temperature and try again.";

#[test]
fn the_bedrock_temperature_template_names_the_row_flat_and_converse_wrapped() {
    let captured: Value =
        serde_json::from_str(&fixture(BEDROCK, "temperature-field-unsupported").1).unwrap();
    assert_eq!(
        captured["message"].as_str(),
        Some(TEMPERATURE_MESSAGE),
        "premise: the template constant is the committed capture's message",
    );
    let row = Outcome::Row(TEMPERATURE_PATH, SelfIdentifying, UnsupportedField);
    let cases = [
        ("flat", bedrock_body(TEMPERATURE_MESSAGE)),
        (
            "converse-wrapped",
            bedrock_body(&format!("{CONVERSE_WRAPPER}{TEMPERATURE_MESSAGE}")),
        ),
    ];

    for (name, body) in cases {
        assert_eq!(outcome(BEDROCK, 400, &body), row, "{name}");
        let parsed = UpstreamBody::new(body);
        let rejected = rejected_wire_path(BEDROCK, 400, parsed.parsed()).expect("localized");
        assert!(rejected.presupposes_presence, "{name}");
        assert!(rejected.field.segments().eq([TEMPERATURE_PATH]), "{name}");
        assert_eq!(
            replacements(BEDROCK, 400, parsed.text()),
            Some(Vec::new()),
            "{name}: a drop names no replacement",
        );
    }
}

#[test]
fn bedrock_temperature_near_misses_localize_nothing() {
    let exact = TEMPERATURE_MESSAGE;
    let swap = |from: &str, to: &str| {
        assert!(exact.contains(from), "premise: {from:?} is in the template");
        exact.replace(from, to)
    };
    let cases = [
        (
            "another field name",
            BEDROCK,
            bedrock_body(&swap("temperature", "top_p")),
        ),
        (
            "missing tail sentence",
            BEDROCK,
            bedrock_body(&swap(" Remove temperature and try again.", "")),
        ),
        (
            "case change",
            BEDROCK,
            bedrock_body(&swap("This model", "this model")),
        ),
        (
            "extra prefix other than the Converse wrapper",
            BEDROCK,
            bedrock_body(&format!("Validation failed. {exact}")),
        ),
        (
            "trailing prose",
            BEDROCK,
            bedrock_body(&format!("{exact} See the docs.")),
        ),
        (
            "same message on anthropic-api",
            ANTHROPIC,
            anthropic_body(exact),
        ),
        (
            "same flat body on anthropic-api",
            ANTHROPIC,
            bedrock_body(exact),
        ),
        (
            "same message on openai-compat",
            OPENAI_COMPAT,
            openai_body(exact, None),
        ),
    ];
    assert_eq!(
        outcome(BEDROCK, 400, &bedrock_body(exact)),
        Outcome::Row(TEMPERATURE_PATH, SelfIdentifying, UnsupportedField),
        "premise: the same harness localizes the exact template",
    );

    for (name, kind, body) in cases {
        assert_eq!(outcome(kind, 400, &body), Outcome::NotLocalized, "{name}");
    }
}

#[test]
fn the_bedrock_temperature_arm_returns_no_body_bytes() {
    let body = UpstreamBody::new(bedrock_body(TEMPERATURE_MESSAGE));
    let body_bytes = body.text().as_bytes().as_ptr_range();

    let localized = localize(BEDROCK, 400, body.parsed().unwrap()).expect("localized");

    assert_eq!(localized.raw, TEMPERATURE_PATH);
    assert!(!body_bytes.contains(&localized.raw.as_ptr()));
}

/// The Bedrock output-limit refusal exactly as the committed capture spells it.
const CEILING_MESSAGE: &str = "The maximum tokens you requested exceeds the model limit of 8192. \
     Try again with a maximum tokens value that is lower than 8192.";

/// The row path and value `parse_rejection` names for `body` on `kind`, or
/// `None` when it names no row.
fn named_value(kind: &str, body: &str) -> Option<(&'static str, Option<RejectionValue>)> {
    let body = UpstreamBody::new(body);
    parse_rejection(
        crate::router::FieldRepairTable::default(),
        kind,
        400,
        body.parsed(),
    )
    .map(|named| (named.row.path, named.value))
}

/// Mutation check: drop the number capture from the Converse ceiling arm ->
/// red on the Converse fixture; drop the Messages ceiling arm -> red on the
/// Invoke and Messages fixtures (each resolves its row with no value).
#[test]
fn every_committed_max_tokens_fixture_carries_the_ceiling_its_template_names() {
    let cases = [
        (
            BEDROCK,
            "max-tokens-above-model-limit",
            Some(RejectionValue::Ceiling(8192)),
        ),
        (
            BEDROCK,
            "max-tokens-above-model-limit-invoke",
            Some(RejectionValue::Ceiling(64_000)),
        ),
        (
            ANTHROPIC,
            "max-tokens-above-model-limit-messages",
            Some(RejectionValue::Ceiling(128_000)),
        ),
    ];

    for (kind, case, expected) in cases {
        let (status, body) = fixture(kind, case);
        assert_eq!(status, 400, "{case}");

        assert_eq!(
            named_value(kind, &body),
            Some((MAX_TOKENS_PATH, expected)),
            "{kind}/{case}"
        );
    }
}

#[test]
fn the_bedrock_ceiling_template_names_the_row_flat_and_converse_wrapped() {
    let captured: Value =
        serde_json::from_str(&fixture(BEDROCK, "max-tokens-above-model-limit").1).unwrap();
    assert_eq!(
        captured["message"].as_str(),
        Some(CEILING_MESSAGE),
        "premise: the template constant is the committed capture's message",
    );
    let other_limit = CEILING_MESSAGE.replace("8192", "4096");
    let cases = [
        ("flat", bedrock_body(CEILING_MESSAGE), 8192),
        (
            "converse-wrapped",
            bedrock_body(&format!("{CONVERSE_WRAPPER}{CEILING_MESSAGE}")),
            8192,
        ),
        ("another limit", bedrock_body(&other_limit), 4096),
    ];

    for (name, body, limit) in cases {
        assert_eq!(
            outcome(BEDROCK, 400, &body),
            Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, InvalidValue),
            "{name}",
        );
        assert_eq!(
            named_value(BEDROCK, &body),
            Some((MAX_TOKENS_PATH, Some(RejectionValue::Ceiling(limit)))),
            "{name}",
        );
        let parsed = UpstreamBody::new(body);
        let rejected = rejected_wire_path(BEDROCK, 400, parsed.parsed()).expect("localized");
        assert!(rejected.presupposes_presence, "{name}");
        assert!(rejected.field.segments().eq([MAX_TOKENS_PATH]), "{name}");
    }
}

#[test]
fn bedrock_ceiling_near_misses_localize_nothing() {
    let exact = CEILING_MESSAGE;
    let second = |to: &str| exact.replacen("lower than 8192", &format!("lower than {to}"), 1);
    let both = |to: &str| exact.replace("8192", to);
    let cases = [
        ("mismatched limits", BEDROCK, bedrock_body(&second("8191"))),
        ("zero", BEDROCK, bedrock_body(&both("0"))),
        ("leading zero", BEDROCK, bedrock_body(&both("08192"))),
        ("plus sign", BEDROCK, bedrock_body(&both("+8192"))),
        ("minus sign", BEDROCK, bedrock_body(&both("-8192"))),
        ("overflow", BEDROCK, bedrock_body(&both("4294967296"))),
        ("non-digit", BEDROCK, bedrock_body(&both("8k"))),
        ("empty limit", BEDROCK, bedrock_body(&both(""))),
        ("grouped digits", BEDROCK, bedrock_body(&both("8,192"))),
        (
            "context-window message",
            BEDROCK,
            bedrock_body(&format!(
                "{CONVERSE_WRAPPER}Input is too long for requested model."
            )),
        ),
        (
            "wrong prose",
            BEDROCK,
            bedrock_body(&exact.replace("exceeds the model limit", "is above the model limit")),
        ),
        (
            "missing final period",
            BEDROCK,
            bedrock_body(exact.strip_suffix('.').unwrap()),
        ),
        (
            "trailing prose",
            BEDROCK,
            bedrock_body(&format!("{exact} See the docs.")),
        ),
        (
            "same message on anthropic-api",
            ANTHROPIC,
            anthropic_body(exact),
        ),
        (
            "same message on openai-compat",
            OPENAI_COMPAT,
            openai_body(exact, None),
        ),
    ];
    assert_eq!(
        outcome(BEDROCK, 400, &bedrock_body(exact)),
        Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, InvalidValue),
        "premise: the same harness localizes the exact template",
    );
    assert_eq!(
        u32::MAX.checked_add(1).map(|n| n.to_string()),
        None,
        "premise: the overflow row is one past u32",
    );

    for (name, kind, body) in cases {
        assert_eq!(outcome(kind, 400, &body), Outcome::NotLocalized, "{name}");
    }
}

/// The openai-compat structured refusal of `max_tokens` refuses the field
/// itself and names no number, so it resolves the row with no value.
#[test]
fn an_openai_compat_unsupported_max_tokens_resolves_the_row_with_no_value() {
    let body = openai_coded_body(
        "Unsupported parameter: 'max_tokens' is not supported with this model.",
        Some(MAX_TOKENS_PATH),
        "unsupported_parameter",
    );

    assert_eq!(
        outcome(OPENAI_COMPAT, 400, &body),
        Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, UnsupportedField),
    );
    assert_eq!(
        named_value(OPENAI_COMPAT, &body),
        Some((MAX_TOKENS_PATH, None))
    );
}

/// The Messages-body ceiling refusal exactly as the committed anthropic-api
/// capture spells it.
const MESSAGES_CEILING_MESSAGE: &str = "max_tokens: 1000000 > 128000, which is the maximum \
     allowed number of output tokens for claude-haiku-5-5";
/// The same template as the committed Bedrock Invoke capture spells it.
const INVOKE_CEILING_MESSAGE: &str = "max_tokens: 1000000 > 64000, which is the maximum \
     allowed number of output tokens for anthropic.claude-haiku-4-5-20251001-v1:0";

#[test]
fn the_messages_ceiling_template_names_the_row_on_anthropic_api_and_invoke() {
    let captured = |kind: &str, case: &str, pointer: &str| {
        let body: Value = serde_json::from_str(&fixture(kind, case).1).unwrap();
        body.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    assert_eq!(
        captured(
            ANTHROPIC,
            "max-tokens-above-model-limit-messages",
            "/error/message"
        )
        .as_deref(),
        Some(MESSAGES_CEILING_MESSAGE),
        "premise: the Messages constant is the committed capture's message",
    );
    assert_eq!(
        captured(BEDROCK, "max-tokens-above-model-limit-invoke", "/message").as_deref(),
        Some(INVOKE_CEILING_MESSAGE),
        "premise: the Invoke constant is the committed capture's message",
    );
    let other_pair = MESSAGES_CEILING_MESSAGE
        .replace("1000000", "9000")
        .replace("128000", "8192");
    let cases = [
        (
            "messages",
            ANTHROPIC,
            anthropic_body(MESSAGES_CEILING_MESSAGE),
            128_000,
        ),
        (
            "messages, another pair",
            ANTHROPIC,
            anthropic_body(&other_pair),
            8192,
        ),
        (
            "invoke",
            BEDROCK,
            bedrock_body(INVOKE_CEILING_MESSAGE),
            64_000,
        ),
        (
            "invoke, another pair",
            BEDROCK,
            bedrock_body(&other_pair),
            8192,
        ),
        (
            "one below the value sent",
            ANTHROPIC,
            anthropic_body(&MESSAGES_CEILING_MESSAGE.replace("1000000", "128001")),
            128_000,
        ),
    ];

    for (name, kind, body, limit) in cases {
        assert_eq!(
            outcome(kind, 400, &body),
            Outcome::Row(MAX_TOKENS_PATH, SelfIdentifying, InvalidValue),
            "{name}",
        );
        assert_eq!(
            named_value(kind, &body),
            Some((MAX_TOKENS_PATH, Some(RejectionValue::Ceiling(limit)))),
            "{name}",
        );
        let parsed = UpstreamBody::new(body);
        let rejected = rejected_wire_path(kind, 400, parsed.parsed()).expect("localized");
        assert!(rejected.presupposes_presence, "{name}");
        assert!(rejected.field.segments().eq([MAX_TOKENS_PATH]), "{name}");
    }
}

/// A near-miss of the Messages ceiling template names no ceiling. Its
/// `max_tokens: ` lead-in still reads as the location prefix, an inferred
/// value refusal carrying no value, on which the ceiling row makes no edit.
#[test]
fn messages_ceiling_near_misses_name_no_ceiling() {
    let exact = MESSAGES_CEILING_MESSAGE;
    let sent = |to: &str| exact.replacen("1000000", to, 1);
    let limit = |to: &str| exact.replacen("128000", to, 1);
    let cases = [
        ("limit equal to the value sent", limit("1000000")),
        ("limit above the value sent", limit("2000000")),
        ("zero limit", limit("0")),
        ("leading zero in the limit", limit("0128000")),
        ("leading zero in the value sent", sent("01000000")),
        ("plus sign on the limit", limit("+128000")),
        ("minus sign on the value sent", sent("-1000000")),
        ("limit overflow", limit("4294967296")),
        ("value sent overflow", sent("4294967296")),
        ("non-digit limit", limit("128k")),
        ("empty limit", limit("")),
        ("empty value sent", sent("")),
        ("grouped digits", limit("128,000")),
        ("other comparison", exact.replace(" > ", " >= ")),
        (
            "wrong prose",
            exact.replace("maximum allowed number", "maximum number"),
        ),
        (
            "empty model",
            exact.strip_suffix("claude-haiku-5-5").unwrap().to_string(),
        ),
        ("trailing prose", format!("{exact}. See the docs.")),
    ];
    for kind in [ANTHROPIC, BEDROCK] {
        let wrap = |message: &str| {
            if kind == ANTHROPIC {
                anthropic_body(message)
            } else {
                bedrock_body(message)
            }
        };
        assert_eq!(
            named_value(kind, &wrap(exact)),
            Some((MAX_TOKENS_PATH, Some(RejectionValue::Ceiling(128_000)))),
            "premise: {kind} reads the ceiling from the exact template",
        );

        for (name, message) in &cases {
            let body = wrap(message);
            assert_eq!(
                outcome(kind, 400, &body),
                Outcome::Row(MAX_TOKENS_PATH, Inferred, InvalidValue),
                "{kind}: {name}",
            );
            assert_eq!(
                named_value(kind, &body),
                Some((MAX_TOKENS_PATH, None)),
                "{kind}: {name}",
            );
        }
    }
    assert_eq!(
        outcome(OPENAI_COMPAT, 400, &openai_body(exact, None)),
        Outcome::NotLocalized,
        "same message on openai-compat",
    );
}
