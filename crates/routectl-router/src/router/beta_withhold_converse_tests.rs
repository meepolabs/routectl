//! A Bedrock Converse target whose upstream is provably not Anthropic
//! withholds every client beta: such a model refuses the `anthropic_beta`
//! bag key itself, so no flag can ride it. The operator floor still ships,
//! an upstream that proves nothing forwards as before, and the two refusal
//! envelopes teach no learner.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Result, Role,
    TokenCount, Usage,
};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use serde_json::{Value, json};

use super::super::{DispatchTarget, Router, RouterOptions};
use crate::beta_capability::beta_capability_key;
use crate::config::Config;
use crate::learned_capability::{EntryVerdict, ExportedEntry, ProbeClaim};
use crate::resolved::ResolvedModel;

const CLAUDE: &str = "us.anthropic.claude-opus-5-5";
const PROFILE_ARN: &str =
    "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abcd1234";

/// A client flag no tier but the Converse non-Claude arm speaks about.
const CLIENT_BETA: &str = "interleaved-thinking-2025-05-14";
/// A second client flag, so "every client beta" is more than one.
const OTHER_CLIENT_BETA: &str = "context-1m-2025-08-07";
/// The flag the operator pins on the Converse provider entry.
const FLOOR_BETA: &str = "context-management-2025-06-27";

const CONVERSE_BLOCK: &str = "kind = \"bedrock\"\nregion = \"us-east-1\"\n\
                              creds = { kind = \"default-chain\" }\napi_shape = \"converse\"\n";

/// One Converse entry `cv` (floor pinned), one Converse entry `cvbare` (no
/// floor), one Invoke entry `inv` and one own-credential anthropic-api
/// entry `api`, with the learning subsystem on.
fn router() -> Router {
    let body = format!(
        "version = 3\n\
         [providers.cv]\n{CONVERSE_BLOCK}anthropic_beta = [\"{FLOOR_BETA}\"]\n\
         [providers.cvbare]\n{CONVERSE_BLOCK}\
         [providers.inv]\n\
         kind = \"bedrock\"\nregion = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\napi_shape = \"invoke\"\n\
         [providers.api]\n\
         kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n\
         [capability]\nenabled = true\n\
         [aliases]\ndefault = \"m\"\n"
    );
    let config: Config = toml::from_str(&body).expect("fixture config parses");
    Router::new(Arc::new(config))
}

/// Records every request a dispatch hands it; answers each with `reject`
/// when set, else a success.
struct RecordingProvider {
    seen: Mutex<Vec<ChatRequest>>,
    reject: Option<(String, String)>,
    calls: AtomicUsize,
}

