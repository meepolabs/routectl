//! The learned lane a dispatch target carries: minted once at chain expansion
//! from the egressing provider entry and the upstream, shared by every
//! nickname on that endpoint, and kept apart from the runtime breaker key.

use super::*;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::stream::BoxStream;
use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatChunk, ChatResponse, Provider, Result, ToolDef};
use serde_json::json;

use crate::learned_capability::RoutingDecision;
use crate::resolved::ResolvedModel;
use crate::seat_pool::SeatTarget;

struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("stub", 500, "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("stub", 500, "unused"))
    }
}

const PROVIDERS: &str = r#"
[providers.alpha]
kind = "openai-compat"
base_url = "https://alpha.example.test/v1"
api_key_ref = "literal:k"

[providers.beta]
kind = "openai-compat"
base_url = "https://beta.example.test/v1"
api_key_ref = "literal:k"
"#;
const KIND: &str = "openai-compat";

fn router() -> Router {
    Router::new(Arc::new(
        toml::from_str(PROVIDERS).expect("valid test toml"),
    ))
}

fn target(router: &Router, nickname: &str, provider: &str, upstream: &str) -> DispatchTarget {
    let p: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = ResolvedModel::new(nickname, provider, p, upstream);
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

fn web_search_request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({ "type": "web_search" }))]),
        ..Default::default()
    }
}

/// Mint a self-identifying web_search negative through the production learn
/// path on `target`.
fn learn_on(router: &Router, target: &DispatchTarget) {
    let err = Error::upstream_full("stub", 400, "{}".to_string(), None, None, None);
    router.commit_learned_observation(
        (
            "web_search".to_string(),
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
        ),
        &FailureClass::BadRequest,
        &err,
        400,
        None,
        KIND,
        target,
        &web_search_request(),
        false,
        &mut HashSet::new(),
        &mut DispatchMeta::for_alias("m"),
        &mut LearnedProbeGuard::inert(),
    );
}

/// Whether the production lookup on `target` reads an acting web_search
/// negative.
fn routes_away(router: &Router, target: &DispatchTarget) -> bool {
    let key = target
        .learned_key("web_search")
        .expect("the target has a lane");
    matches!(
        router
            .acting_negative_with_generation(key, "web_search", KIND, Instant::now())
            .0,
        RoutingDecision::RouteAway { .. }
    )
}

#[test]
fn a_target_lane_is_its_provider_entry_and_upstream() {
    let router = router();

    let t = target(&router, "opus", "alpha", "vendor/model#v2");

    let lane = t.learned_lane.as_ref().expect("a lane is minted");
    assert_eq!(lane.provider_entry(), "alpha");
    assert_eq!(lane.upstream(), "vendor/model#v2");
    assert_eq!(
        t.state_key, "opus",
        "the runtime breaker key stays the nickname"
    );
}

/// `(nickname, provider entry, upstream)` of one fixture target.
type Endpoint = (&'static str, &'static str, &'static str);

#[test]
fn learned_identity_follows_the_endpoint_not_the_nickname() {
    // (name, learning target, reading target, shares the entry)
    let cases: &[(&str, Endpoint, Endpoint, bool)] = &[
        (
            "two nicknames on one endpoint share",
            ("opus", "alpha", "model-x"),
            ("opus-alias", "alpha", "model-x"),
            true,
        ),
        (
            "a renamed nickname keeps its history",
            ("old-name", "alpha", "model-x"),
            ("new-name", "alpha", "model-x"),
            true,
        ),
        (
            "a different provider entry starts fresh",
            ("opus", "alpha", "model-x"),
            ("opus", "beta", "model-x"),
            false,
        ),
        (
            "a different upstream starts fresh",
            ("opus", "alpha", "model-x"),
            ("opus", "alpha", "model-y"),
            false,
        ),
    ];

    for (name, (ln, lp, lu), (rn, rp, ru), shared) in cases {
        let router = router();
        let learner = target(&router, ln, lp, lu);
        let reader = target(&router, rn, rp, ru);

        learn_on(&router, &learner);

        assert!(routes_away(&router, &learner), "{name}: the learner acts");
        assert_eq!(routes_away(&router, &reader), *shared, "{name}");
    }
}

#[test]
fn a_second_nickname_on_a_learned_lane_does_not_corroborate_it_in_one_request() {
    // Two chain targets on one endpoint meet on one registry entry, so the
    // per-request dedupe keys on the lane: the sibling's rejection must not
    // count as a second observation of the same fact.
    let router = router();
    let first = target(&router, "opus", "alpha", "model-x");
    let second = target(&router, "opus-alias", "alpha", "model-x");
    let err = Error::upstream_full("stub", 400, "{}".to_string(), None, None, None);
    let mut dedupe = HashSet::new();
    let mut meta = DispatchMeta::for_alias("m");

    for t in [&first, &second] {
        router.commit_learned_observation(
            (
                "web_search".to_string(),
                SignalTier::Inferred,
                FailurePhase::F1,
            ),
            &FailureClass::BadRequest,
            &err,
            400,
            None,
            KIND,
            t,
            &web_search_request(),
            false,
            &mut dedupe,
            &mut meta,
            &mut LearnedProbeGuard::inert(),
        );
    }

    let entries = router.learned_capabilities.snapshot();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].observations, 1);
    assert!(
        !routes_away(&router, &second),
        "a single inferred observation stays pending"
    );
}

