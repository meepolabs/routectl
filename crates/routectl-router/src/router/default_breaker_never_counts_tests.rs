//! Under the shipped breaker default (no tier sets `circuit_failures`), the
//! failures that are not upstream health never reach the counter: an auth
//! or bad-request rejection, a rejection a learning repair retries, and an
//! auth or bad-request error frame after a stream committed. A 503 before
//! and after commit debits on the same harness, as the positive control.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
};
use serde_json::{Value, json};

use super::field_repair::{TEST_UNSUPPORTED_PATH, TEST_UNSUPPORTED_SENTINEL};
use super::scripted_provider_test_support::{Answer, ScriptedProvider};
use crate::config::{Config, TuningSource, resolve_tuning};
use crate::resolved::ResolvedModel;
use crate::router::{Router, RouterOptions};

const ALIAS: &str = "a";
const NICKNAME: &str = "m0";
const PROVIDER: &str = "p0";

fn config() -> Config {
    let toml_text = format!(
        "version = 5\n[retry]\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [wire_check]\nfirst_n = 0\nsample_one_in = 1000\n\
         [providers.{PROVIDER}]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n\
         [aliases]\n{ALIAS} = [\"{NICKNAME}\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    assert_eq!(
        resolve_tuning(&config, Some(PROVIDER), None)
            .circuit_failures
            .source,
        TuningSource::ShippedDefault,
        "premise: the breaker runs on the shipped default"
    );
    config
}

fn router(provider: Arc<dyn Provider>) -> Router {
    let mut router = Router::new(Arc::new(config()));
    let model = ResolvedModel::new(NICKNAME, PROVIDER, provider, "upstream");
    router.install_resolved_models(BTreeMap::from([(NICKNAME.to_string(), Arc::new(model))]));
    router
}

fn failures(router: &Router) -> u32 {
    router.state[NICKNAME]
        .lock()
        .consecutive_failures_for_tests()
}

/// A request carrying the test-only refused-field row's surface and the
/// client body a learning repair reads.
fn request() -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        max_tokens: Some(16),
        user: Some(TEST_UNSUPPORTED_SENTINEL.to_string()),
        ..Default::default()
    };
    let body = json!({
        "model": ALIAS,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&body).expect("serialize").into(),
    });
    req
}

fn carries_test_row(req: &ChatRequest) -> bool {
    req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL)
}

/// Deposit the test row's wire field while the request still carries it, as
/// an egress emitting it would; the repaired retry drops it.
fn with_test_row_field(req: &ChatRequest, mut client: Value) -> Value {
    if carries_test_row(req) {
        client[TEST_UNSUPPORTED_PATH] = json!(true);
    }
    client
}

fn status(code: u16) -> Error {
    Error::upstream("scripted", code, "upstream said no")
}

/// The anthropic-api 400 naming the test row, which a learning repair drops.
fn field_rejection() -> Error {
    let body = json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("{TEST_UNSUPPORTED_PATH}: Extra inputs are not permitted"),
        },
    });
    Error::upstream("scripted", 400, body.to_string())
}

struct CompleteRow {
    name: &'static str,
    answer: Answer,
    calls: usize,
    debits: u32,
}

#[tokio::test]
async fn completion_failures_debit_only_health_classes() {
    let rows = [
        // A 401 takes the one credential-recovery retry; neither debits.
        CompleteRow {
            name: "auth 401",
            answer: |_| Err(status(401)),
            calls: 2,
            debits: 0,
        },
        CompleteRow {
            name: "bad-request 400",
            answer: |_| Err(status(400)),
            calls: 1,
            debits: 0,
        },
        // The repaired retry ends in a plain 400, not a success, so a debit
        // taken for the repaired rejection would still be on the counter.
        CompleteRow {
            name: "learning repair retry",
            answer: |req| {
                if carries_test_row(req) {
                    return Err(field_rejection());
                }
                Err(status(400))
            },
            calls: 2,
            debits: 0,
        },
        CompleteRow {
            name: "503 (control): every attempt debits",
            answer: |_| Err(status(503)),
            calls: 3,
            debits: 3,
        },
    ];
    for row in rows {
        // Arrange
        let provider = ScriptedProvider::new(Some(with_test_row_field), row.answer);
        let router = router(Arc::clone(&provider) as Arc<dyn Provider>);

        // Act
        let out = Box::pin(router.complete_with_options(request(), RouterOptions::new())).await;

        // Assert
        let name = row.name;
        assert!(out.result.is_err(), "{name}: premise: the request fails");
        assert_eq!(provider.calls(), row.calls, "{name}: upstream calls");
        assert_eq!(failures(&router), row.debits, "{name}: breaker debits");
    }
}

type FrameFn = fn() -> Error;

/// A stream that commits one content chunk, then ends on `frame`.
struct CommittedFrameProvider {
    frame: FrameFn,
    calls: AtomicUsize,
}

fn content() -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Provider for CommittedFrameProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("only the stream surface is driven")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(stream::iter([Ok(content()), Err((self.frame)())]).boxed())
    }
}

#[tokio::test]
async fn committed_error_frames_debit_only_health_classes() {
    let rows: [(&str, FrameFn, u32); 3] = [
        ("auth 401 frame", || status(401), 0),
        ("bad-request 400 frame", || status(400), 0),
        ("503 frame (control)", || status(503), 1),
    ];
    for (name, frame, debits) in rows {
        // Arrange
        let provider = Arc::new(CommittedFrameProvider {
            frame,
            calls: AtomicUsize::new(0),
        });
        let router = router(Arc::clone(&provider) as Arc<dyn Provider>);

        // Act
        let mut stream = router
            .stream(request())
            .await
            .unwrap_or_else(|e| panic!("{name}: premise: the stream commits: {e}"));
        let mut saw_frame = false;
        while let Some(item) = stream.next().await {
            saw_frame |= item.is_err();
        }
        drop(stream);

        // Assert
        assert!(saw_frame, "{name}: premise: the frame reached the client");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "{name}: calls");
        assert_eq!(failures(&router), debits, "{name}: breaker debits");
    }
}
