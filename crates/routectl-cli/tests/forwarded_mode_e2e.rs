//! End-to-end HTTP coverage for `first-party-passthrough.f2.11` -- the
//! validator-facing feature gate for forwarded (pure-proxy) mode.
//!
//! Every test here drives a REAL axum server over a REAL TCP loopback
//! connection (`helpers::spawn`, same pattern as `anthropic_ingress.rs`),
//! not a direct in-process call into `ingress_handle`. That is the coverage
//! gap this file closes: the ADMISSION matrix, the ROUTER refuse, and
//! backward-compat were already pinned at the function-call level
//! (`ingress_handle_tests.rs`, `pure_proxy_admission_log.rs`,
//! `routectl-router`'s in-lib forwarded-gate tests) and at the log-capture
//! level (`forwarded_gate_log.rs`, `forwarded_auth_terminal_log.rs`), but
//! nothing yet proved the real axum route wiring (JSON extraction, header
//! parsing, listener auth interaction, TCP round trip) actually reaches
//! those gates.
//!
//! CONSTRAINT (see the task brief): the WIRE + ROUTER gates pin the
//! forwarded egress to EXACTLY host `api.anthropic.com`. A wiremock
//! upstream's host is never that, so it can serve as an upstream ONLY for
//! scenarios that must never reach it (the ADMISSION matrix rejects before
//! dispatch; the ROUTER gate refuses before dispatch) or for OWN-mode /
//! absent-`[mitm]` requests (which are not subject to the host pin at all).
//! A genuine HTTP round trip of the forwarded HAPPY path or the forwarded
//! TERMINAL 401/403/429 behavior against a real `AnthropicApiProvider` is
//! therefore not attempted here -- that composition is proven at the
//! component level: `AnthropicApiProvider::build_headers` /
//! `resolve_effective_token` unit tests in
//! `routectl-providers/src/anthropic_api/mod.rs` (host-pinned WIRE gate +
//! IDENTITY override) and `Router::complete` / `stream` / `count_tokens`
//! unit tests in `routectl-router/src/router.rs` (TERMINAL 401/403/429,
//! ROUTER refuse, forwarded-egress-dispatches-normally), which together
//! already compose the full pipeline without ever needing to reach the
//! real Anthropic host.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use routectl_router::config::CredentialSource;
use routectl_router::{
    AliasValue, Config, MitmConfig, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

mod helpers {
    use std::sync::Arc;

    use routectl_router::Config;
    use tokio::net::TcpListener;

    pub async fn spawn(config: Arc<Config>) -> String {
        let config = crate::common::isolate_usage_db(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            routectl_cli::server::serve_on_listener(config, listener, None)
                .await
                .expect("server failed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        base_url
    }
}

const MITM_SEAM_HEADER: &str = "x-routectl-mitm-proxied";
const SESSION_ID_HEADER: &str = "x-claude-code-session-id";

/// A fresh, per-call cert dir under the process tempdir. `[mitm]` config
/// startup (triggered as a side effect of `Config.mitm` being `Some`, even
/// though these tests never talk to the MITM front-proxy listener) mints a
/// local CA + leaf cert on disk; a unique dir per test avoids cross-test
/// collisions and keeps the real `~/.config/routectl` untouched. Leaked
/// deliberately (matches `common::isolate_usage_db`'s rationale): the
/// spawned server outlives the test function and is never awaited to
/// shutdown, so a scoped `TempDir` guard could delete the path out from
/// under a still-running MITM cert-generation task.
fn unique_cert_dir(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "routectl-mitm-e2e-{}-{tag}-{n}",
        std::process::id()
    ))
}

/// `[mitm]` block with the given `credential_source`. `listen_port: 0`
/// (OS-assigned) avoids colliding with the sentinel `[server] port` these
/// tests use (see `base_server_config`); the pinned `upstream_origin` /
/// `mitm_host` defaults are left untouched (`validate_mitm_config` requires
/// them exact).
fn mitm_config(credential_source: CredentialSource) -> MitmConfig {
    MitmConfig {
        credential_source,
        listen_port: 0,
        cert_dir: unique_cert_dir(match credential_source {
            CredentialSource::Forwarded => "forwarded",
            CredentialSource::Own => "own",
        }),
        ..Default::default()
    }
}

/// `[server]` block. `port: 1` is a dummy sentinel: `helpers::spawn` binds
/// the REAL listener via `TcpListener::bind("127.0.0.1:0")` independently
/// of this field, so its only job here is to differ from `mitm.listen_port`
/// (0) so `validate_mitm_config`'s collision check passes.
fn base_server_config() -> ServerConfig {
    ServerConfig {
        host: "127.0.0.1".into(),
        port: 1,
        auth: None,
        strict_translation: false,
        allow_disable_fallbacks: true,
        ..Default::default()
    }
}

fn anthropic_response_body() -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    })
}

