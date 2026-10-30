//! The usage row is written after the response-leg compare, so the row's
//! `wire_losses` holds what that compare found and equals the global loss
//! counter's delta. Complete bodies write their row (and the live K and
//! calibration samples) only once sent; a body dropped unsent writes one
//! `client_disconnect` row and no sample. Every stream exit compares before
//! its row. Each test reads the persisted row back from the database, and
//! the order probe pins the compare ahead of the row even where the compare
//! cannot find a loss.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, header};
use routectl_core::ChatChunk;
use routectl_core::wire_check::StreamExit;
use routectl_router::Router;
use routectl_router::wire_check_state::{CheckOutcome, WireCheckSnapshot};
use routectl_usage::{CHANNEL_CAPACITY, Outcome, UsageHandle, UsageWriter};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::order_probe::{self, Step};
use super::tests::{
    CHECK_EVERY, DIALECTS, Dialect, DroppingAdapter, HangingUpstream, Render,
    anthropic_unfinished_stream, outcomes, router, upstream,
};
use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::ingress::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, ResponseEdits, SseEvent,
    StreamErrorClass, StreamRequestContext,
};
use crate::server::AppState;

const SESSION: &str = "sess-row-order";
const ROW_WAIT: Duration = Duration::from_secs(10);

type RouterSwap = Arc<arc_swap::ArcSwap<Router>>;

/// A usage ledger on a temp database, the handle the ingress writes through,
/// and the writer that persists it.
struct Ledger {
    handle: Option<UsageHandle>,
    writer: Option<UsageWriter>,
    db_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

/// One persisted row's outcome and wire losses.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    outcome: String,
    wire_losses: Option<i64>,
}

impl Ledger {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("usage tempdir");
        let db_path = dir.path().join("usage.db");
        let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
        Self {
            handle: Some(handle),
            writer: Some(writer),
            db_path,
            _dir: dir,
        }
    }

    fn state(&self, swap: &RouterSwap) -> Arc<AppState> {
        AppState::for_test_with_usage(
            Arc::clone(swap),
            self.handle.clone().expect("ledger handle present"),
        )
    }

    /// [`Self::rows`] outside any runtime.
    fn rows_blocking(mut self) -> Vec<Row> {
        drop(self.handle.take());
        self.writer.take().expect("writer present").shutdown();
        read_rows(&self.db_path)
    }

    /// Drain the writer once every other handle is gone, and read every row.
    async fn rows(mut self) -> Vec<Row> {
        drop(self.handle.take());
        let writer = self.writer.take().expect("writer present");
        tokio::task::spawn_blocking(move || writer.shutdown())
            .await
            .expect("usage writer shutdown");
        read_rows(&self.db_path)
    }
}

