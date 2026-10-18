//! A transcript whose tool calls and results are not correctly paired is
//! forwarded unchanged on both dispatch surfaces and to the token count.
//! Only a Gemini target refuses it, locally, and the walk moves on. An
//! upstream 400 answering such a transcript neither walks the chain, nor is
//! repaired, nor teaches any learner; a transient failure routes as it does
//! for any request.

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(feature = "gemini")]
use routectl_core::RefusalReason;
use routectl_core::wire_check::IngressDialect;
use routectl_core::{
    ChatRequest, Error, Message, MessageContent, Provider, Result, Role, ToolDef,
    ToolPairingDefect, validate_tool_pairing,
};
use serde_json::{Value, json};

use super::super::super::field_repair::TEST_UNSUPPORTED_PATH;
use super::{
    ALIAS, ANTHROPIC, INVOKE, NEVER_SAMPLED, OPENAI_COMPAT, ScriptedProvider, anthropic_message,
    anthropic_rejection, carries_test_row, carry_test_row, carry_web_search, openai_rejection,
    request,
};
#[cfg(feature = "gemini")]
use super::{GEMINI, router};
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::{DispatchMeta, Router, RouterOptions};

const FORCED_TOOL_CHOICE_FIXTURE: &str =
    include_str!("../../tests/fixtures/forced_tool_choice_capture.json");

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
    CountTokens,
}

/// The two messages surfaces, which walk the chain on a health failure.
const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// Every surface that dispatches a transcript upstream.
const FORWARDING_SURFACES: [Surface; 3] =
    [Surface::Complete, Surface::Stream, Surface::CountTokens];

/// What one dispatch returned and the meta it left.
struct Walk {
    result: Result<()>,
    meta: DispatchMeta,
}

async fn walk(router: &Router, surface: Surface, req: ChatRequest, opts: RouterOptions) -> Walk {
    match surface {
        Surface::Complete => {
            let out = Box::pin(router.complete_with_options(req, opts)).await;
            Walk {
                result: out.result.map(drop),
                meta: out.meta,
            }
        }
        Surface::Stream => {
            let out = Box::pin(router.stream_with_options(req, opts)).await;
            Walk {
                result: out.result.map(drop),
                meta: out.meta,
            }
        }
        Surface::CountTokens => {
            let out = Box::pin(router.count_tokens_with_meta(req)).await;
            Walk {
                result: out.result.map(drop),
                meta: out.meta,
            }
        }
    }
}

/// A router whose alias walks `first` (on an entry configured by
/// `first_block`) and then `second`, one call per attempt.
fn chain_router(
    first_block: &str,
    first: Arc<dyn Provider>,
    second_block: &str,
    second: Arc<dyn Provider>,
) -> Router {
    chain_router_with_retry("", first_block, first, second_block, second)
}

/// [`chain_router`] with `retry_tables` (whole `[retry.*]` tables) added to
/// the retry policy.
fn chain_router_with_retry(
    retry_tables: &str,
    first_block: &str,
    first: Arc<dyn Provider>,
    second_block: &str,
    second: Arc<dyn Provider>,
) -> Router {
    let toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         {retry_tables}[wire_check]\n{NEVER_SAMPLED}\n[providers.p0]\n{first_block}\
         [providers.p1]\n{second_block}\
         [aliases]\n{ALIAS} = [\"m0\", \"m1\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([
        (
            "m0".to_string(),
            Arc::new(ResolvedModel::new("m0", "p0", first, "upstream0")),
        ),
        (
            "m1".to_string(),
            Arc::new(ResolvedModel::new("m1", "p1", second, "upstream1")),
        ),
    ]));
    router
}

fn answers(_: &ChatRequest) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Transcripts
// ---------------------------------------------------------------------------

