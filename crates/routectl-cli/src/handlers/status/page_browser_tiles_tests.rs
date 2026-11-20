// The Now KPI tile sparklines in a real headless Chrome: a bucket with no
// reported value draws as a gap, a reported 0 draws on the baseline.
// `include!`d into a child module of `page::tests` beside `browser`; the
// scenarios live in page_browser_tiles_driver.js, which serves a synthetic
// six-bucket series one bucket width apart.

use serde_json::Value;

const TILES_DRIVER: &str = include_str!("page_browser_tiles_driver.js");

const TILES_BUDGET_MS: u64 = 60_000;

/// The sparkline viewBox: 120 wide, 24 tall, the lowest value drawn 2 above
/// the bottom edge.
const SPARK_WIDTH: f64 = 120.0;
const SPARK_BASELINE_Y: f64 = 22.0;

/// Six buckets across the full width put bucket `i` at `i * 24`.
const BUCKET_STEP_X: f64 = SPARK_WIDTH / 5.0;

fn run(name: &'static str) -> Vec<Value> {
    let scenario = browser::Scenario {
        name,
        hash: "now",
        snippets: &[TILES_DRIVER],
        budget_ms: TILES_BUDGET_MS,
    };
    browser::run_scenario(&scenario)
        .result
        .as_array()
        .expect("one observation per round")
        .clone()
}

/// The polylines of one series as `[[x, y], ...]` runs.
fn runs(spark: &Value, series: &str) -> Vec<Vec<(f64, f64)>> {
    spark[series]
        .as_array()
        .unwrap_or_else(|| panic!("series {series} in {spark:#}"))
        .iter()
        .map(|run| {
            run.as_array()
                .expect("a polyline's points")
                .iter()
                .map(|p| (p[0].as_f64().expect("x"), p[1].as_f64().expect("y")))
                .collect()
        })
        .collect()
}

fn xs(run: &[(f64, f64)]) -> Vec<f64> {
    run.iter().map(|p| p.0).collect()
}

fn bucket_xs(buckets: &[usize]) -> Vec<f64> {
    buckets.iter().map(|&i| i as f64 * BUCKET_STEP_X).collect()
}

/// One null TTFT bucket splits the line into the runs either side of it and
/// fills only under those runs, and stays out of the vertical scale, while the
/// same grid with every bucket present draws one line.
///
/// Mutation checks: bucketSamples coercing each value through num0 -> red (the
/// null bucket draws as 0 and the line stays one run); drawSegments plotting
/// an absent sample -> red; sparkSvg scaling over absent samples -> red.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_a_null_ttft_bucket_breaks_the_sparkline() {
    let rounds = run("tiles-ttft-null-bucket");
    let (present, with_null) = (&rounds[0], &rounds[1]);

    let whole = runs(present, "a");
    assert_eq!(whole.len(), 1, "{present:#}");
    assert_eq!(xs(&whole[0]), bucket_xs(&[0, 1, 2, 3, 4, 5]), "{present:#}");

    let split = runs(with_null, "a");
    assert_eq!(split.len(), 2, "{with_null:#}");
    assert_eq!(xs(&split[0]), bucket_xs(&[0, 1]), "{with_null:#}");
    assert_eq!(xs(&split[1]), bucket_xs(&[3, 4, 5]), "{with_null:#}");
    assert_eq!(with_null["polygons"], 2, "{with_null:#}");
    assert_eq!(
        split[0][0].1, SPARK_BASELINE_Y,
        "the null is outside the scale, so the lowest present value is the baseline: {with_null:#}"
    );
}

/// A bucket that reported 0 is a measurement: it sits on the baseline inside
/// one unbroken line.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_a_zero_ttft_bucket_sits_on_the_baseline() {
    let rounds = run("tiles-ttft-zero-bucket");
    let spark = &rounds[0];

    let line = runs(spark, "a");
    assert_eq!(line.len(), 1, "{spark:#}");
    assert_eq!(xs(&line[0]), bucket_xs(&[0, 1, 2, 3, 4, 5]), "{spark:#}");
    assert_eq!(line[0][2].1, SPARK_BASELINE_Y, "{spark:#}");
    assert!(
        line[0]
            .iter()
            .enumerate()
            .all(|(i, p)| i == 2 || p.1 < SPARK_BASELINE_Y),
        "only the zero bucket is on the baseline: {spark:#}"
    );
}

/// A bucket with no cache write reported draws no write point, beside a read
/// line that stays whole; with no bucket reporting a write there is no write
/// line at all.
#[test]
#[ignore = "drives a system headless Chrome"]
fn page_browser_tiles_unreported_cache_write_buckets_draw_no_write_points() {
    let rounds = run("tiles-cache-write-absent");
    let (partial, none) = (&rounds[0], &rounds[1]);

    let read = runs(partial, "a");
    assert_eq!(read.len(), 1, "{partial:#}");
    assert_eq!(xs(&read[0]), bucket_xs(&[0, 1, 2, 3, 4, 5]), "{partial:#}");
    let write = runs(partial, "b");
    assert_eq!(write.len(), 2, "{partial:#}");
    assert_eq!(xs(&write[0]), bucket_xs(&[0, 1]), "{partial:#}");
    assert_eq!(xs(&write[1]), bucket_xs(&[4, 5]), "{partial:#}");

    assert_eq!(runs(none, "a").len(), 1, "{none:#}");
    assert!(runs(none, "b").is_empty(), "{none:#}");
}
