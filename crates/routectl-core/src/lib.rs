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
pub mod schema;
pub mod system_content;
pub mod tool_def;

pub use cache_control::{Breakpoint, BreakpointPosition, CacheControl};
pub use content_part::{ContentPart, KnownContentPart};
pub use error::{Error, Result};
pub use log_safe::{
    debug_upstream_error_body, sanitize_for_log, sanitize_upstream_body,
    sanitize_upstream_body_with_cap, trace_outgoing_body,
};
pub use provider::Provider;
pub use schema::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Message, MessageContent,
    Reasoning, ReasoningConfig, ReasoningDetail, ReasoningDetailKind, Role, Usage, UsageDelta,
};
pub use system_content::{SystemBlock, SystemContent};
pub use tool_def::{CustomTool, ToolDef};

/// Cross-crate cap on `body_excerpt` fields in upstream-error tracing
/// logs. Sized to fit a typical AWS IAM error body (User ARN + action +
/// resource ARN runs ~300 chars) plus headroom, while keeping log lines
/// scannable. Used by both the bedrock and openai-compat providers so
/// operators see consistent excerpt lengths across `body_excerpt=...`
/// fields when grepping logs.
pub const MAX_LOG_BODY_EXCERPT: usize = 512;
