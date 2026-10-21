//! Each anthropic-vocabulary beta carrier deposits, on the attempt's beta
//! assembly slot, the floor and body-implied sets its assembly computed: the
//! `anthropic-beta` header on the anthropic-api lanes, the Invoke body and
//! the Converse bag on Bedrock.
//!
//! Every row ships the same client set, which already names implied flags
//! (structured outputs, re-added after its withhold, and display updates,
//! never withheld) and withholds a plain client flag and an operator pin, so
//! a row fails if the deposit reads the sets before the union, reports only
//! the flags a union pushed, or drops the floor.

use super::*;
use routectl_core::identity::anthropic::{
    EFFORT_BETA, OAUTH_ANTHROPIC_BETA, REDACT_THINKING_BETA, STRUCTURED_OUTPUTS_BETA,
    default_claude_code_anthropic_betas,
};
use routectl_core::{BetaAssembly, BetaAssemblyReport, ChatRequest, StaticToken};
use serde_json::json;

use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const PLAIN_HOST: &str = "http://127.0.0.1:18080";
const OAUTH_HOST: &str = "https://api.anthropic.com";
const CLIENT_ONLY: &str = "client-only-2099-01-01";
const WITHHELD: &str = "withheld-2099-01-01";
const OPERATOR_PIN: &str = "operator-pin-2099-01-01";
const CONFIG_PIN: &str = "config-pin-2099-01-01";
const DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

#[derive(Debug, Clone, Copy)]
enum Lane {
    Cloak,
    ApiKey,
    Forwarded,
    Invoke,
    Converse,
}

fn owned(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

/// The request every row sends. The router lands operator pins in the client
/// set as well as in `operator_betas`, so this request does too.
fn request() -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(2048),
        anthropic_beta: owned(&[
            STRUCTURED_OUTPUTS_BETA,
            CLIENT_ONLY,
            WITHHELD,
            OPERATOR_PIN,
            REDACT_THINKING_BETA,
            DISPLAY_UPDATES_BETA,
        ]),
        response_format: Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "widget", "schema": {"type": "object"}}
        })),
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some("medium".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    req.routectl_internal.operator_betas = owned(&[OPERATOR_PIN]);
    req.routectl_internal.withheld_betas =
        std::sync::Arc::from(owned(&[WITHHELD, OPERATOR_PIN, STRUCTURED_OUTPUTS_BETA]));
    req.routectl_internal.anthropic_thinking_display = Some("updates".into());
    req
}

fn anthropic_cfg(base_url: &str, auth_kind: AuthKind) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "beta-assembly-test".into(),
        auth: Arc::new(StaticToken::new("test-token")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind,
        header_extras: vec![("anthropic-beta".into(), CONFIG_PIN.into())],
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        mantle: None,
    }
}

fn bedrock_cfg(api_shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:beta-assembly-test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: owned(&[CONFIG_PIN]),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

/// The anthropic-api wire body the header is composed against: it carries
/// both body fields that imply a beta, and a `thinking.display` that drops
/// the redaction flag after every union.
fn anthropic_wire_body() -> Value {
    json!({
        "output_config": {"format": {"type": "json_schema"}, "effort": "medium"},
        "thinking": {"type": "adaptive", "display": "summarized"}
    })
}

fn header_betas(provider: &AnthropicApiProvider, req: &ChatRequest) -> Vec<String> {
    let rb = reqwest::Client::new().post("http://127.0.0.1/test");
    let (rb, _decision) =
        provider.build_headers(rb, req, "test-token", Some(&anthropic_wire_body()));
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .get("anthropic-beta")
        .map(|v| v.to_str().expect("ascii header").to_string())
        .unwrap_or_default()
        .split(',')
        .map(str::to_string)
        .collect()
}

fn body_betas(carrier: Option<&Value>) -> Vec<String> {
    carrier
        .and_then(Value::as_array)
        .expect("the carrier ships anthropic_beta")
        .iter()
        .map(|b| b.as_str().expect("string flag").to_string())
        .collect()
}

/// Assemble `lane`'s carrier for `req` and return the flags it ships.
fn shipped_betas(lane: Lane, req: &mut ChatRequest) -> Vec<String> {
    match lane {
        Lane::Cloak => {
            let provider =
                AnthropicApiProvider::new(anthropic_cfg(OAUTH_HOST, AuthKind::OauthBearer));
            assert!(provider.is_cloak_lane(req), "premise: the cloak lane");
            assert!(provider.is_non_cc(req), "premise: a non-CC request");
            header_betas(&provider, req)
        }
        Lane::ApiKey => {
            let provider = AnthropicApiProvider::new(anthropic_cfg(PLAIN_HOST, AuthKind::ApiKey));
            header_betas(&provider, req)
        }
        Lane::Forwarded => {
            let mut cfg = anthropic_cfg(OAUTH_HOST, AuthKind::OauthBearer);
            cfg.use_forwarded_bearer = true;
            let provider = AnthropicApiProvider::new(cfg);
            req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
                "sk-ant-oat01-forwarded".into(),
            ));
            assert!(provider.forwarded_leg(req), "premise: the forwarded leg");
            header_betas(&provider, req)
        }
        Lane::Invoke => {
            let body = crate::bedrock::invoke::normalize_request(
                &bedrock_cfg(BedrockApiShape::Invoke),
                req,
            )
            .expect("invoke body assembles");
            body_betas(body.get("anthropic_beta"))
        }
        Lane::Converse => {
            let body = crate::bedrock::converse::normalize_request(
                &bedrock_cfg(BedrockApiShape::Converse),
                req,
            )
            .expect("converse body assembles");
            body_betas(body.pointer("/additionalModelRequestFields/anthropic_beta"))
        }
    }
}

