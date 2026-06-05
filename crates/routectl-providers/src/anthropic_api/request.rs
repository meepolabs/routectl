//! Request normalization: routectl shape -> Anthropic wire format.
//!
//! v0.4.0: rewritten to consume the typed canonical (ContentPart,
//! SystemContent, ToolDef) so cache_control round-trips end-to-end on
//! the Anthropic-in / Anthropic-out and Anthropic-in / Bedrock-Invoke-out
//! paths. Forward-compat: ContentPart::Other and ToolDef::Other pass
//! through verbatim, so a new Anthropic block or builtin tool ships
//! without code edits here.
//!
//! Translation rules:
//! - `req.system` is read directly into the wire `system` field (Text or
//!   Blocks). Backwards-compatible fallback: when `req.system` is None,
//!   any Role::System messages in `req.messages` get lifted (today's
//!   behavior) so direct callers without an ingress aren't broken.
//! - User content is translated typed-block-by-typed-block. Unknown
//!   blocks pass through via ContentPart::Other -> ContentBlock::Other.
//! - Assistant content with reasoning_details (multi-turn tool-use)
//!   continues to require a signature on each thinking block.
//! - Tool message: the canonical Tool role becomes a user message with
//!   a tool_result block, same as today.
//! - Tools: ToolDef::Custom -> AnthropicTool::Custom (cache_control,
//!   defer_loading, strict, optional type_tag); ToolDef::Other ->
//!   AnthropicTool::Builtin (passthrough Value).
//! - Top-level cache_control and anthropic_beta are set on the body.
//! - cache_control::validate runs before serialization (debug_assert
//!   only; keeps non-debug builds fast).

use std::borrow::Cow;

use serde_json::{json, Value};

use routectl_core::cache_control::{self, Breakpoint, BreakpointPosition};
use routectl_core::{
    is_canonical_request_key, ChatRequest, ContentPart, CoreHistoryReasoning, CustomTool, Error,
    KnownContentPart, Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Result, Role,
    SystemContent, ToolDef,
};

use crate::effort::clamp_effort_to_supported;

use super::parts::{parse_image_url_source, strip_text_after_tool_use};
use super::types::{
    AnthropicContent, AnthropicMessage, AnthropicRequest, AnthropicRole, AnthropicSystem,
    AnthropicSystemBlock, AnthropicTool, ContentBlock, OutputConfig, ThinkingConfig,
};

/// Hardcoded baseline `max_tokens` value injected on outbound
/// Anthropic-shape requests when the caller omits the field AND the
/// per-model `[models.X].max_output_tokens` override is unset. The
/// Anthropic Messages API requires `max_tokens` and 400s on omission;
/// 64000 is above the 64K ceiling of Sonnet 4.5/4.6 and Opus 4.5 and
/// within Opus 4.7's 128K window.
///
/// Operators with known-low-cap models (Anthropic Opus 4 / 4.1 at
/// 32000, Sonnet 3.5 / DeepSeek V3 at 8000) should set
/// `[models.X].max_output_tokens` to avoid an upstream 400 on the
/// per-model ceiling check. See docs/CONFIGURATION.md.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 64_000;

/// Resolve the outbound `max_tokens` value: caller's request wins;
/// absent that, the router-supplied per-model override from
/// `req.routectl_internal.max_output_tokens` (when non-zero) wins;
/// absent that, the hardcoded `DEFAULT_MAX_OUTPUT_TOKENS` baseline
/// (64000).
///
/// Consumed by Anthropic-shape egresses (anthropic-api +
/// bedrock-invoke; bedrock-invoke delegates body construction to this
/// module's `normalize`). Other egresses (openai-compat,
/// openai-responses, bedrock-converse) forward `req.max_tokens` cleanly
/// when None per the good-translator principle.
fn resolve_max_tokens(req: &ChatRequest) -> u32 {
    if let Some(v) = req.max_tokens {
        return v;
    }
    let from_internal = req.routectl_internal.max_output_tokens;
    if from_internal > 0 {
        return from_internal;
    }
    DEFAULT_MAX_OUTPUT_TOKENS
}

/// Anthropic `tool_choice.type` values that force tool use. Pairing
/// either with `thinking` causes a 400 (extended-thinking docs).
const TOOL_CHOICE_TYPE_ANY: &str = "any";
const TOOL_CHOICE_TYPE_TOOL: &str = "tool";

/// Anthropic Messages API minimum for `thinking.budget_tokens` on the
/// legacy `ThinkingConfig::Enabled` wire shape. Anthropic 400s any
/// value below this AND requires `max_tokens > budget_tokens`. The
/// smallest legal body that carries legacy thinking has
/// `max_tokens > 1024` (1024 budget plus at least 1 visible token).
const ANTHROPIC_MIN_THINKING_BUDGET: u32 = 1024;

/// True iff the caller's `max_tokens` can accommodate the Anthropic
/// minimum thinking budget plus at least one visible-output token.
/// Used by `build_thinking` to drop the legacy `Enabled` shape on
/// probe-sized requests (title generation, topic summaries) instead
/// of emitting a body that Anthropic would 400. The adaptive shape
/// has no equivalent floor and is unaffected.
fn legacy_thinking_fits(req: &ChatRequest) -> bool {
    let max = resolve_max_tokens(req);
    max > ANTHROPIC_MIN_THINKING_BUDGET
}

/// Proportional budget_tokens as fraction of max_tokens per effort level.
/// Only consulted on the legacy `ThinkingConfig::Enabled` path -- the
/// adaptive-thinking path passes `effort` through verbatim into
/// `output_config.effort` and never calls this.
///
/// Must stay in sync with VALID_EFFORT_TOKENS in
/// routectl-providers/src/effort.rs. A new token added to that const
/// without a corresponding arm here returns the default ratio silently.
fn effort_ratio(effort: &str) -> f64 {
    match effort {
        // `max` arrived with the Opus 4.7+ adaptive thinking shape,
        // but the legacy `Enabled { budget_tokens }` path may still
        // see it on a non-adaptive provider. 0.99 leaves 1% of
        // max_tokens for the visible response so the request is
        // accepted; in practice operators who want `max` should set
        // `supports_adaptive_thinking = true` on the model config.
        "max" => 0.99,
        "xhigh" => 0.95,
        "high" => 0.80,
        "medium" => 0.50,
        "low" => 0.20,
        "minimal" => 0.10,
        _ => 0.50,
    }
}

/// Effort string to use for top-level `output_config.effort`. Returns
/// `req.reasoning.effort` clamped against `req.routectl_internal.effort_levels`
/// when that slice is non-empty (operator cost cap); falls back to the
/// raw effort string when effort_levels is empty (Anthropic pass-through
/// default). Falls back to "medium" when no effort is set (Anthropic
/// requires the field when adaptive thinking is active and validates
/// the string).
fn derive_effort(req: &ChatRequest) -> String {
    let raw = req
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .unwrap_or_else(|| "medium".to_string());
    clamp_effort_to_supported(&raw, &req.routectl_internal.effort_levels).into_owned()
}

/// Decide which `ThinkingConfig` variant (if any) to emit. The
/// `adaptive` flag selects the wire shape: when `true` AND thinking
/// would otherwise be `Enabled`, returns `Adaptive` instead (the
/// caller pairs that with a top-level `output_config`); when `false`,
/// returns the legacy `Enabled { budget_tokens }` shape; `Disabled`
/// is always returned verbatim regardless of the flag.
///
/// The `adaptive` flag comes from
/// `req.routectl_internal.supports_adaptive_thinking`, set by the
/// router from the operator-declared `[models.X]
/// supports_adaptive_thinking` capability.
///
/// On the legacy `Enabled` path, `req.routectl_internal.max_thinking_budget`
/// is applied as an operator-declared ceiling BEFORE Anthropic's own
/// `[1024, max_tokens-1]` window clamp. Zero means no operator cap.
///
/// Note on `max_tokens` + adaptive: Anthropic's adaptive thinking wire
/// shape has no field for an explicit budget -- the model picks its
/// own from the effort string. If a caller sets both
/// `reasoning.max_tokens` AND the model is adaptive, the budget is
/// dropped (with a tracing::warn at the call site). The caller's
/// effort string still travels to `output_config.effort`.
pub(crate) fn build_thinking(req: &ChatRequest, adaptive: bool) -> Option<ThinkingConfig> {
    let r = req.reasoning.as_ref()?;

    if r.enabled == Some(false) {
        return Some(ThinkingConfig::Disabled);
    }
    if r.effort.as_deref() == Some("none") {
        return Some(ThinkingConfig::Disabled);
    }

    // Did the caller actually ask for thinking? Any of: explicit
    // enabled=true, a budget, an effort string (other than "none").
    let thinking_active = r.enabled == Some(true)
        || r.max_tokens.is_some()
        || r.effort.as_deref().is_some_and(|e| e != "none");
    if !thinking_active {
        return None;
    }

    if adaptive {
        // Opus 4.7+ wire shape. budget_tokens is gone; effort moves
        // to top-level output_config (handled by build_output_config).
        // If the caller set both an explicit budget AND the model is
        // adaptive, the budget gets dropped because there's no wire
        // field for it. Warn so an operator who set both fields
        // routinely (e.g. a client library that always sends
        // `reasoning.max_tokens`) can see the discard in logs and
        // adjust to using `effort` instead.
        if r.max_tokens.is_some() {
            tracing::warn!(
                budget_tokens = r.max_tokens,
                "reasoning.max_tokens dropped on adaptive thinking path; \
                 Anthropic's adaptive shape has no budget field -- \
                 the model picks its own budget from output_config.effort. \
                 Set reasoning.effort to steer instead."
            );
        }
        return Some(ThinkingConfig::Adaptive);
    }

    // Legacy wire shape constraint: Anthropic requires
    // `budget_tokens >= 1024` AND `max_tokens > budget_tokens`. Probe-
    // sized requests (claude-code title gen, topic summaries) routinely
    // send `max_tokens=64`; emitting any legacy `Enabled` body for
    // those would 400 upstream. Drop thinking for this one request
    // rather than reshape the caller's `max_tokens`.
    if !legacy_thinking_fits(req) {
        tracing::warn!(
            request_max_tokens = req.max_tokens,
            min_required = ANTHROPIC_MIN_THINKING_BUDGET + 1,
            reasoning_effort = ?r.effort,
            reasoning_max_tokens = ?r.max_tokens,
            "anthropic legacy thinking shape requires max_tokens > 1024; \
             dropping thinking for this probe-sized request. Set \
             supports_adaptive_thinking=true on the model config for Opus 4.7+ \
             to avoid the budget-vs-max_tokens coupling, or send max_tokens > 1024."
        );
        return None;
    }

    // Legacy wire shape: Enabled { budget_tokens }. Translate the
    // canonical signal (explicit budget > effort > enabled=true).
    // Every arm runs the budget through `clamp_budget_to_legacy_window`,
    // which enforces BOTH Anthropic invariants:
    //   - `budget_tokens >= 1024` (floor); a sub-1024 explicit budget
    //     gets raised with a WARN so an operator can see the silent
    //     promotion. The effort/enabled arms can only land below the
    //     floor in the 1025-1279 (effort=high) band; same clamp.
    //   - `budget_tokens < max_tokens` (ceiling); an explicit budget
    //     that exceeds `req.max_tokens` would otherwise produce a
    //     wire body Anthropic 400s. The clamp caps at
    //     `max.saturating_sub(1)` to leave at least one visible-
    //     output token. The gate above guarantees `max > 1024`, so
    //     the ceiling (`max - 1`) is always at least 1024 and the
    //     floor never collides with the ceiling.
    let max = resolve_max_tokens(req);
    // Operator-declared per-model ceiling. Non-zero values cap the
    // budget DOWN before Anthropic's own `[1024, max_tokens-1]` window
    // clamp runs. Zero means no operator cap -- Anthropic's clamp is
    // the only guard.
    let operator_cap = req.routectl_internal.max_thinking_budget;

    if let Some(budget) = r.max_tokens {
        let budget = apply_operator_cap(budget, operator_cap);
        return Some(ThinkingConfig::Enabled {
            budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Explicit),
        });
    }
    if let Some(effort) = r.effort.as_deref() {
        let clamped = clamp_effort_to_supported(effort, &req.routectl_internal.effort_levels);
        let budget = ((max as f64) * effort_ratio(clamped.as_ref())).max(1.0) as u32;
        let budget = apply_operator_cap(budget, operator_cap);
        return Some(ThinkingConfig::Enabled {
            budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Derived),
        });
    }
    // r.enabled == Some(true) without budget or effort.
    let budget = apply_operator_cap(max / 2, operator_cap);
    Some(ThinkingConfig::Enabled {
        budget_tokens: clamp_budget_to_legacy_window(budget, max, BudgetSource::Derived),
    })
}

/// Origin of a `budget_tokens` value about to be clamped to
/// Anthropic's legal `[1024, max_tokens-1]` window. Used to gate
/// whether a silent floor promotion should WARN: `Explicit` means
/// the caller asked for a specific number and we are about to ignore
/// it -- worth a log line; `Derived` means routectl computed the
/// number from `effort_ratio` or the `enabled=true` half-of-max
/// fallback, and the operator's per-model config implicitly opted in.
#[derive(Copy, Clone)]
enum BudgetSource {
    Explicit,
    Derived,
}

/// Apply the operator-declared per-model thinking-budget cap. When
/// `operator_cap` is non-zero, the budget is clamped DOWN to that
/// ceiling before Anthropic's own `[1024, max_tokens-1]` window clamp
/// runs. Zero is the sentinel for "no operator cap" and returns the
/// budget unchanged.
fn apply_operator_cap(budget: u32, operator_cap: u32) -> u32 {
    if operator_cap > 0 {
        budget.min(operator_cap)
    } else {
        budget
    }
}

