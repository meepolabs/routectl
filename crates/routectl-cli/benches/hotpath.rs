//! Ingress hot-path micro-benches: request parse, local token estimate,
//! context-anchor measurement, and response render.
//!
//! Bench names follow the pinned `<stage>__<profile>__<dialect>`
//! convention (stages `ingress_parse`, `token_estimate`,
//! `anchor_identity`, `response_render`; dialects `anthropic`, `openai`, `na`). Fixtures are
//! generated ONCE outside every timed closure per the
//! [`bench_fixtures`](routectl_testkit::bench_fixtures) contract.
//!
//! `ingress_parse` measures the full ingress translation the server does
//! per request: raw wire bytes -> `serde_json::Value` -> canonical
//! `ChatRequest` via the dialect adapter. `token_estimate` measures the
//! display meter estimate (`routectl_router::estimate_meter_tokens`) over a
//! canonical request (dialect-agnostic, so `na`); its bench-local
//! `json_heavy_150k` case measures the persisted floor estimate over the
//! anchor fixture for scale. `anchor_identity` measures the Anthropic
//! context anchor's per-request work (the streaming prefix digest and its
//! normalized byte count, in one pass) on that pinned 154k-token request.
//! `response_render` measures rendering a canonical `ChatResponse` back to
//! dialect wire JSON. The adapter consumes the response by value, so each
//! case is a batched one: the per-iteration clone is the untimed setup and
//! only the render is measured.
//!
//! The live wire check is priced on the anthropic -> anthropic-api lane in
//! three cases per profile. `hot_path` is the per-request work with no wire
//! check at all: ingress parse, egress normalize and serialize, then the
//! upstream response parsed, normalized and rendered back to the client.
//! `wire_unadmitted` is that same path as an attempt the sampler declines
//! pays it: the ingress capture, the sampler decision and tap install, and
//! the provider's deposits and declarations into an unadmitted tap.
//! `wire_unadmitted_setup` times that added work alone, so it is not lost
//! in the hot path's run-to-run noise: capture, sampler decision and tap
//! install, then the deposits and the request edits the provider declares
//! on this profile.
//! `wire_check` is the extra work one admitted attempt pays on top, both
//! legs: capture, tap install, deposits and declarations, then each leg's
//! drain, parse, diff and record. It drives the public pieces in the order
//! the router's request check and response check run them; its inputs come
//! from the real provider and renderer, built outside the timed closure.
//! The fidelity diff never reads the canonical request, so both batched
//! cases start from an empty one and time no request drop.
//!
//! Every fixture except `json_heavy_150k` is a `SpectrumProfile`; that one
//! is bench-local and pinned in size by `assert_anchor_fixture`, and
//! `scripts/bench.sh` records it on its separate local-fixtures line.

use std::hint::black_box;
use std::time::Duration;

use axum::http::HeaderMap;
use bytes::Bytes;
use criterion::{BatchSize, Criterion};
use routectl_core::identity::anthropic::parse_claude_cli_version;
use routectl_core::wire_check::{
    AttemptRecord, DeclaredEdit, EditKind, EditSite, EgressKind, IngressDialect, IngressWire, Lane,
    LaneClass, Leg, WireTap, diff,
};
use routectl_core::{
    ChatRequest, ChatResponse, Choice, ContentPart, KnownContentPart, Message, MessageContent,
    Provider, Role, Usage,
};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_router::wire_check_state::{CheckOutcome, WireCheckState};
use routectl_router::{Config, WireCheckConfig};
#[cfg(not(feature = "dhat"))]
use routectl_testkit::bench_alloc::CountingAllocator;
use routectl_testkit::bench_alloc::{self, BenchCase};
use routectl_testkit::bench_fixtures::{BenchFixture, SpectrumProfile};
use serde_json::{Value, json};

use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::anthropic::context_anchor::RequestIdentity;
use routectl_cli::ingress::openai::OpenAiIngress;

// The perf benches run under one of two mutually exclusive global
// allocators, selected at compile time: by default the CountingAllocator
// (exact allocs/bytes tallies -- the totals-of-record), and under the
// `dhat` feature dhat::Alloc (heap-profile attribution -- WHERE the
// allocations happen). SINGLE REMOVAL POINT: when a successor profiler
// replaces dhat, delete the `dhat`-gated arm here and the dhat wiring in
// Cargo.toml.
#[cfg(not(feature = "dhat"))]
#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[cfg(feature = "dhat")]
#[global_allocator]
static GLOBAL: dhat::Alloc = dhat::Alloc;

