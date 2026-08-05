//! Tests for the anthropic-api provider: construction, auth-kind
//! resolution, header plumbing, and request/response orchestration.
//! Declared on `mod.rs` via `#[cfg(test)] #[path = ...]` so the module
//! source stays under the project's 800-LOC ceiling. Tests retain access
//! to private items via `use super::*` plus the client-internal items
//! imported below.

use super::client::{BetaDecision, resolve_user_agent, should_use_forwarded_bearer};
use super::*;
use routectl_core::{StaticToken, TokenSource};
use tracing_test::traced_test;

/// The upstream classifier (`error.type`) lifts from a body that parses
/// -- including a cap-trip prefix whose envelope survived truncation --
/// and yields `None` from an incomplete/unparseable prefix. This is the
/// seam the cap-trip branch of `read_anthropic_error` relies on to attempt
/// classification over the capped prefix.
#[test]
fn parse_anthropic_error_type_lifts_when_prefix_parses() {
    let envelope = r#"{"type":"error","error":{"type":"overloaded_error","message":"x"}}"#;
    let parsed = serde_json::from_str::<Value>(envelope).ok();
    assert_eq!(
        parse_anthropic_error_type(parsed.as_ref()).as_deref(),
        Some("overloaded_error"),
        "classifier must lift from a parseable envelope"
    );

    // An incomplete envelope (truncated mid-value) fails to parse.
    let truncated = serde_json::from_str::<Value>(r#"{"type":"error","error":{"type":"overl"#).ok();
    assert_eq!(
        parse_anthropic_error_type(truncated.as_ref()),
        None,
        "an unparseable prefix yields no classifier"
    );

    // Parseable JSON with no `error.type` yields None.
    let no_type = serde_json::from_str::<Value>(r#"{"ok":true}"#).ok();
    assert_eq!(parse_anthropic_error_type(no_type.as_ref()), None);
}

/// Body fields routectl forwards to Anthropic's
/// `/v1/messages/count_tokens` endpoint. This is the forwarding
/// allowlist, a subset of the count_tokens schema (`messages`,
/// `model`, `cache_control`, `output_config`, `system`, `thinking`,
/// `tool_choice`, `tools`); `metadata` is excluded because it is NOT
/// in that schema. Pinning the list as a const lets the test assert
/// that no extra fields leak into the count_tokens body even when
/// `normalize_request` is extended.
const COUNT_TOKENS_ALLOWED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "mcp_servers",
];

/// Pin: `build_count_tokens_body` copies ONLY the allowlist
/// fields, even when `normalize_request` produces extra keys.
/// Without this contract, a non-schema field such as `metadata`
/// silently flows into `/v1/messages/count_tokens` and the upstream
/// 400s with `Extra inputs are not permitted`.
#[test]
fn build_count_tokens_body_only_emits_allowlist_fields() {
    let normalized = serde_json::json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "hi"}],
        "system": "you are helpful",
        "tools": [{"name": "calculator", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "auto"},
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "mcp_servers": [{"name": "s1", "url": "https://mcp.example.com"}],
        // Fields below MUST NOT reach the upstream count_tokens body:
        "metadata": {"user_id": "u_42"},
        "stream": true,
        "max_tokens": 4096,
        "anthropic_beta": ["context-1m-2025-08-07"],
        "temperature": 0.7,
        "top_p": 0.9,
        "stop_sequences": ["</block>"],
        "output_config": {"format": {"type": "json_schema"}},
    });

    let body = build_count_tokens_body(&normalized);
    let obj = body.as_object().expect("count_tokens body must be object");
    for k in obj.keys() {
        assert!(
            COUNT_TOKENS_ALLOWED_FIELDS.contains(&k.as_str()),
            "count_tokens body must only emit allowlist fields, found: {k}"
        );
    }
    // Allowlist fields that ARE present in the input must round-trip.
    assert_eq!(obj["model"], "claude-haiku-4-5");
    assert_eq!(obj["system"], "you are helpful");
    assert_eq!(obj["tools"][0]["name"], "calculator");
    assert_eq!(obj["thinking"]["type"], "enabled");
    // `metadata` is not part of the count_tokens schema; it must be dropped.
    assert!(!obj.contains_key("metadata"));
}

/// Allowlist fields not present on the input must NOT be synthesized
/// (e.g. `mcp_servers: null`); the helper only forwards keys that
/// existed and were non-null in the normalized body.
#[test]
fn build_count_tokens_body_skips_absent_allowlist_fields() {
    let normalized = serde_json::json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = build_count_tokens_body(&normalized);
    let obj = body.as_object().expect("body is object");
    assert!(obj.contains_key("model"));
    assert!(obj.contains_key("messages"));
    assert!(!obj.contains_key("system"));
    assert!(!obj.contains_key("tools"));
    assert!(!obj.contains_key("tool_choice"));
    assert!(!obj.contains_key("thinking"));
    assert!(!obj.contains_key("mcp_servers"));
    assert!(!obj.contains_key("metadata"));
}

/// Drive `build_headers` end-to-end and return the assembled
/// outbound HTTP header names (lowercased) so allowlist tests can
/// assert which `x-claude-code-*` entries reached the wire.
/// Building the `RequestBuilder` does no I/O; `.build()` just
/// constructs the `reqwest::Request` object.
fn outbound_header_names(provider: &AnthropicApiProvider, req: &ChatRequest) -> Vec<String> {
    let client = reqwest::Client::new();
    let rb = client.post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, req, "test-token", None);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .iter()
        .map(|(name, _)| name.as_str().to_ascii_lowercase())
        .collect()
}

fn cfg_with_allowlist(forward_client_headers: Vec<String>) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers,
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

fn req_with_claude_code_headers(pairs: Vec<(&str, &str)>) -> ChatRequest {
    let mut req = ChatRequest::default();
    // RoutectlInternal is `#[non_exhaustive]`, so we mutate the
    // single field we need on the default-constructed value rather
    // than using a struct expression with `..default()`.
    req.routectl_internal.claude_code_headers = pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    req
}

/// Empty allowlist drops every captured `x-claude-code-*` header.
/// Secure-by-default: a fresh provider with no operator opt-in MUST
/// NOT leak inbound attribution headers to api.anthropic.com.
#[test]
fn forward_client_headers_empty_drops_everything() {
    let cfg = cfg_with_allowlist(Vec::new());
    let provider = AnthropicApiProvider::new(cfg);
    let req = req_with_claude_code_headers(vec![
        ("x-claude-code-session-id", "abc"),
        ("x-claude-code-agent-id", "xyz"),
    ]);
    let names = outbound_header_names(&provider, &req);
    assert!(
        !names.iter().any(|n| n.starts_with("x-claude-code-")),
        "empty allowlist must drop every captured header; got: {names:?}"
    );
}

/// Names on the allowlist pass through verbatim (case preserved as
/// sent by the client). The egress emits the inbound name string,
/// not a normalized version.
#[test]
fn forward_client_headers_listed_names_pass_through() {
    let cfg = cfg_with_allowlist(vec!["x-claude-code-session-id".into()]);
    let provider = AnthropicApiProvider::new(cfg);
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    let names = outbound_header_names(&provider, &req);
    assert!(
        names.iter().any(|n| n == "x-claude-code-session-id"),
        "allowlisted header must reach outbound; got: {names:?}"
    );
}

/// Only allowlisted names reach outbound; unlisted captured headers
/// are dropped at the egress. This is the core defense-in-depth
/// posture: inbound capture is namespace-bounded, but the egress
/// owns the final filter.
#[test]
fn forward_client_headers_unlisted_names_dropped() {
    let cfg = cfg_with_allowlist(vec!["x-claude-code-session-id".into()]);
    let provider = AnthropicApiProvider::new(cfg);
    let req = req_with_claude_code_headers(vec![
        ("x-claude-code-session-id", "sid-42"),
        ("x-claude-code-agent-id", "aid-7"),
    ]);
    let names = outbound_header_names(&provider, &req);
    assert!(
        names.iter().any(|n| n == "x-claude-code-session-id"),
        "session-id must pass through; got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "x-claude-code-agent-id"),
        "unlisted agent-id must be dropped; got: {names:?}"
    );
}

/// Drive `build_headers` end-to-end and return the value of the
/// requested header on the assembled outbound request, or `None`
/// if the header is absent. Composes headers with no assembled body, so
/// body-derived capability betas never fire -- use
/// `outbound_header_value_for_body` to exercise those.
fn outbound_header_value(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
    name: &str,
) -> Option<String> {
    outbound_header_value_for_body(provider, req, name, None)
}

/// `outbound_header_value` with an explicit assembled wire body, so tests
/// can drive the body-derived beta union (`output_config.format` ->
/// structured-outputs).
fn outbound_header_value_for_body(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
    name: &str,
    body: Option<&Value>,
) -> Option<String> {
    let client = reqwest::Client::new();
    let rb = client.post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, req, "test-token", body);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Header collision policy: forwarded client headers WIN over
/// `header_extras` for the same lowercase name. Rationale: the
/// operator opted into client passthrough for that specific name
/// via `forward_client_headers`; the client value is more
/// specific than the operator's static `header_extras` default.
/// Pre-fix the egress called `RequestBuilder::header()` per entry
/// which APPENDS; the upstream then saw both values. With the
/// HeaderMap+`headers()` rebuild, the policy is explicit.
#[test]
fn client_forwarded_headers_override_header_extras_on_collision() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: vec![(
            "x-claude-code-session-id".into(),
            "from-operator-config".into(),
        )],
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: vec!["x-claude-code-session-id".into()],
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "from-client")]);
    let value = outbound_header_value(&provider, &req, "x-claude-code-session-id")
        .expect("session-id header missing");
    assert_eq!(
        value, "from-client",
        "client-forwarded header must override header_extras on collision; got {value}"
    );
}

/// Non-empty `allowed_betas` drops client-requested flags that are
/// not on the operator list. The header must contain only the
/// allowed flag and must NOT contain the blocked one.
#[test]
#[allow(clippy::field_reassign_with_default)]
fn allowed_betas_filters_header_drops_unlisted_flag() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: vec!["allowed-only".into()],
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    // ChatRequest is #[non_exhaustive]; mutate after default().
    let mut req = ChatRequest::default();
    req.anthropic_beta = vec!["allowed-only".into(), "blocked".into()];
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    assert!(
        value.split(',').any(|s| s.trim() == "allowed-only"),
        "allowed flag must reach the header; got {value}"
    );
    assert!(
        !value.split(',').any(|s| s.trim() == "blocked"),
        "blocked flag must be dropped from the header; got {value}"
    );
}

/// Operator `header_extras` betas bypass the allowlist unconditionally
/// while non-allowlisted client betas are dropped. This pins the
/// design contract: operator-supplied config wins regardless of the
/// client-request content, but the allowlist still gates client betas.
#[test]
#[allow(clippy::field_reassign_with_default)]
fn operator_header_extras_beta_bypasses_allowlist() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: vec![("anthropic-beta".into(), "ops-only".into())],
        user_agent: None,
        allowed_betas: vec!["req-allowed".into()],
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let mut req = ChatRequest::default();
    req.anthropic_beta = vec!["req-allowed".into(), "client-blocked".into()];
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    assert!(
        value.split(',').any(|s| s.trim() == "ops-only"),
        "operator header_extras beta must bypass allowlist and reach the header; got {value}"
    );
    assert!(
        value.split(',').any(|s| s.trim() == "req-allowed"),
        "allowlisted client beta must reach the header; got {value}"
    );
    assert!(
        !value.split(',').any(|s| s.trim() == "client-blocked"),
        "non-allowlisted client beta must be dropped; got {value}"
    );
}

/// Empty `allowed_betas` is pass-through mode: every requested
/// beta reaches the header unchanged. This is the default for all
/// deployments that do not set an explicit allowlist.
#[test]
#[allow(clippy::field_reassign_with_default)]
fn allowed_betas_empty_passes_all_through() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    // ChatRequest is #[non_exhaustive]; mutate after default().
    let mut req = ChatRequest::default();
    req.anthropic_beta = vec!["beta-one".into(), "beta-two".into()];
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    assert!(
        value.split(',').any(|s| s.trim() == "beta-one"),
        "beta-one must pass through with empty allowlist; got {value}"
    );
    assert!(
        value.split(',').any(|s| s.trim() == "beta-two"),
        "beta-two must pass through with empty allowlist; got {value}"
    );
}

/// Model-level operator betas (composed by the router onto
/// `routectl_internal.operator_betas`) bypass the allowlist
/// unconditionally, while non-allowlisted client betas folded into
/// `req.anthropic_beta` are still dropped. This pins the invariant:
/// `allowed_betas` gates only client-requested betas, never the
/// betas an operator pinned in `[models.X] header_extras`.
#[test]
#[allow(clippy::field_reassign_with_default)]
fn model_level_operator_beta_bypasses_allowlist() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: vec!["req-allowed".into()],
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let mut req = ChatRequest::default();
    // The router folds the model-level beta into the full union on
    // `req.anthropic_beta` AND records it as an operator floor on
    // `operator_betas`. The allowlist filter drops it from the union,
    // but the floor re-adds it unconditionally.
    req.anthropic_beta = vec![
        "req-allowed".into(),
        "client-blocked".into(),
        "ctx-1m".into(),
    ];
    req.routectl_internal.operator_betas = vec!["ctx-1m".into()];
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    assert!(
        value.split(',').any(|s| s.trim() == "ctx-1m"),
        "model-level operator beta must bypass allowlist and reach the header; got {value}"
    );
    assert!(
        value.split(',').any(|s| s.trim() == "req-allowed"),
        "allowlisted client beta must reach the header; got {value}"
    );
    assert!(
        !value.split(',').any(|s| s.trim() == "client-blocked"),
        "non-allowlisted client beta must be dropped; got {value}"
    );
}

/// Build an oauth-bearer config with the given header_extras and a
/// `user_agent` override (None to exercise the SDK default).
fn oauth_cfg(
    header_extras: Vec<(String, String)>,
    user_agent: Option<String>,
) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("oat-token")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras,
        user_agent,
        allowed_betas: Vec::new(),
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

/// On the OauthBearer path with empty `header_extras`, the compiled
/// Stainless SDK defaults appear on the outgoing request. Zero-config
/// posture: auth_kind + api_key_ref alone yields the full fingerprint.
#[test]
fn oauth_bearer_emits_stainless_defaults_with_empty_extras() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = ChatRequest::default();
    assert_eq!(
        outbound_header_value(&provider, &req, "x-app").as_deref(),
        Some("cli"),
        "x-app default must appear on oauth-bearer",
    );
    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-lang").as_deref(),
        Some("js"),
        "x-stainless-lang default must appear on oauth-bearer",
    );
    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-timeout").as_deref(),
        Some("600"),
        "x-stainless-timeout default must appear on oauth-bearer",
    );
    // Dynamic entries present and mapped (not raw Rust cfg strings).
    let arch = outbound_header_value(&provider, &req, "x-stainless-arch")
        .expect("x-stainless-arch present");
    assert_ne!(arch, "x86_64", "arch must be mapped to Node shape");
    let os =
        outbound_header_value(&provider, &req, "x-stainless-os").expect("x-stainless-os present");
    assert_ne!(os, "linux", "os must be mapped to capitalized shape");
}

