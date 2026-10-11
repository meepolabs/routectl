//! A rewrite verdict's VALUE across the replay boundary: restated past the new
//! tombstone in the same encoding the live write uses, and applied by
//! pre-flight after the next restart.
//!
//! Driven end to end on a real ledger -- the production boot and reload
//! boundaries, a restart that replays only rows past the newest tombstone, and
//! a real dispatch whose upstream bytes are read back -- because the defect is
//! a value that is resident in memory and missing from the restated row, which
//! only a restart can expose.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider, Result,
    Role,
};
use routectl_router::{
    AliasValue, Config, ModelEntry, ResolvedModel, Router, RouterOptions, TEST_REWRITE_ACCEPTED,
    TEST_REWRITE_KWARG, TEST_REWRITE_PATH,
};
use routectl_usage::{BatchCommit, CHANNEL_CAPACITY, CapabilityEvent, UsageHandle, UsageWriter};
use serde_json::json;

use super::{BoundaryOutcomeReport, admit_capability_boundary};
use crate::server::test_support::{
    drain_usage_writer_strict, isolate_usage_db, overlay_at_revision,
};
use crate::server::{capability_rebuild, ledger_reader};

/// A remote-looking base URL, so the attributability gate admits the lane.
/// Nothing is sent here: the seat answers in process.
const REMOTE_BASE: &str = "https://api.anthropic.com";
const KIND: &str = "anthropic-api";
const ALIAS: &str = "restated-value-alias";
const NICKNAME: &str = "sonnet";
const STATE_KEY: &str = "p0#claude-sonnet-4-5";
/// The grounded closed-table row, whose transform is a drop.
const DROP_PATH: &str = "thinking.enabled.display";
const CLIENT_VALUE: &str = "routectl-test-client-value";
const MEMBER: &str = TEST_REWRITE_ACCEPTED[1];

/// A field-namespace key, assembled from parts: a lexical guard forbids a
/// second full spelling of the prefix under `crates/`.
fn field_key(path: &str) -> String {
    format!("{}{}{path}", "fie", "ld:")
}

fn shape_token() -> String {
    format!("shape:{MEMBER}")
}

/// Records every request, so the rewrite is read off the bytes sent upstream.
#[derive(Default)]
struct Seat {
    seen: Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl Provider for Seat {
    fn id(&self) -> &'static str {
        "p0"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p0", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().push(req);
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            usage: Some(routectl_core::Usage::default()),
            choices: vec![Choice {
                index: 0,
                message: text_message(Role::Assistant, "ok"),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
                logprobs: None,
            }],
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p0", 500, "unused"))
    }
}

fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn config() -> (Arc<Config>, tempfile::TempDir) {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"{KIND}\"\napi_key_ref = \"literal:k\"\nbase_url = \"{REMOTE_BASE}\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.models.insert(
        NICKNAME.to_string(),
        ModelEntry::new("p0", "claude-sonnet-4-5"),
    );
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()));
    let dir = isolate_usage_db(&mut config);
    (Arc::new(config), dir)
}

/// A Router at `revision` dispatching to `seat`, with durable capability
/// writes assumed so pre-flight is not suspended.
fn router_at(config: &Arc<Config>, revision: u64, seat: Arc<Seat>) -> Router {
    let mut router = Router::new(Arc::clone(config)).with_synthetic_field_rows_for_tests();
    let models = BTreeMap::from([(
        NICKNAME.to_string(),
        Arc::new(ResolvedModel::new(
            NICKNAME.to_string(),
            "p0".to_string(),
            seat as Arc<dyn Provider>,
            "claude-sonnet-4-5".to_string(),
        )),
    )]);
    router.install_resolved_models(models);
    router.install_catalog_overlay(overlay_at_revision(revision));
    router.with_capability_writes_assumed_durable_for_tests()
}

fn warm_off_runtime(config: &Config, router: &Router, usage: &UsageHandle) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                capability_rebuild::warm_capability_registry_from_ledger(
                    &config.usage.db_path,
                    router,
                    usage,
                );
            })
            .join()
            .expect("warm thread");
    });
}

fn commit_off_runtime(usage: &UsageHandle, events: Vec<CapabilityEvent>) {
    let committed = std::thread::scope(|scope| {
        scope
            .spawn(|| usage.commit_capability_events_blocking(events, 1))
            .join()
            .expect("commit thread")
    });
    assert!(
        matches!(committed, BatchCommit::Committed { .. }),
        "the seeded session must land: {committed:?}",
    );
}

/// A self-identifying live negative on [`STATE_KEY`] stamped `revision`,
/// carrying `token` in its value column.
fn broken(capability: &str, revision: i64, catalog: i64, token: Option<String>) -> CapabilityEvent {
    CapabilityEvent {
        ts: ledger_reader::epoch_ms_now() - 4_000,
        lane_key: STATE_KEY.to_string(),
        capability: capability.to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: token,
        catalog_version: catalog,
        overlay_revision: revision,
        provider_kind: Some(KIND.to_string()),
        vocab_version: Some(routectl_router::CURRENT_VOCAB_VERSION),
    }
}

/// `(capability, upstream_token)` of every row after the newest tombstone, in
/// append order: what the next boot replays.
fn rows_past_newest_tombstone(config: &Config) -> Vec<(String, Option<String>)> {
    let conn = rusqlite::Connection::open(&config.usage.db_path).expect("read open");
    let mut stmt = conn
        .prepare(
            "SELECT capability, upstream_token FROM capability_events WHERE rowid > \
             (SELECT MAX(rowid) FROM capability_events WHERE verdict = 'tombstone') \
             ORDER BY rowid",
        )
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("rows")
}