/// Profiles fed to the `ingress_parse` stage (both dialects).
const PARSE_PROFILES: [SpectrumProfile; 4] = [
    SpectrumProfile::ToolHeavy,
    SpectrumProfile::LargeImage,
    SpectrumProfile::PlainRoundTrip,
    SpectrumProfile::LongSession,
];

/// Profiles fed to the streaming-shaped `token_estimate` stage.
const ESTIMATE_PROFILES: [SpectrumProfile; 3] = [
    SpectrumProfile::ToolHeavy,
    SpectrumProfile::LargeImage,
    SpectrumProfile::LongSession,
];

/// Profiles fed to the `response_render` stage (both dialects).
const RENDER_PROFILES: [SpectrumProfile; 2] =
    [SpectrumProfile::PlainRoundTrip, SpectrumProfile::ToolHeavy];

/// Profiles fed to the wire-check stages (`hot_path`, `wire_unadmitted`,
/// `wire_check`).
const WIRE_CHECK_PROFILES: [SpectrumProfile; 4] = PARSE_PROFILES;

/// The user-agent the wire-check cases capture a client version from.
const WIRE_CHECK_USER_AGENT: &str = "claude-cli/2.1.0 (external, cli)";

/// The label the served response carries for its provider.
const WIRE_CHECK_PROVIDER: &str = "bench-anthropic";

const fn assistant_message(content: MessageContent, tool_calls: Option<Vec<Value>>) -> Message {
    Message {
        role: Role::Assistant,
        content,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls,
        refusal: None,
    }
}

/// Profile token of the large-prompt anchor fixture. Bench-local, like the
/// router bench's `max_body` fixture: it is not a [`SpectrumProfile`], whose
/// variants drive every other bench's fan-out. `scripts/bench.sh` lists it
/// under its local fixtures.
const ANCHOR_PROFILE: &str = "json_heavy_150k";

/// Tool-call rounds in the anchor fixture. Each round is one tool-use turn
/// and one pretty-printed JSON tool result.
const ANCHOR_ROUNDS: usize = 114;

/// Exact serialized size of the anchor fixture: 154,042 tokens at the
/// bytes/4 estimate. Pinned so a fixture drift cannot silently change what
/// the series prices.
const ANCHOR_FIXTURE_BYTES: usize = 616_171;

/// A deterministic long tool-using session of [`ANCHOR_ROUNDS`] rounds plus
/// a final user message. Returns the request and the message count before
/// that final message -- the prefix a prior anchor would claim.
fn anchor_fixture() -> (ChatRequest, usize) {
    let mut messages = vec![user_message("Audit every manifest in the repository.")];
    for round in 0..ANCHOR_ROUNDS {
        let id = format!("toolu_{round:04}");
        let rows: Vec<Value> = (0..40)
            .map(|j| json!({"crate": format!("c{round}-{j}"), "version": "1.0.0", "features": ["std", "derive"]}))
            .collect();
        let payload = serde_json::to_string_pretty(&rows).expect("bench fixture serializes");
        messages.push(tool_part_message(
            Role::Assistant,
            KnownContentPart::ToolUse {
                id: id.clone(),
                name: "read_file".into(),
                input: json!({"path": format!("m{round}.json")}),
                cache_control: None,
            },
        ));
        messages.push(tool_part_message(
            Role::User,
            KnownContentPart::ToolResult {
                tool_use_id: id,
                content: Value::String(payload),
                is_error: None,
                cache_control: None,
            },
        ));
    }
    let prior = messages.len();
    messages.push(user_message("Check the lockfile next."));
    (anchor_request(&messages), prior)
}

/// Pin the anchor fixture's size and shape before sampling, so a drifted
/// fixture fails loudly instead of pricing a different request.
fn assert_anchor_fixture(req: &ChatRequest, prior: usize) {
    let bytes = serde_json::to_string(req).map_or(0, |s| s.len());
    assert_eq!(
        bytes, ANCHOR_FIXTURE_BYTES,
        "anchor fixture {ANCHOR_PROFILE} drifted in size"
    );
    assert_eq!(prior, 1 + 2 * ANCHOR_ROUNDS);
    assert_eq!(req.messages.len(), prior + 1);
    assert!(
        RequestIdentity::measure(req, Some(prior))
            .digest()
            .is_some(),
        "anchor fixture {ANCHOR_PROFILE} must be fingerprintable"
    );
}

