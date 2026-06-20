//! `routectl prompt-size --alias <X> --request <fixture.json>` -- offline
//! report of a request's token footprint and what routectl's cache /
//! reduction machinery WOULD do to it. Never dispatches upstream, never
//! resolves secrets, never touches the network.
//!
//! The fixture is parsed as a canonical `routectl_core::ChatRequest` -- the
//! shape both ingresses produce. It accepts OpenAI Chat Completions and
//! Anthropic Messages bodies structurally: `model` + `messages`, an optional
//! top-level `system`, and optional `tools`. A `system` prompt may appear
//! either as the top-level `system` field OR as `Role::System` messages; both
//! are attributed to the SYSTEM tier in the breakdown below.

use std::fs;
use std::path::Path;

use routectl_core::cache_control::compute_frozen_floor;
use routectl_core::context_reduction::{apply_json_minify, ReductionOutcome};
use routectl_core::schema::Role;
use routectl_core::{scan_volatile, ChatRequest, Error, Result};
use routectl_router::{
    validate_alias_chain_targets, validate_alias_patterns, validate_bedrock_global_config,
    validate_reasoning_defaults, validate_registry_patterns, validate_retry_policy, AliasPattern,
    AliasValue, Config, ALIAS_MAX_RECURSION_DEPTH,
};

/// Rough bytes-to-tokens divisor. Matches `context_reduction.rs`'s
/// `BYTES_PER_TOKEN_ESTIMATE`: four bytes per token is the conventional
/// English-text heuristic, good enough for an operator-facing footprint
/// signal but NOT a billing figure.
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// How auto-emit would behave for this request against the resolved target.
/// Mirrors the router's dispatch-path decision (see
/// `maybe_apply_auto_cache_control` in routectl-router); only the request-
/// level subset that an offline projection can determine is modeled here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoEmitProjection {
    /// The caller already supplied breakpoints; auto-emit defers entirely.
    CallerSupplied { breakpoints: usize },
    /// No caller breakpoints, target honors a top-level breakpoint, and the
    /// stable prefix is not high-confidence volatile: a breakpoint would be
    /// injected.
    WouldInject,
    /// Auto-emit is globally disabled in config (`[cache]
    /// auto_emit_top_level_breakpoint = false`).
    SkippedGloballyDisabled,
    /// The target does not honor a top-level `cache_control` breakpoint.
    SkippedNoCapability,
    /// The stable prefix carries high-confidence volatile tokens.
    SkippedVolatileVetoed,
    /// The target's cache capability could not be determined offline.
    Indeterminate,
}

impl AutoEmitProjection {
    /// One-line operator-facing description.
    pub fn describe(&self) -> String {
        match self {
            AutoEmitProjection::CallerSupplied { breakpoints } => {
                format!("caller-supplied ({breakpoints} breakpoints) -- auto-emit would not fire")
            }
            AutoEmitProjection::WouldInject => {
                "would inject 1 top-level ephemeral_5m breakpoint".to_string()
            }
            AutoEmitProjection::SkippedGloballyDisabled => {
                "skipped: globally_disabled ([cache] auto_emit_top_level_breakpoint = false)"
                    .to_string()
            }
            AutoEmitProjection::SkippedNoCapability => "skipped: no_capability".to_string(),
            AutoEmitProjection::SkippedVolatileVetoed => "skipped: volatile_vetoed".to_string(),
            AutoEmitProjection::Indeterminate => "indeterminate (capability unknown)".to_string(),
        }
    }
}

/// Byte + approx-token size of one request tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierSize {
    pub bytes: usize,
    pub approx_tokens: usize,
}

impl TierSize {
    fn from_bytes(bytes: usize) -> Self {
        Self {
            bytes,
            approx_tokens: bytes / BYTES_PER_TOKEN_ESTIMATE,
        }
    }
}

