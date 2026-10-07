//! Replays captured Bedrock `ValidationException` rejections through real
//! dispatch on a three-lane chain (two Bedrock lanes, one openai-compat
//! fallback) with no operator declaration: a rejection naming a capability
//! the request carries is learned on the first miss per lane and the next
//! request skips that lane; a rejection that names nothing the request
//! carries is never learned and the lane keeps being dialed. A declared
//! `unsupported_features` entry still skips the Bedrock lanes before any
//! dispatch. The learned negatives survive a restart through their persisted
//! rows, are learned on the stream arm before the first chunk, and are never
//! learned from an error that follows streamed content.

use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::time::Instant;

use futures::stream::{BoxStream, StreamExt};
use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::{ChatChunk, ChatResponse, ChunkChoice, ChunkDelta, Provider, Result, ToolDef};
use serde_json::{Value, json};

use crate::capability_rebuild::{CapabilityEventRow, CapabilityLedgerReader, ReplayTombstone};
use crate::capability_vocab::CURRENT_VOCAB_VERSION;
use crate::catalog_baked::CATALOG_VERSION;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

const FIXTURE: &str = include_str!("../../tests/fixtures/bedrock_validation_capture.json");

const ALIAS: &str = "breadth-alias";
const ACCEPT: &str = "accept";
const WIRE_A: &str = "wire-a";
const WIRE_B: &str = "wire-b";
const WIRE_C: &str = "wire-c";

const BEDROCK_PROVIDER_BLOCK: &str = "[providers.p1]\n\
     kind = \"bedrock\"\n\
     region = \"us-east-1\"\n\
     creds = { kind = \"default-chain\" }\n";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("valid bedrock capture fixture")
}

/// One `replay_rows` entry of the capture fixture.
struct ReplayRow {
    request: String,
    lane_a: String,
    lane_b: String,
    expect_capability: Option<String>,
    expect_served: String,
}

impl ReplayRow {
    fn label(&self) -> String {
        format!("{} / {} / {}", self.request, self.lane_a, self.lane_b)
    }
}

fn field(row: &Value, name: &str) -> String {
    row[name]
        .as_str()
        .unwrap_or_else(|| panic!("replay row field {name}"))
        .to_string()
}

fn replay_rows(fx: &Value) -> Vec<ReplayRow> {
    fx["replay_rows"]
        .as_array()
        .expect("replay_rows array")
        .iter()
        .map(|row| ReplayRow {
            request: field(row, "request"),
            lane_a: field(row, "lane_a"),
            lane_b: field(row, "lane_b"),
            expect_capability: row["expect_capability"].as_str().map(str::to_string),
            expect_served: field(row, "expect_served"),
        })
        .collect()
}

/// The 400 body a lane answers with for a fixture name: a captured canary's
/// body verbatim, or a near-miss message inside the minimal flat envelope.
fn rejection_body(fx: &Value, name: &str) -> String {
    if let Some(body) = fx["canaries"][name]["body"].as_str() {
        return body.to_string();
    }
    let message = fx["near_miss_messages"][name]
        .as_str()
        .unwrap_or_else(|| panic!("fixture has no canary or near-miss named {name}"));
    json!({ "message": message }).to_string()
}

/// A lane that counts its calls and either rejects every request with a
/// fixed Bedrock validation 400 or succeeds. On the stream arm the rejection
/// arrives before any chunk, or after one content chunk when
/// `rejects_after_content` is set.
struct CountingLane {
    id: &'static str,
    rejection: Option<(String, String)>,
    rejects_after_content: bool,
    calls: AtomicUsize,
}

impl CountingLane {
    fn for_fixture_name(id: &'static str, fx: &Value, name: &str) -> Arc<Self> {
        let rejection = (name != ACCEPT).then(|| {
            let lifted = fx["lifted_type"].as_str().expect("fixture lifted_type");
            (rejection_body(fx, name), lifted.to_string())
        });
        Arc::new(Self {
            id,
            rejection,
            rejects_after_content: false,
            calls: AtomicUsize::new(0),
        })
    }

