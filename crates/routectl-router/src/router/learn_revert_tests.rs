//! A rejection naming a path routectl's own declared edit put on the wire,
//! on both dispatch surfaces: the edit is undone and the attempt re-sent when
//! its site is revertible and the undone slot holds exactly what the client
//! sent; otherwise nothing is undone. Either way nothing is learned or
//! persisted, one deduped `routectl_edit_rejected` warning names the lane,
//! the masked path and the edit site, and the wire check records exactly one
//! outcome per rejection.
//!
//! Test providers stand in for the egress deposit and the upstream's answer.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{ChatRequest, Error, Provider, Result, SystemContent, ToolDef};
use routectl_testkit::CapturedEvent;
use serde_json::{Value, json};

use super::super::field_repair::{TEST_UNSUPPORTED_PATH, TEST_UNSUPPORTED_SENTINEL};
use super::super::scripted_provider_test_support::{Answer, ScriptedProvider, Shape};
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{Router, RouterOptions};
use crate::wire_check_state::{FieldRepairOutcome, WireCheckSnapshot};

const ALIAS: &str = "a";
const NEVER_SAMPLED: &str = "first_n = 0\nsample_one_in = 1000";
const EDIT_REJECTED: &str = "routectl_edit_rejected";
const REFUSED_FIELD: &str = "Extra inputs are not permitted";
const ANTHROPIC: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
/// An operator extra under `metadata`, whose keys render masked.
const OPERATOR_EXTRA: &str = "payload_extras = { metadata = { op_key = \"v\" } }\n";
/// Two operator extras under `metadata`: `op_key` added, `b` rewritten
/// over the client's own value. Both declarations render `/metadata/*`.
const TWO_OPERATOR_EXTRAS: &str = "payload_extras = { metadata = { op_key = \"v\", b = 2 } }\n";
/// An operator extra at the closed-table test field's path. The faithful
/// egress then sends the field's own value there, not the operator's, so the
/// request's slot is never the wire's and no revert holds.
const OPERATOR_TEST_ROW: &str = "payload_extras = { routectl_test_unsupported_field = \"op\" }\n";
const OP_KEY: &str = "op_key";
const OP_KEY_PATH: &str = "metadata.op_key";
const MASKED_OP_KEY: &str = "/metadata/*";
/// A client key carrying an ECMA-48 escape.
const CONTROL_KEY: &str = "op\u{1b}[31mkey";

/// A router whose alias walks one target per provider, each on a
/// `[providers.pN]` entry configured by `provider_block`, one call per
/// attempt. `cache` turns on the top-level cache marker placement.
fn router(provider_block: &str, cache: bool, providers: &[Arc<ScriptedProvider>]) -> Router {
    let mut toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [cache]\nauto_emit_top_level_breakpoint = {cache}\n[wire_check]\n{NEVER_SAMPLED}\n"
    );
    let mut nicknames = Vec::new();
    for idx in 0..providers.len() {
        toml_text.push_str(&format!("[providers.p{idx}]\n{provider_block}"));
        nicknames.push(format!("\"m{idx}\""));
    }
    toml_text.push_str(&format!(
        "[aliases]\n{ALIAS} = [{}]\n",
        nicknames.join(", ")
    ));
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let models = providers
        .iter()
        .enumerate()
        .map(|(idx, provider)| {
            let nickname = format!("m{idx}");
            let provider = Arc::clone(provider) as Arc<dyn Provider>;
            let model = ResolvedModel::new(&nickname, format!("p{idx}"), provider, "upstream");
            (nickname, Arc::new(model))
        })
        .collect::<BTreeMap<_, _>>();
    router.install_resolved_models(models);
    router
}

/// A request in `dialect` whose client body carries `client_extras` at its
/// top level and, unless `canonical` is false, whose canonical extras do too.
fn request_with(dialect: IngressDialect, client_extras: &Value, canonical: bool) -> ChatRequest {
    let mut client = json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]});
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        ..Default::default()
    };
    if let Some(extras) = client_extras
        .as_object()
        .filter(|extras| !extras.is_empty())
    {
        for (key, value) in extras {
            client[key] = value.clone();
        }
        if canonical {
            req.provider_extras = Some(client_extras.clone());
        }
    }
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

