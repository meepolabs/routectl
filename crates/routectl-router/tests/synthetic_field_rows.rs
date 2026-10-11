//! The synthetic rewrite and re-check rows reach a Router only when a test asks
//! for them.
//!
//! An integration test is a separate crate: it is built with this crate's
//! `test-utils` feature on and `cfg(test)` off, which is the same shape as any
//! build that enables the feature (a dependent crate's tests, or a release built
//! with `--all-features`). So this is the build in which a feature-gated table
//! would leak the synthetic rows into a Router that never opted in.
//!
//! Asserted on the bytes the seat receives: a rejection naming the synthetic
//! rewrite row's accepted value either produces a rewritten retry or it does not.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Result, Role,
    Usage,
};
use routectl_router::{
    AliasValue, Config, ModelEntry, ResolvedModel, Router, TEST_REWRITE_ACCEPTED,
    TEST_REWRITE_KWARG, TEST_REWRITE_PATH,
};

/// A remote-looking base URL, so the attributability gate admits the lane.
/// Nothing is sent here: the seat answers in process.
const REMOTE_BASE: &str = "https://api.anthropic.com";
const ALIAS: &str = "synthetic-rows-alias";
const NICKNAME: &str = "sonnet";
/// A value outside the rewrite row's accepted set, so the seat refuses it.
const CLIENT_VALUE: &str = "routectl-test-client-value";

/// Refuses any request carrying [`CLIENT_VALUE`] with a rejection naming the
/// rewrite row and one accepted member; records every request it sees.
#[derive(Default)]
struct Seat {
    seen: parking_lot::Mutex<Vec<ChatRequest>>,
}

impl Seat {
    fn kwarg_per_attempt(&self) -> Vec<Option<String>> {
        self.seen.lock().iter().map(kwarg_of).collect()
    }
}

fn kwarg_of(req: &ChatRequest) -> Option<String> {
    req.chat_template_kwargs
        .as_ref()
        .and_then(|kwargs| kwargs.get(TEST_REWRITE_KWARG))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn rewrite_rejection() -> Error {
    let message = format!(
        "{TEST_REWRITE_PATH}: Input should be '{}'",
        TEST_REWRITE_ACCEPTED[0]
    );
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message},
    });
    Error::upstream("p0", 400, body.to_string())
}

#[async_trait::async_trait]
impl Provider for Seat {
    fn id(&self) -> &'static str {
        "p0"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p0", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let refused = kwarg_of(&req).as_deref() == Some(CLIENT_VALUE);
        self.seen.lock().push(req);
        if refused {
            return Err(rewrite_rejection());
        }
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            usage: Some(Usage::default()),
            choices: vec![routectl_core::Choice {
                index: 0,
                message: text(Role::Assistant, "ok"),
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

fn text(role: Role, body: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(body.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn config() -> Config {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"{REMOTE_BASE}\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.models.insert(
        NICKNAME.to_string(),
        ModelEntry::new("p0", "claude-sonnet-4-5"),
    );
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()));
    config
}

/// A Router over `config()` resolving to `seat`, built as production builds
/// one: no table seam.
fn production_router(seat: Arc<Seat>) -> Router {
    install(Router::new(Arc::new(config())), seat)
}

fn install(mut router: Router, seat: Arc<Seat>) -> Router {
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
    router.with_capability_writes_assumed_durable_for_tests()
}

fn request_carrying_the_rewrite_field() -> ChatRequest {
    ChatRequest {
        model: ALIAS.to_string(),
        messages: vec![text(Role::User, "hello")].into(),
        chat_template_kwargs: Some(serde_json::json!({ TEST_REWRITE_KWARG: CLIENT_VALUE })),
        ..Default::default()
    }
}

/// A Router built without the seam scans the production table alone, so a
/// rejection naming the synthetic rewrite row repairs nothing: one attempt, the
/// client's value untouched, and the rejection reaches the caller.
///
/// Mutation check: make the non-unit-test `FieldRepairTable::default` return
/// `with_synthetic_rows()` under `test-utils` -> red, the seat sees a second,
/// rewritten attempt.
#[tokio::test]
async fn a_test_utils_router_without_the_seam_has_no_synthetic_rows() {
    let seat = Arc::new(Seat::default());
    let router = production_router(Arc::clone(&seat));

    let served = router.complete(request_carrying_the_rewrite_field()).await;

    assert!(served.is_err(), "nothing repaired the refused value");
    assert_eq!(
        seat.kwarg_per_attempt(),
        vec![Some(CLIENT_VALUE.to_string())],
        "one attempt, carrying the client's own value",
    );
    assert_eq!(router.field_repair_counters().repair_succeeded, 0);
}

/// The positive control: the same Router with the seam installed carries the
/// rewrite row, so the same rejection produces one rewritten retry. Without
/// this the test above would pass against a build whose rewrite arm never fired.
#[tokio::test]
async fn the_seam_installs_the_synthetic_rows() {
    let seat = Arc::new(Seat::default());
    let router = install(
        Router::new(Arc::new(config())).with_synthetic_field_rows_for_tests(),
        Arc::clone(&seat),
    );

    let served = router.complete(request_carrying_the_rewrite_field()).await;

    assert!(served.is_ok(), "the rewritten retry serves: {served:?}");
    assert_eq!(
        seat.kwarg_per_attempt(),
        vec![
            Some(CLIENT_VALUE.to_string()),
            Some(TEST_REWRITE_ACCEPTED[0].to_string()),
        ],
        "the refused attempt, then a retry carrying the named accepted value",
    );
}