/// The full offline report. Pure data: built by `build_report`, rendered by
/// `print_report`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub system: TierSize,
    pub tools: TierSize,
    pub messages: TierSize,
    pub total: TierSize,
    pub auto_emit: AutoEmitProjection,
    pub reduction: ReductionOutcome,
    /// Whether dispatch-path reduction is enabled in config (`[reduction]
    /// enabled`). The reduction headroom is computed either way; this flag
    /// drives whether the description promises an applied reduction or reports
    /// the savings as conditional on enabling the feature.
    pub reduction_enabled: bool,
}

/// Compute the offline report from a parsed request and the resolved target's
/// top-level cache capability.
///
/// `supports_top_level` is `Some(true|false)` when the target's capability
/// could be resolved offline, or `None` when it could not (alias resolves to
/// an unknown / ambiguous target) -- in which case the auto-emit projection is
/// `Indeterminate`.
///
/// `auto_emit_enabled` and `reduction_enabled` are the operator's global config
/// switches (`[cache] auto_emit_top_level_breakpoint` and `[reduction]
/// enabled`). They make the projections honest: an operator sees BOTH the
/// available headroom AND whether their current config would actually apply it.
pub fn build_report(
    req: &ChatRequest,
    supports_top_level: Option<bool>,
    auto_emit_enabled: bool,
    reduction_enabled: bool,
) -> Report {
    let system = TierSize::from_bytes(system_tier_bytes(req));
    let tools = TierSize::from_bytes(tools_tier_bytes(req));
    let messages = TierSize::from_bytes(messages_tier_bytes(req));
    let total = TierSize::from_bytes(system.bytes + tools.bytes + messages.bytes);

    let auto_emit = project_auto_emit(req, supports_top_level, auto_emit_enabled);

    // Reduction is a clone-and-project: the original request is never mutated.
    // The headroom is computed regardless of the config switch; `reduction_enabled`
    // only changes how the outcome is described.
    let mut clone = req.clone();
    let reduction = apply_json_minify(&mut clone);

    Report {
        system,
        tools,
        messages,
        total,
        auto_emit,
        reduction,
        reduction_enabled,
    }
}

/// Project the auto-emit decision from the public cache primitives.
///
/// Mirrors the router's documented decision order: caller-supplied is a
/// request-level fact checked FIRST and takes precedence over every
/// capability / volatile reason; THEN the global `[cache]` kill-switch; THEN
/// capability; THEN the volatile veto.
/// (The router additionally gates on per-provider overrides and a
/// breakpoint-count cap -- those are config / runtime concerns an offline
/// footprint projection deliberately omits.)
fn project_auto_emit(
    req: &ChatRequest,
    supports_top_level: Option<bool>,
    auto_emit_enabled: bool,
) -> AutoEmitProjection {
    let floor = compute_frozen_floor(req);
    if floor.has_caller_breakpoints() {
        return AutoEmitProjection::CallerSupplied {
            breakpoints: floor.caller_breakpoint_count(),
        };
    }
    if !auto_emit_enabled {
        return AutoEmitProjection::SkippedGloballyDisabled;
    }
    match supports_top_level {
        None => AutoEmitProjection::Indeterminate,
        Some(false) => AutoEmitProjection::SkippedNoCapability,
        Some(true) => {
            if scan_volatile(req).is_high_confidence_veto() {
                AutoEmitProjection::SkippedVolatileVetoed
            } else {
                AutoEmitProjection::WouldInject
            }
        }
    }
}

/// SYSTEM tier bytes: the top-level `system` field plus any `Role::System`
/// messages (the OpenAI-shape system tier before the ingress lifts it).
fn system_tier_bytes(req: &ChatRequest) -> usize {
    let mut bytes = 0;
    if let Some(system) = &req.system {
        bytes += serialized_len(system);
    }
    for m in &req.messages {
        if matches!(m.role, Role::System) {
            bytes += serialized_len(m);
        }
    }
    bytes
}

