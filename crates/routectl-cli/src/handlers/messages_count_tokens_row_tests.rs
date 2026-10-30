//! The usage row a `/v1/messages/count_tokens` request leaves in the ledger:
//! one row marked as a token count, with the served identity, outcome and
//! request id, kept out of inference readers and counted in observation
//! totals.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, RefusalReason, Result, TokenCount,
};
use routectl_router::{AliasValue, ProviderEntry, ResolvedModel, RetryPolicy};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::server::AppState;

const COUNT_BODY: &str = r#"{"model":"a","messages":[{"role":"user","content":"hi"}]}"#;
const WINDOW_FROM: i64 = 0;
const WINDOW_TO: i64 = i64::MAX;

/// Groups the priced per-model query returns over the whole window: the
/// reader behind the cost columns of `routectl usage` and `/status/usage`.
fn priced_groups(db: &routectl_usage::UsageDb) -> usize {
    let spec = routectl_usage::QuerySpec {
        from_ms: WINDOW_FROM,
        to_ms: WINDOW_TO,
        group_by: routectl_usage::GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket: None,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    routectl_usage::query(
        db,
        &spec,
        |_| routectl_usage::RowCost::Priced(1.0),
        deadline,
    )
    .expect("priced query")
    .groups
    .len()
}

/// A count provider that counts, refuses the request locally, or holds it
/// forever after signalling that it arrived.
struct CountDouble {
    refuses: bool,
    holds: Option<Arc<tokio::sync::Notify>>,
}

impl CountDouble {
    const fn counting() -> Self {
        Self {
            refuses: false,
            holds: None,
        }
    }

    const fn refusing() -> Self {
        Self {
            refuses: true,
            holds: None,
        }
    }
}

#[async_trait::async_trait]
impl Provider for CountDouble {
    fn id(&self) -> &'static str {
        "count-double"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("count-double", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("count-double", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("count-double", "unused"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        if let Some(arrived) = &self.holds {
            arrived.notify_one();
            std::future::pending::<()>().await;
        }
        if self.refuses {
            return Err(Error::Validation(
                "refused".into(),
                RefusalReason::FieldType,
            ));
        }
        Ok(TokenCount {
            input_tokens: 7,
            extras: Default::default(),
        })
    }
}

fn router_with(double: CountDouble) -> routectl_router::Router {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let config = routectl_router::Config {
        providers: BTreeMap::from([("p".to_owned(), ProviderEntry::anthropic_api("literal:k"))]),
        aliases: BTreeMap::from([("a".to_owned(), AliasValue::Single("m".to_owned()))]),
        retry,
        ..routectl_router::Config::default()
    };
    let mut router = routectl_router::Router::new(Arc::new(config));
    let provider = Arc::new(double) as Arc<dyn Provider>;
    router.install_resolved_models(BTreeMap::from([(
        "m".to_owned(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream")),
    )]));
    router
}

/// What one request left behind: the client status, the `x-request-id` the
/// client received, and the ledger it wrote to.
struct Posted {
    status: StatusCode,
    response_request_id: String,
    _dir: TempDir,
    db_path: PathBuf,
}

/// Post `body` through the real handler behind the production request-id
/// middleware into a fresh ledger, and shut the writer down so every row is
/// on disk.
async fn post_count(double: CountDouble, body: &'static str) -> Posted {
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let state = AppState::for_test_with_usage(
        Arc::new(arc_swap::ArcSwap::from_pointee(router_with(double))),
        usage,
    );
    let app = axum::Router::new()
        .route("/v1/messages/count_tokens", post(super::count_tokens))
        .layer(axum::middleware::from_fn(
            crate::server::request_id::middleware,
        ))
        .with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builds");

    let resp = app.oneshot(req).await.expect("router is infallible");
    let status = resp.status();
    let response_request_id = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("the middleware echoes x-request-id")
        .to_owned();
    to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("response body drains");
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("usage writer shutdown");
    Posted {
        status,
        response_request_id,
        _dir: dir,
        db_path,
    }
}

/// The columns these tests pin, one tuple per persisted row.
type RowView = (
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn rows(db_path: &Path) -> Vec<RowView> {
    let conn = rusqlite::Connection::open(db_path).expect("read open");
    let mut stmt = conn
        .prepare(
            "SELECT operation, provider, model, outcome, request_id, input_tokens, \
             output_tokens, reasoning_tokens, cache_read, cache_write_5m, cache_write_1h \
             FROM requests",
        )
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
        ))
    })
    .expect("query")
    .collect::<std::result::Result<_, _>>()
    .expect("rows")
}

#[tokio::test]
async fn a_successful_count_writes_one_count_tokens_row_with_null_tokens() {
    let posted = post_count(CountDouble::counting(), COUNT_BODY).await;

    assert_eq!(posted.status, StatusCode::OK);
    assert_eq!(
        rows(&posted.db_path),
        [(
            Some("count_tokens".to_owned()),
            Some("p".to_owned()),
            Some("m".to_owned()),
            "ok".to_owned(),
            posted.response_request_id.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
        )]
    );
}

#[tokio::test]
async fn a_failed_count_writes_one_row_with_the_dispatch_error_outcome_and_target() {
    let posted = post_count(CountDouble::refusing(), COUNT_BODY).await;

    assert_ne!(posted.status, StatusCode::OK);
    let rows = rows(&posted.db_path);
    let [(operation, provider, model, outcome, request_id, ..)] = rows.as_slice() else {
        panic!("exactly one row, got {rows:?}");
    };
    assert_eq!(operation.as_deref(), Some("count_tokens"));
    assert_eq!(provider.as_deref(), Some("p"));
    assert_eq!(model.as_deref(), Some("m"));
    assert_eq!(outcome, "upstream_error");
    assert_eq!(request_id, &posted.response_request_id);
}

#[tokio::test]
async fn a_count_row_is_absent_from_aggregate_and_cost_views_and_present_in_observation_totals() {
    // Arrange: a refused count, so the row carries a nonzero observation
    // count that observation totals can only report if they read the row.
    let posted = post_count(CountDouble::refusing(), COUNT_BODY).await;
    let db = routectl_usage::open(&posted.db_path).expect("open ledger");

    // Act
    let aggregate = routectl_usage::aggregate(&db, WINDOW_FROM, WINDOW_TO).expect("aggregate");
    let totals =
        routectl_usage::observation_totals(&db, WINDOW_FROM, WINDOW_TO).expect("observation");

    let priced = priced_groups(&db);

    // Assert
    assert!(aggregate.is_empty(), "count row leaked into {aggregate:?}");
    assert_eq!(priced, 0, "count row leaked into the priced query");
    assert_eq!(totals.local_refusals, 1, "{totals:?}");

    // Control: the same row read as inference is visible to the aggregate,
    // so its absence above is the operation marker, not the fixture.
    db.conn()
        .execute("UPDATE requests SET operation = 'inference'", [])
        .expect("retag row");
    let control = routectl_usage::aggregate(&db, WINDOW_FROM, WINDOW_TO).expect("aggregate");
    assert_eq!(control.len(), 1, "{control:?}");
    assert_eq!(priced_groups(&db), 1);
}

#[tokio::test]
async fn a_count_body_that_never_parses_writes_no_row() {
    let posted = post_count(CountDouble::counting(), "{ not valid json").await;

    assert_eq!(posted.status, StatusCode::BAD_REQUEST);
    assert_eq!(rows(&posted.db_path), []);
}

/// Operation, outcome, provider, model, upstream and missing terminals of
/// one persisted row.
type CancelView = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

/// A count whose handler future is dropped while the provider still holds
/// it writes one `cancelled` row naming the target the walk reached, with
/// no missing terminal.
#[tokio::test]
async fn a_count_aborted_mid_dispatch_writes_one_cancelled_row_at_its_last_target() {
    // Arrange
    let wait = std::time::Duration::from_secs(10);
    let arrived = Arc::new(tokio::sync::Notify::new());
    let double = CountDouble {
        refuses: false,
        holds: Some(Arc::clone(&arrived)),
    };
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (usage, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let state = AppState::for_test_with_usage(
        Arc::new(arc_swap::ArcSwap::from_pointee(router_with(double))),
        usage,
    );
    let app = axum::Router::new()
        .route("/v1/messages/count_tokens", post(super::count_tokens))
        .with_state(state);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("content-type", "application/json")
        .body(Body::from(COUNT_BODY))
        .expect("request builds");

    // Act
    let handler = tokio::spawn(app.oneshot(req));
    tokio::time::timeout(wait, arrived.notified())
        .await
        .expect("the provider receives the count");
    handler.abort();
    let joined = tokio::time::timeout(wait, handler)
        .await
        .expect("the aborted handler finishes");
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("usage writer shutdown");

    // Assert
    assert!(joined.is_err_and(|e| e.is_cancelled()));
    let conn = rusqlite::Connection::open(&db_path).expect("read open");
    let mut stmt = conn
        .prepare(
            "SELECT operation, outcome, provider, model, upstream, missing_terminals \
             FROM requests",
        )
        .expect("prepare");
    let rows: Vec<CancelView> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .expect("query")
        .collect::<std::result::Result<_, _>>()
        .expect("rows");
    assert_eq!(
        rows,
        [(
            "count_tokens".to_owned(),
            "cancelled".to_owned(),
            Some("p".to_owned()),
            Some("m".to_owned()),
            Some("upstream".to_owned()),
            Some(0),
        )]
    );
}
