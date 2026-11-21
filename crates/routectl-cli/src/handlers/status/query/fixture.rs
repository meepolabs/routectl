//! Test-only fully populated `/status/query` panels, grouped and bucketed,
//! read by this route's own `read_panel` from a seeded ledger and priced by
//! the router facade's pricer over the shared fixture config.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_router::prices::PriceBook;
use routectl_router::{Config, Router};
use routectl_usage::{GroupDim, QuerySpec, ReadLimit, open};
use serde_json::Value;

use super::super::panel_fixtures::{FIXTURE_NOW_MS, at_epoch_ms, fixture_config};
use super::super::router_view::StatusRouterHandle;
use super::super::utc_rfc3339;
use super::{Panel, QueryPayload, name_panel_groups, parse_body, read_panel};
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
/// subscription provider served twice (so it ranks into the Overview cards
/// that carry seat quota), once under the configured login and once under a
/// login the served config no longer names, an unpriced compat model and a
/// pre-dispatch hangup, read and named the way the route reads and names it.
pub fn populated_query_panel(
    group_by: GroupDim,
    bucket: Option<BucketUnit>,
) -> Panel<QueryPayload> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let db_path = dir.path().join("usage.db");
    seed_ledger(&db_path, &SEEDS, FIXTURE_NOW_MS);
    query_panel_over(&db_path, fixture_config(), group_by, bucket, FIXTURE_NOW_MS)
}

/// The route's answer to a QUERY `body` at `now_ms` over the ledger at
/// `db_path`: parsed by the route's own `parse_body`, its window resolved in
/// UTC, read and named as [`query_panel_over`] reads and names. `None` for a
/// body the route refuses.
pub fn query_answer(
    db_path: &Path,
    config: Config,
    body: &[u8],
    now_ms: i64,
) -> Option<Panel<QueryPayload>> {
    let parsed = parse_body(body).ok()?;
    let (spec, bucket) = parsed.resolve(&LocalZone::utc(), at_epoch_ms(now_ms));
    Some(read_and_name(db_path, config, spec, bucket, now_ms))
}

/// Today's window (UTC) at `now_ms` over the ledger at `db_path`, read by
/// the route's own `read_panel` and named by its own `name_panel_groups`,
/// priced and named against `config` with no price table.
pub fn query_panel_over(
    db_path: &Path,
    config: Config,
    group_by: GroupDim,
    bucket: Option<BucketUnit>,
    now_ms: i64,
) -> Panel<QueryPayload> {
    let bounds = window_bounds(&LocalZone::utc(), WindowFlag::Today, at_epoch_ms(now_ms));
    let spec = QuerySpec {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        group_by,
        alias_filter: None,
        provider_filter: None,
        model_filter: None,
        bucket: None,
        entry_accounts: Default::default(),
    };
    read_and_name(db_path, config, spec, bucket, now_ms)
}

fn read_and_name(
    db_path: &Path,
    config: Config,
    spec: QuerySpec,
    bucket: Option<BucketUnit>,
    now_ms: i64,
) -> Panel<QueryPayload> {
    let zone = LocalZone::utc();
    let group_by = spec.group_by;
    let router = StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
        config,
    )))));
    let prices = db_path.with_file_name("prices.json");
    let book = Arc::new(PriceBook::new(prices));
    let limit = ReadLimit::from(Instant::now() + Duration::from_secs(30));
    let pricer = router.pricer(book);
    let identities = pricer.account_identities();
    let pricer = pricer.pin();
    let panel = read_panel(
        db_path,
        spec,
        bucket,
        &pricer,
        utc_rfc3339(now_ms),
        &zone,
        at_epoch_ms(now_ms),
        &limit,
    );
    name_panel_groups(panel, group_by, &identities, &pricer)
}

/// One seeded ledger row: `(request id, hours before the fixture instant,
/// alias, model, provider, seat, outcome, failure class)`.
pub type Seed<'a> = (
    &'a str,
    i64,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
    Option<&'a str>,
);

const SEEDS: [Seed<'static>; 8] = [
    (
        "sonnet-ok-1",
        1,
        "fast",
        Some("sonnet"),
        Some("anthropic"),
        None,
        "ok",
        None,
    ),
    (
        "sonnet-ok-2",
        3,
        "fast",
        Some("sonnet"),
        Some("anthropic"),
        None,
        "ok",
        None,
    ),
    (
        "sonnet-limited",
        3,
        "fast",
        Some("sonnet"),
        Some("anthropic"),
        None,
        "upstream_error",
        Some("rate-limited"),
    ),
    (
        "seat-ok",
        2,
        "smart",
        Some("seat-sonnet"),
        Some("claude-seat"),
        Some("anthropic#a"),
        "ok",
        None,
    ),
    (
        "seat-ok-2",
        4,
        "smart",
        Some("seat-sonnet"),
        Some("claude-seat"),
        Some("anthropic#retired"),
        "ok",
        None,
    ),
    (
        "glm-ok",
        5,
        "smart",
        Some("glm"),
        Some("compat"),
        None,
        "ok",
        None,
    ),
    (
        "glm-cancelled",
        5,
        "smart",
        Some("glm"),
        Some("compat"),
        None,
        "cancelled",
        None,
    ),
    (
        "hangup",
        6,
        "smart",
        None,
        None,
        None,
        "client_disconnect",
        None,
    ),
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

/// Write `seeds` into a fresh ledger at `path`, each row its hours before
/// `now_ms`. A row that reached a provider streams, reports every token
/// dimension, a 5-minute cache write, a server-tool map and a fallback; a
/// row with no provider reports nothing.
pub fn seed_ledger(path: &Path, seeds: &[Seed<'_>], now_ms: i64) {
    let db = open(path).expect("open the fixture ledger");
    for &(id, hours_ago, alias, model, provider, seat, outcome, class) in seeds {
        let ts = now_ms - hours_ago * HOUR_MS;
        let served = provider.is_some();
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, upstream, provider, provider_kind, stream, \
                 outcome, resolved_class, latency_ms, ttfb_ms, tool_count, msg_count, \
                 attempt_count, fallback_count, input_tokens, output_tokens, \
                 reasoning_tokens, cache_read, cache_write_tokens, cache_write_lifetimes, \
                 server_tool_use, seat) \
                 VALUES (?1, ?1 + 2000, ?2, 'anthropic', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, 2000, ?12, 1, 4, 1, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
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
                    seat,
                ],
            )
            .expect("seed a fixture row");
    }
}
