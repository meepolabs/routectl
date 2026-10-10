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
use super::{LocalizedPath, localize, parse_rejection};

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

use SignalTier::{Inferred, SelfIdentifying};

/// What the parser makes of one captured envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// Resolved to the closed-table row with this path.
    Row(&'static str, SignalTier),
    /// Localized a path (shown canonicalized) that has no row.
    Unresolved(String, SignalTier),
    /// No arm localized a path.
    NotLocalized,
}

fn outcome(kind: &str, status: u16, body: &str) -> Outcome {
    let body = UpstreamBody::new(body);
    if let Some(named) = parse_rejection(kind, status, body.parsed()) {
        return Outcome::Row(named.row.path, named.tier);
    }
    match body.parsed().and_then(|p| localize(kind, status, p)) {
        Some(LocalizedPath { segments, tier }) => Outcome::Unresolved(segments.join("."), tier),
        None => Outcome::NotLocalized,
    }
}

fn unresolved(path: &str, tier: SignalTier) -> Outcome {
    Outcome::Unresolved(path.to_string(), tier)
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

fn rejections_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rejections")
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
            "anthropic-api/thinking-display-enum",
            Outcome::Row(DISPLAY_PATH, SelfIdentifying),
        ),
        (
            "bedrock/thinking-type-enabled-unsupported",
            unresolved("thinking.type.enabled", SelfIdentifying),
        ),
        (
            "openai-compat/unsupported-parameter-temperature",
            unresolved("temperature", SelfIdentifying),
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
            unresolved("thinking.adaptive.routectl_probe_garbage", SelfIdentifying),
        ),
        ("converse-web-search-composed", Outcome::NotLocalized),
        (
            "schema-error-converse-sonnet-4-6",
            unresolved("output_config.format.schema", Inferred),
        ),
        (
            "schema-error-invoke-sonnet-4-6",
            unresolved("output_config.format.schema", Inferred),
        ),
        (
            "structured-output-converse-opus-5-5",
            unresolved("output_config.format", SelfIdentifying),
        ),
        (
            "structured-output-invoke-opus-5-5",
            unresolved("output_config.format", SelfIdentifying),
        ),
        ("supp-bad-model-id", Outcome::NotLocalized),
        ("tool-changes-sonnet-5", Outcome::NotLocalized),
        (
            "tool-removal-messages-opus-5-5",
            unresolved("messages.content", Inferred),
        ),
        (
            "toolset-name-opus-5-5",
            unresolved("tools.computer_toolset_20260801", Inferred),
        ),
        ("unknown-beta", Outcome::NotLocalized),
        (
            "unknown-body-field",
            unresolved("routectl_envelope_probe_field", SelfIdentifying),
        ),
        (
            "web-search-pydantic-sonnet-4-6",
            unresolved("tools", Inferred),
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

    assert_eq!(reached, unresolved("tool_choice", Inferred));
}

#[test]
fn every_grounded_row_is_reached_by_a_committed_fixture() {
    let reached: Vec<&'static str> = corpus_outcomes()
        .into_values()
        .chain(bedrock_canary_outcomes().into_values())
        .filter_map(|outcome| match outcome {
            Outcome::Row(path, _) => Some(path),
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
            parse_rejection(kind, 400, parsed.parsed()).is_none(),
            "near miss {name} resolved to a row",
        );
        checked += 1;
    }

    assert!(checked > 20, "premise: both near-miss sets were read");
    let control = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));
    assert!(
        parse_rejection(ANTHROPIC, 400, control.parsed()).is_some(),
        "the same harness resolves a grounded message",
    );
}

