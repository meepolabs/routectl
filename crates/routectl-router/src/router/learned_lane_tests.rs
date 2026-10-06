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
fn every_capability_namespace_keys_on_the_lane() {
    let router = router();
    let t = target(&router, "opus", "alpha", "model-x");
    let field_key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("the grounded path is well-formed");

    assert_eq!(t.learned_key(&field_key), Some("alpha#model-x"));
    assert_eq!(t.learned_key("web_search"), Some("alpha#model-x"));
}

#[test]
fn a_field_verdict_learned_under_one_nickname_applies_to_another_on_the_lane() {
    // A field verdict minted through the production identity for one nickname's
    // target, then read through the production lookup on a sibling nickname
    // sharing the provider entry and upstream -- and on a target on another
    // upstream, which must not see it.
    let router = router();
    let learner = target(&router, "opus", "alpha", "model-x");
    let sibling = target(&router, "opus-alias", "alpha", "model-x");
    let elsewhere = target(&router, "opus", "alpha", "model-y");
    let path = "thinking.enabled.display";
    let field_key =
        crate::field_capability::field_capability_key(path).expect("the grounded path mints");
    let identity = crate::field_verdict::FieldVerdictKey::new(
        learner.learned_lane.as_ref().expect("a lane is minted"),
        path,
        KIND,
    )
    .expect("the grounded path mints an identity");
    let now = Instant::now();
    let guard = router
        .field_verdicts()
        .admit_provisional(
            &identity,
            "https://alpha.example.test/v1",
            router.registry_generation(),
            now,
        )
        .expect("an unknown identity admits one repair");
    let learned = guard.commit(400, vec![field_key.clone()], now);
    assert!(
        learned.is_some(),
        "premise: the repaired retry persisted a verdict"
    );

    let field_acts_on = |t: &DispatchTarget| {
        let key = t.learned_key(&field_key).expect("the target has a lane");
        matches!(
            router
                .acting_negative_with_generation(key, &field_key, KIND, Instant::now())
                .0,
            RoutingDecision::RouteAway { .. }
        )
    };
    assert!(
        field_acts_on(&learner),
        "premise: the learner reads its own verdict"
    );
    assert!(
        field_acts_on(&sibling),
        "a sibling nickname on the same lane reads the same field verdict",
    );
    assert!(
        !field_acts_on(&elsewhere),
        "a different upstream is a different lane and starts fresh",
    );
}

/// Two nicknames on the `alpha#model-x` lane, installed in the resolved table
/// so the override sweep sees both.
fn install_two_nicknames(router: &mut Router) {
    let mut table: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for nick in ["opus", "opus-alias"] {
        let p: Arc<dyn Provider> = Arc::new(StubProvider);
        table.insert(
            nick.to_string(),
            Arc::new(ResolvedModel::new(nick, "alpha", p, "model-x")),
        );
    }
    router.install_resolved_models(table);
}

/// Learn a web_search negative on `opus`, then reload under `overrides`
/// appended to the provider table. Returns the reloaded router and the
/// entry's expiry before the reload.
fn reload_with_overrides(overrides: &str) -> (Router, Instant) {
    let mut before = router();
    install_two_nicknames(&mut before);
    learn_on(&before, &target(&before, "opus", "alpha", "model-x"));
    let expires_before = before.learned_capabilities.snapshot()[0].expires_at;

    let config = format!("{PROVIDERS}\n{overrides}");
    let mut after = Router::new(Arc::new(toml::from_str(&config).expect("valid toml")));
    install_two_nicknames(&mut after);
    after.carry_over_learned_from(&before);
    (after, expires_before)
}

#[test]
fn an_override_on_one_nickname_of_a_lane_leaves_the_shared_negative_acting_for_its_sibling() {
    let (after, expires_before) = reload_with_overrides(
        "[capability.overrides.\"alpha:opus-alias\"]\nforce_supported = [\"web_search\"]\n",
    );

    assert_eq!(
        after.learned_capabilities.snapshot()[0].expires_at,
        expires_before,
        "a change scoped to one nickname does not lapse the lane-wide fact",
    );
    assert!(
        routes_away(&after, &target(&after, "opus", "alpha", "model-x")),
        "the sibling whose override did not change still routes away",
    );
}

#[test]
fn an_override_for_the_whole_provider_lapses_the_shared_negative_on_reload() {
    let (after, expires_before) =
        reload_with_overrides("[capability.overrides.alpha]\nforce_supported = [\"web_search\"]\n");

    assert!(
        after.learned_capabilities.snapshot()[0].expires_at < expires_before,
        "a provider-tier change moves the intent for every nickname on the lane",
    );
}

#[test]
fn an_override_on_every_nickname_of_a_lane_lapses_the_shared_negative_on_reload() {
    let (after, expires_before) = reload_with_overrides(
        "[capability.overrides.\"alpha:opus\"]\nforce_supported = [\"web_search\"]\n\
         [capability.overrides.\"alpha:opus-alias\"]\nforce_supported = [\"web_search\"]\n",
    );

    assert!(
        after.learned_capabilities.snapshot()[0].expires_at < expires_before,
        "a change for every nickname on the lane lapses the shared entry",
    );
}
