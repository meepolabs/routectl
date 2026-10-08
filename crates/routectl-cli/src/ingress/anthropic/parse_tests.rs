use serde_json::json;

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

#[test]
fn parse_request_with_system_blocks_and_cache_control() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "system": [{
            "type": "text",
            "text": "you are helpful",
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        }],
        "max_tokens": 1024
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(req.model, "claude-opus-4-7");
    assert!(matches!(
        req.system,
        Some(routectl_core::SystemContent::Blocks(_))
    ));
}

#[test]
fn parse_request_stamps_anthropic_provenance() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(
        req.routectl_internal.provenance,
        routectl_core::RequestProvenance::AnthropicIngress,
    );
}

#[test]
fn parse_request_translates_thinking_to_reasoning() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "thinking": {"type": "enabled", "budget_tokens": 5000}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    let r = req.reasoning.unwrap();
    assert_eq!(r.enabled, Some(true));
    assert_eq!(r.max_tokens, Some(5000));
}

#[test]
fn parse_request_translates_metadata_user_id_to_user() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"user_id": "abc-123"}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(req.user.as_deref(), Some("abc-123"));
}

fn headers_with_session(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-claude-code-session-id",
        axum::http::HeaderValue::from_str(value).unwrap(),
    );
    headers
}

#[test]
fn parse_request_captures_inbound_session_key_from_header() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let req = AnthropicIngress
        .parse_request_value(&headers_with_session("sid-from-header"), body)
        .unwrap();
    assert_eq!(
        req.routectl_internal.inbound_session_key.as_deref(),
        Some("sid-from-header"),
    );
}

#[test]
fn parse_request_falls_back_to_metadata_session_id() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata"}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(
        req.routectl_internal.inbound_session_key.as_deref(),
        Some("sid-from-metadata"),
    );
}

#[test]
fn parse_request_header_session_key_wins_over_metadata() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata"}
    });
    let req = AnthropicIngress
        .parse_request_value(&headers_with_session("sid-from-header"), body)
        .unwrap();
    assert_eq!(
        req.routectl_internal.inbound_session_key.as_deref(),
        Some("sid-from-header"),
    );
}

#[test]
fn parse_request_keyless_yields_none_session_key() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(req.routectl_internal.inbound_session_key, None);
}

#[test]
fn parse_request_empty_header_session_key_falls_through_to_metadata() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata"}
    });
    let req = AnthropicIngress
        .parse_request_value(&headers_with_session("   "), body)
        .unwrap();
    assert_eq!(
        req.routectl_internal.inbound_session_key.as_deref(),
        Some("sid-from-metadata"),
    );
}

#[test]
fn parse_request_header_metadata_conflict_emits_mismatch_warning_without_raw_ids() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata"}
    });

    let events = routectl_testkit::capture_events(|| {
        let req = AnthropicIngress
            .parse_request_value(&headers_with_session("sid-from-header"), body.clone())
            .unwrap();
        // Header still wins for the resolved key; the guardrail only logs
        // the conflict, it never changes the resolution outcome.
        assert_eq!(
            req.routectl_internal.inbound_session_key.as_deref(),
            Some("sid-from-header"),
        );
    });

    let conflict_event = events
        .iter()
        .find(|e| e.field("session_key_source_conflict").is_some())
        .unwrap_or_else(|| panic!("expected mismatch WARN, got events: {events:?}"));
    assert_eq!(conflict_event.level, tracing::Level::WARN);
    assert_eq!(
        conflict_event.field("session_key_source_conflict"),
        Some("true"),
    );
    for event in &events {
        assert!(
            !event.message.contains("sid-from-header")
                && !event.message.contains("sid-from-metadata"),
            "raw session id must never be logged: {event:?}",
        );
        for (_, v) in &event.fields {
            assert!(
                v != "sid-from-header" && v != "sid-from-metadata",
                "raw session id must never appear in a structured field: {event:?}",
            );
        }
    }
}

#[test]
fn parse_request_header_metadata_agreement_emits_no_mismatch_warning() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-same"}
    });

    let events = routectl_testkit::capture_events(|| {
        let _ = AnthropicIngress
            .parse_request_value(&headers_with_session("sid-same"), body)
            .unwrap();
    });

    assert!(
        !events
            .iter()
            .any(|e| e.field("session_key_source_conflict").is_some()),
        "agreeing header and metadata must not fire the mismatch guardrail: {events:?}",
    );
}

#[test]
fn parse_request_preserves_metadata_session_id_in_provider_extras() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "metadata": {"session_id": "sid-from-metadata", "user_id": "abc-123"}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    // Capturing the session key must be a non-destructive read: the full
    // `metadata` object (including `session_id`) still round-trips into
    // provider_extras for Anthropic-shape egresses.
    let extras = req.provider_extras.unwrap();
    assert_eq!(extras["metadata"]["session_id"], "sid-from-metadata");
    assert_eq!(extras["metadata"]["user_id"], "abc-123");
}

