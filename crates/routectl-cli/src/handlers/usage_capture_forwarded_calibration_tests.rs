//! A dispatch served on a forwarded client credential is not admissible
//! calibration evidence: it writes neither a live sample nor a persisted pair
//! the boot rebuild could replay. Only own-credential traffic writes lane
//! evidence. An own-credential dispatch over the same rig records, and the
//! forwarded target's read side still finds that lane.
//!
//! Every meta here comes from a real dispatch through the router, so the
//! forwarded fact is the one dispatch stamps rather than a hand-set flag.

use super::*;
use routectl_core::{
    ChatChunk, ChatResponse, Choice, ForwardedBearer, Message, MessageContent, Provider, Role,
    Usage,
};
use routectl_router::config::CredentialSource;
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, Router};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
use std::collections::BTreeMap;
use std::sync::Arc;

const KIND: &str = "anthropic-api";
const OWN_UPSTREAM: &str = "client-model-0";

/// Answers every completion with a cache-inclusive prompt total 1.5x the
/// router's own estimate of the request it was sent, so each served dispatch
/// carries a usable, in-band evidence pair.
struct UsageEcho;

#[async_trait::async_trait]
impl Provider for UsageEcho {
    fn id(&self) -> &'static str {
        "usage-echo"
    }
    fn normalize_request(&self, _: &routectl_core::ChatRequest) -> routectl_core::Result<Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        Err(Error::normalize_response("usage-echo", "unused"))
    }
    async fn complete(
        &self,
        req: routectl_core::ChatRequest,
    ) -> routectl_core::Result<ChatResponse> {
        let prompt = routectl_router::estimate_total_tokens(&req) * 3 / 2;
        let prompt = u32::try_from(prompt).expect("fixture prompt fits u32");
        Ok(ChatResponse {
            model: req.model,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: text_message(Role::Assistant, "ok"),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: 1,
                total_tokens: prompt + 1,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<futures::stream::BoxStream<'static, routectl_core::Result<ChatChunk>>>
    {
        unreachable!("these tests dispatch non-streaming only")
    }
}

fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

/// Every unlisted model routes through `default` to `fwd-opus`, a forwarded
/// target whose configured upstream no client requests; `own` serves
/// [`OWN_UPSTREAM`] on an own credential of the same provider kind.
fn router() -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "fwd".into(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
    );
    config
        .providers
        .insert("own".into(), ProviderEntry::anthropic_api("literal:k"));
    config
        .aliases
        .insert("default".into(), AliasValue::Single("fwd-opus".into()));
    let provider: Arc<dyn Provider> = Arc::new(UsageEcho);
    let models: BTreeMap<String, Arc<ResolvedModel>> = [
        ResolvedModel::new("fwd-opus", "fwd", provider.clone(), "claude-config"),
        ResolvedModel::new("own", "own", provider, OWN_UPSTREAM),
    ]
    .into_iter()
    .map(|m| (m.nickname.clone(), Arc::new(m)))
    .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn request(model: &str) -> routectl_core::ChatRequest {
    let mut req = routectl_core::ChatRequest {
        model: model.to_string(),
        messages: vec![text_message(
            Role::User,
            &"forwarded-calibration-fixture ".repeat(400),
        )]
        .into(),
        ..Default::default()
    };
    req.routectl_internal.forwarded_bearer =
        Some(ForwardedBearer::new("sk-ant-oat01-test".to_string()));
    req
}

/// Serve `model` through `router`, run the capture's success path exactly as
/// the non-streaming handler does, and return the row's persisted evidence
/// pair alongside whether the dispatch was served on a forwarded credential.
async fn serve(
    router: &Router,
    model: &str,
    session_key: &str,
) -> (bool, (Option<u64>, Option<u64>)) {
    let req = request(model);
    let dispatched = router
        .complete_with_options(req.clone(), Default::default())
        .await;
    let resp = dispatched.result.expect("the stub upstream answers");
    let db_path = crate::test_usage_dir::usage_dir().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path, CHANNEL_CAPACITY, 0, true);
    let draft = build_usage_draft(
        "anthropic",
        &req,
        format!("req-{model}-{session_key}"),
        routectl_usage::Harness::Other,
    );
    let mut cap = UsageCapture::new(
        draft,
        handle,
        "ingress-1".to_string(),
        routectl_core::RequestObservation::new(),
    );
    cap.observe_meta(&dispatched.meta, 0, 0);
    cap.observe_response(&resp);
    cap.record_calibration_sample(&router.live_samples(), Some(session_key));
    cap.admit_evidence_pair(Outcome::Ok);
    let pair = (
        cap.record.calib_estimated_tokens,
        cap.record.calib_prompt_tokens,
    );
    cap.finalize(Outcome::Ok);
    drop(cap);
    writer.shutdown();
    (dispatched.meta.served_forwarded_credential, pair)
}

#[tokio::test]
async fn a_forwarded_dispatch_records_no_calibration_lane_for_any_requested_model() {
    // Arrange: many distinct client models, one of them equal to the upstream
    // an own-credential target serves, so a recorded sample would land
    // either in a new lane or in the operator's own lane.
    let router = router();
    assert!(
        router.calibration_lanes().is_empty(),
        "premise: no lane yet"
    );
    let models: Vec<String> = (0..48).map(|i| format!("client-model-{i}")).collect();
    assert!(models.iter().any(|m| m == OWN_UPSTREAM));

    // Act: three cohorts per model, enough to calibrate any lane it formed.
    let mut served = Vec::new();
    for model in &models {
        for cohort in ["caller-a", "caller-b", "caller-c"] {
            served.push(serve(&router, model, cohort).await);
        }
    }

    // Assert
    assert!(
        served.iter().all(|(forwarded, _)| *forwarded),
        "premise: every dispatch was served on the forwarded credential"
    );
    assert_eq!(router.calibration_lanes().len(), 0);
    assert_eq!(
        router.calibrated_estimate(KIND, OWN_UPSTREAM, 1_000),
        None,
        "the own-credential lane holds no forwarded evidence"
    );
    assert!(
        served.iter().all(|(_, pair)| *pair == (None, None)),
        "a forwarded row persists no pair the boot rebuild could replay"
    );
}

#[tokio::test]
async fn an_own_credential_dispatch_records_a_lane_the_forwarded_target_reads() {
    // Arrange
    let router = router();
    let forwarded_lane = router
        .opening_lane(OWN_UPSTREAM)
        .expect("the forwarded head resolves a lane");
    assert_eq!(
        (
            forwarded_lane.provider_kind,
            forwarded_lane.upstream_model.as_str()
        ),
        (KIND, OWN_UPSTREAM),
        "premise: the forwarded target reads the requested model's lane"
    );

    // Act
    let mut served = Vec::new();
    for i in 0..9 {
        served.push(serve(&router, "own", &format!("caller-{}", i % 3)).await);
    }

    // Assert
    assert!(served.iter().all(|(forwarded, _)| !*forwarded));
    assert!(
        served
            .iter()
            .all(|(_, (est, prompt))| est.is_some() && prompt.is_some()),
        "an own-credential row persists its pair"
    );
    assert_eq!(
        router.calibration_lanes(),
        vec![(KIND.to_string(), OWN_UPSTREAM.to_string())]
    );
    let corrected = router.calibrated_estimate(
        forwarded_lane.provider_kind,
        &forwarded_lane.upstream_model,
        1_000,
    );
    assert!(
        corrected.is_some_and(|tokens| tokens > 1_000),
        "the forwarded target reads the own lane's correction, got {corrected:?}"
    );
}
