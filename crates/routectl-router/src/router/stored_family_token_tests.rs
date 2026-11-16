//! The family tokens routectl stores per lane: the provider kind the usage
//! ledger persists for a served target, the calibration lane key, and the
//! (kind, egress base) pair the price lookup selects a table provider from.
//! Stored rows outlive any config schema, so these tokens stay fixed while
//! the provider-entry shape changes: each row is one provider entry written
//! in TOML, and the expected tokens are literals, never read back from the
//! code that produces them.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use futures::stream::{self, BoxStream, StreamExt};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Message, MessageContent, Provider, Result, Role,
};

use super::scripted_provider_test_support::{content_chunk, ok_response};
use super::{Router, RouterOptions};
use crate::config::Config;
use crate::resolved::ResolvedModel;

const PROVIDER: &str = "p";
const NICKNAME: &str = "fixture-nickname";
const UPSTREAM: &str = "fixture-upstream";

// ---------------------------------------------------------------- fixtures
// Each fixture is one `[providers.p]` entry. A config schema change edits
// these blocks only; every assertion below stays as written.

const ANTHROPIC_DIRECT: &str = r#"
[providers.p]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
"#;

const ANTHROPIC_MANTLE: &str = r#"
[providers.p]
format = "anthropic-messages"
cloud = { name = "aws", region = "us-east-1" }
credential = "env://AWS_BEARER_TOKEN_BEDROCK"
auth = "bearer"
"#;

const COMPAT_DIRECT: &str = r#"
[providers.p]
format = "openai-chat"
base_url = "https://openrouter.ai/api/v1"
credential = "env://OPENROUTER_API_KEY"
auth = "bearer"
"#;

const COMPAT_MANTLE: &str = r#"
[providers.p]
format = "openai-chat"
cloud = { name = "aws", region = "us-east-1" }
credential = "env://AWS_BEARER_TOKEN_BEDROCK"
auth = "bearer"
"#;

const RESPONSES_API_KEY: &str = r#"
[providers.p]
format = "openai-responses"
base_url = "https://api.openai.com/v1"
credential = "env://OPENAI_API_KEY"
auth = "bearer"
"#;

const RESPONSES_MANTLE: &str = r#"
[providers.p]
format = "openai-responses"
cloud = { name = "aws", region = "us-east-1" }
credential = "env://AWS_BEARER_TOKEN_BEDROCK"
auth = "bearer"
"#;

const BEDROCK_INVOKE: &str = r#"
[providers.p]
format = "bedrock-invoke"
cloud = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth = "aws-sigv4"
"#;

const BEDROCK_CONVERSE: &str = r#"
[providers.p]
format = "bedrock-converse"
cloud = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth = "aws-sigv4"
"#;

const GEMINI_API_KEY: &str = r#"
[providers.p]
format = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
credential = "env://GEMINI_API_KEY"
auth = "x-goog-api-key"
"#;

const GEMINI_CLOUD_CODE: &str = r#"
[providers.p]
format = "gemini-cloud-code"
base_url = "https://daily-cloudcode-pa.googleapis.com"
credential = "oauth://antigravity"
auth = "bearer"
"#;

// ------------------------------------------------------------- expectations

struct Row {
    lane: &'static str,
    fixture: &'static str,
    /// Provider-kind token the ledger, the calibration lane and the price
    /// lookup all carry.
    family: &'static str,
    /// Egress base the price lookup matches table `api_host`s against.
    price_base: &'static str,
}

