//! Unit tests for `usage_capture` token-column stamping. Split out so
//! `usage_capture.rs` stays under the project's 800-line file ceiling.
//! Loaded via `#[cfg(test)] #[path = "usage_capture_tests.rs"] mod tests;`
//! from `usage_capture.rs`. `super::*` resolves to the `usage_capture`
//! module, so these tests reach the guard's private `record` field
//! directly to pin the persisted token columns.
//!
//! Coverage: the DB `input_tokens` column must hold cache-EXCLUSIVE new
//! input -- the inverse of `sum_prompt_tokens` -- so the disjoint cost
//! dimensions (input / cache_read / cache_write_*) do not double-count
//! cached tokens. Both capture sites (`observe_response`,
//! `observe_chunk`) must agree, and the None-vs-Some(0) contract must
//! survive the subtraction.

use super::*;
use routectl_core::schema::CacheCreation;
use routectl_core::{
    ChatChunk, ChatResponse, Choice, ChunkChoice, ChunkDelta, EvidenceSource, FailurePhase,
    Message, MessageContent, Role, Usage, UsageDelta,
};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};

/// A throwaway usage handle for guard construction. The tests assert on
/// the in-memory `record` before `finalize`, so the writer is never
/// drained -- it only has to exist so `UsageCapture::new` has a handle.
/// Returns the `TempDir` so the caller holds it to drop-at-scope-end;
/// these tests never touch the DB file, only the in-memory record.
fn dummy_handle() -> (UsageHandle, UsageWriter, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path, CHANNEL_CAPACITY, 0, true);
    (handle, writer, dir)
}

/// A `UsageCapture` over a minimal draft, ready for `observe_*` calls.
fn capture() -> (UsageCapture, UsageWriter, tempfile::TempDir) {
    let req = routectl_core::ChatRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    };
    let draft = build_usage_draft("anthropic", &req, "req-1".to_string());
    let (handle, writer, dir) = dummy_handle();
    (
        UsageCapture::new(draft, handle, "ingress-1".to_string()),
        writer,
        dir,
    )
}

/// A non-streaming response whose usage carries the Anthropic-style
/// cache-inclusive `prompt_tokens` plus the disjoint cache columns.
fn response_with_cache(
    prompt: Option<u32>,
    cache_read: Option<u32>,
    cache_creation_aggregate: Option<u32>,
    cache_write_5m: Option<u32>,
) -> ChatResponse {
    let usage = prompt.map(|p| Usage {
        prompt_tokens: p,
        completion_tokens: 50,
        total_tokens: p + 50,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation_aggregate,
        cache_creation: cache_write_5m.map(|v| CacheCreation {
            ephemeral_5m_input_tokens: Some(v),
            ephemeral_1h_input_tokens: None,
        }),
        ..Default::default()
    });
    ChatResponse {
        id: "resp-1".into(),
        model: "m".into(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        usage,
        ..Default::default()
    }
}

/// A terminal stream chunk carrying the same cumulative usage delta
/// Anthropic emits on `message_delta`.
fn chunk_with_cache(
    prompt: Option<u32>,
    cache_read: Option<u32>,
    cache_creation_aggregate: Option<u32>,
    cache_write_5m: Option<u32>,
) -> ChatChunk {
    let usage = prompt.map(|p| UsageDelta {
        prompt_tokens: Some(p),
        completion_tokens: Some(50),
        total_tokens: Some(p + 50),
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation_aggregate,
        cache_creation: cache_write_5m.map(|v| CacheCreation {
            ephemeral_5m_input_tokens: Some(v),
            ephemeral_1h_input_tokens: None,
        }),
        ..Default::default()
    });
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage,
        ..Default::default()
    }
}

