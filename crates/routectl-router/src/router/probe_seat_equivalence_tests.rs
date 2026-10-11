//! Two nicknames on one learned lane send the same free probe.
//!
//! A lane is `(provider entry, upstream)`, so several nicknames can share one,
//! each with its own `header_extras` and `payload_extras`. The probe seat is
//! the FIRST nickname in order, and that choice selects only the breaker and
//! RPM bucket the probe admits through: the probe body is built from the seat's
//! wire model and the leased payload, never from the nickname's extras. So a
//! rename that reorders the nicknames must not change what the probe asks.
//!
//! Known limit: a model's non-beta `header_extras` and `payload_extras` never
//! reach the free probe, for ANY nickname on the lane. Only beta context rides
//! along, captured into the payload from the admitted request. A field
//! re-check whose answer depends on a model extra would therefore be asked
//! without it.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, Usage};
use serde_json::{Value, json};

use super::Router;
use super::probe_test_support::{
    BoxStreamAlias, GROUNDED_PATH, RECHECK_PATH, RECHECK_VALUE, grounding_request,
    plant_acting_negative, recheck_value,
};
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::{FreeValidatorOutcome, ProbePayload, ProbeValidator};
use crate::resolved::ResolvedModel;

const UPSTREAM: &str = "claude-sonnet-4-5";
const TAG_HEADER: &str = "x-tenant-tag";

/// The nickname carrying the north extras in both routers.
const NORTH: &str = "opus-a";
/// The south nickname in the router where it sorts AFTER the north one.
const SOUTH_LATE: &str = "opus-b";
/// The same south nickname renamed so it sorts BEFORE the north one.
const SOUTH_EARLY: &str = "opus-0";

/// One nickname's operator extras.
struct Extras {
    tag: &'static str,
    top_k: u32,
}

const NORTH_EXTRAS: Extras = Extras {
    tag: "north",
    top_k: 5,
};
const SOUTH_EXTRAS: Extras = Extras {
    tag: "south",
    top_k: 9,
};

impl Extras {
    fn headers(&self) -> BTreeMap<String, String> {
        BTreeMap::from([(TAG_HEADER.to_string(), self.tag.to_string())])
    }

    fn payload(&self) -> Value {
        json!({ "top_k": self.top_k })
    }
}