/// An operator `header_extras` entry for a default key OVERRIDES the
/// compiled Stainless default (insert replaces; the loop runs after
/// the defaults).
#[test]
fn oauth_bearer_header_extras_overrides_stainless_default() {
    let provider = AnthropicApiProvider::new(oauth_cfg(
        vec![("x-stainless-timeout".into(), "999".into())],
        None,
    ));
    let req = ChatRequest::default();
    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-timeout").as_deref(),
        Some("999"),
        "operator header_extras must override the compiled default",
    );
}

/// On the ApiKey path, no Stainless SDK defaults are injected even
/// with empty `header_extras`. The api-key surface is the raw API,
/// not the SDK client, so it carries no SDK fingerprint.
#[test]
fn api_key_path_emits_no_stainless_defaults() {
    let provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
    let req = ChatRequest::default();
    for absent in [
        "x-app",
        "x-stainless-lang",
        "x-stainless-runtime",
        "x-stainless-runtime-version",
        "x-stainless-package-version",
        "x-stainless-timeout",
        "x-stainless-retry-count",
        "x-stainless-arch",
        "x-stainless-os",
        "anthropic-dangerous-direct-browser-access",
    ] {
        assert!(
            outbound_header_value(&provider, &req, absent).is_none(),
            "{absent:?} must NOT be injected on the api-key path",
        );
    }
}

/// On OauthBearer with `user_agent = None`, the resolved client UA
/// falls back to the Claude Code SDK default. An operator override
/// always wins; the ApiKey surface keeps reqwest's default (`None`).
/// We assert the resolver directly: reqwest applies a client-level
/// default UA only at send time, not at `RequestBuilder::build()`,
/// so the value is not observable on a non-executed request.
#[test]
fn oauth_bearer_user_agent_defaults_to_claude_cli() {
    assert_eq!(
        resolve_user_agent(None, AuthKind::OauthBearer).as_deref(),
        Some("claude-cli/2.1.169 (external, cli)"),
        "oauth-bearer with no override must default to the Claude Code SDK UA",
    );
    assert_eq!(
        resolve_user_agent(None, AuthKind::ApiKey),
        None,
        "api-key with no override must keep reqwest's default UA",
    );
    assert_eq!(
        resolve_user_agent(Some("op-ua/9.9"), AuthKind::OauthBearer).as_deref(),
        Some("op-ua/9.9"),
        "operator override must win over the SDK default",
    );
}

/// Build an oauth-bearer config with an explicit base_url and an
/// optional session_id, plus optional header_extras, and an explicit
/// forwarded-gate setting. Used by both the Claude-Code
/// session-identity header tests (own mode) and the forwarded-leg
/// tests (`use_forwarded_bearer: true`).
fn oauth_cfg_with_session(
    base_url: &str,
    session_id: Option<String>,
    header_extras: Vec<(String, String)>,
    use_forwarded_bearer: bool,
) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("oat-token")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras,
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id,
        cloak: CloakConfig::default(),
        use_forwarded_bearer,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

/// Two requests through an OauthBearer api.anthropic.com provider
/// carrying a session_id must stamp the SAME `x-claude-code-session-id`
/// (stable per credential) and DIFFERENT, valid-uuid
/// `x-client-request-id` values (fresh per request).
#[test]
fn oauth_anthropic_base_stamps_stable_session_and_fresh_request_id() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("session-stable-123".into()),
        Vec::new(),
        false,
    ));
    let req = ChatRequest::default();

    let sid_1 = outbound_header_value(&provider, &req, "x-claude-code-session-id");
    let sid_2 = outbound_header_value(&provider, &req, "x-claude-code-session-id");
    assert_eq!(sid_1.as_deref(), Some("session-stable-123"));
    assert_eq!(
        sid_1, sid_2,
        "session-id must be stable across requests on one credential"
    );

    let rid_1 = outbound_header_value(&provider, &req, "x-client-request-id")
        .expect("x-client-request-id must be present");
    let rid_2 = outbound_header_value(&provider, &req, "x-client-request-id")
        .expect("x-client-request-id must be present");
    assert_ne!(
        rid_1, rid_2,
        "x-client-request-id must be fresh per request"
    );
    assert!(
        uuid::Uuid::parse_str(&rid_1).is_ok(),
        "x-client-request-id must be a valid uuid; got {rid_1}"
    );
    assert!(
        uuid::Uuid::parse_str(&rid_2).is_ok(),
        "x-client-request-id must be a valid uuid; got {rid_2}"
    );
}

/// The ApiKey surface is the raw API, not the Claude-Code SDK client:
/// neither session-identity header is stamped.
#[test]
fn api_key_path_stamps_no_session_identity_headers() {
    // cfg_with_allowlist builds an ApiKey config on the
    // api.anthropic.com base.
    let provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
    let req = ChatRequest::default();
    assert!(
        outbound_header_value(&provider, &req, "x-client-request-id").is_none(),
        "ApiKey path must not stamp x-client-request-id",
    );
    assert!(
        outbound_header_value(&provider, &req, "x-claude-code-session-id").is_none(),
        "ApiKey path must not stamp x-claude-code-session-id",
    );
}

/// With `cfg.session_id = None` on the OauthBearer + api.anthropic.com
/// surface, the minted identity supplies a stable session id, so a
/// `x-claude-code-session-id` header IS now stamped (mint-when-absent)
/// and is the SAME across two requests (one identity per provider).
#[test]
fn oauth_anthropic_base_mints_stable_session_when_cfg_absent() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        None,
        Vec::new(),
        false,
    ));
    let req = ChatRequest::default();
    let sid_1 = outbound_header_value(&provider, &req, "x-claude-code-session-id")
        .expect("a session id must be minted when cfg.session_id is None");
    let sid_2 = outbound_header_value(&provider, &req, "x-claude-code-session-id")
        .expect("a session id must be minted when cfg.session_id is None");
    assert_eq!(
        sid_1, sid_2,
        "minted session id must be stable across requests"
    );
    assert!(
        uuid::Uuid::parse_str(&sid_1).is_ok(),
        "minted session id must be a valid uuid; got {sid_1}"
    );
}
/// OauthBearer but a non-anthropic base (a third-party /anthropic
/// surface): the Claude-Code session identity must NOT leak there.
#[test]
fn oauth_non_anthropic_base_stamps_no_session_identity_headers() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://example.invalid",
        Some("session-stable-123".into()),
        Vec::new(),
        false,
    ));
    let req = ChatRequest::default();
    assert!(
        outbound_header_value(&provider, &req, "x-client-request-id").is_none(),
        "non-anthropic base must not stamp x-client-request-id",
    );
    assert!(
        outbound_header_value(&provider, &req, "x-claude-code-session-id").is_none(),
        "non-anthropic base must not stamp x-claude-code-session-id",
    );
}

/// An operator `header_extras` entry for `x-claude-code-session-id`
/// OVERRIDES the built-in value: the identity stamping is in the
/// "inserted first" phase, the header_extras apply loop runs after
/// and replaces.
#[test]
fn operator_header_extras_overrides_built_in_session_id() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("built-in-session".into()),
        vec![(
            "x-claude-code-session-id".into(),
            "from-operator-config".into(),
        )],
        false,
    ));
    let req = ChatRequest::default();
    let value = outbound_header_value(&provider, &req, "x-claude-code-session-id")
        .expect("session-id header must be present");
    assert_eq!(
        value, "from-operator-config",
        "operator header_extras must override the built-in session id; got {value}"
    );
}

#[test]
fn is_anthropic_api_host_matches_only_the_exact_host() {
    // Exhaustive host-matching cases live with the shared predicate in
    // `routectl_core::identity::anthropic`. This thin delegation test
    // pins that the WIRE gate still routes through that single source
    // of truth: an exact host matches, a sibling-domain lookalike does
    // not.
    assert!(is_anthropic_api_host("https://api.anthropic.com"));
    assert!(!is_anthropic_api_host(
        "https://api.anthropic.com.evil.example"
    ));
}

/// A non-anthropic base that merely CONTAINS the host substring must
/// not stamp the Claude-Code session identity (defends the precise
/// host check end-to-end through build_headers).
#[test]
fn lookalike_anthropic_base_stamps_no_session_identity_headers() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com.evil.example",
        Some("session-stable-123".into()),
        Vec::new(),
        false,
    ));
    let req = ChatRequest::default();
    assert!(
        outbound_header_value(&provider, &req, "x-client-request-id").is_none(),
        "a look-alike host must not stamp x-client-request-id",
    );
    assert!(
        outbound_header_value(&provider, &req, "x-claude-code-session-id").is_none(),
        "a look-alike host must not stamp x-claude-code-session-id",
    );
}

// -- Beta floor tests --------------------------------------------------

/// On OauthBearer + api.anthropic.com, all pinned floor betas
/// appear in the outbound `anthropic-beta` header.
#[test]
fn beta_floor_all_pinned_present_on_oauth_anthropic_host() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = ChatRequest::default();
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    for expected in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
        assert!(
            betas.contains(expected),
            "floor beta {expected} must be present on oauth+anthropic host; got: {value}"
        );
    }
}

/// When context_management emulation is active, the
/// `context-management-2025-06-27` floor beta is stripped from the
/// outbound header (the emulation path handles the semantics, so
/// forwarding it upstream would cause a 400 on non-Anthropic hosts).
#[test]
fn beta_floor_context_management_stripped_when_emulation_active() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("oat-token")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: true,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = ChatRequest::default();
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    assert!(
        !betas.contains(&context_management::CONTEXT_MANAGEMENT_BETA),
        "context-management beta must be stripped when emulation is active; got: {value}"
    );
    // Other floor betas must still be present.
    assert!(
        betas.contains(&"oauth-2025-04-20"),
        "non-stripped floor betas must still be present; got: {value}"
    );
}

/// On OauthBearer with a non-Anthropic base, the floor betas must
/// NOT appear -- the floor is scoped to api.anthropic.com only.
#[test]
fn beta_floor_absent_on_non_anthropic_host() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("oat-token")),
        base_url: "https://proxy.example.com/".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = ChatRequest::default();
    // No client betas, no operator betas -> header absent entirely.
    assert!(
        outbound_header_value(&provider, &req, "anthropic-beta").is_none(),
        "beta floor must NOT appear on a non-anthropic host"
    );
}

/// On ApiKey (even with api.anthropic.com base), the floor betas
/// must NOT appear -- the floor is scoped to OauthBearer only.
#[test]
fn beta_floor_absent_on_api_key_auth() {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = ChatRequest::default();
    // No client betas, no operator betas -> header absent entirely.
    assert!(
        outbound_header_value(&provider, &req, "anthropic-beta").is_none(),
        "beta floor must NOT appear on the api-key path"
    );
}

// -- structured-outputs capability beta --------------------------------
/// Plain api-key provider against api.anthropic.com: no OAuth gate, no
/// beta floor, so the structured-outputs beta can only arrive from the
/// body-derived capability union.
fn api_key_cfg_for_betas(allowed_betas: Vec<String>) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas,
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

/// A body carrying `output_config.format` is rejected upstream unless the
/// structured-outputs beta rides along. Pre-fix that flag reached the wire
/// ONLY via the Claude-Code OAuth floor, so an ordinary api-key provider
/// sending a `json_schema` request emitted the beta-gated field with no
/// beta header at all.
#[test]
fn api_key_request_with_output_config_format_carries_structured_outputs_beta() {
    let provider = AnthropicApiProvider::new(api_key_cfg_for_betas(Vec::new()));
    let req = ChatRequest::default();
    let body = serde_json::json!({
        "model": "claude-sonnet-4-5",
        "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
    });

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("a body carrying output_config.format must produce a beta header");
    assert_eq!(
        value,
        routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA,
        "the api-key path must carry exactly the structured-outputs beta"
    );
}

/// The union is capability-driven (a server requirement implied by the
/// shipped body), not a client-opted beta -- so it bypasses the operator
/// `allowed_betas` allowlist exactly as the operator-pinned floor does.
/// Without this, an operator allowlist would silently produce a body
/// upstream rejects.
#[test]
fn structured_outputs_beta_bypasses_the_client_allowlist() {
    let provider = AnthropicApiProvider::new(api_key_cfg_for_betas(vec!["some-other-beta".into()]));
    let req = ChatRequest::default();
    let body = serde_json::json!({"output_config": {"format": {"type": "json_object"}}});

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the capability beta must survive a restrictive allowlist");
    assert!(
        value
            .split(',')
            .any(|b| b.trim() == routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA),
        "allowed_betas gates client-requested betas only; got: {value}"
    );
}

/// No `output_config.format` on the shipped body -> no flag added. Pins
/// that the union is gated on the body and never fires unconditionally.
/// A sibling `output_config.effort` (adaptive-thinking path) is NOT the
/// structured-output directive and must not trigger it either.
#[test]
fn body_without_output_config_format_gains_no_structured_outputs_beta() {
    let provider = AnthropicApiProvider::new(api_key_cfg_for_betas(Vec::new()));
    let req = ChatRequest::default();

    for body in [
        serde_json::json!({"model": "claude-sonnet-4-5"}),
        serde_json::json!({"output_config": {"effort": "high"}}),
    ] {
        assert!(
            outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
                .is_none(),
            "no output_config.format must mean no beta header; body: {body}"
        );
    }
}

/// The trigger reads the ASSEMBLED body, not `req.response_format`: an
/// `output_config.format` that arrives via the `provider_extras`
/// forward-compat sweep (an Anthropic-ingress round-trip) fires the union
/// even though the canonical request carries no `response_format`. This is
/// why the union cannot key off `req` -- `merge_provider_extras` and
/// `reconcile_output_config_effort` reshape `output_config` after
/// translation.
#[test]
fn structured_outputs_beta_triggers_on_output_config_arriving_via_provider_extras() {
    let provider = AnthropicApiProvider::new(api_key_cfg_for_betas(Vec::new()));
    let req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(64),
        // No response_format: the directive rides provider_extras only.
        provider_extras: Some(serde_json::json!({
            "output_config": {"format": {"type": "json_object"}}
        })),
        ..Default::default()
    };

    let body = provider.normalize_request(&req).expect("normalize");
    assert!(
        body["output_config"].get("format").is_some(),
        "precondition: the extras sweep must land output_config.format; got: {body}"
    );

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("an extras-supplied output_config.format must produce a beta header");
    assert!(
        value
            .split(',')
            .any(|b| b.trim() == routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA),
        "the union must read the assembled body, not req.response_format; got: {value}"
    );
}

/// A cloaked Claude-Code OAuth request's beta list stays BYTE-IDENTICAL
/// with the union in play: the pinned floor already carries the
/// structured-outputs flag, so the union is a no-op -- no duplicate, no
/// reorder, same joined header value as a request with no body.
#[test]
fn oauth_claude_code_beta_list_is_byte_identical_with_structured_outputs_body() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = req_with_claude_code_headers(Vec::new());
    let body = serde_json::json!({
        "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
    });

    let without_body = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("the OAuth floor must produce a beta header");
    let with_body = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the OAuth floor must produce a beta header");

    assert_eq!(
        with_body, without_body,
        "the OAuth Claude-Code beta list must be byte-identical to today"
    );
    // The composer emits the OAuth gate flag first, then the floor in
    // corpus order (its own `oauth-2025-04-20` entry deduped away), so the
    // emitted list is the floor's contents with one leading rotation.
    let betas: Vec<&str> = with_body.split(',').map(str::trim).collect();
    let floor = routectl_core::identity::anthropic::default_claude_code_anthropic_betas();
    assert_eq!(
        betas.len(),
        floor.len(),
        "the union must not widen the floor; got: {with_body}"
    );
    for flag in floor {
        assert!(
            betas.contains(flag),
            "the emitted list must still carry every floor flag ({flag}); got: {with_body}"
        );
    }
    assert_eq!(
        betas
            .iter()
            .filter(|b| **b == routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA)
            .count(),
        1,
        "the union must be idempotent, never duplicating the floor's flag"
    );
}

