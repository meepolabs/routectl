//! The wire side of failed attempts on both dispatch surfaces: a failed
//! attempt gives back its sampler charge, and an upstream rejection always
//! gets the full request check and a provenance for the path it names,
//! joined at dispatch end with what field repair made of it. Test providers
//! stand in for the egress deposit and the upstream's answer.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use routectl_core::wire_check::{
    EgressKind, IngressDialect, IngressWire, Lane, RejectionProvenance,
};
use routectl_core::{
    BetaAssembly, ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Error,
    Message, MessageContent, Provider, Result, Role,
};
use serde_json::{Value, json};

use super::super::field_repair::{
    FIELD_ACTION_CEILING_REPAIR, FIELD_ACTION_DROP_REPAIR, FIELD_ACTION_LOSSY_DROP_REPAIR,
    FIELD_ACTION_REWRITE_REPAIR, FIELD_REASON_UPSTREAM_REJECTION, TEST_REWRITE_ACCEPTED,
    TEST_REWRITE_KWARG, TEST_REWRITE_PATH, TEST_UNSUPPORTED_PATH, TEST_UNSUPPORTED_SENTINEL,
};
use super::super::rejection_parser::rejected_wire_path;
use super::PendingWireRejection;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{DispatchMeta, FieldRepair, Router, RouterOptions};
use crate::wire_check_state::{CheckOutcome, FieldRepairOutcome, WireCheckSnapshot};

const ALIAS: &str = "a";
const NEVER_SAMPLED: &str = "first_n = 0\nsample_one_in = 1000";
const WARN_TARGET: &str = "routectl_router::wire_check";
const UNREPAIRED_WARN: &str = "upstream rejected a request field and it was not repaired";

/// How the test provider builds the body it deposits from the client's.
type Shape = fn(&ChatRequest, Value) -> Value;
/// How the test provider answers an attempt, given the request.
type Answer = fn(&ChatRequest) -> Result<()>;

/// A provider that, like a real egress, deposits the body it sent into the
/// attempt's tap, then answers as told.
struct ScriptedProvider {
    shape: Shape,
    answer: Answer,
}

impl ScriptedProvider {
    fn boxed(shape: Shape, answer: Answer) -> Arc<dyn Provider> {
        Arc::new(Self { shape, answer })
    }

    fn run(&self, req: &ChatRequest) -> Result<()> {
        if let (Some(tap), Some(ingress)) = (
            req.routectl_internal.wire_tap.as_ref(),
            req.routectl_internal.ingress_wire.as_ref(),
        ) {
            let client: Value = serde_json::from_slice(&ingress.bytes).expect("ingress json");
            let sent = (self.shape)(req, client);
            tap.deposit(serde_json::to_vec(&sent).expect("serialize").into());
        }
        (self.answer)(req)
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.run(&req).map(|()| ok_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.run(&req)
            .map(|()| stream::iter([Ok(content_chunk())]).boxed())
    }
}

fn ok_response() -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// An anthropic-api 400 whose message names `path` with `reason`.
fn rejection(path: &str, reason: &str) -> Error {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": format!("{path}: {reason}")},
    });
    Error::upstream("scripted", 400, body.to_string())
}

const REFUSED_FIELD: &str = "Extra inputs are not permitted";

/// A router whose alias walks one anthropic-api target per provider in
/// `chain`, one call per target.
fn router(wire_check: &str, chain: Vec<Arc<dyn Provider>>) -> Router {
    let mut toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [wire_check]\n{wire_check}\n"
    );
    let mut nicknames = Vec::new();
    for idx in 0..chain.len() {
        toml_text.push_str(&format!(
            "[providers.p{idx}]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n"
        ));
        nicknames.push(format!("\"m{idx}\""));
    }
    toml_text.push_str(&format!(
        "[aliases]\n{ALIAS} = [{}]\n",
        nicknames.join(", ")
    ));
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let models: BTreeMap<String, Arc<ResolvedModel>> = chain
        .into_iter()
        .enumerate()
        .map(|(idx, provider)| {
            let nickname = format!("m{idx}");
            let model = ResolvedModel::new(&nickname, format!("p{idx}"), provider, "upstream");
            (nickname, Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

fn client_body() -> Value {
    json!({
        "model": ALIAS,
        "max_tokens": 16,
        "metadata": {"user_id": "u-1"},
        "messages": [{"role": "user", "content": "hi"}]
    })
}

fn request(dialect: IngressDialect) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        ..Default::default()
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: None,
        bytes: serde_json::to_vec(&client_body())
            .expect("serialize")
            .into(),
    });
    req
}

