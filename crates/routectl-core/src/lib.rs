//! Core types and Provider trait for routectl.
//!
//! Schema follows the OpenRouter normalized shape so any client that speaks
//! OpenRouter speaks routectl. See `schema` for request/response types and
//! `provider` for the per-backend trait.

pub mod error;
pub mod provider;
pub mod schema;

pub use error::{Error, Result};
pub use provider::Provider;
pub use schema::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Message, MessageContent,
    Reasoning, ReasoningConfig, ReasoningDetail, ReasoningDetailKind, Role, Usage,
};