#[test]
fn observe_response_stores_cache_exclusive_input() {
    // Arrange: prompt is cache-INCLUSIVE (100 new + 600 read + 300 create).
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(Some(1000), Some(600), Some(300), Some(300));

    // Act
    cap.observe_response(&resp);

    // Assert: input is the cache-EXCLUSIVE new input only.
    assert_eq!(cap.record.input_tokens, Some(100_u64));
    // The disjoint cache columns are stamped exactly as before.
    assert_eq!(cap.record.cache_read, Some(600_u64));
    assert_eq!(cap.record.cache_write_5m, Some(300_u64));
    assert_eq!(cap.record.cache_write_1h, None);
    assert_eq!(cap.record.output_tokens, Some(50_u64));
}

#[test]
fn observe_chunk_matches_response_for_same_usage() {
    // Arrange: a terminal chunk carrying the identical cumulative usage.
    let (mut cap, _w, _dir) = capture();
    let chunk = chunk_with_cache(Some(1000), Some(600), Some(300), Some(300));

    // Act
    cap.observe_chunk(&chunk);

    // Assert: parity with observe_response -- cache-exclusive new input.
    assert_eq!(cap.record.input_tokens, Some(100_u64));
    assert_eq!(cap.record.cache_read, Some(600_u64));
    assert_eq!(cap.record.cache_write_5m, Some(300_u64));
}

#[test]
fn observe_response_absent_usage_leaves_input_none() {
    // Arrange: no usage on the response at all.
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(None, None, None, None);

    // Act
    cap.observe_response(&resp);

    // Assert: None preserved (never coerced to Some(0)).
    assert_eq!(cap.record.input_tokens, None);
}

#[test]
fn observe_response_no_cache_fields_keeps_full_prompt() {
    // Arrange: prompt present, both cache_read and cache_creation absent.
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(Some(1000), None, None, None);

    // Act
    cap.observe_response(&resp);

    // Assert: nothing to subtract => input equals the full prompt.
    assert_eq!(cap.record.input_tokens, Some(1000_u64));
}

#[test]
fn observe_response_fully_cached_prompt_stores_zero() {
    // Arrange: prompt == cache_read + cache_creation exactly (700+300).
    let (mut cap, _w, _dir) = capture();
    let resp = response_with_cache(Some(1000), Some(700), Some(300), None);

    // Act
    cap.observe_response(&resp);

    // Assert: a fully-cached prompt is a real Some(0), not None.
    assert_eq!(cap.record.input_tokens, Some(0_u64));
}