/// Bring `budget` into Anthropic's `[1024, max_tokens-1]` window for
/// the legacy `Enabled` wire shape. The gate at the top of
/// `build_thinking` guarantees `max > 1024`, so `max - 1 >= 1024`
/// and the window is non-empty. On an explicit caller budget that
/// gets clamped UP from below the floor, fire a WARN so the operator
/// can correlate "I asked for 500 tokens of thinking, why is the
/// model using 1024" with a single grep.
fn clamp_budget_to_legacy_window(budget: u32, max: u32, source: BudgetSource) -> u32 {
    let ceiling = max.saturating_sub(1);
    let clamped = budget.max(ANTHROPIC_MIN_THINKING_BUDGET).min(ceiling);
    if matches!(source, BudgetSource::Explicit) && budget < ANTHROPIC_MIN_THINKING_BUDGET {
        tracing::warn!(
            requested_budget = budget,
            clamped_to = clamped,
            "reasoning.max_tokens below Anthropic legacy minimum (1024); \
             clamping up. The model will use more thinking budget than \
             the caller asked for."
        );
    }
    clamped
}

/// Pair `ThinkingConfig::Adaptive` with a top-level `output_config`.
/// Returns `Some(OutputConfig)` only when the thinking variant is
/// `Adaptive`; otherwise `None`.
fn build_output_config(
    req: &ChatRequest,
    thinking: &Option<ThinkingConfig>,
) -> Option<OutputConfig> {
    if matches!(thinking, Some(ThinkingConfig::Adaptive)) {
        Some(OutputConfig {
            effort: derive_effort(req),
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// Convert canonical `SystemContent` to wire `AnthropicSystem`. Preserves
/// per-block cache_control and citations.
pub(crate) fn translate_system(s: &SystemContent) -> AnthropicSystem {
    match s {
        SystemContent::Text(t) => AnthropicSystem::Text(t.clone()),
        SystemContent::Blocks(blocks) => AnthropicSystem::Blocks(
            blocks
                .iter()
                .map(|b| AnthropicSystemBlock {
                    kind: b.kind.clone(),
                    text: b.text.clone(),
                    cache_control: b.cache_control.clone(),
                    citations: b.citations.clone(),
                })
                .collect(),
        ),
    }
}

/// Backwards-compat fallback: lift Role::System messages out of the
/// messages array into a flat AnthropicSystem::Text. Used only when
/// `req.system` is None. Returns None when no System messages are
/// present, or when all System messages contain only non-text content
/// (Parts without text blocks, Null) -- avoids emitting a meaningless
/// `system: ""` upstream and the extra newlines from joining blanks.
///
/// `pub(crate)` so the Bedrock Converse egress can reuse the same
/// legacy-shape fallback (single source of truth).
pub(crate) fn lift_legacy_system(messages: &[Message]) -> Option<AnthropicSystem> {
    let texts: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .filter_map(|m| match &m.content {
            MessageContent::Text(t) => {
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(t.clone())
                }
            }
            MessageContent::Parts(parts) => {
                // Pick out text content from typed parts. Image/Document/etc.
                // in a System message are not meaningful for the flat-text
                // lift and would have been dropped by the egress anyway.
                let collected: Vec<String> = parts
                    .iter()
                    .filter_map(|p| match p {
                        routectl_core::ContentPart::Known(
                            routectl_core::KnownContentPart::Text { text, .. },
                        ) => {
                            let trimmed = text.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(text.clone())
                            }
                        }
                        _ => None,
                    })
                    .collect();
                if collected.is_empty() {
                    None
                } else {
                    Some(collected.join("\n"))
                }
            }
            MessageContent::Null => None,
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(AnthropicSystem::Text(texts.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn translate_custom_tool(c: &CustomTool) -> AnthropicTool {
    AnthropicTool::Custom {
        name: c.name.clone(),
        description: c.description.clone(),
        input_schema: c.input_schema.clone(),
        cache_control: c.cache_control.clone(),
        defer_loading: c.defer_loading,
        strict: c.strict,
        type_tag: c.type_tag.clone(),
    }
}

pub(crate) fn translate_tool(td: &ToolDef) -> AnthropicTool {
    match td {
        ToolDef::Custom(c) => translate_custom_tool(c),
        ToolDef::Other(v) => {
            // Backwards-compat: a legacy OpenAI-shape tool
            // `{type: "function", function: {name, description, parameters}}`
            // arriving via ToolDef::Other gets translated to
            // AnthropicTool::Custom so callers that bypass the OpenAI
            // ingress still get a working Anthropic body. Anything else
            // (Anthropic builtins, server-side, future shapes) passes
            // through verbatim as Builtin.
            if let Some(custom) = openai_function_to_custom(v) {
                custom
            } else {
                AnthropicTool::Builtin(v.clone())
            }
        }
    }
}

fn openai_function_to_custom(v: &Value) -> Option<AnthropicTool> {
    let obj = v.as_object()?;
    let is_function = obj.get("type").and_then(|t| t.as_str()) == Some("function");
    if !is_function {
        return None;
    }
    let func = obj.get("function")?.as_object()?;
    let name = func.get("name")?.as_str()?.to_string();
    let description = func
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let input_schema = func
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    let strict = func.get("strict").and_then(|v| v.as_bool());
    Some(AnthropicTool::Custom {
        name,
        description,
        input_schema,
        cache_control: None,
        defer_loading: None,
        strict,
        type_tag: None,
    })
}

/// Translate canonical `tool_choice` values into the Anthropic-shape
/// object the Messages API requires.
///
/// Mapping:
///   - bare `"auto"` -> `{"type":"auto"}`
///   - bare `"required"` -> `{"type":"any"}`
///   - bare `"none"` -> field dropped; the caller must also drop
///     `tools` (otherwise Anthropic defaults to `auto` and may call
///     them, silently flipping the caller's "do not call tools" intent)
///   - OpenAI `{"type":"function","function":{"name":X}}` ->
///     `{"type":"tool","name":X}`
///   - already-Anthropic shape -> passthrough
///   - anything else -> passthrough (let the upstream decide)
fn translate_tool_choice(tc: Option<&Value>, has_tools: bool) -> Option<Value> {
    let tc = tc?;
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(serde_json::json!({"type":"auto"})),
            "required" => Some(serde_json::json!({"type": TOOL_CHOICE_TYPE_ANY})),
            "none" => {
                if has_tools {
                    tracing::warn!(
                        "tool_choice=\"none\" with tools present: routectl drops both fields so \
                         Anthropic cannot auto-select (Anthropic has no native equivalent of \
                         OpenAI's \"none\")"
                    );
                }
                None
            }
            _ => Some(tc.clone()),
        },
        Value::Object(map) => match map.get("type").and_then(|v| v.as_str()) {
            Some("function") => {
                let name = map
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str());
                match name {
                    Some(n) => Some(serde_json::json!({"type": TOOL_CHOICE_TYPE_TOOL, "name": n})),
                    None => {
                        tracing::warn!(
                            "tool_choice with type=\"function\" but missing function.name; \
                             passed through as-is and Anthropic will reject it"
                        );
                        Some(tc.clone())
                    }
                }
            }
            Some("auto") | Some("any") | Some("tool") | Some("none") => Some(tc.clone()),
            _ => Some(tc.clone()),
        },
        _ => Some(tc.clone()),
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// Walk the canonical `ChatRequest` messages and apply two outgoing
/// replay invariants. `history_reasoning` gates ONLY the second
/// (unsigned-thinking strip); the tool_call_id reject is unconditional.
///
/// - Hard-reject (Err) any tool_result message (`Role::Tool`) that
///   lacks a `tool_call_id`. This runs REGARDLESS of `history_reasoning`
///   -- it is a separate correctness invariant, not part of the
///   thinking-strip. Anthropic 400s on such a body and the upstream
///   error doesn't name the bad message; surfacing it locally gives
///   operators a precise field to fix.
/// - STRIP any `Thinking` content block whose `signature` is missing or
///   empty from each message's `Parts` content -- UNLESS
///   `history_reasoning` is `Preserve`. Cross-provider fallback (a prior
///   turn handled by deepseek which signs with its own uuid format, then
///   the next turn falls back to Anthropic) and SDKs that fail to
///   round-trip the signature field would otherwise 400 real Anthropic
///   with a confusing upstream error. Strip drops just the offending
///   block; signed thinking blocks pass through unchanged and so does
///   every other block type.
///
///   `Preserve` skips the strip entirely: deepseek v4's `/anthropic`
///   endpoint (provider kind anthropic-api) emits unsigned thinking AND
///   400s the next turn unless that thinking is echoed back verbatim
///   (`The content[].thinking in the thinking mode must be passed back
///   to the API.`). `Auto` and the unset/None default both strip --
///   there is no dialect-default concept for this egress, so Auto means
///   strip, which is real-Anthropic-safe. Only explicit `Preserve`
///   changes behavior.
/// - When stripping leaves a message with no content blocks AND no
///   `reasoning_details` AND no `tool_calls`, drop the whole message.
///   Anthropic's wire spec rejects `content: []`; emitting the empty
///   message would just trade one 400 for another. The
///   `build_assistant_content` path still fills the wire content array
///   from `reasoning_details` / `tool_calls` when those are present,
///   so we keep the message in that case. Preserve never strips, so
///   this drop path does not run under Preserve.
///
/// One structured WARN fires per request when stripping occurs,
/// carrying the provider id, the count of dropped blocks, and the
/// affected message indices. Block content is never logged (could be
/// reasoning over sensitive data). Preserve strips nothing, so the WARN
/// does not fire under Preserve.
///
/// Returns `Cow::Borrowed(&req.messages)` on the no-strip path (Preserve,
/// or Strip/Auto with nothing to strip) so unmodified requests don't pay
/// a clone.
fn normalize_replay_invariants<'a>(
    id: &str,
    req: &'a ChatRequest,
    history_reasoning: CoreHistoryReasoning,
) -> Result<Cow<'a, [Message]>> {
    // Tool-result tool_call_id check stays a hard fail REGARDLESS of
    // history_reasoning -- it is a separate correctness invariant, not
    // part of the thinking-strip. Anthropic 400s a multi-turn body with
    // tool_use ids that lack matching tool_results.
    for (i, msg) in req.messages.iter().enumerate() {
        if matches!(msg.role, Role::Tool) && msg.tool_call_id.as_deref().unwrap_or("").is_empty() {
            return Err(Error::normalize_request(
                id,
                format!(
                    "messages[{i}] is a tool_result (Role::Tool) without tool_call_id; \
                     Anthropic requires the id of the tool_use this is answering",
                ),
            ));
        }
    }

    // Preserve: skip the unsigned-thinking strip and pass the messages
    // through unchanged. deepseek v4's `/anthropic` endpoint emits
    // unsigned thinking AND 400s the next turn unless it is echoed back
    // verbatim, so stripping would break every multi-turn replay. The
    // tool_call_id check above is validation-only (no mutation), so
    // Preserve can borrow; nothing is stripped, so no message-emptying
    // and no WARN.
    match history_reasoning {
        CoreHistoryReasoning::Preserve => {
            return Ok(Cow::Borrowed(&req.messages));
        }
        CoreHistoryReasoning::Auto | CoreHistoryReasoning::Strip => {}
    }

    // Strip / Auto pre-scan: do we need to strip anything? No -> return
    // Borrowed (no clone). Yes -> rebuild on the second pass.
    let needs_strip = req.messages.iter().any(message_has_unsigned_thinking);
    if !needs_strip {
        return Ok(Cow::Borrowed(&req.messages));
    }

    // Rebuild path: walk every message; for Parts, retain non-unsigned-
    // thinking blocks. Drop the message wholesale when stripping leaves
    // nothing the wire can serialize.
    let mut out: Vec<Message> = Vec::with_capacity(req.messages.len());
    let mut dropped_blocks: usize = 0;
    let mut affected_messages: Vec<usize> = Vec::new();
    for (i, msg) in req.messages.iter().enumerate() {
        let MessageContent::Parts(parts) = &msg.content else {
            // Text / Null content cannot carry a Thinking block.
            out.push(msg.clone());
            continue;
        };
        let original_len = parts.len();
        let kept: Vec<ContentPart> = parts
            .iter()
            .filter(|p| !is_unsigned_thinking_part(p))
            .cloned()
            .collect();
        let stripped_here = original_len.saturating_sub(kept.len());
        if stripped_here > 0 {
            dropped_blocks += stripped_here;
            affected_messages.push(i);
        }
        let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
        let has_reasoning = !msg.reasoning_details.is_empty();
        if kept.is_empty() && !has_tool_calls && !has_reasoning {
            // Stripping emptied this message and there's no other
            // content source. Anthropic's wire spec rejects
            // content: [] for both user and assistant roles; emit
            // nothing rather than trade one 400 for another.
            continue;
        }
        out.push(Message {
            role: msg.role.clone(),
            content: MessageContent::Parts(kept),
            reasoning: msg.reasoning.clone(),
            reasoning_details: msg.reasoning_details.clone(),
            name: msg.name.clone(),
            tool_call_id: msg.tool_call_id.clone(),
            tool_calls: msg.tool_calls.clone(),
        });
    }

    // One structured WARN per request. Block content stays OUT of the
    // log line (could be reasoning over sensitive data); only counts
    // and indices reach the operator. Provider id is always present
    // so an operator triaging a noisy upstream can grep by it.
    tracing::warn!(
        provider = id,
        dropped_blocks,
        affected_messages = ?affected_messages,
        "stripping unsigned thinking blocks from outgoing request: \
         Anthropic requires a signature on replayed Thinking blocks. \
         Cross-provider fallback or SDKs that fail to round-trip the \
         signature field would otherwise 400 the request. Routectl \
         drops just the unsigned blocks; signed thinking blocks and \
         other content pass through unchanged."
    );

    Ok(Cow::Owned(out))
}

/// True iff `p` is a `Thinking` block whose `signature` is missing
/// or empty. Pulled out so the pre-scan and the rebuild walk share a
/// single predicate.
fn is_unsigned_thinking_part(p: &ContentPart) -> bool {
    matches!(
        p,
        ContentPart::Known(KnownContentPart::Thinking { signature, .. })
            if signature.as_deref().unwrap_or("").is_empty()
    )
}

/// True iff any `Parts` content block on `msg` is an unsigned
/// `Thinking` block.
fn message_has_unsigned_thinking(msg: &Message) -> bool {
    if let MessageContent::Parts(parts) = &msg.content {
        parts.iter().any(is_unsigned_thinking_part)
    } else {
        false
    }
}

fn translate_content_part(p: &ContentPart) -> ContentBlock {
    match p {
        ContentPart::Known(k) => translate_known_part(k),
        ContentPart::Other {
            type_tag,
            cache_control,
            extras,
        } => ContentBlock::Other {
            type_tag: type_tag.clone(),
            cache_control: cache_control.clone(),
            extras: extras.clone(),
        },
    }
}

fn translate_known_part(k: &KnownContentPart) -> ContentBlock {
    match k {
        KnownContentPart::Text {
            text,
            cache_control,
        } => ContentBlock::Text {
            text: text.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::Image {
            source,
            cache_control,
        } => ContentBlock::Image {
            source: source.clone(),
            cache_control: cache_control.clone(),
        },
        // OpenAI-shape ImageUrl translates to an Anthropic image
        // block. Two URL shapes need different Anthropic source forms:
        //
        //   - HTTPS direct  ->  {type: "url", url: "..."}
        //   - data: URI     ->  {type: "base64", media_type: "...", data: "..."}
        //
        // Bedrock + Anthropic API both reject data: URIs in the URL
        // source form ("URL sources are not supported"); they require
        // the base64 source. OpenAI multimodal clients (claude-code's
        // OpenAI-compat fallback, vanilla OpenAI SDK, etc.) embed
        // images via `data:image/<fmt>;base64,<payload>`, so we parse
        // the data: prefix here and rewrite. Anything else
        // (https://, gs://, malformed) flows through as URL source --
        // upstream will surface a clean error if it isn't supported.
        KnownContentPart::ImageUrl {
            image_url,
            cache_control,
        } => {
            let url = image_url.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let source = parse_image_url_source(url);
            ContentBlock::Image {
                source,
                cache_control: cache_control.clone(),
            }
        }
        KnownContentPart::Document {
            source,
            title,
            citations,
            cache_control,
        } => ContentBlock::Document {
            source: source.clone(),
            title: title.clone(),
            citations: citations.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::ToolUse {
            id,
            name,
            input,
            cache_control,
        } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            cache_control: cache_control.clone(),
            is_error: *is_error,
        },
        KnownContentPart::Thinking {
            thinking,
            signature,
        } => ContentBlock::Thinking {
            thinking: thinking.clone(),
            // Wire requires signature; absent on canonical means we fall
            // back to empty. Multi-turn callers should always set this;
            // build_assistant_content errors when reasoning_details lack
            // a signature.
            signature: signature.clone().unwrap_or_default(),
            cache_control: None,
        },
        KnownContentPart::RedactedThinking { data } => ContentBlock::RedactedThinking {
            data: data.clone(),
            cache_control: None,
        },
    }
}

/// Reconstruct an Anthropic content array for an assistant message that
/// carries reasoning_details (tool-use continuity). thinking blocks with
/// signatures must be passed back verbatim.
fn build_assistant_content(id: &str, msg: &Message) -> Result<AnthropicContent> {
    let has_tool_calls = msg
        .tool_calls
        .as_ref()
        .map(|tc| !tc.is_empty())
        .unwrap_or(false);
    if msg.reasoning_details.is_empty() && !has_tool_calls {
        // No multi-turn reasoning to thread back AND no OpenAI-shape
        // tool_calls field to re-emit; fall through to the generic
        // content translation (Text or Parts), but strip trailing
        // text-after-tool_use first (see helper docstring).
        return Ok(translate_assistant_simple_content(&msg.content));
    }

    let mut blocks = emit_reasoning_blocks(id, &msg.reasoning_details)?;
    append_assistant_message_blocks(&mut blocks, &msg.content);
    if let Some(tool_calls) = msg.tool_calls.as_ref() {
        emit_tool_use_blocks_from_calls(id, tool_calls, &mut blocks)?;
    }
    Ok(AnthropicContent::Blocks(blocks))
}

/// Re-emit OpenAI-shape `tool_calls` (the canonical
/// representation produced by `walk_content_blocks` on the
/// response side) as Anthropic `ContentBlock::ToolUse` entries
/// for multi-turn replay. Without this, an OpenAI-ingress
/// request whose assistant history carries `tool_calls` -- or a
/// caller that echoes a canonical Message returned by routectl
/// straight back as a multi-turn turn -- would silently drop the
/// tool_use blocks, and the next user turn's `tool_result` would
/// fail upstream with "tool_use ids were found without
/// preceding tool_use blocks".
///
/// OpenAI shape: `{id, type: "function", function: {name, arguments}}`
/// where `arguments` is a JSON-encoded STRING. Anthropic shape:
/// `ContentBlock::ToolUse { id, name, input: Value }` where
/// `input` is the parsed JSON object. We attempt parsing first
/// and fall back to wrapping the raw string under
/// `{"_arguments": "..."}` so the upstream can return a useful
/// error rather than us silently producing a malformed body.
fn emit_tool_use_blocks_from_calls(
    id: &str,
    tool_calls: &[Value],
    blocks: &mut Vec<ContentBlock>,
) -> Result<()> {
    for call in tool_calls {
        let tool_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let arguments_raw = function
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let input = if arguments_raw.is_empty() {
            json!({})
        } else {
            serde_json::from_str(arguments_raw).unwrap_or_else(|e| {
                tracing::warn!(
                    provider = id,
                    tool_id = %tool_id,
                    error = %e,
                    "tool_call.arguments not valid JSON; wrapping under _arguments for upstream",
                );
                json!({ "_arguments": arguments_raw })
            })
        };
        blocks.push(ContentBlock::ToolUse {
            id: tool_id,
            name,
            input,
            cache_control: None,
        });
    }
    Ok(())
}

/// Translate `reasoning_details` into Anthropic `Thinking` /
/// `RedactedThinking` blocks for echo on a multi-turn assistant turn.
/// Index-ordered so an upstream that re-orders reasoning blocks
/// doesn't surprise the downstream signature check. Anthropic rejects
/// a `Thinking` block on echo without the `signature` field; when a
/// detail's signature is missing or empty (Anthropic 4.5 occasionally
/// omits `signature_delta` on tool-only thinking turns), the detail
/// is logged at WARN and skipped so replay doesn't 400 on a
/// guaranteed-malformed echo. WARN level (not DEBUG) so operators
/// see the partial echo and can correlate with upstream cache misses
/// or quality drift -- mixed signed/unsigned histories lose ordering
/// fidelity. See CLAUDE.md "Anthropic streaming reasoning replay".
fn emit_reasoning_blocks(id: &str, details: &[ReasoningDetail]) -> Result<Vec<ContentBlock>> {
    let mut sorted = details.to_vec();
    sorted.sort_by_key(|d| d.index.unwrap_or(0));

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(sorted.len());
    let mut skipped_unsigned: Vec<Option<u32>> = Vec::new();
    for detail in &sorted {
        match detail.kind {
            ReasoningDetailKind::Text => {
                if detail.format.as_deref() != Some(super::ANTHROPIC_FORMAT) {
                    continue;
                }
                let thinking = detail
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let signature = detail
                    .payload
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if signature.is_empty() {
                    // Anthropic 400s on a Thinking block without a
                    // signature; skipping is better than a hard fail.
                    // Aggregate the WARN per-call (Claude 4.5 multi-
                    // block thinking turns can pile up several skipped
                    // entries and per-detail WARN would flood the log).
                    skipped_unsigned.push(detail.index);
                    continue;
                }
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature: signature.to_string(),
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Encrypted => {
                if detail.format.as_deref() != Some(super::ANTHROPIC_FORMAT) {
                    continue;
                }
                let data = detail
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ContentBlock::RedactedThinking {
                    data,
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Summary => {
                // Not an Anthropic block; skip.
            }
        }
    }
    if !skipped_unsigned.is_empty() {
        tracing::warn!(
            provider = id,
            skipped_count = skipped_unsigned.len(),
            skipped_indices = ?skipped_unsigned,
            "skipping Thinking blocks on replay: signature missing or empty \
             (multi-block thinking history is now partially echoed; \
             see CLAUDE.md \"Anthropic streaming reasoning replay\" residual)"
        );
    }
    Ok(blocks)
}

/// Append the assistant message's text/parts content AFTER the
/// reasoning blocks already pushed. For Text, emits a single Text
/// block (skipped on empty/Null since reasoning-only assistant turns
/// are valid). For Parts, translates each block (after stripping
/// trailing text-after-tool_use, which both Bedrock and Anthropic
/// reject with "tool_use ids were found without tool_result blocks
/// immediately after").
fn append_assistant_message_blocks(blocks: &mut Vec<ContentBlock>, content: &MessageContent) {
    match content {
        MessageContent::Text(t) if !t.is_empty() => blocks.push(ContentBlock::Text {
            text: t.clone(),
            cache_control: None,
        }),
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            for p in cleaned.iter() {
                blocks.push(translate_content_part(p));
            }
        }
    }
}

/// Assistant-message variant of `translate_simple_content` that strips
/// trailing text-after-tool_use before per-part translation. Called
/// only from `build_assistant_content`. Text/Null arms delegate to
/// `translate_simple_content` so the two stay in lockstep -- only the
/// `Parts` arm needs the strip.
fn translate_assistant_simple_content(c: &MessageContent) -> AnthropicContent {
    match c {
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            AnthropicContent::Blocks(cleaned.iter().map(translate_content_part).collect())
        }
        // Text/Null arms are identical to `translate_simple_content`;
        // delegate to keep them in one place.
        _ => translate_simple_content(c),
    }
}

/// Translate plain message content (no multi-turn reasoning context).
/// Text -> AnthropicContent::Text (cheaper wire form). Parts ->
/// AnthropicContent::Blocks via per-part translation.
fn translate_simple_content(c: &MessageContent) -> AnthropicContent {
    match c {
        MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
        MessageContent::Null => AnthropicContent::Text(String::new()),
        MessageContent::Parts(parts) => {
            AnthropicContent::Blocks(parts.iter().map(translate_content_part).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Tool-role messages
// ---------------------------------------------------------------------------

fn build_tool_message(msg: &Message) -> AnthropicMessage {
    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
    // Anthropic tool_result.content accepts either a string or an array
    // of content blocks. We honor whichever shape the canonical message
    // carries.
    let content_val = match &msg.content {
        MessageContent::Text(t) => Value::String(t.clone()),
        MessageContent::Parts(parts) => Value::Array(
            parts
                .iter()
                .map(|p| serde_json::to_value(translate_content_part(p)).unwrap_or(Value::Null))
                .collect(),
        ),
        MessageContent::Null => Value::Null,
    };
    AnthropicMessage {
        role: AnthropicRole::User,
        content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id,
            content: content_val,
            cache_control: None,
            is_error: None,
        }]),
    }
}

// ---------------------------------------------------------------------------
// cache_control validation
// ---------------------------------------------------------------------------

/// Walk all positions of an AnthropicRequest and call
/// `cache_control::validate` against the collected breakpoint sequence.
/// Catches 1h-after-5m ordering violations and 5+ breakpoint counts
/// before they reach upstream.
fn validate_breakpoints(ar: &AnthropicRequest) -> Result<()> {
    let mut bps: Vec<Breakpoint<'_>> = Vec::new();

    // Owned cache_control values pulled out of `AnthropicTool::Builtin`'s
    // raw JSON. Lives here so the Breakpoint slice below can reference
    // them without lifetime issues. Indexed by position in `ar.tools`.
    let builtin_tool_ccs: Vec<Option<routectl_core::CacheControl>> = ar
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| match t {
                    AnthropicTool::Builtin(v) => v
                        .as_object()
                        .and_then(|o| o.get("cache_control"))
                        .and_then(|cc| {
                            serde_json::from_value::<routectl_core::CacheControl>(cc.clone()).ok()
                        }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Tools come first in the cache prefix.
    if let Some(tools) = &ar.tools {
        for (i, t) in tools.iter().enumerate() {
            if let Some(cc) = anthropic_tool_cache_control(t) {
                bps.push(Breakpoint {
                    position: BreakpointPosition::Tools,
                    control: cc,
                });
            } else if let Some(cc) = builtin_tool_ccs.get(i).and_then(|o| o.as_ref()) {
                bps.push(Breakpoint {
                    position: BreakpointPosition::Tools,
                    control: cc,
                });
            }
        }
    }

    // Then system blocks.
    if let Some(AnthropicSystem::Blocks(blocks)) = &ar.system {
        for b in blocks {
            if let Some(cc) = b.cache_control.as_ref() {
                bps.push(Breakpoint {
                    position: BreakpointPosition::System,
                    control: cc,
                });
            }
        }
    }

    // Then messages.
    for m in &ar.messages {
        if let AnthropicContent::Blocks(blocks) = &m.content {
            for b in blocks {
                if let Some(cc) = content_block_cache_control(b) {
                    bps.push(Breakpoint {
                        position: BreakpointPosition::Messages,
                        control: cc,
                    });
                }
            }
        }
    }

    // Top-level auto-cache marker.
    if let Some(cc) = ar.cache_control.as_ref() {
        bps.push(Breakpoint {
            position: BreakpointPosition::TopLevel,
            control: cc,
        });
    }

    cache_control::validate(&bps)
}

fn content_block_cache_control(b: &ContentBlock) -> Option<&routectl_core::CacheControl> {
    match b {
        ContentBlock::Text { cache_control, .. }
        | ContentBlock::Image { cache_control, .. }
        | ContentBlock::Document { cache_control, .. }
        | ContentBlock::Thinking { cache_control, .. }
        | ContentBlock::RedactedThinking { cache_control, .. }
        | ContentBlock::ToolUse { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. }
        | ContentBlock::Other { cache_control, .. } => cache_control.as_ref(),
    }
}

fn anthropic_tool_cache_control(t: &AnthropicTool) -> Option<&routectl_core::CacheControl> {
    match t {
        AnthropicTool::Custom { cache_control, .. } => cache_control.as_ref(),
        AnthropicTool::Builtin(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Top-level normalize
// ---------------------------------------------------------------------------

/// Filter `req.anthropic_beta` against the operator-supplied
/// `allowed_betas` list. Empty allowlist = pass-through (default).
/// Otherwise, drop entries not in the list at DEBUG so operators
/// triaging unexpected behavior can see WHICH flags got removed.
/// Mirrors the Bedrock-egress `filter_bedrock_betas` shape.
pub(crate) fn filter_anthropic_betas<'a>(
    provider_id: &str,
    requested: &'a [String],
    allowed: &[String],
) -> Cow<'a, [String]> {
    if allowed.is_empty() {
        return Cow::Borrowed(requested);
    }
    let mut kept = Vec::with_capacity(requested.len());
    for flag in requested {
        if allowed.iter().any(|a| a == flag) {
            kept.push(flag.clone());
        } else {
            tracing::debug!(
                provider = provider_id,
                flag = %routectl_core::sanitize_for_log(flag),
                "dropping beta flag not in operator-supplied [providers.X] allowed_betas"
            );
        }
    }
    Cow::Owned(kept)
}

pub(crate) fn normalize(
    id: &str,
    req: &ChatRequest,
    adaptive: bool,
    allowed_betas: &[String],
    context_management: bool,
    thinking_cache: Option<
        &std::sync::RwLock<crate::anthropic_api::context_management::ThinkingCache>,
    >,
) -> Result<Value> {
    // Anthropic's wire requires every tool_result carry the
    // `tool_use_id` of the tool_use it answers; missing ids are
    // rejected upfront (always, independent of history_reasoning).
    //
    // Thinking blocks must carry a `signature` for multi-turn replay on
    // real Anthropic. Cross-provider fallback (e.g. deepseek ->
    // Anthropic) and SDKs that don't round-trip the signature field can
    // produce unsigned blocks, so by default routectl STRIPS them and
    // forwards a body Anthropic accepts rather than 400ing the request.
    //
    // The strip is gated on `history_reasoning`: `Preserve` keeps
    // unsigned thinking on the wire because deepseek v4's `/anthropic`
    // endpoint emits unsigned thinking AND 400s the next turn unless it
    // is echoed back verbatim. `Auto` (the unset/None default) and
    // `Strip` both strip -- real-Anthropic-safe. The dispatch layer
    // resolves the per-model policy onto `routectl_internal`; library
    // callers that never set it get `Auto` = strip.
    let hr = req
        .routectl_internal
        .history_reasoning
        .unwrap_or(CoreHistoryReasoning::Auto);
    let messages = normalize_replay_invariants(id, req, hr)?;

    let max_tokens = resolve_max_tokens(req);
    let thinking = build_thinking(req, adaptive);
    let output_config = build_output_config(req, &thinking);

    // Prefer canonical req.system; fall back to lifting Role::System
    // messages for direct callers that bypass an ingress.
    let system = req
        .system
        .as_ref()
        .map(translate_system)
        .or_else(|| lift_legacy_system(&req.messages));

    let mut anthropic_messages = translate_messages(id, &messages)?;

    // When context_management emulation is active, re-inject cached
    // thinking blocks before ToolUse blocks per the clear_thinking_20251015
    // edit spec. Collect any cache-miss ids for soft-fail below.
    let clear_thinking_misses: Vec<String> = if context_management {
        if let Some(tc) = thinking_cache {
            let apply_result = crate::anthropic_api::context_management::apply_clear_thinking_edit(
                &mut anthropic_messages,
                req.provider_extras.as_ref(),
                tc,
                id,
            );
            apply_result.missed_tool_ids
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // tool_choice="none" forbids tool use; Anthropic has no native
    // equivalent for the bare-string OpenAI form, so strip BOTH the
    // field and the tools list. The Anthropic-shape `{"type":"none"}`
    // object form passes through above and Anthropic suppresses tool
    // use server-side, so it doesn't need the extra strip.
    let suppress_tools = matches!(
        req.tool_choice.as_ref(),
        Some(Value::String(s)) if s == "none"
    );
    let has_tools = req.tools.as_ref().is_some_and(|t| !t.is_empty());
    let tools = if suppress_tools {
        None
    } else {
        req.tools
            .as_ref()
            .map(|ts| ts.iter().map(translate_tool).collect::<Vec<_>>())
    };

    // Anthropic requires temperature=1.0 when thinking is enabled
    // (legacy and adaptive both): no alternative-continuation sampling
    // while spending reasoning budget.
    let temperature = match &thinking {
        Some(ThinkingConfig::Enabled { .. }) | Some(ThinkingConfig::Adaptive) => Some(1.0f64),
        _ => req.temperature,
    };

    let ar = AnthropicRequest {
        model: req.model.clone(),
        messages: anthropic_messages,
        max_tokens,
        system,
        thinking,
        output_config,
        temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.clone(),
        stream: None, // caller sets this
        tools,
        tool_choice: translate_tool_choice(req.tool_choice.as_ref(), has_tools),
        cache_control: req.cache_control.clone(),
        anthropic_beta: filter_anthropic_betas(id, &req.anthropic_beta, allowed_betas).into_owned(),
    };

    // Belt-and-braces: validate in release too. The Anthropic ingress
    // already runs this at parse time; running it again here catches
    // direct callers (library users without an ingress) and protects
    // upstream from cap/ordering violations regardless of build mode.
    validate_breakpoints(&ar)?;

    let mut body =
        serde_json::to_value(&ar).map_err(|e| Error::normalize_request(id, e.to_string()))?;

    merge_provider_extras(id, &mut body, req.provider_extras.as_ref());

    // When context_management emulation is active we have already applied
    // the edits above. Strip the `context_management` body key so it is
    // never forwarded to the upstream (non-Anthropic providers reject it).
    if context_management {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("context_management");
        }
    }

    // Soft-fail: if cache misses occurred (cold-start or TTL eviction) and
    // the body still has a `thinking` key, the upstream would receive a
    // request that demands thinking tokens but no thinking blocks were
    // injected into history. Non-Anthropic providers 400 on this shape.
    // Strip `thinking` defensively and emit a structured warning so
    // operators can diagnose the gap.
    if !clear_thinking_misses.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            if obj.contains_key("thinking") {
                obj.remove("thinking");
                tracing::warn!(
                    provider = id,
                    missed_tool_ids = ?clear_thinking_misses,
                    "context_management: cache miss for tool_use ids; \
                     stripped `thinking` from body to avoid upstream 400 \
                     (cold-start or TTL eviction)"
                );
            }
        }
    }
    reconcile_output_config_effort(&mut body, adaptive, &req.routectl_internal.effort_levels);
    strip_thinking_when_tool_choice_forces_use(id, &mut body);
    Ok(body)
}

/// Iterate the canonical messages and produce the Anthropic-shaped
/// per-role list. System messages are intentionally dropped here --
/// they're already lifted into `req.system` (canonical) or by
/// `lift_legacy_system` for direct callers without an ingress, so
/// re-emitting them as messages would duplicate.
fn translate_messages(id: &str, messages: &[Message]) -> Result<Vec<AnthropicMessage>> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::System => {
                // Already handled via req.system / lift_legacy_system.
                // Drop here (do not duplicate in the messages array).
            }
            Role::User => out.push(AnthropicMessage {
                role: AnthropicRole::User,
                content: translate_simple_content(&msg.content),
            }),
            Role::Assistant => out.push(AnthropicMessage {
                role: AnthropicRole::Assistant,
                content: build_assistant_content(id, msg)?,
            }),
            Role::Tool => out.push(build_tool_message(msg)),
        }
    }
    Ok(out)
}

/// Merge `provider_extras` into the assembled body. Caller-supplied
/// keys win EXCEPT for routectl-managed top-level keys (see
/// `is_routectl_managed_key`); those are dropped so a malicious or
/// careless `provider_extras = {"messages": [...]}` can't replace the
/// assembled messages array. This was an architecture-review
/// finding (MEDIUM-1).
///
/// Source: this helper only ever sees `req.provider_extras` -- the
/// Anthropic ingress's forward-compat sweep destination. Drops here
/// are by design (the swept key would conflict with a key routectl
/// builds itself, e.g. `thinking`) and were flooding
/// `routectl-warn.log` on every claude-code request. The drop log
/// fires at DEBUG with neutral phrasing. If a future caller wires a
/// new source (operator-config `default_extras` on this egress) the
/// log level should branch on the source the way the openai-compat
/// `merge_extras` does.
fn merge_provider_extras(id: &str, body: &mut Value, extras: Option<&Value>) {
    let Some(extras) = extras else { return };
    let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) else {
        return;
    };
    for (k, v) in extra_obj {
        if is_routectl_managed_key(k) {
            tracing::debug!(
                provider = id,
                key = %k,
                "forward-compat extra would override routectl-managed key; dropped"
            );
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// Top-level Anthropic body keys that routectl owns. Delegates to the
/// shared canonical list and adds Anthropic-API-specific keys that are
/// not on `ChatRequest` but are written by this egress from canonical
/// fields:
/// - `thinking`  -- translated from `req.reasoning` by `build_thinking`;
///   not a raw ChatRequest field but routectl writes it.
///
/// `output_config` is intentionally NOT here -- the full object is
/// allowed through `provider_extras` for legitimate sub-fields like
/// `output_config.format` (structured-output). The `output_config.effort`
/// sub-field is reconciled post-merge in `reconcile_output_config_effort`
/// when the model does not have `supports_adaptive_thinking=true` (Haiku,
/// Sonnet -- Anthropic 400s on `effort` for them).
fn is_routectl_managed_key(key: &str) -> bool {
    is_canonical_request_key(key)
        || matches!(
            key,
            // Anthropic-API-specific managed keys not on ChatRequest:
            // `thinking` is built from req.reasoning by this egress.
            "thinking"
        )
}

/// Post-merge sub-key reconcile for `output_config.effort`.
///
/// Non-adaptive branch: when the model does NOT support adaptive
/// thinking (`supports_adaptive_thinking=false`), remove
/// `body.output_config.effort` so non-Opus models (Sonnet 4.5,
/// Haiku 4.5) don't 400 with `This model does not support the effort
/// parameter.` cc emits `output_config: {effort: "high"}` on every
/// request regardless of the routed model, and the forward-compat
/// sweep through `provider_extras` puts it back in the outgoing body
/// verbatim. This is the symmetric counterpart to
/// `build_output_config`, which only emits the effort sub-key when
/// adaptive is on.
///
/// Adaptive branch: re-clamp `body.output_config.effort` against the
/// operator's `effort_levels` cap. `derive_effort` already clamped on
/// the typed pre-merge struct, but `merge_provider_extras` may have
/// overwritten the clamped value with a raw caller-supplied
/// `output_config.effort` (claude-code 2.1.153+ sends
/// `output_config: {effort: "max"}` on every request, and the ingress
/// preserves the whole `output_config` object verbatim in
/// `provider_extras` so orthogonal sub-keys like
/// `output_config.format` pass through). Without this re-clamp the
/// operator's cost cap is silently bypassed. Empty `effort_levels`
/// means intentional pass-through and skips the re-clamp.
///
/// `output_config.format` (structured-output) and other sibling sub-
/// fields are preserved on both branches -- they're orthogonal to the
/// effort beta and supported across the model family. On the non-
/// adaptive branch, if `effort` was the only sub-key, the now-empty
/// `output_config` object is also removed so the wire body stays
/// clean.
fn reconcile_output_config_effort(body: &mut Value, adaptive: bool, effort_levels: &[String]) {
    if adaptive {
        if effort_levels.is_empty() {
            return;
        }
        let Some(obj) = body.as_object_mut() else {
            return;
        };
        let Some(oc) = obj.get_mut("output_config").and_then(|v| v.as_object_mut()) else {
            return;
        };
        let Some(effort_val) = oc.get_mut("effort") else {
            return;
        };
        let Some(current) = effort_val.as_str() else {
            return;
        };
        let clamped = clamp_effort_to_supported(current, effort_levels);
        if clamped.as_ref() != current {
            *effort_val = Value::String(clamped.into_owned());
        }
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(oc) = obj.get_mut("output_config").and_then(|v| v.as_object_mut()) else {
        return;
    };
    if oc.remove("effort").is_some() && oc.is_empty() {
        obj.remove("output_config");
    }
}

/// Anthropic's extended-thinking docs forbid `thinking` paired with
/// `tool_choice` values that force tool use. The Messages API 400s with
/// "Thinking may not be enabled when tool_choice forces tool use." when
/// the body carries `thinking` AND `tool_choice.type` is `"any"` or
/// `"tool"`. The constraint is identical for adaptive thinking
/// (`{"type":"adaptive"}`) per the adaptive-thinking docs page.
///
/// Real-world trigger: Claude Code's WebSearch tool sub-request fires
/// `tool_choice: {type:"tool", name:"web_search"}` in tandem with
/// `thinking: {type:"adaptive"}` (when the operator config sets
/// `effort: "max"`). routectl emits both because each was set
/// independently by separate concerns -- the tool_choice translator and
/// the thinking composer don't talk to each other.
///
/// Strip `thinking` (not `tool_choice`) so the caller's intent to force
/// the named tool is preserved; the request still completes
/// successfully, just without thinking. `auto`, `none`, and absent
/// `tool_choice` do not trigger the strip.
///
/// Runs after `merge_provider_extras` so the check operates on the
/// final wire body, regardless of whether `thinking` was composed by
/// `build_thinking` or layered in by some future provider-extras path
/// that bypasses `is_routectl_managed_key`.
fn strip_thinking_when_tool_choice_forces_use(provider_id: &str, body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if !obj.contains_key("thinking") {
        return;
    }
    // The `.filter()` short-circuits on non-forcing values (`auto` /
    // `none` / unknown), so the owned `String` is allocated only on the
    // strip path. The Option also lets the immutable borrow on `obj` end
    // at the semicolon -- `obj.remove` below takes the mutable borrow
    // without conflict.
    let ttype = obj
        .get("tool_choice")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str())
        .filter(|&t| t == TOOL_CHOICE_TYPE_ANY || t == TOOL_CHOICE_TYPE_TOOL)
        .map(str::to_string);
    let Some(ttype) = ttype else {
        return;
    };
    obj.remove("thinking");
    tracing::debug!(
        provider = provider_id,
        tool_choice_type = %ttype,
        "stripped thinking from outgoing body: tool_choice forces tool use; \
         Anthropic forbids the combo"
    );
}

#[cfg(test)]
mod allowlist_tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, Role};
    use serde_json::json;

    fn req_with_betas(betas: Vec<String>) -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            anthropic_beta: betas,
            ..Default::default()
        }
    }

    /// Pin: empty allowlist = pass-through. Default behavior, no
    /// operator surprise on upgrade.
    #[test]
    fn empty_allowlist_passes_all_betas() {
        let req = req_with_betas(vec![
            "context-1m-2025-08-07".into(),
            "prompt-caching-2024-07-31".into(),
        ]);
        let body = normalize("p", &req, false, &[], false, None).unwrap();
        assert_eq!(
            body["anthropic_beta"],
            json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
        );
    }

    /// Pin: non-empty allowlist drops entries not in the list.
    #[test]
    fn non_empty_allowlist_drops_unknown() {
        let req = req_with_betas(vec![
            "context-1m-2025-08-07".into(),
            "secret-experimental-flag".into(),
            "prompt-caching-2024-07-31".into(),
        ]);
        let allowed = vec![
            "context-1m-2025-08-07".to_string(),
            "prompt-caching-2024-07-31".to_string(),
        ];
        let body = normalize("p", &req, false, &allowed, false, None).unwrap();
        // Order preserved, unknown flag dropped.
        assert_eq!(
            body["anthropic_beta"],
            json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
        );
    }

    /// Pin: every requested beta is rejected when none are on the
    /// allowlist. The wire field is either absent or an empty array;
    /// both mean "no betas reach upstream" and either serialization
    /// is acceptable.
    #[test]
    fn allowlist_can_drop_all_requested() {
        let req = req_with_betas(vec!["totally-unknown".into()]);
        let allowed = vec!["context-1m-2025-08-07".to_string()];
        let body = normalize("p", &req, false, &allowed, false, None).unwrap();
        let got = &body["anthropic_beta"];
        assert!(
            got.is_null() || got.as_array().map(|a| a.is_empty()).unwrap_or(false),
            "expected absent or empty array, got: {got}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests for context_management emulation in normalize()
// ---------------------------------------------------------------------------

#[cfg(test)]
mod context_management_normalize_tests {
    use super::*;
    use crate::anthropic_api::context_management::{
        snapshot_to_cache, ThinkingCache, CLEAR_THINKING_EDIT_TYPE,
    };
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
    use serde_json::json;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, RwLock};

    fn simple_req() -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        }
    }

    fn req_with_cm_extras() -> ChatRequest {
        ChatRequest {
            provider_extras: Some(json!({
                "context_management": {
                    "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
                }
            })),
            ..simple_req()
        }
    }

    fn req_with_tool_use_history_and_cm() -> ChatRequest {
        // Build a request whose messages contain an assistant turn with
        // tool_calls so translate_messages produces an AnthropicMessage
        // with a ToolUse block -- required for apply_clear_thinking_edit
        // to find qualifying messages.
        ChatRequest {
            model: "claude-sonnet-4".into(),
            max_tokens: Some(4096),
            reasoning: Some(ReasoningConfig {
                enabled: Some(true),
                max_tokens: Some(2048),
                effort: None,
                exclude: None,
            }),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("use the calc tool".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("calling calc".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![json!({
                        "id": "toolu_t1",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{}"}
                    })]),
                },
                Message {
                    role: Role::Tool,
                    content: MessageContent::Text("42".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: Some("toolu_t1".into()),
                    tool_calls: None,
                },
            ],
            provider_extras: Some(json!({
                "context_management": {
                    "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
                }
            })),
            ..Default::default()
        }
    }

    fn small_cache(cap: usize) -> RwLock<ThinkingCache> {
        RwLock::new(lru::LruCache::new(NonZeroUsize::new(cap).expect("cap > 0")))
    }

    /// When context_management=true, the `context_management` body key
    /// (which came from provider_extras) must be stripped before returning.
    /// Non-Anthropic upstreams reject unknown top-level body keys.
    #[test]
    fn normalize_strips_context_management_body_key_when_flag_true() {
        let req = req_with_cm_extras();
        let body = normalize("test", &req, false, &[], true, None).expect("normalize must succeed");
        assert!(
            body.get("context_management").is_none(),
            "context_management body key must be stripped when flag=true; got: {body}"
        );
    }

    /// When context_management=false, the `context_management` body key
    /// must be forwarded verbatim to the upstream (e.g. real Anthropic).
    #[test]
    fn normalize_keeps_context_management_body_key_when_flag_false() {
        let req = req_with_cm_extras();
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        assert!(
            body.get("context_management").is_some(),
            "context_management body key must survive when flag=false; got: {body}"
        );
    }

    /// Soft-fail: when context_management=true and the thinking cache has
    /// no entry for the qualifying tool_use id (cold-start or TTL eviction),
    /// the `thinking` key must be stripped from the outgoing body so the
    /// upstream (which does not honour the beta) does not 400.
    #[test]
    fn normalize_soft_fail_strips_thinking_on_cache_miss() {
        let req = req_with_tool_use_history_and_cm();
        let cache = Arc::new(small_cache(8)); // nothing seeded
        let body = normalize("test", &req, false, &[], true, Some(&cache))
            .expect("normalize must succeed even on cache miss");
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped on cache miss; got: {body}"
        );
    }

    /// No soft-fail when the cache has an entry for the qualifying
    /// tool_use id: the `thinking` key must remain in the outgoing body.
    #[test]
    fn normalize_no_soft_fail_when_cache_hits() {
        let req = req_with_tool_use_history_and_cm();
        let cache = Arc::new(small_cache(8));
        // Seed the cache for the tool_use id used in req_with_tool_use_history_and_cm.
        snapshot_to_cache(
            &cache,
            "test",
            "toolu_t1",
            vec![routectl_core::ReasoningDetail {
                kind: routectl_core::ReasoningDetailKind::Text,
                id: Some("rd-1".into()),
                format: Some(super::super::ANTHROPIC_FORMAT.to_string()),
                index: Some(0),
                payload: json!({"text": "my reasoning", "signature": "sig"}),
            }],
            super::super::context_management::DEFAULT_MAX_THINKING_ENTRY_BYTES,
            super::super::context_management::THINKING_CACHE_TTL,
            "test",
        );
        let body = normalize("test", &req, false, &[], true, Some(&cache))
            .expect("normalize must succeed with cache hit");
        assert!(
            body.get("thinking").is_some(),
            "thinking must NOT be stripped when cache has an entry; got: {body}"
        );
    }
}

#[cfg(test)]
mod multi_turn_tool_use_tests {
    use super::*;
    use routectl_core::{ChatRequest, CoreHistoryReasoning, Message, Role};
    use serde_json::json;

    /// Minimal in-process tracing capture used by
    /// `emits_warn_when_stripping_occurs` to assert structured fields
    /// without taking on a `tracing-test` dev-dependency. Scoped via
    /// `tracing::subscriber::with_default` so concurrent unit tests do
    /// not leak captured state across threads.
    mod test_capture {
        // TODO(consolidation): this is the third copy of the same in-process
        // tracing-capture pattern. The other two live at:
        //   - crates/routectl-cli/tests/anthropic_forward_compat_stream.rs
        //     (lines 175-269): async with_capture for #[tokio::test].
        //   - crates/routectl-core/tests/common/mod.rs:
        //     synchronous capture_events with a TRACE level hint.
        // Next person to touch any of these three: extract a shared helper
        // (likely in routectl-core/tests/common/) that supports both sync
        // and async closures plus an opt-in TRACE level hint, then collapse
        // the copies. Keeping the inline copy for now because each consumer
        // wants a slightly different shape and full extraction is a larger
        // refactor than the original strip-instead-of-reject change.
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};

        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub struct CapturedEvent {
            pub level: tracing::Level,
            pub target: String,
            pub message: String,
            pub fields: Vec<(String, String)>,
        }

        #[derive(Default)]
        struct Collector {
            message: String,
            fields: Vec<(String, String)>,
        }

        impl Visit for Collector {
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_string();
                } else {
                    self.fields.push((field.name().into(), value.into()));
                }
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let s = format!("{value:?}");
                if field.name() == "message" {
                    self.message = s.trim_matches('"').to_string();
                } else {
                    self.fields.push((field.name().into(), s));
                }
            }
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.fields.push((field.name().into(), value.to_string()));
            }
            fn record_i64(&mut self, field: &Field, value: i64) {
                self.fields.push((field.name().into(), value.to_string()));
            }
            fn record_bool(&mut self, field: &Field, value: bool) {
                self.fields.push((field.name().into(), value.to_string()));
            }
        }

        struct CaptureSubscriber {
            captured: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl tracing::Subscriber for CaptureSubscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let meta = event.metadata();
                let mut visitor = Collector::default();
                event.record(&mut visitor);
                let captured_event = CapturedEvent {
                    level: *meta.level(),
                    target: meta.target().to_string(),
                    message: visitor.message,
                    fields: visitor.fields,
                };
                if let Ok(mut guard) = self.captured.lock() {
                    guard.push(captured_event);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        /// Run `f` with the capture subscriber installed as the
        /// thread-local default. Returns the captured events.
        pub fn with_capture<F: FnOnce()>(f: F) -> Vec<CapturedEvent> {
            let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                captured: captured.clone(),
            };
            let _guard = tracing::subscriber::set_default(subscriber);
            f();
            let events = captured.lock().expect("capture lock poisoned").clone();
            events
        }
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_msg(text: &str, tool_calls: Option<Vec<Value>>) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    /// On a multi-turn assistant turn, `Message.tool_calls` (the
    /// canonical OpenAI-shape representation produced by
    /// `walk_content_blocks` on the response side) must be re-emitted
    /// as Anthropic `ContentBlock::ToolUse` entries. Without this,
    /// echoing a canonical Message back through the Anthropic egress
    /// drops the tool_use blocks and the next user `tool_result` turn
    /// fails upstream with "tool_use ids were found without preceding
    /// tool_use blocks".
    #[test]
    fn assistant_message_with_tool_calls_emits_tool_use_blocks() {
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("calculate 2+2"),
                assistant_msg(
                    "Let me calculate.",
                    Some(vec![json!({
                        "id": "toolu_abc123",
                        "type": "function",
                        "function": {
                            "name": "calc",
                            "arguments": "{\"expr\":\"2+2\"}",
                        }
                    })]),
                ),
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message must be present");
        let blocks = assistant
            .get("content")
            .and_then(|v| v.as_array())
            .expect("assistant content must be Blocks form when tool_calls present");

        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .expect("assistant must carry a tool_use block on multi-turn replay");
        assert_eq!(tool_use["id"], "toolu_abc123");
        assert_eq!(tool_use["name"], "calc");
        assert_eq!(tool_use["input"], json!({"expr": "2+2"}));
    }

    #[test]
    fn strips_unsigned_thinking_block_keeps_other_blocks() {
        // Multi-turn input with [text, signed_thinking, unsigned_thinking,
        // tool_use] -> outgoing assistant content has [text,
        // signed_thinking, tool_use]. The unsigned block is dropped;
        // every other content part survives unmodified.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("compute 2+2"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "Let me think.".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "signed analysis".into(),
                            signature: Some("sig_abc".into()),
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "unsigned analysis".into(),
                            signature: None,
                        }),
                        ContentPart::Known(KnownContentPart::ToolUse {
                            id: "toolu_1".into(),
                            name: "calc".into(),
                            input: json!({"expr": "2+2"}),
                            cache_control: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message present");
        let blocks = assistant
            .get("content")
            .and_then(|v| v.as_array())
            .expect("assistant content is Blocks form");

        let types: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            types,
            vec!["text", "thinking", "tool_use"],
            "expected unsigned thinking dropped, others preserved; got {types:?}"
        );

        // The signed thinking block survives with its signature intact.
        let signed = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
            .unwrap();
        assert_eq!(signed["signature"], "sig_abc");

        // Other survivors keep their fields.
        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .unwrap();
        assert_eq!(tool_use["id"], "toolu_1");
        assert_eq!(tool_use["name"], "calc");
    }

    #[test]
    fn passes_through_when_all_thinking_signed() {
        // No mutation when every thinking block carries a signature.
        // Pin: signed-only histories must produce the same body the
        // pre-strip code produced.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "first".into(),
                            signature: Some("sig_one".into()),
                        }),
                        ContentPart::Known(KnownContentPart::Text {
                            text: "answer".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "second".into(),
                            signature: Some("sig_two".into()),
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let assistant = body
            .get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            })
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let types: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            types,
            vec!["thinking", "text", "thinking"],
            "all blocks pass through unchanged when every thinking is signed"
        );
        assert_eq!(blocks[0]["signature"], "sig_one");
        assert_eq!(blocks[2]["signature"], "sig_two");
    }

    #[test]
    fn drops_assistant_message_when_only_block_was_unsigned_thinking() {
        // When stripping leaves the assistant message with content: []
        // AND the message has no reasoning_details / tool_calls to fill
        // the wire content array, drop the whole message. Anthropic's
        // wire spec rejects content: []; emitting it would just trade
        // one 400 for another.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hello"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![ContentPart::Known(
                        KnownContentPart::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        },
                    )]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                user_msg("any update?"),
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        // The empty-after-strip assistant message is gone; only the
        // two user messages remain.
        assert_eq!(
            messages.len(),
            2,
            "empty-after-strip assistant message must be dropped, got: {messages:?}"
        );
        let assistant_present = messages
            .iter()
            .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"));
        assert!(
            !assistant_present,
            "no assistant message must remain when its only block was an unsigned thinking, \
             got: {messages:?}"
        );
    }

    #[test]
    fn keeps_message_with_only_unsigned_thinking_when_tool_calls_present() {
        // Pin the corner: stripping leaves Parts empty BUT the message
        // carries tool_calls. The wire content array still gets blocks
        // from `emit_tool_use_blocks_from_calls`, so the message must
        // be kept (don't drop the tool_calls along with the empty Parts).
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![ContentPart::Known(
                        KnownContentPart::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        },
                    )]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![json!({
                        "id": "toolu_xyz",
                        "type": "function",
                        "function": {"name": "calc", "arguments": "{\"x\":1}"}
                    })]),
                },
            ],
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message must survive when tool_calls fill content");
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let has_tool_use = blocks
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"));
        assert!(
            has_tool_use,
            "tool_use block must reach the wire from tool_calls; got: {blocks:?}"
        );
        // Pin id + name so a translation regression that emits a
        // tool_use block with the wrong identity still fails.
        let tool_block = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tool_block["id"], "toolu_xyz");
        assert_eq!(tool_block["name"], "calc");
        // No thinking block leaks through; the unsigned was dropped.
        let has_thinking = blocks
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"));
        assert!(
            !has_thinking,
            "unsigned thinking must not appear; got: {blocks:?}"
        );
    }

    #[test]
    fn emits_warn_when_stripping_occurs() {
        // Capture the WARN log emitted during normalize and assert:
        // - structured fields `provider`, `dropped_blocks`,
        //   `affected_messages` are present
        // - block content (the `thinking` text) is NEVER logged --
        //   could be reasoning over sensitive data.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "answer".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "TOPSECRET-REASONING-PAYLOAD".into(),
                            signature: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };

        let captured = test_capture::with_capture(|| {
            normalize("provider-x", &req, false, &[], false, None).expect("normalize succeeds");
        });

        let strip_event = captured
            .iter()
            .find(|e| e.message.contains("stripping unsigned thinking blocks"))
            .unwrap_or_else(|| panic!("expected strip WARN, got events: {captured:?}"));
        assert_eq!(strip_event.level, tracing::Level::WARN);

        // Structured fields present.
        let field_keys: Vec<&str> = strip_event.fields.iter().map(|(k, _)| k.as_str()).collect();
        for key in &["provider", "dropped_blocks", "affected_messages"] {
            assert!(
                field_keys.contains(key),
                "expected field `{key}` in WARN, got fields: {:?}",
                strip_event.fields
            );
        }
        let provider_value = strip_event
            .fields
            .iter()
            .find(|(k, _)| k == "provider")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(provider_value, "provider-x");

        // Block content must not appear anywhere in the captured events.
        for evt in &captured {
            assert!(
                !evt.message.contains("TOPSECRET-REASONING-PAYLOAD"),
                "thinking block content leaked into log message: {evt:?}"
            );
            for (_, v) in &evt.fields {
                assert!(
                    !v.contains("TOPSECRET-REASONING-PAYLOAD"),
                    "thinking block content leaked into log fields: {evt:?}"
                );
            }
        }
    }

    #[test]
    fn tool_message_without_tool_call_id_is_rejected() {
        // Anthropic requires `tool_result` to reference the
        // `tool_use.id` it answers. An empty / missing
        // `tool_call_id` on a Role::Tool message used to fall
        // through as `unwrap_or_default()` (empty string) and
        // upstream returned a vague 400. Reject locally with a
        // precise NormalizeRequest error.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                role: Role::Tool,
                content: MessageContent::Text("result content".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        let err = normalize("test-anthropic", &req, false, &[], false, None).unwrap_err();
        assert!(
            err.to_string().contains("tool_call_id"),
            "must mention tool_call_id; got: {err}"
        );
    }

    #[test]
    fn unsigned_thinking_block_is_stripped_not_rejected() {
        // Regression: prior behavior was a HTTP 400
        // ("thinking block without signature"). New behavior STRIPS
        // the unsigned block from the outgoing body and forwards the
        // rest. Cross-provider fallback (deepseek -> Anthropic) and
        // SDKs that fail to round-trip the signature field rely on
        // this -- a hard reject would 400 every multi-turn after
        // such a turn.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("hi"),
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Parts(vec![
                        ContentPart::Known(KnownContentPart::Text {
                            text: "answer".into(),
                            cache_control: None,
                        }),
                        ContentPart::Known(KnownContentPart::Thinking {
                            thinking: "let me think".into(),
                            signature: None,
                        }),
                    ]),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            ..Default::default()
        };
        // Must NOT error: the new behavior is to strip the unsigned
        // block, not reject the request.
        let body = normalize("test-anthropic", &req, false, &[], false, None).expect(
            "normalize must accept the request and strip the unsigned block; \
             a hard reject would regress the cross-provider fallback path",
        );
        let assistant = body
            .get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            })
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        // Only the text block survives; the unsigned thinking is dropped.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn assistant_tool_call_with_unparseable_arguments_wraps_under_underscore() {
        // Defensive fallback: a tool_call.arguments string that
        // isn't valid JSON shouldn't silently produce a malformed
        // upstream body. We wrap under {"_arguments": "..."} and
        // emit a WARN, so the upstream returns a useful error.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![assistant_msg(
                "",
                Some(vec![json!({
                    "id": "toolu_xyz",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "this is not json"}
                })]),
            )],
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .unwrap();
        assert_eq!(tool_use["input"], json!({"_arguments": "this is not json"}));
    }

    /// With `adaptive = true`, the wire shape is the
    /// Opus 4.7+ form -- `thinking: {type:"adaptive"}` (no
    /// `budget_tokens`) plus a top-level `output_config: {effort:...}`
    /// carrying the canonical `reasoning.effort` string verbatim.
    #[test]
    fn adaptive_emits_adaptive_shape_with_output_config() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("xhigh".into()),
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();

        // thinking serializes to {"type":"adaptive"} -- no budget_tokens.
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "adaptive");
        assert!(
            thinking.get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens, got {thinking:?}"
        );

        // output_config carries the effort verbatim.
        let oc = body.get("output_config").expect("output_config present");
        assert_eq!(oc["effort"], "xhigh");

        // Anthropic requires temperature == 1.0 with thinking active --
        // both Enabled and Adaptive variants trigger the same constraint.
        assert_eq!(body["temperature"], 1.0);
    }

    /// With `adaptive = false` (or absent), the wire
    /// shape is the legacy `Enabled { budget_tokens }` form. Older
    /// Claude models (4.5/4.6 family) still want this shape and would
    /// 400 on the adaptive form.
    #[test]
    fn legacy_thinking_unchanged_when_flag_false() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();

        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        // budget_tokens = max_tokens (2048) * effort_ratio("high")=0.80 = 1638
        assert_eq!(thinking["budget_tokens"], 1638);

        // No output_config on the legacy path.
        assert!(
            body.get("output_config").is_none(),
            "legacy shape must not emit output_config, got {body:?}"
        );

        assert_eq!(body["temperature"], 1.0);
    }

    /// `effort = "max"` on the legacy path maps to a near-total
    /// budget (max_tokens * 0.99). Adaptive path passes "max"
    /// verbatim into `output_config.effort` and never calls
    /// `effort_ratio`. This test pins the legacy mapping so a
    /// non-adaptive provider receiving `max` from the canonical
    /// surface still produces a serializable body.
    #[test]
    fn effort_max_maps_to_99_percent_legacy_path() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2000),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        let thinking = body.get("thinking").unwrap();
        assert_eq!(thinking["type"], "enabled");
        // 2000 * 0.99 = 1980
        assert_eq!(thinking["budget_tokens"], 1980);
    }

    /// `reasoning.effort = "none"` produces `Disabled` on both
    /// paths. The adaptive flag does not coerce a Disabled into an
    /// Adaptive -- if the caller said no thinking, we honor it.
    #[test]
    fn disabled_thinking_unchanged_under_adaptive_flag() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(512),
            reasoning: Some(ReasoningConfig {
                effort: Some("none".into()),
                max_tokens: None,
                exclude: None,
                enabled: None,
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        let thinking = body.get("thinking").unwrap();
        assert_eq!(thinking["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    /// The barefoot adaptive case -- `reasoning.enabled = true`
    /// with no effort and no budget. Adaptive shape applies; effort
    /// defaults to "medium". This is the only path where
    /// `derive_effort` returns the fallback string, so we pin it
    /// explicitly. (Without this test the default would silently
    /// drift if anyone changed `derive_effort`.)
    #[test]
    fn adaptive_defaults_effort_to_medium_when_unset() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    /// When `adaptive = true` AND the caller sets an
    /// explicit `reasoning.max_tokens`, the budget is dropped (the
    /// adaptive wire shape has no field for it) and a tracing::warn
    /// fires at normalize time. We can't easily assert the warn in a
    /// unit test without `tracing-test`, but we CAN pin that the
    /// resulting body is the adaptive shape with the caller's
    /// effort string (or "medium" fallback), with no budget_tokens
    /// leaking into the wire.
    #[test]
    fn adaptive_drops_max_tokens_silently() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("low".into()),
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        // budget_tokens MUST NOT leak into the adaptive shape.
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens, got {body:?}"
        );
        // The caller's effort string survives even though the budget
        // was dropped.
        assert_eq!(body["output_config"]["effort"], "low");
    }

    /// Real claude-code probe shape: `max_tokens=64` + operator
    /// `effort="high"`. The legacy `Enabled` wire shape would emit
    /// `budget_tokens=51` (64*0.80) which Anthropic 400s on the
    /// `budget_tokens >= 1024` validator. routectl must drop thinking
    /// for this request rather than emit a body that cannot succeed.
    /// Caller's `max_tokens` is preserved verbatim.
    #[test]
    fn small_max_tokens_drops_legacy_thinking() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking must be absent on probe-sized legacy requests, got {body:?}"
        );
        assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
    }

    /// Companion of the effort=high case for `effort="medium"` (ratio
    /// 0.50): `max_tokens=64` derives `budget_tokens=32`, well below
    /// the 1024 floor. routectl must drop thinking; caller's
    /// `max_tokens` is preserved verbatim (the contract that motivated
    /// rejecting clamp-and-raise).
    #[test]
    fn small_max_tokens_drops_legacy_thinking_effort_medium() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("medium".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking must be absent on effort=medium probe-sized legacy requests, got {body:?}"
        );
        assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
    }

    /// Companion for `effort="xhigh"` (ratio 0.95): `max_tokens=64`
    /// derives `budget_tokens=60`, still well below the 1024 floor.
    /// Even at the highest sub-`max` ratio the gate must fire and
    /// the caller's `max_tokens` survives unchanged.
    #[test]
    fn small_max_tokens_drops_legacy_thinking_effort_xhigh() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("xhigh".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("thinking").is_none(),
            "thinking must be absent on effort=xhigh probe-sized legacy requests, got {body:?}"
        );
        assert_eq!(body["max_tokens"], 64, "caller's max_tokens preserved");
    }

    /// Variant of the above with an explicit sub-1024 `reasoning
    /// .max_tokens`. Even an explicit caller budget must be dropped
    /// when `max_tokens` cannot carry it: emitting `Enabled
    /// { budget_tokens: 500 }` would still 400.
    #[test]
    fn small_max_tokens_drops_thinking_with_explicit_budget() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(500),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert!(body.get("thinking").is_none());
    }

    /// The adaptive shape is unaffected by the legacy floor: probe-
    /// sized `max_tokens` still receives adaptive thinking because
    /// the wire has no `budget_tokens` field and no Anthropic minimum
    /// to violate. Pins that the new gate is legacy-only.
    #[test]
    fn small_max_tokens_keeps_adaptive() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    /// `effort="high"` on `max_tokens=1100` computes `1100*0.80=880`,
    /// which would still 400 (below the 1024 floor) even though the
    /// gate accepts the request (1100 > 1024). The clamp inside each
    /// `Enabled` arm rescues the body by raising the budget to 1024.
    /// 1024 < 1100 holds, so Anthropic's `max_tokens > budget_tokens`
    /// constraint is satisfied; visible-output budget shrinks to 76.
    #[test]
    fn floor_clamps_budget_in_carryable_band() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1100),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    /// Boundary: `max_tokens=1025` is the smallest value the gate
    /// admits (`max > MIN`, not `max >= MIN`). Pins the off-by-one
    /// and confirms the clamp lands at exactly 1024.
    #[test]
    fn exactly_1025_max_tokens_keeps_thinking() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1025),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    /// Anthropic also requires `max_tokens > budget_tokens`. A caller
    /// who sends an explicit `reasoning.max_tokens` larger than
    /// `req.max_tokens` would otherwise produce a wire body that
    /// 400s. The clamp caps the budget at `max_tokens - 1`, leaving
    /// at least one visible-output token. Pins that the cap fires on
    /// the explicit-budget arm.
    #[test]
    fn explicit_budget_above_max_tokens_capped_to_max_minus_one() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1100),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(1200),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1099);
        // Anthropic invariant: max_tokens > budget_tokens.
        assert_eq!(body["max_tokens"], 1100);
    }

    /// Caller's `reasoning.max_tokens` of 500 sits BELOW the
    /// Anthropic floor (1024). With `req.max_tokens=2048` the gate
    /// accepts, and the per-arm clamp raises the budget to 1024.
    /// Pins the silent-promotion behavior on the explicit arm; the
    /// accompanying WARN is observable in production via
    /// `ROUTECTL_LOG=routectl=warn`.
    #[test]
    fn explicit_budget_below_floor_clamped_up_to_min() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(500),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    /// `reasoning.enabled = false` short-circuits to `Disabled`
    /// before the new gate runs. Without this pin, a future refactor
    /// that moved the gate above the `enabled=false` check would
    /// silently rewrite an explicit opt-out into absent-thinking.
    #[test]
    fn explicit_disable_wins_over_small_max_tokens() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(false),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[], false, None).unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    /// Tool-choice translation lives in the egress (different upstreams
    /// want different shapes; the OpenAI ingress passes wire `tool_choice`
    /// through verbatim). Pin the canonical -> Anthropic mapping for
    /// every shape we expect callers to send.
    #[test]
    fn tool_choice_string_auto_translates_to_anthropic_object() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("auto")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], json!({"type":"auto"}));
    }

    #[test]
    fn tool_choice_string_required_translates_to_any() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("required")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], json!({"type":"any"}));
    }

    #[test]
    fn tool_choice_string_none_drops_field() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("none")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "expected tool_choice dropped, got: {body:?}"
        );
        assert!(
            body.get("tools").is_none() || body["tools"].is_null(),
            "expected no tools field when caller sent neither tools nor tool_choice"
        );
    }

    /// `tool_choice = "none"` plus `tools` present must drop BOTH on the
    /// Anthropic wire. Anthropic has no native "none" -- if we send the
    /// tools but no tool_choice, Anthropic defaults to auto-select and
    /// the caller's "do not call tools" intent silently flips to "auto".
    #[test]
    fn tool_choice_none_with_tools_strips_tools_too() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("none")),
            tools: Some(vec![routectl_core::ToolDef::Custom(
                routectl_core::CustomTool {
                    name: "get_weather".into(),
                    description: Some("weather lookup".into()),
                    input_schema: json!({"type":"object"}),
                    cache_control: None,
                    defer_loading: None,
                    strict: None,
                    type_tag: None,
                },
            )]),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "expected tool_choice dropped, got: {body:?}"
        );
        assert!(
            body.get("tools").is_none() || body["tools"].is_null(),
            "expected tools dropped alongside tool_choice=none, got: {body:?}"
        );
    }

    #[test]
    fn tool_choice_function_object_translates_to_anthropic_tool() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!({"type":"function","function":{"name":"get_weather"}})),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({"type":"tool","name":"get_weather"})
        );
    }

    /// Anthropic-shape tool_choice (e.g. from claude-code via Anthropic
    /// ingress) must passthrough verbatim. Without this, the Anthropic
    /// ingress -> Anthropic egress path would double-translate and
    /// silently corrupt the field.
    #[test]
    fn tool_choice_already_anthropic_shape_passes_through_verbatim() {
        for tc in [
            json!({"type":"auto"}),
            json!({"type":"any"}),
            json!({"type":"tool","name":"X"}),
            json!({"type":"none"}),
        ] {
            let req = ChatRequest {
                model: "claude-sonnet-4-5-20250929".into(),
                messages: vec![user_msg("hi")],
                tool_choice: Some(tc.clone()),
                ..Default::default()
            };
            let body = normalize("test", &req, false, &[], false, None).unwrap();
            assert_eq!(body["tool_choice"], tc, "expected passthrough for {tc:?}");
        }
    }

    /// Unknown shapes are not coerced; let the upstream surface its
    /// own error. The OpenAI ingress still passes them through the
    /// canonical body, so the egress sees them here.
    #[test]
    fn tool_choice_unknown_object_passes_through_verbatim() {
        let weird = json!({"type":"some_future_mode","extra":"bag"});
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(weird.clone()),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["tool_choice"], weird);
    }

    /// `output_config` arriving via `provider_extras` (the path used
    /// by the Anthropic ingress for structured-output requests) is
    /// merged into the upstream body so `output_config.format` reaches
    /// api.anthropic.com unchanged. The egress doesn't need a
    /// dedicated field for this -- the provider_extras allow-list
    /// already lets `output_config` through.
    #[test]
    fn structured_output_format_merges_from_provider_extras() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            provider_extras: Some(json!({
                "output_config": {
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object"}
                    }
                }
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[], false, None).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
    }

    /// Review follow-up to Bug K: when the provider is NOT adaptive
    /// (Sonnet, Haiku -- no adaptive capability declared), the
    /// `output_config.effort` field set by cc must be stripped from
    /// the outgoing body. Anthropic 400s with "This model does not
    /// support the effort parameter" otherwise.
    #[test]
    fn output_config_effort_stripped_on_non_adaptive_provider() {
        let req = ChatRequest {
            model: "claude-haiku-4-5".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            provider_extras: Some(json!({
                "output_config": {"effort": "high"}
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, /* adaptive= */ false, &[], false, None).unwrap();
        // effort stripped; output_config now empty, so the whole
        // object is removed for wire cleanliness.
        assert!(
            body.get("output_config").is_none(),
            "non-adaptive provider must have output_config removed when effort \
             was the only sub-key, got body: {body}",
        );
    }

    /// Companion to the above: when output_config carries BOTH effort
    /// and a structured-output `format` field, the strip removes only
    /// effort; `format` is preserved (orthogonal to the effort beta
    /// and supported across the model family).
    #[test]
    fn output_config_effort_stripped_preserves_sibling_format_on_non_adaptive() {
        let req = ChatRequest {
            model: "claude-haiku-4-5".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            provider_extras: Some(json!({
                "output_config": {
                    "effort": "high",
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object", "required": ["x"]}
                    }
                }
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, /* adaptive= */ false, &[], false, None).unwrap();
        let oc = body.get("output_config").expect("output_config preserved");
        assert!(oc.get("effort").is_none(), "effort stripped: {oc}");
        assert_eq!(oc["format"]["type"], "json_schema");
        assert_eq!(oc["format"]["schema"]["required"][0], "x");
    }

    /// Adaptive providers (Opus 4.7 with supports_adaptive_thinking=true)
    /// must preserve `output_config.effort` -- the model accepts it. Pin
    /// this so a future refactor doesn't accidentally strip on the
    /// adaptive path too.
    #[test]
    fn output_config_effort_preserved_on_adaptive_provider() {
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(64),
            provider_extras: Some(json!({
                "output_config": {"effort": "high"}
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, /* adaptive= */ true, &[], false, None).unwrap();
        let oc = body.get("output_config").expect("output_config preserved");
        assert_eq!(oc["effort"], "high");
    }

    // -----------------------------------------------------------------
    // tool_choice + thinking conflict resolution
    //
    // Anthropic's extended-thinking docs explicitly forbid pairing
    // `thinking` with a `tool_choice` value that forces tool use:
    // `{"type":"any"}` or `{"type":"tool", "name": "..."}`. The
    // Messages API 400s the request with "Thinking may not be enabled
    // when tool_choice forces tool use." Real-world trigger: Claude
    // Code's WebSearch tool fires sub-requests with
    // `tool_choice: {type:"tool", name:"web_search"}` AND
    // `thinking: {type:"adaptive"}`. The strip preserves the caller's
    // tool_choice (which carries intent) and drops thinking (which is
    // a routectl-composed convenience) so the request can complete.
    // -----------------------------------------------------------------

    /// Helper: build a request with both reasoning (-> thinking) and
    /// the provided `tool_choice`. `max_tokens=2048` keeps thinking on
    /// the legacy `Enabled` path above the 1024 floor; the legacy and
    /// adaptive paths share the same conflict resolution.
    fn req_with_thinking_and_tool_choice(tool_choice: Option<Value>) -> ChatRequest {
        use routectl_core::ReasoningConfig;
        ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            reasoning: Some(ReasoningConfig {
                effort: Some("medium".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            tool_choice,
            ..Default::default()
        }
    }

    #[test]
    fn tool_choice_any_with_thinking_strips_thinking() {
        // Arrange
        let req = req_with_thinking_and_tool_choice(Some(json!({"type": "any"})));

        // Act
        let body = normalize("test", &req, false, &[], false, None).unwrap();

        // Assert: thinking dropped, tool_choice preserved verbatim.
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped when tool_choice forces tool use, got: {body}"
        );
        assert_eq!(body["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn tool_choice_tool_with_thinking_strips_thinking() {
        // Arrange: the Claude Code WebSearch shape that motivated the fix.
        let req =
            req_with_thinking_and_tool_choice(Some(json!({"type": "tool", "name": "web_search"})));

        // Act
        let body = normalize("test", &req, false, &[], false, None).unwrap();

        // Assert: thinking dropped, tool_choice preserved verbatim.
        assert!(
            body.get("thinking").is_none(),
            "thinking must be stripped when tool_choice.type=tool, got: {body}"
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "web_search"})
        );
    }

    #[test]
    fn tool_choice_auto_with_thinking_keeps_thinking() {
        // Regression guard: `auto` does not force tool use, so thinking
        // must survive.
        let req = req_with_thinking_and_tool_choice(Some(json!("auto")));

        // translate_tool_choice normalizes bare "auto" -> {"type":"auto"}
        // before strip_thinking_when_tool_choice_forces_use runs.
        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn tool_choice_none_with_thinking_keeps_thinking() {
        // Regression guard: `none` translates to no tool_choice on the
        // wire AND drops the tools array; thinking is unaffected.
        let req = req_with_thinking_and_tool_choice(Some(json!("none")));

        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "tool_choice=none must drop the field"
        );
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn no_tool_choice_with_thinking_keeps_thinking() {
        // Regression guard: absent tool_choice never triggers the strip.
        let req = req_with_thinking_and_tool_choice(None);

        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert!(body.get("tool_choice").is_none() || body["tool_choice"].is_null());
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn tool_choice_any_without_thinking_no_op() {
        // Regression guard: when thinking was never composed, the strip
        // is harmless and tool_choice survives.
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(2048),
            tool_choice: Some(json!({"type": "any"})),
            ..Default::default()
        };

        let body = normalize("test", &req, false, &[], false, None).unwrap();

        assert!(body.get("thinking").is_none());
        assert_eq!(body["tool_choice"], json!({"type": "any"}));
    }

    // ----------------------------------------------------------------
    // history_reasoning gating of the unsigned-thinking strip.
    //
    // deepseek v4's `/anthropic` endpoint (provider kind anthropic-api)
    // emits thinking blocks WITHOUT a signature yet 400s the next turn
    // unless that thinking is echoed back. `history_reasoning =
    // "preserve"` tells the egress to skip the unsigned-thinking strip
    // for those endpoints; Auto/Strip/unset keep the real-Anthropic-safe
    // strip.
    // ----------------------------------------------------------------

    /// Build a multi-turn assistant message shaped `[text, thinking,
    /// tool_use]`. `signature = None` makes the thinking block unsigned
    /// (deepseek shape); `Some(..)` makes it signed.
    fn assistant_with_thinking(signature: Option<&str>) -> Message {
        use routectl_core::{ContentPart, KnownContentPart};
        Message {
            role: Role::Assistant,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: "Let me think.".into(),
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::Thinking {
                    thinking: "deepseek reasoning".into(),
                    signature: signature.map(|s| s.to_string()),
                }),
                ContentPart::Known(KnownContentPart::ToolUse {
                    id: "toolu_1".into(),
                    name: "calc".into(),
                    input: json!({"expr": "2+2"}),
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Multi-turn request carrying the given `history_reasoning` policy
    /// on the dispatch carrier. `None` mirrors the dispatch default (no
    /// per-model policy resolved).
    fn req_with_hr(hr: Option<CoreHistoryReasoning>, assistant: Message) -> ChatRequest {
        let mut req = ChatRequest {
            model: "deepseek-chat".into(),
            messages: vec![user_msg("compute 2+2"), assistant],
            ..Default::default()
        };
        req.routectl_internal.history_reasoning = hr;
        req
    }

    /// Pull the assistant message's wire content blocks from a
    /// normalized body.
    fn assistant_blocks(body: &Value) -> Vec<Value> {
        body.get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            })
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_array())
            .cloned()
            .expect("assistant message with Blocks-form content present")
    }

    fn block_types(blocks: &[Value]) -> Vec<&str> {
        blocks
            .iter()
            .filter_map(|b| b.get("type").and_then(|v| v.as_str()))
            .collect()
    }

    #[test]
    fn preserve_history_reasoning_keeps_unsigned_thinking_for_anthropic_api() {
        // Arrange: deepseek-shape unsigned thinking + history_reasoning =
        // Preserve.
        let req = req_with_hr(
            Some(CoreHistoryReasoning::Preserve),
            assistant_with_thinking(None),
        );

        // Act: normalize under a capture so we can also assert no strip
        // WARN fires.
        let mut body = None;
        let captured = test_capture::with_capture(|| {
            body = Some(
                normalize("deepseek", &req, false, &[], false, None).expect("normalize succeeds"),
            );
        });
        let body = body.expect("normalize ran");

        // Assert: all three blocks survive; the unsigned thinking is
        // preserved (deepseek requires it echoed back).
        let blocks = assistant_blocks(&body);
        assert_eq!(
            block_types(&blocks),
            vec!["text", "thinking", "tool_use"],
            "Preserve must retain the unsigned thinking block"
        );
        let thinking = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
            .expect("thinking block present under Preserve");
        assert_eq!(thinking["thinking"], "deepseek reasoning");
        // Unsigned: signature serializes as the empty string, not dropped.
        assert_eq!(thinking["signature"], "");

        // No strip => no WARN.
        assert!(
            !captured
                .iter()
                .any(|e| e.message.contains("stripping unsigned thinking blocks")),
            "Preserve must not emit the strip WARN; got events: {captured:?}"
        );
    }

    #[test]
    fn strip_mode_still_strips_unsigned_thinking() {
        // Arrange.
        let req = req_with_hr(
            Some(CoreHistoryReasoning::Strip),
            assistant_with_thinking(None),
        );

        // Act.
        let body =
            normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

        // Assert: unsigned thinking removed, text + tool_use survive.
        let blocks = assistant_blocks(&body);
        assert_eq!(
            block_types(&blocks),
            vec!["text", "tool_use"],
            "Strip must drop the unsigned thinking block"
        );
    }

    #[test]
    fn auto_and_unset_default_to_strip() {
        // The dispatch default (None) and explicit Auto both resolve to
        // strip for the anthropic-api egress: there is no dialect-default
        // concept here, so Auto means strip (real-Anthropic-safe). Pins
        // that the default path is unchanged by the Preserve gate.
        for hr in [None, Some(CoreHistoryReasoning::Auto)] {
            // Arrange.
            let req = req_with_hr(hr, assistant_with_thinking(None));

            // Act.
            let body =
                normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

            // Assert.
            let blocks = assistant_blocks(&body);
            assert_eq!(
                block_types(&blocks),
                vec!["text", "tool_use"],
                "Auto/unset ({hr:?}) must strip unsigned thinking"
            );
        }
    }

    #[test]
    fn signed_thinking_passes_through_in_all_modes() {
        // A SIGNED thinking block is never the target of the
        // unsigned-strip, so it survives under both Preserve and Strip.
        // Pins that the gate only ever affects unsigned blocks.
        for hr in [CoreHistoryReasoning::Preserve, CoreHistoryReasoning::Strip] {
            // Arrange.
            let req = req_with_hr(Some(hr), assistant_with_thinking(Some("sig_xyz")));

            // Act.
            let body =
                normalize("anthropic", &req, false, &[], false, None).expect("normalize succeeds");

            // Assert.
            let blocks = assistant_blocks(&body);
            assert_eq!(
                block_types(&blocks),
                vec!["text", "thinking", "tool_use"],
                "signed thinking must survive under {hr:?}"
            );
            let thinking = blocks
                .iter()
                .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
                .unwrap_or_else(|| panic!("thinking block absent under {hr:?}"));
            assert_eq!(
                thinking["signature"], "sig_xyz",
                "signed thinking keeps its signature under {hr:?}"
            );
        }
    }

    #[test]
    fn tool_call_id_reject_stays_unconditional_under_preserve() {
        // The tool_result/tool_call_id hard-reject is a separate
        // correctness invariant from the thinking-strip. Preserve must
        // NOT relax it: a Role::Tool message lacking tool_call_id still
        // errors regardless of history_reasoning.
        let mut req = ChatRequest {
            model: "deepseek-chat".into(),
            messages: vec![Message {
                role: Role::Tool,
                content: MessageContent::Text("result content".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        req.routectl_internal.history_reasoning = Some(CoreHistoryReasoning::Preserve);

        let err = normalize("deepseek", &req, false, &[], false, None).unwrap_err();
        assert!(
            err.to_string().contains("tool_call_id"),
            "must reject missing tool_call_id even under Preserve; got: {err}"
        );
    }

    /// `routectl_internal` field path consulted: when `supports_adaptive_thinking`
    /// is read from `req.routectl_internal` and is `true`, the adaptive wire
    /// shape is emitted. This pins that normalize reads the canonical internal
    /// carrier rather than a hardcoded literal passed by the caller.
    #[test]
    fn normalize_reads_supports_adaptive_thinking_from_routectl_internal() {
        use routectl_core::ReasoningConfig;

        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hello")],
            max_tokens: Some(8192),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Set the flag via the routectl_internal carrier (not a parameter).
        req.routectl_internal.supports_adaptive_thinking = true;

        let body = normalize(
            "test",
            &req,
            req.routectl_internal.supports_adaptive_thinking,
            &[],
            false,
            None,
        )
        .expect("normalize must succeed");

        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(
            thinking["type"], "adaptive",
            "routectl_internal.supports_adaptive_thinking=true must yield adaptive shape"
        );
        assert!(
            thinking.get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens"
        );
    }

    /// Operator cap applied: max_thinking_budget=2000 with max_tokens=10000
    /// clamps the budget DOWN to 2000 before Anthropic's window clamp runs.
    #[test]
    fn max_thinking_budget_nonzero_clamps_budget_down() {
        use routectl_core::ReasoningConfig;

        let mut req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hello")],
            max_tokens: Some(10000),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Operator cap of 2000 < caller's explicit 8000.
        req.routectl_internal.max_thinking_budget = 2000;

        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(
            thinking["budget_tokens"], 2000,
            "max_thinking_budget=2000 must cap the explicit budget of 8000 down to 2000"
        );
    }

    /// No operator cap: max_thinking_budget=0 passes the budget through
    /// unchanged (only Anthropic's window clamp applies).
    #[test]
    fn max_thinking_budget_zero_no_op() {
        use routectl_core::ReasoningConfig;

        let mut req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hello")],
            max_tokens: Some(10000),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: Some(3000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Zero = no operator cap.
        req.routectl_internal.max_thinking_budget = 0;

        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        // budget=3000 fits in [1024, 9999] unchanged.
        assert_eq!(
            thinking["budget_tokens"], 3000,
            "max_thinking_budget=0 must not alter the budget; got {thinking:?}"
        );
    }
}

// -----------------------------------------------------------------
// Anthropic effort clamping: operator-declared effort_levels must
// cap the caller's effort on the Anthropic-shape egress (adaptive
// and legacy) matching the existing OpenAI-shape behavior.
// -----------------------------------------------------------------
#[cfg(test)]
mod anthropic_effort_clamp_tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Operator declares effort_levels = ["low","medium","high"] on an
    /// Anthropic adaptive model. Caller sends effort="max". The outgoing
    /// output_config.effort must be "high" (clamped down to the operator
    /// cap), not "max".
    #[test]
    fn adaptive_clamps_effort_to_operator_cap() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort clamped from "max" down to "high" (operator cap).
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "high",
            "effort must clamp from max to high against operator-declared effort_levels; got: {oc}"
        );
    }

    /// Operator declares effort_levels = [] (empty). Caller sends
    /// effort="max". The outgoing output_config.effort must be "max"
    /// (pass-through; current Anthropic behavior).
    #[test]
    fn adaptive_passthrough_when_effort_levels_empty() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        // Empty = pass-through semantics (default).
        req.routectl_internal.effort_levels = std::sync::Arc::from(Vec::<String>::new());

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort passes through unchanged.
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "max",
            "empty effort_levels must not clamp; got: {oc}"
        );
    }

    /// Operator declares effort_levels = ["low","medium"] on an
    /// Anthropic legacy (non-adaptive) model. Caller sends effort="high".
    /// The legacy budget must be derived from "medium" (clamped down to
    /// the operator cap), not "high".
    ///
    /// Concretely: max_tokens=4096, effort_ratio("medium")=0.50 ->
    /// budget_tokens=2048. If the clamp were absent, effort_ratio("high")
    /// would yield 0.80*4096=3276.
    #[test]
    fn legacy_clamps_effort_to_operator_cost_cap() {
        // Arrange
        let mut req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(4096),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        req.routectl_internal.effort_levels =
            std::sync::Arc::from(vec!["low".to_string(), "medium".to_string()]);

        // Act
        let body =
            normalize("test", &req, false, &[], false, None).expect("normalize must succeed");

        // Assert: budget derived from "medium" (0.50 * 4096 = 2048), not
        // from "high" (0.80 * 4096 = 3276).
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(
            thinking["budget_tokens"], 2048,
            "legacy path must clamp effort from high to medium against operator cap; got: {thinking}"
        );
    }

    /// Companion to `adaptive_clamps_effort_to_operator_cap`: the clamp
    /// must hold even when the caller's raw `output_config.effort`
    /// arrives via `provider_extras`. claude-code 2.1.153+ sends
    /// `output_config: {effort: "max"}` on every request; the Anthropic
    /// ingress preserves the whole `output_config` object verbatim in
    /// `provider_extras` so the orthogonal `output_config.format`
    /// sub-key (structured-output) passes through. derive_effort clamps
    /// "max" -> "high" on the typed struct, but merge_provider_extras
    /// then overwrites the clamped wire value with the raw caller
    /// value. Without a re-clamp on the adaptive branch of
    /// reconcile_output_config_effort, the operator's effort_levels
    /// cap is silently bypassed.
    ///
    /// The pre-existing `adaptive_clamps_effort_to_operator_cap` test
    /// leaves `provider_extras=None` so `merge_provider_extras` early-
    /// returns and the bug is masked; the
    /// `output_config_effort_preserved_on_adaptive_provider` test has
    /// empty `effort_levels` so there is no cap to violate. This test
    /// pins both: non-empty `effort_levels` AND raw `output_config.effort`
    /// in `provider_extras`.
    #[test]
    fn adaptive_clamps_effort_to_operator_cap_even_when_provider_extras_carries_raw() {
        use serde_json::json;

        // Arrange: caller asks for effort="max" both via the canonical
        // lift (req.reasoning) and via the raw output_config that the
        // ingress mirrored into provider_extras (claude-code shape);
        // operator caps effort_levels at "high".
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            provider_extras: Some(json!({
                "output_config": {"effort": "max"}
            })),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort clamped to "high" even though raw "max" was
        // layered back in by merge_provider_extras.
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "high",
            "effort_levels cap (high) must override caller-supplied output_config.effort=max \
             even when carried via provider_extras; got: {oc}"
        );
    }

    /// Companion: empty effort_levels = intentional pass-through, no
    /// re-clamp. Even when provider_extras carries
    /// `output_config.effort = "max"`, an operator who declared
    /// `effort_levels = []` (or omitted it) wants the raw value to flow
    /// through verbatim.
    #[test]
    fn adaptive_passes_through_provider_extras_effort_when_levels_empty() {
        use serde_json::json;

        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            provider_extras: Some(json!({
                "output_config": {"effort": "max"}
            })),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(Vec::<String>::new());

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(
            oc["effort"], "max",
            "empty effort_levels must pass provider_extras output_config.effort through unchanged; got: {oc}"
        );
    }

    /// Companion: `output_config.format` (structured-output) and other
    /// sibling sub-keys inside `output_config` must continue to flow
    /// through verbatim from provider_extras. The re-clamp must only
    /// touch the `effort` sub-key, never `format`.
    #[test]
    fn adaptive_reclamp_preserves_sibling_output_config_keys() {
        use serde_json::json;

        // Arrange
        let mut req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            provider_extras: Some(json!({
                "output_config": {
                    "effort": "max",
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object", "required": ["x"]}
                    }
                }
            })),
            ..Default::default()
        };
        req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);

        // Act
        let body = normalize("test", &req, true, &[], false, None).expect("normalize must succeed");

        // Assert: effort clamped, format preserved verbatim.
        let oc = body
            .get("output_config")
            .expect("output_config present on adaptive path");
        assert_eq!(oc["effort"], "high", "effort must clamp; got: {oc}");
        assert_eq!(oc["format"]["type"], "json_schema");
        assert_eq!(oc["format"]["schema"]["required"][0], "x");
    }

    #[test]
    fn output_config_is_not_routectl_managed() {
        // Pinning this invariant: output_config must remain a non-managed
        // key so provider_extras-carried sub-fields like
        // `output_config.format` flow through verbatim. The adaptive-branch
        // re-clamp at reconcile_output_config_effort relies on output_config
        // surviving merge_provider_extras intact.
        assert!(!is_routectl_managed_key("output_config"));
    }
}

// -----------------------------------------------------------------
// effort_ratio parity test: every token in VALID_EFFORT_TOKENS must
// have a non-default arm in effort_ratio. Guards against a new token
// being added to the const without a matching arm, which would
// silently return the 0.50 default ratio.
// -----------------------------------------------------------------
#[cfg(test)]
mod effort_ratio_parity_tests {
    use super::effort_ratio;
    use crate::effort::VALID_EFFORT_TOKENS;

    /// Assert that every token listed in VALID_EFFORT_TOKENS returns a
    /// ratio distinct from the default fallback arm (0.50). The only
    /// token that should legitimately equal 0.50 is "medium". All
    /// others must have a dedicated arm.
    ///
    /// If a new token is added to VALID_EFFORT_TOKENS without a
    /// matching arm in effort_ratio, it will silently receive 0.50
    /// (the default). This test surfaces that gap.
    #[test]
    fn every_valid_effort_token_has_non_default_ratio_or_is_medium() {
        // Tokens that are EXPECTED to map to 0.50 (the default ratio).
        // Only "medium" is intentional.
        const EXPECTED_DEFAULT: &[&str] = &["medium"];

        for &token in &VALID_EFFORT_TOKENS {
            let ratio = effort_ratio(token);
            if EXPECTED_DEFAULT.contains(&token) {
                // "medium" is intentionally 0.50.
                assert_eq!(
                    ratio, 0.50,
                    "token \"{token}\" expected 0.50 but got {ratio}"
                );
            } else {
                // All other tokens must have a dedicated arm (not the 0.50 default).
                assert_ne!(
                    ratio, 0.50,
                    "token \"{token}\" maps to the default ratio 0.50; \
                     add a dedicated arm to effort_ratio for this token"
                );
            }
        }
    }
}
