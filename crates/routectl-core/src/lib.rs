//! Core types and Provider trait for routectl.
//!
//! Schema follows the OpenRouter normalized shape so any client that speaks
//! OpenRouter speaks routectl. See `schema` for request/response types and
//! `provider` for the per-backend trait.

pub mod cache_control;
pub mod content_part;
pub mod error;
pub mod log_safe;
pub mod provider;
pub mod reasoning_dialect;
pub mod reserved;
pub mod schema;
pub mod schema_opaque;
pub mod system_content;
pub mod token_source;
pub mod tool_def;

pub use cache_control::{Breakpoint, BreakpointPosition, CacheControl};
pub use content_part::{ContentPart, KnownContentPart};
pub use error::{Error, Result};
pub use log_safe::{
    debug_upstream_error_body, extract_structural_summary, extract_upstream_message,
    header_trace_enabled, headers_to_json, init_log_overrides, redact_prompts_in,
    redact_prompts_with_flag, sanitize_for_log, sanitize_upstream_body,
    sanitize_upstream_body_with_cap, trace_body_cap, trace_egress_body, trace_egress_headers,
    trace_ingress_body, trace_ingress_headers, trace_outgoing_body, trace_outgoing_headers,
    trace_stream_summary, trace_structural_summary, trace_upstream_response_headers,
    trace_upstream_success_body, wrap_stream_with_summary, StructuralSummary, HDR_MSG_EGRESS,
    HDR_MSG_INGRESS, HDR_MSG_OUTGOING, HDR_MSG_UPSTREAM, MAX_TRACE_BODY_BYTES,
};
// Re-export the deprecated alias from the crate root so downstream
// consumers that imported the old name from `routectl_core::*` get a
// `#[deprecated]` warning instead of a compile error. Kept until the
// next breaking release.
#[allow(deprecated)]
pub use log_safe::MAX_TRACE_OUTGOING_BODY_BYTES;
pub use provider::Provider;
pub use reasoning_dialect::{
    HistoryReasoning as CoreHistoryReasoning, ReasoningDialect as CoreReasoningDialect,
};
pub use reserved::is_canonical_request_key;
pub use schema::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Message, MessageContent,
    Reasoning, ReasoningConfig, ReasoningDetail, ReasoningDetailKind, Role, RoutectlInternal,
    TokenCount, Usage, UsageDelta,
};
pub use schema_opaque::OpaqueSseEvent;
pub use system_content::{SystemBlock, SystemContent};
pub use token_source::{StaticToken, TokenSource};
pub use tool_def::{CustomTool, ToolDef};

/// Cross-crate cap on `body_excerpt` fields in upstream-error tracing
/// logs. Sized to fit a typical AWS IAM error body (User ARN + action +
/// resource ARN runs ~300 chars) plus headroom, while keeping log lines
/// scannable. Used by both the bedrock and openai-compat providers so
/// operators see consistent excerpt lengths across `body_excerpt=...`
/// fields when grepping logs.
pub const MAX_LOG_BODY_EXCERPT: usize = 512;