/// A request whose client sent an empty `metadata`, so an operator key under
/// it is declared at its own masked path.
fn anthropic_request() -> ChatRequest {
    request_with(IngressDialect::Anthropic, &json!({"metadata": {}}), true)
}

// ---------------------------------------------------------------------------
// Egress shapes
// ---------------------------------------------------------------------------

/// The client's body as a fidelity egress sends it: the model the request
/// names, its extras merged at the top level, its system blocks and tools,
/// and its top-level cache marker.
fn faithful(req: &ChatRequest, mut body: Value) -> Value {
    body["model"] = json!(req.model);
    if let Some(extras) = req.provider_extras.as_ref().and_then(Value::as_object) {
        for (key, value) in extras {
            body[key] = value.clone();
        }
    }
    if let Some(system) = req.system.as_ref() {
        body["system"] = serde_json::to_value(system).expect("serialize");
    }
    if let Some(tools) = req.tools.as_ref() {
        body["tools"] = serde_json::to_value(tools).expect("serialize");
    }
    if let Some(marker) = req.cache_control.as_ref() {
        body["cache_control"] = serde_json::to_value(marker).expect("serialize");
    }
    if req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL) {
        body[TEST_UNSUPPORTED_PATH] = json!(true);
    }
    body
}

/// A faithful body whose operator extra went out with another value than
/// the request holds, so the request's slot is not the wire's.
fn rewritten_on_the_wire(req: &ChatRequest, client: Value) -> Value {
    let mut body = faithful(req, client);
    if body["metadata"][OP_KEY].is_string() {
        body["metadata"][OP_KEY] = json!("wire-only");
    }
    body
}

/// A faithful body that lost the operator-edited key on the way out.
fn op_key_dropped(req: &ChatRequest, client: Value) -> Value {
    let mut body = faithful(req, client);
    if let Some(metadata) = body["metadata"].as_object_mut() {
        metadata.remove(OP_KEY);
    }
    body
}

/// The client's body with nothing routectl added.
fn client_only(_: &ChatRequest, client: Value) -> Value {
    client
}

// ---------------------------------------------------------------------------
// Upstream answers
// ---------------------------------------------------------------------------

/// An anthropic-api 400 whose message names `path` as a refused field.
fn rejection(path: &str) -> Error {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": format!("{path}: {REFUSED_FIELD}")},
    });
    Error::upstream("scripted", 400, body.to_string())
}

fn carries_op_key(req: &ChatRequest) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(|extras| extras.get("metadata"))
        .and_then(|metadata| metadata.get(OP_KEY))
        .is_some()
}

fn refuse_op_key_while_carried(req: &ChatRequest) -> Result<()> {
    if carries_op_key(req) {
        return Err(rejection(OP_KEY_PATH));
    }
    Ok(())
}

fn refuse_op_key_then_unavailable(req: &ChatRequest) -> Result<()> {
    refuse_op_key_while_carried(req)?;
    Err(Error::upstream("scripted", 503, "down"))
}

fn refuse_cache_marker_while_carried(req: &ChatRequest) -> Result<()> {
    if req.cache_control.is_some() {
        return Err(rejection("cache_control"));
    }
    Ok(())
}

fn system_marker(req: &ChatRequest) -> bool {
    matches!(
        req.system.as_ref(),
        Some(SystemContent::Blocks(blocks)) if blocks[0].cache_control.is_some()
    )
}

fn refuse_system_marker_while_carried(req: &ChatRequest) -> Result<()> {
    if system_marker(req) {
        return Err(rejection("system.0.cache_control"));
    }
    Ok(())
}