/// Records every request it is handed, split by method, and answers both
/// successfully so the probe settles rather than retrying.
#[derive(Default)]
struct CapturingProvider {
    completions: parking_lot::Mutex<Vec<ChatRequest>>,
    counts: parking_lot::Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.completions.lock().push(req);
        Ok(ChatResponse {
            model: UPSTREAM.to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p1", 500, "body"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.counts.lock().push(req);
        Ok(TokenCount {
            input_tokens: 11,
            extras: serde_json::Map::new(),
        })
    }
}

/// A router with one anthropic-api entry `p1` on the public API and two
/// nicknames, both sending `UPSTREAM` through it, each with its own extras.
fn router_with(nicknames: [(&str, &Extras); 2], provider: &Arc<CapturingProvider>) -> Router {
    let mut config = Config::default();
    let mut entry = ProviderEntry::anthropic_api("literal:k");
    if let ProviderEntry::AnthropicApi { base_url, .. } = &mut entry {
        *base_url = "https://api.anthropic.com".to_string();
    }
    config.providers.insert("p1".to_string(), entry);
    for (nickname, extras) in nicknames {
        config.models.insert(
            nickname.to_string(),
            ModelEntry::new("p1", UPSTREAM)
                .with_header_extras(extras.headers())
                .with_payload_extras(extras.payload()),
        );
    }
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(nicknames[0].0.to_string()),
    );
    let mut router = Router::new(Arc::new(config));
    // The dispatch overlay reads the extras off the RESOLVED model, so they
    // are installed there too; the config entry alone never reaches a request.
    let models = nicknames
        .into_iter()
        .map(|(nickname, extras)| {
            let model = ResolvedModel::new(
                nickname,
                "p1",
                Arc::clone(provider) as Arc<dyn Provider>,
                UPSTREAM,
            )
            .with_header_extras(extras.headers())
            .with_payload_extras(extras.payload());
            (nickname.to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    router
}

/// North sorts first, so the probe seat is the north nickname.
fn north_first(provider: &Arc<CapturingProvider>) -> Router {
    router_with(
        [(NORTH, &NORTH_EXTRAS), (SOUTH_LATE, &SOUTH_EXTRAS)],
        provider,
    )
}

/// Identical except the south nickname is renamed to sort first, so the probe
/// seat is the south nickname.
fn south_first(provider: &Arc<CapturingProvider>) -> Router {
    router_with(
        [(SOUTH_EARLY, &SOUTH_EXTRAS), (NORTH, &NORTH_EXTRAS)],
        provider,
    )
}

fn lane_key() -> FieldVerdictKey {
    let lane = crate::state_key::StateKey::new("p1", UPSTREAM)
        .expect("a separator-free entry mints a lane");
    FieldVerdictKey::new(&lane, GROUNDED_PATH, "anthropic-api").expect("identity")
}

fn seat_nickname(router: &Router) -> String {
    router
        .probe_seat_for(&lane_key())
        .expect("the lane resolves to a seat")
        .state_key
}

fn learned_lane_of(router: &Router, nickname: &str) -> crate::state_key::StateKey {
    let model = router
        .resolved_models
        .get(nickname)
        .cloned()
        .expect("the nickname is installed");
    router
        .expand_chain_to_targets(vec![model], None)
        .pop()
        .and_then(|target| target.learned_lane)
        .expect("a non-seat target mints a lane")
}

/// The request the upstream saw for a plain dispatch through `nickname`.
async fn dispatched_through(nickname: &str) -> ChatRequest {
    let provider = Arc::new(CapturingProvider::default());
    let router = north_first(&provider);
    let req = ChatRequest {
        model: nickname.to_string(),
        ..Default::default()
    };
    router.complete(req).await.expect("the dispatch succeeds");
    provider
        .completions
        .lock()
        .pop()
        .expect("the upstream saw the dispatch")
}

/// A fixed payload with both beta sources populated, so an equal pair of
/// probes is equal over a non-empty beta context.
fn fixed_payload() -> ProbePayload {
    ProbePayload::new(
        GROUNDED_PATH,
        "summarized".to_string(),
        &["interleaved-thinking-2025-05-14".to_string()],
        &["context-1m-2025-08-07".to_string()],
        true,
    )
    .expect("a modeled display token within every bound")
}

/// Run the free count-tokens validator for the lane with the fixed payload and
/// return the request it dialed.
async fn free_probe_request(router: &Router, provider: &CapturingProvider) -> ChatRequest {
    let outcome = router
        .run_free_validator_for_tests(&lane_key(), &fixed_payload(), ProbeValidator::CountTokens)
        .await;
    assert_eq!(outcome, FreeValidatorOutcome::Settled);
    provider
        .counts
        .lock()
        .pop()
        .expect("the validator dialed count_tokens")
}

#[test]
fn both_nicknames_share_one_learned_lane_and_the_order_picks_a_different_seat() {
    let provider = Arc::new(CapturingProvider::default());
    let north = north_first(&provider);
    let south = south_first(&provider);

    assert_eq!(
        learned_lane_of(&north, NORTH).as_lane_key(),
        learned_lane_of(&north, SOUTH_LATE).as_lane_key(),
        "both nicknames must learn on one lane"
    );
    assert_eq!(
        learned_lane_of(&north, NORTH).as_lane_key(),
        lane_key().lane().as_lane_key(),
        "and that lane must be the one the probe identity names"
    );
    assert_eq!(seat_nickname(&north), NORTH);
    assert_eq!(
        seat_nickname(&south),
        SOUTH_EARLY,
        "the rename must move the probe seat to the other nickname"
    );
}

#[tokio::test]
async fn the_two_nicknames_dispatch_with_different_extras() {
    let north = dispatched_through(NORTH).await;
    let south = dispatched_through(SOUTH_LATE).await;

    let tag = |req: &ChatRequest| {
        req.routectl_internal
            .header_extras
            .as_ref()
            .and_then(|headers| headers.get(TAG_HEADER).cloned())
    };
    assert_eq!(tag(&north).as_deref(), Some(NORTH_EXTRAS.tag));
    assert_eq!(tag(&south).as_deref(), Some(SOUTH_EXTRAS.tag));
    assert_eq!(north.provider_extras, Some(NORTH_EXTRAS.payload()));
    assert_eq!(south.provider_extras, Some(SOUTH_EXTRAS.payload()));
}

#[tokio::test]
async fn one_fixed_payload_builds_the_same_probe_on_either_nicknames_seat() {
    let north_provider = Arc::new(CapturingProvider::default());
    let south_provider = Arc::new(CapturingProvider::default());
    let north = free_probe_request(&north_first(&north_provider), &north_provider).await;
    let south = free_probe_request(&south_first(&south_provider), &south_provider).await;

    assert_eq!(format!("{north:?}"), format!("{south:?}"));
    assert_eq!(north.model, UPSTREAM);
    assert_eq!(
        north.routectl_internal.operator_betas,
        ["context-1m-2025-08-07"]
    );
    assert_eq!(north.anthropic_beta, ["interleaved-thinking-2025-05-14"]);
    assert_eq!(
        north.routectl_internal.header_extras, None,
        "no nickname's headers reach the probe"
    );
    assert_eq!(
        north.provider_extras, None,
        "no nickname's payload extras reach the probe"
    );
}

#[tokio::test]
async fn reordering_the_nicknames_leaves_the_end_to_end_probe_unchanged() {
    let (north, north_ran) = admitted_then_probed(north_first).await;
    let (south, south_ran) = admitted_then_probed(south_first).await;

    assert_eq!((north_ran, south_ran), (1, 1));
    assert_eq!(
        recheck_value(&north),
        Some(RECHECK_VALUE),
        "the probe must carry the admitted field"
    );
    assert_eq!(format!("{north:?}"), format!("{south:?}"));
}

/// Admit a grounding request through the north nickname, which carries the
/// same extras in both routers so the captured payload is identical, then run
/// the worker and return the count_tokens request it dialed. The lane holds an
/// acting negative on the re-check row first, since a probe only ever
/// re-checks one.
async fn admitted_then_probed(
    build: fn(&Arc<CapturingProvider>) -> Router,
) -> (ChatRequest, usize) {
    let provider = Arc::new(CapturingProvider::default());
    let router = build(&provider);
    plant_acting_negative(&router, lane_key().lane(), RECHECK_PATH, "anthropic-api");
    let mut admitted = grounding_request();
    admitted.model = NORTH.to_string();
    router
        .complete(admitted)
        .await
        .expect("the dispatch succeeds");

    let ran = router.run_due_probes().await;

    let probe = provider
        .counts
        .lock()
        .pop()
        .expect("the worker dialed count_tokens");
    (probe, ran)
}
