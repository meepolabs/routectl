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
//! Dispatch order is fixed for stability across releases and is the
//! source of truth in `LIFT_STEPS`. `lift_all` iterates that slice;
//! `DOCUMENTED_DISPATCH_ORDER` mirrors its names; the
//! `dispatch_order_matches_documentation` test asserts they match.
//!
//! Order rationale (see `LIFT_STEPS` for the authoritative sequence):
//!   - `tools` and `tool_choice` are independent of message content.
//!   - `content` runs BEFORE `tool_use` so image rewriting sees the
//!     original assistant content array. `tool_use` may strip blocks
//!     and collapse `content` to a string or null after the lift.
//!   - `thinking` runs BEFORE `tool_use`: pulls Anthropic-shape
//!     `thinking` / `redacted_thinking` content blocks out of
//!     assistant content into the message-envelope `reasoning_details`
//!     field. The dialect's `preserve_history_reasoning` runtime
//!     (deepseek + vllm: lower to `reasoning_content`; openrouter:
//!     pass through typed) reads `reasoning_details` later, after
//!     `lift_all` returns. Running before `tool_use` means
//!     `tool_use`'s content-collapse logic sees the surviving blocks
//!     (text + tool_use) without confusion from leftover thinking.
//!   - `response_format` rewrites top-level keys only; runs after
//!     content/tool steps so none of them can clobber its output.
//!   - `tool_use` runs before `tool_result` because tool_use lifts
//!     INTO an assistant message (sibling fields), while tool_result
//!     SPLITS user messages into multiple wire messages. Doing tool_use
//!     first keeps message indices stable for tool_use's per-message
//!     edits; tool_result then reshapes the array shape.

mod content;
mod response_format;
mod thinking;
mod tool_choice;
mod tool_result;
mod tool_use;
mod tools;

use serde_json::Map;
use tracing::warn;

use routectl_core::{ChatRequest, Error, Result};

/// Reject (strict) or warn-and-drop (lenient) a canonical-only shape
/// that has no OpenAI-compat wire representation.
///
/// `context` names the lift site (e.g. "message 2 image block"); `what`
/// names the unrepresentable shape (e.g. "document content block"). In
/// strict mode the two compose into a `strict_translation` validation
/// error; in lenient mode they feed a structured warn before the caller
/// drops the offending shape.
pub fn reject_or_drop_unrepresentable(
    id: &str,
    strict: bool,
    context: &str,
    what: &str,
) -> Result<()> {
    if strict {
        return Err(Error::Validation(format!(
            "strict_translation: provider `{id}`: {context}: {what} \
             cannot be represented on the OpenAI-compat wire"
        )));
    }
    warn!(
        provider = id,
        context = context,
        what = what,
        "openai-compat egress: dropping unrepresentable shape"
    );
    Ok(())
}

/// Uniform lift-function pointer type. Every sub-module's `lift` must
/// match this shape so it can be stored in `LIFT_STEPS`.
type LiftFn = fn(&str, &mut Map<String, serde_json::Value>, &ChatRequest, bool) -> Result<()>;

/// Single source of truth for dispatch order. `lift_all` iterates this
/// slice; the order test introspects the same slice so a reorder in
/// either direction is caught at compile-test time.
///
/// To add a new step: append `(name, module::lift)` here. Do NOT edit
/// `lift_all` separately -- it derives from this slice.
const LIFT_STEPS: &[(&str, LiftFn)] = &[
    ("tools", tools::lift),
    ("tool_choice", tool_choice::lift),
    ("content", content::lift),
    ("thinking", thinking::lift),
    ("response_format", response_format::lift),
    ("tool_use", tool_use::lift),
    ("tool_result", tool_result::lift),
];

/// Documented names for the dispatch order. This parallel constant lets
/// the `dispatch_order_matches_documentation` test assert that it equals
/// the name projection of `LIFT_STEPS`, so they cannot diverge silently.
/// Only used in tests; defined here (not in the test module) so it stays
/// visible alongside `LIFT_STEPS` for maintenance.
#[cfg(test)]
pub const DOCUMENTED_DISPATCH_ORDER: &[&str] = &[
    "tools",
    "tool_choice",
    "content",
    "thinking",
    "response_format",
    "tool_use",
    "tool_result",
];

pub fn lift_all(
    id: &str,
    obj: &mut Map<String, serde_json::Value>,
    req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    for (_name, lift_fn) in LIFT_STEPS {
        lift_fn(id, obj, req, strict)?;
    }
    Ok(())
}

#[cfg(test)]
mod order_test {
    //! Pins the dispatch order of `lift_all` via `LIFT_STEPS`.
    //!
    //! If a contributor adds a new lift, they must:
    //!   1. Append `(name, module::lift)` to `LIFT_STEPS`.
    //!   2. Append the name to `DOCUMENTED_DISPATCH_ORDER`.
    //!   3. Verify both tests pass.
    //!
    //! Reordering either slice without updating the other will fail
    //! `dispatch_order_matches_documentation`. Reordering `LIFT_STEPS`
    //! alone also changes runtime behavior and will be caught by
    //! behavioral tests in the sub-module test suites.