/// Spin until the writer has persisted `want` rows or a deadline passes.
fn wait_persisted(handle: &UsageHandle, want: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.counters().persisted() < want {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

/// A `UsageCapture` plus the handle (so tests can poll the persisted
/// counter) and the writer (so tests can drain it to disk on shutdown).
fn capture_with_handle() -> (UsageCapture, UsageHandle, UsageWriter, tempfile::TempDir) {
    let req = routectl_core::ChatRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    };
    let draft = build_usage_draft("anthropic", &req, "req-take".to_string());
    let (handle, writer, dir) = dummy_handle();
    let cap = UsageCapture::new(draft, handle.clone(), "ingress-1".to_string());
    (cap, handle, writer, dir)
}

#[tokio::test]
async fn finalize_then_drop_still_one_row() {
    // Arrange
    let (mut cap, handle, writer, _dir) = capture_with_handle();

    // Act: explicit finalize sets `finalized`; the trailing Drop sees the
    // flag and short-circuits (never touches the moved-out record).
    cap.finalize(Outcome::Ok);
    drop(cap);

    // Assert: the Drop guard does NOT emit a second row.
    assert!(wait_persisted(&handle, 1), "row not persisted");
    writer.shutdown();
    assert_eq!(
        handle.counters().persisted(),
        1,
        "finalize-then-drop must persist exactly one row"
    );
}

#[test]
fn thrash_fires_only_for_auto_emitted_create_without_read() {
    // Thrash: routectl auto-emitted, a cache entry was created, no read.
    assert!(is_cache_thrash(Some("auto_emitted"), 300, 0));

    // Healthy: created AND read -> the cache is working, not thrash.
    assert!(!is_cache_thrash(Some("auto_emitted"), 300, 600));

    // Auto-emitted but nothing created -> not thrash.
    assert!(!is_cache_thrash(Some("auto_emitted"), 0, 0));

    // Read-only hit on a pre-existing entry (no new creation this request)
    // -> not thrash; the cache is being used.
    assert!(!is_cache_thrash(Some("auto_emitted"), 0, 600));

    // Caller-supplied breakpoint -> routectl did not decide; never thrash.
    assert!(!is_cache_thrash(Some("caller_supplied"), 300, 0));

    // Skipped strategies -> never thrash.
    assert!(!is_cache_thrash(Some("auto_skipped:no_capability"), 300, 0));
    assert!(!is_cache_thrash(Some("volatile_vetoed"), 300, 0));

    // No strategy recorded (request never dispatched) -> never thrash.
    assert!(!is_cache_thrash(None, 300, 0));
}

#[test]
fn cache_hit_pct_zero_read_is_zero() {
    // Arrange / Act / Assert: no read against a real prompt is 0%.
    assert_eq!(cache_hit_pct(0, 1000), 0);
}

#[test]
fn cache_hit_pct_full_read_is_hundred() {
    // Arrange / Act / Assert: read == prompt is exactly 100%.
    assert_eq!(cache_hit_pct(1000, 1000), 100);
}

#[test]
fn cache_hit_pct_partial_read_is_integer_percent() {
    // Arrange / Act / Assert: 600 of 1000 -> 60% (integer truncation).
    assert_eq!(cache_hit_pct(600, 1000), 60);
    // 1 of 3 -> 33% (floor, not round).
    assert_eq!(cache_hit_pct(1, 3), 33);
}

#[test]
fn cache_hit_pct_zero_prompt_guards_divide() {
    // Arrange / Act / Assert: prompt == 0 yields 0%, never a panic.
    assert_eq!(cache_hit_pct(0, 0), 0);
    assert_eq!(cache_hit_pct(500, 0), 0);
}

// -------- observe_meta: forwarded-credential disambiguation ---------------
//
// `DispatchMeta` is `#[non_exhaustive]`, so it cannot be struct-literal
// constructed from outside `routectl-router` -- only the router builds one
// (see `ingress_handle_tests.rs`'s `k_recording_router_and_meta`, the same
// pattern reused here). `any_dispatch_meta` gets a REAL router-built meta
// from a one-entry chain over an unreachable upstream: `mark_target` runs
// BEFORE the upstream is ever touched, so the dispatch fails but the meta
// is still fully populated, with no network required. These tests then
// mutate the already-owned instance's public fields directly (legal
// regardless of `#[non_exhaustive]`, which blocks literal construction,
// not field mutation on an owned value) to exercise both branches of
// `observe_meta`'s marker-stamping without needing a full forwarded
// AnthropicApi chain wired through the factory.

async fn any_dispatch_meta() -> routectl_router::DispatchMeta {
    use routectl_auth::{MemoryStore, SecretStore};
    use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", crate::test_secret::file_ref("k")),
    );
    let mut models = BTreeMap::new();
    models.insert("m".to_string(), ModelEntry::new("p", "gpt-4o"));
    let mut aliases = BTreeMap::new();
    aliases.insert("a".to_string(), AliasValue::Single("m".to_string()));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let config = Arc::new(Config {
        providers,
        aliases,
        models,
        retry,
        ..Default::default()
    });
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = crate::server::build_router_from_config(config, secrets)
        .await
        .expect("build router");
    let req = routectl_core::ChatRequest {
        model: "a".to_string(),
        messages: vec![].into(),
        ..Default::default()
    };
    router
        .complete_with_options(req, Default::default())
        .await
        .meta
}