    /// Streams one content chunk and then the fixture rejection.
    fn rejecting_after_content(id: &'static str, fx: &Value, name: &str) -> Arc<Self> {
        let lifted = fx["lifted_type"].as_str().expect("fixture lifted_type");
        Arc::new(Self {
            id,
            rejection: Some((rejection_body(fx, name), lifted.to_string())),
            rejects_after_content: true,
            calls: AtomicUsize::new(0),
        })
    }

    fn accepting(id: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id,
            rejection: None,
            rejects_after_content: false,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    const fn rejects(&self) -> bool {
        self.rejection.is_some()
    }

    fn rejection_error(&self) -> Option<Error> {
        self.rejection.as_ref().map(|(body, upstream_type)| {
            Error::upstream_full(
                self.id,
                400,
                body.clone(),
                None,
                Some(upstream_type.clone()),
                None,
            )
        })
    }
}

fn text_chunk(text: &str) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Provider for CountingLane {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.rejection_error() {
            Some(err) => Err(err),
            None => Ok(ChatResponse {
                model: req.model,
                ..Default::default()
            }),
        }
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let items = match self.rejection_error() {
            Some(err) if self.rejects_after_content => vec![Ok(text_chunk("partial")), Err(err)],
            Some(err) => return Err(err),
            None => vec![Ok(text_chunk("served"))],
        };
        Ok(futures::stream::iter(items).boxed())
    }
}

struct Lanes {
    a: Arc<CountingLane>,
    b: Arc<CountingLane>,
    fallback: Arc<CountingLane>,
}

impl Lanes {
    fn for_row(fx: &Value, row: &ReplayRow) -> Self {
        Self {
            a: CountingLane::for_fixture_name("p1", fx, &row.lane_a),
            b: CountingLane::for_fixture_name("p1", fx, &row.lane_b),
            fallback: CountingLane::accepting("p2"),
        }
    }
}

/// `m1` -> `wire-a` and `m2` -> `wire-b` on Bedrock `p1`, `m3` on an
/// openai-compat `p2`, chained `[m1, m2, m3]` under [`ALIAS`]. The synthetic
/// wire ids keep every baked catalog prior out of play.
fn three_lane_router(p1_extra: &str, lanes: &Lanes) -> Router {
    let toml_text = format!(
        "{BEDROCK_PROVIDER_BLOCK}{p1_extra}\
         [providers.p2]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n\
         [aliases]\n\
         \"{ALIAS}\" = [\"m1\", \"m2\", \"m3\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let models: BTreeMap<String, Arc<ResolvedModel>> = [
        ResolvedModel::new("m1", "p1", lanes.a.clone(), WIRE_A),
        ResolvedModel::new("m2", "p1", lanes.b.clone(), WIRE_B),
        ResolvedModel::new("m3", "p2", lanes.fallback.clone(), WIRE_C),
    ]
    .into_iter()
    .map(|model| (model.nickname.clone(), Arc::new(model)))
    .collect();
    router.install_resolved_models(models);
    router
}

fn function_tool() -> ToolDef {
    ToolDef::Other(json!({
        "type": "function",
        "function": {"name": "lookup", "parameters": {"type": "object"}}
    }))
}

fn replay_request(shape: &str) -> ChatRequest {
    let base = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        ..Default::default()
    };
    match shape {
        "web_search" => ChatRequest {
            tools: Some(vec![ToolDef::Other(json!({
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 1
            }))]),
            ..base
        },
        "computer" => ChatRequest {
            tools: Some(vec![ToolDef::Other(json!({
                "type": "computer_20250124",
                "name": "computer",
                "display_width_px": 1024,
                "display_height_px": 768
            }))]),
            ..base
        },
        "structured_output" => ChatRequest {
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "answer",
                    "schema": {"type": "object", "additionalProperties": false}
                }
            })),
            ..base
        },
        "plain" => ChatRequest {
            tools: Some(vec![function_tool()]),
            ..base
        },
        other => panic!("unknown replay request shape {other}"),
    }
}