fn turn(role: Role, content: MessageContent) -> Message {
    Message {
        role,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn user(text: &str) -> Message {
    turn(Role::User, MessageContent::Text(text.into()))
}

fn assistant(text: &str) -> Message {
    turn(Role::Assistant, MessageContent::Text(text.into()))
}

fn calls(ids: &[&str]) -> Message {
    let tool_calls = ids
        .iter()
        .map(|id| json!({"id": id, "type": "function", "function": {"name": "lookup", "arguments": "{}"}}))
        .collect();
    Message {
        tool_calls: Some(tool_calls),
        ..turn(Role::Assistant, MessageContent::Null)
    }
}

fn result(id: &str) -> Message {
    Message {
        tool_call_id: Some(id.into()),
        ..turn(Role::Tool, MessageContent::Text("r".into()))
    }
}

/// One transcript per pairing rule.
fn defective_transcripts() -> [(ToolPairingDefect, Vec<Message>); 7] {
    [
        (
            ToolPairingDefect::OrphanResult,
            vec![user("q"), result("x")],
        ),
        (
            ToolPairingDefect::ResultBeforeCall,
            vec![user("q"), result("a"), calls(&["a"])],
        ),
        (
            ToolPairingDefect::CrossSegmentResult,
            vec![
                user("q"),
                calls(&["a"]),
                result("a"),
                assistant("t"),
                calls(&["b"]),
                result("b"),
                result("a"),
            ],
        ),
        (
            ToolPairingDefect::DuplicateResult,
            vec![user("q"), calls(&["a"]), result("a"), result("a")],
        ),
        (
            ToolPairingDefect::DuplicateCallId,
            vec![
                user("q"),
                calls(&["a"]),
                result("a"),
                calls(&["a"]),
                result("a"),
            ],
        ),
        (
            ToolPairingDefect::InterruptedCalls,
            vec![user("q"), calls(&["a"]), user("i"), result("a")],
        ),
        (
            ToolPairingDefect::TrailingPendingCalls,
            vec![user("q"), calls(&["a"])],
        ),
    ]
}

fn defective() -> Vec<Message> {
    vec![user("q"), result("x")]
}

fn paired() -> Vec<Message> {
    vec![user("q"), calls(&["a"]), result("a")]
}

fn request_with(messages: Vec<Message>) -> ChatRequest {
    let mut req = request(IngressDialect::Anthropic);
    req.messages = messages.into();
    req
}

fn as_json(messages: &[Message]) -> Value {
    serde_json::to_value(messages).expect("serialize messages")
}

fn unlocalized(router: &Router) -> u64 {
    router.field_repair_counters().parser_unlocalized
}

/// The matcher-drift signals the generic learner raises for a rejection it
/// could not attribute.
fn drift_signals(router: &Router) -> u64 {
    let feature_naming = router.metrics.feature_naming_unmatched_total();
    #[cfg(feature = "bedrock")]
    let feature_naming = feature_naming + router.metrics.bedrock_validation_unmatched_total();
    feature_naming
}

// ---------------------------------------------------------------------------
// Forwarded unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_pairing_defect_reaches_a_non_gemini_target_unchanged() {
    for surface in FORWARDING_SURFACES {
        for (defect, messages) in defective_transcripts() {
            assert_eq!(
                validate_tool_pairing(&messages).map_err(|e| e.defect()),
                Err(defect),
                "premise: the transcript breaks {defect}"
            );
            let provider = ScriptedProvider::new(None, answers);
            let router = super::router(ANTHROPIC, Arc::clone(&provider) as Arc<dyn Provider>);
            let sent = as_json(&messages);

            let out = walk(
                &router,
                surface,
                request_with(messages),
                RouterOptions::new(),
            )
            .await;

            assert!(out.result.is_ok(), "{surface:?} {defect}: {:?}", out.result);
            assert_eq!(
                *provider.messages.lock(),
                [sent],
                "{surface:?} {defect}: the messages the upstream received"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Gemini refuses locally
// ---------------------------------------------------------------------------

/// `(lane, reason, count)` for every local-refusal key the router holds.
#[cfg(feature = "gemini")]
fn refusals(router: &Router) -> Vec<(&'static str, &'static str, u64)> {
    router
        .wire_check_snapshot()
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
        .collect()
}

/// `(lane, reason, count)` local-refusal keys.
#[cfg(feature = "gemini")]
type Refusals = &'static [(&'static str, &'static str, u64)];

/// A chain of Gemini then another target: the case name, the transcript,
/// the calls each target takes, and the refusal keys left behind.
#[cfg(feature = "gemini")]
type GeminiChainRow = (&'static str, fn() -> Vec<Message>, usize, usize, Refusals);

#[cfg(feature = "gemini")]
#[tokio::test]
async fn a_gemini_target_refuses_a_defective_transcript_and_the_next_target_serves_it() {
    let rows: [GeminiChainRow; 2] = [
        (
            "defective: refused on gemini, served by the next target",
            defective,
            0,
            1,
            &[("anthropic->gemini", "tool-pairing", 1)],
        ),
        ("paired: gemini serves", paired, 1, 0, &[]),
    ];
    for surface in SURFACES {
        for (name, messages, gemini_calls, second_calls, expected) in rows {
            let gemini = ScriptedProvider::new(None, answers);
            let second = ScriptedProvider::new(None, answers);
            let router = chain_router(
                GEMINI,
                Arc::clone(&gemini) as Arc<dyn Provider>,
                ANTHROPIC,
                Arc::clone(&second) as Arc<dyn Provider>,
            );

            let out = walk(
                &router,
                surface,
                request_with(messages()),
                RouterOptions::new(),
            )
            .await;

            assert!(out.result.is_ok(), "{surface:?} {name}: {:?}", out.result);
            assert_eq!(
                gemini.calls(),
                gemini_calls,
                "{surface:?} {name}: gemini calls"
            );
            assert_eq!(
                second.calls(),
                second_calls,
                "{surface:?} {name}: second calls"
            );
            assert_eq!(refusals(&router), expected, "{surface:?} {name}: refusals");
        }
    }
}

#[cfg(feature = "gemini")]
#[tokio::test]
async fn a_defective_transcript_with_only_gemini_to_try_ends_in_the_pairing_refusal() {
    let expected_message = validate_tool_pairing(&defective())
        .expect_err("premise: defective")
        .to_string();
    for surface in SURFACES {
        for disable_fallbacks in [false, true] {
            let gemini = ScriptedProvider::new(None, answers);
            let second = ScriptedProvider::new(None, answers);
            let router = if disable_fallbacks {
                chain_router(
                    GEMINI,
                    Arc::clone(&gemini) as Arc<dyn Provider>,
                    ANTHROPIC,
                    Arc::clone(&second) as Arc<dyn Provider>,
                )
            } else {
                router(GEMINI, Arc::clone(&gemini) as Arc<dyn Provider>)
            };
            let mut opts = RouterOptions::new();
            opts.disable_fallbacks = disable_fallbacks;
            let case = if disable_fallbacks {
                "gemini first, fallbacks disabled"
            } else {
                "a gemini-only chain"
            };

            let out = walk(&router, surface, request_with(defective()), opts).await;

            let err = out.result.expect_err("the request is refused");
            assert!(
                matches!(&err, Error::Validation(message, RefusalReason::ToolPairing) if *message == expected_message),
                "{surface:?} {case}: {err:?}"
            );
            assert_eq!(gemini.calls(), 0, "{surface:?} {case}");
            assert_eq!(second.calls(), 0, "{surface:?} {case}");
            assert_eq!(
                refusals(&router),
                [("anthropic->gemini", "tool-pairing", 1)],
                "{surface:?} {case}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// An upstream rejection of a defective transcript
// ---------------------------------------------------------------------------

/// What one dispatch left behind.
#[derive(Debug, PartialEq)]
struct Observed {
    served: bool,
    first_calls: usize,
    second_calls: usize,
    field_repairs: usize,
    learned: Vec<String>,
    /// Parser-unlocalized count delta; `None` where the case does not pin it.
    unlocalized: Option<u64>,
    /// Matcher-drift signal delta.
    drift_signals: u64,
}

/// One rejecting first target, dispatched on a defective and on a paired
/// transcript.
struct RejectionRow {
    name: &'static str,
    first_block: &'static str,
    answer: super::Answer,
    carry: fn(&mut ChatRequest),
    defective: Observed,
    paired: Observed,
}

async fn run_rejection_row(row: &RejectionRow, surface: Surface) {
    for (transcript, messages, expected) in [
        ("defective", defective(), &row.defective),
        ("paired", paired(), &row.paired),
    ] {
        let first = ScriptedProvider::new(None, row.answer);
        let second = ScriptedProvider::new(None, answers);
        let router = chain_router(
            row.first_block,
            Arc::clone(&first) as Arc<dyn Provider>,
            ANTHROPIC,
            Arc::clone(&second) as Arc<dyn Provider>,
        );
        let mut req = request_with(messages);
        (row.carry)(&mut req);
        let before = unlocalized(&router);
        let drift_before = drift_signals(&router);

        let out = walk(&router, surface, req, RouterOptions::new()).await;

        let observed = Observed {
            served: out.result.is_ok(),
            first_calls: first.calls(),
            second_calls: second.calls(),
            field_repairs: out.meta.field_repairs.len(),
            learned: out
                .meta
                .learned_capabilities
                .iter()
                .map(|event| event.capability_key.clone())
                .collect(),
            unlocalized: expected.unlocalized.map(|_| unlocalized(&router) - before),
            drift_signals: drift_signals(&router) - drift_before,
        };
        assert_eq!(
            &observed, expected,
            "{surface:?} {}: {transcript}",
            row.name
        );
        assert!(
            out.meta.cleared_capabilities.is_empty(),
            "{surface:?} {}: {transcript}: no verdict is cleared",
            row.name
        );
    }
}

fn refuse_test_row(req: &ChatRequest) -> Result<()> {
    if carries_test_row(req) {
        return Err(anthropic_rejection(TEST_UNSUPPORTED_PATH));
    }
    Ok(())
}

/// An anthropic-api entry whose operator remap routes a 400 as a server
/// error.
const ANTHROPIC_400_AS_SERVER_ERROR: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n\
     [providers.p0.class_overrides]\n400 = \"server-error\"\n";

/// An openai-compat 400 refusing `web_search` as an unsupported parameter,
/// which the classifier lifts out of `BadRequest`.
fn refuse_web_search(_: &ChatRequest) -> Result<()> {
    Err(openai_rejection("web_search"))
}

/// A 400 naming no field, which no learner can attribute.
fn refuse_plainly(_: &ChatRequest) -> Result<()> {
    Err(anthropic_message("invalid request"))
}

fn carry_nothing(_: &mut ChatRequest) {}

/// The test row, on a request whose internal state still holds a pairing
/// defect from before this dispatch.
fn carry_test_row_and_stale_defect(req: &mut ChatRequest) {
    carry_test_row(req);
    req.routectl_internal.transcript_defect = Some(ToolPairingDefect::OrphanResult);
}

/// The captured Bedrock rejection of a forced `tool_choice`, which the
/// generic learner mints from on first sight.
fn bedrock_forced_tool_choice_rejection(_: &ChatRequest) -> Result<()> {
    let fixture: Value = serde_json::from_str(FORCED_TOOL_CHOICE_FIXTURE).expect("fixture json");
    Err(Error::upstream_full(
        "scripted",
        400,
        fixture["bedrock"]["invoke_body"]
            .as_str()
            .expect("invoke body")
            .to_string(),
        None,
        Some(
            fixture["bedrock"]["lifted_type"]
                .as_str()
                .expect("lifted type")
                .to_string(),
        ),
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

fn rejection_rows() -> Vec<RejectionRow> {
    let mut rows = vec![RejectionRow {
        name: "a field rejection the engine repairs",
        first_block: ANTHROPIC,
        answer: refuse_test_row,
        carry: carry_test_row,
        defective: Observed {
            served: false,
            first_calls: 1,
            second_calls: 0,
            field_repairs: 0,
            learned: vec![],
            unlocalized: Some(1),
            drift_signals: 0,
        },
        paired: Observed {
            served: true,
            first_calls: 2,
            second_calls: 0,
            field_repairs: 1,
            learned: vec![
                crate::field_capability::field_capability_key(TEST_UNSUPPORTED_PATH)
                    .expect("the test row has a field key"),
            ],
            unlocalized: None,
            drift_signals: 0,
        },
    }];
    rows.push(RejectionRow {
        name: "a 400 the classifier lifts to an unsupported feature",
        first_block: OPENAI_COMPAT,
        answer: refuse_web_search,
        carry: carry_web_search,
        defective: Observed {
            served: false,
            first_calls: 1,
            second_calls: 0,
            field_repairs: 0,
            learned: vec![],
            unlocalized: Some(1),
            drift_signals: 0,
        },
        paired: Observed {
            served: true,
            first_calls: 1,
            second_calls: 1,
            field_repairs: 0,
            learned: vec!["web_search".into()],
            unlocalized: None,
            drift_signals: 0,
        },
    });
    rows.push(RejectionRow {
        name: "a 400 the operator remaps to a server error",
        first_block: ANTHROPIC_400_AS_SERVER_ERROR,
        answer: refuse_plainly,
        carry: carry_nothing,
        defective: Observed {
            served: false,
            first_calls: 1,
            second_calls: 0,
            field_repairs: 0,
            learned: vec![],
            unlocalized: Some(1),
            drift_signals: 0,
        },
        paired: Observed {
            served: true,
            first_calls: 1,
            second_calls: 1,
            field_repairs: 0,
            learned: vec![],
            unlocalized: None,
            drift_signals: 0,
        },
    });
    rows.push(RejectionRow {
        name: "a stale defect left on the request's internal state",
        first_block: ANTHROPIC,
        answer: refuse_test_row,
        carry: carry_test_row_and_stale_defect,
        defective: Observed {
            served: false,
            first_calls: 1,
            second_calls: 0,
            field_repairs: 0,
            learned: vec![],
            unlocalized: Some(1),
            drift_signals: 0,
        },
        paired: Observed {
            served: true,
            first_calls: 2,
            second_calls: 0,
            field_repairs: 1,
            learned: vec![
                crate::field_capability::field_capability_key(TEST_UNSUPPORTED_PATH)
                    .expect("the test row has a field key"),
            ],
            unlocalized: None,
            drift_signals: 0,
        },
    });
    if cfg!(feature = "bedrock") {
        rows.push(RejectionRow {
            name: "a capability the generic learner mints",
            first_block: INVOKE,
            answer: bedrock_forced_tool_choice_rejection,
            carry: carry_forced_tool_choice,
            defective: Observed {
                served: false,
                first_calls: 1,
                second_calls: 0,
                field_repairs: 0,
                learned: vec![],
                unlocalized: Some(1),
                drift_signals: 0,
            },
            paired: Observed {
                served: true,
                first_calls: 1,
                second_calls: 1,
                field_repairs: 0,
                learned: vec!["forced_tool_choice".into()],
                unlocalized: None,
                drift_signals: 0,
            },
        });
    }
    rows
}

#[tokio::test]
async fn an_upstream_bad_request_on_a_defective_transcript_ends_the_walk_unlearned() {
    for surface in SURFACES {
        for row in rejection_rows() {
            run_rejection_row(&row, surface).await;
        }
    }
}

/// A field rejection on the token count's first seat: the paired transcript
/// is repaired and counted on the same seat, the defective one is not.
fn count_tokens_repair_row(
    name: &'static str,
    first_block: &'static str,
    carry: fn(&mut ChatRequest),
) -> RejectionRow {
    RejectionRow {
        name,
        first_block,
        answer: refuse_test_row,
        carry,
        defective: Observed {
            served: false,
            first_calls: 1,
            second_calls: 0,
            field_repairs: 0,
            learned: vec![],
            unlocalized: Some(1),
            drift_signals: 0,
        },
        paired: Observed {
            served: true,
            first_calls: 2,
            second_calls: 0,
            field_repairs: 1,
            learned: vec![
                crate::field_capability::field_capability_key(TEST_UNSUPPORTED_PATH)
                    .expect("the test row has a field key"),
            ],
            unlocalized: None,
            drift_signals: 0,
        },
    }
}

#[tokio::test]
async fn an_upstream_bad_request_on_a_defective_transcript_ends_the_token_count_unrepaired() {
    let rows = [
        count_tokens_repair_row(
            "a field rejection the count repairs",
            ANTHROPIC,
            carry_test_row,
        ),
        count_tokens_repair_row(
            "a field rejection the operator remaps to a server error",
            ANTHROPIC_400_AS_SERVER_ERROR,
            carry_test_row,
        ),
        count_tokens_repair_row(
            "a stale defect left on the request's internal state",
            ANTHROPIC,
            carry_test_row_and_stale_defect,
        ),
    ];
    for row in rows {
        run_rejection_row(&row, Surface::CountTokens).await;
    }
}

#[tokio::test]
async fn a_transient_failure_on_a_defective_transcript_routes_as_for_any_request() {
    let rows: [(&str, super::Answer); 2] = [
        ("429", |_| {
            Err(Error::upstream("scripted", 429, r#"{"error":"slow down"}"#))
        }),
        ("503", |_| {
            Err(Error::upstream(
                "scripted",
                503,
                r#"{"error":"unavailable"}"#,
            ))
        }),
    ];
    for surface in SURFACES {
        for (status, answer) in rows {
            let first = ScriptedProvider::new(None, answer);
            let second = ScriptedProvider::new(None, answers);
            let router = chain_router(
                ANTHROPIC,
                Arc::clone(&first) as Arc<dyn Provider>,
                ANTHROPIC,
                Arc::clone(&second) as Arc<dyn Provider>,
            );

            let out = walk(
                &router,
                surface,
                request_with(defective()),
                RouterOptions::new(),
            )
            .await;

            assert!(out.result.is_ok(), "{surface:?} {status}: {:?}", out.result);
            assert_eq!(first.calls(), 1, "{surface:?} {status}: first calls");
            assert_eq!(second.calls(), 1, "{surface:?} {status}: second calls");
        }
    }
}

#[tokio::test]
async fn a_raised_bad_request_retry_cap_does_not_resend_a_defective_transcript() {
    // `retry = 2` is the smallest cap that resends: the first failure has
    // already spent one attempt against it.
    const RAISED_RETRY: &str = "[retry.classes.bad-request]\nretry = 2\n";
    for (transcript, messages, first_calls, second_calls) in
        [("defective", defective(), 1, 0), ("paired", paired(), 2, 1)]
    {
        let first = ScriptedProvider::new(None, refuse_plainly);
        let second = ScriptedProvider::new(None, answers);
        let router = chain_router_with_retry(
            RAISED_RETRY,
            ANTHROPIC,
            Arc::clone(&first) as Arc<dyn Provider>,
            ANTHROPIC,
            Arc::clone(&second) as Arc<dyn Provider>,
        );

        let out = walk(
            &router,
            Surface::Complete,
            request_with(messages),
            RouterOptions::new(),
        )
        .await;

        assert_eq!(
            (first.calls(), second.calls()),
            (first_calls, second_calls),
            "{transcript}: (first, second) calls, result {:?}",
            out.result
        );
    }
}
