//! The `cache` section on the CLI report and the served no-network report.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::cache_decision::CALLER_SUPPLIED;
use routectl_router::{Config, Finding, Status, overall_exit};
use routectl_testkit::ScopedEnv;

use crate::cache_ignore::UNAVAILABLE_QUERY_TIMEOUT;
use crate::commands::pricing::PriceSnapshot;
use crate::server::capability_rebuild::{WarmOutcome, WarmReport};

use super::super::render::render_human;
use super::super::served::{ResidentLearned, ServedInputs, ServedPrices};
use super::super::{
    GatherSources, build_report, build_report_no_network, gather_context_no_network,
};

/// Seed `turns` consecutive caller-marked turns on `(provider, model)`, an
/// hour before now, each reading and writing no cache. The first turn has no
/// previous one, so `turns` turns are `turns - 1` evidence turns.
fn seed_ignoring_lane(db_path: &Path, provider: &str, model: &str, turns: i64) {
    let db = routectl_usage::open(db_path).expect("open ledger");
    let base = chrono::Utc::now().timestamp_millis() - 3_600_000;
    db.conn().execute_batch("BEGIN").expect("begin");
    for i in 0..turns {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, provider_kind, seat, session_id, \
                 stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
                 fallback_count, input_tokens, cache_read, cache_write_tokens, \
                 cache_front_decision, prefix_epoch_event, operation, probe) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req', 'a', ?3, ?4, 'anthropic-api', NULL, \
                 'sess-1', 0, 'ok', 0, 0, 0, 1, 0, 5000, 0, 0, ?5, 0, 'inference', 0)",
                rusqlite::params![
                    base + i * 1_000,
                    format!("{provider}-{model}-{i}"),
                    model,
                    provider,
                    CALLER_SUPPLIED,
                ],
            )
            .expect("insert turn");
    }
    db.conn().execute_batch("COMMIT").expect("commit");
}

/// `direct` is anthropic-api on the default base (observable); `vendor` is
/// anthropic-api on a custom base with no `cache_capability` override, so
/// its cache hits are not observable.
fn fixture_toml(db_path: &Path) -> String {
    format!(
        "version = {}\n\
         [usage]\n\
         db_path = \"{}\"\n\
         [providers.direct]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://CACHE_SECTION_TEST_UNUSED\"\n\
         [providers.vendor]\n\
         kind = \"anthropic-api\"\n\
         base_url = \"https://vendor.example.invalid/anthropic\"\n\
         api_key_ref = \"env://CACHE_SECTION_TEST_UNUSED\"\n",
        routectl_router::CURRENT_CONFIG_VERSION,
        db_path.display(),
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    _xdg: ScopedEnv,
    config_path: PathBuf,
    db_path: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_path = dir.path().join("config.toml");
    let db_path = dir.path().join("usage.db");
    std::fs::write(&config_path, fixture_toml(&db_path)).unwrap();
    Fixture {
        _dir: dir,
        _xdg: xdg,
        config_path,
        db_path,
    }
}

/// The served inputs of a daemon that booted on `boot_db_path` and now holds
/// the config at `config_path`.
fn served_inputs(config_path: &Path, boot_db_path: &Path) -> ServedInputs {
    let config: Config =
        crate::server::parse_config_only(config_path).expect("fixture config parses");
    ServedInputs::new(
        Arc::new(config),
        ResidentLearned {
            entries: Vec::new(),
            seed_clears: Vec::new(),
            now: Instant::now(),
            now_ms: 1_000,
        },
        WarmReport {
            outcome: WarmOutcome::Replayed,
            summary: None,
            loaded_rows: 0,
        },
        ServedPrices::Pinned(PriceSnapshot::default()),
        boot_db_path.to_path_buf(),
    )
}

async fn served_cache_findings(inputs: ServedInputs, config_path: &Path) -> Vec<Finding> {
    let ctx =
        gather_context_no_network(config_path, GatherSources::Served(Box::new(inputs)), &[]).await;
    cache_findings(&build_report_no_network(&ctx).findings)
}

async fn disk_report(config_path: &Path) -> routectl_router::DoctorReport {
    let ctx = gather_context_no_network(config_path, GatherSources::Disk, &[]).await;
    build_report(&ctx)
}

fn cache_findings(findings: &[Finding]) -> Vec<Finding> {
    findings
        .iter()
        .filter(|f| f.section == "cache")
        .cloned()
        .collect()
}

fn named<'f>(findings: &'f [Finding], name: &str) -> &'f Finding {
    findings
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no `{name}` finding: {findings:#?}"))
}

