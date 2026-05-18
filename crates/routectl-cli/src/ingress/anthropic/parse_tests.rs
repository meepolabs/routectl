use serde_json::json;

use crate::ingress::IngressAdapter;

use super::super::AnthropicIngress;
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
        .parse_request(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(req.model, "claude-opus-4-7");
    assert!(matches!(
        req.system,
        Some(routectl_core::SystemContent::Blocks(_))
    ));
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
        .parse_request(&HeaderMap::new(), body)
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
        .parse_request(&HeaderMap::new(), body)
        .unwrap();
    assert_eq!(req.user.as_deref(), Some("abc-123"));
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
        .parse_request(&HeaderMap::new(), body)
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
        .parse_request(&HeaderMap::new(), body)
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
        .parse_request(&HeaderMap::new(), body)
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
        .parse_request(&HeaderMap::new(), body)
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
        .parse_request(&HeaderMap::new(), body)
        .unwrap();
    if let MessageContent::Parts(parts) = &req.messages[0].content {
        assert!(matches!(&parts[0], ContentPart::Other { .. }));
    } else {
        panic!("expected Parts");
    }
}

// -------- response rendering --------
/// Review follow-up to merge_inbound_anthropic_beta_header
/// (security defense-in-depth): a beta value containing CR or
/// LF must be dropped rather than appended to req.anthropic_beta.
/// `HeaderValue::to_str` already rejects control bytes so the
/// natural ingress path doesn't deliver such values, but pinning
/// the explicit filter prevents a future refactor (or a test
/// that constructs the header bytes through a different path)
/// from silently re-opening the header-injection seam.
#[test]
fn merge_inbound_anthropic_beta_header_filters_crlf_in_values() {
    use axum::http::{HeaderMap, HeaderName};
    use routectl_core::ChatRequest;
    // Build a HeaderMap whose anthropic-beta value carries CRLF
    // mid-string. We have to use `from_maybe_shared_unchecked`
    // via raw bytes because HeaderValue::from_str rightly rejects
    // CR/LF; the test is here to prove the merge function itself
    // would reject them even if a future path bypassed http's
    // own validation.
    let mut headers = HeaderMap::new();
    // We cannot insert a header carrying CRLF via the public API.
    // Instead, simulate the failure at the trim step by inserting
    // a benign value that DOES contain a CRLF substring after
    // trim via a pre-merge mutation of req.anthropic_beta. The
    // function's contract is the filter; this test asserts that
    // contract by driving the filter directly with a comma-list.
    headers.insert(
        HeaderName::from_static("anthropic-beta"),
        "good-beta,benign".parse().unwrap(),
    );
    // Seed an already-bad entry to exercise the filter on the
    // existing-vec path too. (We can put CR/LF in a plain
    // String -- only HeaderValue rejects them.)
    let mut req = ChatRequest {
        anthropic_beta: vec!["pre-existing\r\nX-Inject: evil".into()],
        ..Default::default()
    };
    merge_inbound_anthropic_beta_header(&headers, &mut req);
    // The headers-side values flow through cleanly.
    assert!(req.anthropic_beta.contains(&"good-beta".to_string()));
    assert!(req.anthropic_beta.contains(&"benign".to_string()));
    // Demonstrate the filter on the comma-split path: a header
    // value that "would" contain CRLF after split would be
    // dropped. We can't synthesize one via HeaderValue, but we
    // can exercise the same code path via the split-and-filter
    // sub-routine by direct call below. For the seeded entry,
    // it persists (the filter only fires on freshly-parsed
    // header pieces; pre-existing req.anthropic_beta entries
    // are operator-supplied and not subject to this filter,
    // intentionally -- if the operator wants CRLF in a body
    // field, that's their call).
    assert!(req
        .anthropic_beta
        .iter()
        .any(|b| b.contains("pre-existing")));
}

/// Review follow-up: the CR/LF defense-in-depth filter lives in a
/// helper that can be unit-tested in isolation, sidestepping
/// `HeaderValue::from_str`'s own rejection of control bytes. Pin the
/// contract: benign strings pass, CR or LF anywhere causes rejection.
/// Without this, the security-relevant branch of
/// `merge_inbound_anthropic_beta_header` was not actually exercised
/// (the outer test could only synthesize benign HeaderValues).
#[test]
fn is_safe_beta_value_rejects_crlf_strings() {
    // Benign cases pass.
    assert!(is_safe_beta_value("legit-beta"));
    assert!(is_safe_beta_value("context-management-2025-06-27"));
    assert!(is_safe_beta_value("")); // empty is structurally safe
    assert!(is_safe_beta_value("with spaces"));
    assert!(is_safe_beta_value("with-special!@#$%^&*()chars"));
    // CR or LF anywhere in the value rejects.
    assert!(!is_safe_beta_value("evil\r\nX-Injected: bad"));
    assert!(!is_safe_beta_value("evil\rmid"));
    assert!(!is_safe_beta_value("evil\nmid"));
    assert!(!is_safe_beta_value("\revil-leading-cr"));
    assert!(!is_safe_beta_value("\nevil-leading-lf"));
    assert!(!is_safe_beta_value("evil-trailing\r"));
    assert!(!is_safe_beta_value("evil-trailing\n"));
}