fn anchor_request(messages: &[Message]) -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: messages.to_vec().into(),
        max_tokens: Some(4096),
        ..Default::default()
    }
}

fn user_message(text: &str) -> Message {
    Message {
        role: Role::User,
        ..assistant_message(MessageContent::Text(text.into()), None)
    }
}

fn tool_part_message(role: Role, part: KnownContentPart) -> Message {
    Message {
        role,
        ..assistant_message(MessageContent::Parts(vec![ContentPart::Known(part)]), None)
    }
}

/// A deterministic canonical response per render profile. Plain profiles
/// carry a text completion; the tool-heavy profile carries a `tool_calls`
/// payload so the render exercises the tool-call rewrite path.
fn build_response(profile: SpectrumProfile) -> ChatResponse {
    let (message, finish_reason) = match profile {
        SpectrumProfile::ToolHeavy => {
            let tool_calls = vec![json!({
                "id": "call_bench_01",
                "type": "function",
                "function": {
                    "name": "lookup",
                    "arguments": "{\"query\":\"status of the pending request\"}"
                }
            })];
            (
                assistant_message(MessageContent::Null, Some(tool_calls)),
                "tool_calls",
            )
        }
        _ => (
            assistant_message(
                MessageContent::Text(
                    "The requested summary is ready and the context window is healthy.".into(),
                ),
                None,
            ),
            "stop",
        ),
    };

    ChatResponse {
        id: "msg_bench_render".into(),
        model: "claude-3-5-sonnet".into(),
        created: 0,
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: Some(finish_reason.into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 128,
            completion_tokens: 64,
            total_tokens: 192,
            ..Usage::default()
        }),
        routectl_provider: Some("bench".into()),
        extras: serde_json::Map::new(),
        upstream_meta: None,
    }
}

/// The lane every wire-check case travels.
fn wire_check_lane() -> Lane {
    Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi)
}

/// A `[wire_check]` block with the given sampling knobs.
fn wire_check_config(first_n: u32, sample_one_in: u32) -> WireCheckConfig {
    let config: Config = toml::from_str(&format!(
        "version = 5\n[wire_check]\nfirst_n = {first_n}\nsample_one_in = {sample_one_in}\n"
    ))
    .expect("bench wire_check config parses");
    config.wire_check
}

/// One profile's wire-check inputs, each produced by the real ingress,
/// provider and renderer outside any timed closure.
struct WireCheckFixture {
    /// The client's request body.
    ingress: Bytes,
    /// The request body the provider sent.
    egress: Bytes,
    /// The request edits the provider declared while building `egress`.
    request_edits: Vec<DeclaredEdit>,
    /// The upstream's complete response body.
    upstream: Bytes,
    /// The response body the client received.
    client: Bytes,
}

fn build_wire_check_fixture(
    headers: &HeaderMap,
    provider: &AnthropicApiProvider,
    profile: SpectrumProfile,
    fx: &BenchFixture,
) -> WireCheckFixture {
    let canonical = AnthropicIngress
        .parse_request(headers, &fx.anthropic_wire)
        .expect("anthropic fixture parses");
    let tap = WireTap::admitted();
    let mut tapped = canonical.clone();
    tapped.routectl_internal.wire_tap = Some(tap.clone());
    let egress = serialize_egress(provider, &tapped);
    let request_edits = tap.take_request().edits(Leg::Request).to_vec();
    let upstream = AnthropicIngress
        .render_response(build_response(profile))
        .expect("anthropic render");
    let client = render_upstream(provider, &canonical, &upstream);
    WireCheckFixture {
        ingress: Bytes::from(fx.anthropic_wire.clone()),
        egress,
        request_edits,
        upstream,
        client,
    }
}

/// The provider's request body for `req`, deposited into the request's tap
/// when it carries one, as the provider does before sending.
fn serialize_egress(provider: &AnthropicApiProvider, req: &ChatRequest) -> Bytes {
    let body = provider
        .normalize_request(req)
        .expect("anthropic normalize_request");
    let bytes = Bytes::from(serde_json::to_vec(&body).expect("egress body serializes"));
    if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
        tap.deposit(bytes.clone());
    }
    bytes
}