#[tokio::test]
async fn observe_meta_forwarded_credential_stamps_credential_source_marker() {
    // Arrange: simulate the forwarded branch `mark_target` takes --
    // served_upstream carries the client's model, and the marker is set.
    let mut meta = any_dispatch_meta().await;
    meta.served_forwarded_credential = true;
    meta.served_upstream = Some("opus".to_string());
    let (mut cap, _w, _dir) = capture();

    // Act
    cap.observe_meta(&meta);

    // Assert: the client's model lands in the `upstream` column, and the
    // disambiguation marker is stamped into the existing `extra` JSON
    // column rather than a new schema column.
    assert_eq!(cap.record.upstream, Some("opus".to_string()));
    assert_eq!(
        cap.record.extra,
        Some(serde_json::json!({"credential_source": "forwarded"})),
    );
}

#[tokio::test]
async fn observe_meta_own_credential_leaves_extra_untouched() {
    // Arrange: an own-lane meta -- `served_forwarded_credential` is false
    // by construction (mark_target's default for a non-forwarded target).
    let meta = any_dispatch_meta().await;
    assert!(!meta.served_forwarded_credential, "sanity: own lane");
    let (mut cap, _w, _dir) = capture();

    // Act
    cap.observe_meta(&meta);

    // Assert: byte-for-byte unchanged -- no disambiguation marker on an
    // own-credential row.
    assert_eq!(cap.record.extra, None);
}

#[tokio::test]
async fn observe_meta_forwarded_credential_preserves_existing_extra_keys() {
    // Arrange: `extra` already carries an unrelated marker (as
    // `mark_stream_stage` would leave it) before observe_meta runs.
    let mut meta = any_dispatch_meta().await;
    meta.served_forwarded_credential = true;
    let (mut cap, _w, _dir) = capture();
    cap.mark_stream_stage(StreamStage::MidStream);

    // Act
    cap.observe_meta(&meta);

    // Assert: additive -- both keys survive.
    assert_eq!(
        cap.record.extra,
        Some(serde_json::json!({
            "stream_stage": "mid_stream",
            "credential_source": "forwarded",
        })),
    );
}

// -------- observe_meta: learn-event drain to the usage writer -------------
//
// `observe_meta` drains `DispatchMeta::learned_capabilities` into the
// usage writer via `try_send_learn_event` -- one enqueue per captured
// event, mapping the router-side struct to the plain usage-side row
// (`SignalTier::as_str` for the tier, the derived feature set carried
// through for replay verification). These tests build a REAL router meta
// (`any_dispatch_meta`, the `#[non_exhaustive]` construction pattern above)
// and push learn events onto its public `learned_capabilities` vec, then
// assert the writer receives / persists exactly N rows and that the field
// mapping survives the round trip. The handle is dropped before the writer
// shutdown so the channel closes (repo learning: shutdown otherwise blocks
// on a deadline waiting for a channel that never closes).

