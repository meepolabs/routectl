//! Header-merge tests (header_extras passthrough + generated identity headers).

use super::*;
use routectl_core::{ChatRequest, MessageContent, StaticToken, TokenSource};
use std::sync::Arc;

fn oauth_provider_with_extras(extras: Vec<(String, String)>) -> OpenAiResponsesProvider {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:hm-test".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct-uuid".into()),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        auth_kind: AuthKind::ChatgptOauth,
        header_extras: extras,
        user_agent: None,
        session_id: None,
    };
    OpenAiResponsesProvider::new(cfg)
}

/// Build a ChatgptOauth provider carrying an optional `session_id`,
/// with empty `header_extras`. Used by the codex session-id tests.
fn oauth_provider_with_session(session_id: Option<String>) -> OpenAiResponsesProvider {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:hm-session".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct-uuid".into()),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        auth_kind: AuthKind::ChatgptOauth,
        header_extras: Vec::new(),
        user_agent: None,
        session_id,
    };
    OpenAiResponsesProvider::new(cfg)
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: Some(32),
        ..Default::default()
    }
}

fn header_vals(request: &reqwest::Request, name: &str) -> Vec<String> {
    request
        .headers()
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(std::string::ToString::to_string)
        .collect()
}

/// On the ChatgptOauth path, an operator `header_extras` entry for
/// `originator` now FLOWS THROUGH to the wire (the old fingerprint
/// guard that dropped it is gone). Identity / fingerprint values are
/// the operator's responsibility via config.
#[test]
fn chatgpt_oauth_header_extras_originator_reaches_wire() {
    // Arrange
    let provider = oauth_provider_with_extras(vec![(
        "originator".to_string(),
        "operator-value".to_string(),
    )]);
    let rb = provider.client.post("https://chatgpt.test/responses");

    // Act
    let rb = provider
        .build_headers(rb, &base_req(), "test-jwt")
        .expect("build_headers ok");
    let request = rb.build().expect("build");

    // Assert: the operator's originator value is on the wire.
    assert_eq!(
        header_vals(&request, "originator"),
        vec!["operator-value".to_string()],
        "operator originator from header_extras must reach the wire",
    );
}

/// On the ApiKey path, `header_extras` pass through the normal
/// auth-guard merge. (There is no fingerprint filter anymore, so
/// this is just the standard `is_auth_header` / `is_managed_header`
/// behavior.)
#[test]
fn api_key_header_extras_not_blocked_by_fingerprint_filter() {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:hm-apikey".into(),
        auth: Arc::new(StaticToken::new("sk-test")) as Arc<dyn TokenSource>,
        account_id: None,
        base_url: "https://api.openai.com/v1".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: vec![("version".to_string(), "custom-1.0".to_string())],
        user_agent: None,
        session_id: None,
    };
    let provider = OpenAiResponsesProvider::new(cfg);
    let rb = provider.client.post("https://api.openai.com/v1/responses");

    let rb = provider
        .build_headers(rb, &base_req(), "sk-test")
        .expect("build_headers ok");
    let request = rb.build().expect("build");

    // `version` header from extras must be present on api-key path.
    assert!(
        header_vals(&request, "version")
            .iter()
            .any(|v| v == "custom-1.0"),
        "version extra must pass through on api-key path; got: {:?}",
        header_vals(&request, "version"),
    );
}

/// thread-id rotates per request, and within a single request
/// thread-id == x-client-request-id.
#[test]
fn thread_id_rotates_per_request_and_matches_x_client_request_id() {
    // Arrange
    let provider = oauth_provider_with_extras(Vec::new());

    // Act: two consecutive build_headers calls on the same provider.
    let req_a = provider
        .build_headers(
            provider.client.post("https://chatgpt.test/responses"),
            &base_req(),
            "test-jwt",
        )
        .expect("build_headers ok")
        .build()
        .expect("build");
    let req_b = provider
        .build_headers(
            provider.client.post("https://chatgpt.test/responses"),
            &base_req(),
            "test-jwt",
        )
        .expect("build_headers ok")
        .build()
        .expect("build");

    // Assert: thread-id present, single-valued, rotates per request.
    let tid_a = header_vals(&req_a, "thread-id");
    let tid_b = header_vals(&req_b, "thread-id");
    assert_eq!(tid_a.len(), 1, "thread-id must be single-valued: {tid_a:?}");
    assert_eq!(tid_b.len(), 1, "thread-id must be single-valued: {tid_b:?}");
    assert_ne!(tid_a[0], tid_b[0], "thread-id must rotate per request");

    // Assert: within each request, thread-id == x-client-request-id.
    assert_eq!(
        header_vals(&req_a, "x-client-request-id"),
        tid_a,
        "x-client-request-id must equal thread-id within a request",
    );
    assert_eq!(
        header_vals(&req_b, "x-client-request-id"),
        tid_b,
        "x-client-request-id must equal thread-id within a request",
    );
}