fn forwarded_admission_body() -> Value {
    json!({
        "model": "heavy",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

// ---------------------------------------------------------------------------
// ADMISSION rejection matrix -- real HTTP, no upstream involved (every case
// rejects BEFORE dispatch). Zero `[providers]` triggers the zero-config
// synthetic-egress injection (`inject_synthetic_anthropic_egress_if_needed`),
// which is the realistic zero-config forwarded-mode shape and never gets
// far enough to touch it on any of these paths.
// ---------------------------------------------------------------------------

fn forwarded_zero_providers_config() -> Arc<Config> {
    Arc::new(Config {
        server: base_server_config(),
        mitm: Some(mitm_config(CredentialSource::Forwarded)),
        retry: RetryPolicy::default(),
        ..Default::default()
    })
}

/// Case 1 (token_missing, 401): MITM-marked + a session id, but no inbound
/// `Authorization` -- Claude Code not logged into claude.ai.
#[tokio::test]
async fn e2e_admission_rejects_token_missing_401() {
    let base = helpers::spawn(forwarded_zero_providers_config()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header(SESSION_ID_HEADER, "sess-token-missing")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "Anthropic envelope");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reason=token_missing"),
        "body: {body}"
    );
}

/// Case 2 (not_mitm, 400): a bearer + session id, but no MITM seam header --
/// a direct :9100 loopback client, not a valid pure-proxy path.
#[tokio::test]
async fn e2e_admission_rejects_not_mitm_400() {
    let base = helpers::spawn(forwarded_zero_providers_config()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("authorization", "Bearer sk-ant-oat01-e2e-not-mitm")
        .header(SESSION_ID_HEADER, "sess-not-mitm")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reason=not_mitm"),
        "body: {body}"
    );
}

/// Case 3 (identity_missing, 400): MITM-marked + a bearer, but no
/// `x-claude-code-session-id`. The strongest leak probe of the matrix: it
/// carries a real-looking token, so pin that the token never appears
/// anywhere in the HTTP response.
#[tokio::test]
async fn e2e_admission_rejects_identity_missing_400_and_never_leaks_token() {
    const TOKEN: &str = "sk-ant-oat01-E2E-LEAK-CANARY-identity-missing";
    let base = helpers::spawn(forwarded_zero_providers_config()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reason=identity_missing"),
        "body: {body}"
    );
    assert!(
        !body.to_string().contains(TOKEN),
        "the forwarded token must never appear in the HTTP response: {body}"
    );
}

/// Case 4a (non_anthropic_dialect, 400): a fully-formed forwarded request
/// (seam + bearer + session id -- every OTHER admission key satisfied) is
/// still rejected on `/v1/chat/completions`, proving the dialect check is
/// reachable through the REAL OpenAI chat-completions route, not only via a
/// direct `enforce_pure_proxy_admission` call with a synthesized envelope
/// shape.
#[tokio::test]
async fn e2e_admission_rejects_non_anthropic_dialect_chat_completions_400() {
    let base = helpers::spawn(forwarded_zero_providers_config()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", "Bearer sk-ant-oat01-e2e-dialect")
        .header(SESSION_ID_HEADER, "sess-dialect-chat")
        .json(&json!({
            "model": "heavy",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("type").is_none(),
        "OpenAI envelope is flat (no top-level type): {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reason=non_anthropic_dialect"),
        "body: {body}"
    );
}

/// Case 4b: same rejection, reached through the REAL `/v1/responses` route
/// -- the second non-Anthropic dialect the shared driver must also gate.
#[tokio::test]
async fn e2e_admission_rejects_non_anthropic_dialect_responses_400() {
    let base = helpers::spawn(forwarded_zero_providers_config()).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", "Bearer sk-ant-oat01-e2e-dialect-responses")
        .header(SESSION_ID_HEADER, "sess-dialect-responses")
        .json(&json!({"model": "heavy", "input": "hi"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("type").is_none(),
        "OpenAI envelope is flat: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reason=non_anthropic_dialect"),
        "body: {body}"
    );
}

/// A well-formed forwarded request (seam + bearer + session id, Anthropic
/// dialect) is ADMITTED past the gate -- proven by a distinct rejection
/// downstream (ROUTER refuse) rather than the admission 401/400 shapes,
/// since dispatch to a real Anthropic host cannot happen in this suite (see
/// the module-level constraint note). Confirms the matrix's ADMIT branch is
/// reachable end-to-end, not just refused branches.
#[tokio::test]
async fn e2e_admission_admits_fully_valid_forwarded_request_past_the_gate() {
    let config = forwarded_config_with_non_anthropic_provider("http://127.0.0.1:1");
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", "Bearer sk-ant-oat01-e2e-admitted")
        .header(SESSION_ID_HEADER, "sess-admitted")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    // Admission passed the request through to the ROUTER gate, which then
    // refuses it (400 non_anthropic_target) -- a DIFFERENT reason than any
    // admission-matrix rejection, proving the request cleared admission.
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("non_anthropic_target"),
        "expected the ROUTER-gate refuse reason (proving admission passed), got: {body}"
    );
}

// ---------------------------------------------------------------------------
// ROUTER gate refuse -- real HTTP, resolved target is NOT an
// api.anthropic.com anthropic-api egress. Refused before ANY upstream
// dispatch; a mounted wiremock upstream must see zero requests.
// ---------------------------------------------------------------------------

fn forwarded_config_with_non_anthropic_provider(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "compat-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), "literal:test-key"),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "heavy-model".to_string(),
        ModelEntry::new("compat-mock", "some-model"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "heavy".to_string(),
        AliasValue::Single("heavy-model".into()),
    );

    Arc::new(Config {
        server: base_server_config(),
        mitm: Some(mitm_config(CredentialSource::Forwarded)),
        providers,
        aliases,
        models,
        retry: RetryPolicy::default(),
        ..Default::default()
    })
}

#[tokio::test]
async fn e2e_router_refuses_forwarded_request_to_non_anthropic_target_upstream_never_called() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&upstream)
        .await;

    let config = forwarded_config_with_non_anthropic_provider(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", "Bearer sk-ant-oat01-e2e-router-refuse")
        .header(SESSION_ID_HEADER, "sess-router-refuse")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "Anthropic envelope");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("non_anthropic_target"),
        "body: {body}"
    );

    let received = upstream.received_requests().await.unwrap();
    assert!(
        received.is_empty(),
        "the ROUTER gate must refuse BEFORE any upstream dispatch; wiremock saw: {received:?}"
    );
}

// ---------------------------------------------------------------------------
// Backward compat -- own mode and absent-[mitm] mode behave byte-for-byte
// pre-f2, INCLUDING when a client sends forwarded-style headers a curious
// or misconfigured client might send. Real HTTP against a wiremock upstream
// standing in for api.anthropic.com; the host pin does not apply because
// neither path ever sets `forwarded_bearer` (config-is-the-capability).
// ---------------------------------------------------------------------------

fn backward_compat_config(upstream_base: &str, mitm: Option<MitmConfig>) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic-mock".to_string(),
        ProviderEntry::anthropic_api("literal:test-key").with_base_url(upstream_base.to_string()),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "haiku".to_string(),
        ModelEntry::new("anthropic-mock", "claude-haiku-4-5"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("heavy".to_string(), AliasValue::Single("haiku".into()));

    Arc::new(Config {
        server: base_server_config(),
        mitm,
        providers,
        aliases,
        models,
        retry: RetryPolicy::default(),
        ..Default::default()
    })
}

/// The distinctive rogue bearer a curious (or misconfigured) client sends
/// alongside the MITM seam header even though the server is NOT in
/// forwarded mode. Neither backward-compat test's upstream assertion
/// should ever see this value: own mode's `credential_source == Own`, and
/// the absent-`[mitm]` path has no forwarded-capture gate to arm at all.
const ROGUE_CLIENT_BEARER: &str = "sk-ant-oat01-ROGUE-CLIENT-BEARER-must-never-reach-upstream";

async fn assert_backward_compat_round_trip(mitm: Option<MitmConfig>) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_response_body()))
        .mount(&upstream)
        .await;

    let config = backward_compat_config(&upstream.uri(), mitm);
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", format!("Bearer {ROGUE_CLIENT_BEARER}"))
        .header(SESSION_ID_HEADER, "sess-backward-compat")
        .json(&json!({
            "model": "heavy",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "a non-forwarded config must serve normally even with forwarded-style headers present"
    );

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "exactly one upstream dispatch");
    // The wiremock matcher above already pinned x-api-key=test-key; this is
    // an additional explicit scan proving the rogue client bearer never
    // rides ANY outbound header or the body.
    for (name, value) in &received[0].headers {
        assert_ne!(
            value.to_str().unwrap_or_default(),
            format!("Bearer {ROGUE_CLIENT_BEARER}"),
            "header {name} must not carry the rogue client bearer"
        );
    }
    assert!(
        !String::from_utf8_lossy(&received[0].body).contains(ROGUE_CLIENT_BEARER),
        "the rogue client bearer must not leak into the outbound body"
    );
}

/// Own mode (`[mitm] credential_source = "own"`, present explicitly):
/// forwarded-style headers on the inbound request are inert.
#[tokio::test]
async fn e2e_own_mode_backward_compat_ignores_forwarded_style_headers() {
    assert_backward_compat_round_trip(Some(mitm_config(CredentialSource::Own))).await;
}

/// Absent `[mitm]` block entirely (the pre-f1 shape, and still the default
/// for any deployment that never opts into the MITM feature): the same
/// forwarded-style headers are equally inert, because there is no
/// `[mitm]` block to arm the capture gate's config-is-the-capability half.
#[tokio::test]
async fn e2e_absent_mitm_backward_compat_ignores_forwarded_style_headers() {
    assert_backward_compat_round_trip(None).await;
}