/// Genuine-CC (`is_non_cc == false`, so the speculative beta floor is
/// skipped) still gets the structured-outputs flag when the body actually
/// carries `output_config.format`. The union is deliberately NOT gated on
/// `is_non_cc`: unlike the floor it is feature-triggered, so a real Claude
/// Code request using structured outputs needs the flag exactly as a
/// cloaked one does -- without it Anthropic rejects the field.
#[test]
fn genuine_cc_request_with_output_config_format_carries_structured_outputs_beta() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let mut req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    req.anthropic_beta = vec!["fine-grained-tool-streaming-2025-05-14".into()];
    let body = serde_json::json!({
        "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
    });

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("anthropic-beta header must be present on the genuine-CC path");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    assert_eq!(
        betas,
        vec![
            // The client's own beta set, verbatim and first.
            "fine-grained-tool-streaming-2025-05-14",
            // The OAuth gate flag, unioned on both CC paths.
            routectl_core::identity::anthropic::OAUTH_ANTHROPIC_BETA,
            // The body-derived capability flag -- the floor is skipped here,
            // so this is the only other entry.
            routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA,
        ],
        "genuine-CC must get its own betas + the OAuth gate + exactly the \
         structured-outputs flag; got: {value}"
    );
}

/// Fail-closed on a third-party anthropic-shaped host: the union is not
/// pinned to `api.anthropic.com`, so a self-hosted or gateway base_url
/// carrying `output_config.format` still emits the gating beta. A
/// downstream that ignores unknown betas loses nothing; one that enforces
/// them would otherwise reject the field.
#[test]
fn non_anthropic_host_with_output_config_format_still_carries_structured_outputs_beta() {
    let mut cfg = api_key_cfg_for_betas(Vec::new());
    cfg.base_url = "https://anthropic.gateway.example.com".into();
    let provider = AnthropicApiProvider::new(cfg);
    let req = ChatRequest::default();
    let body = serde_json::json!({
        "output_config": {"format": {"type": "json_object"}},
    });

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("a third-party anthropic-shaped host must still get the beta");
    assert_eq!(
        value,
        routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA,
        "the union is host-independent; got: {value}"
    );
}

/// Genuine-CC (own-mode, `is_non_cc() == false`) requests must NOT get
/// the fingerprint-widening beta floor: real Claude Code never asked
/// for capability betas like `context-1m` on e.g. a haiku/WebFetch
/// call, and force-widening its own beta set makes Anthropic 400 it.
#[test]
fn genuine_cc_request_omits_floor_only_betas() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present (oauth gate flag)");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    for floor_only in ["context-1m-2025-08-07", "interleaved-thinking-2025-05-14"] {
        assert!(
            !betas.contains(&floor_only),
            "genuine-CC request must not carry floor-only beta {floor_only}; got: {value}"
        );
    }
}

/// Non-CC (routectl is cloaking the request as Claude Code) requests
/// still get the FULL pinned floor, unchanged from pre-gate behavior.
#[test]
fn non_cc_request_gets_full_floor() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = req_with_claude_code_headers(Vec::new());
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    for expected in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
        assert!(
            betas.contains(expected),
            "non-CC request must carry full floor beta {expected}; got: {value}"
        );
    }
}

/// `oauth-2025-04-20` is required for OAuth to function on
/// api.anthropic.com, so it is unioned unconditionally -- present on
/// BOTH the genuine-CC and the non-CC path, independent of the floor
/// gate.
#[test]
fn oauth_beta_present_for_both_genuine_cc_and_non_cc() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));

    let genuine_cc = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid")]);
    let value = outbound_header_value(&provider, &genuine_cc, "anthropic-beta")
        .expect("anthropic-beta header must be present for genuine-CC");
    assert!(
        value.split(',').any(|b| b.trim() == "oauth-2025-04-20"),
        "oauth gate flag must be present for genuine-CC; got: {value}"
    );

    let non_cc = req_with_claude_code_headers(Vec::new());
    let value = outbound_header_value(&provider, &non_cc, "anthropic-beta")
        .expect("anthropic-beta header must be present for non-CC");
    assert!(
        value.split(',').any(|b| b.trim() == "oauth-2025-04-20"),
        "oauth gate flag must be present for non-CC; got: {value}"
    );
}

/// The gate never strips a genuine-CC client's OWN requested betas --
/// only the routectl-minted floor is suppressed. A real Claude Code
/// request that itself asked for `context-1m` still gets it.
#[test]
fn genuine_cc_own_requested_beta_survives_the_gate() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let mut req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    req.anthropic_beta = vec!["context-1m-2025-08-07".into()];

    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    assert!(
        value
            .split(',')
            .any(|b| b.trim() == "context-1m-2025-08-07"),
        "the client's own requested beta must never be stripped by the gate; got: {value}"
    );
}

// -- forwarded (pure-proxy) leg: client identity overrides mint --------
//
// On the forwarded leg (`forwarded_bearer` Some AND the base is exactly
// api.anthropic.com) the egress is a TRANSPARENT forwarder: Claude
// Code's REAL inbound identity headers must reach Anthropic and
// OVERRIDE routectl's minted cloak fingerprint. Own mode
// (`forwarded_bearer` None) is byte-for-byte unchanged -- proven by the
// minted-fingerprint tests above plus the explicit own-mode guard here.

/// The distinctive forwarded-bearer secret used in the leg tests. It is
/// only ever read as a GATE (is_some) by build_headers, never emitted,
/// so the no-leak test can assert it appears in no outbound header.
const FORWARDED_TOKEN_CANARY: &str = "sk-ant-oat01-FWD-DO-NOT-LEAK-xyz";

/// Build a forwarded-leg request: a captured first-party bearer plus the
/// client's inbound identity (`x-stainless-*` on `stainless_headers`,
/// `x-claude-code-*` on `claude_code_headers`, betas on `anthropic_beta`).
fn forwarded_req(
    stainless: &[(&str, &str)],
    claude_code: &[(&str, &str)],
    betas: &[&str],
) -> ChatRequest {
    let mut req = ChatRequest::default();
    req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
        FORWARDED_TOKEN_CANARY.into(),
    ));
    req.routectl_internal.stainless_headers = stainless
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    req.routectl_internal.claude_code_headers = claude_code
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    req.anthropic_beta = betas.iter().map(|s| (*s).to_string()).collect();
    req
}

/// Every outbound `(name, value)` pair on the assembled request. Lets a
/// test scan all header values (e.g. for a leaked token).
fn outbound_header_pairs(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
) -> Vec<(String, String)> {
    let client = reqwest::Client::new();
    let rb = client.post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, req, "test-token", None);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .iter()
        .map(|(n, v)| {
            (
                n.as_str().to_ascii_lowercase(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// The minted `x-stainless-package-version` default, looked up from the
/// shared identity source so the test does not hardcode the version.
fn minted_stainless_package_version() -> String {
    routectl_core::identity::anthropic::default_claude_code_identity_headers()
        .into_iter()
        .find_map(|(n, v)| (n == "x-stainless-package-version").then(|| v.to_string()))
        .expect("minted default carries x-stainless-package-version")
}

/// Forwarded leg: the client's `x-stainless-*` headers OVERRIDE the
/// minted Stainless fingerprint on the outbound request, so Anthropic
/// sees the genuine client SDK identity rather than routectl's mint.
#[test]
fn forwarded_leg_client_stainless_overrides_minted_fingerprint() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        true,
    ));
    // Sanity: without a client value the minted default would win.
    let minted = minted_stainless_package_version();
    let req = forwarded_req(
        &[
            ("x-stainless-package-version", "1.2.3-client"),
            ("x-stainless-os", "ClientOS"),
            ("x-stainless-lang", "client-lang"),
        ],
        &[],
        &[],
    );

    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-package-version").as_deref(),
        Some("1.2.3-client"),
        "client x-stainless-package-version must override the minted default",
    );
    assert_ne!(
        outbound_header_value(&provider, &req, "x-stainless-package-version").as_deref(),
        Some(minted.as_str()),
        "the minted Stainless version must NOT win on the forwarded leg",
    );
    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-os").as_deref(),
        Some("ClientOS"),
        "client x-stainless-os must override the minted default",
    );
    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-lang").as_deref(),
        Some("client-lang"),
        "client x-stainless-lang must override the minted default",
    );
}

/// Forwarded leg: the client's inbound `x-claude-code-session-id`
/// OVERRIDES routectl's minted per-credential session id, so the
/// forwarded request carries the client's real conversation identity.
#[test]
fn forwarded_leg_client_session_id_overrides_minted() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        true,
    ));
    let req = forwarded_req(&[], &[("x-claude-code-session-id", "client-sid-abc")], &[]);

    assert_eq!(
        outbound_header_value(&provider, &req, "x-claude-code-session-id").as_deref(),
        Some("client-sid-abc"),
        "client session id must override the minted session id on the forwarded leg",
    );
}

/// Forwarded leg: the client's captured `x-claude-code-*` headers are
/// forwarded TRANSPARENTLY -- not gated by `forward_client_headers`
/// (empty here, the secure-by-default posture that own mode honors).
#[test]
fn forwarded_leg_forwards_all_client_claude_code_headers_transparently() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        true,
    ));
    let req = forwarded_req(
        &[],
        &[
            ("x-claude-code-session-id", "client-sid-abc"),
            ("x-claude-code-agent-id", "client-agent-9"),
        ],
        &[],
    );

    assert_eq!(
        outbound_header_value(&provider, &req, "x-claude-code-agent-id").as_deref(),
        Some("client-agent-9"),
        "a forwarded leg forwards every captured x-claude-code-* header, allowlist-free",
    );
}

/// Forwarded leg: the client's `anthropic-beta` set is emitted verbatim
/// and the minted 14-flag Claude Code floor is SUPPRESSED, so Anthropic
/// sees exactly the client's betas.
#[test]
fn forwarded_leg_client_anthropic_beta_wins_and_floor_suppressed() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        true,
    ));
    let req = forwarded_req(&[], &[], &["client-only-beta"]);

    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present with client betas");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    assert!(
        betas.contains(&"client-only-beta"),
        "the client's beta must reach the header; got {value}",
    );
    assert!(
        !betas.contains(&"claude-code-20250219"),
        "the minted CC beta floor must be suppressed on the forwarded leg; got {value}",
    );
    assert!(
        !betas.contains(&"oauth-2025-04-20"),
        "no minted floor beta may leak on the forwarded leg; got {value}",
    );
}

/// Forwarded leg: the standard Anthropic protocol version reaches the
/// upstream (Claude Code and routectl both use 2023-06-01), so the
/// client's version flows through unchanged.
#[test]
fn forwarded_leg_emits_client_anthropic_version() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        true,
    ));
    let req = forwarded_req(&[], &[], &[]);

    assert_eq!(
        outbound_header_value(&provider, &req, "anthropic-version").as_deref(),
        Some("2023-06-01"),
        "the Anthropic protocol version must reach the upstream on the forwarded leg",
    );
}

/// Security: the forwarded bearer is read only as a GATE by
/// build_headers, never emitted as a header value. No outbound header
/// (identity or otherwise) may carry the raw token.
#[test]
fn forwarded_leg_never_leaks_forwarded_token_in_any_header() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        true,
    ));
    let req = forwarded_req(
        &[("x-stainless-package-version", "1.2.3-client")],
        &[("x-claude-code-session-id", "client-sid-abc")],
        &["client-only-beta"],
    );

    let pairs = outbound_header_pairs(&provider, &req);
    for (name, value) in &pairs {
        assert!(
            !value.contains(FORWARDED_TOKEN_CANARY),
            "the forwarded token must never appear in any header value; leaked in {name}: {value}",
        );
    }
}

/// Own-mode-unchanged guard: with `forwarded_bearer` None, even when the
/// carrier happens to hold `stainless_headers` + `claude_code_headers`,
/// the minted fingerprint STILL wins -- the override is gated strictly
/// on the forwarded bearer, not on the mere presence of captured
/// headers. `forward_client_headers` is empty (own-mode secure default),
/// so no captured header reaches the wire.
///
/// The captured `x-claude-code-session-id` also makes this request
/// `is_non_cc() == false` (genuine CC) under the default Auto cloak
/// mode, so the fingerprint-widening beta floor is correctly SUPPRESSED
/// here -- only the unconditional `oauth-2025-04-20` gate flag survives.
#[test]
fn own_mode_keeps_minted_fingerprint_even_with_captured_headers() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("minted-sid-known".into()),
        Vec::new(),
        false,
    ));
    // No forwarded bearer -> own mode, but the carrier is populated as
    // if it had been captured, to prove the gate ignores it.
    let mut req = ChatRequest::default();
    req.routectl_internal.stainless_headers =
        vec![("x-stainless-package-version".into(), "1.2.3-client".into())];
    req.routectl_internal.claude_code_headers =
        vec![("x-claude-code-session-id".into(), "client-sid-abc".into())];

    // Minted Stainless fingerprint wins (client value ignored).
    assert_eq!(
        outbound_header_value(&provider, &req, "x-stainless-package-version").as_deref(),
        Some(minted_stainless_package_version().as_str()),
        "own mode must keep the minted Stainless fingerprint",
    );
    // Minted session id wins (client value ignored, allowlist empty).
    assert_eq!(
        outbound_header_value(&provider, &req, "x-claude-code-session-id").as_deref(),
        Some("minted-sid-known"),
        "own mode must keep the minted session id",
    );
    // Genuine-CC (is_non_cc == false): the fingerprint-widening floor
    // is suppressed, but the OAuth gate flag still reaches the wire.
    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present (oauth gate flag)");
    assert!(
        !value.split(',').any(|b| b.trim() == "claude-code-20250219"),
        "genuine-CC request must NOT get the widening CC beta floor; got {value}",
    );
    assert!(
        value.split(',').any(|b| b.trim() == "oauth-2025-04-20"),
        "the unconditional oauth gate flag must still be present; got {value}",
    );
}

// -- forwarded-leg body transparency -----------------------------------

/// `forwarded_leg` is true EXACTLY when all three legs are positive:
/// the provider is configured `use_forwarded_bearer`, a bearer was
/// captured on this request, AND the egress host is api.anthropic.com.
/// It mirrors `should_use_forwarded_bearer` and is the single gate every
/// body-mutation site consults.
#[test]
fn forwarded_leg_predicate_true_iff_all_three_positive() {
    for use_fwd in [false, true] {
        for has_bearer in [false, true] {
            for anthropic_host in [false, true] {
                let base = if anthropic_host {
                    "https://api.anthropic.com"
                } else {
                    "https://example.invalid"
                };
                let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
                    base,
                    Some("sid".into()),
                    Vec::new(),
                    use_fwd,
                ));
                let req = if has_bearer {
                    forwarded_req(&[], &[], &[])
                } else {
                    ChatRequest::default()
                };
                let expected = use_fwd && has_bearer && anthropic_host;
                assert_eq!(
                    provider.forwarded_leg(&req),
                    expected,
                    "forwarded_leg(use_fwd={use_fwd}, bearer={has_bearer}, host={anthropic_host}) must be {expected}",
                );
            }
        }
    }
}