impl RecordingProvider {
    fn new(reject: Option<(String, String)>) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            reject,
            calls: AtomicUsize::new(0),
        })
    }

    fn answer(&self, req: ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().push(req);
        match &self.reject {
            Some((body, lifted_type)) => Err(Error::upstream_full(
                "cv",
                400,
                body.clone(),
                None,
                Some(lifted_type.clone()),
                None,
            )),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("recording", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(req).map(|()| ChatResponse {
            model: "wire".to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("recording", 500, "unused"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        Err(Error::upstream("recording", 500, "unused"))
    }
}

fn target(router: &Router, provider_name: &str, upstream: &str) -> DispatchTarget {
    let provider: Arc<dyn Provider> = RecordingProvider::new(None);
    let model = ResolvedModel::new("m", provider_name, provider, upstream);
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("a non-seat model expands to one target")
}

fn request(flags: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(16),
        anthropic_beta: flags.iter().map(ToString::to_string).collect(),
        ..Default::default()
    }
}

fn withheld(router: &Router, target: &DispatchTarget, flags: &[&str]) -> Vec<String> {
    let mut admissions = Vec::new();
    let out = router.withheld_betas_for_target(
        target,
        &request(flags),
        ProbeClaim::Claim,
        &mut admissions,
        Instant::now(),
    );
    assert!(admissions.is_empty(), "no fixture here lapses a negative");
    out
}

fn owned(flags: &[&str]) -> Vec<String> {
    flags.iter().map(ToString::to_string).collect()
}

#[test]
fn every_non_claude_converse_id_that_refused_the_bag_key_withholds_every_client_beta() {
    // Arrange: one row per upstream id whose live Converse turn drew
    // `Unknown parameter: 'anthropic_beta'` (GPT) or
    // `extraneous key [anthropic_beta]` (Nova, Llama).
    let router = router();
    let ids = [
        "us.openai.gpt-6.1-sol",
        "us.openai.gpt-6-astra",
        "us.openai.gpt-6-sol",
        "us.openai.gpt-6-luna",
        "us.openai.gpt-5.6-sol",
        "us.openai.gpt-5.6-terra",
        "us.openai.gpt-5.6-luna",
        "us.amazon.nova-2-lite-v1:0",
        "us.meta.llama4-maverick-17b-instruct-v1:0",
        "us.meta.llama4-scout-17b-instruct-v1:0",
        "us.meta.llama3-3-70b-instruct-v1:0",
        "meta.llama3-1-70b-instruct-v1:0",
        "meta.llama3-1-8b-instruct-v1:0",
        "meta.llama3-70b-instruct-v1:0",
        "meta.llama3-8b-instruct-v1:0",
    ];

    for id in ids {
        // Act
        let out = withheld(
            &router,
            &target(&router, "cvbare", id),
            &[CLIENT_BETA, OTHER_CLIENT_BETA],
        );

        // Assert
        assert_eq!(
            routectl_core::anthropic_family::anthropic_family(id),
            routectl_core::anthropic_family::AnthropicFamily::No,
            "row {id}: classification",
        );
        assert_eq!(out, owned(&[CLIENT_BETA, OTHER_CLIENT_BETA]), "row {id}");
    }
}

#[test]
fn a_claude_id_a_profile_arn_and_other_egresses_withhold_nothing_new() {
    // Arrange: rows the arm must not reach. The non-Claude Invoke row is one
    // config validation refuses at load; it is built directly here.
    let router = router();
    let rows = [
        ("claude id on converse", "cvbare", CLAUDE),
        ("inference-profile arn on converse", "cvbare", PROFILE_ARN),
        (
            "non-claude id on invoke",
            "inv",
            "us.amazon.nova-2-lite-v1:0",
        ),
        (
            "non-claude id on anthropic-api",
            "api",
            "us.openai.gpt-6-sol",
        ),
    ];

    for (name, provider_name, upstream) in rows {
        // Act
        let out = withheld(
            &router,
            &target(&router, provider_name, upstream),
            &[CLIENT_BETA, OTHER_CLIENT_BETA],
        );

        // Assert
        assert!(out.is_empty(), "row {name}: withheld {out:?}");
    }
}

#[test]
fn the_operator_floor_is_not_withheld_on_a_non_claude_converse_target() {
    // Arrange
    let router = router();
    let target = target(&router, "cv", "us.amazon.nova-2-lite-v1:0");

    // Act: the client also sends the pinned flag.
    let out = withheld(&router, &target, &[CLIENT_BETA, FLOOR_BETA]);

    // Assert
    assert_eq!(out, owned(&[CLIENT_BETA]));
}

#[test]
fn a_lapsed_negative_on_a_non_claude_converse_target_claims_no_re_probe_slot() {
    // Arrange: a learned negative whose decay has lapsed would claim its
    // single re-probe slot on any lane that reads it.
    let router = router();
    let target = target(&router, "cvbare", "us.openai.gpt-6-sol");
    let base = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: "bedrock".into(),
            state_key: target
                .learned_key("")
                .expect("the fixture target has a lane")
                .to_string(),
            feature_key: beta_capability_key(CLIENT_BETA).expect("well-formed flag"),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base,
            evidence_class: None,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);
    let mut admissions = Vec::new();

    // Act
    let out = router.withheld_betas_for_target(
        &target,
        &request(&[CLIENT_BETA]),
        ProbeClaim::Claim,
        &mut admissions,
        Instant::now(),
    );

    // Assert
    assert!(
        admissions.is_empty(),
        "a withheld flag claimed a probe slot"
    );
    assert_eq!(out, owned(&[CLIENT_BETA]));
}

fn bearer() -> String {
    ["converse", "beta", "test", "key"].join("-")
}

/// The Converse bag a real Bedrock egress assembles from `req` for `model_id`
/// with the operator floor `floor`.
fn converse_bag(req: &ChatRequest, model_id: &str, floor: &[&str]) -> Value {
    let cfg = BedrockConfig {
        id: "bedrock:cv".into(),
        region: "us-east-1".into(),
        model_id: model_id.into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: bearer() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: owned(floor),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let provider = BedrockProvider::new(cfg, ResolvedCreds::Bearer { key: bearer() })
        .expect("canonical region");
    let body = provider.normalize_request(req).expect("converse normalize");
    body.get("additionalModelRequestFields")
        .cloned()
        .unwrap_or(Value::Null)
}

/// The attempt request the router hands the `provider_name` lane for
/// `upstream`, carrying `flags`.
async fn routed_attempt(provider_name: &str, upstream: &str, flags: &[&str]) -> ChatRequest {
    let mut router = router();
    let provider = RecordingProvider::new(None);
    let model = ResolvedModel::new("m", provider_name, provider.clone(), upstream);
    router.install_resolved_models(BTreeMap::from([("m".to_string(), Arc::new(model))]));

    let dispatched =
        Box::pin(router.complete_with_options(request(flags), RouterOptions::default())).await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let seen = provider.seen.lock();
    assert_eq!(seen.len(), 1, "one attempt");
    seen[0].clone()
}

#[tokio::test]
async fn a_routed_non_claude_converse_bag_with_only_client_betas_carries_no_beta_key() {
    // Arrange
    let upstream = "us.amazon.nova-2-lite-v1:0";
    let attempt = routed_attempt("cvbare", upstream, &[CLIENT_BETA, OTHER_CLIENT_BETA]).await;

    // Act
    let bag = converse_bag(&attempt, upstream, &[]);

    // Assert
    assert_eq!(
        attempt.routectl_internal.withheld_betas.to_vec(),
        owned(&[CLIENT_BETA, OTHER_CLIENT_BETA]),
    );
    assert!(
        bag.get("anthropic_beta").is_none(),
        "the bag carries the refused key: {bag}"
    );
}

#[tokio::test]
async fn a_routed_non_claude_converse_bag_still_ships_the_operator_floor() {
    // Arrange
    let upstream = "us.openai.gpt-6-sol";
    let attempt = routed_attempt("cv", upstream, &[CLIENT_BETA]).await;

    // Act
    let bag = converse_bag(&attempt, upstream, &[FLOOR_BETA]);

    // Assert
    assert_eq!(bag["anthropic_beta"], json!([FLOOR_BETA]));
}

#[tokio::test]
async fn a_routed_profile_arn_converse_bag_forwards_client_betas() {
    // Arrange
    let attempt = routed_attempt("cvbare", PROFILE_ARN, &[CLIENT_BETA]).await;

    // Act
    let bag = converse_bag(&attempt, PROFILE_ARN, &[]);

    // Assert
    assert_eq!(bag["anthropic_beta"], json!([CLIENT_BETA]));
}

/// `(status, lifted_type, body)` of a committed bedrock rejection fixture.
fn rejection(case: &str) -> (u16, String, String) {
    let file = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rejections/bedrock")
        .join(format!("{case}.json"));
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(file).expect("fixture readable"))
            .expect("fixture is JSON");
    (
        u16::try_from(fixture["status"].as_u64().expect("status")).expect("u16 status"),
        fixture["lifted_type"]
            .as_str()
            .expect("lifted_type")
            .to_string(),
        fixture["body"].as_str().expect("body").to_string(),
    )
}

const BAG_KEY_REFUSALS: [&str; 2] = [
    "beta-bag-key-unknown-parameter-converse",
    "beta-bag-key-extraneous-converse",
];

#[test]
fn a_bag_key_refusal_names_no_beta_flag() {
    for case in BAG_KEY_REFUSALS {
        // Arrange
        let (status, lifted_type, body) = rejection(case);
        let body = routectl_core::UpstreamBody::new(&body);

        // Act
        let named = super::super::rejection_parser::rejected_beta_flags(
            "bedrock",
            status,
            Some(&lifted_type),
            body.parsed(),
        );

        // Assert
        assert_eq!(named, None, "row {case}");
    }
}

#[tokio::test]
async fn a_bag_key_refusal_neither_mints_nor_strips() {
    for case in BAG_KEY_REFUSALS {
        // Arrange: an upstream that proves nothing, so the client flag rides
        // and the refusal reaches the learners.
        let (_, lifted_type, body) = rejection(case);
        let mut router = router();
        let provider = RecordingProvider::new(Some((body, lifted_type)));
        let model = ResolvedModel::new("m", "cvbare", provider.clone(), PROFILE_ARN);
        router.install_resolved_models(BTreeMap::from([("m".to_string(), Arc::new(model))]));

        // Act: twice, so a learned verdict would act on the second.
        let first = Box::pin(
            router.complete_with_options(request(&[CLIENT_BETA]), RouterOptions::default()),
        )
        .await;
        let second = Box::pin(
            router.complete_with_options(request(&[CLIENT_BETA]), RouterOptions::default()),
        )
        .await;

        // Assert
        assert!(first.result.is_err(), "row {case}: the refusal surfaces");
        assert!(second.result.is_err(), "row {case}");
        assert!(
            first.meta.learned_capabilities.is_empty()
                && second.meta.learned_capabilities.is_empty(),
            "row {case}: a learn event fired",
        );
        assert!(
            router.learned_capabilities.is_empty(),
            "row {case}: a verdict was minted",
        );
        assert_eq!(router.metrics.strip_total(), 0, "row {case}: a strip fired");
        assert_eq!(
            router.metrics.bedrock_validation_unmatched_total(),
            2,
            "row {case}: each refusal reached the Bedrock validation reader",
        );
        let seen = provider.seen.lock();
        assert_eq!(seen.len(), 2, "row {case}: no retry, no route-away");
        for attempt in seen.iter() {
            assert!(
                attempt.routectl_internal.withheld_betas.is_empty(),
                "row {case}: the flag was withheld after the refusal",
            );
            assert_eq!(attempt.anthropic_beta, owned(&[CLIENT_BETA]), "row {case}");
        }
    }
}