/// `req` with `value` also at `key` of its client body.
fn with_client_member(mut req: ChatRequest, key: &str, value: Value) -> ChatRequest {
    let ingress = req
        .routectl_internal
        .ingress_wire
        .as_ref()
        .expect("ingress");
    let mut client: Value = serde_json::from_slice(&ingress.bytes).expect("ingress json");
    client[key] = value;
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

/// A request whose client sent one system block with no cache marker.
fn system_block_request() -> ChatRequest {
    let block = json!([{"type": "text", "text": "be brief"}]);
    let req = request_with(IngressDialect::Anthropic, &json!({"metadata": {}}), true);
    let mut req = with_client_member(req, "system", block.clone());
    req.system = Some(serde_json::from_value(block).expect("system blocks"));
    req
}

fn tool_marker(req: &ChatRequest) -> bool {
    matches!(
        req.tools.as_deref(),
        Some([ToolDef::Custom(tool)]) if tool.cache_control.is_some()
    )
}

fn refuse_tool_marker_while_carried(req: &ChatRequest) -> Result<()> {
    if tool_marker(req) {
        return Err(rejection("tools.0.cache_control"));
    }
    Ok(())
}

/// A request whose client sent one custom tool with no cache marker and no
/// system prompt, so the front marker is placed on that tool.
fn custom_tool_request() -> ChatRequest {
    let tools = json!([{"name": "lookup", "input_schema": {"type": "object"}}]);
    let req = request_with(IngressDialect::Anthropic, &json!({"metadata": {}}), true);
    let mut req = with_client_member(req, "tools", tools.clone());
    req.tools = Some(serde_json::from_value(tools).expect("custom tools"));
    req
}

fn refuse_model(_: &ChatRequest) -> Result<()> {
    Err(rejection("model"))
}

fn refuse_test_row_while_carried(req: &ChatRequest) -> Result<()> {
    if req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL) {
        return Err(rejection(TEST_UNSUPPORTED_PATH));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch and observation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// What one dispatch did.
struct Outcome {
    served: bool,
    field_repairs: usize,
    learned: usize,
    events: Vec<CapturedEvent>,
}

async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) -> Outcome {
    let run = async {
        match surface {
            Surface::Complete => {
                let out = Box::pin(router.complete_with_options(req, RouterOptions::new())).await;
                (out.result.is_ok(), out.meta)
            }
            Surface::Stream => {
                let out = Box::pin(router.stream_with_options(req, RouterOptions::new())).await;
                (out.result.is_ok(), out.meta)
            }
        }
    };
    let ((served, meta), events) = routectl_testkit::with_capture(run).await;
    Outcome {
        served,
        field_repairs: meta.field_repairs.len(),
        learned: meta.learned_capabilities.len(),
        events,
    }
}

/// `(provenance, outcome)` of every rejection finding, oldest first.
fn rejections(snapshot: &WireCheckSnapshot) -> Vec<(&'static str, FieldRepairOutcome)> {
    snapshot
        .findings
        .iter()
        .filter_map(|f| Some((f.provenance?.label(), f.field_repair_outcome?)))
        .collect()
}

fn edit_rejected_warnings(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|event| event.message == EDIT_REJECTED)
        .collect()
}

/// `(lane, path, edit_site)` of every `routectl_edit_rejected` warning.
fn warned(events: &[CapturedEvent]) -> Vec<(String, String, String)> {
    edit_rejected_warnings(events)
        .into_iter()
        .map(|event| {
            let field = |name| event.field(name).unwrap_or_default().to_owned();
            (field("lane"), field("path"), field("edit_site"))
        })
        .collect()
}

/// Nothing a rejected routectl edit drew may outlive the request: no learned
/// capability, no field verdict.
fn assert_nothing_learned(router: &Router, outcome: &Outcome, name: &str) {
    assert_eq!(outcome.learned, 0, "{name}: no learned event");
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "{name}: the learned registry stays empty"
    );
    assert_eq!(
        router.field_verdicts().snapshot_len(),
        0,
        "{name}: no field verdict"
    );
}

