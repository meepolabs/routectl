//! Dispatch hot-path micro-benches: `ChatRequest::clone()` and the
//! lossless JSON minifier that runs on every eligible dispatch.
//!
//! The router clones the canonical request on the dispatch path (fallback
//! chains re-attempt against a fresh copy). This bench measures that clone
//! directly on the shared spectrum fixtures. Dialect-agnostic, so every
//! bench name uses the `na` dialect token; names follow the pinned
//! `<stage>__<profile>__<dialect>` convention.
//!
//! The `reduction_minify*` series is the gate for turning the minifier on
//! by default. `apply_json_minify` runs against a request whose message
//! buffer is SHARED (the per-attempt clone holds a second reference), so
//! any `Arc::make_mut` on that buffer pays a full deep copy of
//! `[Message]`. The overwhelmingly common outcome is nothing-to-strip, and
//! that outcome must not pay the copy. Each profile therefore contributes
//! a PAIR of cases measured in the same run:
//!
//! - `reduction_minify__<profile>__na` -- the shipped path.
//! - `reduction_minify_forced_cow__<profile>__na` -- the same work with the
//!   deep copy taken unconditionally first, i.e. the cost shape before the
//!   copy-on-write short-circuit existed.
//!
//! The pair is the comparison of record: both members run under the same
//! toolchain, allocator, and machine load, so their ratio is meaningful
//! even when absolute wall time is not (a loaded box shifts both).

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, Criterion};
use routectl_core::content_part::{ContentPart, KnownContentPart};
use routectl_core::context_reduction::{ReductionOutcome, apply_json_minify};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
#[cfg(not(feature = "dhat"))]
use routectl_testkit::bench_alloc::CountingAllocator;
use routectl_testkit::bench_alloc::{self, BenchCase};
use routectl_testkit::bench_fixtures::{BenchFixture, SpectrumProfile};
use serde_json::Value;

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

/// Profiles whose canonical-request clone cost is measured.
const CLONE_PROFILES: [SpectrumProfile; 3] = [
    SpectrumProfile::ToolHeavy,
    SpectrumProfile::LargeImage,
    SpectrumProfile::LongSession,
];

/// Shared-fixture profiles whose minify cost is measured. `LargeImage` is
/// absent on purpose: its single image part carries no minify target, so a
/// series for it would measure only `mutable_suffix_start`.
const REDUCTION_PROFILES: [SpectrumProfile; 2] =
    [SpectrumProfile::ToolHeavy, SpectrumProfile::LongSession];

/// Mutable-tail payload size of the maximum-body fixture, matching the
/// server's default `[server] max_body_bytes` ceiling -- the largest
/// request the minifier can ever be handed.
const MAX_BODY_TAIL_BYTES: usize = 32 * 1024 * 1024;

/// Number of tool-result messages the maximum-body payload is split across,
/// so the tail is a handful of large targets rather than one, matching how
/// a long tool-using session accumulates output.
const MAX_BODY_CHUNKS: usize = 4;

/// The profile token of the maximum-body fixture. It is built here rather
/// than added to [`SpectrumProfile`] because that enum's variants drive
/// every other bench's fan-out and the pinned baseline profile catalog; a
/// 32 MiB variant would silently enlarge all of them.
const MAX_BODY_PROFILE: &str = "max_body";

/// One tool-result message whose content is a JSON-text target.
fn tool_result_message(content: Value) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "toolu_bench".into(),
            content,
            is_error: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// A maximum-body request whose whole mutable tail is JSON text that is
/// already compact -- the nothing-to-strip outcome at the largest size the
/// minifier can see.
///
/// Each chunk is a compact JSON array of space-separated word strings. Two
/// properties make it the honest worst case: the scan must parse every byte
/// before it can conclude the payload cannot shrink, and the whitespace it
/// does carry sits INSIDE string literals, so a whitespace-presence
/// pre-filter could not dismiss it without parsing either.
fn max_body_request() -> ChatRequest {
    let words_per_chunk = MAX_BODY_TAIL_BYTES / MAX_BODY_CHUNKS / 8;
    let messages: Vec<Message> = (0..MAX_BODY_CHUNKS)
        .map(|chunk| {
            let text: String = (0..words_per_chunk)
                .map(|i| WORDS[(chunk + i) % WORDS.len()])
                .collect::<Vec<_>>()
                .join(" ");
            let compact = serde_json::to_string(&Value::Array(vec![Value::String(text)]))
                .expect("bench fixture: max-body chunk serializes");
            tool_result_message(Value::String(compact))
        })
        .collect();

    ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: messages.into(),
        max_tokens: Some(4096),
        ..Default::default()
    }
}