/// One ignoring lane is exactly one Warn in `cache`, worded as markers sent
/// with no cache activity, and the doctor exit code stays 0.
///
/// Mutation check: map an ignoring lane to `Status::Fail` -> red here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn one_ignoring_lane_is_one_warn_and_the_exit_code_stays_zero() {
    // Arrange
    let fx = fixture();
    seed_ignoring_lane(&fx.db_path, "direct", "m-ignored", 4);
    seed_ignoring_lane(&fx.db_path, "direct", "m-thin", 2);

    // Act
    let report = disk_report(&fx.config_path).await;

    // Assert
    let cache = cache_findings(&report.findings);
    let warns: Vec<&Finding> = cache.iter().filter(|f| f.status == Status::Warn).collect();
    assert_eq!(warns.len(), 1, "{cache:#?}");
    assert_eq!(warns[0].name, "lane direct m-ignored");
    assert!(
        warns[0]
            .detail
            .starts_with("markers sent, no cache activity: 3 turns in 1 session over"),
        "{}",
        warns[0].detail
    );
    assert!(cache.iter().all(|f| f.status != Status::Fail));
    assert_eq!(
        named(&cache, "cache markers").detail,
        "2 lanes evaluated over the last 6 h: 1 with markers sent and no cache activity, 0 with \
         cache activity, 1 with too few turns to judge"
    );
    assert_eq!(overall_exit(&cache), 0);
    assert!(
        report.findings.iter().all(|f| f.status != Status::Fail),
        "premise: nothing else in the fixture fails: {:#?}",
        report.findings
    );
    assert_eq!(overall_exit(&report.findings), 0);
}

/// The disk and served gathers read the same ledger under the same config and
/// produce identical `cache` findings.
///
/// Mutation check: make the served gather skip the read (always
/// Unavailable) -> red here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn disk_and_served_gathers_produce_identical_cache_findings() {
    // Arrange
    let fx = fixture();
    seed_ignoring_lane(&fx.db_path, "direct", "m-ignored", 5);
    seed_ignoring_lane(&fx.db_path, "vendor", "m-dark", 5);

    // Act
    let disk = cache_findings(&disk_report(&fx.config_path).await.findings);
    let served =
        served_cache_findings(served_inputs(&fx.config_path, &fx.db_path), &fx.config_path).await;

    // Assert
    assert!(
        disk.iter()
            .any(|f| f.status == Status::Warn && f.name == "lane direct m-ignored"),
        "control: the disk gather sees the ignoring lane: {disk:#?}"
    );
    assert_eq!(disk, served);
}

/// The served gather reads the ledger the daemon booted on, as its driver and
/// the panel's other ledger reads do, even after a reload names another:
/// `usage.db_path` is restart-required.
///
/// Mutation check: have the served gather read `config.usage.db_path` ->
/// red here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_served_gather_reads_the_boot_ledger_not_a_reloaded_db_path() {
    // Arrange
    let fx = fixture();
    seed_ignoring_lane(&fx.db_path, "direct", "m-ignored", 5);
    let reloaded_db = fx.db_path.with_file_name("reloaded-usage.db");
    std::fs::write(&fx.config_path, fixture_toml(&reloaded_db)).unwrap();
    let inputs = served_inputs(&fx.config_path, &fx.db_path);

    // Act
    let served = served_cache_findings(inputs, &fx.config_path).await;
    let disk = cache_findings(&disk_report(&fx.config_path).await.findings);

    // Assert
    assert!(
        served
            .iter()
            .any(|f| f.status == Status::Warn && f.name == "lane direct m-ignored"),
        "the served gather reads the boot ledger: {served:#?}"
    );
    assert_eq!(
        named(&disk, "cache markers").detail,
        "unavailable: no_data",
        "control: the reloaded config names a ledger that does not exist"
    );
}

/// The unobservable line names a custom-base anthropic-api provider with no
/// `cache_capability` override, and nothing from it is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_unobservable_line_names_a_custom_base_anthropic_provider() {
    // Arrange
    let fx = fixture();
    seed_ignoring_lane(&fx.db_path, "vendor", "m-dark", 5);

    // Act
    let cache = cache_findings(&disk_report(&fx.config_path).await.findings);

    // Assert
    let line = named(&cache, "unobservable providers");
    assert_eq!(line.status, Status::Pass);
    assert_eq!(
        line.detail,
        "not evaluated (cache hits not observable): vendor"
    );
    let fix = line.remediation.as_deref().expect("remediation");
    for key in ["supports_top_level_cache_control", "cache_hit_observable"] {
        assert!(fix.contains(key), "the remediation names both keys: {fix}");
    }
    assert!(cache.iter().all(|f| f.status == Status::Pass), "{cache:#?}");
}

