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
//! Dispatch order is fixed for stability across releases. As of
//! Wave 1 (`tools` + `tool_choice` only), neither lift consults the
//! other's state: tools are translated independently of the chosen
//! tool, and tool_choice's name passthrough does not validate against
//! the lifted tool list (an unknown name surfaces as the upstream's
//! own validation error rather than routectl-side rejection).
//!
//! When M2 adds `content`, `tool_use`, `tool_result`, and
//! `response_format` lifts, the order will need real
//! before/after constraints (e.g. content before tool_use because
//! tool_use blocks may strip text siblings); pinning the order now
//! avoids cascading edits later.
//!
//! Current order:
//!   1. tools
//!   2. tool_choice
//!   // content::lift        (TODO(M2)) -- content before tool_use
//!   //                                   so tool_use sees originals.
//!   // tool_use::lift       (TODO(M2)) -- may strip blocks; sees originals.
//!   // tool_result::lift    (TODO(M2))
//!   // response_format::lift (TODO(M2)) -- last on request body.

mod tools;
mod tool_choice;

use routectl_core::{ChatRequest, Result};

pub fn lift_all(
    id: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
    req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    tools::lift(id, obj, req, strict)?;
    tool_choice::lift(id, obj, req)?;
    Ok(())
}