fn read_rows(db_path: &std::path::Path) -> Vec<Row> {
    let conn = rusqlite::Connection::open(db_path).expect("read open");
    let mut query = conn
        .prepare("SELECT outcome, wire_losses FROM requests ORDER BY rowid")
        .expect("prepare");
    query
        .query_map([], |r| {
            Ok(Row {
                outcome: r.get(0)?,
                wire_losses: r.get(1)?,
            })
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

/// The global loss counter's total over every lane and class.
fn global_losses(snapshot: &WireCheckSnapshot) -> i64 {
    let total: u64 = snapshot.loss.iter().map(|c| c.reading.count).sum();
    i64::try_from(total).expect("loss count fits")
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert("x-session-id", HeaderValue::from_static(SESSION));
    headers
}

fn request(dialect: Dialect, stream: bool) -> Bytes {
    Bytes::from(serde_json::to_vec(&dialect.request_body(stream)).expect("json"))
}

/// Send one request on `dialect` through the ingress under `state`, and
/// return the response before its body is read.
async fn respond(
    dialect: Dialect,
    state: Arc<AppState>,
    stream: bool,
    render: Render,
) -> axum::response::Response {
    let body = Ok(request(dialect, stream));
    match dialect {
        Dialect::Anthropic => {
            let adapter = DroppingAdapter::new(AnthropicIngress, dialect, render);
            ingress_handle(state, headers(), None, body, adapter).await
        }
        Dialect::Chat => {
            let adapter = DroppingAdapter::new(OpenAiIngress, dialect, render);
            ingress_handle(state, headers(), None, body, adapter).await
        }
        Dialect::Responses => {
            let adapter = DroppingAdapter::new(ResponsesIngress, dialect, render);
            ingress_handle(state, headers(), None, body, adapter).await
        }
    }
}

async fn swap_over(dialect: Dialect, base: &str) -> RouterSwap {
    Arc::new(arc_swap::ArcSwap::from_pointee(
        router(dialect, base, CHECK_EVERY).await,
    ))
}

/// Run one fully read request whose renderer drops one upstream member, and
/// return its rows with the global loss total afterwards.
async fn rows_after_a_dropped_member(dialect: Dialect, stream: bool) -> (Vec<Row>, i64) {
    let server = upstream(dialect, stream, None).await;
    let swap = swap_over(dialect, &server.uri()).await;
    let ledger = Ledger::start();

    let resp = respond(dialect, ledger.state(&swap), stream, Render::Dropping).await;
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("the client takes the body");

    let rows = ledger.rows().await;
    (rows, global_losses(&swap.load().wire_check_snapshot()))
}

fn ok_with_losses(n: i64) -> Vec<Row> {
    vec![Row {
        outcome: "ok".to_string(),
        wire_losses: Some(n),
    }]
}

#[tokio::test]
async fn a_complete_response_persists_the_losses_its_response_compare_found() {
    for dialect in DIALECTS {
        let (rows, global) = rows_after_a_dropped_member(dialect, false).await;

        assert_eq!(global, 1, "{dialect:?}: the response leg found one loss");
        assert_eq!(rows, ok_with_losses(global), "{dialect:?}");
    }
}

#[tokio::test]
async fn a_clean_stream_persists_the_losses_its_response_compare_found() {
    for dialect in DIALECTS {
        let (rows, global) = rows_after_a_dropped_member(dialect, true).await;

        assert_eq!(global, 1, "{dialect:?}: the response leg found one loss");
        assert_eq!(rows, ok_with_losses(global), "{dialect:?}");
    }
}

/// The K windows' total sample count and the calibration lanes holding
/// evidence.
fn live_samples(router: &Router) -> (usize, usize) {
    let k: usize = router
        .k_session_store
        .export_entries()
        .iter()
        .map(|(_, window)| window.len())
        .sum();
    (k, router.calibration_lanes().len())
}

#[tokio::test]
async fn a_complete_body_fully_read_writes_one_ok_row_and_one_k_and_calibration_sample() {
    let server = upstream(Dialect::Chat, false, None).await;
    let swap = swap_over(Dialect::Chat, &server.uri()).await;
    let ledger = Ledger::start();
    let resp = respond(Dialect::Chat, ledger.state(&swap), false, Render::AsShipped).await;
    let before = live_samples(&swap.load());

    order_probe::start();
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("the client takes the body");
    let steps = order_probe::take();

    assert_eq!(
        before,
        (0, 0),
        "nothing is recorded before the body is sent"
    );
    assert_eq!(live_samples(&swap.load()), (1, 1));
    assert_eq!(steps, handed_off_then_ok());
    assert_eq!(ledger.rows().await, ok_with_losses(0));
}

fn handed_off_then_ok() -> Vec<Step> {
    vec![Step::Compared(StreamExit::Ended), Step::Row(Outcome::Ok)]
}

#[tokio::test]
async fn a_complete_body_dropped_unread_writes_one_disconnect_row_and_no_sample() {
    let server = upstream(Dialect::Chat, false, None).await;
    let swap = swap_over(Dialect::Chat, &server.uri()).await;
    let ledger = Ledger::start();
    let resp = respond(Dialect::Chat, ledger.state(&swap), false, Render::AsShipped).await;

    order_probe::start();
    drop(resp);
    let steps = order_probe::take();

    assert_eq!(live_samples(&swap.load()), (0, 0));
    assert_eq!(
        steps,
        [
            Step::Compared(StreamExit::Cancelled),
            Step::Row(Outcome::ClientDisconnect)
        ]
    );
    let rows = ledger.rows().await;
    assert_eq!(
        rows.iter().map(|r| r.outcome.as_str()).collect::<Vec<_>>(),
        ["client_disconnect"]
    );
}

/// The complete path through a real hyper server: the handler returns, and
/// hyper polls the body afterwards. The row, written once the body is sent,
/// carries the loss the compare found.
#[tokio::test]
async fn a_complete_body_served_by_hyper_writes_its_row_after_the_compare() {
    use axum::routing::post;

    let dialect = Dialect::Chat;
    let server = upstream(dialect, false, None).await;
    let swap = swap_over(dialect, &server.uri()).await;
    let ledger = Ledger::start();
    let state = ledger.state(&swap);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, body: Bytes| async move {
            let adapter = DroppingAdapter::new(OpenAiIngress, dialect, Render::Dropping);
            ingress_handle(state, headers, None, Ok(body), adapter).await
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let serving = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    order_probe::start();
    let client = reqwest::Client::new();
    let body = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .headers(headers())
        .body(request(dialect, false))
        .send()
        .await
        .expect("the server answers")
        .bytes()
        .await
        .expect("the client reads the body");
    serving.abort();
    let _ = serving.await;
    drop(client);
    let steps = order_probe::take();
    let rows = ledger.rows().await;

    assert!(!body.is_empty());
    assert_eq!(steps, handed_off_then_ok());
    assert_eq!(global_losses(&swap.load().wire_check_snapshot()), 1);
    assert_eq!(rows, ok_with_losses(1));
}

/// An Anthropic adapter whose chunk render fails, so the stream takes its
/// render-failure exit.
struct RenderFails;

impl IngressAdapter for RenderFails {
    fn id(&self) -> &str {
        AnthropicIngress.id()
    }
    fn dialect(&self) -> routectl_core::wire_check::IngressDialect {
        AnthropicIngress.dialect()
    }
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        AnthropicIngress.error_envelope_shape()
    }
    fn parse_request(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> routectl_core::Result<routectl_core::ChatRequest> {
        AnthropicIngress.parse_request(headers, body)
    }
    fn render_response(
        &self,
        resp: routectl_core::ChatResponse,
    ) -> routectl_core::Result<bytes::Bytes> {
        AnthropicIngress.render_response(resp)
    }
    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        AnthropicIngress.new_stream_state(ctx)
    }
    fn render_chunk(
        &self,
        _chunk: ChatChunk,
        _state: &mut dyn IngressStreamState,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        Err(routectl_core::Error::Internal(
            "synthetic chunk render failure".into(),
        ))
    }
    fn render_chunk_declared(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
        _edits: &mut ResponseEdits,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        self.render_chunk(chunk, state)
    }
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        AnthropicIngress.render_eos(state)
    }
    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        AnthropicIngress.render_error_eos(state, error, class)
    }
}

/// How one stream exit is reached.
#[derive(Clone, Copy, Debug)]
enum Exit {
    Ended,
    UpstreamError,
    RenderFailure,
    Disconnect,
}

impl Exit {
    const fn stream_exit(self) -> StreamExit {
        match self {
            Self::Ended => StreamExit::Ended,
            Self::UpstreamError => StreamExit::UpstreamError,
            Self::RenderFailure => StreamExit::RenderFailure,
            Self::Disconnect => StreamExit::Disconnect,
        }
    }
}

const fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ok => "ok",
        Outcome::UpstreamError => "upstream_error",
        Outcome::ClientDisconnect => "client_disconnect",
        _ => "other",
    }
}

