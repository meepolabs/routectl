//! Ingress hot-path micro-benches: request parse, local token estimate,
//! and response render.
//!
//! Bench names follow the pinned `<stage>__<profile>__<dialect>`
//! convention (stages `ingress_parse`, `token_estimate`,
//! `response_render`; dialects `anthropic`, `openai`, `na`). Fixtures are
//! generated ONCE outside every timed closure per the
//! [`bench_fixtures`](routectl_testkit::bench_fixtures) contract.
//!
//! `ingress_parse` measures the full ingress translation the server does
//! per request: raw wire bytes -> `serde_json::Value` -> canonical
//! `ChatRequest` via the dialect adapter. `token_estimate` measures the
//! local pre-usage estimate over a canonical request (dialect-agnostic, so
//! `na`). `response_render` measures rendering a canonical `ChatResponse`
//! back to dialect wire JSON. The adapter consumes the response by value,
//! so each case is a batched one: the per-iteration clone is the untimed
//! setup and only the render is measured.

use std::hint::black_box;
use std::time::Duration;

use axum::http::HeaderMap;
use criterion::{BatchSize, Criterion};
use routectl_core::{ChatResponse, Choice, Message, MessageContent, Role, Usage};
#[cfg(not(feature = "dhat"))]
use routectl_testkit::bench_alloc::CountingAllocator;
use routectl_testkit::bench_alloc::{self, BenchCase};
use routectl_testkit::bench_fixtures::{BenchFixture, SpectrumProfile};
use serde_json::{Value, json};

use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::openai::OpenAiIngress;
use routectl_cli::ingress::token_estimate::estimate_input_tokens;

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
                black_box(estimate_input_tokens(req));
            },
        ));
    }

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