/// Parse and normalize the upstream body, relabel it for the client and
/// render it back to the client's dialect.
fn render_upstream(provider: &AnthropicApiProvider, req: &ChatRequest, upstream: &Bytes) -> Bytes {
    if let Some(tap) = req
        .routectl_internal
        .wire_tap
        .as_ref()
        .filter(|tap| tap.checks_response())
    {
        tap.deposit_response(upstream.clone());
    }
    let raw: Value = serde_json::from_slice(upstream).expect("upstream body parses");
    let mut resp = provider
        .normalize_response(raw)
        .expect("anthropic normalize_response");
    resp.routectl_provider = Some(WIRE_CHECK_PROVIDER.into());
    resp.model.clone_from(&req.model);
    AnthropicIngress
        .render_response(resp)
        .expect("anthropic render")
}

/// One request through the hot path: ingress parse, then
/// [`serve_parsed`].
fn serve(
    headers: &HeaderMap,
    provider: &AnthropicApiProvider,
    wire: &[u8],
    upstream: &Bytes,
) -> (Bytes, Bytes) {
    let req = AnthropicIngress
        .parse_request(headers, wire)
        .expect("anthropic fixture parses");
    serve_parsed(provider, &req, upstream)
}

fn serve_parsed(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
    upstream: &Bytes,
) -> (Bytes, Bytes) {
    let egress = serialize_egress(provider, req);
    let client = render_upstream(provider, req, upstream);
    (egress, client)
}

/// Attach the client's body to `req` as its ingress wire, with the version
/// the bench user-agent names.
fn capture_ingress(req: &mut ChatRequest, ingress: &Bytes) {
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: parse_claude_cli_version(WIRE_CHECK_USER_AGENT).map(str::to_string),
        bytes: ingress.clone(),
    });
}

/// Ask the sampler about `req` and attach the tap its answer earns.
fn install_tap(req: &mut ChatRequest, state: &WireCheckState, config: &WireCheckConfig) -> WireTap {
    let lane = wire_check_lane();
    let version = req
        .routectl_internal
        .ingress_wire
        .as_ref()
        .and_then(|wire| wire.client_version.as_deref());
    let tap = if state.admit(lane, version, config) {
        WireTap::checking_response(lane.ingress())
    } else {
        WireTap::default()
    };
    req.routectl_internal.wire_tap = Some(tap.clone());
    tap
}

/// Every response edit the router declares on a served complete body.
fn declare_response_edits(tap: &WireTap) {
    for (site, kind) in [
        (EditSite::ProviderAttributionAdd, EditKind::Add),
        (EditSite::ResponseModelRelabel, EditKind::Rewrite),
        (EditSite::ResponseModelRelabel, EditKind::Add),
    ] {
        let path = if site == EditSite::ProviderAttributionAdd {
            "/routectl_provider"
        } else {
            "/model"
        };
        tap.declare_response(DeclaredEdit::new(site, path, kind).expect("response edit pattern"));
    }
}

/// The full check of one admitted attempt, both legs, on top of the work
/// the hot path already did.
fn run_full_check(
    fx: &WireCheckFixture,
    mut req: ChatRequest,
    state: &WireCheckState,
    config: &WireCheckConfig,
) -> (CheckOutcome, CheckOutcome) {
    capture_ingress(&mut req, &fx.ingress);
    let tap = install_tap(&mut req, state, config);
    assert!(tap.checks_response(), "wire_check case must be admitted");
    for edit in &fx.request_edits {
        tap.declare(edit.clone());
    }
    tap.deposit(fx.egress.clone());
    tap.deposit_response(fx.upstream.clone());
    declare_response_edits(&tap);
    let request = check_request_leg(state, &req, &tap);
    let response = check_response_leg(state, &tap, &fx.client);
    (request, response)
}

fn check_request_leg(state: &WireCheckState, req: &ChatRequest, tap: &WireTap) -> CheckOutcome {
    let lane = wire_check_lane();
    let ingress = req
        .routectl_internal
        .ingress_wire
        .as_ref()
        .expect("captured ingress");
    let contents = tap.take_request();
    let edits: Vec<DeclaredEdit> = req
        .routectl_internal
        .declared_edits
        .iter()
        .chain(contents.edits(Leg::Request))
        .cloned()
        .collect();
    let record = AttemptRecord::new(ingress, req, contents.request(), &edits);
    let sent = record
        .egress()
        .and_then(|body| serde_json::from_slice::<Value>(body).ok());
    let received = serde_json::from_slice::<Value>(&record.ingress().bytes).ok();
    let (Some(sent), Some(received)) = (sent, received) else {
        state.record_incomplete(lane);
        return CheckOutcome::Incomplete;
    };
    let outcome = diff(
        &received,
        &sent,
        LaneClass::Fidelity,
        record.edits(Leg::Request),
    );
    state.record_compared(
        lane,
        Leg::Request,
        outcome.findings.iter().map(|f| (&f.path, f.class)),
        outcome.truncated,
    )
}

