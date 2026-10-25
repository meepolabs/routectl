//! The row scope keyed on a lane's egress: each direct egress carries its
//! kind's rows, each Mantle egress exactly its own allowlist, and an unknown
//! egress nothing, whether the rows are carried by a request or named by a
//! rejection.

use std::collections::BTreeSet;

use routectl_core::Error;
use routectl_core::failure_class::FailureClass;
use routectl_core::wire_check::EgressKind;

use super::super::Router;
use super::super::learn_engine::{EffortRank, SentEffort};
use super::effort::{
    ANTHROPIC_API_MANTLE_ROWS, OPENAI_COMPAT_MANTLE_ROWS, OPENAI_RESPONSES_MANTLE_ROWS,
};
use super::{FieldRepairTable, parse_rejection};

const ANTHROPIC_API: &str = "anthropic-api";
const BEDROCK: &str = "bedrock";
const OPENAI_COMPAT: &str = "openai-compat";
const OPENAI_RESPONSES: &str = "openai-responses";

/// The grounded rows that are not effort levels.
const NON_RANKED_ROWS: &[&str] = &[
    "thinking.enabled.display",
    "thinking.type.enabled",
    "thinking.type.disabled",
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "max_tokens",
];

/// The grounded effort-level rows.
const RANKED_ROWS: &[&str] = &[
    "output_config.effort.minimal",
    "output_config.effort.low",
    "output_config.effort.medium",
    "output_config.effort.high",
    "output_config.effort.xhigh",
    "output_config.effort.max",
];

/// The grounded rows' paths in the view of `kind` sending with `egress`.
fn lane_paths(kind: &str, egress: Option<EgressKind>) -> BTreeSet<&'static str> {
    FieldRepairTable::grounded_only()
        .for_lane(kind, egress)
        .lane_rows()
        .map(|row| row.path)
        .collect()
}