/// Providers whose current `supports_top_level_cache_control` differs from
/// the kind's default and from each other: `direct` (default host, true),
/// `vendor` (custom base, no override, false), `vendor_open` (custom base,
/// overridden observable with top-level support off), `aws_dark` (bedrock
/// invoke, overridden unobservable with top-level support on).
fn capability_toml(db_path: &Path) -> String {
    format!(
        "{}\
         [providers.vendor_open]\n\
         kind = \"anthropic-api\"\n\
         base_url = \"https://vendor-open.example.invalid/anthropic\"\n\
         api_key_ref = \"env://CACHE_SECTION_TEST_UNUSED\"\n\
         cache_capability = {{ supports_top_level_cache_control = false, \
         cache_hit_observable = true }}\n\
         [providers.aws_dark]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\n\
         cache_capability = {{ supports_top_level_cache_control = true, \
         cache_hit_observable = false }}\n",
        fixture_toml(db_path),
    )
}

/// Every remediation that sets `cache_capability` renders the provider's
/// current `supports_top_level_cache_control`, never the kind's default:
/// following it by hand leaves auto-emit as it is.
///
/// Mutation check: render `supports_top_level_cache_control = true` in place
/// of the carried value -> red at the vendor and vendor_open rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_remediation_keeps_each_providers_current_top_level_support() {
    // Arrange
    let fx = fixture();
    std::fs::write(&fx.config_path, capability_toml(&fx.db_path)).unwrap();
    seed_ignoring_lane(&fx.db_path, "direct", "m-a", 4);
    seed_ignoring_lane(&fx.db_path, "vendor_open", "m-b", 4);

    // Act
    let cache = cache_findings(&disk_report(&fx.config_path).await.findings);

    // Assert
    let rows: [(&str, &str, &str); 4] = [
        (
            "custom-base anthropic-api vendor, no override",
            "unobservable providers",
            "`vendor`: `cache_capability = { supports_top_level_cache_control = false, \
             cache_hit_observable = true }`",
        ),
        (
            "bedrock overridden unobservable, top-level support on",
            "unobservable providers",
            "`aws_dark`: `cache_capability = { supports_top_level_cache_control = true, \
             cache_hit_observable = true }`",
        ),
        (
            "default-host anthropic-api lane",
            "lane direct m-a",
            "`cache_capability = { supports_top_level_cache_control = true, \
             cache_hit_observable = false }`",
        ),
        (
            "custom-base lane overridden observable, top-level support off",
            "lane vendor_open m-b",
            "`cache_capability = { supports_top_level_cache_control = false, \
             cache_hit_observable = false }`",
        ),
    ];
    for (row, finding, expected) in rows {
        let fix = named(&cache, finding)
            .remediation
            .as_deref()
            .unwrap_or_else(|| panic!("row {row}: no remediation"));
        assert!(fix.contains(expected), "row {row}: {fix}");
        assert!(!fix.contains("kind's default"), "row {row}: {fix}");
    }
}

/// A missing ledger is one Pass `unavailable: no_data`, and the
/// unobservable line still stands, since it reads the config alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_missing_ledger_is_one_pass_unavailable_no_data() {
    // Arrange
    let fx = fixture();
    assert!(!fx.db_path.exists(), "premise: no ledger file");

    // Act
    let cache = cache_findings(&disk_report(&fx.config_path).await.findings);

    // Assert
    let markers = named(&cache, "cache markers");
    assert_eq!(markers.status, Status::Pass);
    assert_eq!(markers.detail, "unavailable: no_data");
    assert_eq!(
        cache.len(),
        2,
        "status line and unobservable line: {cache:#?}"
    );
}

