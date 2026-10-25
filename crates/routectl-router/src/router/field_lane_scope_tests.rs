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
/// sibling's rows, and each allowlist is empty: no Mantle refusal has been
/// captured to ground a row there.
///
/// Mutation check: map a Mantle egress to its kind's scope in
/// `RowScope::of_lane` -> red on that sub-lane.
#[test]
fn each_mantle_egress_carries_exactly_its_empty_allowlist() {
    let cases: [(&str, &str, EgressKind, &[&str]); 3] = [
        (
            "anthropic-api Mantle",
            ANTHROPIC_API,
            EgressKind::AnthropicApiMantle,
            ANTHROPIC_API_MANTLE_ROWS,
        ),
        (
            "openai-compat Mantle",
            OPENAI_COMPAT,
            EgressKind::OpenaiCompatMantle,
            OPENAI_COMPAT_MANTLE_ROWS,
        ),
        (
            "openai-responses Mantle",
            OPENAI_RESPONSES,
            EgressKind::OpenaiResponsesMantle,
            OPENAI_RESPONSES_MANTLE_ROWS,
        ),
    ];

    for (name, kind, egress, allowlist) in cases {
        assert!(allowlist.is_empty(), "{name}: the allowlist is empty");
        assert_eq!(lane_paths(kind, Some(egress)), set(allowlist), "{name}");
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

/// The committed `rejections/<dir>/<case>.json` fixture as an upstream error.
fn fixture_error(dir: &str, case: &str) -> Error {
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
/// resolves to its row resolves to none on that lane's Mantle sibling, though
/// both share the sibling's dialect.
///
/// Mutation check: map the Mantle egresses to their kind's scope in
/// `RowScope::of_lane` -> red on each Mantle row.
#[test]
fn a_refusal_names_its_row_on_the_direct_lane_and_none_on_its_mantle_sibling() {
    let compat = fixture_error("openai-compat", "unsupported-parameter-temperature");
    let ranked = fixture_error(
        "openai-responses",
        "reasoning-effort-minimal-unsupported-codex",
    );
    let minimal = SentEffort::Rank(EffortRank::of_token("minimal").expect("a rank member"));
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
            "ranked responses refusal",
            &ranked,
            OPENAI_RESPONSES,
            EgressKind::OpenaiResponses,
            EgressKind::OpenaiResponsesMantle,
            minimal,
            "output_config.effort.minimal",
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
/// not: the blindness count reads the lane's own view.
///
/// Mutation check: map the compat Mantle egress to its kind's scope in
/// `RowScope::of_lane` -> red on the Mantle assertion.
#[test]
fn a_compat_refusal_on_compat_mantle_counts_as_unlocalized() {
    let err = fixture_error("openai-compat", "unsupported-parameter-temperature");
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