/// TOOLS tier bytes: the `tools` array (absent -> 0).
fn tools_tier_bytes(req: &ChatRequest) -> usize {
    req.tools.as_ref().map(serialized_len).unwrap_or(0)
}

/// MESSAGES tier bytes: every non-system message (system messages are counted
/// under the SYSTEM tier).
fn messages_tier_bytes(req: &ChatRequest) -> usize {
    req.messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .map(serialized_len)
        .sum()
}

/// Serialized JSON byte length of any serializable value. A serialize failure
/// (not expected for canonical types) contributes 0 rather than panicking.
fn serialized_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

/// Resolve `--alias` to its target provider's top-level cache capability using
/// CONFIG-ONLY resolution -- no live secret resolution, no provider build.
///
/// Walks the same precedence the router's `dispatch_chain` uses (exact alias
/// key -> longest-prefix glob -> direct nickname -> `default` catch-all),
/// expands to the FIRST model nickname, then looks up that model's provider
/// entry and reads `cache_capability().supports_top_level_cache_control`.
/// Returns `None` (indeterminate) when any hop cannot be resolved offline.
fn resolve_supports_top_level(config: &Config, alias: &str) -> Option<bool> {
    let value = resolve_alias_value(config, alias)?;
    let nickname = first_nickname(config, value, 0)?;
    let model = config.models.get(&nickname)?;
    let provider = config.providers.get(&model.provider)?;
    Some(provider.cache_capability().supports_top_level_cache_control)
}

/// Resolve an alias key to its `AliasValue` via the router's precedence:
/// exact key, then longest-prefix glob, then direct-nickname (synthesized as a
/// single), then the `default` catch-all.
fn resolve_alias_value(config: &Config, alias: &str) -> Option<AliasValue> {
    if let Some(v) = config.aliases.get(alias) {
        return Some(v.clone());
    }
    if let Some(v) = longest_glob_match(config, alias) {
        return Some(v);
    }
    if config.models.contains_key(alias) {
        return Some(AliasValue::Single(alias.to_string()));
    }
    config.aliases.get("default").cloned()
}

/// Longest-prefix glob match over the alias table, mirroring the router's
/// `PrefixIndex` ordering (longest matching prefix wins). Malformed glob keys
/// are skipped here; `validate_alias_patterns` rejects them at the guards.
fn longest_glob_match(config: &Config, alias: &str) -> Option<AliasValue> {
    config
        .aliases
        .iter()
        .filter_map(|(key, value)| match AliasPattern::parse(key) {
            Ok(p @ AliasPattern::Prefix(_)) if p.matches(alias) => {
                Some((p.prefix_len(), value.clone()))
            }
            _ => None,
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, value)| value)
}

/// First model nickname an alias value expands to, recursing through nested
/// alias keys exactly as the router's `expand_alias_value` does (alias keys
/// win over model nicknames). Bounded recursion depth guards against a cycle a
/// glob hit could re-introduce; the static validator catches cycles earlier.
fn first_nickname(config: &Config, value: AliasValue, depth: usize) -> Option<String> {
    if depth > ALIAS_MAX_RECURSION_DEPTH {
        return None;
    }
    for entry in value.nicknames() {
        if let Some(nested) = config.aliases.get(entry) {
            if let Some(found) = first_nickname(config, nested.clone(), depth + 1) {
                return Some(found);
            }
        } else if config.models.contains_key(entry) {
            return Some(entry.to_string());
        }
    }
    None
}