/// One rejection dispatched end to end on a one-target chain.
struct Row {
    name: &'static str,
    extras: &'static str,
    cache: bool,
    request: fn() -> ChatRequest,
    shape: Shape,
    answer: Answer,
    /// Upstream calls the dispatch took: two when the attempt was re-sent.
    calls: usize,
    served: bool,
    /// The field repairs the dispatch recorded.
    field_repairs: usize,
    /// The wire check's findings: provenance label and outcome.
    findings: &'static [(&'static str, FieldRepairOutcome)],
    /// The `routectl_edit_rejected` warning's `(lane, path, edit_site)`.
    warning: Option<(&'static str, &'static str, &'static str)>,
}

async fn run_rows(rows: &[Row]) {
    for surface in SURFACES {
        for row in rows {
            let name = format!("{surface:?} {}", row.name);
            let provider = ScriptedProvider::new(Some(row.shape), row.answer);
            let block = format!("{ANTHROPIC}{}", row.extras);
            let router = router(&block, row.cache, &[Arc::clone(&provider)]);

            let outcome = dispatch(&router, surface, (row.request)()).await;

            assert_eq!(provider.calls(), row.calls, "{name}: upstream calls");
            assert_eq!(outcome.served, row.served, "{name}: served");
            assert_eq!(
                outcome.field_repairs, row.field_repairs,
                "{name}: field repairs"
            );
            assert_eq!(
                rejections(&router.wire_check_snapshot()),
                row.findings,
                "{name}: one finding per rejection"
            );
            let expected: Vec<(String, String, String)> = row
                .warning
                .iter()
                .map(|(lane, path, site)| ((*lane).into(), (*path).into(), (*site).into()))
                .collect();
            assert_eq!(warned(&outcome.events), expected, "{name}: the warning");
            if row.field_repairs == 0 {
                assert_nothing_learned(&router, &outcome, &name);
            }
        }
    }
}

const ANTHROPIC_LANE: &str = "anthropic->anthropic-api";

// ---------------------------------------------------------------------------
// One outcome per cause
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_revertible_edit_is_reverted_and_the_retry_repairs_the_rejection() {
    run_rows(&[
        Row {
            name: "operator extra added under a masked container",
            extras: OPERATOR_EXTRA,
            cache: false,
            request: anthropic_request,
            shape: faithful,
            answer: refuse_op_key_while_carried,
            calls: 2,
            served: true,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Repaired)],
            warning: Some((ANTHROPIC_LANE, MASKED_OP_KEY, "operator_payload_extras")),
        },
        Row {
            name: "top-level cache marker placed",
            extras: "",
            cache: true,
            request: anthropic_request,
            shape: faithful,
            answer: refuse_cache_marker_while_carried,
            calls: 2,
            served: true,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Repaired)],
            warning: Some((ANTHROPIC_LANE, "/cache_control", "cache_placement")),
        },
        Row {
            name: "system block cache marker placed",
            extras: "",
            cache: true,
            request: system_block_request,
            shape: faithful,
            answer: refuse_system_marker_while_carried,
            calls: 2,
            served: true,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Repaired)],
            warning: Some((ANTHROPIC_LANE, "/system/0/cache_control", "cache_placement")),
        },
        Row {
            name: "custom tool cache marker placed",
            extras: "",
            cache: true,
            request: custom_tool_request,
            shape: faithful,
            answer: refuse_tool_marker_while_carried,
            calls: 2,
            served: true,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Repaired)],
            warning: Some((ANTHROPIC_LANE, "/tools/0/cache_control", "cache_placement")),
        },
    ])
    .await;
}

