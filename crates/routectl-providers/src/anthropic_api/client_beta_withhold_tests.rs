//! The router's withheld client betas leave the `anthropic-beta` header,
//! except a flag this egress itself asserts (provider and operator pins, the
//! OAuth gate, the minted Claude Code floor, the body-implied unions), and
//! never on the forwarded leg.
//!
//! Every case composes through `build_headers` and asserts the whole header
//! string, so order is pinned along with membership.

use super::*;
use routectl_core::identity::anthropic::{
    OAUTH_ANTHROPIC_BETA, STRUCTURED_OUTPUTS_BETA, default_claude_code_anthropic_betas,
};
use routectl_core::{ChatRequest, StaticToken};

const PLAIN_HOST: &str = "http://127.0.0.1:18080";
const OAUTH_HOST: &str = "https://api.anthropic.com";
const PROVIDER_ID: &str = "beta-withhold-test";
const NEVER: &str = "zz-never-send-2099-01-01";
const KEPT: &str = "kept-2099-01-01";
const OTHER: &str = "other-2099-01-01";
const CC_FLAG: &str = "claude-code-20250219";
const INTERLEAVED: &str = "interleaved-thinking-2025-05-14";

fn cfg(base_url: &str, auth_kind: AuthKind) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: PROVIDER_ID.into(),
        auth: Arc::new(StaticToken::new("test-token")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

fn owned(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

fn req(client: &[&str], withheld: &[&str]) -> ChatRequest {
    let mut req = ChatRequest {
        anthropic_beta: owned(client),
        ..Default::default()
    };
    req.routectl_internal.withheld_betas = std::sync::Arc::from(owned(withheld));
    req
}

fn header(provider: &AnthropicApiProvider, req: &ChatRequest, body: Option<&Value>) -> String {
    let rb = reqwest::Client::new().post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, req, "test-token", body);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .get("anthropic-beta")
        .map(|v| v.to_str().expect("ascii header").to_string())
        .unwrap_or_default()
}

fn plain_provider() -> AnthropicApiProvider {
    AnthropicApiProvider::new(cfg(PLAIN_HOST, AuthKind::ApiKey))
}

/// `base` followed by every minted floor flag `base` does not already hold,
/// in floor order: the header a non-CC cloak-lane request composes.
fn with_cc_floor(base: &[&str]) -> String {
    let mut out = owned(base);
    for flag in default_claude_code_anthropic_betas() {
        if !out.iter().any(|f| f == flag) {
            out.push((*flag).to_string());
        }
    }
    out.join(",")
}

#[test]
fn withheld_flag_outside_the_floor_is_dropped() {
    let provider = plain_provider();
    let req = req(&[KEPT, NEVER, OTHER], &[NEVER]);

    let events = routectl_testkit::capture_events(|| {
        assert_eq!(header(&provider, &req, None), format!("{KEPT},{OTHER}"));
    });

    let drops: Vec<_> = events
        .iter()
        .filter(|e| e.message == "dropping beta flag withheld for this lane")
        .collect();
    assert_eq!(
        drops.len(),
        1,
        "one debug line per withheld flag: {events:?}"
    );
    assert_eq!(drops[0].level, tracing::Level::DEBUG);
    assert_eq!(drops[0].field("flag"), Some(NEVER));
    assert_eq!(drops[0].field("provider"), Some(PROVIDER_ID));
}

#[test]
fn withheld_flag_pinned_through_operator_betas_keeps_its_client_position() {
    let provider = plain_provider();
    let mut req = req(&[KEPT, NEVER, OTHER], &[NEVER]);
    req.routectl_internal.operator_betas = owned(&[NEVER]);

    assert_eq!(
        header(&provider, &req, None),
        format!("{KEPT},{NEVER},{OTHER}")
    );
}

#[test]
fn withheld_flag_pinned_through_provider_header_extras_is_kept() {
    let mut config = cfg(PLAIN_HOST, AuthKind::ApiKey);
    config.header_extras = vec![("anthropic-beta".into(), format!(" {NEVER} "))];
    let provider = AnthropicApiProvider::new(config);
    let req = req(&[KEPT, NEVER, OTHER], &[NEVER]);

    assert_eq!(
        header(&provider, &req, None),
        format!("{KEPT},{NEVER},{OTHER}")
    );
}

#[test]
fn withheld_minted_floor_flags_still_ship_on_a_non_cc_cloak_request() {
    let provider = AnthropicApiProvider::new(cfg(OAUTH_HOST, AuthKind::OauthBearer));
    let req = req(
        &[CC_FLAG, NEVER, OAUTH_ANTHROPIC_BETA],
        &[CC_FLAG, NEVER, OAUTH_ANTHROPIC_BETA],
    );
    assert!(provider.is_non_cc(&req), "premise: no CC session capture");

    assert_eq!(
        header(&provider, &req, None),
        with_cc_floor(&[CC_FLAG, OAUTH_ANTHROPIC_BETA])
    );
}

#[test]
fn withheld_oauth_gate_ships_on_a_genuine_cc_request_while_other_flags_drop() {
    let provider = AnthropicApiProvider::new(cfg(OAUTH_HOST, AuthKind::OauthBearer));
    let mut req = req(
        &[OAUTH_ANTHROPIC_BETA, INTERLEAVED, NEVER, KEPT],
        &[OAUTH_ANTHROPIC_BETA, INTERLEAVED, NEVER],
    );
    req.routectl_internal.claude_code_headers =
        vec![("x-claude-code-session-id".into(), "sess-1".into())];
    assert!(!provider.is_non_cc(&req), "premise: a genuine CC request");

    // The gate keeps its client position ahead of the kept flag; re-added by
    // the gate union instead, it would trail it.
    assert_eq!(
        header(&provider, &req, None),
        format!("{OAUTH_ANTHROPIC_BETA},{KEPT}")
    );
}

#[test]
fn withheld_body_implied_flag_is_re_added_by_the_union() {
    let provider = plain_provider();
    let req = req(&[STRUCTURED_OUTPUTS_BETA, KEPT], &[STRUCTURED_OUTPUTS_BETA]);
    let body = serde_json::json!({"output_config": {"format": {"type": "json_schema"}}});

    assert_eq!(
        header(&provider, &req, Some(&body)),
        format!("{KEPT},{STRUCTURED_OUTPUTS_BETA}")
    );
}

#[test]
fn forwarded_leg_ships_every_client_flag() {
    let mut config = cfg(OAUTH_HOST, AuthKind::OauthBearer);
    config.use_forwarded_bearer = true;
    let provider = AnthropicApiProvider::new(config);
    let mut req = req(&[KEPT, NEVER, OTHER], &[NEVER]);
    req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
        "sk-ant-oat01-forwarded".into(),
    ));
    assert!(provider.forwarded_leg(&req), "premise: the forwarded leg");

    assert_eq!(
        header(&provider, &req, None),
        format!("{KEPT},{NEVER},{OTHER}")
    );
}

#[test]
fn empty_withheld_set_leaves_the_header_unchanged() {
    let provider = plain_provider();
    let req = req(&[KEPT, NEVER, OTHER, KEPT], &[]);

    assert_eq!(
        header(&provider, &req, None),
        format!("{KEPT},{NEVER},{OTHER}")
    );
}