/// Asserts the outcome was served by the expected seat and its wire id.
fn assert_served(outcome: &Dispatched, seat: &str, context: &str) {
    let wire = match seat {
        "m2" => WIRE_B,
        "m3" => WIRE_C,
        other => panic!("unexpected served seat {other}"),
    };
    assert!(outcome.result.is_ok(), "{context}: served");
    assert_eq!(
        outcome.meta.served_model.as_deref(),
        Some(seat),
        "{context}: seat"
    );
    assert_eq!(
        outcome.meta.served_upstream.as_deref(),
        Some(wire),
        "{context}: wire id"
    );
}

async fn dispatch(router: &Router, shape: &str) -> Dispatched {
    router
        .complete_with_options(replay_request(shape), RouterOptions::default())
        .await
}

#[tokio::test]
async fn learn_rows_learn_once_per_lane_and_next_request_skips_it() {
    let fx = fixture();
    let rows: Vec<ReplayRow> = replay_rows(&fx)
        .into_iter()
        .filter(|row| row.expect_capability.is_some())
        .collect();
    assert_eq!(rows.len(), 5, "learn rows in the fixture");
    for row in rows {
        // Arrange
        let label = row.label();
        let expected = row.expect_capability.clone().expect("learn row");
        let lanes = Lanes::for_row(&fx, &row);
        let router = three_lane_router("", &lanes);
        let rejecting: Vec<(&Arc<CountingLane>, &str)> = [(&lanes.a, WIRE_A), (&lanes.b, WIRE_B)]
            .into_iter()
            .filter(|(lane, _)| lane.rejects())
            .collect();

        // Act: the first request meets every rejecting lane once.
        let first = dispatch(&router, &row.request).await;

        // Assert
        assert_served(
            &first,
            &row.expect_served,
            &format!("{label}: first request"),
        );
        for (lane, wire) in &rejecting {
            assert_eq!(lane.calls(), 1, "{label}: {wire} dialed once");
        }
        let learned = &first.meta.learned_capabilities;
        assert_eq!(
            learned.len(),
            rejecting.len(),
            "{label}: one learn per rejecting lane"
        );
        for (event, (_, wire)) in learned.iter().zip(&rejecting) {
            assert_eq!(event.capability_key, expected, "{label}");
            assert_eq!(event.state_key, format!("p1#{wire}"), "{label}");
            assert_eq!(event.signal_tier, SignalTier::SelfIdentifying, "{label}");
            assert_eq!(event.phase, FailurePhase::F1, "{label}");
        }

        // Act: the next request of the same shape.
        let second = dispatch(&router, &row.request).await;

        // Assert: a route-away only tail-demotes, so the unchanged call
        // count is what proves each learned lane was skipped.
        assert_served(
            &second,
            &row.expect_served,
            &format!("{label}: second request"),
        );
        for (lane, wire) in &rejecting {
            assert_eq!(lane.calls(), 1, "{label}: {wire} skipped after learning");
        }
        assert_eq!(router.metrics.strip_total(), 0, "{label}");
    }
}

#[tokio::test]
async fn must_not_learn_rows_learn_nothing_and_keep_dialing_the_lane() {
    let fx = fixture();
    let rows: Vec<ReplayRow> = replay_rows(&fx)
        .into_iter()
        .filter(|row| row.expect_capability.is_none())
        .collect();
    assert_eq!(rows.len(), 4, "must-not-learn rows in the fixture");
    for row in rows {
        // Arrange
        let label = row.label();
        let lanes = Lanes::for_row(&fx, &row);
        let router = three_lane_router("", &lanes);

        // Act
        let first = dispatch(&router, &row.request).await;
        let second = dispatch(&router, &row.request).await;

        // Assert
        for (n, outcome) in [(1, &first), (2, &second)] {
            assert_served(
                outcome,
                &row.expect_served,
                &format!("{label}: request {n}"),
            );
            assert!(
                outcome.meta.learned_capabilities.is_empty(),
                "{label}: request {n} learned nothing"
            );
        }
        assert_eq!(lanes.a.calls(), 2, "{label}: lane a still dialed");
    }
}

