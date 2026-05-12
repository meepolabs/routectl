//! Wire-shape lifter for the OpenAI-compat egress.
//!
//! This module sits between `dyn_dialect.apply_request` (line 108 of
//! `request.rs`) and the `provider_extras` merge (line 114). Running
//! BEFORE the extras merge means an operator-supplied
//! `provider_extras = {"tools": [...]}` cannot clobber a lift that
//! just rewrote canonical Anthropic-shape tools into OpenAI function
//! shape. The extras merge allow-list enforces the same invariant at
//! the key level, but defense in depth requires lift-before-merge.
//!
//! Dispatch order is fixed for stability across releases.
//!
//! Order rationale:
//!   - `tools` and `tool_choice` are independent of message content.
//!   - `content` runs BEFORE `tool_use` so image rewriting sees the
//!     original assistant content array. `tool_use` may strip blocks
//!     and collapse `content` to a string or null after the lift.
//!   - `response_format` rewrites top-level keys only and runs last so
//!     no later lift can clobber its output.
//!   - `tool_use` runs before `tool_result` because tool_use lifts
//!     INTO an assistant message (sibling fields), while tool_result
//!     SPLITS user messages into multiple wire messages. Doing tool_use
//!     first keeps message indices stable for tool_use's per-message
//!     edits; tool_result then reshapes the array shape.
//!
//! Current order:
//!   1. tools
//!   2. tool_choice
//!   3. content
//!   4. response_format
//!   5. tool_use
//!   6. tool_result

mod content;
mod response_format;
mod tool_choice;
mod tool_result;
mod tool_use;
mod tools;

use routectl_core::{ChatRequest, Result};

pub fn lift_all(
    id: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
    req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    tools::lift(id, obj, req, strict)?;
    tool_choice::lift(id, obj, req)?;
    content::lift(id, obj, req, strict)?;
    response_format::lift(id, obj, req, strict)?;
    tool_use::lift(id, obj, req, strict)?;
    tool_result::lift(id, obj, req, strict)?;
    Ok(())
}
