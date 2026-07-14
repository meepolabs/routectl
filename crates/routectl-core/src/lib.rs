//! Core types and Provider trait for routectl.
//!
//! Schema follows the OpenRouter normalized shape so any client that speaks
//! OpenRouter speaks routectl. See `schema` for request/response types and
//! `provider` for the per-backend trait.

pub mod cache_control;
pub mod capability;
pub mod cloud_project;
pub mod content_part;
pub mod context_reduction;
pub mod error;
pub mod failure_class;
pub mod identity;
pub mod log_safe;
pub mod provider;
pub mod reasoning_dialect;
pub mod reserved;
pub mod schema;
pub mod schema_opaque;
pub mod system_content;
pub mod token_source;
pub mod tool_def;
pub mod upstream_meta;
pub mod volatile;

/// Shared canonical-request / canonical-response builders for the
/// cross-crate contract tests. Compiled only under `cfg(test)` or the
/// `test-utils` feature so the fixtures never ship in release builds.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub use cache_control::{
    Breakpoint, BreakpointPosition, CacheBreakpointSource, CacheControl, FrozenFloor,
    OwnedBreakpoint, compute_frozen_floor, mutable_suffix_start, validate_source,
};
pub use capability::{
    COMPUTER_USE, STRUCTURED_OUTPUT, SignalTier, WEB_SEARCH, WELL_KNOWN_CAPABILITY_KEYS,
    normalize_capability_key,
};
pub use cloud_project::{CloudProjectCache, InMemoryProjectCache};
pub use content_part::{ContentPart, KnownContentPart};
pub use context_reduction::{
    ReductionDelta, ReductionOutcome, apply_json_minify, minify_json_whitespace,
};
pub use error::{Error, Result};
pub use log_safe::{
    HDR_MSG_EGRESS, HDR_MSG_INGRESS, HDR_MSG_OUTGOING, HDR_MSG_UPSTREAM, MAX_TRACE_BODY_BYTES,
    StructuralSummary, debug_upstream_error_body, extract_upstream_message, header_trace_enabled,
    headers_to_json, init_log_overrides, is_json_error_envelope, redact_prompts_in,
    sanitize_for_log, sanitize_upstream_body, sanitize_upstream_body_with_cap, trace_body_cap,
    trace_egress_body, trace_egress_headers, trace_ingress_body, trace_ingress_headers,
    trace_outgoing_body, trace_outgoing_headers, trace_stream_summary, trace_structural_summary,
    trace_upstream_response_headers, trace_upstream_success_body, wrap_stream_with_summary,
};
pub use provider::{ProbeOutcome, Provider};
pub use reasoning_dialect::{
    HistoryReasoning as CoreHistoryReasoning, ReasoningDialect as CoreReasoningDialect,
};
pub use reserved::is_canonical_request_key;
pub use schema::{
    CacheCreation, ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta,
    ForwardedBearer, Message, MessageContent, Reasoning, ReasoningConfig, ReasoningDetail,
    ReasoningDetailKind, RequestProvenance, Role, RoutectlInternal, TokenCount, Usage, UsageDelta,
};
pub use schema_opaque::OpaqueSseEvent;
pub use system_content::{SystemBlock, SystemContent};
pub use token_source::{StaticToken, TokenSource};
pub use tool_def::{CustomTool, ToolDef};
pub use upstream_meta::{AnthropicUnifiedQuota, UpstreamMeta};
pub use volatile::{VolatileConfidence, VolatileKind, VolatileReport, scan_volatile};

/// Cross-crate cap on `body_excerpt` fields in upstream-error tracing
/// logs. Sized to fit a typical AWS IAM error body (User ARN + action +
/// resource ARN runs ~300 chars) plus headroom, while keeping log lines
/// scannable. Used by both the bedrock and openai-compat providers so
/// operators see consistent excerpt lengths across `body_excerpt=...`
/// fields when grepping logs.
pub const MAX_LOG_BODY_EXCERPT: usize = 512;
