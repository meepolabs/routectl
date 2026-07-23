//! Capability strip policy + the single request interceptor.
//!
//! Two decisions live here, both data-driven and const-style like
//! `capability_matcher.rs`:
//!
//! - [`action_for`] -- the single policy consult point. Given a feature
//!   key, decide whether a target that only-fails on it should be routed
//!   away from ([`CapabilityAction::RouteAway`]) or have the capability
//!   stripped in place ([`CapabilityAction::Strip`]). The essentials
//!   (`structured_output`, `computer_use`, `web_search`) are listed
//!   EXPLICITLY as route-away for intent; the catch-all `_ => RouteAway`
//!   is the fail-closed default -- an unmapped key is treated as
//!   essential and never auto-stripped, because silently corrupting the
//!   request semantics is the non-recoverable harm.
//! - [`strip_plan`] -- the per-key transform. Data-driven and able to
//!   remove a capability across MORE THAN ONE request surface (a tool in
//!   `tools`, a token in `anthropic_beta`, a key in `provider_extras`),
//!   not a single-path delete.
//!
//! [`StripInterceptor`] applies the transform under the snapshot ->
//! strip -> validate -> rollback discipline: strict pre-check before any
//! mutation, snapshot the touched fields, remove in sorted key order,
//! then a NARROW post-strip check for strip-CREATED hazards only. On a
//! hazard the snapshot is restored and the caller falls back to
//! route-away for that attempt.
//!
//! # Seeded table (precision over recall)
//!
//! - `advisor` -> `Strip(ToolParam)`. A real Anthropic server tool that
//!   Bedrock InvokeModel rejects; stripping the tool shape lets the
//!   request proceed. Its RELATED beta token is not determinable from
//!   the current codebase, so none is fabricated: advisor strips only
//!   its grounded surface, the tool shape.
//! - `context_management` -> `Strip(BetaFlag)` primary, but its
//!   transform spans TWO grounded surfaces: the `context-management-*`
//!   `anthropic_beta` token AND the `context_management` `provider_extras`
//!   body key. This is the grounded multi-surface droppable.
//!
//! Further beta-flag / body-key droppables are added only when grounded
//! in a real capability key; an unverified droppable stays out, since
//! the route-away default is always safe.

use serde_json::Value;

use routectl_core::capability::{COMPUTER_USE, STRUCTURED_OUTPUT, WEB_SEARCH};
use routectl_core::error::Error;
use routectl_core::{ChatRequest, ToolDef};

use crate::feature_keys::strip_date_suffix;

/// Feature key for the Anthropic `advisor` server tool.
const ADVISOR: &str = "advisor";
/// Feature key for the Anthropic context-management beta.
const CONTEXT_MANAGEMENT: &str = "context_management";
/// The grounded `anthropic_beta` token that enables context management.
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// What to do with a target that only fails on a given capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityAction {
    /// The capability is essential (or unknown): route the request to a
    /// target that supports it; never strip.
    RouteAway,
    /// The capability is droppable: strip it in place and proceed. The
    /// carried [`StripKind`] names the primary surface for intent; the
    /// full transform lives in [`strip_plan`] and may span more.
    Strip(StripKind),
}

/// The request surface a strip primarily touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripKind {
    /// A tool definition in `ChatRequest::tools`.
    ToolParam,
    /// A token in `ChatRequest::anthropic_beta`.
    BetaFlag,
}

/// The result of running the interceptor over a request.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing matched; the request is untouched.
    Unchanged,
    /// At least one capability was stripped and the result passed the
    /// post-strip check.
    Stripped,
    /// The request was NOT dispatched: either a strict-mode block (no
    /// mutation) or a rolled-back post-strip hazard (snapshot restored).
    Reject(Error),
}

