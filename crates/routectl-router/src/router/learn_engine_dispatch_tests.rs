//! What the engine does with a located rejection, on both dispatch
//! surfaces: a rejection naming a path the body that went out did not carry
//! takes no repair and mints no verdict for that path, and the wire check
//! records it unrepaired; every reading the egress cannot settle proceeds as
//! it would with no wire record at all. A token count drawing a rejection
//! the generic learner attributes mints nothing from it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use routectl_core::wire_check::IngressDialect;
use routectl_core::{ChatRequest, Error, Provider, Result, ToolDef};
use serde_json::{Value, json};

use super::super::super::field_repair::TEST_UNSUPPORTED_PATH;
use super::{
    ANTHROPIC, Answer, CONVERSE, DISPLAY_FIXTURE, OPENAI_COMPAT, OPENAI_RESPONSES,
    ScriptedProvider, Shape, anthropic_rejection, bag_prefixed, bedrock_rejection, carries_display,
    carries_test_row, carry_display, carry_test_row, carry_web_search, converse_empty_bag,
    converse_test_row_in_bag, fixture_rejection, openai_rejection, request, router,
    test_row_at_root, thinking_with_display, thinking_without_display, unchanged, web_search_tool,
};
use crate::router::{Router, RouterOptions};
use crate::wire_check_state::{FieldRepairOutcome, WireCheckSnapshot};

const FORCED_TOOL_CHOICE_FIXTURE: &str =
    include_str!("../../tests/fixtures/forced_tool_choice_capture.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Complete,
    Stream,
    CountTokens,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// What one dispatch did: whether it served, the repair records, and the
/// capabilities it learned.
struct Outcome {
    served: bool,
    field_repairs: usize,
    learned: Vec<String>,
}

async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) -> Outcome {
    let (served, meta) = match surface {
        Surface::Complete => {
            let out = Box::pin(router.complete_with_options(req, RouterOptions::new())).await;
            (out.result.is_ok(), out.meta)
        }
        Surface::Stream => {
            let out = Box::pin(router.stream_with_options(req, RouterOptions::new())).await;
            (out.result.is_ok(), out.meta)
        }
        Surface::CountTokens => {
            let out = Box::pin(router.count_tokens_with_meta(req)).await;
            (out.result.is_ok(), out.meta)
        }
    };
    Outcome {
        served,
        field_repairs: meta.field_repairs.len(),
        learned: meta
            .learned_capabilities
            .iter()
            .map(|event| event.capability_key.clone())
            .collect(),
    }
}

/// `(provenance, field-repair outcome)` of every rejection finding.
fn rejections(snapshot: &WireCheckSnapshot) -> Vec<(&'static str, FieldRepairOutcome)> {
    snapshot
        .findings
        .iter()
        .filter_map(|f| Some((f.provenance?.label(), f.field_repair_outcome?)))
        .collect()
}

/// One repairable rejection dispatched end to end.
struct DispatchRow {
    name: &'static str,
    provider_block: &'static str,
    dialect: IngressDialect,
    carry: fn(&mut ChatRequest),
    sent: Option<Shape>,
    answer: Answer,
    /// Upstream calls the dispatch took: two when a repair re-sent the body.
    calls: usize,
    /// The finding the wire check records: provenance label and outcome.
    finding: (&'static str, FieldRepairOutcome),
}

async fn run_dispatch_rows(rows: &[DispatchRow]) {
    for surface in SURFACES {
        for row in rows {
            let provider = ScriptedProvider::new(row.sent, row.answer);
            let router = router(
                row.provider_block,
                Arc::clone(&provider) as Arc<dyn Provider>,
            );
            let mut req = request(row.dialect);
            (row.carry)(&mut req);

            let outcome = dispatch(&router, surface, req).await;

            let repaired = row.calls == 2;
            let name = row.name;
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                row.calls,
                "{surface:?} {name}: upstream calls"
            );
            assert_eq!(outcome.served, repaired, "{surface:?} {name}: served");
            assert_eq!(
                outcome.field_repairs,
                usize::from(repaired),
                "{surface:?} {name}: repair records"
            );
            if !repaired {
                assert!(
                    outcome.learned.is_empty(),
                    "{surface:?} {name}: no verdict: {:?}",
                    outcome.learned
                );
            }
            assert_eq!(
                rejections(&router.wire_check_snapshot()),
                [row.finding],
                "{surface:?} {name}: the wire check finding"
            );
        }
    }
}