fn wait_learn_persisted(handle: &UsageHandle, want: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.counters().learn_events_persisted() < want {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

fn learn_event(
    capability_key: &str,
    tier: routectl_core::SignalTier,
) -> routectl_router::router::CapabilityLearnEvent {
    routectl_router::router::CapabilityLearnEvent {
        state_key: "prov".to_string(),
        capability_key: capability_key.to_string(),
        provider_kind: "anthropic-api".to_string(),
        signal_tier: tier,
        observations: 2,
        upstream_status: 400,
        remapped: false,
        request_features: vec!["web_search".to_string(), "prefill".to_string()],
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
    }
}

#[tokio::test]
async fn observe_meta_drains_each_learn_event_to_one_row() {
    // Arrange: two captured learn events ride the dispatch meta.
    let mut meta = any_dispatch_meta().await;
    meta.learned_capabilities = vec![
        learn_event("web_search", routectl_core::SignalTier::Inferred),
        learn_event("computer_use", routectl_core::SignalTier::SelfIdentifying),
    ];
    let (mut cap, handle, writer, _dir) = capture_with_handle();

    // Act: observe_meta drains both events into the writer.
    cap.observe_meta(&meta);

    // Assert: N events -> N enqueued and N persisted rows.
    assert_eq!(handle.counters().learn_events_enqueued(), 2);
    assert!(
        wait_learn_persisted(&handle, 2),
        "learn events not persisted"
    );
    drop(cap);
    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn observe_meta_empty_learn_events_enqueues_nothing() {
    // Arrange: the common path -- no captured capability observations.
    let meta = any_dispatch_meta().await;
    assert!(
        meta.learned_capabilities.is_empty(),
        "sanity: none captured"
    );
    let (mut cap, handle, writer, _dir) = capture_with_handle();

    // Act
    cap.observe_meta(&meta);

    // Assert: an empty vec enqueues nothing.
    assert_eq!(handle.counters().learn_events_enqueued(), 0);
    drop(cap);
    drop(handle);
    writer.shutdown();
}

#[tokio::test]
async fn observe_meta_maps_learn_event_fields_onto_the_row() {
    // Arrange: one inferred event with a known feature set.
    let mut meta = any_dispatch_meta().await;
    meta.learned_capabilities = vec![learn_event(
        "web_search",
        routectl_core::SignalTier::Inferred,
    )];
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = routectl_usage::UsageWriter::start(
        db_path.clone(),
        routectl_usage::CHANNEL_CAPACITY,
        0,
        true,
    );
    let draft = build_usage_draft("anthropic", &minimal_request(), "req-learn".to_string());
    let mut cap = UsageCapture::new(draft, handle.clone(), "ingress-1".to_string());

    // Act
    cap.observe_meta(&meta);
    assert!(
        wait_learn_persisted(&handle, 1),
        "learn event not persisted"
    );
    drop(cap);
    drop(handle);
    writer.shutdown();

    // Assert: the router struct maps onto the plain usage row -- the tier is
    // the persisted `as_str` token and the feature set is the JSON array.
    let conn = rusqlite::Connection::open(&db_path).expect("read db");
    let (state_key, capability_key, provider_kind, tier, observations, status, remapped, features): (
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        String,
    ) = conn
        .query_row(
            "SELECT state_key, capability_key, provider_kind, signal_tier, observations, \
             upstream_status, remapped, request_features FROM capability_learn_events",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .expect("one learn event row");
    assert_eq!(state_key, "prov");
    assert_eq!(capability_key, "web_search");
    assert_eq!(provider_kind, "anthropic-api");
    assert_eq!(tier, "inferred");
    assert_eq!(observations, 2);
    assert_eq!(status, 400);
    assert_eq!(remapped, 0);
    assert_eq!(features, r#"["web_search","prefill"]"#);
}

#[tokio::test]
async fn observe_meta_bounds_untrusted_request_features_on_the_row() {
    // Arrange: a learn event whose feature set is both too long (more than
    // the entry cap) and holds an overlong string -- both derive from
    // arbitrary client tool-type strings, so both must be bounded before
    // they reach the persisted row.
    let mut meta = any_dispatch_meta().await;
    let mut ev = learn_event("web_search", routectl_core::SignalTier::Inferred);
    let overlong = "a".repeat(500);
    let mut features: Vec<String> = (0..40).map(|i| format!("feature_{i}")).collect();
    features.push(overlong);
    ev.request_features = features;
    meta.learned_capabilities = vec![ev];

    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = routectl_usage::UsageWriter::start(
        db_path.clone(),
        routectl_usage::CHANNEL_CAPACITY,
        0,
        true,
    );
    let draft = build_usage_draft("anthropic", &minimal_request(), "req-bound".to_string());
    let mut cap = UsageCapture::new(draft, handle.clone(), "ingress-1".to_string());

    // Act
    cap.observe_meta(&meta);
    assert!(
        wait_learn_persisted(&handle, 1),
        "learn event not persisted"
    );
    drop(cap);
    drop(handle);
    writer.shutdown();

    // Assert: the persisted JSON array is capped at 16 entries and no entry
    // exceeds 128 chars.
    let conn = rusqlite::Connection::open(&db_path).expect("read db");
    let features_json: String = conn
        .query_row(
            "SELECT request_features FROM capability_learn_events",
            [],
            |r| r.get(0),
        )
        .expect("one learn event row");
    let persisted: Vec<String> =
        serde_json::from_str(&features_json).expect("features are a JSON array");
    assert_eq!(persisted.len(), 16, "entry count is capped");
    assert!(
        persisted.iter().all(|f| f.chars().count() <= 128),
        "each entry is truncated to the char cap"
    );
}

fn minimal_request() -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

// -------- build_usage_draft: session_id sourced from inbound_session_key ----
//
// `build_usage_draft` no longer takes a separately-derived `session_id`
// parameter; it reads `req.routectl_internal.inbound_session_key` --
// the SAME canonical value the Anthropic ingress resolves (header THEN
// `metadata.session_id` fallback) and the K-estimator keys on. These
// tests drive the real ingress parse so the request carries a genuine
// `inbound_session_key`, then assert the draft's `session_id` column
// matches it exactly.

fn anthropic_request(
    headers: &axum::http::HeaderMap,
    body: serde_json::Value,
) -> routectl_core::ChatRequest {
    use crate::ingress::IngressAdapter;
    use crate::ingress::anthropic::AnthropicIngress;
    AnthropicIngress.parse_request_value(headers, body).unwrap()
}

#[test]
fn build_usage_draft_metadata_only_session_key_lands_in_session_id() {
    // Arrange: header absent, session identity only in body metadata.
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata"}
    });
    let req = anthropic_request(&axum::http::HeaderMap::new(), body);

    // Act
    let draft = build_usage_draft("anthropic", &req, "req-meta".to_string());

    // Assert: the ledger row still gets a session identity even though
    // the header was never sent.
    assert_eq!(draft.session_id.as_deref(), Some("sid-from-metadata"));
}

#[test]
fn build_usage_draft_trims_header_derived_session_id() {
    // Arrange: the header carries leading/trailing whitespace.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-claude-code-session-id",
        axum::http::HeaderValue::from_str("  sid-from-header  ").unwrap(),
    );
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let req = anthropic_request(&headers, body);

    // Act
    let draft = build_usage_draft("anthropic", &req, "req-trim".to_string());

    // Assert: the untrimmed raw header value never reaches the ledger row.
    assert_eq!(draft.session_id.as_deref(), Some("sid-from-header"));
}

#[test]
fn build_usage_draft_header_wins_over_metadata_in_session_id() {
    // Arrange: both the header and body metadata carry a session id.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-claude-code-session-id",
        axum::http::HeaderValue::from_str("sid-from-header").unwrap(),
    );
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata"}
    });
    let req = anthropic_request(&headers, body);

    // Act
    let draft = build_usage_draft("anthropic", &req, "req-both".to_string());

    // Assert
    assert_eq!(draft.session_id.as_deref(), Some("sid-from-header"));
}