/// The vetted inputs for one interceptor run: the capability keys the
/// dispatch layer has decided to strip, and whether strict translation
/// is in force.
#[derive(Debug, Clone, Default)]
pub struct StripContext {
    /// Capability keys to strip. De-duplicated and sorted internally, so
    /// call order never affects the output.
    pub keys: Vec<String>,
    /// When true, a would-strip key is a hard error (400) rather than a
    /// silent degradation.
    pub strict: bool,
}

/// The single policy consult point. See the module docs.
pub fn action_for(feature_key: &str) -> CapabilityAction {
    match feature_key {
        WEB_SEARCH | COMPUTER_USE | STRUCTURED_OUTPUT => CapabilityAction::RouteAway,
        ADVISOR => CapabilityAction::Strip(StripKind::ToolParam),
        CONTEXT_MANAGEMENT => CapabilityAction::Strip(StripKind::BetaFlag),
        _ => CapabilityAction::RouteAway,
    }
}

/// The surfaces a single capability's strip touches. A `None`/empty
/// field means that surface carries no target for this capability.
struct StripPlan {
    /// Date-stripped tool `type` to remove from `tools`.
    tool_type: Option<&'static str>,
    /// `anthropic_beta` tokens to remove (exact match).
    beta_tokens: &'static [&'static str],
    /// `provider_extras` top-level keys to remove.
    body_keys: &'static [&'static str],
}

/// The `anthropic_beta` tokens a strip of `feature_key` removes. Empty
/// for route-away keys, unknowns, and strip keys with no beta surface.
/// The dispatch layer's operator-floor-pin guard reads this: a strip
/// whose beta token an operator pins to the wire is re-added downstream
/// (the Bedrock egress re-adds provider `anthropic_beta`; the
/// Anthropic-API egress re-adds the `operator_betas` floor), so
/// stripping such a token is a false success -- the feature must route
/// away instead of stripping.
pub fn strip_beta_tokens(feature_key: &str) -> &'static [&'static str] {
    strip_plan(feature_key).map_or(&[], |plan| plan.beta_tokens)
}

/// The per-key transform, mirroring the const-style table of
/// `action_for`. `None` for any key with no strip transform (route-away
/// keys and unknowns).
fn strip_plan(feature_key: &str) -> Option<StripPlan> {
    match feature_key {
        ADVISOR => Some(StripPlan {
            tool_type: Some(ADVISOR),
            beta_tokens: &[],
            body_keys: &[],
        }),
        CONTEXT_MANAGEMENT => Some(StripPlan {
            tool_type: None,
            beta_tokens: &[CONTEXT_MANAGEMENT_BETA],
            body_keys: &[CONTEXT_MANAGEMENT],
        }),
        _ => None,
    }
}

/// The single request interceptor. One trait, one impl, one call site
/// (foundations defers the framework to a second consumer).
pub trait RequestInterceptor {
    /// Apply the interceptor to a per-attempt request clone. Pure in
    /// `(req, ctx)`: two runs over identical input produce byte-identical
    /// output. The caller's original request is never passed here.
    fn apply(&self, req: &mut ChatRequest, ctx: &StripContext) -> Outcome;
}

/// Strips droppable capabilities under the snapshot / validate / rollback
/// discipline.
#[derive(Debug, Default, Clone, Copy)]
pub struct StripInterceptor;

/// One surface's rollback state: either it was never touched, or it
/// carries the pre-strip value to restore. Avoids an `Option<Option<T>>`
/// for the fields that are themselves `Option`.
enum FieldSnapshot<T> {
    Untouched,
    Restore(T),
}

impl<T> FieldSnapshot<T> {
    /// Capture the surface only when a matched plan touches it, so an
    /// untouched surface is never cloned.
    fn capture(touched: bool, value: impl FnOnce() -> T) -> Self {
        if touched {
            Self::Restore(value())
        } else {
            Self::Untouched
        }
    }
}

