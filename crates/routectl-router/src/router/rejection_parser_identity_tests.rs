//! Pins the row-less field identity: it localizes exactly what the wire-path
//! read localizes, carries the same tier `parse_rejection` names when a row
//! exists, and folds a thinking type value so a refused rewrite reads as the
//! field it rewrote.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use routectl_core::UpstreamBody;
use routectl_core::capability::SignalTier;
use serde_json::{Value, json};

use super::super::field_repair::{
    FieldRepairRow, FieldRepairTable, FieldTransform, grounded_table,
};
use super::{FieldIdentity, LocalizedField, localized_field, parse_rejection, rejected_wire_path};

use SignalTier::{Inferred, SelfIdentifying};

const ANTHROPIC: &str = "anthropic-api";
const BEDROCK: &str = "bedrock";
const OPENAI_COMPAT: &str = "openai-compat";
const OPENAI_RESPONSES: &str = "openai-responses";

const CONVERSE_WRAPPER: &str = "The model returned the following errors: ";
const BEDROCK_CAPTURE: &str = include_str!("../../tests/fixtures/bedrock_validation_capture.json");

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

fn off_switch(accepted: &str, refused: &str) -> String {
    format!(
        "To turn thinking off on this model, send \"thinking\": {{\"type\": \"{accepted}\"}} \
         instead of {{\"type\": \"{refused}\"}}."
    )
}

fn field(kind: &str, status: u16, body: &str) -> Option<LocalizedField> {
    localized_field(kind, status, UpstreamBody::new(body).parsed())
}

fn row(path: &str) -> &'static FieldRepairRow {
    FieldRepairTable::default()
        .row_for_path(path)
        .unwrap_or_else(|| panic!("no row {path}"))
}

/// Every committed `rejections/<kind>/<case>.json` fixture and every Bedrock
/// validation canary, as `(name, kind, status, body)`.
fn corpus() -> Vec<(String, String, u16, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rejections");
    let mut cases = Vec::new();
    for kind_dir in std::fs::read_dir(dir).expect("rejections dir") {
        let kind_dir = kind_dir.expect("dir entry").path();
        let kind = kind_dir.file_name().unwrap().to_str().unwrap().to_string();
        for file in std::fs::read_dir(&kind_dir).expect("kind dir") {
            let file = file.expect("file entry").path();
            let fixture: Value =
                serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let status = u16::try_from(fixture["status"].as_u64().unwrap()).unwrap();
            let body = fixture["body"].as_str().unwrap().to_string();
            cases.push((file.display().to_string(), kind.clone(), status, body));
        }
    }
    let capture: Value = serde_json::from_str(BEDROCK_CAPTURE).unwrap();
    for (name, canary) in capture["canaries"].as_object().unwrap() {
        let status = u16::try_from(canary["status"].as_u64().unwrap()).unwrap();
        let body = canary["body"].as_str().unwrap().to_string();
        cases.push((name.clone(), BEDROCK.to_string(), status, body));
    }
    cases
}

