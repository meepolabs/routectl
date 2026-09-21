//! The probe body, the seat it dials, and the pre-dial recheck.
//!
//! Two properties meet here. The body must carry the field under test --
//! without it the validator answers a plain token count and reads its
//! success as evidence about a field it never sent. And the seat actually
//! dialed must be re-checked against the CURRENT provider entry, because
//! activation guarded the entry it resolved then and a reload can have
//! replaced it since.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use routectl_core::{ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};

use super::Router;
use super::probe_test_support::{
    BodyAssertingProvider, BoxStreamAlias, GROUNDED_PATH, OkProvider, grounding_request,
    remote_router, remote_router_with_model_beta,
};
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry};
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbeValidator;
use crate::resolved::ResolvedModel;

#[tokio::test]
async fn the_probe_body_carries_the_grounded_field_the_upstream_model_and_one_message() {
    let provider = Arc::new(BodyAssertingProvider {
        seen: parking_lot::Mutex::new(Vec::new()),
    });
    let router = remote_router(provider.clone());
    let mut admitted = grounding_request();
    admitted.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    let _ = router.complete(admitted).await;

    let ran = router.run_due_probes().await;
    assert_eq!(ran, 1);

    let seen = provider.seen.lock();
    let probe = seen.last().expect("the worker dialed count_tokens");
    // The exact closed-table field AND the value the admitted request
    // carried -- a probe naming the field but sending a different value
    // would ask about a shape the upstream never saw.
    assert_eq!(
        probe
            .routectl_internal
            .anthropic_thinking_display
            .as_deref(),
        Some("summarized"),
        "the probe must carry the capability field/value under test"
    );
    // The seat's WIRE model, not the nickname: this body goes straight to
    // the provider, past alias/model resolution.
    assert_eq!(
        probe.model, "claude-sonnet-4-5",
        "the probe must name the resolved seat's upstream wire model"
    );
    assert_eq!(
        probe.messages.len(),
        1,
        "one minimal message, so count_tokens is well-formed"
    );
    // The minimal canonical reasoning state the Anthropic normalizer
    // requires, without which `build_thinking` returns early and the wire
    // body carries no `thinking` object at all -- the probe would then ask a
    // plain token count and read its success as evidence about a field it
    // never sent. The serializer-level pin lives in routectl-providers
    // (`a_probe_shaped_body_serializes_both_thinking_type_and_display`);
    // this asserts the router produces exactly that shape.
    let reasoning = probe
        .reasoning
        .as_ref()
        .expect("the probe body must carry a reasoning state");
    assert!(
        reasoning.enabled == Some(true)
            || reasoning.effort.is_some()
            || reasoning.max_tokens.is_some(),
        "the reasoning state must make thinking ACTIVE, or the normalizer emits nothing: \
         {reasoning:?}"
    );
    assert!(
        probe.max_tokens.is_some(),
        "the legacy thinking shape needs a budget window to clamp into"
    );
    // And the run SETTLED, which it only can if the field was present.
    assert_eq!(router.probe_scheduler_snapshot().resolved_total, 1);
}