/// What [`crate::beta_withhold::body_implied_betas`] reads off `lane`'s
/// shipped carrier: the anthropic-api wire body, the Invoke body, or the
/// Converse bag.
fn union_implied(lane: Lane, req: &ChatRequest) -> Vec<String> {
    use crate::beta_withhold::{BetaLane, body_implied_betas};
    let (carrier, beta_lane) = match lane {
        Lane::Cloak => (anthropic_wire_body(), BetaLane::AnthropicApiCloak),
        Lane::ApiKey => (anthropic_wire_body(), BetaLane::AnthropicApi),
        Lane::Forwarded => (anthropic_wire_body(), BetaLane::AnthropicApiForwarded),
        Lane::Invoke => (
            crate::bedrock::invoke::normalize_request(&bedrock_cfg(BedrockApiShape::Invoke), req)
                .expect("invoke body assembles"),
            BetaLane::BedrockInvoke,
        ),
        Lane::Converse => (
            crate::bedrock::converse::normalize_request(
                &bedrock_cfg(BedrockApiShape::Converse),
                req,
            )
            .expect("converse body assembles")["additionalModelRequestFields"]
                .clone(),
            BetaLane::BedrockConverse,
        ),
    };
    let carrier = carrier.as_object().expect("an object carrier");
    owned(&body_implied_betas(carrier, beta_lane))
}

/// The cloak lane's floor: the config and operator pins, then the OAuth gate
/// and the minted floor, minus the redaction flag the display body drops.
fn cloak_floor() -> Vec<String> {
    let mut floor = owned(&[CONFIG_PIN, OPERATOR_PIN, OAUTH_ANTHROPIC_BETA]);
    for flag in default_claude_code_anthropic_betas() {
        if *flag != REDACT_THINKING_BETA && !floor.iter().any(|f| f == flag) {
            floor.push((*flag).to_string());
        }
    }
    floor
}

#[test]
fn each_carrier_deposits_the_floor_and_body_implied_sets_it_ships() {
    // (lane, expected floor, expected body-implied)
    let rows: [(Lane, Vec<String>, Vec<String>); 5] = [
        (
            Lane::Cloak,
            cloak_floor(),
            owned(&[STRUCTURED_OUTPUTS_BETA, EFFORT_BETA]),
        ),
        (
            Lane::ApiKey,
            owned(&[CONFIG_PIN, OPERATOR_PIN]),
            owned(&[STRUCTURED_OUTPUTS_BETA]),
        ),
        (
            Lane::Forwarded,
            owned(&[CONFIG_PIN, OPERATOR_PIN]),
            Vec::new(),
        ),
        (
            Lane::Invoke,
            owned(&[CONFIG_PIN, OPERATOR_PIN]),
            owned(&[STRUCTURED_OUTPUTS_BETA]),
        ),
        (
            Lane::Converse,
            owned(&[CONFIG_PIN, OPERATOR_PIN]),
            owned(&[DISPLAY_UPDATES_BETA, STRUCTURED_OUTPUTS_BETA]),
        ),
    ];

    for (lane, floor, body_implied) in rows {
        let mut req = request();
        let slot = BetaAssemblyReport::default();
        req.routectl_internal.beta_assembly = Some(slot.clone());

        let shipped = shipped_betas(lane, &mut req);
        let deposited = slot.take();

        assert_eq!(
            deposited,
            Some(BetaAssembly::new(floor, body_implied)),
            "{lane:?}: deposited sets; shipped {shipped:?}"
        );
        let deposited = deposited.expect("asserted above");
        assert_eq!(
            deposited.body_implied(),
            union_implied(lane, &req),
            "{lane:?}: the deposit is the union's own implied set"
        );
        for flag in deposited.floor().iter().chain(deposited.body_implied()) {
            assert!(
                shipped.contains(flag),
                "{lane:?}: deposited {flag} is not on the wire {shipped:?}"
            );
        }
        let forwards_verbatim = matches!(lane, Lane::Forwarded);
        assert_eq!(
            shipped.iter().any(|f| f == WITHHELD),
            forwards_verbatim,
            "{lane:?}: premise: the withhold ran everywhere but the forwarded leg"
        );
    }
}

#[test]
fn a_carrier_with_no_slot_records_nothing_and_ships_the_same_flags() {
    for lane in [
        Lane::Cloak,
        Lane::ApiKey,
        Lane::Forwarded,
        Lane::Invoke,
        Lane::Converse,
    ] {
        let mut listening = request();
        listening.routectl_internal.beta_assembly = Some(BetaAssemblyReport::default());
        let mut silent = request();

        assert_eq!(
            shipped_betas(lane, &mut listening),
            shipped_betas(lane, &mut silent),
            "{lane:?}: the slot changes no wire byte"
        );
    }
}