#[test]
fn each_localize_arm_yields_a_field_exactly_when_the_wire_path_read_does() {
    let quoted_grounded = format!(
        "{CONVERSE_WRAPPER}\"thinking.enabled.display\" is not supported for this model. Use \
         \"thinking.type.adaptive\" instead."
    );
    let cases: Vec<(&str, &str, u16, String, Option<SignalTier>)> = vec![
        (
            "grounded location prefix",
            ANTHROPIC,
            400,
            anthropic_body("thinking.enabled.display: Input should be 'summarized', 'omitted'"),
            Some(SelfIdentifying),
        ),
        (
            "prose location prefix",
            ANTHROPIC,
            422,
            anthropic_body("thinking.enabled.display: is not available for this request"),
            Some(Inferred),
        ),
        (
            "location prefix ending in an index",
            ANTHROPIC,
            400,
            anthropic_body("tools.0: Extra inputs are not permitted"),
            Some(SelfIdentifying),
        ),
        (
            "grounded quoted template",
            BEDROCK,
            400,
            bedrock_body(&quoted_grounded),
            Some(SelfIdentifying),
        ),
        (
            "prose quoted not-supported",
            ANTHROPIC,
            400,
            anthropic_body("\"thinking.enabled.display\" is not supported on this lane"),
            Some(Inferred),
        ),
        (
            "thinking off-switch",
            BEDROCK,
            400,
            bedrock_body(&off_switch("between_tools", "disabled")),
            Some(SelfIdentifying),
        ),
        (
            "messages ceiling",
            ANTHROPIC,
            400,
            anthropic_body(
                "max_tokens: 64001 > 64000, which is the maximum allowed number of output \
                 tokens for some-model",
            ),
            Some(SelfIdentifying),
        ),
        (
            "bedrock temperature",
            BEDROCK,
            400,
            bedrock_body(
                "This model doesn't support the temperature field. Remove temperature and try \
                 again.",
            ),
            Some(SelfIdentifying),
        ),
        (
            "bedrock ceiling",
            BEDROCK,
            400,
            bedrock_body(
                "The maximum tokens you requested exceeds the model limit of 8192. Try again \
                 with a maximum tokens value that is lower than 8192.",
            ),
            Some(SelfIdentifying),
        ),
        (
            "openai structured param",
            OPENAI_COMPAT,
            400,
            openai_body("anything at all", Some("top_p")),
            Some(SelfIdentifying),
        ),
        (
            "openai unsupported-parameter template",
            OPENAI_COMPAT,
            400,
            openai_body(
                "Unsupported parameter: 'top_p' is not supported with this model.",
                None,
            ),
            Some(SelfIdentifying),
        ),
        (
            "status that carries no field rejection",
            ANTHROPIC,
            500,
            anthropic_body("temperature: Extra inputs are not permitted"),
            None,
        ),
        (
            "responses structured effort param",
            OPENAI_RESPONSES,
            400,
            openai_body("anything at all", Some("reasoning.effort")),
            Some(SelfIdentifying),
        ),
        (
            "responses structured param other than the effort one",
            OPENAI_RESPONSES,
            400,
            openai_body("anything at all", Some("top_p")),
            None,
        ),
        (
            "kind without a parser",
            "gemini",
            400,
            openai_body("anything at all", Some("top_p")),
            None,
        ),
        (
            "unparseable body",
            ANTHROPIC,
            400,
            "not json".to_string(),
            None,
        ),
        (
            "prose naming no path",
            ANTHROPIC,
            400,
            anthropic_body("Something about temperature went wrong"),
            None,
        ),
        (
            "off-switch with an unknown type token",
            BEDROCK,
            400,
            bedrock_body(&off_switch("between_tools", "sideways")),
            None,
        ),
        (
            "non-ascii path",
            ANTHROPIC,
            400,
            anthropic_body("temp\u{e9}rature: Extra inputs are not permitted"),
            None,
        ),
    ];

    for (name, kind, status, body, expected_tier) in cases {
        let parsed = UpstreamBody::new(body.clone());
        assert_eq!(
            field(kind, status, &body).map(|field| field.tier),
            expected_tier,
            "{name}"
        );
        assert_eq!(
            rejected_wire_path(kind, status, parsed.parsed()).is_some(),
            expected_tier.is_some(),
            "{name}: wire-path parity"
        );
    }
}

#[test]
fn every_committed_rejection_localizes_a_field_exactly_when_the_wire_path_read_does() {
    let mut localized = 0;
    for (name, kind, status, body) in corpus() {
        let parsed = UpstreamBody::new(body.clone());
        let wire = rejected_wire_path(&kind, status, parsed.parsed()).is_some();
        assert_eq!(field(&kind, status, &body).is_some(), wire, "{name}");
        localized += usize::from(wire);
    }
    assert!(localized > 0, "the corpus localizes at least one rejection");
}

/// Every rejection that resolves to a row localizes that row's identity with
/// the same tier. Each grounded row is reached: a row with a field of its own
/// as a resolved row, a ranked row (whose last segment is a level the sent
/// request names, not the envelope) through its folded identity, localized
/// in every lane kind that spells the effort field.
#[test]
fn a_rejection_with_a_row_carries_the_row_tier_and_folds_onto_the_row_identity() {
    let mut reached = BTreeSet::new();
    let mut localized: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, kind, status, body) in corpus() {
        if let Some(field) = field(&kind, status, &body) {
            localized
                .entry(format!("{:?}", field.identity.0))
                .or_default()
                .insert(kind.clone());
        }
        let parsed = UpstreamBody::new(body.clone());
        let Some(named) = parse_rejection(
            FieldRepairTable::default(),
            &kind,
            crate::router::field_repair::direct_egress(&kind),
            status,
            parsed.parsed(),
            crate::router::learn_engine::SentEffort::Unknown,
        ) else {
            continue;
        };
        let field = field(&kind, status, &body).expect("a resolved row localizes a field");
        assert_eq!(field.tier, named.tier, "{name}");
        assert_eq!(field.identity, FieldIdentity::of_row(named.row), "{name}");
        reached.insert(named.row.path);
    }
    let effort_spellings = BTreeSet::from([
        ANTHROPIC.to_string(),
        BEDROCK.to_string(),
        OPENAI_RESPONSES.to_string(),
    ]);
    for row in grounded_table() {
        if matches!(row.transform, FieldTransform::RankedRewrite { .. }) {
            let identity = format!("{:?}", FieldIdentity::of_row(row).0);
            assert_eq!(
                localized.get(&identity),
                Some(&effort_spellings),
                "ranked row {} is localized in every effort spelling",
                row.path,
            );
        } else {
            assert!(
                reached.contains(row.path),
                "row {} reached by no capture",
                row.path
            );
        }
    }
}