#[test]
fn each_arm_assigns_its_tier() {
    let cases = [
        (
            "grounded location prefix",
            ANTHROPIC,
            anthropic_body(DISPLAY_MESSAGE),
            SelfIdentifying,
        ),
        (
            "prose location prefix",
            ANTHROPIC,
            anthropic_body("thinking.enabled.display: is not available for this request"),
            Inferred,
        ),
        (
            "grounded quoted template",
            BEDROCK,
            bedrock_body(&format!(
                "{CONVERSE_WRAPPER}\"thinking.enabled.display\" is not supported for this \
                 model. Use \"thinking.type.adaptive\" instead."
            )),
            SelfIdentifying,
        ),
        (
            "prose quoted not-supported",
            ANTHROPIC,
            anthropic_body("\"thinking.enabled.display\" is not supported on this lane"),
            Inferred,
        ),
        (
            "structured param",
            OPENAI_RESPONSES,
            openai_body("anything at all", Some(DISPLAY_PATH)),
            SelfIdentifying,
        ),
        (
            "unsupported-parameter template",
            OPENAI_COMPAT,
            openai_body(
                "Unsupported parameter: 'thinking.enabled.display' is not supported with this \
                 model.",
                None,
            ),
            SelfIdentifying,
        ),
    ];

    for (name, kind, body, tier) in cases {
        let body = UpstreamBody::new(body);
        let named = parse_rejection(kind, 400, body.parsed())
            .unwrap_or_else(|| panic!("{name}: resolves the display row"));
        assert_eq!(named.row.path, DISPLAY_PATH, "{name}");
        assert_eq!(named.tier, tier, "{name}");
        assert_eq!(named.accepted, None, "{name}");
    }
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
            parse_rejection(OPENAI_COMPAT, 400, body.parsed()).is_none(),
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

        let named = parse_rejection(kind, 400, body.parsed());

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

    assert!(parse_rejection(ANTHROPIC, 400, body.parsed()).is_none());
}

#[test]
fn a_non_ascii_or_over_long_param_resolves_to_none() {
    let index_tail = ".0".repeat(30);
    let over_long = format!("{DISPLAY_PATH}{index_tail}");
    assert!(over_long.len() > routectl_core::MAX_SAFE_TOKEN_LEN);
    let short_with_index = format!("{DISPLAY_PATH}.0");

    for param in ["thinking.enabled.displ\u{e9}y", over_long.as_str()] {
        let body = UpstreamBody::new(openai_body("rejected", Some(param)));

        assert!(
            parse_rejection(OPENAI_COMPAT, 400, body.parsed()).is_none(),
            "{param:?}",
        );
    }
    let control = UpstreamBody::new(openai_body("rejected", Some(&short_with_index)));
    assert!(
        parse_rejection(OPENAI_COMPAT, 400, control.parsed()).is_some(),
        "the same path, bounded, resolves",
    );
}

#[test]
fn the_named_row_is_a_table_entry_holding_no_body_bytes() {
    let body = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));
    let body_bytes = body.text().as_bytes().as_ptr_range();

    let named = parse_rejection(ANTHROPIC, 400, body.parsed()).expect("display row");

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

    assert!(parse_rejection(ANTHROPIC, 400, over_cap.parsed()).is_none());
    assert!(parse_rejection(ANTHROPIC, 400, None::<&ParsedErrorBody>).is_none());
}

#[test]
fn a_kind_or_status_without_a_parser_resolves_to_none() {
    let anthropic = UpstreamBody::new(anthropic_body(DISPLAY_MESSAGE));
    let openai = UpstreamBody::new(openai_body("rejected", Some(DISPLAY_PATH)));
    assert!(
        parse_rejection(ANTHROPIC, 422, anthropic.parsed()).is_some(),
        "premise: the body resolves on a parsed kind and rejection status",
    );

    assert!(parse_rejection("gemini", 400, anthropic.parsed()).is_none());
    assert!(parse_rejection("gemini", 400, openai.parsed()).is_none());
    assert!(parse_rejection(ANTHROPIC, 500, anthropic.parsed()).is_none());
    assert!(parse_rejection(ANTHROPIC, 429, anthropic.parsed()).is_none());
}

#[test]
fn an_anthropic_message_outside_an_invalid_request_error_resolves_to_none() {
    let body =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": DISPLAY_MESSAGE}});
    let body = UpstreamBody::new(body.to_string());

    assert!(parse_rejection(ANTHROPIC, 400, body.parsed()).is_none());
}
