//! End-to-end HTTP coverage for the validator-facing feature gate for
//! forwarded (pure-proxy) mode.
//!
//! Every test here drives a REAL axum server over a REAL TCP loopback
//! connection (`helpers::spawn`, same pattern as `anthropic_ingress.rs`),
//! not a direct in-process call into `ingress_handle`. That is the coverage
//! gap this file closes: the ADMISSION matrix, coexistence dissolution, and
//! backward-compat were already pinned at the function-call level
//! (`ingress_handle_tests.rs`, `pure_proxy_admission_log.rs`,
//! `routectl-router`'s in-lib coexistence + missing-bearer tests) and at the
//! log-capture level (`forwarded_auth_terminal_log.rs`), but nothing yet
//! proved the real axum route wiring (JSON extraction, header parsing,
//! listener auth interaction, TCP round trip) actually reaches those gates.
//!
//! CONSTRAINT (see the task brief): the WIRE gate pins a forwarded-
//! CREDENTIAL egress to EXACTLY host `api.anthropic.com`. A wiremock
//! upstream's host is never that, so it can serve as an upstream ONLY for
//! scenarios that must never reach an Anthropic egress at all (the ADMISSION
//! matrix rejects before dispatch; a non-Anthropic OWN-credential provider is
//! never subject to the host pin) or for OWN-mode / absent-`[mitm]` requests.
//! A genuine HTTP round trip of the forwarded-credential HAPPY path or the
//! forwarded TERMINAL 401/403/429 behavior against a real
//! `AnthropicApiProvider` is therefore not attempted here -- that
//! composition is proven at the component level:
//! `AnthropicApiProvider::build_headers` / `resolve_effective_token` unit
//! tests in `routectl-providers/src/anthropic_api/mod.rs` (host-pinned WIRE
//! gate + IDENTITY override) and `Router::complete` / `stream` /
//! `count_tokens` unit tests in `routectl-router/src/router.rs` (TERMINAL
//! 401/403/429, missing-bearer terminal guard, coexistence dissolved),
//! which together already compose the full pipeline without ever needing to
//! reach the real Anthropic host.

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

/// `[mitm]` block for a test scenario tagged `scenario` (used only to keep
/// each scenario's cert dir distinct -- `[mitm]` itself is transport-only
/// post-f3, forwardedness now lives on the `[providers.X]` entry, not
/// here). `listen_port: 0` (OS-assigned) avoids colliding with the
/// sentinel `[server] port` these tests use (see `base_server_config`);
/// the pinned `upstream_origin` / `mitm_host` defaults are left untouched
/// (`validate_mitm_config` requires them exact).
fn mitm_config(scenario: &str) -> MitmConfig {
    MitmConfig {
        listen_port: 0,
        cert_dir: unique_cert_dir(scenario),
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
// rejects BEFORE dispatch).
// ---------------------------------------------------------------------------

/// Case 1 (adapted for the seam-nonce hardening): the seam header is now
/// unspoofable -- it is authoritative only when its value matches the
/// server's per-process nonce, which no direct HTTP client can ever learn
/// or supply. A client that sends the header with ANY value (here the old
/// literal `"1"`) must downgrade to seam-ABSENT: the token_missing
/// admission rejection never fires, and a request with no bearer and no
/// session id is admitted and dispatches exactly like ordinary
/// own-provider traffic, even while a forwarded provider coexists.
#[tokio::test]
async fn e2e_admission_spoofed_seam_header_downgrades_to_seam_absent() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = config_with_forwarded_and_own_providers(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "a spoofed seam header (no matching process nonce) must never trigger the \
         token_missing admission rejection -- the request is admitted as ordinary \
         own-provider traffic"
    );
}

/// Case 2 (migrated for f3): a bearer + session id, but no MITM seam
/// header -- a direct :9100 loopback client -- used to 400 as `not_mitm`
/// whenever `[mitm] credential_source = forwarded` was set. f3 dropped that
/// request-global rejection: the pre-parse gate now fires ONLY when the
/// seam header IS present, so this exact traffic shape must be ADMITTED and
/// dispatch normally to its own-credential provider, even while a forwarded
/// provider coexists in `[providers]` (`has_forwarded_provider() == true`).
#[tokio::test]
async fn e2e_admission_admits_own_provider_traffic_without_seam_header() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = config_with_forwarded_and_own_providers(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("authorization", "Bearer sk-ant-oat01-e2e-no-seam")
        .header(SESSION_ID_HEADER, "sess-no-seam")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "an own-provider request without the seam header must be admitted \
         even while a forwarded provider is configured"
    );
}