/// The cloak LANE predicate `is_cloak_lane` is true ONLY for the
/// OauthBearer + exact api.anthropic.com host + non-forwarded
/// combination. It is false on the forwarded leg, for OAuth pointed at
/// another host, and for any non-OAuth auth kind. This is the lane, not
/// the cloak state -- its value is exercised across all combinations
/// here; independence from `is_non_cc`/`CloakMode` is asserted separately.
#[test]
fn is_cloak_lane_true_iff_own_oauth_anthropic() {
    // Own OAuth to the exact api.anthropic.com host, no forwarded bearer.
    let own = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("sid".into()),
        Vec::new(),
        false,
    ));
    assert!(
        own.is_cloak_lane(&ChatRequest::default()),
        "own OAuth to api.anthropic.com (non-forwarded) is the cloak lane",
    );

    // Forwarded leg: use_forwarded_bearer + captured bearer + anthropic
    // host -> forwarded_leg true -> lane false.
    let fwd_provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("sid".into()),
        Vec::new(),
        true,
    ));
    assert!(
        !fwd_provider.is_cloak_lane(&forwarded_req(&[], &[], &[])),
        "the forwarded leg is not the cloak lane",
    );

    // OAuth pointed at a non-anthropic host -> lane false.
    let other_host = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://example.invalid",
        Some("sid".into()),
        Vec::new(),
        false,
    ));
    assert!(
        !other_host.is_cloak_lane(&ChatRequest::default()),
        "OAuth to another host is not the cloak lane",
    );

    // Non-OAuth auth kind (ApiKey) on the anthropic host -> lane false.
    let api_key = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
    assert!(
        !api_key.is_cloak_lane(&ChatRequest::default()),
        "a non-OAuth auth kind is never the cloak lane",
    );
}

/// `is_cloak_lane` is the LANE, not the cloak STATE: its value must not
/// depend on `is_non_cc` or `CloakMode`. Under every cloak mode -- Never
/// (is_non_cc false), Always (is_non_cc true), and Auto -- an own OAuth
/// provider on the anthropic host stays in the lane. This is the guard
/// that keeps `cloak.mode = never` from escaping lane-wide requirements.
#[test]
fn is_cloak_lane_independent_of_cloak_mode_and_is_non_cc() {
    for mode in [CloakMode::Never, CloakMode::Always, CloakMode::Auto] {
        let provider = oauth_provider_with_cloak(CloakConfig {
            mode,
            ..CloakConfig::default()
        });
        // A session header drives is_non_cc under Auto; assert the lane is
        // stable regardless of whether it is present.
        for headers in [Vec::new(), vec![("x-claude-code-session-id", "sid-42")]] {
            let req = req_with_claude_code_headers(headers);
            assert!(
                provider.is_cloak_lane(&req),
                "is_cloak_lane must hold under mode {mode:?} regardless of is_non_cc",
            );
        }
    }
}

/// On the forwarded leg cloak_body is a no-op: it returns None and leaves
/// the body byte-for-byte unchanged, so the client's real body (billing
/// block included) reaches Anthropic untouched.
#[test]
fn cloak_body_forwarded_leg_is_noop() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("session-stable-123".into()),
        Vec::new(),
        true,
    ));
    let req = forwarded_req(&[], &[], &[]);
    let mut body = cloak_test_body();
    let before = body.clone();

    let result = provider.cloak_body(&mut body, &req);

    assert!(
        result.is_none(),
        "cloak_body must return None on the forwarded leg"
    );
    assert_eq!(body, before, "the forwarded leg must not mutate the body");
    assert!(
        body_has_billing(&body),
        "the client billing block must survive the forwarded leg untouched"
    );
}

/// The cch re-sign gate is the OauthBearer + api.anthropic.com surface
/// MINUS the forwarded leg. On the forwarded leg the gate is false, so
/// `resign_cch_in_place` never runs and the client's billing checksum
/// reaches Anthropic verbatim; own mode keeps the gate true. Asserts the
/// exact gate expression from the dispatch methods -- no egress needed.
#[test]
fn resign_gate_false_on_forwarded_leg_true_in_own_mode() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("session-stable-123".into()),
        Vec::new(),
        true,
    ));

    let fwd = forwarded_req(&[], &[], &[]);
    let resign_on_fwd = provider.cfg.auth_kind == AuthKind::OauthBearer
        && is_anthropic_api_host(&provider.cfg.base_url)
        && !provider.forwarded_leg(&fwd);
    assert!(
        !resign_on_fwd,
        "cch re-sign must be gated off on the forwarded leg"
    );

    let own = ChatRequest::default();
    let resign_own = provider.cfg.auth_kind == AuthKind::OauthBearer
        && is_anthropic_api_host(&provider.cfg.base_url)
        && !provider.forwarded_leg(&own);
    assert!(
        resign_own,
        "cch re-sign must still run in own mode (no forwarded bearer)"
    );
}

/// build_headers on the forwarded leg emits the client's anthropic-beta
/// set VERBATIM, bypassing the operator `allowed_betas` allowlist: the
/// client's real beta fingerprint must reach Anthropic unfiltered.
#[test]
fn forwarded_leg_anthropic_beta_header_bypasses_allowlist() {
    let cfg = AnthropicApiConfig {
        allowed_betas: vec!["allowed-beta".into()],
        ..oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("sid".into()),
            Vec::new(),
            true,
        )
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = forwarded_req(&[], &[], &["allowed-beta", "client-blocked"]);

    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    assert!(
        betas.contains(&"client-blocked"),
        "a client beta not in allowed_betas must still pass verbatim on the forwarded leg; got {value}",
    );
    assert!(
        betas.contains(&"allowed-beta"),
        "the client's allowed beta must pass too; got {value}",
    );
    assert!(
        !betas.contains(&"oauth-2025-04-20"),
        "no minted OAuth floor beta may leak onto the forwarded-leg header; got {value}",
    );
    assert!(
        !betas.contains(&"claude-code-20250219"),
        "no minted Claude Code floor beta may leak onto the forwarded-leg header; got {value}",
    );
}

/// Own-mode counterpart: with `allowed_betas` set, a client beta not on
/// the allowlist IS stripped from the anthropic-beta header. This pins
/// that only the forwarded leg bypasses the filter.
#[test]
fn own_mode_anthropic_beta_header_applies_allowlist() {
    let cfg = AnthropicApiConfig {
        allowed_betas: vec!["allowed-beta".into()],
        ..oauth_cfg_with_session(
            "https://api.anthropic.com",
            Some("sid".into()),
            Vec::new(),
            false,
        )
    };
    let provider = AnthropicApiProvider::new(cfg);
    let req = ChatRequest {
        anthropic_beta: vec!["allowed-beta".into(), "client-blocked".into()],
        ..Default::default()
    };

    let value = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("anthropic-beta header must be present");
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    assert!(
        !betas.contains(&"client-blocked"),
        "own mode must strip a client beta not in allowed_betas; got {value}",
    );
    assert!(
        betas.contains(&"allowed-beta"),
        "the allowlisted beta must survive; got {value}",
    );
}

/// Body-strip proof (drives the request.rs body-field decision):
/// request.rs `normalize` populates the body `anthropic_beta` field, but
/// every api.anthropic.com egress path strips it before the wire --
/// complete and stream `remove("anthropic_beta")`, count_tokens reshapes
/// through the field allowlist. So the request.rs site is inert on
/// egress and needs no forwarded-leg gate.
#[test]
fn body_anthropic_beta_field_absent_on_all_egress_paths() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("sid".into()),
        Vec::new(),
        false,
    ));
    let req = ChatRequest {
        anthropic_beta: vec!["client-beta".into()],
        ..Default::default()
    };

    let normalized = provider.normalize_request(&req).expect("normalize");
    assert!(
        normalized.get("anthropic_beta").is_some(),
        "normalize must populate the body anthropic_beta field (the site under proof)",
    );

    // complete() path: strip stream + anthropic_beta, then cloak.
    let mut complete_body = normalized.clone();
    if let Some(obj) = complete_body.as_object_mut() {
        obj.remove("stream");
        obj.remove("anthropic_beta");
    }
    provider.cloak_body(&mut complete_body, &req);
    assert!(
        complete_body.get("anthropic_beta").is_none(),
        "complete egress body must not carry anthropic_beta",
    );

    // stream() path: set stream=true, strip anthropic_beta, then cloak.
    let mut stream_body = normalized.clone();
    if let Some(obj) = stream_body.as_object_mut() {
        obj.insert("stream".into(), serde_json::Value::Bool(true));
        obj.remove("anthropic_beta");
    }
    provider.cloak_body(&mut stream_body, &req);
    assert!(
        stream_body.get("anthropic_beta").is_none(),
        "stream egress body must not carry anthropic_beta",
    );

    // count_tokens() path: cloak, then reshape through the allowlist.
    let mut ct = normalized.clone();
    provider.cloak_body(&mut ct, &req);
    let ct_body = build_count_tokens_body(&ct);
    assert!(
        ct_body.get("anthropic_beta").is_none(),
        "count_tokens egress body must not carry anthropic_beta",
    );
}

// -- cloak_body gate + body rewrite ------------------------------------
/// Body carrying a Claude Code billing block + a client system block,
/// used by the cloak_body tests so both the billing strip and the
/// (non-)identity-stamp are observable in one body.
fn cloak_test_body() -> Value {
    serde_json::json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1; cch=abcde;"},
            {"type": "text", "text": "client system prompt"},
        ],
        "messages": [{"role": "user", "content": "hello"}]
    })
}

/// True when any `system` block's text starts with the billing prefix.
fn body_has_billing(body: &Value) -> bool {
    body["system"].as_array().is_some_and(|arr| {
        arr.iter().any(|b| {
            b["text"]
                .as_str()
                .is_some_and(|t| t.trim_start().starts_with("x-anthropic-billing-header:"))
        })
    })
}

/// (a) OauthBearer + api.anthropic.com + NON-CC req (no captured
/// `x-claude-code-session-id`): the body's `system` is reduced to the
/// interactive identity line only, the client system is relocated into
/// the first user message as a `<system-reminder>`, `metadata.user_id` is
/// minted, AND the billing block is stripped.
#[test]
fn cloak_body_non_cc_stamps_identity_and_metadata_and_strips_billing() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("session-stable-123".into()),
        Vec::new(),
        false,
    ));
    // Non-CC: no x-claude-code-session-id captured.
    let req = req_with_claude_code_headers(vec![("x-claude-code-agent-id", "aid-7")]);
    let mut body = cloak_test_body();

    provider.cloak_body(&mut body, &req);

    // System is identity-only.
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1, "system must be reduced to identity only");
    assert_eq!(
        arr[0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    // Client system relocated into the first user message as a reminder.
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "<system-reminder>\nclient system prompt\n</system-reminder>"
    );
    // Metadata user_id minted.
    assert!(
        body["metadata"]["user_id"].is_string(),
        "non-CC cloak must mint metadata.user_id"
    );
    // Billing block stripped.
    assert!(
        !body_has_billing(&body),
        "billing block must be stripped on the non-CC path"
    );
}

/// (b) OauthBearer + api.anthropic.com + GENUINE-CC req (captured
/// `x-claude-code-session-id`): the billing block is stripped, but NO
/// identity stamp and NO metadata mint.
#[test]
fn cloak_body_genuine_cc_strips_billing_only() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("session-stable-123".into()),
        Vec::new(),
        false,
    ));
    // Genuine CC: the session-id header is present in the capture.
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    let mut body = cloak_test_body();

    provider.cloak_body(&mut body, &req);

    // Billing block stripped, leaving only the client system block.
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1, "only the client system block must remain");
    assert_eq!(arr[0]["text"], "client system prompt");
    assert!(
        !body_has_billing(&body),
        "billing block must be stripped on the genuine-CC path"
    );
    // No identity stamp, no metadata mint.
    assert!(
        body.get("metadata").is_none(),
        "genuine-CC path must not mint metadata"
    );
    // The genuine-CC path must not relocate the client system: no
    // system-reminder block appears anywhere.
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains("<system-reminder>"),
        "genuine-CC path must not add a system-reminder block"
    );
}

/// (c) ApiKey path (api.anthropic.com): the gate skips, so the body is
/// completely untouched -- billing block stays, no identity, no metadata.
#[test]
fn cloak_body_api_key_path_leaves_body_untouched() {
    let provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
    let req = req_with_claude_code_headers(Vec::new());
    let mut body = cloak_test_body();
    let before = body.clone();

    provider.cloak_body(&mut body, &req);

    assert_eq!(
        body, before,
        "ApiKey path must leave the body untouched (gate skips)"
    );
}

/// (d) OauthBearer + NON-anthropic host: the gate skips, so the body is
/// completely untouched.
#[test]
fn cloak_body_non_anthropic_host_leaves_body_untouched() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://example.invalid",
        Some("session-stable-123".into()),
        Vec::new(),
        false,
    ));
    let req = req_with_claude_code_headers(Vec::new());
    let mut body = cloak_test_body();
    let before = body.clone();

    provider.cloak_body(&mut body, &req);

    assert_eq!(
        body, before,
        "non-anthropic host must leave the body untouched (gate skips)"
    );
}

// -- cloak mode (T6) ---------------------------------------------------

/// Build an OauthBearer + api.anthropic.com provider with an explicit
/// `CloakConfig` and a stable session id, for the mode tests.
fn oauth_provider_with_cloak(cloak: CloakConfig) -> AnthropicApiProvider {
    let cfg = AnthropicApiConfig {
        id: "test".into(),
        auth: Arc::new(StaticToken::new("oat-token")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: Some("session-stable-123".into()),
        cloak,
        use_forwarded_bearer: false,

        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    AnthropicApiProvider::new(cfg)
}

/// `is_non_cc` under `CloakMode::Always` is unconditionally true,
/// regardless of whether a session-id header is present.
#[test]
fn is_non_cc_always_is_true() {
    let provider = oauth_provider_with_cloak(CloakConfig {
        mode: CloakMode::Always,
        ..CloakConfig::default()
    });
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    assert!(provider.is_non_cc(&req));
}

/// `is_non_cc` under `CloakMode::Never` is unconditionally false,
/// regardless of whether a session-id header is present. This arm has
/// no inline equivalent today -- it exists for `build_headers`, which
/// (unlike `cloak_body`) does not early-return on `Never`.
#[test]
fn is_non_cc_never_is_false() {
    let provider = oauth_provider_with_cloak(CloakConfig {
        mode: CloakMode::Never,
        ..CloakConfig::default()
    });
    let req = req_with_claude_code_headers(Vec::new());
    assert!(!provider.is_non_cc(&req));
}

/// `is_non_cc` under `CloakMode::Auto` is false when a captured
/// `x-claude-code-session-id` header is present, matched
/// case-insensitively.
#[test]
fn is_non_cc_auto_is_false_when_session_header_present() {
    let provider = oauth_provider_with_cloak(CloakConfig::default());
    let req = req_with_claude_code_headers(vec![("X-Claude-Code-Session-Id", "sid-42")]);
    assert!(!provider.is_non_cc(&req));
}

/// `is_non_cc` under `CloakMode::Auto` is true when no session-id
/// header was captured.
#[test]
fn is_non_cc_auto_is_true_when_session_header_absent() {
    let provider = oauth_provider_with_cloak(CloakConfig::default());
    let req = req_with_claude_code_headers(Vec::new());
    assert!(provider.is_non_cc(&req));
}

/// `mode = never` skips ALL cloak transforms: billing block NOT stripped,
/// identity NOT injected, `mcp_` NOT normalized, and `cloak_body` returns
/// None.
#[test]
fn cloak_mode_never_skips_all_transforms() {
    let provider = oauth_provider_with_cloak(CloakConfig {
        mode: CloakMode::Never,
        ..CloakConfig::default()
    });
    // Non-CC request, with a tool that would normally be normalized.
    let req = req_with_claude_code_headers(Vec::new());
    let mut body = serde_json::json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1"},
            {"type": "text", "text": "client system prompt"},
        ],
        "tools": [{"name": "mcp_foo"}]
    });
    let before = body.clone();

    let result = provider.cloak_body(&mut body, &req);

    assert!(
        result.is_none(),
        "mode=never must return None from cloak_body"
    );
    assert_eq!(body, before, "mode=never must leave the body untouched");
    // Explicitly: billing block survives and mcp_ is NOT normalized.
    assert!(
        body_has_billing(&body),
        "billing block must survive mode=never"
    );
    assert_eq!(body["tools"][0]["name"], "mcp_foo");
}

