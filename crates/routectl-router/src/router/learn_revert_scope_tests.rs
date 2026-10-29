//! Where the revert does not reach and what bounds it: a path the body
//! lacks, a translation lane, a routectl-added path no declaration explains
//! (warned, then left to the learners), a second rejection after a revert,
//! the once-per-target and per-request bounds, and the one unrevertible
//! site whose rejection still reaches the proposers.

use std::sync::Arc;

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, IngressDialect};
use routectl_core::{ChatRequest, Result};
use serde_json::json;

use super::super::super::field_repair::TEST_UNSUPPORTED_SENTINEL;
use super::super::super::repair_budget::REPAIRS_PER_REQUEST;
use super::*;
use crate::wire_check_state::FieldRepairOutcome;

fn carry_test_row(req: &mut ChatRequest) {
    req.user = Some(TEST_UNSUPPORTED_SENTINEL.to_string());
}

/// Refuse the operator extra, then the closed-table test field, then serve.
fn refuse_op_key_then_test_row(req: &ChatRequest) -> Result<()> {
    refuse_op_key_while_carried(req)?;
    refuse_test_row_while_carried(req)
}

/// A reverted retry that drew a second rejection did not succeed, even when
/// a later repair of that second rejection serves the request: the revert's
/// rejection stays unrepaired and only the later one is repaired.
#[tokio::test]
async fn a_later_repair_does_not_credit_a_revert_whose_retry_failed() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(Some(faithful), refuse_op_key_then_test_row);
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, false, &[Arc::clone(&provider)]);
        let mut req = anthropic_request();
        carry_test_row(&mut req);

        let outcome = dispatch(&router, surface, req).await;

        assert_eq!(
            provider.calls(),
            3,
            "{surface:?}: revert, then field repair"
        );
        assert!(
            outcome.served,
            "{surface:?}: premise, the field repair serves"
        );
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [
                ("routectl_added", FieldRepairOutcome::Unrepaired),
                ("routectl_added", FieldRepairOutcome::Repaired),
            ],
            "{surface:?}"
        );
    }
}

/// A path the body that went out does not carry is ignored as before: no
/// declaration is matched against it, no revert runs and no warning names it.
#[tokio::test]
async fn a_rejection_naming_a_path_the_body_lacks_is_ignored_without_a_warning() {
    run_rows(&[
        Row {
            name: "a client value the operator rewrote, lost on the way out",
            extras: OPERATOR_EXTRA,
            cache: false,
            request: || {
                request_with(
                    IngressDialect::Anthropic,
                    &json!({"metadata": {"op_key": "client"}}),
                    true,
                )
            },
            shape: op_key_dropped,
            answer: |_| Err(rejection(OP_KEY_PATH)),
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("client_changed", FieldRepairOutcome::Unrepaired)],
            warning: None,
        },
        Row {
            name: "operator extra declared but not sent",
            extras: OPERATOR_EXTRA,
            cache: false,
            request: anthropic_request,
            shape: client_only,
            answer: refuse_op_key_while_carried,
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("unlocalized", FieldRepairOutcome::Unrepaired)],
            warning: None,
        },
    ])
    .await;
}

/// Router edits are declared on fidelity lanes only, and on a translation
/// lane the client's pointers are not the egress's: even an edit the egress
/// declares at the rejected path is not reverted, and nothing warns.
#[tokio::test]
async fn a_translation_lane_neither_reverts_nor_warns() {
    for surface in SURFACES {
        let edit = DeclaredEdit::new(EditSite::DialectFieldStrip, MASKED_OP_KEY, EditKind::Add)
            .expect("a matchable pattern");
        let provider = ScriptedProvider::declaring(faithful, refuse_op_key_while_carried, edit);
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, false, &[Arc::clone(&provider)]);
        let req = request_with(IngressDialect::Chat, &json!({"metadata": {}}), true);

        let outcome = dispatch(&router, surface, req).await;

        assert_eq!(provider.calls(), 1, "{surface:?}: not re-sent");
        assert!(!outcome.served, "{surface:?}");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("n_a", FieldRepairOutcome::Unrepaired)],
            "{surface:?}"
        );
        assert!(
            warned(&outcome.events).is_empty(),
            "{surface:?}: no warning"
        );
        assert_nothing_learned(&router, &outcome, &format!("{surface:?}"));
    }
}

/// A path routectl added with no declaration explaining it still warns, then
/// goes to the learners exactly as it would have: the field repair answers it.
#[tokio::test]
async fn an_undeclared_routectl_added_path_warns_and_falls_through_to_the_learners() {
    run_rows(&[Row {
        name: "closed-table field added without a declaration",
        extras: "",
        cache: false,
        request: || {
            let mut req = anthropic_request();
            carry_test_row(&mut req);
            req
        },
        shape: faithful,
        answer: refuse_test_row_while_carried,
        calls: 2,
        served: true,
        field_repairs: 1,
        findings: &[("routectl_added", FieldRepairOutcome::Repaired)],
        warning: Some((
            ANTHROPIC_LANE,
            "/routectl_test_unsupported_field",
            "undeclared",
        )),
    }])
    .await;
}