/// Case 4a (migrated for f3): the SAME "no seam -> admitted" contract,
/// driven through the REAL `/v1/chat/completions` route. f3 dropped the
/// `non_anthropic_dialect` rejection along with `not_mitm` -- both fired on
/// the removed request-global forwarded flag -- so a non-Anthropic-dialect
/// request without the seam header must be admitted too, proving the
/// pre-parse gate no longer discriminates by dialect.
#[tokio::test]
async fn e2e_admission_admits_non_anthropic_dialect_without_seam_header() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = config_with_forwarded_and_own_providers(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("authorization", "Bearer sk-ant-oat01-e2e-dialect-no-seam")
        .header(SESSION_ID_HEADER, "sess-dialect-no-seam")
        .json(&json!({
            "model": "heavy",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "non-Anthropic-dialect traffic without the seam header must be \
         admitted even while a forwarded provider is configured"
    );
}

/// Case 3 (adapted for the seam-nonce hardening): the OLD identity_missing
/// rejection required only that the header be present, which a spoofed
/// value could trigger. Post-hardening, a spoofed value downgrades to
/// seam-ABSENT, so a bearer-carrying request with no session id is admitted
/// and dispatches normally instead of being rejected -- and, as the
/// strongest leak probe, the distinctive bearer must never appear anywhere
/// in the response or reach the upstream (it is never captured at all: the
/// forwarded-bearer gate also requires the nonce match).
#[tokio::test]
async fn e2e_admission_spoofed_seam_header_with_bearer_downgrades_to_seam_absent() {
    const TOKEN: &str = "sk-ant-oat01-E2E-LEAK-CANARY-identity-missing";
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = config_with_forwarded_and_own_providers(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "a spoofed seam header must never trigger the identity_missing admission rejection \
         either"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        !body.to_string().contains(TOKEN),
        "the bearer must never leak into the response: {body}"
    );
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "exactly one upstream dispatch");
    assert!(
        !String::from_utf8_lossy(&received[0].body).contains(TOKEN),
        "the bearer must never leak into the outbound request either"
    );
}

/// A well-formed forwarded-marked request (seam + bearer + session id,
/// Anthropic dialect) is ADMITTED past the admission matrix and then
/// routes exactly like any other request: f3 dissolved the f2 whole-
/// chain ROUTER gate, so a captured bearer no longer bends routing --
/// the alias resolves to its configured (non-Anthropic, OWN-credential)
/// provider and dispatch proceeds normally, reaching the real upstream
/// with that provider's own credentials. See `assert_backward_compat_round_trip`
/// for the header-level proof that the captured bearer never rides the
/// outbound request.
#[tokio::test]
async fn e2e_forwarded_capture_present_but_alias_routes_to_own_credential_provider_normally() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_compat_response_body()))
        .mount(&upstream)
        .await;

    let config = forwarded_config_with_non_anthropic_provider(&upstream.uri());
    let base = helpers::spawn(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header(MITM_SEAM_HEADER, "1")
        .header("authorization", format!("Bearer {ROGUE_CLIENT_BEARER}"))
        .header(SESSION_ID_HEADER, "sess-admitted")
        .json(&forwarded_admission_body())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        200,
        "admission passed and the non-Anthropic provider dispatched normally",
    );

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "exactly one upstream dispatch");
    for (name, value) in &received[0].headers {
        assert_ne!(
            value.to_str().unwrap_or_default(),
            format!("Bearer {ROGUE_CLIENT_BEARER}"),
            "header {name} must not carry the captured forwarded bearer -- \
             this provider is OWN-credential and must use its own api_key_ref",
        );
    }
    assert!(
        !String::from_utf8_lossy(&received[0].body).contains(ROGUE_CLIENT_BEARER),
        "the captured forwarded bearer must not leak into the outbound body",
    );
}

// ---------------------------------------------------------------------------
// Coexistence dissolved (f3): a captured forwarded bearer no longer bends
// routing. An alias to a non-Anthropic, OWN-credential provider dispatches
// normally with that provider's own credentials -- no whole-chain refusal,
// no steering. The token-containment invariant (the forwarded bearer never
// reaches a non-Anthropic egress) is proven above at the header/body level.
// ---------------------------------------------------------------------------

fn openai_compat_response_body() -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "some-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    })
}

fn forwarded_config_with_non_anthropic_provider(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "compat-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), common::file_ref("test-key")),
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
        mitm: Some(mitm_config("forwarded")),
        providers,
        aliases,
        models,
        retry: RetryPolicy::default(),
        ..Default::default()
    })
}

/// A config with BOTH a genuinely forwarded `[providers]` entry
/// (`credential_source = "forwarded"`, pinned to `api.anthropic.com`,
/// never dispatched to in these tests) and an own-credential provider
/// aliased as `heavy`. `Router::has_forwarded_provider()` is `true` here --
/// used to prove the admission gate no longer 400s own-provider /
/// non-Anthropic-dialect traffic just because a forwarded provider
/// coexists (f3's whole point: forwardedness moved off the request-global
/// `[mitm]` flag onto this per-provider config).
fn config_with_forwarded_and_own_providers(upstream_base: &str) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "compat-mock".to_string(),
        ProviderEntry::openai_compat(upstream_base.to_string(), common::file_ref("test-key")),
    );
    providers.insert(
        "forwarded-provider".to_string(),
        ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded),
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
        mitm: None,
        providers,
        aliases,
        models,
        retry: RetryPolicy::default(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Backward compat -- own mode and absent-[mitm] mode behave byte-for-byte
// pre-passthrough, INCLUDING when a client sends forwarded-style headers a curious
// or misconfigured client might send. Real HTTP against a wiremock upstream
// standing in for api.anthropic.com; the host pin does not apply because
// neither path ever sets `forwarded_bearer` (config-is-the-capability).
// ---------------------------------------------------------------------------

fn backward_compat_config(upstream_base: &str, mitm: Option<MitmConfig>) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic-mock".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("test-key"))
            .with_base_url(upstream_base.to_string()),
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
/// should ever see this value: own mode has no forwarded provider
/// configured (`has_forwarded_provider() == false`), and the
/// absent-`[mitm]` path has no forwarded-capture gate to arm at all.
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

/// Own mode (`[mitm]` present, transport-only, no forwarded provider
/// configured): forwarded-style headers on the inbound request are inert.
#[tokio::test]
async fn e2e_own_mode_backward_compat_ignores_forwarded_style_headers() {
    assert_backward_compat_round_trip(Some(mitm_config("own"))).await;
}

/// Absent `[mitm]` block entirely (the pre-MITM shape, and still the default
/// for any deployment that never opts into the MITM feature): the same
/// forwarded-style headers are equally inert, because there is no
/// `[mitm]` block to arm the capture gate's config-is-the-capability half.
#[tokio::test]
async fn e2e_absent_mitm_backward_compat_ignores_forwarded_style_headers() {
    assert_backward_compat_round_trip(None).await;
}