#[tokio::test]
async fn the_generated_probe_body_serializes_a_thinking_object_through_the_real_normalizer() {
    // WELDED, not asserted about. The two assertions above describe the
    // router's canonical `ChatRequest`; whether that shape actually reaches the
    // wire as a `thinking` object is a fact about the ANTHROPIC NORMALIZER, and
    // a router-side assertion about `reasoning.enabled` is a claim about the
    // normalizer rather than a measurement of it. `build_thinking` returns
    // early on several conditions, so the budget/reasoning state the probe sets
    // is only load-bearing if the real normalizer emits the field given it.
    //
    // So: take the REAL generated probe body and run it through the REAL
    // provider's `normalize_request`. No private provider constant is exported
    // for this -- the `Provider` trait method is the production seam, and a
    // hand-built body here would reintroduce exactly the claim being removed.
    let provider = Arc::new(BodyAssertingProvider {
        seen: parking_lot::Mutex::new(Vec::new()),
    });
    // The operator beta floor comes from the model's own `header_extras`, the
    // way the dispatch overlay composes it in production -- planting it on the
    // ingress request instead would be a fixture that outlives its own premise,
    // since the overlay RECOMPUTES the field per target and would overwrite it.
    let router = remote_router_with_model_beta(provider.clone(), "context-1m-2025-08-07");
    let mut admitted = grounding_request();
    admitted.routectl_internal.anthropic_thinking_display = Some("summarized".to_string());
    // A client/ingress beta alongside it, so both sources are represented.
    admitted.anthropic_beta = vec!["interleaved-thinking-2025-05-14".to_string()];
    let _ = router.complete(admitted).await;
    assert_eq!(router.run_due_probes().await, 1);

    let probe = provider
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the worker dialed count_tokens");

    let real = routectl_providers::anthropic_api::AnthropicApiProvider::new(
        routectl_providers::anthropic_api::AnthropicApiConfig::new("p1", "literal-key"),
    );
    let body = real
        .normalize_request(&probe)
        .expect("the generated probe body must normalize");

    assert_eq!(
        body["thinking"]["type"], "enabled",
        "the generated probe body must reach the wire as an ACTIVE thinking object"
    );
    assert_eq!(
        body["thinking"]["display"], "summarized",
        "and it must carry the display value under test"
    );

    // Beta fidelity: the probe must preserve the admitted request's effective
    // beta context, from both sources, so a beta-gated field is tested in the
    // header context the upstream was about to see. Removing the capture makes
    // both of these empty.
    assert!(
        probe
            .anthropic_beta
            .contains(&"interleaved-thinking-2025-05-14".to_string()),
        "the probe must carry the client/ingress beta union: {:?}",
        probe.anthropic_beta
    );
    assert!(
        probe
            .routectl_internal
            .operator_betas
            .contains(&"context-1m-2025-08-07".to_string()),
        "the probe must carry the operator beta floor: {:?}",
        probe.routectl_internal.operator_betas
    );
}

// ---------------------------------------------------------------------
// Pooled seat selection and the pre-dial recheck
// ---------------------------------------------------------------------