fn check_response_leg(state: &WireCheckState, tap: &WireTap, client: &Bytes) -> CheckOutcome {
    let lane = wire_check_lane();
    let declared = tap.take_response();
    let upstream = declared
        .response()
        .and_then(|body| serde_json::from_slice::<Value>(body).ok());
    let client = serde_json::from_slice::<Value>(client).ok();
    let (Some(upstream), Some(client)) = (upstream, client) else {
        state.record_incomplete(lane);
        return CheckOutcome::Incomplete;
    };
    let edits: Vec<DeclaredEdit> = declared.edits(Leg::Response).to_vec();
    let outcome = diff(&upstream, &client, LaneClass::Fidelity, &edits);
    state.record_compared(
        lane,
        Leg::Response,
        outcome.findings.iter().map(|f| (&f.path, f.class)),
        outcome.truncated,
    )
}

/// Pin that a profile's check finishes clean on both legs, so the case
/// prices the full diff of the common outcome and never the cheap
/// incomplete exit or the loss path's ring and log work.
fn assert_wire_check_compares(profile: SpectrumProfile, fx: &WireCheckFixture) {
    let state = WireCheckState::default();
    let (request, response) =
        run_full_check(fx, ChatRequest::default(), &state, &wire_check_config(0, 1));
    for (leg, outcome) in [("request", request), ("response", response)] {
        assert!(
            matches!(outcome, CheckOutcome::Clean),
            "wire_check {profile:?} {leg} leg was not clean: {outcome:?}"
        );
    }
}