#[tokio::test]
async fn declared_unsupported_feature_skips_bedrock_lanes_before_dispatch() {
    // Arrange: the first learn row's lanes, plus the operator declaration.
    let fx = fixture();
    let row = replay_rows(&fx)
        .into_iter()
        .next()
        .expect("first replay row");
    let lanes = Lanes::for_row(&fx, &row);
    let router = three_lane_router("unsupported_features = [\"web_search\"]\n", &lanes);

    // Act
    let outcome = dispatch(&router, &row.request).await;

    // Assert
    assert_served(&outcome, &row.expect_served, "declared web_search");
    assert_eq!(lanes.a.calls(), 0);
    assert_eq!(lanes.b.calls(), 0);
    assert!(outcome.meta.learned_capabilities.is_empty());
}

/// The first fixture row: a web_search request both Bedrock lanes refuse.
fn web_search_row(fx: &Value) -> ReplayRow {
    let row = replay_rows(fx)
        .into_iter()
        .next()
        .expect("first replay row");
    assert_eq!(row.request, "web_search", "first replay row shape");
    assert_eq!(row.lane_a, "web-search-pydantic-sonnet-4-6");
    assert_eq!(row.lane_b, "web-search-template-sonnet-5");
    row
}

/// A restart's view of the ledger: one boundary and the rows after it.
struct Ledger(Vec<CapabilityEventRow>);

impl CapabilityLedgerReader for Ledger {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        Some(ReplayTombstone::new(0, CATALOG_VERSION, 0))
    }

    fn read_events(&self) -> Vec<CapabilityEventRow> {
        self.0.clone()
    }
}

/// The persisted rows of the learned events one request carried.
fn persisted_rows(meta: &DispatchMeta) -> Vec<CapabilityEventRow> {
    meta.learned_capabilities
        .iter()
        .zip(1_i64..)
        .map(|(event, rowid)| {
            CapabilityEventRow::new(
                rowid,
                Instant::now(),
                "broken".to_string(),
                Some(event.phase.as_str().to_string()),
                event.source.as_str().to_string(),
                Some(event.signal_tier.as_str().to_string()),
                None,
                event.capability_key.clone(),
                event.state_key.clone(),
                event.provider_kind.clone(),
                CATALOG_VERSION,
                0,
            )
            .with_vocab_version(Some(CURRENT_VOCAB_VERSION))
        })
        .collect()
}

async fn open_stream(router: &Router, shape: &str) -> DispatchedStream {
    router
        .stream_with_options(replay_request(shape), RouterOptions::default())
        .await
}

/// Drains a stream, returning its content texts and whether it ended on an
/// error frame.
async fn drain(stream: BoxStream<'static, Result<ChatChunk>>) -> (Vec<String>, bool) {
    let frames: Vec<Result<ChatChunk>> = stream.collect().await;
    let errored = frames.iter().any(std::result::Result::is_err);
    let texts = frames
        .into_iter()
        .filter_map(std::result::Result::ok)
        .flat_map(|chunk| chunk.choices)
        .filter_map(|choice| choice.delta.content)
        .collect();
    (texts, errored)
}

fn assert_stream_served_m3(outcome: &DispatchedStream, context: &str) {
    assert!(outcome.result.is_ok(), "{context}: stream opened");
    assert_eq!(
        outcome.meta.served_model.as_deref(),
        Some("m3"),
        "{context}: seat"
    );
    assert_eq!(
        outcome.meta.served_upstream.as_deref(),
        Some(WIRE_C),
        "{context}: wire id"
    );
}

