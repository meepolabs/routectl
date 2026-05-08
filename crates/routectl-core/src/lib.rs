//! Core types and Provider trait for routectl.
//!
//! Schema follows the OpenRouter normalized shape so any client that speaks
//! OpenRouter speaks routectl. See `schema` for request/response types and
//! `provider` for the per-backend trait.

pub mod cache_control;
pub mod content_part;
pub mod error;
pub mod provider;
pub mod schema;
pub mod system_content;
pub mod tool_def;

pub use cache_control::{Breakpoint, BreakpointPosition, CacheControl};
pub use content_part::{ContentPart, KnownContentPart};
pub use error::{Error, Result};
pub use provider::Provider;
pub use schema::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Message, MessageContent,
    Reasoning, ReasoningConfig, ReasoningDetail, ReasoningDetailKind, Role, Usage, UsageDelta,
};
pub use system_content::{SystemBlock, SystemContent};
pub use tool_def::{CustomTool, ToolDef};
