//! Egress hot-path micro-bench: canonical `ChatRequest` -> provider wire
//! `Value` via `Provider::normalize_request` on the Anthropic Messages
//! provider.
//!
//! The provider is constructed ONCE outside the timed closures (it builds
//! a reqwest client); only the typed->Value normalization is measured.
//! Bench names follow the pinned `<stage>__<profile>__<dialect>`
//! convention with the `anthropic` dialect token, since the serialization
//! shape is Anthropic-specific.

use std::hint::black_box;
use std::time::Duration;

use criterion::Criterion;
use routectl_core::Provider;
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
#[cfg(not(feature = "dhat"))]
use routectl_testkit::bench_alloc::CountingAllocator;
use routectl_testkit::bench_alloc::{self, BenchCase};
use routectl_testkit::bench_fixtures::{BenchFixture, SpectrumProfile};

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

/// Profiles whose typed->wire normalization cost is measured.
const EGRESS_PROFILES: [SpectrumProfile; 3] = [
    SpectrumProfile::ToolHeavy,
    SpectrumProfile::PlainRoundTrip,
    SpectrumProfile::LongSession,
];

fn main() {
    let provider = AnthropicApiProvider::new(AnthropicApiConfig::new(
        "bench-anthropic",
        "sk-ant-bench-key",
    ));

    let fixtures: Vec<(SpectrumProfile, BenchFixture)> =
        EGRESS_PROFILES.iter().map(|p| (*p, p.generate())).collect();

    let mut cases: Vec<BenchCase> = Vec::new();
    for (profile, fx) in &fixtures {
        let req = &fx.canonical;
        let provider_ref = &provider;
        cases.push(BenchCase::new(
            format!("egress_serialize__{}__anthropic", profile.snake_name()),
            move || {
                let wire = provider_ref
                    .normalize_request(req)
                    .expect("anthropic normalize_request");
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
        c.bench_function(case.name(), |b| b.iter(|| case.run()));
    }
    c.final_summary();
}