#[tokio::test]
async fn learned_negatives_survive_a_restart_through_their_persisted_rows() {
    // Arrange: request 1 learns both lanes on the first router.
    let fx = fixture();
    let row = web_search_row(&fx);
    let first_lanes = Lanes::for_row(&fx, &row);
    let first_router = three_lane_router("", &first_lanes);
    let first = dispatch(&first_router, &row.request).await;
    assert_served(&first, "m3", "request 1");
    let rows = persisted_rows(&first.meta);
    assert_eq!(rows.len(), 2, "one persisted negative per Bedrock lane");

    // Act: a fresh router with fresh lanes, rebuilt only from those rows.
    let lanes = Lanes::for_row(&fx, &row);
    let router = three_lane_router("", &lanes);
    let summary = router.rebuild_learned_from_ledger(&Ledger(rows));
    let outcome = dispatch(&router, &row.request).await;

    // Assert
    assert_eq!(summary.replayed_negative, 2, "replayed negatives");
    assert_served(&outcome, "m3", "after restart");
    assert_eq!(lanes.a.calls(), 0, "wire-a skipped after restart");
    assert_eq!(lanes.b.calls(), 0, "wire-b skipped after restart");
    assert_eq!(lanes.fallback.calls(), 1);
}

#[tokio::test]
async fn stream_rejection_before_the_first_chunk_is_learned_per_lane() {
    // Arrange
    let fx = fixture();
    let row = web_search_row(&fx);
    let lanes = Lanes::for_row(&fx, &row);
    let router = three_lane_router("", &lanes);

    // Act
    let first = open_stream(&router, &row.request).await;

    // Assert
    assert_stream_served_m3(&first, "first stream");
    let learned: Vec<(&str, &str)> = first
        .meta
        .learned_capabilities
        .iter()
        .map(|event| (event.capability_key.as_str(), event.state_key.as_str()))
        .collect();
    assert_eq!(
        learned,
        vec![("web_search", "p1#wire-a"), ("web_search", "p1#wire-b")],
        "one learned web_search negative per Bedrock lane"
    );
    assert_eq!(lanes.a.calls(), 1);
    assert_eq!(lanes.b.calls(), 1);

    // Act: the next streamed request of the same shape.
    let second = open_stream(&router, &row.request).await;

    // Assert
    assert_stream_served_m3(&second, "second stream");
    assert_eq!(lanes.a.calls(), 1, "wire-a skipped after learning");
    assert_eq!(lanes.b.calls(), 1, "wire-b skipped after learning");
}

#[tokio::test]
async fn stream_rejection_after_content_is_not_learned() {
    // Arrange: lane a streams content and then the same canary rejection
    // that the pre-content test learns from.
    let fx = fixture();
    let row = web_search_row(&fx);
    let lanes = Lanes {
        a: CountingLane::rejecting_after_content("p1", &fx, &row.lane_a),
        ..Lanes::for_row(&fx, &row)
    };
    let router = three_lane_router("", &lanes);

    // Act
    let first = open_stream(&router, &row.request).await;
    let first_learned = first.meta.learned_capabilities.len();
    let (texts, errored) = drain(first.result.expect("content committed lane a")).await;

    // Assert: lane a committed, then failed in-stream.
    assert_eq!(first.meta.served_upstream.as_deref(), Some(WIRE_A));
    assert_eq!(texts, vec!["partial".to_string()]);
    assert!(errored, "the post-content rejection ends the stream");
    assert_eq!(first_learned, 0, "nothing learned on the first request");
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "nothing resident after the stream drained"
    );

    // Act
    let second = open_stream(&router, &row.request).await;

    // Assert
    assert_eq!(lanes.a.calls(), 2, "wire-a dialed again");
    assert_eq!(second.meta.served_upstream.as_deref(), Some(WIRE_A));
    assert!(second.meta.learned_capabilities.is_empty());
}