/// After one stream run on `exit`: the rows, the global loss total, and
/// the outcome of the response-leg check.
struct ExitRun {
    rows: Vec<Row>,
    global: i64,
    response_leg: Vec<(&'static str, CheckOutcome, u64)>,
    steps: Vec<Step>,
}

/// The upstream `exit` needs, held for the run, and its base URL.
async fn upstream_for(exit: Exit) -> (Option<MockServer>, String) {
    let server = match exit {
        Exit::Ended | Exit::RenderFailure => upstream(Dialect::Anthropic, true, None).await,
        Exit::UpstreamError => {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(anthropic_unfinished_stream()),
                )
                .mount(&server)
                .await;
            server
        }
        Exit::Disconnect => return (None, HangingUpstream::start().await.base),
    };
    let base = server.uri();
    (Some(server), base)
}

async fn run_exit(exit: Exit) -> ExitRun {
    let dialect = Dialect::Anthropic;
    let ledger = Ledger::start();
    let (_server, base) = upstream_for(exit).await;
    let swap = swap_over(dialect, &base).await;
    order_probe::start();
    match exit {
        Exit::Ended => {
            let resp = respond(dialect, ledger.state(&swap), true, Render::Dropping).await;
            to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        }
        Exit::UpstreamError => {
            let resp = respond(dialect, ledger.state(&swap), true, Render::AsShipped).await;
            to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        }
        Exit::RenderFailure => {
            let state = ledger.state(&swap);
            let resp = ingress_handle(
                state,
                headers(),
                None,
                Ok(request(dialect, true)),
                RenderFails,
            )
            .await;
            to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        }
        Exit::Disconnect => leave_after_the_first_frame(&swap, ledger.state(&swap)).await,
    }
    let rows = ledger.rows().await;
    let steps = order_probe::take();
    let snapshot = swap.load().wire_check_snapshot();
    let response_leg = outcomes(&snapshot).into_iter().skip(1).collect();
    ExitRun {
        rows,
        global: global_losses(&snapshot),
        response_leg,
        steps,
    }
}