pub fn run(config: Config, alias: &str, request_path: &Path) -> Result<()> {
    // Same cheap config-validation guards `routectl test` runs, so a
    // misconfigured alias surfaces a clean error rather than a confusing
    // resolution miss. None of these resolve secrets or touch the network.
    validate_bedrock_global_config(&config)?;
    validate_reasoning_defaults(&config)?;
    validate_alias_chain_targets(&config)?;
    validate_alias_patterns(&config)?;
    validate_retry_policy(&config)?;
    validate_registry_patterns(&config)?;

    let text = fs::read_to_string(request_path).map_err(|e| {
        Error::Config(format!(
            "cannot read request fixture `{}`: {e}",
            request_path.display()
        ))
    })?;
    let req: ChatRequest = serde_json::from_str(&text).map_err(|e| {
        Error::Validation(format!(
            "request fixture `{}` is not a valid ChatRequest body: {e}",
            request_path.display()
        ))
    })?;

    let supports_top_level = resolve_supports_top_level(&config, alias);
    let report = build_report(
        &req,
        supports_top_level,
        config.cache.auto_emit_top_level_breakpoint,
        config.reduction.enabled,
    );
    print_report(alias, &report);
    Ok(())
}

fn print_report(alias: &str, report: &Report) {
    println!("[prompt-size: alias `{alias}`]");
    println!("--- size breakdown (approx tokens = bytes / 4, rough estimate, not billing) ---");
    print_tier("SYSTEM  ", report.system);
    print_tier("TOOLS   ", report.tools);
    print_tier("MESSAGES", report.messages);
    print_tier("TOTAL   ", report.total);

    println!("--- auto-emit ---");
    println!("{}", report.auto_emit.describe());

    println!("--- reduction ---");
    println!(
        "{}",
        describe_reduction(&report.reduction, report.reduction_enabled)
    );
}

fn print_tier(label: &str, size: TierSize) {
    println!(
        "{label}  {} bytes  (~{} tokens)",
        size.bytes, size.approx_tokens
    );
}