/// x-codex-window-id is stable across two requests on the same
/// provider instance (generated once in `new()`).
#[test]
fn window_id_stable_across_requests_on_same_provider() {
    // Arrange
    let provider = oauth_provider_with_extras(Vec::new());

    // Act
    let req_a = provider
        .build_headers(
            provider.client.post("https://chatgpt.test/responses"),
            &base_req(),
            "test-jwt",
        )
        .expect("build_headers ok")
        .build()
        .expect("build");
    let req_b = provider
        .build_headers(
            provider.client.post("https://chatgpt.test/responses"),
            &base_req(),
            "test-jwt",
        )
        .expect("build_headers ok")
        .build()
        .expect("build");

    // Assert
    let wid_a = header_vals(&req_a, "x-codex-window-id");
    let wid_b = header_vals(&req_b, "x-codex-window-id");
    assert_eq!(wid_a.len(), 1, "window-id must be single-valued: {wid_a:?}");
    assert_eq!(
        wid_a, wid_b,
        "x-codex-window-id must be stable across requests on the same provider",
    );
}

/// Two requests through a ChatgptOauth provider carrying a session_id
/// stamp the SAME `session-id` (stable per credential), while
/// thread-id / x-client-request-id stay fresh per request.
#[test]
fn session_id_stable_across_requests_while_thread_id_rotates() {
    // Arrange
    let provider = oauth_provider_with_session(Some("session-stable-123".into()));

    // Act
    let req_a = provider
        .build_headers(
            provider.client.post("https://chatgpt.test/responses"),
            &base_req(),
            "test-jwt",
        )
        .expect("build_headers ok")
        .build()
        .expect("build");
    let req_b = provider
        .build_headers(
            provider.client.post("https://chatgpt.test/responses"),
            &base_req(),
            "test-jwt",
        )
        .expect("build_headers ok")
        .build()
        .expect("build");

    // Assert: session-id is single-valued and identical across requests.
    let sid_a = header_vals(&req_a, "session-id");
    let sid_b = header_vals(&req_b, "session-id");
    assert_eq!(sid_a, vec!["session-stable-123".to_string()]);
    assert_eq!(
        sid_a, sid_b,
        "session-id must be stable across requests on one credential",
    );

    // Assert: the per-request identity headers still rotate.
    let tid_a = header_vals(&req_a, "thread-id");
    let tid_b = header_vals(&req_b, "thread-id");
    assert_ne!(tid_a[0], tid_b[0], "thread-id must rotate per request");
    assert_ne!(
        header_vals(&req_a, "x-client-request-id")[0],
        header_vals(&req_b, "x-client-request-id")[0],
        "x-client-request-id must rotate per request",
    );
}

/// A provider with `session_id == None` stamps no `session-id` header.
#[test]
fn no_session_id_stamps_no_session_header() {
    // Arrange
    let provider = oauth_provider_with_session(None);
    let rb = provider.client.post("https://chatgpt.test/responses");

    // Act
    let request = provider
        .build_headers(rb, &base_req(), "test-jwt")
        .expect("build_headers ok")
        .build()
        .expect("build");

    // Assert
    assert!(
        header_vals(&request, "session-id").is_empty(),
        "session_id None must not stamp a session-id header",
    );
}

/// The ApiKey (non-ChatgptOauth) path stamps no `session-id` header,
/// even when a session_id is somehow set on the config.
#[test]
fn api_key_path_stamps_no_session_header() {
    // Arrange: ApiKey config carrying a session_id (which would be
    // None in practice -- the factory only resolves it for
    // ChatgptOauth -- but proves the path gate, not just the value).
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:hm-apikey-session".into(),
        auth: Arc::new(StaticToken::new("sk-test")) as Arc<dyn TokenSource>,
        account_id: None,
        base_url: "https://api.openai.com/v1".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: Some("session-stable-123".into()),
    };
    let provider = OpenAiResponsesProvider::new(cfg);
    let rb = provider.client.post("https://api.openai.com/v1/responses");

    // Act
    let request = provider
        .build_headers(rb, &base_req(), "sk-test")
        .expect("build_headers ok")
        .build()
        .expect("build");

    // Assert
    assert!(
        header_vals(&request, "session-id").is_empty(),
        "ApiKey path must not stamp a session-id header",
    );
}