/// A request carrying the test-only refused-field row's surface, so field
/// repair has a row to drop when the upstream names it.
fn repairable_request() -> ChatRequest {
    let mut req = request(IngressDialect::Anthropic);
    req.user = Some(TEST_UNSUPPORTED_SENTINEL.to_string());
    req
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// Dispatch `req` on `surface`; whether it succeeded.
async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) -> bool {
    match surface {
        Surface::Complete => Box::pin(router.complete_with_options(req, RouterOptions::new()))
            .await
            .result
            .is_ok(),
        Surface::Stream => Box::pin(router.stream_with_options(req, RouterOptions::new()))
            .await
            .result
            .is_ok(),
    }
}

/// `(path, provenance, field-repair outcome)` of every rejection finding, oldest first.
fn rejections(snapshot: &WireCheckSnapshot) -> Vec<(String, &'static str, FieldRepairOutcome)> {
    snapshot
        .findings
        .iter()
        .filter_map(|f| {
            Some((
                f.path.as_str().to_string(),
                f.provenance?.label(),
                f.field_repair_outcome?,
            ))
        })
        .collect()
}

fn check_count(snapshot: &WireCheckSnapshot) -> u64 {
    snapshot.checks.iter().map(|c| c.reading.count).sum()
}

fn unchanged(_: &ChatRequest, client: Value) -> Value {
    client
}

fn with_added_key(_: &ChatRequest, mut client: Value) -> Value {
    client["added_by_routectl"] = json!(true);
    client
}

fn with_rewritten_max_tokens(_: &ChatRequest, mut client: Value) -> Value {
    client["max_tokens"] = json!(4096);
    client
}

#[tokio::test]
async fn a_rejected_path_is_classified_by_where_it_came_from() {
    let rows: [(&str, IngressDialect, Shape, Answer, &str, &str); 5] = [
        (
            "a key routectl added",
            IngressDialect::Anthropic,
            with_added_key,
            |_| Err(rejection("added_by_routectl", REFUSED_FIELD)),
            "/added_by_routectl",
            "routectl_added",
        ),
        (
            "a client key sent unchanged",
            IngressDialect::Anthropic,
            unchanged,
            |_| Err(rejection("metadata.user_id", REFUSED_FIELD)),
            "/metadata/*",
            "client_unchanged",
        ),
        (
            "a client key routectl rewrote",
            IngressDialect::Anthropic,
            with_rewritten_max_tokens,
            |_| Err(rejection("max_tokens", "Input should be less than 1024")),
            "/max_tokens",
            "client_changed",
        ),
        (
            "a rejection naming no path",
            IngressDialect::Anthropic,
            unchanged,
            |_| {
                Err(Error::upstream(
                    "scripted",
                    400,
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"malformed request"}}"#,
                ))
            },
            "",
            "unlocalized",
        ),
        (
            "a located path on a translation lane",
            IngressDialect::Chat,
            unchanged,
            |_| Err(rejection("metadata.user_id", REFUSED_FIELD)),
            "/metadata/*",
            "n_a",
        ),
    ];
    for surface in SURFACES {
        for (name, dialect, shape, answer, path, provenance) in rows {
            let router = router(NEVER_SAMPLED, vec![ScriptedProvider::boxed(shape, answer)]);

            let served = dispatch(&router, surface, request(dialect)).await;

            assert!(!served, "{surface:?} {name}: premise, the 400 is terminal");
            let snapshot = router.wire_check_snapshot();
            assert_eq!(
                rejections(&snapshot),
                [(path.to_string(), provenance, FieldRepairOutcome::Unrepaired)],
                "{surface:?} {name}"
            );
        }
    }
}

#[tokio::test]
async fn a_rejection_is_checked_even_when_the_sampler_admits_nothing() {
    for surface in SURFACES {
        let provider = ScriptedProvider::boxed(with_added_key, |_| {
            Err(rejection("added_by_routectl", REFUSED_FIELD))
        });
        let router = router(NEVER_SAMPLED, vec![provider]);

        dispatch(&router, surface, request(IngressDialect::Anthropic)).await;

        let snapshot = router.wire_check_snapshot();
        assert_eq!(check_count(&snapshot), 1, "{surface:?}: one full check");
        assert!(
            snapshot
                .checks
                .iter()
                .all(|c| c.outcome != CheckOutcome::Incomplete),
            "{surface:?}: the check compared the deposited body"
        );
    }
}

