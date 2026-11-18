//! Test-only fully populated `/status/query` panels, grouped and bucketed,
//! read by this route's own `build_panel` from a seeded ledger and priced by
//! the router facade's pricer over the shared fixture config.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_router::prices::PriceBook;
use routectl_usage::{GroupDim, QueryResult, QuerySpec, ReadLimit, open};
use serde_json::Value;

use super::super::panel_fixtures::{FIXTURE_NOW_MS, fixture_as_of, fixture_now, fixture_router};
use super::super::router_view::StatusRouterHandle;
use super::{Panel, build_panel};
use crate::commands::usage::{BucketUnit, WindowFlag, window_bounds};
use crate::local_time::LocalZone;

const HOUR_MS: i64 = 3_600_000;

/// The serialized grouped (by model) envelope, as the Usage tab asks for it.
pub fn populated_query() -> Value {
    serialized(GroupDim::Model, None)
}

/// The serialized bucketed (by provider, hourly) envelope, as the Overview
/// tab asks for it.
pub fn populated_query_series() -> Value {
    serialized(GroupDim::Provider, Some(BucketUnit::Hour))
}

fn serialized(group_by: GroupDim, bucket: Option<BucketUnit>) -> Value {
    let panel = populated_query_panel(group_by, bucket);
    assert!(panel.unavailable.is_none(), "the fixture read succeeds");
    serde_json::to_value(panel).expect("the query panel serializes")
}

/// Today's window over a seeded ledger holding a priced API-key model, a
/// subscription seat, an unpriced compat model and a pre-dispatch hangup,
/// read the way the route reads it.
pub fn populated_query_panel(group_by: GroupDim, bucket: Option<BucketUnit>) -> Panel<QueryResult> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let db_path = dir.path().join("usage.db");
    seed_ledger(&db_path);
    let zone = LocalZone::utc();
    let now = fixture_now();
    let bounds = window_bounds(&zone, WindowFlag::Today, now);
    let spec = QuerySpec {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        group_by,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    };
    let router = StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(fixture_router())));
    let book = Arc::new(PriceBook::new(dir.path().join("prices.json")));
    let limit = ReadLimit::from(Instant::now() + Duration::from_secs(30));
    build_panel(
        &db_path,
        spec,
        bucket,
        &router.pricer(book).pin(),
        fixture_as_of(),
        &zone,
        now,
        &limit,
    )
}

/// One seeded ledger row: `(request id, hours before the fixture instant,
/// alias, model, provider, outcome, failure class)`.
type Seed = (
    &'static str,
    i64,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
    Option<&'static str>,
);

const SEEDS: [Seed; 7] = [
    (
        "sonnet-ok-1",
        1,
        "fast",
        Some("sonnet"),
        Some("anthropic"),
        "ok",
        None,
    ),
    (
        "sonnet-ok-2",
        3,
        "fast",
        Some("sonnet"),
        Some("anthropic"),
        "ok",
        None,
    ),
    (
        "sonnet-limited",
        3,
        "fast",
        Some("sonnet"),
        Some("anthropic"),
        "upstream_error",
        Some("rate-limited"),
    ),
    (
        "seat-ok",
        2,
        "smart",
        Some("seat-sonnet"),
        Some("claude-seat"),
        "ok",
        None,
    ),
    (
        "glm-ok",
        5,
        "smart",
        Some("glm"),
        Some("compat"),
        "ok",
        None,
    ),
    (
        "glm-cancelled",
        5,
        "smart",
        Some("glm"),
        Some("compat"),
        "cancelled",
        None,
    ),
    ("hangup", 6, "smart", None, None, "client_disconnect", None),
];

fn upstream_of(provider: &str) -> &'static str {
    match provider {
        "compat" => "glm-5",
        _ => "claude-sonnet-4-5",
    }
}

fn kind_of(provider: &str) -> &'static str {
    match provider {
        "compat" => "openai-compat",
        _ => "anthropic-api",
    }
}

/// Write [`SEEDS`] into a fresh ledger at `path`. A row that reached a
/// provider streams, reports every token dimension, a 5-minute cache write,
/// a server-tool map and a fallback; the hangup reports nothing.
fn seed_ledger(path: &Path) {
    let db = open(path).expect("open the fixture ledger");
    for (id, hours_ago, alias, model, provider, outcome, class) in SEEDS {
        let ts = FIXTURE_NOW_MS - hours_ago * HOUR_MS;
        let served = provider.is_some();
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, upstream, provider, provider_kind, stream, \
                 outcome, resolved_class, latency_ms, ttfb_ms, tool_count, msg_count, \
                 attempt_count, fallback_count, input_tokens, output_tokens, \
                 reasoning_tokens, cache_read, cache_write_tokens, cache_write_lifetimes, \
                 server_tool_use) \
                 VALUES (?1, ?1 + 2000, ?2, 'anthropic', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, 2000, ?12, 1, 4, 1, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                rusqlite::params![
                    ts,
                    id,
                    model.unwrap_or(alias),
                    alias,
                    model,
                    provider.map(upstream_of),
                    provider,
                    provider.map(kind_of),
                    i64::from(served),
                    outcome,
                    class,
                    served.then_some(400),
                    i64::from(id == "sonnet-ok-2"),
                    served.then_some(1_200),
                    served.then_some(300),
                    served.then_some(40),
                    served.then_some(800),
                    served.then_some(120),
                    served.then_some(r#"{"300":120}"#),
                    served.then_some(r#"{"web_search":1}"#),
                ],
            )
            .expect("seed a fixture row");
    }
}