/// On the ApiKey path the three codex identity headers
/// (thread-id, x-client-request-id, x-codex-window-id) are NOT
/// injected, even when ChatgptOauth-shaped header_extras are present.
#[test]
fn api_key_path_omits_generated_identity_headers() {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:hm-apikey-id".into(),
        auth: Arc::new(StaticToken::new("sk-test")) as Arc<dyn TokenSource>,
        account_id: None,
        base_url: "https://api.openai.com/v1".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: vec![("originator".to_string(), "codex_cli_rs".to_string())],
        user_agent: None,
        session_id: None,
    };
    let provider = OpenAiResponsesProvider::new(cfg);
    let rb = provider.client.post("https://api.openai.com/v1/responses");

    let rb = provider
        .build_headers(rb, &base_req(), "sk-test")
        .expect("build_headers ok");
    let request = rb.build().expect("build");

    // Assert: the three generated identity headers are absent on the
    // api-key path. (The originator header_extra DOES pass through --
    // that is the normal merge -- but the generated identity trio
    // must not be auto-injected here.)
    for absent in ["thread-id", "x-client-request-id", "x-codex-window-id"] {
        assert!(
            header_vals(&request, absent).is_empty(),
            "{absent:?} must NOT be injected on the api-key path",
        );
    }
}

/// With empty `header_extras`, the compiled codex identity defaults
/// (originator, residency, version) appear on the outgoing request.
/// This is the zero-config posture: an operator who sets only
/// auth_kind + api_key_ref still emits a full codex fingerprint.
#[test]
fn defaults_appear_on_wire_with_empty_header_extras() {
    use routectl_core::identity::codex::{
        CODEX_ORIGINATOR, PINNED_CODEX_VERSION, RESIDENCY_HEADER_VALUE,
    };

    // Arrange
    let provider = oauth_provider_with_extras(Vec::new());
    let rb = provider.client.post("https://chatgpt.test/responses");

    // Act
    let rb = provider
        .build_headers(rb, &base_req(), "test-jwt")
        .expect("build_headers ok");
    let request = rb.build().expect("build");

    // Assert: each compiled default lands once with its default value.
    assert_eq!(
        header_vals(&request, "originator"),
        vec![CODEX_ORIGINATOR.to_string()],
        "originator default must appear with empty header_extras",
    );
    assert_eq!(
        header_vals(&request, "x-openai-internal-codex-residency"),
        vec![RESIDENCY_HEADER_VALUE.to_string()],
        "residency default must appear with empty header_extras",
    );
    assert_eq!(
        header_vals(&request, "version"),
        vec![PINNED_CODEX_VERSION.to_string()],
        "version default must appear with empty header_extras",
    );
}

/// An operator `header_extras` entry for a default key OVERRIDES the
/// compiled default: the wire shows the operator value, not the
/// built-in one, and only once (insert replaces, not appends).
#[test]
fn header_extras_overrides_default_originator() {
    // Arrange
    let provider =
        oauth_provider_with_extras(vec![("originator".to_string(), "custom".to_string())]);
    let rb = provider.client.post("https://chatgpt.test/responses");

    // Act
    let rb = provider
        .build_headers(rb, &base_req(), "test-jwt")
        .expect("build_headers ok");
    let request = rb.build().expect("build");

    // Assert: the operator's value wins; the default is gone.
    assert_eq!(
        header_vals(&request, "originator"),
        vec!["custom".to_string()],
        "operator header_extras must override the compiled default",
    );
}

/// The per-request UUIDs still override a `header_extras` `thread-id`
/// even though defaults now run before the header_extras loop. The
/// UUIDs fire LAST, so they win over both the defaults and any
/// operator-supplied value.
#[test]
fn per_request_uuid_overrides_header_extras_thread_id() {
    // Arrange
    let provider = oauth_provider_with_extras(vec![(
        "thread-id".to_string(),
        "operator-thread".to_string(),
    )]);
    let rb = provider.client.post("https://chatgpt.test/responses");

    // Act
    let rb = provider
        .build_headers(rb, &base_req(), "test-jwt")
        .expect("build_headers ok");
    let request = rb.build().expect("build");

    // Assert: single value, and it is NOT the operator's.
    let tids = header_vals(&request, "thread-id");
    assert_eq!(tids.len(), 1, "thread-id must be single-valued: {tids:?}");
    assert_ne!(
        tids[0], "operator-thread",
        "generated thread-id must override the header_extras value",
    );
}