#[tokio::test]
async fn a_server_error_runs_no_rejection_check() {
    for surface in SURFACES {
        let provider =
            ScriptedProvider::boxed(unchanged, |_| Err(Error::upstream("scripted", 500, "boom")));
        let router = router("first_n = 0\nsample_one_in = 1", vec![provider]);

        dispatch(&router, surface, request(IngressDialect::Anthropic)).await;

        let snapshot = router.wire_check_snapshot();
        assert!(snapshot.checks.is_empty(), "{surface:?}");
        assert!(snapshot.findings.is_empty(), "{surface:?}");
    }
}

fn lose_metadata(_: &ChatRequest, mut client: Value) -> Value {
    client
        .as_object_mut()
        .expect("object body")
        .remove("metadata");
    client
}

#[tokio::test]
async fn a_failed_attempt_gives_back_its_sampler_charge() {
    for surface in SURFACES {
        let failing =
            ScriptedProvider::boxed(unchanged, |_| Err(Error::upstream("scripted", 503, "down")));
        let serving = ScriptedProvider::boxed(lose_metadata, |_| Ok(()));
        let router = router("first_n = 1\nsample_one_in = 1000", vec![failing, serving]);

        let served = dispatch(&router, surface, request(IngressDialect::Anthropic)).await;

        assert!(served, "{surface:?}: premise, the fallback serves");
        let snapshot = router.wire_check_snapshot();
        assert_eq!(
            snapshot.loss.iter().map(|c| c.reading.count).sum::<u64>(),
            1,
            "{surface:?}: the first success is the first checked, so its loss is recorded"
        );
        assert_eq!(snapshot.findings[0].path.as_str(), "/metadata");
    }
}

/// Deposit the test row's wire field while the request still carries its
/// surface, as an egress emitting it would; the repaired retry drops it.
fn with_test_row_field(req: &ChatRequest, mut client: Value) -> Value {
    if req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL) {
        client[TEST_UNSUPPORTED_PATH] = json!(true);
    }
    client
}

fn carries_test_row(req: &ChatRequest) -> bool {
    req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL)
}

/// The value the client sends in the test-only rewrite field: outside the
/// row's accepted set.
const CLIENT_REWRITE_VALUE: &str = "routectl-test-client-value";

fn rewrite_value(req: &ChatRequest) -> Option<&str> {
    req.chat_template_kwargs
        .as_ref()
        .and_then(|kwargs| kwargs.get(TEST_REWRITE_KWARG))
        .and_then(Value::as_str)
}

/// A request carrying the test-only rewrite row's surface with a value
/// outside its accepted set.
fn rewritable_request() -> ChatRequest {
    let mut req = request(IngressDialect::Anthropic);
    req.chat_template_kwargs = Some(json!({ TEST_REWRITE_KWARG: CLIENT_REWRITE_VALUE }));
    req
}

/// Deposit the rewrite field's value while the request carries one, as an
/// egress emitting it would.
fn with_rewrite_field(req: &ChatRequest, mut client: Value) -> Value {
    if let Some(value) = rewrite_value(req) {
        client[TEST_REWRITE_PATH] = json!(value);
    }
    client
}

/// Refuse the client's rewrite value, naming exactly one accepted value, and
/// serve the rewritten retry.
fn refuse_client_rewrite_value(req: &ChatRequest) -> Result<()> {
    if rewrite_value(req) == Some(CLIENT_REWRITE_VALUE) {
        let named = TEST_REWRITE_ACCEPTED[0];
        return Err(rejection(
            TEST_REWRITE_PATH,
            &format!("Input should be '{named}'"),
        ));
    }
    Ok(())
}

/// One outcome-table row: the request, what the egress deposits, how the
/// upstream answers, the joined outcome and its label, and how many
/// unrepaired WARNs fire.
struct OutcomeRow {
    name: &'static str,
    request: fn() -> ChatRequest,
    shape: Shape,
    answer: Answer,
    path: &'static str,
    expected: FieldRepairOutcome,
    label: &'static str,
    warns: usize,
}

