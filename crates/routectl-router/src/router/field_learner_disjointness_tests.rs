//! The field-repair learner and the generic capability learner never claim the
//! same rejection: a closed-table row's path, refused in any repair kind's own
//! dialect, resolves no generic capability. Field repair runs first and
//! `continue`s, so an overlap would be decided by arm order rather than by a
//! deliberate precedence -- a row that collides fails here when it is added.

use super::*;
use routectl_core::UpstreamBody;
use routectl_core::failure_class::classify;
use serde_json::json;

use crate::capability_matcher::resolve_requested_capability;
use crate::router::learn_engine::{EffortRank, SentEffort};

/// The pydantic reason the anthropic-api and bedrock dialects refuse a field
/// with.
const EXTRA_INPUTS: &str = "Extra inputs are not permitted";

/// An anthropic-api `invalid_request_error` refusing `path`.
fn anthropic_rejection(path: &str) -> Error {
    Error::upstream(
        "disjoint-mock",
        400,
        json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": format!("{path}: {EXTRA_INPUTS}")},
        })
        .to_string(),
    )
}

/// A bedrock-runtime `ValidationException` refusing `path`.
fn bedrock_rejection(path: &str) -> Error {
    Error::upstream_with_body(
        "disjoint-mock",
        400,
        UpstreamBody::new(json!({ "message": format!("{path}: {EXTRA_INPUTS}") }).to_string()),
        None,
        Some("ValidationException".to_string()),
        None,
    )
}

/// An openai-compat unsupported-parameter refusal of `path`, with or without
/// the `error.code` that lifts the class out of `BadRequest`.
fn openai_compat_rejection(path: &str, code: Option<&str>) -> Error {
    let mut error = json!({
        "message": format!("Unsupported parameter: '{path}' is not supported with this model."),
        "type": "invalid_request_error",
        "param": path,
    });
    if let Some(code) = code {
        error["code"] = json!(code);
    }
    Error::upstream_with_body(
        "disjoint-mock",
        400,
        UpstreamBody::new(json!({ "error": error }).to_string()),
        None,
        Some("invalid_request_error".to_string()),
        code.map(str::to_string),
    )
}

/// Every repair kind's refusals of `path`, labelled for a failure message.
fn dialect_rejections(path: &str) -> Vec<(&'static str, &'static str, Error)> {
    vec![
        (
            ANTHROPIC_API_KIND,
            "location prefix",
            anthropic_rejection(path),
        ),
        (BEDROCK_KIND, "location prefix", bedrock_rejection(path)),
        (
            BEDROCK_KIND,
            "converse field bag",
            bedrock_rejection(&format!("additionalModelRequestFields.{path}")),
        ),
        (
            OPENAI_COMPAT_KIND,
            "coded",
            openai_compat_rejection(path, Some(OPENAI_UNSUPPORTED_CODE)),
        ),
        (
            OPENAI_COMPAT_KIND,
            "template only",
            openai_compat_rejection(path, None),
        ),
    ]
}

/// The path an upstream names to refuse `row`, and the effort level the
/// refused request sent: a ranked row's level is the sent level, never a
/// segment of the named path.
fn refused_as(row: &FieldRepairRow) -> (&'static str, SentEffort) {
    match row.surface {
        FieldSurface::Effort(level) => (
            "output_config.effort",
            SentEffort::Rank(EffortRank::of_token(level).expect("a ranked row names a level")),
        ),
        _ => (row.path, SentEffort::Unknown),
    }
}

/// Mutation check: add a closed-table row whose path a generic resolver
/// translates (e.g. `output_config.format`, or a bedrock `advisor` row) -> red
/// on that row's kind.
#[test]
fn no_closed_table_row_resolves_a_generic_capability_in_any_repair_kind() {
    for row in closed_table() {
        let (path, sent) = refused_as(row);
        for (kind, shape, err) in dialect_rejections(path) {
            let classified = classify(&err, Some(kind));
            let table = crate::router::FieldRepairTable::default();
            let in_view = table
                .for_lane(kind, crate::router::field_repair::direct_egress(kind))
                .lane_rows()
                .any(|lane_row| lane_row.path == row.path);

            assert_eq!(
                rejected_field(
                    table,
                    &classified.class,
                    &err,
                    kind,
                    crate::router::field_repair::direct_egress(kind),
                    sent
                )
                .is_some(),
                in_view,
                "control: {kind} ({shape}) names `{}` to the field parser under {:?} exactly \
                 when the kind's view carries the row",
                row.path,
                classified.class,
            );
            assert_eq!(
                resolve_requested_capability(kind, &err, &classified),
                None,
                "{kind} ({shape}): `{}` is a field-repair row and must resolve no generic \
                 capability",
                row.path,
            );
        }
    }
}

/// What the field-row and carried-member proposers each make of `err` on an
/// anthropic-api target, given the attempt's settlement.
struct Proposed {
    field_row: bool,
    carried_member: bool,
}