#[test]
fn mark_stream_http_committed_stamps_200_when_unset() {
    // Arrange: a fresh capture with no http_status yet.
    let (mut cap, _w, _dir) = capture();
    assert_eq!(cap.record.http_status, None);

    // Act
    cap.mark_stream_http_committed();

    // Assert: the committed SSE head records the client 200.
    assert_eq!(cap.record.http_status, Some(200));
}

#[test]
fn mark_stream_http_committed_is_idempotent() {
    // Arrange
    let (mut cap, _w, _dir) = capture();

    // Act: repeated over the stream lifetime (once per successful send).
    cap.mark_stream_http_committed();
    cap.mark_stream_http_committed();
    cap.mark_stream_http_committed();

    // Assert: still exactly 200, never disturbed by the repeats.
    assert_eq!(cap.record.http_status, Some(200));
}

#[test]
fn observe_error_preserves_committed_200() {
    // Arrange: the SSE head already committed to 200.
    let (mut cap, _w, _dir) = capture();
    cap.mark_stream_http_committed();

    // Act: a mid-stream upstream failure arrives after the head committed.
    cap.observe_error(&Error::upstream("p", 503, "mid-stream boom"));

    // Assert: the transport status stays 200 (the failure rides
    // outcome / error_class, not http_status), and the class is recorded.
    assert_eq!(
        cap.record.http_status,
        Some(200),
        "observe_error must not overwrite a committed 200"
    );
    assert!(
        cap.record.error_class.is_some(),
        "error_class still recorded"
    );
}

