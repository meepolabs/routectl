//! What the forward-compat sweep keeps out of `provider_extras`, and what
//! that does to the egress bodies. An Anthropic body carrying the OpenAI
//! token-cap alias `max_completion_tokens` beside `max_tokens`, and a
//! top-level `reasoning_details`, must reach every egress exactly as the
//! same body without them does, while an unknown knob beside them still
//! forwards to every Anthropic-shape egress. Sent without `max_tokens`, the
//! alias becomes the token cap every Anthropic-family egress ships.

use routectl_core::Provider;
use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

const ALIAS_KEYS: [&str; 2] = ["max_completion_tokens", "reasoning_details"];
const KNOB: &str = "zz_future_knob";

fn parse(body: Value) -> ChatRequest {
    AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("request should parse")
}

fn body_without_aliases() -> Value {
    json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 4096,
        KNOB: {"depth": 3},
    })
}

fn body_with_aliases() -> Value {
    let mut body = body_without_aliases();
    body["max_completion_tokens"] = json!(777);
    body["reasoning_details"] = json!([{"type": "reasoning.text", "text": "zz-rd-sentinel"}]);
    body
}

fn bedrock_cfg(api_shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-opus-4-7".into(),
        api_shape,
        creds: BedrockCreds::BearerKey { key: "k".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn openai_compat() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat:test".into(),
        base_url: "http://127.0.0.1:9".into(),
        api_key: "k".into(),
        header_extras: Vec::new(),
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        mantle: None,
    })
}

/// `(lane, serialized egress body)` for each egress the weld covers.
fn egress_bodies(req: &ChatRequest) -> Vec<(&'static str, String)> {
    let anthropic = AnthropicApiProvider::new(AnthropicApiConfig::new("anthropic:test", "k"))
        .normalize_request(req)
        .expect("anthropic-api body");
    let invoke = routectl_providers::bedrock::invoke::normalize_request(
        &bedrock_cfg(BedrockApiShape::Invoke),
        req,
    )
    .expect("invoke body");
    let converse = routectl_providers::bedrock::converse::normalize_request(
        &bedrock_cfg(BedrockApiShape::Converse),
        req,
    )
    .expect("converse body");
    let compat = openai_compat()
        .normalize_request(req)
        .expect("openai-compat body");
    [
        ("anthropic-api", anthropic),
        ("bedrock-invoke", invoke),
        ("bedrock-converse", converse),
        ("openai-compat", compat),
    ]
    .into_iter()
    .map(|(lane, body)| (lane, serde_json::to_string(&body).expect("serialize")))
    .collect()
}

#[test]
fn the_canonical_aliases_leave_every_egress_body_byte_identical() {
    // Arrange
    let with = parse(body_with_aliases());
    let without = parse(body_without_aliases());

    // Act
    let with_bodies = egress_bodies(&with);
    let without_bodies = egress_bodies(&without);

    // Assert
    for ((lane, with_body), (_, without_body)) in with_bodies.iter().zip(&without_bodies) {
        assert_eq!(with_body, without_body, "{lane}");
    }
    for (lane, body) in &with_bodies {
        assert_eq!(
            body.contains(KNOB),
            *lane != "openai-compat",
            "the knob forwards on every Anthropic-shape lane, and never to the Chat wire \
             another dialect's client did not address: {lane}: {body}"
        );
    }
}

#[test]
fn the_sweep_leaves_the_canonical_aliases_out_of_provider_extras() {
    // Arrange / Act
    let req = parse(body_with_aliases());

    // Assert
    let extras = req
        .provider_extras
        .as_ref()
        .and_then(Value::as_object)
        .expect("the unknown knob lands in provider_extras");
    assert!(extras.contains_key(KNOB), "{extras:?}");
    for key in ALIAS_KEYS {
        assert!(!extras.contains_key(key), "{key}: {extras:?}");
    }
}

fn compat_managed_key_conflicts() -> u64 {
    routectl_providers::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| {
            e.lane == "openai-compat" && e.policy_class == "provider_extra_managed_key_conflict"
        })
        .map_or(0, |e| e.action_count)
}