#[tokio::test]
async fn an_operator_extra_rewriting_a_client_value_is_reverted_to_that_value() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(Some(faithful), |req| {
            let rewritten = req
                .provider_extras
                .as_ref()
                .and_then(|extras| extras.get("tier"))
                != Some(&json!("client"));
            if rewritten {
                return Err(rejection("tier"));
            }
            Ok(())
        });
        let block = format!("{ANTHROPIC}payload_extras = {{ tier = \"operator\" }}\n");
        let router = router(&block, false, &[Arc::clone(&provider)]);
        let req = request_with(IngressDialect::Anthropic, &json!({"tier": "client"}), true);

        let outcome = dispatch(&router, surface, req).await;

        assert_eq!(provider.calls(), 2, "{surface:?}: re-sent once");
        assert!(
            outcome.served,
            "{surface:?}: the client's value is accepted"
        );
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("client_changed", FieldRepairOutcome::Repaired)],
            "{surface:?}"
        );
        assert_eq!(
            warned(&outcome.events),
            [(
                ANTHROPIC_LANE.to_owned(),
                "/tier".to_owned(),
                "operator_payload_extras".to_owned()
            )],
            "{surface:?}"
        );
    }
}

#[tokio::test]
async fn a_rejection_the_revert_cannot_answer_is_unrepaired_and_learns_nothing() {
    run_rows(&[
        Row {
            name: "two declarations match the path: conflict",
            extras: TWO_OPERATOR_EXTRAS,
            cache: false,
            request: || {
                request_with(
                    IngressDialect::Anthropic,
                    &json!({"metadata": {"b": 1}}),
                    true,
                )
            },
            shape: faithful,
            answer: refuse_op_key_while_carried,
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Unrepaired)],
            warning: Some((ANTHROPIC_LANE, MASKED_OP_KEY, "conflict")),
        },
        Row {
            name: "the declared site is not revertible",
            extras: "",
            cache: false,
            request: anthropic_request,
            shape: faithful,
            answer: refuse_model,
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("client_changed", FieldRepairOutcome::Unrepaired)],
            warning: Some((ANTHROPIC_LANE, "/model", "model_rename")),
        },
        Row {
            name: "postcondition: the request's slot is not the wire's",
            extras: OPERATOR_EXTRA,
            cache: false,
            request: anthropic_request,
            shape: rewritten_on_the_wire,
            answer: refuse_op_key_while_carried,
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Unrepaired)],
            warning: Some((ANTHROPIC_LANE, MASKED_OP_KEY, "operator_payload_extras")),
        },
        Row {
            name: "postcondition: undoing leaves the slot unlike the client's wire body",
            extras: OPERATOR_EXTRA,
            cache: false,
            request: || {
                let mut req = request_with(
                    IngressDialect::Anthropic,
                    &json!({"metadata": {"op_key": "client"}}),
                    true,
                );
                req.provider_extras = Some(json!({"metadata": {}}));
                req
            },
            shape: faithful,
            answer: refuse_op_key_while_carried,
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("client_changed", FieldRepairOutcome::Unrepaired)],
            warning: Some((ANTHROPIC_LANE, MASKED_OP_KEY, "operator_payload_extras")),
        },
        Row {
            // The rejected path is also a closed-table field the request
            // carries, so a field repair would take if a proposer were asked.
            name: "postcondition fails on a path the field table also names",
            extras: OPERATOR_TEST_ROW,
            cache: false,
            request: || {
                let mut req = anthropic_request();
                req.user = Some(TEST_UNSUPPORTED_SENTINEL.to_string());
                req
            },
            shape: faithful,
            answer: refuse_test_row_while_carried,
            calls: 1,
            served: false,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Unrepaired)],
            warning: Some((
                ANTHROPIC_LANE,
                "/routectl_test_unsupported_field",
                "operator_payload_extras",
            )),
        },
        Row {
            name: "the reverted retry fails",
            extras: OPERATOR_EXTRA,
            cache: false,
            request: anthropic_request,
            shape: faithful,
            answer: refuse_op_key_then_unavailable,
            calls: 2,
            served: false,
            field_repairs: 0,
            findings: &[("routectl_added", FieldRepairOutcome::Unrepaired)],
            warning: Some((ANTHROPIC_LANE, MASKED_OP_KEY, "operator_payload_extras")),
        },
    ])
    .await;
}

#[path = "learn_revert_scope_tests.rs"]
mod learn_revert_scope_tests;

#[path = "learn_revert_warning_tests.rs"]
mod learn_revert_warning_tests;