/// Refuse the operator extra while carried, then the placed top-level cache
/// marker while carried, then serve.
fn refuse_op_key_then_cache_marker(req: &ChatRequest) -> Result<()> {
    refuse_op_key_while_carried(req)?;
    refuse_cache_marker_while_carried(req)
}

/// A target reverts at most once: a second declared edit rejected on the
/// same target is not reverted, though budget remains.
#[tokio::test]
async fn a_target_reverts_at_most_once() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(Some(faithful), refuse_op_key_then_cache_marker);
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, true, &[Arc::clone(&provider)]);

        let outcome = dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(
            provider.calls(),
            2,
            "{surface:?}: the second edit is not undone"
        );
        assert!(!outcome.served, "{surface:?}");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Unrepaired); 2],
            "{surface:?}"
        );
        let sites: Vec<String> = warned(&outcome.events)
            .into_iter()
            .map(|(_, _, site)| site)
            .collect();
        assert_eq!(
            sites,
            ["operator_payload_extras", "cache_placement"],
            "{surface:?}: each rejected edit warns"
        );
    }
}

/// The revert draws the one per-request repair budget. A chain whose
/// reverted retries each fall back spends it on the targets ahead of the
/// last; the last target's rejection finds it spent and is not re-sent.
#[tokio::test]
async fn a_spent_repair_budget_leaves_the_rejection_unrepaired() {
    const TARGETS: usize = REPAIRS_PER_REQUEST as usize + 1;
    for surface in SURFACES {
        let providers: Vec<_> = (0..TARGETS)
            .map(|_| ScriptedProvider::new(Some(faithful), refuse_op_key_then_unavailable))
            .collect();
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, false, &providers);

        let outcome = dispatch(&router, surface, anthropic_request()).await;

        let calls: Vec<usize> = providers.iter().map(|p| p.calls()).collect();
        let mut expected = vec![2; TARGETS - 1];
        expected.push(1);
        assert_eq!(
            calls, expected,
            "{surface:?}: the last target is not re-sent"
        );
        assert!(!outcome.served, "{surface:?}");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Unrepaired); TARGETS],
            "{surface:?}: one finding per rejection"
        );
        assert_nothing_learned(&router, &outcome, &format!("{surface:?}"));
    }
}

/// Two operator leaves under a masked container, each declared at the same
/// masked pattern.
const TWO_MASKED_LEAVES: &str = "payload_extras = { metadata = { op_a = \"v\", op_b = \"v\" } }\n";

fn carries_metadata_leaf(req: &ChatRequest, leaf: &str) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(|extras| extras.get("metadata"))
        .and_then(|metadata| metadata.get(leaf))
        .is_some()
}

/// Refuse `op_b` while carried, then `op_a` while carried, then serve.
fn refuse_op_b_then_op_a(req: &ChatRequest) -> Result<()> {
    for leaf in ["op_b", "op_a"] {
        if carries_metadata_leaf(req, leaf) {
            return Err(rejection(&format!("metadata.{leaf}")));
        }
    }
    Ok(())
}

/// Reverting one operator leaf leaves the other leaf, still on the retry,
/// declared: a second rejection naming it is laid to the operator's edit
/// (and, the target's one revert spent, declined), never to an undeclared
/// routectl addition the learners would read.
#[tokio::test]
async fn reverting_one_masked_leaf_keeps_its_sibling_declared() {
    run_rows(&[Row {
        name: "op_b reverted, then op_a rejected",
        extras: TWO_MASKED_LEAVES,
        cache: false,
        request: anthropic_request,
        shape: faithful,
        answer: refuse_op_b_then_op_a,
        calls: 2,
        served: false,
        field_repairs: 0,
        findings: &[("routectl_added", FieldRepairOutcome::Unrepaired); 2],
        warning: Some((ANTHROPIC_LANE, MASKED_OP_KEY, "operator_payload_extras")),
    }])
    .await;
}

/// The declared path of the closed-table test field, as a pointer pattern.
const TEST_ROW_POINTER: &str = "/routectl_test_unsupported_field";

/// One dispatch of the closed-table test field, refused while carried, on a
/// provider that declares `site` wrote that field.
async fn dispatch_test_row_declared_by(
    site: EditSite,
    surface: Surface,
) -> (Arc<ScriptedProvider>, Router, Outcome) {
    let edit =
        DeclaredEdit::new(site, TEST_ROW_POINTER, EditKind::Add).expect("a matchable pattern");
    let provider = ScriptedProvider::declaring(faithful, refuse_test_row_while_carried, edit);
    let router = router(ANTHROPIC, false, &[Arc::clone(&provider)]);
    let mut req = anthropic_request();
    carry_test_row(&mut req);
    let outcome = dispatch(&router, surface, req).await;
    (provider, router, outcome)
}

