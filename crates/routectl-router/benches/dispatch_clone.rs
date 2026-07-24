//! Dispatch hot-path micro-bench: `ChatRequest::clone()`.
//!
//! The router clones the canonical request on the dispatch path (fallback
//! chains re-attempt against a fresh copy). This bench measures that clone
//! directly on the shared spectrum fixtures. Dialect-agnostic, so every
//! bench name uses the `na` dialect token; names follow the pinned
//! `<stage>__<profile>__<dialect>` convention.

use std::hint::black_box;
use std::time::Duration;

use criterion::Criterion;
use routectl_testkit::bench_alloc::{self, BenchCase, CountingAllocator};
use routectl_testkit::bench_fixtures::{BenchFixture, SpectrumProfile};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Profiles whose canonical-request clone cost is measured.
const CLONE_PROFILES: [SpectrumProfile; 3] = [
    SpectrumProfile::ToolHeavy,
    SpectrumProfile::LargeImage,
    SpectrumProfile::LongSession,
];

fn main() {
    let fixtures: Vec<(SpectrumProfile, BenchFixture)> =
        CLONE_PROFILES.iter().map(|p| (*p, p.generate())).collect();

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