/// Read the first frame of a stream that never ends, then leave, and wait
/// for the response leg to record.
async fn leave_after_the_first_frame(swap: &RouterSwap, state: Arc<AppState>) {
    use futures::StreamExt;

    let resp = respond(Dialect::Anthropic, state, true, Render::AsShipped).await;
    let mut frames = resp.into_body().into_data_stream();
    let first = frames.next().await.expect("a first frame").expect("bytes");
    assert!(!first.is_empty());
    drop(frames);
    tokio::time::timeout(ROW_WAIT, async {
        while outcomes(&swap.load().wire_check_snapshot()).len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the response leg records once the client leaves");
}

/// Every stream exit writes exactly one row whose `wire_losses` equals the
/// global loss delta, the response leg included, and compares before that
/// row is written. Only `Ended` can carry a response-leg loss: every other
/// exit leaves the client's document incomplete, and an incomplete compare
/// records no loss, so on those rows only the order probe shows the
/// sequence.
#[tokio::test]
async fn every_stream_exit_persists_the_response_leg_compare_in_its_one_row() {
    let cases = [
        (Exit::Ended, Outcome::Ok, CheckOutcome::Loss, 1),
        (
            Exit::UpstreamError,
            Outcome::UpstreamError,
            CheckOutcome::Incomplete,
            0,
        ),
        (
            Exit::RenderFailure,
            Outcome::UpstreamError,
            CheckOutcome::Incomplete,
            0,
        ),
        (
            Exit::Disconnect,
            Outcome::ClientDisconnect,
            CheckOutcome::Incomplete,
            0,
        ),
    ];
    for (exit, outcome, leg, losses) in cases {
        let run = run_exit(exit).await;

        assert_eq!(
            run.steps,
            [Step::Compared(exit.stream_exit()), Step::Row(outcome)],
            "{exit:?}: the compare runs before the row"
        );

        assert_eq!(
            run.response_leg
                .iter()
                .map(|(_, o, _)| *o)
                .collect::<Vec<_>>(),
            [leg],
            "{exit:?}: the response leg was compared"
        );
        assert_eq!(run.global, losses, "{exit:?}");
        assert_eq!(
            run.rows,
            [Row {
                outcome: outcome_label(outcome).to_string(),
                wire_losses: Some(run.global),
            }],
            "{exit:?}"
        );
    }
}

/// A stream task the runtime drops mid-stream (a shutdown or an abort, with
/// the client still connected) compares as cancelled before its capture
/// writes the `cancelled` row: nothing observed a client hangup.
#[test]
fn an_aborted_stream_task_compares_before_its_row() {
    let ledger = Ledger::start();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (body, _swap) = runtime.block_on(async {
        use futures::StreamExt;

        let upstream = HangingUpstream::start().await;
        let swap = swap_over(Dialect::Anthropic, &upstream.base).await;
        let resp = respond(
            Dialect::Anthropic,
            ledger.state(&swap),
            true,
            Render::AsShipped,
        )
        .await;
        let mut frames = resp.into_body().into_data_stream();
        let first = frames.next().await.expect("a first frame").expect("bytes");
        assert!(!first.is_empty());
        (frames, swap)
    });

    order_probe::start();
    drop(runtime);
    let steps = order_probe::take();
    drop(body);
    let rows = ledger.rows_blocking();

    assert_eq!(
        steps,
        [
            Step::Compared(StreamExit::Cancelled),
            Step::Row(Outcome::Cancelled)
        ]
    );
    assert_eq!(
        rows,
        [Row {
            outcome: "cancelled".to_string(),
            wire_losses: Some(0),
        }]
    );
}

/// An OpenAI adapter whose complete render is empty.
struct EmptyBody;

impl IngressAdapter for EmptyBody {
    fn id(&self) -> &str {
        OpenAiIngress.id()
    }
    fn dialect(&self) -> routectl_core::wire_check::IngressDialect {
        OpenAiIngress.dialect()
    }
    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        OpenAiIngress.error_envelope_shape()
    }
    fn parse_request(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> routectl_core::Result<routectl_core::ChatRequest> {
        OpenAiIngress.parse_request(headers, body)
    }
    fn render_response(
        &self,
        _resp: routectl_core::ChatResponse,
    ) -> routectl_core::Result<bytes::Bytes> {
        Ok(Bytes::new())
    }
    fn new_stream_state(&self, ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
        OpenAiIngress.new_stream_state(ctx)
    }
    fn render_chunk(
        &self,
        chunk: ChatChunk,
        state: &mut dyn IngressStreamState,
    ) -> routectl_core::Result<Vec<SseEvent>> {
        OpenAiIngress.render_chunk(chunk, state)
    }
    fn render_eos(&self, state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        OpenAiIngress.render_eos(state)
    }
    fn render_error_eos(
        &self,
        state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
        class: &StreamErrorClass,
    ) -> Vec<SseEvent> {
        OpenAiIngress.render_error_eos(state, error, class)
    }
}

/// An empty complete body has nothing to send, so hyper never polls it: it
/// is handed off as built, and its drop writes one `ok` row.
#[tokio::test]
async fn an_empty_complete_body_writes_one_ok_row_without_being_polled() {
    let server = upstream(Dialect::Chat, false, None).await;
    let swap = swap_over(Dialect::Chat, &server.uri()).await;
    let ledger = Ledger::start();
    let resp = ingress_handle(
        ledger.state(&swap),
        headers(),
        None,
        Ok(request(Dialect::Chat, false)),
        EmptyBody,
    )
    .await;

    order_probe::start();
    drop(resp);
    let steps = order_probe::take();

    assert_eq!(steps, handed_off_then_ok());
    assert_eq!(ledger.rows().await, ok_with_losses(0));
}