/// `mode = always` cloaks as a non-CC client even when the request DID
/// carry an `x-claude-code-session-id` capture (which `Auto` would treat
/// as genuine CC): identity stamped + metadata minted.
#[test]
fn cloak_mode_always_stamps_identity_even_with_session_header() {
    let provider = oauth_provider_with_cloak(CloakConfig {
        mode: CloakMode::Always,
        ..CloakConfig::default()
    });
    // Genuine-CC-looking request: session-id header present.
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    let mut body = cloak_test_body();

    provider.cloak_body(&mut body, &req);

    // Despite the session header, identity is stamped and metadata minted.
    assert_eq!(
        body["system"][0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert!(
        body["metadata"]["user_id"].is_string(),
        "mode=always must mint metadata.user_id even with a session header"
    );
    assert!(!body_has_billing(&body), "billing block must be stripped");
}

/// `mode = auto` (the default) keeps the original heuristic: a request
/// WITH a session-id capture is treated as genuine CC (no identity stamp,
/// no metadata), billing still stripped.
#[test]
fn cloak_mode_auto_matches_increment1_for_genuine_cc() {
    let provider = oauth_provider_with_cloak(CloakConfig::default());
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    let mut body = cloak_test_body();

    provider.cloak_body(&mut body, &req);

    // Genuine CC under Auto: only the client block remains, no metadata.
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["text"], "client system prompt");
    assert!(body.get("metadata").is_none());
    assert!(!body_has_billing(&body));
}

/// Build a `reqwest::Response` from a status + body for driving
/// `read_anthropic_error` directly, without a live HTTP round-trip or
/// the `complete()` path's global-tracing side effects (which race the
/// `#[traced_test]` upstream_log tests in this crate's test binary).
///
/// `http::Response` is only in scope under the `bedrock` feature
/// (`dep:http`), which the default and `--all-features` builds both
/// enable -- so these tests run under the standard gate.
#[cfg(feature = "bedrock")]
fn reqwest_response(status: u16, body: &str) -> reqwest::Response {
    let http_resp = http::Response::builder()
        .status(status)
        .body(body.to_string())
        .expect("build http::Response");
    reqwest::Response::from(http_resp)
}

/// A structured Anthropic `{error:...}` 400 must carry the RAW JSON
/// envelope in `Error::Upstream.body` so the ingress sanitizer can
/// re-extract the upstream's own `error.message` for the client. This
/// is the recovery lever: Claude Code self-heals a stale thinking-block
/// 400 only if it can SEE the message.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn read_anthropic_error_carries_raw_envelope_for_structured_400() {
    let raw = "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\
                    \"message\":\"messages.23.content.5: `thinking` or `redacted_thinking` \
                    blocks in the latest assistant message cannot be modified.\"}}";
    let resp = reqwest_response(400, raw);

    let (msg, err) = read_anthropic_error("anthropic_oauth_prod", 400, resp).await;

    // The returned `msg` stays the clean extracted message for logging.
    assert!(
        msg.contains("cannot be modified"),
        "returned msg is the extracted message: {msg:?}"
    );
    match err {
        Error::Upstream {
            status,
            body,
            upstream_type,
            ..
        } => {
            assert_eq!(status, 400);
            assert_eq!(upstream_type.as_deref(), Some("invalid_request_error"));
            // `.body` must be the RAW envelope so the ingress sanitizer
            // re-parses `/error/message`.
            let parsed: Value =
                serde_json::from_str(&body).expect("body must still be the raw JSON envelope");
            assert_eq!(
                parsed.pointer("/error/message").and_then(Value::as_str),
                Some(
                    "messages.23.content.5: `thinking` or `redacted_thinking` \
                         blocks in the latest assistant message cannot be modified."
                )
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

/// A non-JSON upstream body (HTML gateway page) must NOT be carried raw
/// in `.body`; the sanitized excerpt is stored so the ingress sanitizer
/// falls back to a status-only client message and nothing leaks.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn read_anthropic_error_sanitizes_non_json_body() {
    let resp = reqwest_response(
        502,
        "<html><body>upstream-host gateway timeout</body></html>",
    );

    let (_msg, err) = read_anthropic_error("anthropic_oauth_prod", 502, resp).await;

    match err {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 502);
            assert!(
                !body.contains("upstream-host"),
                "raw HTML body must not be carried in .body: {body:?}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

/// A mantle 403 carrying a namespaced AWS `__type` must surface the bare
/// exception token in `upstream_type` (403 already classifies Auth by
/// status; the lifted token is what makes the log truthful). The
/// free-text message is scrubbed by the shared Bedrock 403 path -- every
/// 403 collapses to the generic "bedrock access denied" client message
/// (the actionable classifier survives in `upstream_type`), so an AWS
/// AccessDenied body can never leak a principal ARN / account / resource.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn read_anthropic_error_lifts_aws_signature_token_from_403() {
    let raw = r#"{"__type":"com.amazonaws.bedrock#SignatureDoesNotMatch","message":"The request signature we calculated does not match."}"#;
    let resp = reqwest_response(403, raw);

    let (msg, err) = read_anthropic_error("mantle_prod", 403, resp).await;

    assert_eq!(
        msg, "bedrock access denied",
        "a 403 free-text message must collapse to the generic scrub: {msg:?}"
    );
    match err {
        Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            body,
            ..
        } => {
            assert_eq!(status, 403);
            assert_eq!(upstream_type.as_deref(), Some("SignatureDoesNotMatch"));
            assert_eq!(upstream_code, None);
            // The AWS body is never carried raw on the mantle lift; the
            // client-facing body is the scrubbed message.
            assert!(
                !body.contains("__type"),
                "AWS envelope must not be carried raw in .body: {body:?}"
            );
            assert_eq!(
                body, "bedrock access denied",
                "the client-facing body must be the scrubbed message: {body:?}"
            );
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

/// A mantle 429 carrying a bare AWS `code` token must surface it in
/// `upstream_code`.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn read_anthropic_error_lifts_aws_throttling_code_from_429() {
    let raw = r#"{"code":"ThrottlingException","Message":"Too many requests"}"#;
    let resp = reqwest_response(429, raw);

    let (msg, err) = read_anthropic_error("mantle_prod", 429, resp).await;

    assert!(msg.contains("Too many requests"), "extracted msg: {msg:?}");
    match err {
        Error::Upstream {
            status,
            upstream_type,
            upstream_code,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(upstream_type, None);
            assert_eq!(upstream_code.as_deref(), Some("ThrottlingException"));
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

/// The Anthropic `error.type` shape wins over any sibling AWS keys, so a
/// body carrying both keeps the Anthropic classifier and never lifts the
/// AWS token.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn read_anthropic_error_prefers_anthropic_shape_over_aws() {
    let raw = r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"},"__type":"com.amazonaws.bedrock#ThrottlingException"}"#;
    let resp = reqwest_response(529, raw);

    let (_msg, err) = read_anthropic_error("mantle_prod", 529, resp).await;

    match err {
        Error::Upstream {
            upstream_type,
            upstream_code,
            ..
        } => {
            assert_eq!(upstream_type.as_deref(), Some("overloaded_error"));
            assert_eq!(upstream_code, None);
        }
        other => panic!("expected Error::Upstream, got {other:?}"),
    }
}

/// Arbitrary JSON, non-JSON, empty, and oversized bodies must never
/// panic and must degrade to the sanitized-excerpt fallback with no
/// lifted tokens.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn read_anthropic_error_never_panics_on_malformed_bodies() {
    let huge = "x".repeat(crate::http_client::MAX_RESPONSE_BODY_BYTES * 2);
    let cases: [&str; 5] = [
        "",
        "not json at all",
        r#"{"random":[1,2,3],"nested":{"deep":true}}"#,
        r#"{"__type":42,"code":{"not":"a string"}}"#,
        &huge,
    ];
    for (idx, body) in cases.into_iter().enumerate() {
        let resp = reqwest_response(400, body);
        let (_msg, err) = read_anthropic_error("mantle_prod", 400, resp).await;
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                ..
            } => {
                assert_eq!(status, 400);
                assert_eq!(upstream_type, None, "case {idx}");
                assert_eq!(upstream_code, None, "case {idx}");
            }
            other => panic!("expected Error::Upstream, got {other:?}"),
        }
    }
}

// -- forwarded-bearer host-pinned token resolution --------------------

/// A `TokenSource` whose `token()` ALWAYS errors. Used to PROVE that
/// the forwarded-passthrough path never calls `self.cfg.auth.token()`:
/// if the resolver touched this source, resolution would fail and the
/// test would observe an `Err` instead of the forwarded token. The
/// synthetic pure-proxy provider has no live routectl credential, so
/// this models "calling cfg.auth.token() here would error".
#[derive(Debug)]
struct FailingTokenSource;

#[async_trait]
impl TokenSource for FailingTokenSource {
    async fn token(&self) -> Result<String> {
        Err(Error::Auth(
            "FailingTokenSource: token() must not be called on the forwarded path".into(),
        ))
    }
}

/// Build an OauthBearer provider with a chosen `base_url`, token
/// source, and forwarded-gate setting, mirroring a
/// `credential_source = "forwarded"` provider entry (OauthBearer,
/// `api.anthropic.com` by default). Pass `use_forwarded_bearer: false`
/// to model a coexisting own-creds Anthropic provider instead.
fn oauth_cfg_with_auth(
    base_url: &str,
    auth: Arc<dyn TokenSource>,
    use_forwarded_bearer: bool,
) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "test".into(),
        auth,
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

/// A default request carrying a forwarded first-party bearer, as the
/// ingress populates it in forwarded (pure-proxy) mode on the MITM
/// Anthropic leg. `RoutectlInternal` is `#[non_exhaustive]`, so mutate
/// the single field on the default value.
fn req_with_forwarded_bearer(token: &str) -> ChatRequest {
    let mut req = ChatRequest::default();
    req.routectl_internal.forwarded_bearer =
        Some(routectl_core::ForwardedBearer::new(token.to_string()));
    req
}

/// Resolve the effective token through the host-pinned resolver, stamp
/// it via `build_headers`, and return the built outbound request so a
/// test can inspect the exact headers that would go on the wire.
async fn build_wire_request(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
) -> reqwest::Request {
    let token = provider
        .resolve_effective_token(req)
        .await
        .expect("effective token must resolve");
    let client = reqwest::Client::new();
    let rb = client.post("http://127.0.0.1/test");
    provider
        .build_headers(rb, req, &token, None)
        .0
        .build()
        .expect("build outbound request")
}

/// True when `needle` appears in ANY header value on the built request.
fn any_header_value_contains(request: &reqwest::Request, needle: &str) -> bool {
    request
        .headers()
        .iter()
        .filter_map(|(_, v)| v.to_str().ok())
        .any(|v| v.contains(needle))
}

/// forwarded_bearer Some + base_url host == api.anthropic.com: the
/// resolver returns the FORWARDED token and NEVER calls
/// `self.cfg.auth.token()`. Proof: the auth source ERRORS on every
/// call, yet resolution succeeds -- so the resolver could not have
/// touched it. This is the errors-if-cfg.auth.token()-called proof.
#[tokio::test]
async fn resolve_forwarded_bearer_on_anthropic_host_skips_cfg_auth() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com",
        Arc::new(FailingTokenSource),
        true,
    ));
    let req = req_with_forwarded_bearer("forwarded-full-scope-token");

    let token = provider
        .resolve_effective_token(&req)
        .await
        .expect("forwarded path must not call cfg.auth.token()");

    assert_eq!(
        token, "forwarded-full-scope-token",
        "forwarded token must be used verbatim as the effective token"
    );
}

/// WIRE: on the anthropic host, the forwarded token is stamped as the
/// outbound `Authorization: Bearer <forwarded>` (the synthetic
/// pure-proxy provider is OauthBearer). End-to-end through
/// `build_headers`, with a failing auth source that is never consulted.
#[tokio::test]
async fn forwarded_bearer_stamped_as_bearer_on_anthropic_host() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com",
        Arc::new(FailingTokenSource),
        true,
    ));
    let req = req_with_forwarded_bearer("forwarded-full-scope-token");

    let request = build_wire_request(&provider, &req).await;

    let auth = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        auth,
        Some("Bearer forwarded-full-scope-token"),
        "forwarded token must be stamped as the outbound Bearer on the anthropic host"
    );
}

/// base_url host != api.anthropic.com (a proxy / self-host) +
/// forwarded_bearer Some: the forwarded token is IGNORED. The resolver
/// returns the provider's OWN token and the forwarded token never
/// appears on any outbound header for that host.
#[tokio::test]
async fn forwarded_bearer_ignored_on_non_anthropic_host() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://proxy.example.com",
        Arc::new(StaticToken::new("provider-own-token")),
        true,
    ));
    let req = req_with_forwarded_bearer("forwarded-should-be-ignored");

    let token = provider
        .resolve_effective_token(&req)
        .await
        .expect("non-anthropic host resolves the provider's own token");
    assert_eq!(
        token, "provider-own-token",
        "non-anthropic host must resolve the provider's own token, not the forwarded one"
    );

    let request = build_wire_request(&provider, &req).await;
    assert!(
        !any_header_value_contains(&request, "forwarded-should-be-ignored"),
        "the forwarded token must never reach the wire on a non-anthropic host"
    );
    assert_eq!(
        request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer provider-own-token"),
        "the provider's own resolved token is what gets stamped on a non-anthropic host"
    );
}

/// A sibling-domain look-alike base (`api.anthropic.com.evil.example`)
/// is NOT the anthropic host: the forwarded full-scope token must NOT
/// be sent there. Defends the exact-host pin end-to-end through the
/// resolver (guards against a substring host check leaking the token to
/// a takeover domain).
#[tokio::test]
async fn forwarded_bearer_ignored_on_lookalike_anthropic_host() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com.evil.example",
        Arc::new(StaticToken::new("provider-own-token")),
        true,
    ));
    let req = req_with_forwarded_bearer("forwarded-full-scope-token");

    let token = provider
        .resolve_effective_token(&req)
        .await
        .expect("look-alike host resolves the provider's own token");
    assert_eq!(
        token, "provider-own-token",
        "a look-alike host must not receive the forwarded token"
    );

    let request = build_wire_request(&provider, &req).await;
    assert!(
        !any_header_value_contains(&request, "forwarded-full-scope-token"),
        "the forwarded token must never reach a look-alike anthropic host"
    );
}