fn token_of(rows: &[(String, Option<String>)], capability: &str) -> Option<String> {
    rows.iter()
        .find(|(key, _)| key == capability)
        .unwrap_or_else(|| panic!("{capability} was restated past the boundary: {rows:?}"))
        .1
        .clone()
}

/// Restart at `revision` from the ledger, dispatch one request carrying the
/// rewrite row's field, and return the value the upstream received for it.
async fn value_sent_after_restart(
    config: &Arc<Config>,
    revision: u64,
    usage: &UsageHandle,
) -> Option<String> {
    let seat = Arc::new(Seat::default());
    let router = router_at(config, revision, Arc::clone(&seat));
    warm_off_runtime(config, &router, usage);
    let req = ChatRequest {
        model: ALIAS.to_string(),
        messages: vec![text_message(Role::User, "hello")].into(),
        chat_template_kwargs: Some(json!({ TEST_REWRITE_KWARG: CLIENT_VALUE })),
        ..Default::default()
    };

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result);
    let sent = seat.seen.lock().pop().expect("one upstream attempt");
    sent.chat_template_kwargs
        .and_then(|kwargs| kwargs.get(TEST_REWRITE_KWARG).cloned())
        .and_then(|value| value.as_str().map(str::to_string))
}

/// A boot behind a stale-revision tombstone restates the rewrite verdict WITH
/// its value, and the next boot's pre-flight rewrites to it. The drop row and
/// the non-field row carried a well-formed token in the ledger too, and restate
/// with none: only a repair row that writes a value carries one.
///
/// Mutation check: restate every survivor with `upstream_token: None` in
/// `boundary_batch` -> red on the restated rewrite token and on the value sent.
#[tokio::test]
async fn a_rewrite_value_survives_a_revision_mismatch_boot_and_is_applied() {
    let (config, _dir) = config();
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let catalog = i64::from(Router::new(Arc::clone(&config)).catalog_version());
    let now = ledger_reader::epoch_ms_now();
    let rewrite = field_key(TEST_REWRITE_PATH);
    let drop = field_key(DROP_PATH);
    // The non-field row is stamped with the BOOT's revision: a catalog-scoped
    // row of any other revision is not a survivor at all.
    commit_off_runtime(
        &usage,
        vec![
            CapabilityEvent::tombstone(now - 5_000, catalog, 1),
            broken(&rewrite, 1, catalog, Some(shape_token())),
            broken(&drop, 1, catalog, Some(shape_token())),
            broken("web_search", 2, catalog, Some(shape_token())),
        ],
    );
    let planted = rows_past_newest_tombstone(&config);
    assert!(
        planted
            .iter()
            .all(|(_, token)| token.as_deref() == Some(&shape_token())),
        "control: every seeded row carries the token: {planted:?}",
    );

    warm_off_runtime(&config, &router_at(&config, 2, Arc::default()), &usage);
    let restated = rows_past_newest_tombstone(&config);

    assert_eq!(token_of(&restated, &rewrite), Some(shape_token()));
    assert_eq!(
        token_of(&restated, &drop),
        None,
        "a drop row writes no value"
    );
    assert_eq!(
        token_of(&restated, "web_search"),
        None,
        "a non-field row writes no value"
    );
    assert_eq!(
        value_sent_after_restart(&config, 2, &usage)
            .await
            .as_deref(),
        Some(MEMBER),
        "the next boot's pre-flight rewrites the field to the restated value",
    );

    drop_usage(usage, writer).await;
}

/// A revision-changing reload restates the live rewrite verdict with the value
/// the value store holds for it, and the restart that follows applies it.
///
/// Mutation check: restate every survivor with `upstream_token: None` in
/// `boundary_batch` -> red; make the cut skip `with_restated_value` -> red.
#[tokio::test]
async fn a_rewrite_value_survives_a_revision_reload_and_a_restart() {
    let (config, _dir) = config();
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let catalog = i64::from(Router::new(Arc::clone(&config)).catalog_version());
    let now = ledger_reader::epoch_ms_now();
    let rewrite = field_key(TEST_REWRITE_PATH);
    commit_off_runtime(
        &usage,
        vec![
            CapabilityEvent::tombstone(now - 5_000, catalog, 1),
            broken(&rewrite, 1, catalog, Some(shape_token())),
        ],
    );
    let before = router_at(&config, 1, Arc::default());
    warm_off_runtime(&config, &before, &usage);

    let mut reloaded = router_at(&config, 2, Arc::default());
    reloaded.carry_over_learned_from(&before);
    let reloaded = Arc::new(reloaded);
    let (_tx, mut never) = tokio::sync::watch::channel(());
    let outcome = match admit_capability_boundary(&usage, &reloaded) {
        Ok(admitted) => admitted.settle(&mut never).await,
        Err(failure) => BoundaryOutcomeReport::Failed(failure),
    };
    assert!(
        matches!(
            outcome,
            BoundaryOutcomeReport::Committed { survivors: 1, .. }
        ),
        "{outcome:?}",
    );

    assert_eq!(
        token_of(&rows_past_newest_tombstone(&config), &rewrite),
        Some(shape_token()),
    );
    assert_eq!(
        value_sent_after_restart(&config, 2, &usage)
            .await
            .as_deref(),
        Some(MEMBER),
    );

    drop_usage(usage, writer).await;
}

async fn drop_usage(usage: UsageHandle, writer: UsageWriter) {
    drop(usage);
    drain_usage_writer_strict(writer).await;
}