const ROWS: [Row; 10] = [
    Row {
        lane: "anthropic direct",
        fixture: ANTHROPIC_DIRECT,
        family: "anthropic-api",
        price_base: "https://api.anthropic.com",
    },
    Row {
        lane: "anthropic mantle",
        fixture: ANTHROPIC_MANTLE,
        family: "anthropic-api",
        price_base: "https://bedrock-mantle.us-east-1.api.aws/anthropic",
    },
    Row {
        lane: "openai-compat direct",
        fixture: COMPAT_DIRECT,
        family: "openai-compat",
        price_base: "https://openrouter.ai/api/v1",
    },
    Row {
        lane: "openai-compat mantle",
        fixture: COMPAT_MANTLE,
        family: "openai-compat",
        price_base: "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
    },
    Row {
        lane: "responses api-key",
        fixture: RESPONSES_API_KEY,
        family: "openai-responses",
        price_base: "https://api.openai.com/v1",
    },
    Row {
        lane: "responses mantle",
        fixture: RESPONSES_MANTLE,
        family: "openai-responses",
        price_base: "https://bedrock-mantle.us-east-1.api.aws/openai/v1",
    },
    Row {
        lane: "bedrock invoke",
        fixture: BEDROCK_INVOKE,
        family: "bedrock",
        price_base: "https://bedrock-runtime.us-west-2.amazonaws.com",
    },
    Row {
        lane: "bedrock converse",
        fixture: BEDROCK_CONVERSE,
        family: "bedrock",
        price_base: "https://bedrock-runtime.us-west-2.amazonaws.com",
    },
    Row {
        lane: "gemini api-key",
        fixture: GEMINI_API_KEY,
        family: "gemini",
        price_base: "https://generativelanguage.googleapis.com/v1beta",
    },
    Row {
        lane: "gemini cloud-code",
        fixture: GEMINI_CLOUD_CODE,
        family: "gemini",
        price_base: "https://daily-cloudcode-pa.googleapis.com",
    },
];

// ------------------------------------------------------------------ harness

struct OkProvider;

#[async_trait::async_trait]
impl Provider for OkProvider {
    fn id(&self) -> &'static str {
        "ok"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(routectl_core::Error::normalize_response("ok", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ok_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Ok(stream::iter([Ok(content_chunk())]).boxed())
    }
}

fn router_for(fixture: &str, lane: &str) -> Router {
    let config: Config =
        toml::from_str(fixture).unwrap_or_else(|e| panic!("{lane}: fixture parses: {e}"));
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new(NICKNAME, PROVIDER, Arc::new(OkProvider), UPSTREAM);
    router.install_resolved_models(BTreeMap::from([(NICKNAME.to_string(), Arc::new(model))]));
    router
}

fn request() -> ChatRequest {
    ChatRequest {
        model: NICKNAME.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        ..Default::default()
    }
}

/// The (kind, egress base) pair `PriceTable::price_for` derives from the
/// served provider's entry before selecting a table provider.
fn price_lookup_key(router: &Router, served_provider: &str) -> (&'static str, Option<String>) {
    let (_, entry) = router
        .config
        .provider_entry_for_target(served_provider)
        .expect("served provider has an entry");
    (
        entry.kind_str(),
        entry
            .classifiable_egress_base_url()
            .map(std::borrow::Cow::into_owned),
    )
}

/// What one served dispatch stores for a lane.
#[derive(Debug, PartialEq, Eq)]
struct Stored {
    ledger_kind: Option<String>,
    calibration_lanes: Vec<(String, String)>,
    opening_lane_kind: Option<&'static str>,
    price_lookup: (&'static str, Option<String>),
}

async fn dispatch_and_store(row: &Row) -> Stored {
    let router = router_for(row.fixture, row.lane);
    let dispatched = router
        .complete_with_options(request(), RouterOptions::default())
        .await;
    dispatched
        .result
        .unwrap_or_else(|e| panic!("{}: dispatch serves: {e}", row.lane));
    let meta = dispatched.meta;
    for i in 0..9 {
        router.record_calibration_sample(
            meta.served_provider_kind.as_deref(),
            meta.served_upstream.as_deref(),
            Some(&format!("caller-{}", i % 3)),
            1_000,
            2_000,
            SystemTime::now(),
        );
    }
    let served_provider = meta
        .served_provider
        .clone()
        .unwrap_or_else(|| panic!("{}: a provider served", row.lane));
    Stored {
        ledger_kind: meta.served_provider_kind.clone(),
        calibration_lanes: router.calibration_lanes(),
        opening_lane_kind: router.opening_lane(NICKNAME).map(|l| l.provider_kind),
        price_lookup: price_lookup_key(&router, &served_provider),
    }
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn each_lane_stores_its_family_token_unchanged() {
    assert_eq!(ROWS.len(), 10, "one row per lane");
    let mut mismatches = Vec::new();
    for row in &ROWS {
        // Act
        let stored = dispatch_and_store(row).await;

        // Assert
        let expected = Stored {
            ledger_kind: Some(row.family.to_string()),
            calibration_lanes: vec![(row.family.to_string(), UPSTREAM.to_string())],
            opening_lane_kind: Some(row.family),
            price_lookup: (row.family, Some(row.price_base.to_string())),
        };
        if stored != expected {
            mismatches.push(format!(
                "{}:\n  stored:   {stored:?}\n  expected: {expected:?}",
                row.lane
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