#[test]
fn observe_error_stamps_upstream_status_when_uncommitted() {
    // Arrange: no head committed yet (pre-head failure).
    let (mut cap, _w, _dir) = capture();
    assert_eq!(cap.record.http_status, None);

    // Act
    cap.observe_error(&Error::upstream("p", 529, "overloaded"));

    // Assert: with no committed head, the pre-head recorder stamps the
    // real upstream transport status.
    assert_eq!(cap.record.http_status, Some(529));
}

#[test]
fn observe_error_status_zero_sentinel_stays_none() {
    // Arrange: a status-0 upstream error is a local gate / timeout sentinel.
    let (mut cap, _w, _dir) = capture();

    // Act
    cap.observe_error(&Error::upstream("p", 0, "local timeout"));

    // Assert: no real HTTP code -> http_status stays None.
    assert_eq!(cap.record.http_status, None);
}

// -------- observe_error: gated resolved_class stamp -----------------------

#[test]
fn observe_error_stamps_kebab_class_when_dispatch_reached() {
    // Arrange: the request reached a dispatch attempt, so provider_kind is
    // set (as observe_meta would have set it).
    let (mut cap, _w, _dir) = capture();
    cap.record.provider_kind = Some("openai-compat".to_string());

    // Act: a classifiable upstream failure (429 -> RateLimited).
    cap.observe_error(&Error::upstream("p", 429, "rate limited"));

    // Assert: the canonical kebab token lands in resolved_class.
    assert_eq!(cap.record.resolved_class, Some("rate-limited".to_string()));
}

#[test]
fn observe_error_leaves_resolved_class_null_pre_dispatch() {
    // Arrange: a pre-dispatch failure -- provider_kind was never set because
    // no dispatch attempt was reached (validation / local gate).
    let (mut cap, _w, _dir) = capture();
    assert!(
        cap.record.provider_kind.is_none(),
        "sanity: no dispatch reached"
    );

    // Act
    cap.observe_error(&Error::Validation("bad body".into()));

    // Assert: no fake class is stamped; the row reads back "unclassified".
    assert!(
        cap.record.resolved_class.is_none(),
        "pre-dispatch failure must persist NULL resolved_class"
    );
}

#[test]
fn observe_error_stores_none_when_class_has_no_token() {
    // Arrange: dispatch reached, but the error classifies as Unknown (a
    // non-upstream, non-streaming variant), whose class_token is None.
    let (mut cap, _w, _dir) = capture();
    cap.record.provider_kind = Some("openai-compat".to_string());

    // Act
    cap.observe_error(&Error::Internal("boom".into()));

    // Assert: an unclassifiable failure stores NULL, not a fabricated token.
    assert!(
        cap.record.resolved_class.is_none(),
        "Unknown class (no token) must store NULL"
    );
}

#[tokio::test]
async fn disconnect_drop_emits_single_row_with_null_resolved_class() {
    // Arrange: a live capture that never finalizes explicitly.
    let (cap, handle, writer, dir) = capture_with_handle();
    let path = dir.path().join("usage.db");

    // Act: drop without finalize -- the Drop guard finalizes the abnormal
    // exit as ClientDisconnect. observe_error never ran, so resolved_class
    // was never stamped.
    drop(cap);

    // Assert: exactly one row, outcome client_disconnect, resolved_class NULL.
    assert!(wait_persisted(&handle, 1), "row not persisted");
    writer.shutdown();
    assert_eq!(
        handle.counters().persisted(),
        1,
        "disconnect Drop must emit exactly one row"
    );
    let conn = rusqlite::Connection::open(&path).expect("read open");
    let (outcome, resolved_class): (String, Option<String>) = conn
        .query_row("SELECT outcome, resolved_class FROM requests", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("row");
    assert_eq!(outcome, "client_disconnect");
    assert!(
        resolved_class.is_none(),
        "disconnect row must carry NULL resolved_class"
    );
}

/// Build a terminal chunk carrying an Anthropic unified-quota snapshot
/// in its `upstream_meta`, so `observe_chunk` -> `observe_quota` lifts it
/// into the QUOTA columns.
fn chunk_with_quota(quota: routectl_core::upstream_meta::AnthropicUnifiedQuota) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        upstream_meta: Some(
            routectl_core::upstream_meta::UpstreamMeta::from_anthropic_unified(quota),
        ),
        ..Default::default()
    }
}