/// Run `req` through openai-compat with a tap attached; return the
/// managed-key conflict count delta and the top-level paths the egress
/// declared as managed-extras removals.
fn compat_conflicts_and_strips(mut req: ChatRequest) -> (u64, Vec<String>) {
    let tap = WireTap::admitted();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let before = compat_managed_key_conflicts();
    openai_compat()
        .normalize_request(&req)
        .expect("openai-compat body");
    let delta = compat_managed_key_conflicts() - before;
    let strips = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|e| e.site() == EditSite::ManagedExtrasStrip && e.kind() == EditKind::Remove)
        .map(|e| e.path().to_string())
        .collect();
    (delta, strips)
}

#[test]
fn openai_compat_refuses_no_managed_key_for_the_canonical_aliases() {
    // Arrange -- the control carries the two aliases in the extras bag by
    // hand, so it proves the counter and the tap would see them there.
    let mut control = parse(body_without_aliases());
    let with_aliases = body_with_aliases();
    let extras = control
        .provider_extras
        .as_mut()
        .and_then(Value::as_object_mut)
        .expect("the knob is swept");
    for key in ALIAS_KEYS {
        extras.insert(key.into(), with_aliases[key].clone());
    }
    let swept = parse(with_aliases);

    // Act
    let (control_delta, control_strips) = compat_conflicts_and_strips(control);
    let (swept_delta, swept_strips) = compat_conflicts_and_strips(swept);

    // Assert
    assert!(control_delta >= 1, "control counted: {control_delta}");
    for key in ALIAS_KEYS {
        let path = format!("/{key}");
        assert!(
            control_strips.contains(&path),
            "control {key}: {control_strips:?}"
        );
        assert!(!swept_strips.contains(&path), "{key}: {swept_strips:?}");
    }
    assert_eq!(swept_delta, 0, "{swept_strips:?}");
}

/// The token cap each Anthropic-family egress body carries: `max_tokens` on
/// anthropic-api and Invoke, `inferenceConfig.maxTokens` on Converse.
fn shipped_cap(lane: &str, body: &Value) -> Option<u64> {
    let cap = match lane {
        "bedrock-converse" => body.pointer("/inferenceConfig/maxTokens"),
        _ => body.get("max_tokens"),
    };
    cap.and_then(Value::as_u64)
}

#[test]
fn max_completion_tokens_maps_to_the_token_cap_on_every_anthropic_family_lane() {
    let default = u64::from(routectl_providers::anthropic_api::DEFAULT_MAX_OUTPUT_TOKENS);
    let body = |max_tokens: Option<u64>, mct: Option<u64>| {
        let mut body = json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
        });
        if let Some(v) = max_tokens {
            body["max_tokens"] = json!(v);
        }
        if let Some(v) = mct {
            body["max_completion_tokens"] = json!(v);
        }
        body
    };
    // (name, body, [anthropic-api, bedrock-invoke, bedrock-converse])
    let rows: [(&str, Value, [Option<u64>; 3]); 3] = [
        (
            "only max_completion_tokens",
            body(None, Some(777)),
            [Some(777), Some(777), Some(777)],
        ),
        (
            "both: max_tokens wins",
            body(Some(4096), Some(777)),
            [Some(4096), Some(4096), Some(4096)],
        ),
        (
            "neither: the egress default",
            body(None, None),
            [Some(default), Some(default), None],
        ),
    ];

    for (name, body, want) in rows {
        let req = parse(body);
        let bodies = egress_bodies(&req);
        let lanes = ["anthropic-api", "bedrock-invoke", "bedrock-converse"];
        for (lane, want) in lanes.into_iter().zip(want) {
            let (_, sent) = bodies.iter().find(|(l, _)| *l == lane).expect("lane body");
            assert!(
                !sent.contains("max_completion_tokens"),
                "{name}: {lane}: {sent}"
            );
            let sent: Value = serde_json::from_str(sent).expect("egress json");
            assert_eq!(shipped_cap(lane, &sent), want, "{name}: {lane}: {sent}");
        }
    }
}