    use serde_json::{Map, Value, json};

    use routectl_core::{ChatRequest, Message, MessageContent, Role};

    use super::{DOCUMENTED_DISPATCH_ORDER, LIFT_STEPS, lift_all};

    fn minimal_req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    /// Asserts that `LIFT_STEPS` names equal `DOCUMENTED_DISPATCH_ORDER`
    /// entry for entry. This is the pinning contract: both slices must be
    /// kept in sync. If you swap two rows in `LIFT_STEPS` without updating
    /// `DOCUMENTED_DISPATCH_ORDER` (or vice versa), this test fails.
    #[test]
    fn dispatch_order_matches_documentation() {
        // Arrange
        let actual: Vec<&str> = LIFT_STEPS.iter().map(|(n, _)| *n).collect();

        // Act + Assert
        assert_eq!(
            actual.as_slice(),
            DOCUMENTED_DISPATCH_ORDER,
            "LIFT_STEPS names must match DOCUMENTED_DISPATCH_ORDER. \
             Update both when adding or reordering lift steps."
        );
    }

    /// Smoke-tests that `lift_all` runs to completion on a minimal request.
    #[test]
    fn lift_all_runs_on_minimal_req() {
        // Arrange
        let req = minimal_req();
        let mut obj: Map<String, Value> = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();

        // Act
        let result = lift_all("test", &mut obj, &req, false);

        // Assert
        assert!(result.is_ok(), "lift_all failed on minimal req: {result:?}");
    }

    /// Verifies the dependency-critical ordering: content lift must run
    /// BEFORE tool_use. A request with an assistant message carrying a
    /// tool_use block should produce `tool_calls` on the assistant message
    /// (set by tool_use lift) without corrupting the content array that
    /// content lift may have transformed.
    ///
    /// If tool_use were moved before content in LIFT_STEPS, this test
    /// would still pass (tool_use does not read content-lift output), but
    /// `dispatch_order_matches_documentation` above pins the sequence
    /// against DOCUMENTED_DISPATCH_ORDER regardless.
    #[test]
    fn lift_all_tool_use_sees_messages_after_content_lift() {
        use routectl_core::ContentPart;

        // Arrange: assistant message with mixed text + tool_use blocks
        // (Anthropic shape). After lift_all the tool_use block should
        // become a top-level `tool_calls` array on the assistant message.
        let assistant_msg = Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(
                routectl_core::KnownContentPart::Text {
                    text: "here".into(),
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "toolu_x1",
                "type": "function",
                "function": {"name": "calc", "arguments": "{}"}
            })]),
        };
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                Message {
                    refusal: None,
                    role: Role::User,
                    content: MessageContent::Text("go".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                assistant_msg,
            ],
            ..Default::default()
        };

        let mut obj: Map<String, Value> = serde_json::to_value(&req)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();

        // Act
        lift_all("test", &mut obj, &req, false).unwrap();

        // Assert: messages array is present and has two entries.
        let msgs = obj["messages"].as_array().expect("messages must be array");
        assert_eq!(
            msgs.len(),
            2,
            "message count must be preserved after lift_all"
        );
    }

    /// Guards the dependency on LIFT_STEPS order: the `tools` step
    /// must run BEFORE `tool_choice` so that `tool_choice::lift` reads a
    /// populated `obj["tools"]`. A forcing tool_choice with real tools must
    /// therefore survive the full lift end-to-end. If a future reorder put
    /// tool_choice ahead of tools, the guard would see empty wire tools and
    /// drop the forcing choice -- this test would fail.
    #[test]
    fn lift_all_forcing_tool_choice_with_tools_survives() {
        use routectl_core::{CustomTool, ToolDef};

        // Arrange -- a request with a real custom tool AND a forcing
        // (`required`) tool_choice.
        let tool = ToolDef::Custom(CustomTool {
            name: "calculator".into(),
            description: Some("do math".into()),
            input_schema: json!({"type": "object", "properties": {}}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        });
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("go".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: Some(vec![tool]),
            tool_choice: Some(json!("required")),
            ..Default::default()
        };
        let mut obj: Map<String, Value> = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "go"}]
        }))
        .unwrap();

        // Act
        lift_all("test", &mut obj, &req, false).unwrap();

        // Assert -- tools step populated obj["tools"] before tool_choice::lift
        // ran, so the forcing choice was NOT dropped by the forcing-choice guard.
        let tools = obj["tools"].as_array().expect("tools must be on the wire");
        assert_eq!(tools.len(), 1, "the custom tool must reach the wire");
        assert_eq!(
            obj.get("tool_choice"),
            Some(&json!("required")),
            "forcing tool_choice must survive when tools are present (LIFT_STEPS \
             must run tools before tool_choice)"
        );
    }
}