/// The coexistence-bug regression: an OWN-creds Anthropic provider
/// (`use_forwarded_bearer` false) on the exact anthropic host with a
/// floating captured bearer present (e.g. captured for a sibling
/// forwarded provider on the same router) must NOT consume it. The
/// resolver returns the provider's own token, and no outbound header
/// carries the floating bearer.
#[tokio::test]
async fn own_provider_ignores_floating_forwarded_bearer_on_anthropic_host() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com",
        Arc::new(StaticToken::new("provider-own-token")),
        false,
    ));
    let req = req_with_forwarded_bearer("floating-forwarded-bearer");

    let token = provider
        .resolve_effective_token(&req)
        .await
        .expect("own-mode provider resolves its own token");
    assert_eq!(
        token, "provider-own-token",
        "an own-mode provider must resolve its own token even with a floating bearer present"
    );

    let request = build_wire_request(&provider, &req).await;
    assert!(
        !any_header_value_contains(&request, "floating-forwarded-bearer"),
        "the floating bearer must never reach the wire for an own-mode provider"
    );
    assert_eq!(
        request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer provider-own-token"),
        "the own-mode provider's own token is what gets stamped, not the floating bearer"
    );
}

/// The pure `should_use_forwarded_bearer` predicate shared by
/// `resolve_effective_token` and the `build_headers` forwarded leg.
/// Baseline: TRUE only when all three legs hold (configured forwarded +
/// bearer present + exact anthropic host). Each case below flips
/// exactly one leg off the baseline and must land on false --
/// including the two coexistence cases: a forwarded provider on a
/// non-anthropic host, and an own-mode provider with a bearer present
/// on the exact anthropic host. Host-pinned egress cannot be driven
/// through wiremock, so this matrix is the full end-to-end proof of the
/// gate's logic.
#[test]
fn should_use_forwarded_bearer_gate_matrix() {
    let cases: &[(bool, bool, &str, bool)] = &[
        // (use_forwarded_bearer, has_bearer, base_url, expected)
        (true, true, "https://api.anthropic.com", true),
        (false, true, "https://api.anthropic.com", false),
        (true, false, "https://api.anthropic.com", false),
        (true, true, "https://proxy.example.com", false),
        (false, false, "https://api.anthropic.com", false),
        (false, true, "https://proxy.example.com", false),
        (true, false, "https://proxy.example.com", false),
        (false, false, "https://proxy.example.com", false),
        (true, true, "https://api.anthropic.com.evil.example", false),
    ];

    for (use_forwarded_bearer, has_bearer, base_url, expected) in cases {
        assert_eq!(
            should_use_forwarded_bearer(*use_forwarded_bearer, *has_bearer, base_url),
            *expected,
            "use_forwarded_bearer={use_forwarded_bearer} has_bearer={has_bearer} \
                 base_url={base_url} expected={expected}"
        );
    }
}

/// forwarded_bearer None on the anthropic host: identical to today --
/// the resolver returns the provider's own token via cfg.auth.token().
#[tokio::test]
async fn forwarded_bearer_none_resolves_provider_token() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com",
        Arc::new(StaticToken::new("provider-token")),
        true,
    ));
    // ChatRequest::default() leaves forwarded_bearer None.
    let req = ChatRequest::default();

    let token = provider
        .resolve_effective_token(&req)
        .await
        .expect("token resolves");
    assert_eq!(
        token, "provider-token",
        "the None path must resolve the provider's own token"
    );
}

/// forwarded_bearer None on the anthropic host STILL calls
/// cfg.auth.token() -- the None path is behaviorally identical to the
/// pre-passthrough egress. Proof: with a failing auth source and no
/// forwarded token, resolution errors (it can only error by calling
/// cfg.auth.token()).
#[tokio::test]
async fn forwarded_bearer_none_still_calls_cfg_auth() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com",
        Arc::new(FailingTokenSource),
        true,
    ));
    let req = ChatRequest::default();

    let result = provider.resolve_effective_token(&req).await;
    assert!(
        result.is_err(),
        "the None path must resolve through cfg.auth.token(), which errors here"
    );
}

/// The resolver must never log the forwarded token. Drive the forwarded
/// path under a log capture and assert the token string is absent from
/// every emitted event -- a regression guard against a future debug log
/// in the resolver. Uses a current-thread runtime so the test stays on
/// the crate's established `#[traced_test] #[test]` shape.
#[traced_test]
#[test]
fn resolve_forwarded_bearer_does_not_log_token() {
    let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
        "https://api.anthropic.com",
        Arc::new(FailingTokenSource),
        true,
    ));
    let secret = "forwarded-full-scope-SECRET-abc123";
    let req = req_with_forwarded_bearer(secret);

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build current-thread runtime");
    let token = rt
        .block_on(provider.resolve_effective_token(&req))
        .expect("forwarded token resolves");
    assert_eq!(token, secret);

    assert!(
        !logs_contain(secret),
        "the forwarded token must never be logged by the resolver"
    );
}

// -- beta-decision observability ----------------------------------------

/// A genuine Claude Code request (a captured `x-claude-code-session-id`
/// header) classifies as NOT non-CC, but the mandatory OAuth gate still
/// fires independent of that classification. `context-1m-2025-08-07` is
/// not in the floor, so it never widens the beta set for a genuine CC
/// client either.
#[test]
fn beta_decision_reflects_genuine_cc_request() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = req_with_claude_code_headers(vec![("x-claude-code-session-id", "sid-42")]);
    let client = reqwest::Client::new();
    let rb = client.post("http://127.0.0.1/test");
    let (_rb, decision) = provider.build_headers(rb, &req, "test-token", None);

    assert!(
        !decision.is_non_cc,
        "a captured session-id header must classify as genuine-CC"
    );
    assert!(
        decision.oauth_added,
        "the mandatory oauth gate must fire independent of is_non_cc"
    );
    assert!(
        !decision.has_context_1m_beta,
        "a genuine-CC request must not be floor-widened with context-1m"
    );
}

/// The mirror case: no captured session-id header classifies as
/// non-CC. The beta floor no longer carries `context-1m-2025-08-07`, so
/// the classification alone never widens the outgoing beta set with it
/// -- a true `has_context_1m_beta` now means the CALLER (or an operator
/// `header_extras`) asked for it, never floor contamination.
#[test]
fn beta_decision_reflects_non_cc_request() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = req_with_claude_code_headers(Vec::new());
    let client = reqwest::Client::new();
    let rb = client.post("http://127.0.0.1/test");
    let (_rb, decision) = provider.build_headers(rb, &req, "test-token", None);

    assert!(
        decision.is_non_cc,
        "no captured session-id header must classify as non-CC"
    );
    assert!(
        decision.oauth_added,
        "the mandatory oauth gate must fire for non-CC too"
    );
    assert!(
        !decision.has_context_1m_beta,
        "a non-CC request must NOT be floor-widened with context-1m"
    );
}

/// Drive `log_beta_decision_on_4xx` directly (bypassing a full HTTP
/// round-trip) and assert the beta-context fields land on the emitted
/// event, so a beta-caused 400 recurrence is diagnosable without
/// enabling header tracing.
#[traced_test]
#[test]
fn log_beta_decision_on_4xx_emits_beta_context_fields() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let decision = BetaDecision {
        is_non_cc: true,
        forwarded_leg: false,
        cloak_mode: CloakMode::Auto,
        oauth_added: true,
        has_context_1m_beta: true,
        has_context_management_beta: false,
    };

    provider.log_beta_decision_on_4xx(400, &decision, "invalid_request_error: bad beta");

    assert!(logs_contain(
        "anthropic-api oauth 4xx beta decision context"
    ));
    assert!(logs_contain("status=400"));
    assert!(logs_contain("is_non_cc=true"));
    assert!(logs_contain("oauth_added=true"));
    assert!(logs_contain("has_context_1m_beta=true"));
    assert!(logs_contain("has_context_management_beta=false"));
}

/// `should_log_beta_4xx` is the single gate shared by `complete()`,
/// `stream()`, and `count_tokens()` -- exercise the full matrix here
/// instead of trusting three copy-pasted conditions to stay in sync.
/// Baseline: TRUE only for a 4xx, OauthBearer, api.anthropic.com,
/// non-forwarded request. Each deviation below flips exactly one
/// dimension of that baseline and must land on false.
#[test]
fn should_log_beta_4xx_gate_matrix() {
    let oauth_provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    assert!(
        oauth_provider.should_log_beta_4xx(400, false),
        "baseline: 4xx + OauthBearer + api.anthropic.com + own leg must fire"
    );

    for status in [500, 502] {
        assert!(
            !oauth_provider.should_log_beta_4xx(status, false),
            "5xx status {status} must not fire (beta gating cannot cause a 5xx)"
        );
    }

    let api_key_provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));
    assert!(
        !api_key_provider.should_log_beta_4xx(400, false),
        "ApiKey auth must not fire even on api.anthropic.com"
    );

    let non_anthropic_provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://example.invalid",
        None,
        Vec::new(),
        false,
    ));
    assert!(
        !non_anthropic_provider.should_log_beta_4xx(400, false),
        "a non-anthropic base_url must not fire even with OauthBearer"
    );

    assert!(
        !oauth_provider.should_log_beta_4xx(400, true),
        "forwarded_leg must suppress the log (own-token lane only)"
    );

    for status in [200, 204, 301] {
        assert!(
            !oauth_provider.should_log_beta_4xx(status, false),
            "2xx/3xx status {status} must not fire"
        );
    }
}

// -----------------------------------------------------------------------
// Bedrock mantle lane: header composition + post-build signing.
// -----------------------------------------------------------------------

/// A mantle-lane config with a resolved bearer credential. The bearer
/// path keeps `resolve` synchronous-cheap (no AWS chain load) while
/// exercising the same `Some(mantle)` lane selection as SigV4.
#[cfg(feature = "bedrock")]
async fn mantle_cfg_bearer() -> AnthropicApiConfig {
    let creds = crate::bedrock::auth::resolve(
        &crate::bedrock::BedrockCreds::BearerKey {
            key: "mantle-key-xyz".into(),
        },
        "us-west-2",
    )
    .await
    .unwrap();
    // api_key_ref is empty on the mantle lane -- the resolved creds carry
    // auth, so the token is never presented as x-api-key.
    let mut cfg = AnthropicApiConfig::new("mantle-test", "");
    cfg.mantle = Some(MantleAuth {
        region: "us-west-2".into(),
        creds,
    });
    cfg
}

/// The mantle lane attaches NO `x-api-key` and NO `Authorization` in
/// `build_headers` -- the signer owns auth and stamps it post-build.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn mantle_build_headers_omit_x_api_key_and_authorization() {
    let provider = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    let req = ChatRequest::default();
    let names = outbound_header_names(&provider, &req);
    assert!(
        !names.iter().any(|n| n == "x-api-key"),
        "mantle lane must not attach x-api-key; got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "authorization"),
        "build_headers must not attach Authorization on the mantle lane \
         (the signer owns it); got: {names:?}"
    );
}

/// anthropic-version stamps on the mantle lane exactly as on the
/// first-party lane (default 2023-06-01 is correct for mantle).
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn mantle_build_headers_stamp_anthropic_version() {
    let provider = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    let req = ChatRequest::default();
    assert_eq!(
        outbound_header_value(&provider, &req, "anthropic-version").as_deref(),
        Some("2023-06-01"),
    );
}

/// The mantle Anthropic lane carries betas on the `anthropic-beta` HEADER
/// (its body-side `anthropic_beta` is stripped before send, like the
/// first-party lane), so the body-derived capability union must fire here
/// too: a mantle request whose body carries `output_config.format` gets the
/// gating beta despite the lane running no beta floor at all.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn mantle_build_headers_carry_structured_outputs_beta_for_output_config_format() {
    let provider = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    let req = ChatRequest::default();
    let body = serde_json::json!({
        "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
    });

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the mantle lane must emit the gating beta for a structured-output body");
    assert_eq!(
        value,
        routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA,
        "the mantle lane composes no floor, so the union is the only source; got: {value}"
    );
}

/// No Claude-Code SDK fingerprint reaches the wire on the mantle lane:
/// the identity headers, session id, request id, and Stainless headers
/// all gate on OauthBearer, which the mantle lane forbids.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn mantle_build_headers_emit_no_claude_code_fingerprint() {
    let provider = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    let req = ChatRequest::default();
    let names = outbound_header_names(&provider, &req);
    assert!(
        !names.iter().any(|n| n.starts_with("x-claude-code-")),
        "no Claude-Code headers on the mantle lane; got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("x-stainless-")),
        "no Stainless SDK headers on the mantle lane; got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "x-client-request-id"),
        "no Claude-Code request id on the mantle lane; got: {names:?}"
    );
}

/// The mantle lane is api-key (OauthBearer is rejected at config
/// validation), so the Claude-Code SDK User-Agent never fires: reqwest
/// keeps its default UA.
#[cfg(feature = "bedrock")]
#[test]
fn mantle_lane_resolves_no_claude_code_user_agent() {
    assert_eq!(resolve_user_agent(None, AuthKind::ApiKey), None);
}

/// `is_mantle` is true exactly when a mantle sub-config is present.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn is_mantle_tracks_mantle_presence() {
    let mantle = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    assert!(mantle.is_mantle());
    let plain = AnthropicApiProvider::new(AnthropicApiConfig::new("plain", "k"));
    assert!(!plain.is_mantle());
}

/// The signed request carries in-memory bytes (SigV4-hashable) and the
/// bearer path attaches `Authorization: Bearer <key>` post-build -- the
/// shape count_tokens relies on when it abandons `.json()` for `.body()`.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn sign_mantle_attaches_bearer_authorization_over_body_bytes() {
    let provider = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    let client = reqwest::Client::new();
    let mut request = client
        .post("https://bedrock-mantle.us-west-2.api.aws/anthropic/v1/messages/count_tokens")
        .body(serde_json::to_vec(&serde_json::json!({ "model": "m" })).unwrap())
        .build()
        .unwrap();
    provider.sign_mantle(&mut request).await.unwrap();
    assert_eq!(
        request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer mantle-key-xyz"),
    );
    assert!(
        request.body().and_then(|b| b.as_bytes()).is_some(),
        "mantle body must resolve to signable in-memory bytes"
    );
}

/// `auth_mode` reflects the resolved credential shape for the lane
/// observability fields.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn mantle_auth_mode_reflects_creds_shape() {
    let bearer = mantle_cfg_bearer().await;
    assert_eq!(bearer.mantle.as_ref().unwrap().auth_mode(), "bearer");

    let sigv4_creds = crate::bedrock::auth::resolve(
        &crate::bedrock::BedrockCreds::Static {
            access_key: "AKIAtest".into(),
            secret_key: "s".into(),
            session_token: None,
        },
        "us-west-2",
    )
    .await
    .unwrap();
    let mantle = MantleAuth {
        region: "us-west-2".into(),
        creds: sigv4_creds,
    };
    assert_eq!(mantle.auth_mode(), "sigv4");
}