#[test]
fn a_self_identifying_rejection_on_a_path_with_no_row_still_names_its_field() {
    let body = anthropic_body("zzz_two: Extra inputs are not permitted");
    let parsed = UpstreamBody::new(body.clone());
    assert!(
        parse_rejection(
            FieldRepairTable::default(),
            ANTHROPIC,
            crate::router::field_repair::direct_egress(ANTHROPIC),
            400,
            parsed.parsed(),
            crate::router::learn_engine::SentEffort::Unknown
        )
        .is_none(),
        "no row for the path"
    );

    let field = field(ANTHROPIC, 400, &body).expect("row-less field localizes");

    assert_eq!(field.tier, SelfIdentifying);
    assert_ne!(field.identity, FieldIdentity::of_row(row("temperature")));
}

#[test]
fn every_refused_thinking_type_shares_one_identity_with_the_type_rows() {
    let type_identity = FieldIdentity::of_row(row("thinking.type.disabled"));
    assert_eq!(
        FieldIdentity::of_row(row("thinking.type.enabled")),
        type_identity
    );
    for refused in ["disabled", "adaptive", "between_tools", "enabled"] {
        let body = bedrock_body(&off_switch("between_tools", refused));
        let field = field(BEDROCK, 400, &body).expect("off-switch localizes");
        assert_eq!(field.identity, type_identity, "refused {refused}");
    }
}

/// Every effort row folds onto the one field every lane's effort refusal
/// localizes, so a refusal of the replacement level reads as the field the
/// repair rewrote.
///
/// Mutation check: drop the ranked-row fold in `FieldIdentity::folded` ->
/// red.
#[test]
fn every_effort_row_shares_one_identity_with_the_effort_refusals() {
    let identity = FieldIdentity::of_row(row("output_config.effort.high"));
    let refusals = [
        (
            ANTHROPIC,
            anthropic_body(
                "This model does not support effort level 'xhigh'. Supported levels: high, low.",
            ),
        ),
        (
            BEDROCK,
            bedrock_body(&format!(
                "{CONVERSE_WRAPPER}This model does not support effort level 'xhigh'. Supported \
                 levels: high, low."
            )),
        ),
        (
            OPENAI_RESPONSES,
            openai_body(
                "Invalid value: 'bogus'. Supported values are: 'low' and 'high'.",
                Some("reasoning.effort"),
            ),
        ),
    ];
    for level in routectl_providers::effort::VALID_EFFORT_TOKENS {
        let path = format!("output_config.effort.{level}");
        assert_eq!(FieldIdentity::of_row(row(&path)), identity, "{path}");
    }
    for (kind, body) in refusals {
        let field = field(kind, 400, &body).expect("the effort refusal localizes");
        assert_eq!(field.identity, identity, "{kind}");
    }
}

#[test]
fn distinct_fields_keep_distinct_identities() {
    let identity = |path| FieldIdentity::of_row(row(path));
    let cases = [
        (
            "display vs thinking type",
            "thinking.enabled.display",
            "thinking.type.enabled",
        ),
        ("temperature vs top_p", "temperature", "top_p"),
        (
            "presence vs frequency penalty",
            "presence_penalty",
            "frequency_penalty",
        ),
        ("max_tokens vs temperature", "max_tokens", "temperature"),
        (
            "effort vs thinking type",
            "output_config.effort.high",
            "thinking.type.enabled",
        ),
        (
            "effort vs max_tokens",
            "output_config.effort.high",
            "max_tokens",
        ),
    ];
    for (name, left, right) in cases {
        assert_ne!(identity(left), identity(right), "{name}");
    }
}

#[test]
fn a_field_identity_debug_prints_no_segment_text() {
    let body = anthropic_body("secret_client_key: Extra inputs are not permitted");
    let field = field(ANTHROPIC, 400, &body).expect("localizes");

    let rendered = format!("{field:?}");

    assert!(!rendered.contains("secret_client_key"), "{rendered}");
    assert!(rendered.contains("FieldIdentity(1 segments)"), "{rendered}");
}