/// A required field routectl wrote with its own value cannot be reverted,
/// yet the upstream refusing it still says what it accepts: the rejection
/// reaches the field repair, whose retry serves and learns. A declined revert
/// answered nothing, so the finding takes the field repair's outcome.
#[tokio::test]
async fn an_unrevertible_required_field_default_reaches_the_field_repair() {
    for surface in SURFACES {
        let (provider, router, outcome) =
            dispatch_test_row_declared_by(EditSite::RequiredFieldDefault, surface).await;

        assert_eq!(provider.calls(), 2, "{surface:?}: repaired and re-sent");
        assert!(outcome.served, "{surface:?}: the repaired retry serves");
        assert_eq!(outcome.field_repairs, 1, "{surface:?}: one field repair");
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            1,
            "{surface:?}: the repair learns a field verdict"
        );
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Repaired)],
            "{surface:?}: the field repair is the finding's outcome"
        );
        assert_eq!(
            warned(&outcome.events),
            [(
                ANTHROPIC_LANE.to_owned(),
                TEST_ROW_POINTER.to_owned(),
                "required_field_default".to_owned()
            )],
            "{surface:?}: the matched edit warns"
        );
    }
}

/// The token-count walk answers the same rejection the same way: the
/// unrevertible required-field default reaches the field repair, whose
/// repaired count is the one returned. A count settles no verdict.
#[tokio::test]
async fn an_unrevertible_required_field_default_reaches_the_count_repair() {
    let edit = DeclaredEdit::new(
        EditSite::RequiredFieldDefault,
        TEST_ROW_POINTER,
        EditKind::Add,
    )
    .expect("a matchable pattern");
    let provider = ScriptedProvider::declaring(faithful, refuse_test_row_while_carried, edit);
    let router = router(ANTHROPIC, false, &[Arc::clone(&provider)]);
    let mut req = anthropic_request();
    carry_test_row(&mut req);

    let counted = router.count_tokens(req).await;

    assert!(counted.is_ok(), "the repaired count serves: {counted:?}");
    assert_eq!(provider.calls(), 2, "repaired and re-counted");
}

/// Every other unrevertible site keeps the rejection from the proposers,
/// though the field table names the path the declaration covers.
#[tokio::test]
async fn another_unrevertible_site_still_proceeds_without_a_proposer() {
    for surface in SURFACES {
        let (provider, router, outcome) =
            dispatch_test_row_declared_by(EditSite::ThinkingSamplingClamp, surface).await;

        assert_eq!(provider.calls(), 1, "{surface:?}: not re-sent");
        assert!(!outcome.served, "{surface:?}");
        assert_eq!(outcome.field_repairs, 0, "{surface:?}: no field repair");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Unrepaired)],
            "{surface:?}"
        );
        assert_eq!(
            warned(&outcome.events),
            [(
                ANTHROPIC_LANE.to_owned(),
                TEST_ROW_POINTER.to_owned(),
                "thinking_sampling_clamp".to_owned()
            )],
            "{surface:?}"
        );
        assert_nothing_learned(&router, &outcome, &format!("{surface:?}"));
    }
}

const ANTHROPIC_VERSION: &str = "anthropic_version";

/// A faithful body that also carries routectl's `anthropic_version`.
fn with_anthropic_version(req: &ChatRequest, client: Value) -> Value {
    let mut body = faithful(req, client);
    body[ANTHROPIC_VERSION] = json!("routectl-version");
    body
}

fn refuse_anthropic_version(_: &ChatRequest) -> Result<()> {
    Err(rejection(ANTHROPIC_VERSION))
}

/// A required-field default at a path no closed-table row names reaches the
/// proposers and none answers it: no re-send (so no repair budget drawn), no
/// repair, nothing learned.
#[tokio::test]
async fn an_unrevertible_required_field_default_with_no_row_proceeds_inert() {
    for surface in SURFACES {
        let edit = DeclaredEdit::new(
            EditSite::RequiredFieldDefault,
            "/anthropic_version",
            EditKind::Add,
        )
        .expect("a matchable pattern");
        let provider =
            ScriptedProvider::declaring(with_anthropic_version, refuse_anthropic_version, edit);
        let router = router(ANTHROPIC, false, &[Arc::clone(&provider)]);

        let outcome = dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(provider.calls(), 1, "{surface:?}: not re-sent");
        assert!(!outcome.served, "{surface:?}");
        assert_eq!(outcome.field_repairs, 0, "{surface:?}: no field repair");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Unrepaired)],
            "{surface:?}"
        );
        assert_eq!(
            warned(&outcome.events),
            [(
                ANTHROPIC_LANE.to_owned(),
                "/anthropic_version".to_owned(),
                "required_field_default".to_owned()
            )],
            "{surface:?}"
        );
        assert_nothing_learned(&router, &outcome, &format!("{surface:?}"));
    }
}