/// MantleAuth Debug shows only the region and auth-mode discriminator;
/// no credential material ever renders.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn mantle_auth_debug_redacts_credentials() {
    let creds = crate::bedrock::auth::resolve(
        &crate::bedrock::BedrockCreds::Static {
            access_key: "AKIAsecret123".into(),
            secret_key: "supersecret".into(),
            session_token: Some("session-tok".into()),
        },
        "us-west-2",
    )
    .await
    .unwrap();
    let mantle = MantleAuth {
        region: "us-west-2".into(),
        creds,
    };
    let rendered = format!("{mantle:?}");
    assert!(rendered.contains("us-west-2"), "region shown: {rendered}");
    assert!(rendered.contains("sigv4"), "auth mode shown: {rendered}");
    assert!(
        !rendered.contains("supersecret"),
        "secret key must not leak: {rendered}"
    );
    assert!(
        !rendered.contains("AKIAsecret123"),
        "access key must not leak: {rendered}"
    );
    assert!(
        !rendered.contains("session-tok"),
        "session token must not leak: {rendered}"
    );
}

/// A minimal subscriber that captures span `record` field values, so the
/// mantle lane-context contract (`lane`/`auth_mode`/`region`) can be
/// asserted deterministically. The shared testkit capture subscriber
/// treats span `record` as a no-op, so a dedicated one is needed here.
/// `current_span` is implemented so `Span::current()` inside
/// `record_mantle_span_fields` resolves to the entered span rather than a
/// disabled one.
#[cfg(feature = "bedrock")]
struct SpanFieldCapture {
    fields: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    meta: std::sync::Mutex<Option<&'static tracing::Metadata<'static>>>,
    depth: std::sync::Mutex<usize>,
}

#[cfg(feature = "bedrock")]
impl SpanFieldCapture {
    fn new(fields: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) -> Self {
        Self {
            fields,
            meta: std::sync::Mutex::new(None),
            depth: std::sync::Mutex::new(0),
        }
    }
}

#[cfg(feature = "bedrock")]
impl tracing::Subscriber for SpanFieldCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        *self.meta.lock().unwrap() = Some(attrs.metadata());
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        struct Visitor(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);
        impl tracing::field::Visit for Visitor {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0
                    .lock()
                    .unwrap()
                    .push((field.name().to_string(), value.to_string()));
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .lock()
                    .unwrap()
                    .push((field.name().to_string(), format!("{value:?}")));
            }
        }
        values.record(&mut Visitor(std::sync::Arc::clone(&self.fields)));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {
        *self.depth.lock().unwrap() += 1;
    }
    fn exit(&self, _: &tracing::span::Id) {
        let mut depth = self.depth.lock().unwrap();
        *depth = depth.saturating_sub(1);
    }
    fn current_span(&self) -> tracing_core::span::Current {
        if *self.depth.lock().unwrap() > 0
            && let Some(meta) = *self.meta.lock().unwrap()
        {
            return tracing_core::span::Current::new(tracing::span::Id::from_u64(1), meta);
        }
        tracing_core::span::Current::none()
    }
}

/// On the mantle lane, `record_mantle_span_fields` stamps the request
/// span with `lane="bedrock-mantle"`, `auth_mode`, and `region` -- the
/// lane context the shared upstream-failure WARN inherits.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn record_mantle_span_fields_stamps_lane_auth_mode_region() {
    let provider = AnthropicApiProvider::new(mantle_cfg_bearer().await);
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = SpanFieldCapture::new(std::sync::Arc::clone(&captured));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "test",
            lane = tracing::field::Empty,
            auth_mode = tracing::field::Empty,
            region = tracing::field::Empty,
        );
        let _guard = span.enter();
        provider.record_mantle_span_fields();
    });
    let fields = captured.lock().unwrap().clone();
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "lane" && v == "bedrock-mantle"),
        "lane field must be recorded; got {fields:?}"
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "auth_mode" && v == "bearer"),
        "auth_mode field must be recorded; got {fields:?}"
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "region" && v == "us-west-2"),
        "region field must be recorded; got {fields:?}"
    );
}

/// The first-party lane records no lane context: `record_mantle_span_fields`
/// is a no-op when `mantle` is `None`.
#[cfg(feature = "bedrock")]
#[test]
fn record_mantle_span_fields_is_noop_without_mantle() {
    let provider = AnthropicApiProvider::new(AnthropicApiConfig::new("plain", "k"));
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = SpanFieldCapture::new(std::sync::Arc::clone(&captured));
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "test",
            lane = tracing::field::Empty,
            auth_mode = tracing::field::Empty,
            region = tracing::field::Empty,
        );
        let _guard = span.enter();
        provider.record_mantle_span_fields();
    });
    assert!(
        captured.lock().unwrap().is_empty(),
        "first-party lane must record no lane fields"
    );
}

// -- sampling strip (normalize_claude_sampling) ------------------------

/// Build a canonical request carrying caller sampling params plus a
/// `stop_sequences`-bound `stop`, so the assembled body exercises both the
/// stripped keys and the preserved neighbour.
fn req_with_sampling(temperature: Option<f64>, top_p: Option<f64>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(2048),
        temperature,
        top_p,
        stop: Some(vec!["HALT".into()]),
        ..Default::default()
    }
}

/// Reproduce the FINAL outbound body exactly as `complete` / `stream`
/// assemble it: `normalize_request` -> `cloak_body` -> the lane-gated
/// sampling strip. The two dispatch paths are proven to run this sequence
/// by `complete_and_stream_both_strip_sampling_on_the_cloak_lane`, which
/// drives the real methods; this helper is for the matrix cases that only
/// need the resulting body.
fn final_body(provider: &AnthropicApiProvider, req: &ChatRequest) -> Value {
    let mut body = provider.normalize_request(req).expect("normalize");
    provider.cloak_body(&mut body, req);
    if provider.is_cloak_lane(req) {
        extras::normalize_claude_sampling(&provider.cfg.id, &mut body);
    }
    body
}

/// Own-OAuth lane: the caller's `temperature` and `top_p` never reach the
/// wire, while `stop_sequences` (accepted by the seat) survives. Anthropic's
/// OAuth seat 400s a body carrying either sampling param.
#[test]
fn cloak_lane_strips_caller_sampling_and_keeps_stop_sequences() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    // temperature and top_p are mutually exclusive on the assembled body
    // (temperature wins), so drive each independently.
    for (temperature, top_p) in [(Some(0.7), None), (None, Some(0.9))] {
        let req = req_with_sampling(temperature, top_p);
        let body = final_body(&provider, &req);
        assert!(
            body.get("temperature").is_none(),
            "temperature must not reach the OAuth seat; got: {body}"
        );
        assert!(
            body.get("top_p").is_none(),
            "top_p must not reach the OAuth seat; got: {body}"
        );
        assert_eq!(
            body["stop_sequences"],
            serde_json::json!(["HALT"]),
            "stop_sequences is accepted by the seat and must survive; got: {body}"
        );
    }
}

/// routectl's OWN thinking clamp forces `temperature: 1.0` whenever
/// thinking is composed. The strip is the last word on the body, so that
/// self-inflicted value is removed too -- not just caller-supplied ones.
#[test]
fn cloak_lane_strips_thinking_clamp_temperature() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(4096),
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Precondition: without the strip the assembly emits temperature 1.0.
    let mut assembled = provider.normalize_request(&req).expect("normalize");
    provider.cloak_body(&mut assembled, &req);
    assert_eq!(
        assembled["temperature"],
        serde_json::json!(1.0),
        "precondition: the thinking clamp must force temperature 1.0; got: {assembled}"
    );

    let body = final_body(&provider, &req);
    assert!(
        body.get("temperature").is_none(),
        "the thinking-clamp temperature must be stripped too; got: {body}"
    );
}

/// `provider_extras` cannot smuggle sampling onto the wire at all:
/// `temperature` / `top_p` are canonical routectl-managed keys, so
/// `merge_provider_extras` drops them BEFORE the strip ever runs. Pin both
/// layers -- the extras shield upstream, and the stripped final body -- so a
/// future relaxation of `is_routectl_managed_key` cannot quietly re-open the
/// route without failing here. (`top_k` is NOT canonical and remains a
/// genuine pass-through hole; see docs/WIRE-GOTCHAS.md.)
#[test]
fn provider_extras_cannot_smuggle_sampling_onto_the_cloak_lane() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(2048),
        provider_extras: Some(serde_json::json!({"temperature": 0.3, "top_p": 0.4})),
        ..Default::default()
    };

    let mut pre_strip = provider.normalize_request(&req).expect("normalize");
    provider.cloak_body(&mut pre_strip, &req);
    assert!(
        pre_strip.get("temperature").is_none() && pre_strip.get("top_p").is_none(),
        "the managed-key shield must drop extras sampling before the strip; got: {pre_strip}"
    );

    let body = final_body(&provider, &req);
    assert!(
        body.get("temperature").is_none() && body.get("top_p").is_none(),
        "no sampling may reach the OAuth seat; got: {body}"
    );
}

/// Lane gate, NOT cloak state: the strip is keyed on `is_cloak_lane`, so it
/// fires for both `is_non_cc` classifications. Under `CloakMode::Never`
/// (is_non_cc false) the sampling params are still dropped -- gating on the
/// cloak flag instead would let `cloak.mode = never` re-introduce the
/// lane-wide 400.
#[test]
fn cloak_lane_strips_sampling_under_every_cloak_mode() {
    for mode in [CloakMode::Never, CloakMode::Always, CloakMode::Auto] {
        let provider = oauth_provider_with_cloak(CloakConfig {
            mode,
            ..CloakConfig::default()
        });
        let req = req_with_sampling(Some(0.7), None);
        assert!(
            provider.is_cloak_lane(&req),
            "precondition: every cloak mode stays on the lane ({mode:?})"
        );
        let body = final_body(&provider, &req);
        assert!(
            body.get("temperature").is_none(),
            "cloak mode {mode:?} must not change the lane-gated strip; got: {body}"
        );
    }
}

/// Off-lane matrix: forwarded-OAuth, OAuth to another host, and the API-key
/// lane all leave the body BYTE-UNCHANGED -- the caller's sampling reaches
/// upstream verbatim. The strip exists for routectl's own OAuth seat, not
/// for credentials or hosts routectl does not own.
///
/// Driven for BOTH sampling keys: a temperature-only matrix would assert
/// nothing about `top_p` (which the assembly omits when temperature wins),
/// so each off-lane case is re-run with a top_p-only request and the
/// surviving key is named explicitly.
#[test]
fn off_lane_requests_keep_sampling_byte_unchanged() {
    let forwarded_provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("sid".into()),
        Vec::new(),
        true,
    ));
    let other_host_provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://example.invalid",
        Some("sid".into()),
        Vec::new(),
        false,
    ));
    let api_key_provider = AnthropicApiProvider::new(cfg_with_allowlist(Vec::new()));

    // (temperature, top_p, the key the assembly emits, its value)
    for (temperature, top_p, kept_key, kept_value) in [
        (Some(0.7), None, "temperature", 0.7),
        (None, Some(0.9), "top_p", 0.9),
    ] {
        let mut forwarded = req_with_sampling(temperature, top_p);
        forwarded.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
            "fwd-bearer".to_string(),
        ));

        let cases: Vec<(&str, &AnthropicApiProvider, ChatRequest)> = vec![
            ("forwarded-OAuth", &forwarded_provider, forwarded),
            (
                "OAuth-to-another-host",
                &other_host_provider,
                req_with_sampling(temperature, top_p),
            ),
            (
                "API-key",
                &api_key_provider,
                req_with_sampling(temperature, top_p),
            ),
        ];

        for (label, provider, req) in cases {
            assert!(
                !provider.is_cloak_lane(&req),
                "precondition: {label} must be off the cloak lane"
            );
            // The un-stripped assembly is the byte baseline.
            let mut expected = provider.normalize_request(&req).expect("normalize");
            provider.cloak_body(&mut expected, &req);
            assert!(
                expected.get(kept_key).is_some(),
                "{label}: precondition -- the assembly must emit {kept_key}; got: {expected}"
            );

            let body = final_body(provider, &req);
            assert_eq!(
                body, expected,
                "{label} must be byte-unchanged by the strip ({kept_key} seed)"
            );
            assert_eq!(
                body[kept_key],
                serde_json::json!(kept_value),
                "{label} must forward the caller's {kept_key} verbatim"
            );
        }
    }
}

/// Both dispatch paths run the strip. Driving the REAL `complete` and
/// `stream` (not the assembly helper) is what pins that neither path can
/// omit the call: the lane is host-pinned to api.anthropic.com so no mock
/// server can serve it, but token resolution happens AFTER the strip, so a
/// failing token source halts each path with zero network I/O while the
/// post-mutation body has already been emitted by `trace_outgoing_body`.
///
/// Driven once per sampling key. A temperature-only seed makes the `top_p`
/// assertion vacuous (the assembly never emits `top_p` alongside
/// `temperature`), so each path is re-run with a top_p-only request and the
/// pre-strip assembly is asserted to actually carry the seeded key first.
/// A body carrying BOTH keys is unreachable through a `ChatRequest` --
/// `reconcile_sampling_params` emits `top_p` only when `temperature` is
/// absent, and `merge_provider_extras` shields both as managed keys -- so
/// the combined case is covered at the emitter level by
/// `sampling_strip_emits_one_warn_naming_both_dropped_keys`.
#[tokio::test]
async fn complete_and_stream_both_strip_sampling_on_the_cloak_lane() {
    for path in ["complete", "stream"] {
        for (temperature, top_p, seeded_key) in
            [(Some(0.7), None, "temperature"), (None, Some(0.9), "top_p")]
        {
            let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
                "https://api.anthropic.com",
                Arc::new(FailingTokenSource),
                false,
            ));
            let req = req_with_sampling(temperature, top_p);

            // Precondition: without the strip this seed really does put
            // `seeded_key` on the assembled body, so its absence below is
            // the strip's doing and not an assembly accident.
            let mut pre_strip = provider.normalize_request(&req).expect("normalize");
            provider.cloak_body(&mut pre_strip, &req);
            assert!(
                pre_strip.get(seeded_key).is_some(),
                "precondition: the {seeded_key} seed must reach the assembled body; got: {pre_strip}"
            );

            let (result, lines) = routectl_testkit::capture_lines(async {
                if path == "complete" {
                    provider.complete(req.clone()).await.map(|_| ())
                } else {
                    provider.stream(req.clone()).await.map(|_| ())
                }
            })
            .await;

            assert!(
                result.is_err(),
                "{path} must halt at token resolution, after the strip"
            );
            let outgoing: Vec<&String> = lines
                .iter()
                .filter(|l| l.contains("outgoing request body"))
                .collect();
            assert_eq!(
                outgoing.len(),
                1,
                "{path} must emit exactly one outgoing-body trace; got: {lines:?}"
            );
            let body = outgoing[0];
            assert!(
                !body.contains("temperature"),
                "{path} ({seeded_key} seed) must strip temperature before the outgoing trace; \
                 got: {body}"
            );
            assert!(
                !body.contains("top_p"),
                "{path} ({seeded_key} seed) must strip top_p before the outgoing trace; got: {body}"
            );
            assert!(
                body.contains("stop_sequences"),
                "{path} must preserve stop_sequences; got: {body}"
            );
        }
    }
}

// -- sampling strip: observability contract ----------------------------