/// The surfaces a strip touched, held for rollback. A single-surface
/// strip never clones the others (`tools` can carry many verbose
/// schemas; `provider_extras` is a deep tree). `tool_choice` is absent
/// entirely: it is only READ by `validate_post_strip`, never mutated.
struct Snapshot {
    tools: FieldSnapshot<Option<Vec<ToolDef>>>,
    anthropic_beta: FieldSnapshot<Vec<String>>,
    provider_extras: FieldSnapshot<Option<Value>>,
}

impl RequestInterceptor for StripInterceptor {
    fn apply(&self, req: &mut ChatRequest, ctx: &StripContext) -> Outcome {
        let mut keys: Vec<&str> = ctx.keys.iter().map(String::as_str).collect();
        keys.sort_unstable();
        keys.dedup();

        // Detection only -- no mutation before the strict pre-check.
        let would_strip: Vec<&str> = keys
            .into_iter()
            .filter(|key| {
                if !matches!(action_for(key), CapabilityAction::Strip(_)) {
                    return false;
                }
                match strip_plan(key) {
                    Some(plan) => plan_matches(req, &plan),
                    // action_for and strip_plan must stay in lockstep: a
                    // Strip action with no transform would degrade silently
                    // to Unchanged. Fail loud in debug/test; stay
                    // fail-closed (route-away) in release.
                    None => {
                        debug_assert!(
                            false,
                            "action_for maps `{key}` to Strip but strip_plan has no transform"
                        );
                        false
                    }
                }
            })
            .collect();

        if would_strip.is_empty() {
            return Outcome::Unchanged;
        }

        if ctx.strict {
            let named = would_strip.join(", ");
            return Outcome::Reject(Error::Validation(format!(
                "strict translation forbids stripping capability: {named}"
            )));
        }

        // Snapshot only the surfaces the matched plans actually touch.
        let mut touch_tools = false;
        let mut touch_beta = false;
        let mut touch_body = false;
        for key in &would_strip {
            if let Some(plan) = strip_plan(key) {
                touch_tools |= plan.tool_type.is_some();
                touch_beta |= !plan.beta_tokens.is_empty();
                touch_body |= !plan.body_keys.is_empty();
            }
        }
        let snapshot = Snapshot {
            tools: FieldSnapshot::capture(touch_tools, || req.tools.clone()),
            anthropic_beta: FieldSnapshot::capture(touch_beta, || req.anthropic_beta.clone()),
            provider_extras: FieldSnapshot::capture(touch_body, || req.provider_extras.clone()),
        };

        for key in &would_strip {
            if let Some(plan) = strip_plan(key) {
                apply_plan(req, &plan);
            }
        }

        if let Err(err) = validate_post_strip(req, touch_tools) {
            restore(req, snapshot);
            return Outcome::Reject(err);
        }

        Outcome::Stripped
    }
}

/// True when at least one of the plan's surfaces carries a target in the
/// request. Read-only.
fn plan_matches(req: &ChatRequest, plan: &StripPlan) -> bool {
    let tool_hit = plan
        .tool_type
        .is_some_and(|tool_type| tools_contains_type(req, tool_type));
    let beta_hit = plan
        .beta_tokens
        .iter()
        .any(|token| req.anthropic_beta.iter().any(|b| b == token));
    let body_hit = plan.body_keys.iter().any(|key| body_contains_key(req, key));
    tool_hit || beta_hit || body_hit
}

/// Remove every surface named by the plan from the request in place.
fn apply_plan(req: &mut ChatRequest, plan: &StripPlan) {
    if let Some(tool_type) = plan.tool_type {
        if let Some(tools) = req.tools.as_mut() {
            tools.retain(|tool| !tool_matches_type(tool, tool_type));
        }
        // Normalize an emptied list back to None so egress serialization
        // never emits `tools: []` (skip_serializing_if only skips None),
        // which Anthropic / Bedrock Invoke reject with a 400. Done outside
        // the as_mut borrow and only when this plan touched the tools
        // surface, so a beta/body-only strip never disturbs (or fails to
        // snapshot) an untouched tools field.
        if req.tools.as_ref().is_some_and(Vec::is_empty) {
            req.tools = None;
        }
    }
    if !plan.beta_tokens.is_empty() {
        req.anthropic_beta
            .retain(|token| !plan.beta_tokens.contains(&token.as_str()));
    }
    if !plan.body_keys.is_empty()
        && let Some(obj) = req.provider_extras.as_mut().and_then(Value::as_object_mut)
    {
        for key in plan.body_keys {
            obj.remove(*key);
        }
    }
}