/// A served read past its deadline sheds to `unavailable: query_timeout`,
/// and the rest of the report still builds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_served_read_past_its_deadline_reports_query_timeout() {
    // Arrange: enough rows that the query runs past one progress callback.
    let fx = fixture();
    {
        let db = routectl_usage::open(&fx.db_path).expect("open ledger");
        drop(db);
    }
    for lane in 0..500 {
        seed_ignoring_lane(&fx.db_path, "direct", &format!("m-{lane}"), 8);
    }
    let inputs = served_inputs(&fx.config_path, &fx.db_path).with_cache_read_budget(Duration::ZERO);

    // Act
    let ctx = gather_context_no_network(
        &fx.config_path,
        GatherSources::Served(Box::new(inputs)),
        &[],
    )
    .await;
    let report = build_report_no_network(&ctx);

    // Assert
    let cache = cache_findings(&report.findings);
    assert_eq!(
        named(&cache, "cache markers").detail,
        format!("unavailable: {UNAVAILABLE_QUERY_TIMEOUT}")
    );
    assert!(
        report.findings.iter().any(|f| f.section == "tuning"),
        "the other sections still render: {:#?}",
        report.findings
    );
}

/// The human render titles the section rather than filing it under "Other".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_human_render_titles_the_cache_section() {
    let fx = fixture();

    let lines = render_human(&disk_report(&fx.config_path).await);

    let at = lines
        .iter()
        .position(|l| l == "[Cache markers]")
        .unwrap_or_else(|| panic!("no cache title: {lines:?}"));
    assert!(
        lines[at + 1].starts_with("  PASS cache markers: unavailable: no_data"),
        "{lines:?}"
    );
}

/// The count nouns follow their counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn counts_of_one_read_singular() {
    // Arrange: one ignoring lane with 3 turns in 1 session; one lane total.
    let fx = fixture();
    seed_ignoring_lane(&fx.db_path, "direct", "m-ignored", 4);

    // Act
    let cache = cache_findings(&disk_report(&fx.config_path).await.findings);

    // Assert
    assert!(
        named(&cache, "cache markers")
            .detail
            .starts_with("1 lane evaluated over the last 6 h: 1 with markers sent"),
        "{cache:#?}"
    );
    let warn = named(&cache, "lane direct m-ignored");
    assert!(
        warn.detail.contains("3 turns in 1 session over"),
        "{}",
        warn.detail
    );
    let fix = warn.remediation.as_deref().expect("remediation");
    for key in [
        "supports_top_level_cache_control",
        "cache_hit_observable = false",
    ] {
        assert!(fix.contains(key), "the remediation names both keys: {fix}");
    }
}

/// The lane name prints the identifiers every surface prints: a key-shaped
/// model nickname is scrubbed exactly as the daemon log and usage print it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn the_lane_name_renders_the_shared_lane_identifiers() {
    // Arrange
    let fx = fixture();
    let model = crate::cache_ignore::key_shaped_model();
    seed_ignoring_lane(&fx.db_path, "direct", &model, 4);
    let ids = crate::cache_ignore::LaneIdentifiers::render("direct", None, Some(&model));

    // Act
    let cache = cache_findings(&disk_report(&fx.config_path).await.findings);

    // Assert
    let warn = cache
        .iter()
        .find(|f| f.status == Status::Warn)
        .unwrap_or_else(|| panic!("no warn: {cache:#?}"));
    assert_eq!(
        warn.name,
        format!("lane direct {}", ids.model.expect("model"))
    );
    assert!(
        !warn.name.contains(crate::cache_ignore::KEY_SHAPED_DIGITS),
        "{}",
        warn.name
    );
}

#[test]
fn each_not_evaluated_reason_is_its_own_pass_line() {
    use crate::cache_ignore::{NotEvaluatedProvider, NotEvaluatedReason};
    let providers = [
        NotEvaluatedProvider {
            provider: "conv".to_owned(),
            reason: NotEvaluatedReason::ConverseShape,
            supports_top_level_cache_control: false,
        },
        NotEvaluatedProvider {
            provider: "vendor".to_owned(),
            reason: NotEvaluatedReason::HitsNotObservable,
            supports_top_level_cache_control: false,
        },
    ];

    let findings = super::not_evaluated_findings(&providers);

    let lines: Vec<(&str, &str, Status)> = findings
        .iter()
        .map(|f| (f.name.as_str(), f.detail.as_str(), f.status))
        .collect();
    assert_eq!(
        lines,
        [
            (
                "unobservable providers",
                "not evaluated (cache hits not observable): vendor",
                Status::Pass
            ),
            (
                "converse providers",
                "not evaluated (Bedrock Converse forwards a top-level cache marker inert): conv",
                Status::Pass
            ),
        ]
    );
}