fn main() {
    let headers = HeaderMap::new();
    let headers = &headers;

    let parse_fixtures: Vec<(SpectrumProfile, BenchFixture)> =
        PARSE_PROFILES.iter().map(|p| (*p, p.generate())).collect();
    let estimate_fixtures: Vec<(SpectrumProfile, BenchFixture)> = ESTIMATE_PROFILES
        .iter()
        .map(|p| (*p, p.generate()))
        .collect();
    let render_fixtures: Vec<(SpectrumProfile, ChatResponse)> = RENDER_PROFILES
        .iter()
        .map(|p| (*p, build_response(*p)))
        .collect();

    let anchor_fixture = anchor_fixture();

    let wire_check_provider = AnthropicApiProvider::new(AnthropicApiConfig::new(
        WIRE_CHECK_PROVIDER,
        "sk-ant-bench-key",
    ));
    let wire_check_fixtures: Vec<(SpectrumProfile, BenchFixture, WireCheckFixture)> =
        WIRE_CHECK_PROFILES
            .iter()
            .map(|p| {
                let fx = p.generate();
                let check = build_wire_check_fixture(headers, &wire_check_provider, *p, &fx);
                assert_wire_check_compares(*p, &check);
                (*p, fx, check)
            })
            .collect();
    let checked_state = WireCheckState::default();
    let checked_config = wire_check_config(0, 1);
    let declined_state = WireCheckState::default();
    // Past `first_n` the sampler admits the M-th attempt, so with M at
    // u32::MAX no timed attempt is ever admitted.
    let declined_config = wire_check_config(0, u32::MAX);

    let mut cases: Vec<BenchCase> = Vec::new();

    for (profile, fx) in &parse_fixtures {
        let anthropic_wire = &fx.anthropic_wire;
        cases.push(BenchCase::new(
            format!("ingress_parse__{}__anthropic", profile.snake_name()),
            move || {
                let req = AnthropicIngress
                    .parse_request(headers, anthropic_wire)
                    .expect("anthropic fixture parses");
                black_box(req);
            },
        ));

        let openai_wire = &fx.openai_wire;
        cases.push(BenchCase::new(
            format!("ingress_parse__{}__openai", profile.snake_name()),
            move || {
                let req = OpenAiIngress
                    .parse_request(headers, openai_wire)
                    .expect("openai fixture parses");
                black_box(req);
            },
        ));
    }

    for (profile, fx) in &estimate_fixtures {
        let req = &fx.canonical;
        cases.push(BenchCase::new(
            format!("token_estimate__{}__na", profile.snake_name()),
            move || {
                black_box(routectl_router::estimate_meter_tokens(req));
            },
        ));
    }

    // The anchor's normalized digest pass, with the persisted estimate over
    // the same request beside it for scale.
    let anchor_req = &anchor_fixture.0;
    let anchor_prior = anchor_fixture.1;
    assert_anchor_fixture(anchor_req, anchor_prior);
    cases.push(BenchCase::new(
        format!("token_estimate__{ANCHOR_PROFILE}__na"),
        move || {
            black_box(routectl_router::estimate_total_tokens(anchor_req));
        },
    ));
    cases.push(BenchCase::new(
        format!("anchor_identity__{ANCHOR_PROFILE}__na"),
        move || {
            black_box(RequestIdentity::measure(anchor_req, Some(anchor_prior)));
        },
    ));

    for (profile, resp) in &render_fixtures {
        cases.push(BenchCase::new_batched(
            format!("response_render__{}__anthropic", profile.snake_name()),
            move || resp.clone(),
            |resp: ChatResponse| {
                let wire = AnthropicIngress
                    .render_response(resp)
                    .expect("anthropic render");
                black_box(wire);
            },
        ));
        cases.push(BenchCase::new_batched(
            format!("response_render__{}__openai", profile.snake_name()),
            move || resp.clone(),
            |resp: ChatResponse| {
                let wire = OpenAiIngress.render_response(resp).expect("openai render");
                black_box(wire);
            },
        ));
    }

    let wire_check_provider = &wire_check_provider;
    let checked_state = &checked_state;
    let checked_config = &checked_config;
    let declined_state = &declined_state;
    let declined_config = &declined_config;
    for (profile, fx, check) in &wire_check_fixtures {
        let wire = &fx.anthropic_wire;
        cases.push(BenchCase::new(
            format!("hot_path__{}__anthropic", profile.snake_name()),
            move || {
                black_box(serve(headers, wire_check_provider, wire, &check.upstream));
            },
        ));
        cases.push(BenchCase::new(
            format!("wire_unadmitted__{}__anthropic", profile.snake_name()),
            move || {
                let mut req = AnthropicIngress
                    .parse_request(headers, wire)
                    .expect("anthropic fixture parses");
                capture_ingress(&mut req, &check.ingress);
                let tap = install_tap(&mut req, declined_state, declined_config);
                assert!(!tap.is_admitted(), "wire_unadmitted case must be declined");
                black_box(serve_parsed(wire_check_provider, &req, &check.upstream));
            },
        ));
        cases.push(BenchCase::new_batched(
            format!("wire_unadmitted_setup__{}__anthropic", profile.snake_name()),
            ChatRequest::default,
            move |mut req: ChatRequest| {
                capture_ingress(&mut req, &check.ingress);
                let tap = install_tap(&mut req, declined_state, declined_config);
                for edit in &check.request_edits {
                    tap.declare(edit.clone());
                }
                tap.deposit(check.egress.clone());
                black_box(req);
            },
        ));
        cases.push(BenchCase::new_batched(
            format!("wire_check__{}__anthropic", profile.snake_name()),
            ChatRequest::default,
            move |req: ChatRequest| {
                black_box(run_full_check(check, req, checked_state, checked_config));
            },
        ));
    }

    #[cfg(feature = "dhat")]
    if bench_alloc::dhat_profile_mode() {
        // Run every case once under a single heap profiler; the JSON is
        // written on profiler drop to DHAT_OUTPUT (or dhat's default file
        // name). Never inside criterion's sampling loop -- the dhat
        // allocator + backtrace capture distort wall time.
        let mut builder = dhat::Profiler::builder();
        if let Ok(path) = std::env::var("DHAT_OUTPUT") {
            builder = builder.file_name(path);
        }
        let _profiler = builder.build();
        for case in &cases {
            case.run();
        }
        return;
    }

    if bench_alloc::alloc_count_mode() {
        bench_alloc::run_alloc_count(&cases);
        return;
    }

    let mut c = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(20)
        .configure_from_args();
    for case in &cases {
        c.bench_function(case.name(), |b| {
            if case.is_batched() {
                b.iter_batched(
                    || case.setup(),
                    |input| case.run_measured(input),
                    BatchSize::SmallInput,
                );
            } else {
                b.iter(|| case.run());
            }
        });
    }
    c.final_summary();
}