#[test]
fn parse_request_anthropic_only_fields_land_in_provider_extras() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "top_k": 40,
        "service_tier": "auto",
        "container": "ctr_01"
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    let extras = req.provider_extras.unwrap();
    assert_eq!(extras["top_k"], 40);
    assert_eq!(extras["service_tier"], "auto");
    assert_eq!(extras["container"], "ctr_01");
}

#[test]
fn parse_request_anthropic_beta_round_trips() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "anthropic_beta": ["context-1m-2025-08-07"]
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(
        req.anthropic_beta,
        vec!["context-1m-2025-08-07".to_string()]
    );
}

#[test]
fn parse_request_rejects_too_many_breakpoints() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "c", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "d", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "e", "cache_control": {"type": "ephemeral"}}
            ]
        }],
        "max_tokens": 1024
    });
    let err = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap_err();
    assert!(matches!(err, Error::Validation(_)));
    assert!(err.to_string().contains("exceeds maximum"));
}

#[test]
fn parse_request_rejects_5m_then_1h_ordering() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "five", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
                {"type": "text", "text": "one", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]
        }],
        "max_tokens": 1024
    });
    let err = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap_err();
    assert!(matches!(err, Error::Validation(_)));
    assert!(err.to_string().contains("after a 5m"));
}

#[test]
fn parse_request_unknown_block_type_passes_through() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "server_tool_use",
                "id": "srvtu_01",
                "name": "web_search",
                "input": {"query": "rust"}
            }]
        }],
        "max_tokens": 1024
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    if let MessageContent::Parts(parts) = &req.messages[0].content {
        assert!(matches!(&parts[0], ContentPart::Other { .. }));
    } else {
        panic!("expected Parts");
    }
}

// -------- response rendering --------
/// Body and header beta entries are normalized together, body first: each
/// entry is comma-split and trimmed, empties and CR/LF-bearing pieces are
/// dropped, and a repeat across the two sources ships once.
#[test]
fn body_and_header_betas_are_normalized_together() {
    use axum::http::{HeaderMap, HeaderName};
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("anthropic-beta"),
        "good-beta, body-b ,".parse().unwrap(),
    );
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 16,
        "anthropic_beta": [" body-a", "body-b,body-c ", "", "evil\r\nX-Inject: bad"]
    });

    let req = AnthropicIngress
        .parse_request_value(&headers, body)
        .unwrap();

    assert_eq!(
        req.anthropic_beta,
        vec!["body-a", "body-b", "body-c", "good-beta"],
    );
}

/// The distinct-flag cap counts body and header flags together and rejects
/// one over it as a validation error, which the handler answers with 400.
#[test]
fn beta_flag_cap_counts_body_and_header_flags_together() {
    use crate::ingress::MAX_CLIENT_BETA_FLAGS;
    use axum::http::{HeaderMap, HeaderName};
    let body_flags: Vec<String> = (0..MAX_CLIENT_BETA_FLAGS)
        .map(|i| format!("f-{i}"))
        .collect();
    let body = |flags: &[String]| {
        json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16,
            "anthropic_beta": flags
        })
    };
    let mut over = HeaderMap::new();
    over.insert(
        HeaderName::from_static("anthropic-beta"),
        "one-more".parse().unwrap(),
    );

    let at_cap = AnthropicIngress.parse_request_value(&HeaderMap::new(), body(&body_flags));
    let over_cap = AnthropicIngress.parse_request_value(&over, body(&body_flags));

    assert_eq!(at_cap.unwrap().anthropic_beta.len(), MAX_CLIENT_BETA_FLAGS);
    assert!(
        matches!(over_cap, Err(Error::Validation(_))),
        "{over_cap:?}"
    );
}

/// Gateway-correctness contract: every inbound header whose name
/// starts with `x-claude-code-` is captured into
/// `routectl_internal.claude_code_headers` so the Anthropic-API egress
/// can forward them upstream. Other headers (auth, the routectl alias
/// header, anything not matching the prefix) MUST NOT be captured.
/// axum/http normalizes inbound HeaderMap keys to lowercase on
/// receive, so this test always sees lowercase names regardless of
/// what the client wrote on the wire.
#[test]
fn captures_x_claude_code_headers_inside_namespace() {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_static("11111111-aaaa-bbbb-cccc-222222222222"),
    );
    headers.insert(
        HeaderName::from_static("x-claude-code-agent-id"),
        HeaderValue::from_static("33333333-aaaa-bbbb-cccc-444444444444"),
    );
    headers.insert(
        HeaderName::from_static("x-routectl-alias"),
        HeaderValue::from_static("default"),
    );
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static("Bearer dummy"),
    );

    let body = serde_json::json!({"model": "claude-test", "messages": []});
    let req = translate_request(&headers, body).unwrap();

    let h = &req.routectl_internal.claude_code_headers;
    assert_eq!(h.len(), 2, "captured: {h:?}");
    // Casing contract: axum/http normalizes inbound HeaderMap keys to
    // lowercase, so the captured tuples store lowercase names. Pin
    // this contract explicitly so a future refactor (or a different
    // http-stack version) that changes the casing surfaces in CI.
    assert!(h.iter().any(|(n, _)| n == "x-claude-code-session-id"));
    assert!(h.iter().any(|(n, _)| n == "x-claude-code-agent-id"));
    // No mixed-case names should appear: every captured name is the
    // lowercase form regardless of the client's wire casing.
    for (name, _) in h {
        assert_eq!(
            name.as_str(),
            name.to_ascii_lowercase().as_str(),
            "captured name `{name}` must be lowercase",
        );
    }
    // x-routectl-alias and authorization must NOT be captured.
    assert!(!h.iter().any(|(n, _)| n == "x-routectl-alias"));
    assert!(!h.iter().any(|(n, _)| n == "authorization"));
}