/// Narrow checker for strip-CREATED hazards ONLY -- not a general
/// `ChatRequest` validator. `stripped_tools` says whether the strip
/// actually touched the tools surface; when it did not, no tool-surface
/// hazard can be strip-created, so a pre-existing invalid request (e.g. a
/// mandatory `tool_choice` over no tools before this run) is never
/// misclassified as a strip rollback. The two hazards the seeded table
/// can create are a forced `tool_choice` that now names a removed tool,
/// and a mandatory `tool_choice` whose tools the strip emptied.
fn validate_post_strip(req: &ChatRequest, stripped_tools: bool) -> Result<(), Error> {
    if !stripped_tools {
        return Ok(());
    }
    if let Some(forced) = forced_tool_name(req.tool_choice.as_ref())
        && !tools_contains_name(req, forced)
    {
        return Err(Error::Validation(format!(
            "capability strip removed tool `{forced}` still forced by tool_choice"
        )));
    }
    if tool_choice_is_mandatory(req.tool_choice.as_ref()) && tools_are_empty(req) {
        return Err(Error::Validation(
            "capability strip emptied tools while tool_choice mandates a tool".to_string(),
        ));
    }
    Ok(())
}

/// True when `tool_choice` requires the model to call some tool, across
/// both dialects: Anthropic `{"type":"any"}` / `{"type":"tool"}` and
/// OpenAI `"required"` (string) / `{"type":"function"}`. Auto / none /
/// absent do not mandate a tool.
fn tool_choice_is_mandatory(tool_choice: Option<&Value>) -> bool {
    let Some(choice) = tool_choice else {
        return false;
    };
    if choice.as_str() == Some("required") {
        return true;
    }
    matches!(
        choice
            .as_object()
            .and_then(|obj| obj.get("type"))
            .and_then(Value::as_str),
        Some("any" | "tool" | "function")
    )
}

/// True when the request carries no usable tool (`None` after the
/// emptied-list normalization, or an empty list defensively).
fn tools_are_empty(req: &ChatRequest) -> bool {
    req.tools.as_ref().is_none_or(|tools| tools.is_empty())
}

/// Restore the touched surfaces, undoing a strip. An `Untouched` field
/// was never captured, so it is left as-is.
fn restore(req: &mut ChatRequest, snapshot: Snapshot) {
    if let FieldSnapshot::Restore(tools) = snapshot.tools {
        req.tools = tools;
    }
    if let FieldSnapshot::Restore(anthropic_beta) = snapshot.anthropic_beta {
        req.anthropic_beta = anthropic_beta;
    }
    if let FieldSnapshot::Restore(provider_extras) = snapshot.provider_extras {
        req.provider_extras = provider_extras;
    }
}

fn tools_contains_type(req: &ChatRequest, tool_type: &str) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool_matches_type(tool, tool_type)))
}

/// A builtin (`Other`) tool whose date-stripped `type` equals
/// `tool_type`. `Custom` tools are canonical custom tools, never
/// builtins, so they never match a tool-type key (mirrors the
/// feature-key derivation).
fn tool_matches_type(tool: &ToolDef, tool_type: &str) -> bool {
    match tool {
        ToolDef::Other(v) => {
            v.get("type").and_then(Value::as_str).map(strip_date_suffix) == Some(tool_type)
        }
        ToolDef::Custom(_) => false,
    }
}

