//! InvokeModel adapter -- vendor-specific request body, with Anthropic
//! Messages JSON for Claude models.
//!
//! For Anthropic on Bedrock, the body shape is identical to the
//! Anthropic Messages API except:
//!   - `anthropic_version` is `"bedrock-2023-05-31"` (Bedrock-specific
//!     version string) and lives in the BODY, not in a header.
//!   - No auth headers (replaced by SigV4 signing on the HTTP layer).
//!   - Beta flags travel in the body's `anthropic_beta` array.
//!
//! We delegate body construction to `anthropic_api::request::normalize`
//! so the reasoning / cache_control / tool-use logic stays in one place,
//! then patch the result for Bedrock-specific fields.
//!
//! Response parsing is a direct call to `anthropic_api::response::normalize`
//! -- the Bedrock InvokeModel response shape for Anthropic models is
//! identical to the Anthropic Messages API response shape (which is
//! the whole point of the Invoke shape: pure passthrough).

use serde_json::Value;

use routectl_core::{ChatRequest, ChatResponse, Error, Result};

use super::betas::filter_bedrock_betas;
use super::BedrockConfig;

/// The Bedrock-required `anthropic_version` body field. Distinct from
/// the Anthropic API's `anthropic-version: 2023-06-01` header.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Build the InvokeModel request body from a routectl `ChatRequest`.
///
/// For Anthropic Claude models the body is Anthropic Messages JSON
/// patched with Bedrock-specific fields. For non-Anthropic Bedrock-hosted
/// vendors, prefer Converse (vendor-neutral) -- the InvokeModel adapter
/// here does not currently shape Mistral/Llama/Cohere bodies.
pub fn normalize_request(cfg: &BedrockConfig, req: &ChatRequest) -> Result<Value> {
    let mut body = crate::anthropic_api::request::normalize(
        &cfg.id,
        req,
        cfg.adaptive_thinking.unwrap_or(false),
    )?;
    let obj = body.as_object_mut().ok_or_else(|| {
        Error::NormalizeRequest(
            cfg.id.clone(),
            "anthropic_api::request::normalize returned non-object".into(),
        )
    })?;

    // Bedrock requires the body-side anthropic_version. Override
    // anything the upstream normalizer may have added.
    obj.insert(
        "anthropic_version".into(),
        Value::String(BEDROCK_ANTHROPIC_VERSION.into()),
    );

    // Merge the configured beta flags. If the body already has its own
    // anthropic_beta (e.g. from a per-request override), prepend the
    // provider-level flags so user overrides take precedence on
    // duplicate keys.
    if !cfg.anthropic_beta.is_empty() {
        let combined: Vec<Value> = cfg
            .anthropic_beta
            .iter()
            .cloned()
            .map(Value::String)
            .chain(
                obj.get("anthropic_beta")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            )
            .collect();
        obj.insert("anthropic_beta".into(), Value::Array(combined));
    }

    // Filter the merged anthropic_beta against the operator-supplied
    // `[bedrock] allowed_betas` list (routectl ships no const default).
    // Operator-supplied flags from `cfg.anthropic_beta`
    // (`[providers.X] anthropic_beta`) pass through unconditionally;
    // flags lifted from the inbound `anthropic-beta` HTTP header that
    // are not on the operator's accepted list drop at DEBUG. Without
    // this filter, claude-code's TS SDK 400s every Bedrock request
    // because it ships up to ten betas, only a subset of which AWS
    // gates for distribution. Shared with the Converse adapter via
    // `super::betas`.
    //
    // Empty `cfg.allowed_betas` puts the filter in pass-through mode
    // (every flag survives) -- the discovery default for operators
    // bringing up routectl against a fresh AWS account.
    filter_bedrock_betas(&cfg.id, obj, &cfg.anthropic_beta, &cfg.allowed_betas);

    // Merge any additional model request fields at the top level
    // with the same routectl-managed-keys allow-list as the Anthropic
    // and openai-compat egresses. The Bedrock-Invoke body is in
    // Anthropic shape (today); without this filter, a config-file
    // `additional_model_request_fields = { "messages" = [...] }`
    // would silently replace the assembled history. Filter here
    // (rather than at config load) so the WARN line carries the
    // provider id at request time and matches the operator's
    // existing log workflow.
    if let Some(extras) = cfg.additional_model_request_fields.as_ref() {
        if let Some(extras_obj) = extras.as_object() {
            for (k, v) in extras_obj {
                if is_bedrock_invoke_managed_key(k) {
                    tracing::warn!(
                        provider = %cfg.id,
                        key = %k,
                        "additional_model_request_fields attempted to override \
                         routectl-managed key; dropped"
                    );
                    continue;
                }
                obj.insert(k.clone(), v.clone());
            }
        }
    }

    // Stream flag is decided at HTTP level (different endpoint suffix),
    // not via a body field, so strip any leftover.
    obj.remove("stream");

    // Bedrock's InvokeModel takes the model identifier in the URL path
    // (`/model/{model_id}/invoke`), not the body. The Anthropic API
    // path requires it in the body, which `anthropic_api::request::
    // normalize` honors -- but Bedrock's strict-schema validator
    // rejects a body-side `model` with "Extra inputs are not
    // permitted". Strip it here on the Bedrock-Invoke seam.
    obj.remove("model");

    // Filter the assembled body against `[bedrock] allowed_body_fields`
    // so Anthropic-ingress forward-compat sweeps (`mcp_servers`,
    // `diagnostics`, `context_hint`, `speed`, ...) drop on the egress
    // before the request hits AWS. Without this filter, every
    // claude-code request 400s on the first unrecognized field --
    // Bedrock validates with strict-schema "Extra inputs are not
    // permitted". See `super::body_fields` for the full contract.
    super::body_fields::filter_bedrock_body_fields(
        &cfg.id,
        obj,
        &cfg.allowed_body_fields,
        super::body_fields::FilterContext::InvokeBody,
    );

    Ok(body)
}