#[tokio::test]
async fn the_field_repair_outcome_joins_its_rejection_and_only_unrepaired_warns() {
    let rows = [
        OutcomeRow {
            name: "repaired",
            request: repairable_request,
            shape: with_test_row_field,
            answer: |req| {
                if carries_test_row(req) {
                    return Err(rejection(TEST_UNSUPPORTED_PATH, REFUSED_FIELD));
                }
                Ok(())
            },
            path: TEST_UNSUPPORTED_PATH,
            expected: FieldRepairOutcome::Repaired,
            label: "repaired",
            warns: 0,
        },
        OutcomeRow {
            name: "repaired lossy",
            request: repairable_request,
            shape: with_test_row_field,
            answer: |req| {
                if carries_test_row(req) {
                    return Err(rejection(TEST_UNSUPPORTED_PATH, "Input should be 'x'"));
                }
                Ok(())
            },
            path: TEST_UNSUPPORTED_PATH,
            expected: FieldRepairOutcome::RepairedLossy,
            label: "repaired_lossy",
            warns: 0,
        },
        OutcomeRow {
            name: "rewritten",
            request: rewritable_request,
            shape: with_rewrite_field,
            answer: refuse_client_rewrite_value,
            path: TEST_REWRITE_PATH,
            expected: FieldRepairOutcome::RepairedLossy,
            label: "repaired_lossy",
            warns: 0,
        },
        OutcomeRow {
            name: "unrepaired",
            request: repairable_request,
            shape: with_test_row_field,
            answer: |_| Err(rejection(TEST_UNSUPPORTED_PATH, REFUSED_FIELD)),
            path: TEST_UNSUPPORTED_PATH,
            expected: FieldRepairOutcome::Unrepaired,
            label: "unrepaired",
            warns: 1,
        },
    ];
    for surface in SURFACES {
        for row in &rows {
            let name = row.name;
            let provider = ScriptedProvider::boxed(row.shape, row.answer);
            let router = router(NEVER_SAMPLED, vec![provider]);

            let (served, events) =
                routectl_testkit::with_capture(dispatch(&router, surface, (row.request)())).await;

            assert_eq!(
                served,
                row.warns == 0,
                "{surface:?} {name}: only an unrepaired rejection reaches the client"
            );
            let found = rejections(&router.wire_check_snapshot());
            let first = found
                .first()
                .unwrap_or_else(|| panic!("{surface:?} {name}: the rejection is recorded"));
            assert_eq!(
                (first.0.as_str(), first.1, first.2),
                (
                    format!("/{}", row.path).as_str(),
                    "routectl_added",
                    row.expected
                ),
                "{surface:?} {name}: classified against the pre-repair body"
            );
            assert_eq!(
                first.2.label(),
                row.label,
                "{surface:?} {name}: the outcome label the finding carries"
            );
            let warned = events
                .iter()
                .filter(|e| e.target == WARN_TARGET && e.message == UNREPAIRED_WARN)
                .count();
            assert_eq!(warned, row.warns, "{surface:?} {name}");
        }
    }
}

/// The client beta flag the upstream rejects by name.
const NAMED_BETA: &str = "zz-named-2099-01-01";

/// A request whose client beta set carries the flag the upstream rejects.
fn beta_request() -> ChatRequest {
    let mut req = request(IngressDialect::Anthropic);
    req.anthropic_beta = vec![NAMED_BETA.to_string()];
    req
}

/// Record the empty beta assembly an egress records, so the named flag is
/// a removable client flag, and say whether the attempt withheld it.
fn assemble_betas(req: &ChatRequest) -> bool {
    if let Some(slot) = req.routectl_internal.beta_assembly.as_ref() {
        slot.record(BetaAssembly::new(Vec::new(), Vec::new()));
    }
    req.routectl_internal
        .withheld_betas
        .iter()
        .any(|flag| flag == NAMED_BETA)
}

/// The anthropic-api 400 naming `NAMED_BETA` in the `anthropic-beta` header.
fn beta_rejection() -> Error {
    let body = json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!(
                "Unexpected value(s) `{NAMED_BETA}` for the `anthropic-beta` header. Please \
                 consult our documentation at platform.claude.com/docs or try again without \
                 the header."
            ),
        },
    });
    Error::upstream("scripted", 400, body.to_string())
}

/// A 400 naming no path, answering the repaired attempt.
fn unlocalized_rejection() -> Error {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": "the request is malformed"},
    });
    Error::upstream("scripted", 400, body.to_string())
}

#[tokio::test]
async fn a_beta_rejection_takes_the_outcome_of_its_repaired_attempt() {
    let rows: [(&str, Answer, bool, Vec<FieldRepairOutcome>); 2] = [
        (
            "repaired attempt succeeds",
            |req| {
                if assemble_betas(req) {
                    return Ok(());
                }
                Err(beta_rejection())
            },
            true,
            vec![FieldRepairOutcome::RepairedLossy],
        ),
        (
            "repaired attempt fails",
            |req| {
                if assemble_betas(req) {
                    return Err(unlocalized_rejection());
                }
                Err(beta_rejection())
            },
            false,
            vec![
                FieldRepairOutcome::Unrepaired,
                FieldRepairOutcome::Unrepaired,
            ],
        ),
    ];
    for surface in SURFACES {
        for (name, answer, serves, expected) in &rows {
            let provider = ScriptedProvider::boxed(unchanged, *answer);
            let router = router(NEVER_SAMPLED, vec![provider]);

            let served = dispatch(&router, surface, beta_request()).await;

            assert_eq!(served, *serves, "{surface:?} {name}: served");
            let found = rejections(&router.wire_check_snapshot());
            let outcomes: Vec<FieldRepairOutcome> = found.iter().map(|f| f.2).collect();
            assert_eq!(
                &outcomes, expected,
                "{surface:?} {name}: outcomes, oldest first"
            );
            assert_eq!(
                (found[0].0.as_str(), found[0].1),
                ("", "unlocalized"),
                "{surface:?} {name}: a header rejection names no body path"
            );
        }
    }
}