/// A provider that reports which seat answered, so a test can prove the
/// probe reached the seat its identity names rather than seat zero.
struct NamedSeatProvider {
    id: &'static str,
    calls: Arc<parking_lot::Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl Provider for NamedSeatProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Ok(ChatResponse::default())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStreamAlias> {
        Err(Error::upstream("p", 500, "body"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.lock().push(self.id);
        Ok(TokenCount {
            input_tokens: 3,
            extras: serde_json::Map::new(),
        })
    }
}

/// A two-seat pooled model. Seat `seat-a` is FIRST in the member list, so a
/// resolver taking seat zero would always pick it.
fn pooled_router() -> (Router, Arc<parking_lot::Mutex<Vec<&'static str>>>) {
    pooled_router_named("m1", ["seat-a", "seat-b"])
}

#[tokio::test]
async fn a_pooled_identity_probes_the_seat_its_state_key_names_not_seat_zero() {
    let (router, calls) = pooled_router();
    // An identity minted against the SECOND member. Seat zero is `seat-a`,
    // so a resolver ignoring the label would dial the wrong account.
    let key = FieldVerdictKey::new("m1#seat-b", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1);
    assert_eq!(
        calls.lock().as_slice(),
        ["seat-b"],
        "the probe must reach the seat its state key names"
    );
}

#[tokio::test]
async fn a_pooled_identity_naming_no_live_member_probes_nothing() {
    // Converse: a label that matches no current member must dial NOTHING
    // rather than falling back to an arbitrary seat.
    let (router, calls) = pooled_router();
    let key =
        FieldVerdictKey::new("m1#seat-gone", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(
        calls.lock().is_empty(),
        "an unresolvable seat must not fall back to another account"
    );
}

#[tokio::test]
async fn the_actual_dial_target_is_re_guarded_for_loopback() {
    // Activation guarded the entry it resolved THEN. A reload can
    // replace that entry with a loopback one before the dial, and a probe
    // must not dial a target whose rejection it could not attribute.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    // Swap the provider entry under the queued job, as a reload would.
    let mut config = (*router.config).clone();
    let mut entry = ProviderEntry::anthropic_api("literal:k");
    if let ProviderEntry::AnthropicApi { base_url, .. } = &mut entry {
        *base_url = "http://127.0.0.1:9".to_string();
    }
    config.providers.insert("p1".to_string(), entry);
    let mut router = router;
    router.config = Arc::new(config);

    router.run_due_probes().await;

    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "the pre-dial guard must refuse a now-loopback target"
    );
}

#[tokio::test]
async fn the_actual_dial_target_is_re_guarded_for_a_forwarded_entry() {
    // The FORWARDED direction of the same re-check, and the one a recheck
    // narrower than activation's refusal set would miss.
    //
    // Activation refused forwarded targets and admitted this job against a
    // routectl-credentialled entry. A reload then swaps that entry for one
    // authenticating with the CLIENT's bearer -- and nothing else looks
    // again, so if the recheck does not mirror activation's refusal the
    // queued job dials a target whose rejection is not attributable to a
    // routectl-owned seat, minting a permanent verdict from one client's
    // request.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    // Replace the entry with a forwarded-credential one on the SAME remote
    // base url, so the only thing that changed is the credential source.
    let mut config = (*router.config).clone();
    let forwarded = ProviderEntry::anthropic_api("https://api.anthropic.com")
        .with_credential_source(crate::config::CredentialSource::Forwarded);
    assert!(
        forwarded.forwarded_base_url().is_some(),
        "premise: the swapped entry must actually read as forwarded, or this \
         test proves nothing about the refusal"
    );
    config.providers.insert("p1".to_string(), forwarded);
    let mut router = router;
    router.config = Arc::new(config);

    router.run_due_probes().await;

    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "a queued job must not dial an entry that now forwards the client's \
         credential -- its rejection would not be attributable"
    );
    // Positive control that the fixture CAN dial:
    // `a_closed_breaker_lets_the_probe_dial` runs the same activation and
    // worker on an unswapped entry and observes a dial.
}

#[tokio::test]
async fn the_actual_dial_target_is_re_guarded_for_a_vanished_entry() {
    // The Mantle / missing-entry direction of the same re-check: the
    // accessor answers None, so there is no attributable base url.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let mut config = (*router.config).clone();
    config.providers.remove("p1");
    let mut router = router;
    router.config = Arc::new(config);

    router.run_due_probes().await;

    assert_eq!(provider.count_calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------
// The runtime gate applies before any probe dial
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_open_breaker_blocks_the_probe_dial_without_spending_the_free_step() {
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);
    router.force_open_breaker("m1", Duration::from_hours(1));

    router.run_due_probes().await;

    assert_eq!(
        provider.count_calls.load(Ordering::SeqCst),
        0,
        "a probe must not dial into an open breaker"
    );
    let snap = router.probe_scheduler_snapshot();
    assert_eq!(
        snap.free_exhausted_total, 0,
        "a gate refusal must not spend the free step"
    );
    assert_eq!(
        snap.backing_off, 1,
        "a gate refusal retries under backoff -- the gate said not now, not no"
    );
}

#[tokio::test]
async fn a_closed_breaker_lets_the_probe_dial() {
    // Positive control for the gate test above: same fixture, breaker
    // closed, and the dial happens -- so the refusal is about the gate
    // rather than about the worker never dialing.
    let provider = Arc::new(OkProvider {
        count_calls: AtomicUsize::new(0),
    });
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new("m1", GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    router.run_due_probes().await;

    assert!(provider.count_calls.load(Ordering::SeqCst) > 0);
}

// ---------------------------------------------------------------------

/// A pooled router whose nickname and seat labels are caller-chosen, so a test
/// can put the seat-key separator INSIDE either half.
///
/// The seat provider ids are `&'static str` for the call log, so the labels
/// must be statics too.
fn pooled_router_named(
    nickname: &str,
    labels: [&'static str; 2],
) -> (Router, Arc<parking_lot::Mutex<Vec<&'static str>>>) {
    let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut config = Config::default();
    for name in labels {
        config
            .providers
            .insert(name.to_string(), ProviderEntry::anthropic_api("literal:k"));
    }
    config.models.insert(
        nickname.to_string(),
        ModelEntry::new("pool", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(nickname.to_string()),
    );
    let mut router = Router::new(Arc::new(config));
    let seats: Vec<crate::seat_pool::SeatTarget> = labels
        .into_iter()
        .map(|name| crate::seat_pool::SeatTarget {
            provider_name: name.to_string(),
            provider: Arc::new(NamedSeatProvider {
                id: name,
                calls: Arc::clone(&calls),
            }) as Arc<dyn Provider>,
            auth_secret_ref: None,
        })
        .collect();
    let model = ResolvedModel::new(
        nickname,
        "pool",
        Arc::new(NamedSeatProvider {
            id: "pool",
            calls: Arc::clone(&calls),
        }) as Arc<dyn Provider>,
        "claude-sonnet-4-5",
    )
    .with_seats(Arc::from(seats));
    let mut models = std::collections::BTreeMap::new();
    models.insert(nickname.to_string(), Arc::new(model));
    router.install_resolved_models(models);
    (router, calls)
}

#[tokio::test]
async fn a_pooled_nickname_containing_the_separator_still_reaches_its_own_seat() {
    // The key is composed as `{nickname}#{label}`, so a nickname that itself
    // contains `#` puts a SECOND separator to the left of the real one.
    //
    // `split_once('#')` takes the FIRST, so it reads this key as nickname
    // "weird" + label "name#seat-b" -- no such model, no such seat, nothing
    // dialed. Recomposing each candidate's canonical key and comparing is
    // indifferent to how many separators either half contains.
    let (router, calls) = pooled_router_named("weird#name", ["seat-a", "seat-b"]);
    let composed = crate::seat_pool::seat_state_key("weird#name", Some("seat-b"));
    assert_eq!(
        composed, "weird#name#seat-b",
        "premise: the composer must actually produce two separators here"
    );
    let key = FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1);
    assert_eq!(
        calls.lock().as_slice(),
        ["seat-b"],
        "a nickname containing the separator must still reach the credential \
         its own composed key names"
    );
}

#[tokio::test]
async fn a_pooled_label_containing_the_separator_still_reaches_its_own_seat() {
    // The mirror case, and why `rsplit_once` is not the fix either: a LABEL
    // containing `#` puts an extra separator to the right of the real one.
    //
    // `rsplit_once('#')` takes the LAST, reading this key as nickname
    // "m1#seat" + label "b" -- again no such model and no such seat. Only one
    // of the two splits is wrong for each of these two tests, which is the
    // point: no single split direction is correct while the grammar permits
    // the character on both sides.
    let (router, calls) = pooled_router_named("m1", ["seat-a", "seat#b"]);
    let composed = crate::seat_pool::seat_state_key("m1", Some("seat#b"));
    assert_eq!(
        composed, "m1#seat#b",
        "premise: the composer must actually produce two separators here"
    );
    let key = FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1);
    assert_eq!(
        calls.lock().as_slice(),
        ["seat#b"],
        "a label containing the separator must still reach its own credential"
    );
}

#[tokio::test]
async fn a_pooled_key_with_separators_in_both_halves_reaches_its_own_seat() {
    // Both at once, which no single split direction can parse: two separators
    // to the left of the real one and one to its right. The recomposed
    // comparison resolves it because it never has to decide WHICH separator
    // is the delimiter -- it only asks which candidate composes to this key.
    let (router, calls) = pooled_router_named("a#b#c", ["plain", "d#e"]);
    let composed = crate::seat_pool::seat_state_key("a#b#c", Some("d#e"));
    assert_eq!(composed, "a#b#c#d#e", "premise: four separators");
    let key = FieldVerdictKey::new(&composed, GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    let ran = router.run_due_probes().await;

    assert_eq!(ran, 1);
    assert_eq!(
        calls.lock().as_slice(),
        ["d#e"],
        "the intended credential must be reached regardless of separators in \
         either half"
    );
    // And the SIBLING was not dialed, so this is a match rather than a
    // fallback to an arbitrary seat.
    assert!(
        !calls.lock().contains(&"plain"),
        "no fallback to another member may occur"
    );
}