/// Collect the sampling-strip WARNs emitted while assembling `req`.
fn strip_warns(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
) -> Vec<routectl_testkit::CapturedEvent> {
    warns_from(|| {
        let _ = final_body(provider, req);
    })
}

/// Collect the sampling-strip WARNs emitted by stripping `body` directly.
/// Needed for the both-keys case: the canonical assembly emits at most ONE
/// sampling key (temperature wins over top_p) and `merge_provider_extras`
/// shields both as managed keys, so a two-key body cannot be produced
/// through a `ChatRequest`. The emitter's combine-into-one contract is still
/// worth pinning against a future assembly that can land both.
fn strip_warns_for_body(body: &Value) -> Vec<routectl_testkit::CapturedEvent> {
    warns_from(|| {
        let mut body = body.clone();
        extras::normalize_claude_sampling("test", &mut body);
    })
}

fn warns_from<F: FnOnce()>(f: F) -> Vec<routectl_testkit::CapturedEvent> {
    routectl_testkit::capture_events(f)
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("dropped_params").is_some())
        .collect()
}

/// Two removed keys produce exactly ONE event carrying both names, with the
/// fixed lane label and the provider id. The values themselves are never
/// logged -- only the key names.
#[test]
fn sampling_strip_emits_one_warn_naming_both_dropped_keys() {
    let warns = strip_warns_for_body(&serde_json::json!({
        "model": "claude-sonnet-4-5",
        "temperature": 0.3,
        "top_p": 0.4,
    }));

    assert_eq!(warns.len(), 1, "two keys must combine into ONE event");
    let warn = &warns[0];
    assert_eq!(warn.field("provider"), Some("test"));
    assert_eq!(warn.field("lane"), Some("oauth-own-anthropic"));
    assert_eq!(
        warn.field("dropped_params"),
        Some("temperature,top_p"),
        "both key names ride one bounded field"
    );
}

/// Only the keys actually present are named: a body carrying just
/// `temperature` must not claim `top_p` was dropped.
#[test]
fn sampling_strip_warn_names_only_present_keys() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let warns = strip_warns(&provider, &req_with_sampling(Some(0.7), None));
    assert_eq!(warns.len(), 1);
    assert_eq!(warns[0].field("dropped_params"), Some("temperature"));
}

/// No affected key -> no WARN at all. The strip must stay silent on the
/// overwhelmingly common request shape rather than logging a no-op.
#[test]
fn sampling_strip_emits_no_warn_when_nothing_is_dropped() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(2048),
        ..Default::default()
    };
    assert!(
        strip_warns(&provider, &req).is_empty(),
        "a no-op strip must emit no WARN"
    );
}

/// Log hygiene: the REMOVED VALUES never appear in any field or in the
/// message. Hostile floats are used as the canary -- if the emitter ever
/// interpolated the stripped value (or the body) the sentinel would show up.
#[test]
fn sampling_strip_warn_never_carries_the_removed_values() {
    let warns = strip_warns_for_body(&serde_json::json!({
        "model": "claude-sonnet-4-5",
        "temperature": 0.123_456_789,
        "top_p": 0.987_654_321,
    }));

    assert_eq!(warns.len(), 1);
    let warn = &warns[0];
    for sentinel in ["0.123456789", "0.987654321"] {
        assert!(
            !warn.message.contains(sentinel),
            "the removed value {sentinel} must not appear in the message: {}",
            warn.message
        );
        for (name, value) in &warn.fields {
            assert!(
                !value.contains(sentinel),
                "the removed value {sentinel} must not appear in field {name}: {value}"
            );
        }
    }
}

/// The one-WARN contract on the REAL dispatch paths, not just the emitter.
/// A helper-level assertion cannot see a second call site, so drive
/// `complete` and `stream` themselves (host-pinned lane -> `FailingTokenSource`
/// halts each path after the strip, before any network I/O) and assert
/// exactly ONE strip WARN per path, with the exact static fields and only
/// the seeded key name. A hostile sampling value is used as the canary so a
/// regression that interpolated the removed value -- or the body -- into any
/// field or the message would surface here on the production path.
#[tokio::test]
async fn real_complete_and_stream_each_emit_exactly_one_strip_warn() {
    const HOSTILE_TEMPERATURE: f64 = 0.123_456_789;
    const HOSTILE_TOP_P: f64 = 0.987_654_321;

    for path in ["complete", "stream"] {
        for (temperature, top_p, expected_names, sentinel) in [
            (
                Some(HOSTILE_TEMPERATURE),
                None,
                "temperature",
                "0.123456789",
            ),
            (None, Some(HOSTILE_TOP_P), "top_p", "0.987654321"),
        ] {
            let provider = AnthropicApiProvider::new(oauth_cfg_with_auth(
                "https://api.anthropic.com",
                Arc::new(FailingTokenSource),
                false,
            ));
            let req = req_with_sampling(temperature, top_p);

            let (result, events) = routectl_testkit::with_capture(async {
                if path == "complete" {
                    provider.complete(req.clone()).await.map(|_| ())
                } else {
                    provider.stream(req.clone()).await.map(|_| ())
                }
            })
            .await;
            assert!(
                result.is_err(),
                "{path} must halt at token resolution, after the strip"
            );

            let warns: Vec<_> = events
                .iter()
                .filter(|e| e.level == tracing::Level::WARN && e.field("dropped_params").is_some())
                .collect();
            assert_eq!(
                warns.len(),
                1,
                "{path} ({expected_names}) must emit exactly ONE strip WARN; got: {warns:?}"
            );
            let warn = warns[0];
            assert_eq!(warn.field("provider"), Some("test"));
            assert_eq!(warn.field("lane"), Some("oauth-own-anthropic"));
            assert_eq!(
                warn.field("dropped_params"),
                Some(expected_names),
                "{path} must name only the key actually dropped"
            );
            assert!(
                !warn.message.contains(sentinel),
                "{path}: the removed value must not appear in the message: {}",
                warn.message
            );
            for (name, value) in &warn.fields {
                assert!(
                    !value.contains(sentinel),
                    "{path}: the removed value must not appear in field {name}: {value}"
                );
            }
        }
    }
}

/// The count_tokens path deliberately does NOT call the strip:
/// `build_count_tokens_body` already drops sampling by allowlist, so adding
/// the call there would only multiply WARNs on a path Claude Code polls
/// heavily. Pin both halves -- sampling absent from the count_tokens body,
/// and no strip WARN attributable to building it.
#[test]
fn count_tokens_body_drops_sampling_by_allowlist_without_a_strip_warn() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = req_with_sampling(Some(0.7), None);

    let events = routectl_testkit::capture_events(|| {
        let mut normalized = provider.normalize_request(&req).expect("normalize");
        provider.cloak_body(&mut normalized, &req);
        let body = build_count_tokens_body(&normalized);
        assert!(
            body.get("temperature").is_none() && body.get("top_p").is_none(),
            "the count_tokens allowlist must already exclude sampling; got: {body}"
        );
    });
    assert!(
        !events.iter().any(|e| e.field("dropped_params").is_some()),
        "the count_tokens path must not emit a sampling-strip WARN"
    );
}

// -- effort capability beta union --------------------------------------

/// Count how many times `flag` appears in a joined `anthropic-beta` value.
fn beta_occurrences(header: &str, flag: &str) -> usize {
    header.split(',').filter(|b| b.trim() == flag).count()
}

/// An adaptive-thinking request composes `output_config.effort`, so the
/// union adds `EFFORT_BETA` exactly once. Asserted against the flag's own
/// occurrence count rather than the floor length: an effort body
/// legitimately widens the list past `floor.len()`.
#[test]
fn adaptive_effort_body_gains_the_effort_beta_exactly_once() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        max_tokens: Some(4096),
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.supports_adaptive_thinking = true;

    let body = provider.normalize_request(&req).expect("normalize");
    assert!(
        body["output_config"].get("effort").is_some(),
        "precondition: adaptive thinking must compose output_config.effort; got: {body}"
    );

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the OAuth lane must produce a beta header");
    assert_eq!(
        beta_occurrences(&value, routectl_core::identity::anthropic::EFFORT_BETA),
        1,
        "the effort beta must appear exactly once; got: {value}"
    );
    // The floor is preserved in order, with the effort flag appended.
    let betas: Vec<&str> = value.split(',').map(str::trim).collect();
    for flag in routectl_core::identity::anthropic::default_claude_code_anthropic_betas() {
        assert!(
            betas.contains(flag),
            "the union must not drop floor flag {flag}; got: {value}"
        );
    }
    assert_eq!(
        betas.last(),
        Some(&routectl_core::identity::anthropic::EFFORT_BETA),
        "the capability union appends after the floor; got: {value}"
    );
}

/// The union reads the ASSEMBLED body, so an `output_config.effort` that
/// arrives through the `provider_extras` forward-compat sweep triggers it
/// too -- exactly once, with no reorder of the floor.
///
/// Drives the WHOLE path rather than a fabricated body: the effort lives in
/// `ChatRequest.provider_extras`, so `merge_provider_extras` must copy it
/// onto the assembled body and the late `reconcile_output_config_effort`
/// must let a caller-supplied value stand before header composition ever
/// sees it. A fabricated body would prove only the union, leaving the merge
/// and the late reconciliation untested.
#[test]
fn provider_extras_effort_body_gains_the_effort_beta_exactly_once() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        max_tokens: Some(4096),
        // Adaptive thinking is what keeps `output_config.effort` on the
        // body through the late reconciliation; the extras value below is
        // deliberately DIFFERENT from this one so a merge that silently
        // dropped the extras would fail the value assertion.
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some("low".into()),
            ..Default::default()
        }),
        provider_extras: Some(serde_json::json!({
            "output_config": {"effort": "high"},
        })),
        ..Default::default()
    };
    req.routectl_internal.supports_adaptive_thinking = true;

    let body = provider.normalize_request(&req).expect("normalize");
    assert_eq!(
        body["output_config"].get("effort").and_then(Value::as_str),
        Some("high"),
        "the extras-supplied effort must survive the merge and the late \
         reconciliation onto the shipped body; got: {body}"
    );

    let baseline = outbound_header_value(&provider, &req, "anthropic-beta")
        .expect("the OAuth floor must produce a beta header");
    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the OAuth floor must produce a beta header");

    assert_eq!(
        beta_occurrences(&value, routectl_core::identity::anthropic::EFFORT_BETA),
        1,
        "the effort beta must appear exactly once; got: {value}"
    );
    assert_eq!(
        value,
        format!(
            "{baseline},{}",
            routectl_core::identity::anthropic::EFFORT_BETA
        ),
        "the union appends without reordering the existing set"
    );
}

/// A body carrying BOTH gated fields gains BOTH betas. `output_config.format`
/// is already in the floor, so the observable delta is the effort flag; the
/// assertion pins that each gated field is independently covered.
#[test]
fn body_with_format_and_effort_gains_both_capability_betas() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let body = serde_json::json!({
        "output_config": {
            "format": {"type": "json_object"},
            "effort": "high",
        },
    });
    let req = req_with_claude_code_headers(Vec::new());

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the OAuth floor must produce a beta header");
    for flag in [
        routectl_core::identity::anthropic::STRUCTURED_OUTPUTS_BETA,
        routectl_core::identity::anthropic::EFFORT_BETA,
    ] {
        assert_eq!(
            beta_occurrences(&value, flag),
            1,
            "{flag} must be present exactly once; got: {value}"
        );
    }
}

/// Effort removed by the late thinking reconciliation gains NO beta. A
/// `tool_choice` that forces tool use strips `thinking`, and
/// `reconcile_output_config_effort` then drops the orphan effort -- so the
/// shipped body does not carry the gated field and must not claim the flag.
#[test]
fn effort_dropped_by_late_reconciliation_gains_no_effort_beta() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        max_tokens: Some(4096),
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        }),
        tool_choice: Some(serde_json::json!({"type": "any"})),
        ..Default::default()
    };
    req.routectl_internal.supports_adaptive_thinking = true;

    let body = provider.normalize_request(&req).expect("normalize");
    assert!(
        body.get("output_config")
            .and_then(|oc| oc.get("effort"))
            .is_none(),
        "precondition: forced tool_choice must strip thinking and orphan effort; got: {body}"
    );

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the OAuth floor must produce a beta header");
    assert_eq!(
        beta_occurrences(&value, routectl_core::identity::anthropic::EFFORT_BETA),
        0,
        "no effort field on the wire means no effort beta; got: {value}"
    );
}

/// One-way invariant: a caller who explicitly requests the effort beta with
/// NO `output_config.effort` on the body keeps the flag. The union adds the
/// beta for the field; it never removes a beta for a missing field.
#[test]
fn explicit_caller_effort_beta_survives_without_the_field() {
    let provider = AnthropicApiProvider::new(oauth_cfg(Vec::new(), None));
    let req = ChatRequest {
        anthropic_beta: vec![routectl_core::identity::anthropic::EFFORT_BETA.into()],
        ..Default::default()
    };
    let body = serde_json::json!({"model": "claude-opus-4-7"});

    let value = outbound_header_value_for_body(&provider, &req, "anthropic-beta", Some(&body))
        .expect("the OAuth floor must produce a beta header");
    assert_eq!(
        beta_occurrences(&value, routectl_core::identity::anthropic::EFFORT_BETA),
        1,
        "a caller-supplied effort beta must survive untouched; got: {value}"
    );
}

/// The effort union is suppressed OFF the cloak lane: a forwarded-OAuth leg
/// and an API-key provider both emit byte-identical beta headers with and
/// without an effort-carrying body. The forwarded leg must reach Anthropic
/// with the client's own set verbatim, and the API-key lane never composes
/// routectl's adaptive-effort directive.
#[test]
fn off_lane_beta_headers_are_byte_unchanged_by_an_effort_body() {
    let effort_body = serde_json::json!({"output_config": {"effort": "high"}});

    // Forwarded-OAuth leg: the client supplies its own beta set.
    let forwarded_provider = AnthropicApiProvider::new(oauth_cfg_with_session(
        "https://api.anthropic.com",
        Some("sid".into()),
        Vec::new(),
        true,
    ));
    let mut forwarded_req = ChatRequest {
        anthropic_beta: vec!["client-sent-beta".into()],
        ..Default::default()
    };
    forwarded_req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
        "fwd-bearer".to_string(),
    ));

    // API-key lane on the same host.
    let api_key_provider = AnthropicApiProvider::new(api_key_cfg_for_betas(Vec::new()));
    let api_key_req = ChatRequest {
        anthropic_beta: vec!["client-sent-beta".into()],
        ..Default::default()
    };

    for (label, provider, req) in [
        ("forwarded-OAuth", &forwarded_provider, &forwarded_req),
        ("API-key", &api_key_provider, &api_key_req),
    ] {
        assert!(
            !provider.is_cloak_lane(req),
            "precondition: {label} must be off the cloak lane"
        );
        let without_body = outbound_header_value(provider, req, "anthropic-beta");
        let with_body =
            outbound_header_value_for_body(provider, req, "anthropic-beta", Some(&effort_body));
        assert_eq!(
            with_body, without_body,
            "{label} beta header must be byte-unchanged by an effort body"
        );
        assert_eq!(
            with_body
                .as_deref()
                .map(|v| beta_occurrences(v, routectl_core::identity::anthropic::EFFORT_BETA)),
            Some(0),
            "{label} must not gain the effort beta"
        );
    }
}
