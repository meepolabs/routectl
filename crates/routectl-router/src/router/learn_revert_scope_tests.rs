//! Where the revert does not reach and what bounds it: a path the body
//! lacks, a translation lane, a routectl-added path no declaration explains
//! (warned, then left to the learners), a second rejection after a revert,
//! and the once-per-target and per-request bounds.

use std::sync::Arc;

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, IngressDialect};
use routectl_core::{ChatRequest, Result};
use serde_json::json;

use super::super::super::field_repair::TEST_UNSUPPORTED_SENTINEL;
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
        let provider = ScriptedProvider::new(faithful, refuse_op_key_then_test_row);
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
        let provider = ScriptedProvider::new(faithful, refuse_op_key_then_cache_marker);
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
/// reverted retries each fall back spends it on the first two targets; the
/// third target's rejection finds it spent and is not re-sent.
#[tokio::test]
async fn a_spent_repair_budget_leaves_the_rejection_unrepaired() {
    for surface in SURFACES {
        let providers: Vec<_> = (0..3)
            .map(|_| ScriptedProvider::new(faithful, refuse_op_key_then_unavailable))
            .collect();
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, false, &providers);

        let outcome = dispatch(&router, surface, anthropic_request()).await;

        let calls: Vec<usize> = providers.iter().map(|p| p.calls()).collect();
        assert_eq!(
            calls,
            [2, 2, 1],
            "{surface:?}: the third target is not re-sent"
        );
        assert!(!outcome.served, "{surface:?}");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Unrepaired); 3],
            "{surface:?}: one finding per rejection"
        );
        assert_nothing_learned(&router, &outcome, &format!("{surface:?}"));
    }
}