fn set(paths: &[&'static str]) -> BTreeSet<&'static str> {
    paths.iter().copied().collect()
}

/// Each direct egress reads its kind's rows, spelled as literal path sets so
/// a change to the derivation cannot be echoed back by the expectation.
///
/// Mutation check: make `RowScope::of_lane` answer `All` for the compat
/// egress (or the responses egress) -> red on that row.
#[test]
fn each_direct_egress_carries_its_kinds_literal_row_set() {
    let all: BTreeSet<&str> = NON_RANKED_ROWS.iter().chain(RANKED_ROWS).copied().collect();
    let cases = [
        (
            "anthropic-api",
            ANTHROPIC_API,
            EgressKind::AnthropicApi,
            all.clone(),
        ),
        (
            "bedrock invoke",
            BEDROCK,
            EgressKind::BedrockInvoke,
            all.clone(),
        ),
        (
            "bedrock converse",
            BEDROCK,
            EgressKind::BedrockConverse,
            all,
        ),
        (
            "openai-compat",
            OPENAI_COMPAT,
            EgressKind::OpenaiCompat,
            set(NON_RANKED_ROWS),
        ),
        (
            "openai-responses",
            OPENAI_RESPONSES,
            EgressKind::OpenaiResponses,
            set(RANKED_ROWS),
        ),
    ];

    assert_eq!(
        FieldRepairTable::grounded_only().rows().len(),
        NON_RANKED_ROWS.len() + RANKED_ROWS.len(),
        "premise: the literal sets name every grounded row",
    );
    for (name, kind, egress, expected) in cases {
        assert_eq!(lane_paths(kind, Some(egress)), expected, "{name}");
    }
}

/// Each Mantle egress reads exactly its own allowlist, never its direct
/// sibling's rows, and each allowlist is the literal set of rows a refusal
/// captured on that sub-lane grounds.
///
/// Mutation check: map a Mantle egress to its kind's scope in
/// `RowScope::of_lane` -> red on that sub-lane; add or drop a path in an
/// allowlist const -> red on that sub-lane.
#[test]
fn each_mantle_egress_carries_exactly_its_allowlist() {
    type Case<'a> = (&'a str, &'a str, EgressKind, &'a [&'a str], &'a [&'a str]);
    let cases: [Case; 3] = [
        (
            "anthropic-api Mantle",
            ANTHROPIC_API,
            EgressKind::AnthropicApiMantle,
            ANTHROPIC_API_MANTLE_ROWS,
            &["max_tokens"],
        ),
        (
            "openai-compat Mantle",
            OPENAI_COMPAT,
            EgressKind::OpenaiCompatMantle,
            OPENAI_COMPAT_MANTLE_ROWS,
            &["top_p", "presence_penalty", "frequency_penalty"],
        ),
        (
            "openai-responses Mantle",
            OPENAI_RESPONSES,
            EgressKind::OpenaiResponsesMantle,
            OPENAI_RESPONSES_MANTLE_ROWS,
            &["output_config.effort.minimal"],
        ),
    ];

    for (name, kind, egress, allowlist, literal) in cases {
        assert_eq!(set(allowlist), set(literal), "{name}: the allowlist");
        assert_eq!(lane_paths(kind, Some(egress)), set(literal), "{name}");
    }
}

/// A lane whose egress is unknown reads no row, whatever its kind: the scope
/// cannot tell a Mantle sub-lane from its direct sibling, so it admits
/// nothing rather than the sibling's rows.
///
/// Mutation check: answer the kind's scope for a `None` egress in
/// `RowScope::of_lane` -> red.
#[test]
fn an_unknown_egress_carries_no_row() {
    for kind in [ANTHROPIC_API, BEDROCK, OPENAI_COMPAT, OPENAI_RESPONSES] {
        assert!(lane_paths(kind, None).is_empty(), "{kind}");
    }
}

/// A local answer sent nothing upstream, so no row is carried on it or named
/// by a rejection there, whatever the kind.
///
/// Mutation check: map `EgressKind::Local` to the kind's scope in
/// `RowScope::of_lane` -> red.
#[test]
fn a_local_egress_carries_no_row() {
    for kind in [ANTHROPIC_API, BEDROCK, OPENAI_COMPAT, OPENAI_RESPONSES] {
        assert!(
            lane_paths(kind, Some(EgressKind::Local)).is_empty(),
            "{kind}"
        );
    }
}

/// The committed `rejections/<dir>/<case>.json` fixture as an upstream error
/// carrying its status and body only, without the `error.type` and
/// `error.code` tokens an egress lifts.
fn unlifted_fixture_error(dir: &str, case: &str) -> Error {
    let file = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rejections")
        .join(dir)
        .join(format!("{case}.json"));
    let fixture: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file).expect("fixture")).expect("json");
    let status = u16::try_from(fixture["status"].as_u64().expect("status")).expect("u16");
    Error::upstream("p", status, fixture["body"].as_str().expect("body"))
}

/// The row a fixture refusal names on `kind` sending with `egress`.
fn named_path(
    err: &Error,
    kind: &str,
    egress: Option<EgressKind>,
    sent: SentEffort,
) -> Option<&'static str> {
    let status = super::super::class_observe::upstream_facts(err)
        .status
        .expect("an upstream status");
    parse_rejection(
        FieldRepairTable::grounded_only(),
        kind,
        egress,
        status,
        err.parsed_body(),
        sent,
    )
    .map(|named| named.row.path)
}

/// One grammar per kind, one scope per egress: a refusal a direct lane
/// resolves to a row its Mantle sibling does not admit resolves to none on
/// that sibling, though both share the sibling's dialect.
///
/// Mutation check: map the Mantle egresses to their kind's scope in
/// `RowScope::of_lane` -> red on each Mantle row.
#[test]
fn a_refusal_names_its_row_on_the_direct_lane_and_none_on_its_mantle_sibling() {
    let compat = unlifted_fixture_error("openai-compat", "unsupported-parameter-temperature");
    let thinking = unlifted_fixture_error("anthropic-api", "thinking-type-disabled-unsupported");
    let cases = [
        (
            "compat sampling refusal",
            &compat,
            OPENAI_COMPAT,
            EgressKind::OpenaiCompat,
            EgressKind::OpenaiCompatMantle,
            SentEffort::Unknown,
            "temperature",
        ),
        (
            "anthropic thinking refusal",
            &thinking,
            ANTHROPIC_API,
            EgressKind::AnthropicApi,
            EgressKind::AnthropicApiMantle,
            SentEffort::Unknown,
            "thinking.type.disabled",
        ),
    ];

    for (name, err, kind, direct, mantle, sent, row) in cases {
        assert_eq!(
            named_path(err, kind, Some(direct), sent),
            Some(row),
            "{name}: the direct lane names its row",
        );
        assert_eq!(
            named_path(err, kind, Some(mantle), sent),
            None,
            "{name}: the Mantle sibling names none",
        );
    }
}

/// A refusal the Mantle scope resolves to no row is counted as one the
/// parser did not localize there, and the same refusal on the direct lane is
/// not: the blindness count reads the lane's own view. `temperature` is not
/// admitted on compat Mantle: the one temperature refusal captured there
/// arrives with the `unsupported_value` code, which the field class gate does
/// not pass on openai-compat, so it grounds nothing.
///
/// Mutation check: map the compat Mantle egress to its kind's scope in
/// `RowScope::of_lane` -> red on the Mantle assertion.
#[test]
fn a_compat_refusal_on_compat_mantle_counts_as_unlocalized() {
    let err = unlifted_fixture_error("openai-compat", "unsupported-parameter-temperature");
    let unlocalized = |egress| {
        Router::rejection_localizes_no_field(
            FieldRepairTable::grounded_only(),
            &FailureClass::BadRequest,
            &err,
            OPENAI_COMPAT,
            Some(egress),
        )
    };

    assert!(
        !unlocalized(EgressKind::OpenaiCompat),
        "control: the direct lane localizes it",
    );
    assert!(unlocalized(EgressKind::OpenaiCompatMantle));
}

/// Each admitted Mantle row is named on its sub-lane by the refusal captured
/// there.
///
/// Mutation check: empty an allowlist const -> red on that sub-lane's row.
#[test]
fn a_mantle_capture_names_its_admitted_row_on_its_own_sub_lane() {
    let minimal = SentEffort::Rank(EffortRank::of_token("minimal").expect("a rank member"));
    let cases = [
        (
            "anthropic Mantle ceiling",
            unlifted_fixture_error("anthropic-api", "max-tokens-above-model-limit-mantle"),
            ANTHROPIC_API,
            EgressKind::AnthropicApiMantle,
            SentEffort::Unknown,
            "max_tokens",
        ),
        (
            "compat Mantle sampling drop",
            unlifted_fixture_error("openai-compat", "unsupported-parameter-top-p-mantle"),
            OPENAI_COMPAT,
            EgressKind::OpenaiCompatMantle,
            SentEffort::Unknown,
            "top_p",
        ),
        (
            "responses Mantle ranked effort",
            unlifted_fixture_error(
                "openai-responses",
                "reasoning-effort-minimal-unsupported-mantle",
            ),
            OPENAI_RESPONSES,
            EgressKind::OpenaiResponsesMantle,
            minimal,
            "output_config.effort.minimal",
        ),
    ];

    for (name, err, kind, egress, sent, row) in cases {
        assert_eq!(
            named_path(&err, kind, Some(egress), sent),
            Some(row),
            "{name}"
        );
    }
}

/// The temperature refusal captured on compat Mantle, with the `error.type`
/// and `error.code` tokens the egress lifts, classifies as
/// `FeatureUnsupported` with the `unsupported_value` code on openai-compat,
/// which the field class gate does not pass, so it names no row on compat
/// Mantle or on direct compat. The same envelope with the
/// `unsupported_parameter` code names the row on the direct lane, the
/// positive control.
///
/// Mutation check: admit `unsupported_value` on openai-compat in
/// `class_can_name_a_field` -> red on the captured envelope's direct-compat
/// row (compat Mantle still names none: its view omits `temperature`).
#[test]
fn a_compat_temperature_refusal_coded_unsupported_value_names_no_row() {
    // Arrange
    let fixture = super::lane_weld_tests::corpus_fixture(
        "openai-compat/unsupported-value-temperature-mantle",
    )
    .expect("the fixture reads")
    .expect("the fixture is committed");
    let (captured, _) =
        super::lane_weld_tests::fixture_error(&fixture).expect("the fixture is an upstream error");
    let recoded = {
        let mut envelope: serde_json::Value =
            serde_json::from_str(fixture["body"].as_str().expect("a body"))
                .expect("the body is JSON");
        envelope["error"]["code"] = serde_json::json!("unsupported_parameter");
        Error::upstream_full(
            "p",
            400,
            envelope.to_string(),
            None,
            Some("invalid_request_error".into()),
            Some("unsupported_parameter".into()),
        )
    };
    let named = |err: &Error, egress: EgressKind| {
        let class = routectl_core::failure_class::classify(err, Some(OPENAI_COMPAT)).class;
        super::rejected_field(
            FieldRepairTable::grounded_only(),
            &class,
            err,
            OPENAI_COMPAT,
            Some(egress),
            SentEffort::Unknown,
        )
        .map(|named| named.row.path)
    };

    // Act
    let class = routectl_core::failure_class::classify(&captured, Some(OPENAI_COMPAT)).class;

    // Assert
    assert!(
        matches!(
            &class,
            FailureClass::FeatureUnsupported { capability } if capability == "unsupported_value"
        ),
        "premise: the captured envelope classifies as unsupported_value, got {class:?}",
    );
    assert_eq!(
        named(&recoded, EgressKind::OpenaiCompat),
        Some("temperature"),
        "control: the parameter code names the row on direct compat",
    );
    for egress in [EgressKind::OpenaiCompat, EgressKind::OpenaiCompatMantle] {
        assert_eq!(named(&captured, egress), None, "{egress:?}");
    }
}