/// Top-level Bedrock-Invoke body keys (Anthropic-shape today) that
/// `additional_model_request_fields` is NOT permitted to override.
/// Mirrors the Anthropic egress allow-list plus the Bedrock-specific
/// `anthropic_version` field. Long-tail Anthropic-only knobs
/// (`top_k`, `metadata`, `service_tier`) still pass through.
fn is_bedrock_invoke_managed_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "messages"
            | "system"
            | "max_tokens"
            | "thinking"
            | "tools"
            | "tool_choice"
            | "stream"
            | "stop_sequences"
            | "temperature"
            | "top_p"
            | "anthropic_beta"
            | "anthropic_version"
            | "cache_control"
    )
}

// `filter_bedrock_betas` moved to `super::betas`; the Converse adapter
// applies the identical filter via the same helper.

/// Parse the Bedrock InvokeModel response body into a `ChatResponse`.
///
/// For Anthropic Claude models this is exactly the Anthropic Messages
/// API response shape, so we delegate.
pub fn normalize_response(provider_id: &str, raw: Value) -> Result<ChatResponse> {
    crate::anthropic_api::response::normalize(provider_id, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    use crate::bedrock::{BedrockApiShape, BedrockCreds};

    fn fake_cfg() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            extra_headers: Vec::new(),
            anthropic_beta: vec!["context-1m-2025-08-07".into()],
            allowed_betas: vec![
                "context-1m-2025-08-07".into(),
                "claude-code-20250219".into(),
                "interleaved-thinking-2025-05-14".into(),
                "context-management-2025-06-27".into(),
                "effort-2025-11-24".into(),
                "fine-grained-tool-streaming-2025-05-14".into(),
                "computer-use-2025-01-24".into(),
                "computer-use-2024-10-22".into(),
                "mcp-client-2025-04-04".into(),
                "search-results-2025-06-09".into(),
            ],
            allowed_body_fields: full_body_fields(),
            // `top_p` is canonical and would now be filtered out;
            // use `top_k` here as a real long-tail Anthropic-only
            // knob the allow-list lets through.
            additional_model_request_fields: Some(json!({"top_k": 40})),
            adaptive_thinking: None,
        }
    }

    /// Empirical 2026-05-12 Bedrock body-field allowlist + `top_k`,
    /// reused across the Invoke test fixtures so a request lifted from
    /// the Anthropic ingress survives the body-field filter.
    fn full_body_fields() -> Vec<String> {
        vec![
            "anthropic_version".into(),
            "anthropic_beta".into(),
            "max_tokens".into(),
            "messages".into(),
            "system".into(),
            "temperature".into(),
            "top_p".into(),
            "top_k".into(),
            "tools".into(),
            "tool_choice".into(),
            "stop_sequences".into(),
            "thinking".into(),
            "output_config".into(),
            "cache_control".into(),
            "metadata".into(),
            "context_management".into(),
        ]
    }

    fn user_req() -> ChatRequest {
        ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            stream: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn body_includes_bedrock_anthropic_version_and_beta_flags() {
        let cfg = fake_cfg();
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        assert_eq!(body["anthropic_beta"], json!(["context-1m-2025-08-07"]),);
        assert_eq!(body["top_k"], json!(40));
        assert!(body.get("stream").is_none(), "stream should be stripped");
        assert!(
            body.get("model").is_none(),
            "model must be stripped: Bedrock takes it in the URL, not the body"
        );
    }

    #[test]
    fn additional_model_request_fields_cannot_override_managed_keys() {
        // Regression for the round 6 finding: a misconfigured
        // `additional_model_request_fields = { "messages" = [...] }`
        // would silently replace the assembled history. Now blocked
        // with a WARN, matching the openai-compat / anthropic_api
        // egress filters.
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            // managed -- MUST be dropped:
            "messages": [{"role": "user", "content": "INJECTED"}],
            "system": "INJECTED",
            "anthropic_version": "evil-version",
            "max_tokens": 1,
            // long-tail -- MUST pass through:
            "metadata": {"user_id": "u-1"},
        }));
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        // Anthropic-version stays as the Bedrock-required value.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        // Messages stays the user_req() text, NOT "INJECTED".
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        // System didn't get stomped.
        assert!(body.get("system").is_none() || body["system"] != "INJECTED");
        // max_tokens stays the request's value, not the override.
        assert_ne!(body["max_tokens"], 1);
        // Long-tail extras land verbatim.
        assert_eq!(body["metadata"]["user_id"], "u-1");
    }

    #[test]
    fn cache_control_on_user_text_round_trips_to_bedrock_invoke_body() {
        // v0.4.0 mandate: Anthropic-in -> Bedrock-Invoke-out path
        // preserves cache_control byte-for-byte. Bedrock-Invoke
        // delegates body construction to anthropic_api::request::
        // normalize, so this test pins the inheritance.
        use routectl_core::{
            cache_control::CacheControl, content_part::ContentPart, system_content::SystemContent,
            KnownContentPart, SystemBlock,
        };

        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));
        req.messages[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]);

        let body = normalize_request(&cfg, &req).unwrap();

        // Top-level cache_control on body.
        assert_eq!(body["cache_control"]["ttl"], "5m");
        // System block preserved with cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
        // User text block preserved with cache_control.
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(blk["cache_control"]["ttl"], "5m");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// FX-1: `BedrockConfig::adaptive_thinking = Some(true)` propagates
    /// through `normalize_request` -> `anthropic_api::request::normalize`
    /// and produces the Opus 4.7+ wire shape (no `budget_tokens`,
    /// top-level `output_config.effort`). This is the integration
    /// point that lets a Bedrock provider opt into adaptive thinking
    /// without touching the shared anthropic_api normalizer's
    /// signature beyond the `bool` flag.
    #[test]
    fn adaptive_thinking_propagates_from_bedrock_cfg() {
        use routectl_core::ReasoningConfig;
        let mut cfg = fake_cfg();
        cfg.adaptive_thinking = Some(true);

        let mut req = user_req();
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: Some(2000),
            exclude: None,
            enabled: Some(true),
        });
        let body = normalize_request(&cfg, &req).unwrap();

        // thinking is the adaptive shape (no budget_tokens) and the
        // effort moves to top-level output_config.
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    /// Structured output (`output_config.format`) flows through
    /// provider_extras -> Bedrock-Invoke body verbatim. Bedrock-Invoke
    /// for Claude is pure Anthropic-shape passthrough (only the
    /// `anthropic_version` body field differs from api.anthropic.com),
    /// so when the canonical request carries `output_config` in
    /// provider_extras (placed there by the Anthropic ingress, see
    /// `routectl_cli::ingress::anthropic::translate_request`), the
    /// Bedrock egress must emit the field unchanged. Confirms the
    /// user's "Layer 3 needs json_schema -> tool-use translation"
    /// theory is wrong: Bedrock-Invoke needs no extra translation.
    #[test]
    fn structured_output_format_passes_through_to_bedrock_invoke_body() {
        let cfg = fake_cfg();
        let mut req = user_req();
        req.provider_extras = Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
                }
            }
        }));
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }

    // -----------------------------------------------------------------
    // INV-6: anthropic_beta filter against Bedrock-accepted set
    // -----------------------------------------------------------------

    fn fake_cfg_no_betas() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            extra_headers: Vec::new(),
            anthropic_beta: vec![],
            allowed_betas: vec![
                "context-1m-2025-08-07".into(),
                "claude-code-20250219".into(),
                "interleaved-thinking-2025-05-14".into(),
                "context-management-2025-06-27".into(),
                "effort-2025-11-24".into(),
                "fine-grained-tool-streaming-2025-05-14".into(),
                "computer-use-2025-01-24".into(),
                "computer-use-2024-10-22".into(),
                "mcp-client-2025-04-04".into(),
                "search-results-2025-06-09".into(),
            ],
            allowed_body_fields: full_body_fields(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        }
    }

    /// Pre-canned request whose canonical anthropic_beta has 4 flags,
    /// 2 in the operator-supplied `allowed_betas` (from
    /// `fake_cfg_no_betas()`) and 2 not. After normalize_request, only
    /// the two accepted survive in the body.
    #[test]
    fn invoke_filters_unsupported_anthropic_beta_against_accepted_set() {
        // Arrange
        let cfg = fake_cfg_no_betas();
        let mut req = user_req();
        req.anthropic_beta = vec![
            "context-1m-2025-08-07".into(),           // accepted
            "oauth-2025-04-20".into(),                // rejected by Bedrock
            "interleaved-thinking-2025-05-14".into(), // accepted
            "redact-thinking-2026-02-12".into(),      // rejected by Bedrock
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: only the accepted subset survives.
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            strs.contains(&"context-1m-2025-08-07"),
            "missing accepted: {strs:?}"
        );
        assert!(
            strs.contains(&"interleaved-thinking-2025-05-14"),
            "missing accepted: {strs:?}"
        );
        assert!(
            !strs.contains(&"oauth-2025-04-20"),
            "rejected flag leaked through: {strs:?}"
        );
        assert!(
            !strs.contains(&"redact-thinking-2026-02-12"),
            "rejected flag leaked through: {strs:?}"
        );
    }

    /// Provider-config anthropic_beta survives the filter even when its
    /// contents are not in the routectl-shipped accepted set. This is
    /// the documented operator escape hatch for AWS allowlist drift
    /// (see CLAUDE.md gotcha + issues.md::INV-6).
    #[test]
    fn invoke_provider_config_betas_bypass_filter() {
        // Arrange
        let mut cfg = fake_cfg_no_betas();
        cfg.anthropic_beta = vec![
            // A flag NOT in the operator-supplied `allowed_betas`
            // list, but the operator typed it into
            // `[providers.X] anthropic_beta` -- they assert it is
            // safe (e.g. AWS gated it for their account before the
            // next routectl release).
            "future-flag-2026-12-31".into(),
        ];
        let req = user_req();

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: cfg-asserted flag survives.
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            strs.contains(&"future-flag-2026-12-31"),
            "operator-asserted flag was filtered out: {strs:?}"
        );
    }

    /// When all input flags are rejected, the field is removed from the
    /// body entirely (not emitted as `anthropic_beta: []`). This matches
    /// the wire shape callers without any betas already produce.
    #[test]
    fn invoke_strips_field_when_filter_empties_array() {
        // Arrange
        let cfg = fake_cfg_no_betas();
        let mut req = user_req();
        req.anthropic_beta = vec!["oauth-2025-04-20".into(), "files-api-2025-04-14".into()];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        assert!(
            body.get("anthropic_beta").is_none(),
            "filter emptied the array but field survived: {body}"
        );
    }

    /// issues.md::INV-6 dedup-vs-allowlist edge case.
    /// The Anthropic ingress's `merge_inbound_anthropic_beta_header`
    /// dedupes header-vs-body, but a direct caller (e.g. a library
    /// user constructing ChatRequest by hand) could supply duplicates.
    /// The filter must also dedup so a flag appearing twice in the
    /// canonical does not appear twice in the upstream body.
    #[test]
    fn invoke_preserves_dedup_when_header_and_body_share_flag() {
        // Arrange
        let cfg = fake_cfg_no_betas();
        let mut req = user_req();
        req.anthropic_beta = vec![
            "context-1m-2025-08-07".into(),
            "context-1m-2025-08-07".into(), // duplicate
            "claude-code-20250219".into(),
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert: duplicate collapses; both unique accepted flags survive.
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strs.iter()
                .filter(|s| **s == "context-1m-2025-08-07")
                .count(),
            1,
            "duplicate flag was not deduped: {strs:?}"
        );
        assert!(strs.contains(&"claude-code-20250219"), "missing: {strs:?}");
    }

    /// `cfg.allowed_betas` is sourced from the global
    /// `[bedrock] allowed_betas` TOML field. Lets operators add flags
    /// AWS gated post-release, or remove flags AWS deprecated, without
    /// a routectl rebuild.
    #[test]
    fn invoke_allowed_betas_filters_against_operator_list() {
        // Arrange: a request with two flags. One is in the operator's
        // list (kept), the other is not (dropped).
        let mut cfg = fake_cfg_no_betas();
        cfg.allowed_betas = vec!["future-flag-2026-12-31".into()];
        let mut req = user_req();
        req.anthropic_beta = vec![
            // NOT in operator list: should be DROPPED.
            "context-1m-2025-08-07".into(),
            // In operator list: should be ACCEPTED.
            "future-flag-2026-12-31".into(),
        ];

        // Act
        let body = normalize_request(&cfg, &req).unwrap();

        // Assert
        let arr = body["anthropic_beta"].as_array().unwrap();
        let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strs,
            vec!["future-flag-2026-12-31"],
            "allowed_betas filter did not match operator list: {strs:?}"
        );
    }
}