fn describe_reduction(outcome: &ReductionOutcome, enabled: bool) -> String {
    match outcome {
        ReductionOutcome::Applied(delta) if enabled => format!(
            "would apply json-minify: {} strings minified, {} bytes saved (~{} tokens)",
            delta.strings_minified, delta.bytes_saved, delta.est_tokens_saved
        ),
        ReductionOutcome::Applied(delta) => format!(
            "reduction disabled in config ([reduction] enabled = false); would save {} bytes (~{} tokens) if enabled",
            delta.bytes_saved, delta.est_tokens_saved
        ),
        ReductionOutcome::NoMutableTail if enabled => {
            "no mutable tail (frozen prefix or no messages) -- nothing to reduce".to_string()
        }
        ReductionOutcome::NoMutableTail => {
            "reduction disabled in config ([reduction] enabled = false); no mutable tail to reduce"
                .to_string()
        }
        ReductionOutcome::NothingToStrip if enabled => {
            "mutable tail present but nothing to strip (already compact / non-JSON)".to_string()
        }
        ReductionOutcome::NothingToStrip => {
            "reduction disabled in config ([reduction] enabled = false); nothing to strip (already compact / non-JSON)".to_string()
        }
        // `ReductionOutcome` is `#[non_exhaustive]` in another crate, so this
        // wildcard is required; keep it self-describing rather than vacuous.
        _ => format!("reduction outcome: {outcome:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::cache_control::CacheControl;
    use routectl_core::content_part::{ContentPart, KnownContentPart};
    use routectl_core::schema::{Message, MessageContent};
    use routectl_core::SystemContent;
    use serde_json::json;

    fn user_text(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn tool_result_msg(content: serde_json::Value, cc: Option<CacheControl>) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content,
                    is_error: None,
                    cache_control: cc,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn reduction_projects_bytes_saved_for_pretty_tool_result_in_mutable_tail() {
        // Arrange: a pretty-JSON tool_result string in the mutable tail.
        let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
        // Compute the EXACT expected savings the same way core does: original
        // length minus the whitespace-stripped form. Keeps the assertion
        // self-consistent with `context_reduction::minify_json_whitespace`.
        let expected_bytes_saved = pretty.len()
            - routectl_core::context_reduction::minify_json_whitespace(pretty)
                .expect("pretty JSON should minify")
                .len();
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)],
            ..Default::default()
        };

        // Act
        let report = build_report(&req, Some(true), true, true);

        // Assert
        match report.reduction {
            ReductionOutcome::Applied(delta) => {
                assert_eq!(delta.strings_minified, 1);
                assert_eq!(delta.bytes_saved, expected_bytes_saved);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn auto_emit_reports_caller_supplied_when_breakpoint_present() {
        // Arrange: a caller cache_control breakpoint on a content part.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                    text: "frozen".into(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                })]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };

        // Act: caller-supplied is checked FIRST, so capability is irrelevant.
        let report = build_report(&req, Some(false), true, true);

        // Assert
        assert_eq!(
            report.auto_emit,
            AutoEmitProjection::CallerSupplied { breakpoints: 1 }
        );
    }

    #[test]
    fn auto_emit_would_inject_for_capable_target_with_non_volatile_prose() {
        // Arrange: capable target, no breakpoints, plain prose system prompt.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text("You are a careful assistant.".into())),
            messages: vec![user_text("hello there")],
            ..Default::default()
        };

        // Act
        let report = build_report(&req, Some(true), true, true);

        // Assert
        assert_eq!(report.auto_emit, AutoEmitProjection::WouldInject);
    }

    #[test]
    fn auto_emit_skipped_no_capability_for_incapable_target() {
        // Arrange: incapable target, no breakpoints.
        let req = ChatRequest {
            model: "gpt-4o".into(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            messages: vec![user_text("hi")],
            ..Default::default()
        };

        // Act
        let report = build_report(&req, Some(false), true, true);

        // Assert
        assert_eq!(report.auto_emit, AutoEmitProjection::SkippedNoCapability);
    }

    #[test]
    fn auto_emit_skipped_volatile_vetoed_for_high_confidence_prefix() {
        // Arrange: capable target, no breakpoints, a uuid in the system prefix.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text(
                "Session 550e8400-e29b-41d4-a716-446655440000 active.".into(),
            )),
            messages: vec![user_text("hi")],
            ..Default::default()
        };

        // Act
        let report = build_report(&req, Some(true), true, true);

        // Assert
        assert_eq!(report.auto_emit, AutoEmitProjection::SkippedVolatileVetoed);
    }

    #[test]
    fn auto_emit_indeterminate_when_capability_unknown() {
        // Arrange: no breakpoints, capability could not be resolved offline.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            messages: vec![user_text("hi")],
            ..Default::default()
        };

        // Act
        let report = build_report(&req, None, true, true);

        // Assert
        assert_eq!(report.auto_emit, AutoEmitProjection::Indeterminate);
    }

    #[test]
    fn tier_breakdown_sums_to_total_on_a_multi_tier_request() {
        // Arrange: a request with system, tools, and (system + non-system)
        // messages -- every tier populated.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text("You are a helpful assistant.".into())),
            tools: Some(vec![routectl_core::ToolDef::Other(json!({
                "type": "function",
                "name": "search",
                "description": "Search the web for a query string."
            }))]),
            messages: vec![
                Message {
                    refusal: None,
                    role: Role::System,
                    content: MessageContent::Text("Extra system note.".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                user_text("What is the weather today?"),
            ],
            ..Default::default()
        };

        // Act
        let report = build_report(&req, Some(true), true, true);

        // Assert: tier bytes sum to TOTAL bytes, and all tiers are populated.
        assert!(report.system.bytes > 0, "system tier should be non-empty");
        assert!(report.tools.bytes > 0, "tools tier should be non-empty");
        assert!(
            report.messages.bytes > 0,
            "messages tier should be non-empty"
        );
        assert_eq!(
            report.total.bytes,
            report.system.bytes + report.tools.bytes + report.messages.bytes
        );
    }

    /// A config with two providers of known top-level cache capability:
    /// `anthro` (anthropic-api kind -> supports_top_level = true) and `oai`
    /// (openai-compat kind -> false), each bound to a model nickname.
    fn capability_config() -> Config {
        let toml = r#"
[providers.anthro]
kind = "anthropic-api"
api_key_ref = "literal:placeholder"

[providers.oai]
kind = "openai-compat"
base_url = "https://api.example.invalid/v1"
api_key_ref = "literal:placeholder"

[models.sonnet]
provider = "anthro"
upstream = "claude-sonnet-4"

[models.gpt]
provider = "oai"
upstream = "gpt-4o"

[aliases]
fast = "sonnet"
"#;
        toml::from_str(toml).expect("test config should deserialize")
    }

    #[test]
    fn resolve_supports_top_level_follows_exact_alias_key_to_capability() {
        // Arrange: `fast` -> `sonnet` -> anthro (top-level-capable).
        let config = capability_config();

        // Act
        let supports = resolve_supports_top_level(&config, "fast");

        // Assert
        assert_eq!(supports, Some(true));
    }

    #[test]
    fn resolve_supports_top_level_falls_through_to_direct_nickname() {
        // Arrange: `gpt` has no [aliases] entry; it is a direct model nickname
        // bound to the openai-compat provider (not top-level-capable).
        let config = capability_config();

        // Act
        let supports = resolve_supports_top_level(&config, "gpt");

        // Assert
        assert_eq!(supports, Some(false));
    }

    #[test]
    fn first_nickname_returns_none_for_chain_deeper_than_recursion_cap() {
        // Arrange: a chain of alias keys a0 -> a1 -> ... that never reaches a
        // model nickname within ALIAS_MAX_RECURSION_DEPTH hops. Each aN points
        // to a(N+1), which is itself another alias key (never a model).
        let mut config = Config::default();
        let chain_len = ALIAS_MAX_RECURSION_DEPTH + 4;
        for i in 0..chain_len {
            config
                .aliases
                .insert(format!("a{i}"), AliasValue::Single(format!("a{}", i + 1)));
        }

        // Act: start the walk at the head of the over-deep chain.
        let head = config.aliases.get("a0").cloned().unwrap();
        let resolved = first_nickname(&config, head, 0);

        // Assert: the shared cap stops the walk before any nickname resolves.
        assert_eq!(resolved, None);
    }

    #[test]
    fn build_report_skips_auto_emit_when_globally_disabled() {
        // Arrange: capable target, no breakpoints, but auto-emit globally off.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            messages: vec![user_text("hi")],
            ..Default::default()
        };

        // Act: auto_emit_enabled = false.
        let report = build_report(&req, Some(true), false, true);

        // Assert: the global kill-switch wins over capability.
        assert_eq!(
            report.auto_emit,
            AutoEmitProjection::SkippedGloballyDisabled
        );
    }

    #[test]
    fn build_report_reduction_description_reflects_disabled_config_but_keeps_headroom() {
        // Arrange: a pretty-JSON tool_result in the mutable tail (reducible),
        // but reduction is disabled in config.
        let pretty = "{\n  \"rows\": [1, 2, 3]\n}";
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![tool_result_msg(json!(pretty), None)],
            ..Default::default()
        };

        // Act: reduction_enabled = false.
        let report = build_report(&req, Some(true), true, false);

        // Assert: the outcome still carries the headroom (Applied), but the
        // operator-facing description tells the truth about the config.
        assert!(!report.reduction_enabled);
        let bytes_saved = match &report.reduction {
            ReductionOutcome::Applied(delta) => {
                assert!(delta.bytes_saved > 0, "expected headroom");
                delta.bytes_saved
            }
            other => panic!("expected Applied, got {other:?}"),
        };
        let described = describe_reduction(&report.reduction, report.reduction_enabled);
        assert!(
            described.contains("reduction disabled in config"),
            "description should flag disabled config: {described}"
        );
        assert!(
            described.contains(&format!("{bytes_saved} bytes")),
            "description should still report headroom: {described}"
        );
    }
}