/// Refuse with `err` while the request carries the test-only row.
fn refuse_test_row(req: &ChatRequest, err: fn() -> Error) -> Result<()> {
    if carries_test_row(req) {
        return Err(err());
    }
    Ok(())
}

fn reject_display_while_carried(req: &ChatRequest) -> Result<()> {
    if carries_display(req) {
        return Err(fixture_rejection(DISPLAY_FIXTURE));
    }
    Ok(())
}

#[tokio::test]
async fn an_anthropic_rejection_is_repaired_only_when_the_egress_carries_its_path() {
    run_dispatch_rows(&[
        DispatchRow {
            name: "tagged-union path carried: repaired",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_display,
            sent: Some(thinking_with_display),
            answer: reject_display_while_carried,
            calls: 2,
            finding: ("routectl_added", FieldRepairOutcome::RepairedLossy),
        },
        DispatchRow {
            name: "tagged-union path not carried: ignored",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_display,
            sent: Some(thinking_without_display),
            answer: reject_display_while_carried,
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
        DispatchRow {
            name: "field not carried: ignored",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
        DispatchRow {
            name: "no deposit: repaired as with no wire record",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: None,
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("unlocalized", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "coarsened path: proceeds as with no wire record",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |_| Err(anthropic_rejection("a.b.c.d.e.f.g.h.i")),
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn a_responses_client_on_a_translation_lane_is_located_on_the_egress() {
    run_dispatch_rows(&[
        DispatchRow {
            name: "carried on the egress: repaired",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Responses,
            carry: carry_test_row,
            sent: Some(test_row_at_root),
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("n_a", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "not carried on the egress: ignored",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Responses,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |req| refuse_test_row(req, || anthropic_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 1,
            finding: ("n_a", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn an_openai_param_rejection_is_located_on_the_body() {
    run_dispatch_rows(&[
        DispatchRow {
            name: "param carried: repaired",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            carry: carry_test_row,
            sent: Some(test_row_at_root),
            answer: |req| refuse_test_row(req, || openai_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("routectl_added", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "param not carried: ignored",
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            carry: carry_test_row,
            sent: Some(unchanged),
            answer: |req| refuse_test_row(req, || openai_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 1,
            finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn a_converse_rejection_is_located_in_the_field_bag() {
    if !cfg!(feature = "bedrock") {
        return;
    }
    run_dispatch_rows(&[
        DispatchRow {
            name: "model field carried in the bag: repaired",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(converse_test_row_in_bag),
            answer: |req| refuse_test_row(req, || bedrock_rejection(TEST_UNSUPPORTED_PATH)),
            calls: 2,
            finding: ("n_a", FieldRepairOutcome::Repaired),
        },
        DispatchRow {
            name: "bag-prefixed field not carried: ignored",
            provider_block: CONVERSE,
            dialect: IngressDialect::Anthropic,
            carry: carry_test_row,
            sent: Some(converse_empty_bag),
            answer: |req| {
                refuse_test_row(req, || {
                    bedrock_rejection(&bag_prefixed(TEST_UNSUPPORTED_PATH))
                })
            },
            calls: 1,
            finding: ("n_a", FieldRepairOutcome::Unrepaired),
        },
    ])
    .await;
}

#[tokio::test]
async fn an_openai_responses_rejection_proceeds_as_with_no_wire_record() {
    if !cfg!(feature = "openai-responses") {
        return;
    }
    run_dispatch_rows(&[DispatchRow {
        name: "a param the parser does not read",
        provider_block: OPENAI_RESPONSES,
        dialect: IngressDialect::Responses,
        carry: carry_test_row,
        sent: Some(unchanged),
        answer: |req| refuse_test_row(req, || openai_rejection(TEST_UNSUPPORTED_PATH)),
        calls: 1,
        finding: ("unlocalized", FieldRepairOutcome::Unrepaired),
    }])
    .await;
}

/// A capability named in `error.param` is no body path, so its absence from
/// the body suppresses no capability learning: the generic learner keeps its
/// own gate on what the request carries.
#[tokio::test]
async fn a_capability_named_as_a_param_is_still_learned_when_the_body_lacks_that_key() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(Some(web_search_tool), |_| {
            Err(openai_rejection("web_search"))
        });
        let router = router(OPENAI_COMPAT, Arc::clone(&provider) as Arc<dyn Provider>);
        let mut req = request(IngressDialect::Chat);
        carry_web_search(&mut req);

        let outcome = dispatch(&router, surface, req).await;

        assert!(!outcome.served, "{surface:?}: premise, the 400 is terminal");
        assert_eq!(outcome.learned, ["web_search"], "{surface:?}");
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("unlocalized", FieldRepairOutcome::Unrepaired)],
            "{surface:?}"
        );
    }
}

/// The captured anthropic-api rejection of a forced `tool_choice`, which the
/// generic learner attributes from its message.
fn anthropic_forced_tool_choice_rejection(_: &ChatRequest) -> Result<()> {
    let fixture: Value =
        serde_json::from_str(FORCED_TOOL_CHOICE_FIXTURE).expect("forced tool choice fixture");
    let rejection = &fixture["anthropic_api"];
    let status = rejection["status"].as_u64().expect("status");
    Err(Error::upstream_full(
        "scripted",
        u16::try_from(status).expect("u16 status"),
        rejection["body"].as_str().expect("body").to_string(),
        None,
        Some("invalid_request_error".into()),
        None,
    ))
}

fn carry_forced_tool_choice(req: &mut ChatRequest) {
    req.tools = Some(vec![ToolDef::Other(json!({
        "type": "function",
        "function": {"name": "lookup", "parameters": {"type": "object"}}
    }))]);
    req.tool_choice = Some(json!({"type": "any"}));
}

/// A token count validates only what the count endpoint projects, so a
/// rejection it draws teaches no generic capability verdict, while the same
/// rejection on a message dispatch to the same target does.
#[tokio::test]
async fn a_count_tokens_400_mints_no_generic_verdict() {
    for surface in [Surface::Complete, Surface::CountTokens] {
        // Arrange
        let provider = ScriptedProvider::new(None, anthropic_forced_tool_choice_rejection);
        let router = router(ANTHROPIC, Arc::clone(&provider) as Arc<dyn Provider>);
        let mut req = request(IngressDialect::Anthropic);
        carry_forced_tool_choice(&mut req);

        // Act
        let outcome = dispatch(&router, surface, req).await;

        // Assert
        assert!(!outcome.served, "{surface:?}: premise, the 400 is terminal");
        assert_eq!(provider.calls(), 1, "{surface:?}: no repair re-dispatch");
        let resident: Vec<String> = router
            .learned_capability_snapshot()
            .into_iter()
            .map(|entry| entry.feature_key)
            .collect();
        if surface == Surface::Complete {
            assert_eq!(
                resident,
                ["forced_tool_choice"],
                "{surface:?}: the control records the capability"
            );
        } else {
            assert!(
                resident.is_empty() && outcome.learned.is_empty(),
                "{surface:?}: no verdict: resident {resident:?}, learned {:?}",
                outcome.learned
            );
        }
    }
}
