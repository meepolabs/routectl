//! Fingerprint-coherence end-to-end: with `codex_version` set in config, the
//! SAME version reaches every codex fingerprint surface -- the egress request
//! User-Agent (leg a), the egress `version` identity header (leg b), and the
//! OAuth refresh client's User-Agent (leg c). All three derive from ONE
//! process-global resolved identity, so a single factory install pins them
//! together; a drift between any two is the exact failure the 2026-06
//! incident punished.
//!
//! The provider is built through the REAL factory path
//! (`build_resolved_models`), the same boundary `serve` / `routectl test` /
//! reload route through, so the assertion covers the production wiring rather
//! than a hand-built config.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its own
//! test binary with a SINGLE test.

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::identity::codex::codex_user_agent;
use routectl_router::{BuildOptions, Config, build_resolved_models};
use routectl_testkit::ScopedEnv;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A distinctive version that could never be the pinned default, so an
/// assertion that finds it on the wire proves the operator knob (not the
/// baked-in pin) reached every surface.
const CODEX_VERSION: &str = "7.7.7-coherence";

/// A minimal `response.completed` SSE body; `complete()` forces
/// `stream:true` and drains SSE until this terminal event lands.
fn completed_sse() -> String {
    let completed = serde_json::json!({
        "id": "resp_coherence",
        "object": "response",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "pong"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed).unwrap()
    )
}

fn base_req() -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

#[tokio::test]
#[serial_test::serial]
async fn configured_codex_version_pins_ua_version_header_and_refresh_ua() {
    // A temp XDG dir so the chatgpt-oauth installation-id resolution mints
    // under the temp config dir, never the real ~/.config/routectl.
    let xdg = tempfile::tempdir().expect("temp xdg");
    let _xdg_guard = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    // A static bearer + account id via env refs so `build_resolved_models`
    // resolves without an OAuthStore. `auth_kind` stays chatgpt-oauth, so
    // the egress still emits the full codex fingerprint (originator /
    // residency / version).
    let _tok = ScopedEnv::set("ROUTECTL_CID_TOKEN", "test-jwt");
    let _acct = ScopedEnv::set("ROUTECTL_CID_ACCT", "acct-uuid");

    // Wiremock upstream: capture the egress request headers, return a
    // minimal completed SSE.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&server)
        .await;

    // Build through the REAL factory path. The codex identity install runs
    // at the START of build_resolved_models from `codex_version`, before any
    // provider construction.
    let config: Config = toml::from_str(&format!(
        "[providers.cx]\nkind = \"openai-responses\"\nauth_kind = \"chatgpt-oauth\"\n\
         api_key_ref = \"env://ROUTECTL_CID_TOKEN\"\n\
         account_id_ref = \"env://ROUTECTL_CID_ACCT\"\n\
         base_url = \"{}\"\ncodex_version = \"{CODEX_VERSION}\"\n\
         [models.m]\nprovider = \"cx\"\nupstream = \"gpt-5-codex\"\n",
        server.uri(),
    ))
    .expect("fixture parses");
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (models, failed) = build_resolved_models(&config, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(
        failed.is_empty(),
        "no model should fail to build: {failed:?}"
    );
    let resolved = models.get("m").expect("model m resolved");

    // Egress leg: one completion so the wiremock captures the headers.
    resolved
        .provider
        .complete(base_req())
        .await
        .expect("complete against wiremock");

    let received = server.received_requests().await.expect("captured requests");
    assert_eq!(received.len(), 1, "exactly one egress request");
    let headers = &received[0].headers;

    // Leg (a): the egress request User-Agent carries the configured version.
    let ua = headers
        .get("user-agent")
        .expect("egress request must carry a User-Agent")
        .to_str()
        .unwrap();
    assert!(
        ua.contains(CODEX_VERSION),
        "egress UA must carry the configured version: {ua}",
    );

    // Leg (b): the egress `version` identity header equals the configured
    // version exactly.
    let version_header = headers
        .get("version")
        .expect("egress request must carry a version header")
        .to_str()
        .unwrap();
    assert_eq!(
        version_header, CODEX_VERSION,
        "egress version header must equal the configured version",
    );

    // Leg (c): the OAuth refresh client stamps `codex_user_agent()` on its
    // token-endpoint POST -- see routectl-auth oauth/providers/codex.rs
    // `codex_identity`, whose own unit test
    // `codex_identity_stamps_user_agent_originator_and_residency` pins that
    // the refresh request builder's User-Agent IS `codex_user_agent()`.
    // Driving the full refresh POST in-process here is disproportionate: the
    // codex flow pins `TOKEN_URL` to a const, so it cannot be redirected to a
    // wiremock token endpoint without changing production code. This test
    // instead closes the loop the auth unit test leaves open -- that under a
    // CONFIGURED version the value the refresh client stamps
    // (`codex_user_agent()`) carries that version AND is byte-identical to the
    // UA the egress just put on the wire. Composed with the auth unit test,
    // that transitively pins the refresh leg at the configured version.
    let refresh_ua = codex_user_agent();
    assert!(
        refresh_ua.contains(CODEX_VERSION),
        "the UA the refresh client stamps must carry the configured version: {refresh_ua}",
    );
    assert_eq!(
        ua, refresh_ua,
        "the egress UA on the wire and the refresh-client UA are byte-identical -- \
         one fingerprint across every surface",
    );
}