fn propose_on_anthropic(
    err: &Error,
    provenance: routectl_core::wire_check::RejectionProvenance,
    in_egress: Option<bool>,
) -> Proposed {
    use crate::router::learn_engine::ThinkingShape;
    use crate::router::learn_engine::{Proposal, Proposer, RejectionInput, RejectionSite};
    use crate::router::learn_proposers::{CarriedMemberProposer, FieldRowProposer};

    let config: crate::config::Config =
        toml::from_str("[providers.p1]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n")
            .expect("valid test toml");
    let router = crate::router::Router::new(std::sync::Arc::new(config));
    let provider: std::sync::Arc<dyn routectl_core::Provider> =
        std::sync::Arc::new(crate::router::field_repair::generic_tests::NoopSeat);
    let model = crate::resolved::ResolvedModel::new("m1", "p1", provider, "wire-model");
    let target = router
        .expand_chain_to_targets(vec![std::sync::Arc::new(model)], None)
        .pop()
        .expect("one target");
    let site = RejectionSite {
        provenance,
        in_egress,
        matched_edit: crate::router::learn_revert::MatchedEdit::None,
        locator: None,
        sent: crate::router::learn_revert::SentSlot::default(),
        pending: 0,
        sent_effort: SentEffort::Unknown,
        thinking: ThinkingShape::Unknown,
    };
    let classified = classify(err, Some(ANTHROPIC_API_KIND));
    let input = RejectionInput {
        err,
        class: &classified,
        native_class: &classified.class,
        target: &target,
        replay_carried: false,
        table: crate::router::FieldRepairTable::default(),
        site: Some(site),
        client_betas: &[],
        withheld_betas: &[],
        beta_assembly: None,
    };
    Proposed {
        field_row: matches!(
            FieldRowProposer.propose(&input),
            Some(Proposal::FieldRow { .. })
        ),
        carried_member: matches!(
            CarriedMemberProposer.propose(&input),
            Some(Proposal::CarriedMember { .. })
        ),
    }
}

/// A member no closed row names and the capability strip does not own: the
/// generic arm proposes it when the settlement attributes it to the client.
const UNOWNED_MEMBER: &str = "service_tier_hint";

/// A refused client member the capability strip owns is left to the strip:
/// the generic carried-member arm proposes nothing for it, while the same
/// refusal of a member the strip does not own is proposed. Both requests
/// carry their member at the top level, unchanged from the client's body.
///
/// Mutation check: drop the `strippable_keys` check in
/// `CarriedMemberProposer::propose` -> red on `context_management`.
#[test]
fn a_member_the_capability_strip_owns_is_never_taken_by_the_generic_arm() {
    use routectl_core::wire_check::RejectionProvenance;

    for (member, proposed) in [
        (crate::capability_strip::CONTEXT_MANAGEMENT, false),
        (UNOWNED_MEMBER, true),
    ] {
        assert!(
            crate::capability_strip::strippable_keys().any(|key| key == member) != proposed,
            "premise: {member}",
        );
        let err = anthropic_rejection(member);

        let outcome = propose_on_anthropic(&err, RejectionProvenance::ClientUnchanged, Some(true));

        assert_eq!(outcome.carried_member, proposed, "{member}");
    }
}

/// A rejection a closed-table row claims is the row's, never the generic
/// arm's, even though it refuses a member by the same grounded template.
///
/// Mutation check: drop the `FieldRowProposer.propose(input).is_some()`
/// check in `CarriedMemberProposer::propose` -> red.
#[test]
fn a_rejection_a_closed_row_claims_is_never_taken_by_the_generic_arm() {
    use routectl_core::wire_check::RejectionProvenance;
    let row_path = "temperature";
    let err = anthropic_rejection(row_path);

    let outcome = propose_on_anthropic(&err, RejectionProvenance::ClientUnchanged, Some(true));

    assert!(
        outcome.field_row,
        "premise: the closed row claims `{row_path}`"
    );
    assert!(!outcome.carried_member);
}

/// A member the client sent unchanged is attributed to it only when the body
/// that went out is proven to carry it; a proven absence and an unproven
/// presence each leave the rejection unattributed. The `Some(true)` row is the
/// positive control on the same rejection.
///
/// Mutation check: drop `site.in_egress == Some(true)` from the attribution
/// in `CarriedMemberProposer::propose` -> red on both refusing rows.
#[test]
fn egress_presence_must_be_proven_for_the_generic_arm() {
    use routectl_core::wire_check::RejectionProvenance;
    let err = anthropic_rejection(UNOWNED_MEMBER);

    for (name, in_egress, proposed) in [
        ("present on the wire", Some(true), true),
        ("absent from the wire", Some(false), false),
        ("presence unproven", None, false),
    ] {
        let outcome = propose_on_anthropic(&err, RejectionProvenance::ClientUnchanged, in_egress);

        assert!(!outcome.field_row, "premise: {name}: no closed row");
        assert_eq!(outcome.carried_member, proposed, "{name}");
    }
}