/// claude-code 2.1.153 sends thinking={type:"adaptive"} and
/// output_config={effort:"low"} as separate fields. Verify that effort
/// is lifted into canonical req.reasoning.effort, that enabled stays
/// Some(true) from the thinking field, and that output_config is
/// preserved intact in provider_extras for Anthropic-API egress passthrough.
#[test]
fn parse_request_adaptive_thinking_lifts_effort() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "low"}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    let r = req.reasoning.as_ref().unwrap();
    assert_eq!(
        r.enabled,
        Some(true),
        "enabled must be Some(true) from adaptive thinking"
    );
    assert_eq!(
        r.effort.as_deref(),
        Some("low"),
        "effort must be lifted from output_config"
    );
    // output_config must remain intact in provider_extras for egress passthrough.
    let extras = req.provider_extras.as_ref().unwrap();
    assert_eq!(extras["output_config"]["effort"], "low");
}

/// When output_config.effort is present but no thinking field is sent,
/// the lift must still write req.reasoning.effort. enabled must be None
/// because no thinking field set it -- the caller only requested an effort
/// level, not a reasoning mode.
#[test]
fn parse_request_output_config_effort_no_thinking() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "output_config": {"effort": "high"}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    let r = req.reasoning.as_ref().unwrap();
    assert_eq!(
        r.effort.as_deref(),
        Some("high"),
        "effort must be lifted from output_config"
    );
    assert_eq!(
        r.enabled, None,
        "enabled must be None -- thinking field was absent"
    );
    // output_config must remain intact in provider_extras.
    let extras = req.provider_extras.as_ref().unwrap();
    assert_eq!(extras["output_config"]["effort"], "high");
}

/// When output_config contains only a format key (structured outputs schema)
/// and no effort key, req.reasoning must be untouched and output_config must
/// survive intact in provider_extras.
#[test]
fn parse_request_output_config_without_effort() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "output_config": {
            "format": {"type": "json_schema", "json_schema": {"name": "reply"}}
        }
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    assert!(
        req.reasoning.is_none(),
        "reasoning must be None -- no thinking and no effort"
    );
    // output_config must remain intact in provider_extras.
    let extras = req.provider_extras.as_ref().unwrap();
    assert_eq!(extras["output_config"]["format"]["type"], "json_schema");
}

/// Bug fix: `output_format: null` (JSON null, not field-absent) must be
/// treated as not-set and NOT promoted into output_config.format. Promoting
/// null causes a 400 from api.anthropic.com on requests that include
/// `output_format: null` as a default SDK serialization artifact.
#[test]
fn parse_request_null_output_format_is_dropped() {
    // Case 1: output_format: null with no output_config at all.
    // output_config must not appear in provider_extras.
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "output_format": null
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();
    if let Some(extras) = req.provider_extras.as_ref() {
        assert!(
            extras.get("output_config").is_none(),
            "output_config must not be created when output_format is null, \
             got extras: {extras}"
        );
    }

    // Case 2: output_format: null with a pre-existing output_config.
    // The pre-existing output_config must survive unchanged.
    let body2 = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "output_format": null,
        "output_config": {"effort": "high"}
    });
    let req2 = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body2)
        .unwrap();
    let extras2 = req2.provider_extras.as_ref().unwrap();
    // output_config is preserved; no format key was injected from the null.
    assert_eq!(extras2["output_config"]["effort"], "high");
    assert!(
        extras2["output_config"].get("format").is_none(),
        "null output_format must not inject a format key into output_config"
    );
}

#[test]
fn camelcase_input_schema_tool_keeps_its_schema() {
    // Arrange: an MCP / Agent-SDK-shaped tool spells the schema key in
    // camelCase; a snake_case sibling is the positive control.
    let schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"]
    });
    let body = json!({
        "model": "claude-opus-4-7",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"name": "snake", "input_schema": schema.clone()},
            {"name": "camel", "inputSchema": schema.clone()}
        ]
    });

    // Act
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .unwrap();

    // Assert
    let tools = req.tools.expect("tools present");
    let schemas: Vec<&serde_json::Value> = tools
        .iter()
        .map(|t| match t {
            routectl_core::ToolDef::Custom(c) => &c.input_schema,
            other => panic!("expected Custom, got {other:?}"),
        })
        .collect();
    assert_eq!(schemas, vec![&schema, &schema]);
}