#[test]
fn a_pooled_seat_lane_is_the_member_entry_not_the_pool_or_the_account() {
    let router = router();
    let p: Arc<dyn Provider> = Arc::new(StubProvider);
    let seats: Vec<SeatTarget> = ["alpha", "beta"]
        .iter()
        .map(|member| SeatTarget {
            provider_name: (*member).to_string(),
            provider: Arc::clone(&p),
            auth_secret_ref: Some(
                routectl_auth::SecretRef::parse("oauth://acct#shared").expect("parse oauth ref"),
            ),
        })
        .collect();
    let model = ResolvedModel::new("sonnet", "the-pool", Arc::clone(&p), "claude-sonnet")
        .with_seats(seats.into());

    let targets = router.expand_chain_to_targets(vec![Arc::new(model)], None);

    let lanes: Vec<(String, String, String)> = targets
        .iter()
        .map(|t| {
            let lane = t.learned_lane.as_ref().expect("a seat lane is minted");
            (
                t.state_key.clone(),
                lane.provider_entry().to_string(),
                lane.upstream().to_string(),
            )
        })
        .collect();
    assert_eq!(
        lanes,
        vec![
            (
                "sonnet#alpha".into(),
                "alpha".into(),
                "claude-sonnet".into()
            ),
            ("sonnet#beta".into(), "beta".into(), "claude-sonnet".into()),
        ],
    );
    assert!(
        targets
            .iter()
            .all(|t| t.seat.as_deref() == Some("acct#shared")),
        "both seats share one account identity, yet their lanes differ",
    );
}

#[test]
fn a_field_verdict_keeps_the_runtime_key_its_owner_mints() {
    let router = router();
    let t = target(&router, "opus", "alpha", "model-x");
    let field_key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("the grounded path is well-formed");

    assert_eq!(t.learned_key(&field_key), Some("opus"));
    assert_eq!(t.learned_key("web_search"), Some("alpha#model-x"));
}

#[test]
fn an_override_on_any_nickname_of_a_lane_lapses_the_shared_negative_on_reload() {
    // The override sweep resolves the lane to every model on it through the
    // resolved table, so a cell added for the SECOND nickname still lapses the
    // entry the first one learned.
    let models = |router: &mut Router| {
        let mut table: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        for nick in ["opus", "opus-alias"] {
            let p: Arc<dyn Provider> = Arc::new(StubProvider);
            table.insert(
                nick.to_string(),
                Arc::new(ResolvedModel::new(nick, "alpha", p, "model-x")),
            );
        }
        router.install_resolved_models(table);
    };
    let mut before = router();
    models(&mut before);
    let learner = target(&before, "opus", "alpha", "model-x");
    learn_on(&before, &learner);
    let expires_before = before.learned_capabilities.snapshot()[0].expires_at;

    let overridden = format!(
        "{PROVIDERS}\n[capability.overrides.\"alpha:opus-alias\"]\nforce_supported = [\"web_search\"]\n"
    );
    let mut after = Router::new(Arc::new(toml::from_str(&overridden).expect("valid toml")));
    models(&mut after);
    after.carry_over_learned_from(&before);

    let entry = &after.learned_capabilities.snapshot()[0];
    assert!(
        entry.expires_at < expires_before,
        "the override change on the sibling nickname lapsed the shared entry",
    );
}