fn tools_contains_name(req: &ChatRequest, name: &str) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool_name(tool) == Some(name)))
}

fn tool_name(tool: &ToolDef) -> Option<&str> {
    match tool {
        ToolDef::Custom(c) => Some(c.name.as_str()),
        ToolDef::Other(v) => v.get("name").and_then(Value::as_str),
    }
}

fn body_contains_key(req: &ChatRequest, key: &str) -> bool {
    req.provider_extras
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|obj| obj.contains_key(key))
}

/// The tool name a forced `tool_choice` names, if any. Handles the
/// Anthropic (`{"type":"tool","name":X}`) and OpenAI
/// (`{"type":"function","function":{"name":X}}`) shapes; every other
/// shape (auto / any / none / string) forces no specific tool.
fn forced_tool_name(tool_choice: Option<&Value>) -> Option<&str> {
    let obj = tool_choice?.as_object()?;
    match obj.get("type").and_then(Value::as_str) {
        Some("tool") => obj.get("name").and_then(Value::as_str),
        Some("function") => obj
            .get("function")
            .and_then(Value::as_object)
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn other_tool(value: Value) -> ToolDef {
        ToolDef::Other(value)
    }

    fn ctx(keys: &[&str], strict: bool) -> StripContext {
        StripContext {
            keys: keys.iter().map(|k| (*k).to_string()).collect(),
            strict,
        }
    }

    // --- action_for table ---

    #[test]
    fn essentials_route_away() {
        // Arrange / Act / Assert -- the three essentials are explicit
        // route-away for intent.
        for key in [WEB_SEARCH, COMPUTER_USE, STRUCTURED_OUTPUT] {
            assert_eq!(action_for(key), CapabilityAction::RouteAway, "key {key}");
        }
    }

    #[test]
    fn advisor_strips_as_tool_param() {
        // Act
        let action = action_for("advisor");

        // Assert
        assert_eq!(action, CapabilityAction::Strip(StripKind::ToolParam));
    }

    #[test]
    fn context_management_strips_as_beta_flag() {
        // Act
        let action = action_for("context_management");

        // Assert
        assert_eq!(action, CapabilityAction::Strip(StripKind::BetaFlag));
    }

    #[test]
    fn unknown_key_routes_away_fail_closed() {
        // An unmapped key is treated as essential and never stripped.
        for key in ["prefill", "some_future_tool", ""] {
            assert_eq!(action_for(key), CapabilityAction::RouteAway, "key {key}");
        }
    }

    #[test]
    fn every_strip_action_key_has_a_transform() {
        // action_for and strip_plan must stay in lockstep: a Strip action
        // with no transform would degrade silently to Unchanged.
        for key in [ADVISOR, CONTEXT_MANAGEMENT] {
            assert!(
                matches!(action_for(key), CapabilityAction::Strip(_)),
                "key {key}"
            );
            assert!(strip_plan(key).is_some(), "key {key}");
        }
    }

    #[test]
    fn strip_beta_tokens_names_the_beta_surface_only() {
        // context_management rides a beta token -> the guard sees it.
        assert_eq!(
            strip_beta_tokens(CONTEXT_MANAGEMENT),
            &[CONTEXT_MANAGEMENT_BETA]
        );
        // advisor strips a tool shape, not a beta token -> empty.
        assert!(strip_beta_tokens(ADVISOR).is_empty());
        // route-away and unknown keys carry no strip transform -> empty.
        for key in [WEB_SEARCH, "prefill", ""] {
            assert!(strip_beta_tokens(key).is_empty(), "key {key}");
        }
    }

    // --- apply: per-surface removal ---

    #[test]
    fn tool_param_removes_matching_tool() {
        // Arrange -- an advisor builtin (date-suffixed) plus a keeper.
        let mut req = ChatRequest {
            tools: Some(vec![
                other_tool(json!({"type": "advisor_20250101", "name": "advisor"})),
                other_tool(json!({"type": "web_search_20250305", "name": "search"})),
            ]),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        let remaining: Vec<&str> = req
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(tool_name)
            .collect();
        assert_eq!(remaining, vec!["search"]);
    }

    #[test]
    fn beta_flag_removes_matching_token() {
        // Arrange
        let mut req = ChatRequest {
            anthropic_beta: vec![
                "context-management-2025-06-27".to_string(),
                "prompt-caching-2024-07-31".to_string(),
            ],
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["context_management"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        assert_eq!(req.anthropic_beta, vec!["prompt-caching-2024-07-31"]);
    }

    #[test]
    fn body_key_removes_matching_provider_extra() {
        // Arrange
        let mut req = ChatRequest {
            provider_extras: Some(json!({
                "context_management": {"applied_edits": []},
                "service_tier": "auto"
            })),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["context_management"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        let extras = req.provider_extras.as_ref().unwrap().as_object().unwrap();
        assert!(!extras.contains_key("context_management"));
        assert_eq!(extras.get("service_tier").unwrap(), "auto");
    }

    #[test]
    fn multi_field_key_removes_across_both_surfaces() {
        // Arrange -- context_management rides BOTH a beta token and a
        // provider_extras body key; one strip removes both.
        let mut req = ChatRequest {
            anthropic_beta: vec!["context-management-2025-06-27".to_string()],
            provider_extras: Some(json!({"context_management": {"applied_edits": []}})),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["context_management"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        assert!(req.anthropic_beta.is_empty());
        let extras = req.provider_extras.as_ref().unwrap().as_object().unwrap();
        assert!(!extras.contains_key("context_management"));
    }

    // --- apply: strict, rollback, determinism, no-op ---

    #[test]
    fn strict_rejects_would_strip_key_and_leaves_request_unmutated() {
        // Arrange
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], true));

        // Assert -- 400 naming the key, tools untouched.
        match outcome {
            Outcome::Reject(Error::Validation(msg)) => assert!(msg.contains("advisor"), "{msg}"),
            other => panic!("expected Reject(Validation), got {other:?}"),
        }
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn dangling_forced_tool_choice_rolls_back() {
        // Arrange -- tool_choice forces the advisor tool that the strip
        // removes, a strip-created hazard.
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

        // Assert -- rejected AND the snapshot restored (tool + choice back).
        assert!(matches!(outcome, Outcome::Reject(Error::Validation(_))));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(
            req.tool_choice.as_ref().unwrap(),
            &json!({"type": "tool", "name": "advisor"})
        );
    }

    #[test]
    fn forced_tool_choice_survives_when_forced_tool_kept() {
        // Arrange -- strip advisor while tool_choice forces a DIFFERENT,
        // surviving tool; no hazard.
        let mut req = ChatRequest {
            tools: Some(vec![
                other_tool(json!({"type": "advisor", "name": "advisor"})),
                other_tool(json!({"type": "web_search_20250305", "name": "search"})),
            ]),
            tool_choice: Some(json!({"type": "tool", "name": "search"})),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn beta_only_strip_leaves_tools_untouched() {
        // A BetaFlag-only strip must not snapshot or disturb req.tools:
        // the tools list is byte-identical before and after.
        let tools = vec![
            other_tool(json!({"type": "web_search_20250305", "name": "search"})),
            other_tool(json!({"type": "bash_20250124", "name": "bash"})),
        ];
        let mut req = ChatRequest {
            tools: Some(tools.clone()),
            anthropic_beta: vec!["context-management-2025-06-27".to_string()],
            ..Default::default()
        };
        let tools_before = serde_json::to_value(&tools).unwrap();

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["context_management"], false));

        // Assert -- beta gone, tools identical.
        assert!(matches!(outcome, Outcome::Stripped));
        assert!(req.anthropic_beta.is_empty());
        assert_eq!(
            serde_json::to_value(req.tools.unwrap()).unwrap(),
            tools_before
        );
    }

    #[test]
    fn rollback_restores_every_touched_surface() {
        // Arrange -- a mixed strip: advisor (tool) removal dangles the
        // forced tool_choice AND context_management (beta) is stripped in
        // the same pass. The rollback must restore BOTH touched surfaces.
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            anthropic_beta: vec!["context-management-2025-06-27".to_string()],
            tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
            ..Default::default()
        };

        // Act
        let outcome =
            StripInterceptor.apply(&mut req, &ctx(&["advisor", "context_management"], false));

        // Assert -- rejected; tool AND beta token both restored.
        assert!(matches!(outcome, Outcome::Reject(Error::Validation(_))));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(req.anthropic_beta, vec!["context-management-2025-06-27"]);
    }

    #[test]
    fn two_applies_over_identical_input_are_byte_identical() {
        // Arrange -- keys supplied in unsorted, duplicated order to prove
        // the sort/dedup makes output order-independent.
        let base = ChatRequest {
            tools: Some(vec![
                other_tool(json!({"type": "advisor_20250101", "name": "advisor"})),
                other_tool(json!({"type": "bash_20250124", "name": "bash"})),
            ]),
            anthropic_beta: vec![
                "context-management-2025-06-27".to_string(),
                "prompt-caching-2024-07-31".to_string(),
            ],
            provider_extras: Some(json!({"context_management": {}, "service_tier": "auto"})),
            ..Default::default()
        };
        let keys = ctx(&["context_management", "advisor", "advisor"], false);

        // Act
        let mut a = base.clone();
        let mut b = base.clone();
        let out_a = StripInterceptor.apply(&mut a, &keys);
        let out_b = StripInterceptor.apply(&mut b, &keys);

        // Assert
        assert!(matches!(out_a, Outcome::Stripped));
        assert!(matches!(out_b, Outcome::Stripped));
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
    }

    #[test]
    fn nothing_matching_yields_unchanged() {
        // Arrange -- a strip key that matches no surface in the request.
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "web_search_20250305", "name": "search"}),
            )]),
            ..Default::default()
        };
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Unchanged));
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }

    #[test]
    fn strict_with_no_matching_key_is_unchanged_not_rejected() {
        // Strict only blocks an ACTUAL would-strip; an absent capability
        // is a plain no-op even under strict.
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "web_search_20250305", "name": "search"}),
            )]),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], true));

        // Assert
        assert!(matches!(outcome, Outcome::Unchanged));
    }

    #[test]
    fn route_away_key_is_never_stripped_even_if_listed() {
        // Defensive: an essential key handed in the strip list is ignored
        // (action_for gates strip on Strip(_) only).
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "web_search_20250305", "name": "search"}),
            )]),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["web_search"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Unchanged));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
    }

    // --- emptied-tools normalization + mandatory tool_choice ---

    #[test]
    fn sole_stripped_tool_normalizes_tools_to_none() {
        // The advisor is the only tool: after the retain the list is empty
        // and must normalize to None, not serialize as `[]`.
        let mut req = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        assert!(
            req.tools.is_none(),
            "an emptied tools list normalizes to None, not Some([])",
        );
    }

    #[test]
    fn tool_choice_is_mandatory_recognizes_all_four_shapes() {
        for choice in [
            json!({"type": "any"}),
            json!({"type": "tool"}),
            json!("required"),
            json!({"type": "function"}),
        ] {
            assert!(
                tool_choice_is_mandatory(Some(&choice)),
                "mandatory shape {choice}",
            );
        }
        for choice in [
            json!({"type": "auto"}),
            json!({"type": "none"}),
            json!("auto"),
            json!("none"),
        ] {
            assert!(
                !tool_choice_is_mandatory(Some(&choice)),
                "non-mandatory shape {choice}",
            );
        }
        assert!(!tool_choice_is_mandatory(None));
    }

    #[test]
    fn mandatory_choice_emptying_tools_rolls_back_for_every_shape() {
        // For each mandatory shape, stripping the sole tool empties the
        // list; the post-strip check rejects and rollback restores the
        // original Some([...]) and the original tool_choice.
        for choice in [
            json!({"type": "any"}),
            json!({"type": "tool"}),
            json!("required"),
            json!({"type": "function"}),
        ] {
            let mut req = ChatRequest {
                tools: Some(vec![other_tool(
                    json!({"type": "advisor", "name": "advisor"}),
                )]),
                tool_choice: Some(choice.clone()),
                ..Default::default()
            };

            // Act
            let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

            // Assert -- rejected, and the original Some([...]) restored.
            assert!(
                matches!(outcome, Outcome::Reject(Error::Validation(_))),
                "shape {choice} must reject",
            );
            assert_eq!(
                req.tools.as_ref().map(Vec::len),
                Some(1),
                "rollback restores the original Some([...]) for shape {choice}",
            );
            assert_eq!(
                req.tool_choice.as_ref(),
                Some(&choice),
                "rollback leaves tool_choice untouched for shape {choice}",
            );
        }
    }

    #[test]
    fn mandatory_choice_with_surviving_tool_is_not_rejected() {
        // Stripping advisor leaves a surviving tool, so a mandatory choice
        // is still satisfiable -- no hazard, no rollback.
        let mut req = ChatRequest {
            tools: Some(vec![
                other_tool(json!({"type": "advisor", "name": "advisor"})),
                other_tool(json!({"type": "web_search_20250305", "name": "search"})),
            ]),
            tool_choice: Some(json!({"type": "any"})),
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["advisor"], false));

        // Assert
        assert!(matches!(outcome, Outcome::Stripped));
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn preexisting_mandatory_no_tools_is_not_misclassified_as_rollback() {
        // The request is ALREADY invalid (mandatory choice, no tools)
        // before this run; a beta-only strip does not touch the tools
        // surface, so the pre-existing invalidity must not be read as a
        // strip-created hazard and rolled back.
        let mut req = ChatRequest {
            tools: None,
            tool_choice: Some(json!({"type": "any"})),
            anthropic_beta: vec!["context-management-2025-06-27".to_string()],
            ..Default::default()
        };

        // Act
        let outcome = StripInterceptor.apply(&mut req, &ctx(&["context_management"], false));

        // Assert -- the beta strip proceeds; pre-existing invalidity is not
        // the strip's hazard.
        assert!(matches!(outcome, Outcome::Stripped));
        assert!(req.anthropic_beta.is_empty());
    }

    #[test]
    fn rejection_message_distinguishes_named_from_mandatory() {
        // named-tool-removed: tool_choice forces the removed tool by name.
        let mut named = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
            ..Default::default()
        };
        match StripInterceptor.apply(&mut named, &ctx(&["advisor"], false)) {
            Outcome::Reject(Error::Validation(msg)) => {
                assert!(msg.contains("removed tool `advisor`"), "{msg}");
            }
            other => panic!("expected named-tool rejection, got {other:?}"),
        }

        // mandatory-choice-no-tools: tool_choice mandates a tool but names
        // none, and the strip emptied the list.
        let mut mandatory = ChatRequest {
            tools: Some(vec![other_tool(
                json!({"type": "advisor", "name": "advisor"}),
            )]),
            tool_choice: Some(json!({"type": "any"})),
            ..Default::default()
        };
        match StripInterceptor.apply(&mut mandatory, &ctx(&["advisor"], false)) {
            Outcome::Reject(Error::Validation(msg)) => {
                assert!(msg.contains("emptied tools"), "{msg}");
                assert!(!msg.contains("removed tool"), "{msg}");
            }
            other => panic!("expected mandatory-choice rejection, got {other:?}"),
        }
    }
}