const CONTAINER_KEY_PATH: &str = "metadata.user_id";
const CONTAINER_KEY: &str = "user_id";
const STATE_KEY: &str = "p0";

/// A pending rejection for an anthropic-api 400 naming `metadata.user_id`,
/// recorded before any repair.
fn container_key_rejection() -> PendingWireRejection {
    let err = rejection(CONTAINER_KEY_PATH, REFUSED_FIELD);
    let named = rejected_wire_path("anthropic-api", 400, err.parsed_body())
        .expect("premise: the rejection localizes");
    PendingWireRejection {
        lane: Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi),
        path: named.wire,
        field: Some(named.field),
        provenance: RejectionProvenance::ClientUnchanged,
        state_key: STATE_KEY.to_string(),
        repairs_before: 0,
        beta_repaired: false,
    }
}

fn repair_of(field_path: &'static str, succeeded: bool) -> FieldRepair {
    FieldRepair {
        action: FIELD_ACTION_DROP_REPAIR,
        state_key: STATE_KEY.to_string(),
        field_path,
        reason: FIELD_REASON_UPSTREAM_REJECTION,
        repair_attempted: true,
        repair_succeeded: succeeded,
        learned: false,
    }
}

#[test]
fn a_rejected_container_key_joins_its_repair_by_the_unmasked_field() {
    let rows: [(&str, Vec<FieldRepair>, FieldRepairOutcome); 3] = [
        (
            "a successful repair of the same key",
            vec![repair_of(CONTAINER_KEY_PATH, true)],
            FieldRepairOutcome::Repaired,
        ),
        ("no repair", vec![], FieldRepairOutcome::Unrepaired),
        (
            "a successful repair of a sibling key",
            vec![repair_of("metadata.other_id", true)],
            FieldRepairOutcome::Unrepaired,
        ),
    ];
    for (name, repairs, expected) in rows {
        let router = router(NEVER_SAMPLED, vec![]);
        let mut meta = DispatchMeta::for_alias(ALIAS);
        meta.wire_rejections.push(container_key_rejection());
        meta.field_repairs = repairs;

        router.emit_wire_rejections(&meta);

        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("/metadata/*".to_string(), "client_unchanged", expected)],
            "{name}: the rendered path stays masked"
        );
    }
}

/// Mutation check: drop the ceiling token from the lossy arm of
/// `field_repair_outcome` -> red on `ceiling`.
#[test]
fn each_succeeded_repair_action_joins_its_own_outcome() {
    let rows = [
        (
            "drop",
            FIELD_ACTION_DROP_REPAIR,
            FieldRepairOutcome::Repaired,
        ),
        (
            "lossy drop",
            FIELD_ACTION_LOSSY_DROP_REPAIR,
            FieldRepairOutcome::RepairedLossy,
        ),
        (
            "rewrite",
            FIELD_ACTION_REWRITE_REPAIR,
            FieldRepairOutcome::RepairedLossy,
        ),
        (
            "ceiling",
            FIELD_ACTION_CEILING_REPAIR,
            FieldRepairOutcome::RepairedLossy,
        ),
    ];
    for (name, action, expected) in rows {
        let router = router(NEVER_SAMPLED, vec![]);
        let mut meta = DispatchMeta::for_alias(ALIAS);
        meta.wire_rejections.push(container_key_rejection());
        meta.field_repairs = vec![FieldRepair {
            action,
            ..repair_of(CONTAINER_KEY_PATH, true)
        }];

        router.emit_wire_rejections(&meta);

        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("/metadata/*".to_string(), "client_unchanged", expected)],
            "{name}",
        );
    }
}

#[test]
fn a_pending_rejection_debug_never_shows_the_unmasked_key() {
    let pending = container_key_rejection();

    let rendered = format!("{pending:?}");

    assert!(
        rendered.contains("/metadata/*"),
        "premise: the masked path renders: {rendered}"
    );
    assert!(!rendered.contains(CONTAINER_KEY), "{rendered}");
}