/// Fixed word bank for the maximum-body payload. Deterministic by
/// construction -- no clock, no entropy -- so two runs are byte-identical
/// and their numbers comparable.
const WORDS: [&str; 8] = [
    "request", "content", "tool", "result", "session", "message", "output", "context",
];

/// Reduce a spectrum fixture to the surface `apply_json_minify` reads: the
/// message buffer and the top-level `cache_control` that decides whether a
/// mutable tail exists at all.
///
/// Tools and system blocks are dropped deliberately. The measured closure
/// consumes its request by value, so whatever the request owns is DROPPED
/// inside the measurement -- and `ToolHeavy`'s four large tool schemas cost
/// hundreds of allocations to free, which would dominate the sample and
/// make the paired forced-copy case look cheaper than the shipped one. The
/// minifier never looks at those fields, so removing them narrows the
/// measurement to its actual subject. What survives is the message buffer
/// behind its `Arc`, whose per-iteration clone and drop are refcount work
/// unless the measured code forces a copy -- which is precisely the cost
/// this series is here to price.
fn minify_subject(req: &ChatRequest) -> ChatRequest {
    ChatRequest {
        model: req.model.clone(),
        messages: Arc::clone(&req.messages),
        cache_control: req.cache_control.clone(),
        ..Default::default()
    }
}

/// Pin the outcome each reduction fixture is measuring BEFORE the sampling
/// loop. The series exists to price the nothing-to-strip common case; a
/// fixture that drifted into `Applied` or `NoMutableTail` would keep
/// producing numbers while silently pricing a different path, so the
/// baseline would compare against work that is not the gate's subject.
fn assert_nothing_to_strip(profile: &str, req: &ChatRequest) {
    let mut probe = req.clone();
    let outcome = apply_json_minify(&mut probe);
    assert!(
        matches!(outcome, ReductionOutcome::NothingToStrip(_)),
        "reduction fixture {profile} must exercise the nothing-to-strip path, got {outcome:?}"
    );
}

fn main() {
    let fixtures: Vec<(SpectrumProfile, BenchFixture)> =
        CLONE_PROFILES.iter().map(|p| (*p, p.generate())).collect();
    let reduction_fixtures: Vec<(String, ChatRequest)> = REDUCTION_PROFILES
        .iter()
        .map(|p| {
            (
                p.snake_name().to_owned(),
                minify_subject(&p.generate().canonical),
            )
        })
        .chain(std::iter::once((
            MAX_BODY_PROFILE.to_owned(),
            max_body_request(),
        )))
        .collect();

    let mut cases: Vec<BenchCase> = Vec::new();
    for (profile, fx) in &fixtures {
        let req = &fx.canonical;
        cases.push(BenchCase::new(
            format!("dispatch_clone__{}__na", profile.snake_name()),
            move || {
                black_box(req.clone());
            },
        ));
    }
    for (profile, req) in &reduction_fixtures {
        assert_nothing_to_strip(profile, req);
        // The setup clone is what makes the message buffer shared, exactly
        // as the dispatch path's per-attempt clone does; it is O(1) and,
        // being setup, stays out of both the timing and the alloc tally.
        cases.push(BenchCase::new_batched(
            format!("reduction_minify__{profile}__na"),
            move || req.clone(),
            move |mut attempt: ChatRequest| {
                black_box(apply_json_minify(&mut attempt));
            },
        ));
        cases.push(BenchCase::new_batched(
            format!("reduction_minify_forced_cow__{profile}__na"),
            move || req.clone(),
            move |mut attempt: ChatRequest| {
                // The pre-copy-on-write cost shape: `Arc::make_mut` on a
                // shared buffer deep-copies `[Message]` before any target
                // is even classified. The paired series above is the same
                // work without it.
                black_box(Arc::make_mut(&mut attempt.messages).len());
                black_box(apply_json_minify(&mut attempt));
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