#[test]
fn observe_quota_lifts_parseable_utilization_and_reset_into_numeric_columns() {
    // Arrange: a snapshot whose numeric fields are valid decimal/integer
    // strings. `AnthropicUnifiedQuota` is `#[non_exhaustive]`, so it is
    // built from a default and mutated rather than via a struct literal.
    let (mut cap, _w, _dir) = capture();
    let mut quota = routectl_core::upstream_meta::AnthropicUnifiedQuota::default();
    quota.status = Some("allowed".into());
    quota.overage_status = Some("disabled".into());
    quota.utilization = Some("0.21".into());
    quota.overage_utilization = Some("0.05".into());
    quota.representative_claim = Some("five_hour".into());
    quota.reset = Some("1900000000".into());
    let chunk = chunk_with_quota(quota);

    // Act
    cap.observe_chunk(&chunk);

    // Assert: string columns copied verbatim, numeric columns parsed.
    assert_eq!(cap.record.quota_claim.as_deref(), Some("five_hour"));
    assert_eq!(cap.record.quota_status.as_deref(), Some("allowed"));
    assert_eq!(cap.record.quota_overage_status.as_deref(), Some("disabled"));
    assert_eq!(cap.record.quota_utilization, Some(0.21_f64));
    assert_eq!(cap.record.quota_overage_utilization, Some(0.05_f64));
    assert_eq!(cap.record.quota_reset, Some(1_900_000_000_i64));
    assert!(cap.record.quota_extras.is_none());
}

#[test]
fn observe_quota_leaves_unparseable_utilization_none_without_failing_the_row() {
    // Arrange: garbage numeric strings must NOT poison the row -- the
    // string columns still land, the numeric ones stay None.
    let (mut cap, _w, _dir) = capture();
    let mut quota = routectl_core::upstream_meta::AnthropicUnifiedQuota::default();
    quota.status = Some("allowed".into());
    quota.utilization = Some("not-a-number".into());
    quota.reset = Some("soon".into());
    quota.representative_claim = Some("five_hour".into());
    let chunk = chunk_with_quota(quota);

    // Act
    cap.observe_chunk(&chunk);

    // Assert: the row survives; unparseable numerics degrade to None.
    assert_eq!(cap.record.quota_status.as_deref(), Some("allowed"));
    assert_eq!(cap.record.quota_claim.as_deref(), Some("five_hour"));
    assert_eq!(cap.record.quota_utilization, None);
    assert_eq!(cap.record.quota_reset, None);
}

#[test]
fn observe_quota_maps_non_empty_extras_into_json_object() {
    // Arrange: forward-compat `extras` pairs must land as a JSON object
    // keyed by suffix.
    let (mut cap, _w, _dir) = capture();
    let mut quota = routectl_core::upstream_meta::AnthropicUnifiedQuota::default();
    quota.representative_claim = Some("five_hour".into());
    quota.extras = vec![
        ("fallback-percentage".into(), "12".into()),
        ("7d-status".into(), "allowed".into()),
    ];
    let chunk = chunk_with_quota(quota);

    // Act
    cap.observe_chunk(&chunk);

    // Assert: extras become a JSON object with string values.
    assert_eq!(
        cap.record.quota_extras,
        Some(serde_json::json!({
            "fallback-percentage": "12",
            "7d-status": "allowed"
        }))
    );
}
