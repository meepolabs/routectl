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
use routectl_core::context_reduction::{ReductionOutcome, apply_json_minify};
use routectl_core::schema::Role;
use routectl_core::{ChatRequest, Error, RefusalReason, Result, scan_volatile};
use routectl_router::{
    ALIAS_MAX_RECURSION_DEPTH, AliasPattern, AliasValue, CacheRules, Config, GateDecision,
    KeepReason, PrefixReductionCandidate, TrimConfig, break_even_k, cache_rules,
    collect_config_validation, evaluate, propose_steady_state_trim,
};

/// Rough bytes-to-tokens divisor. Matches `context_reduction.rs`'s
/// `BYTES_PER_TOKEN_ESTIMATE`: four bytes per token is the conventional
/// English-text heuristic, good enough for an operator-facing footprint
/// signal but NOT a billing figure.
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// How auto-emit would behave for this request against the resolved target.
/// Mirrors the router's dispatch-path decision (see
/// `apply_auto_cache_placement` in routectl-router); only the request-
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
            Self::CallerSupplied { breakpoints } => {
                format!("caller-supplied ({breakpoints} breakpoints) -- auto-emit would not fire")
            }
            Self::WouldInject => "would inject 1 top-level ephemeral_5m breakpoint".to_string(),
            Self::SkippedGloballyDisabled => {
                "skipped: globally_disabled ([cache] auto_emit_top_level_breakpoint = false)"
                    .to_string()
            }
            Self::SkippedNoCapability => "skipped: no_capability".to_string(),
            Self::SkippedVolatileVetoed => "skipped: volatile_vetoed".to_string(),
            Self::Indeterminate => "indeterminate (capability unknown)".to_string(),
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
    const fn from_bytes(bytes: usize) -> Self {
        Self {
            bytes,
            approx_tokens: bytes / BYTES_PER_TOKEN_ESTIMATE,
        }
    }
}

/// The full offline report. Pure data: built by `build_report`, rendered by
/// `print_report`. `Eq` is intentionally not derived: the optional
/// `economics` projection carries `f64` break-even / verdict values.
#[derive(Debug, Clone, PartialEq)]
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
    /// OPTIONAL cache-break economics projection. `Some` only when the operator
    /// supplied `--hypothetical-d`; `None` keeps the report's legacy output
    /// byte-for-byte unchanged (backward compatibility).
    pub economics: Option<EconomicsProjection>,
}

/// Optional CLI arguments that turn on the cache-break economics projection.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionArgs {
    pub hypothetical_d: Option<u64>,
    pub hypothetical_k: Option<f64>,
    pub c_after: Option<u64>,
    /// When set, compute the REAL steady-state-trim candidate for the request
    /// (via `propose_steady_state_trim`) and price IT, instead of pricing an
    /// operator-supplied `--hypothetical-d`. Mutually exclusive with
    /// `--hypothetical-d` (the parser rejects supplying both).
    pub steady_state: bool,
}

/// The advisory cache-break economics projection: the resolved provider
/// kind's cache rules, the break-even reuse threshold K*, and (when a `--hypothetical-k`
/// was supplied) a keep/break verdict. Pure data: built by
/// `build_economics`, rendered by `print_economics`. Never mutates a request,
/// resolves a secret, or touches the network.
#[derive(Debug, Clone, PartialEq)]
pub struct EconomicsProjection {
    /// Resolved provider-kind discriminant (e.g. `anthropic-api`), or `None`
    /// when the alias could not be resolved to a target offline.
    pub provider_kind: Option<String>,
    /// Resolved upstream model id, or `None` when unresolved offline.
    pub model: Option<String>,
    /// The resolved provider kind's cache rules, or `None` when the alias
    /// did not resolve offline or the kind has no rules (Gemini): the
    /// projection is then unpriced.
    pub rules: Option<CacheRules>,
    /// Break-even reuse count K*: the minimum future prefix reuses at which
    /// breaking the cache turns net-positive. `None` when `d == 0` or the
    /// projection is unpriced (no live decision is meaningful).
    pub break_even_k: Option<f64>,
    /// The candidate cut handed to the gate.
    pub candidate: PrefixReductionCandidate,
    /// The keep/break verdict at `--hypothetical-k`, present only when that
    /// flag was supplied.
    pub verdict: Option<GateDecision>,
    /// For the `--steady-state` projection: whether the deterministic trimmer
    /// produced a real cut for this request (`Some(true)`), declined because no
    /// safe trigger-clearing cut exists (`Some(false)`), or this is the
    /// `--hypothetical-d` path where the steady-state trimmer was not run
    /// (`None`).
    pub steady_state_would_trim: Option<bool>,
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
        economics: None,
    }
}

/// Build the advisory cache-break economics projection for a resolved target.
///
/// `C` (total cacheable prefix tokens) is taken from the report's TOTAL
/// approx-token count: the command does not distinguish a separate stable /
/// cacheable-prefix token count (its tiers are SYSTEM / TOOLS / MESSAGES, not a
/// frozen-prefix slice), so the whole prompt-token footprint is the
/// conservative `C`. `C_after` defaults to `C` (the oldest-first conservative
/// case) unless the operator overrides it. `D` is the proposed cut.
///
/// `target` is `Some((provider_kind, model))` when the alias resolved offline,
/// or `None` (the projection is then unpriced and the verdict reads
/// "insufficient data").
fn build_economics(
    report_total_tokens: usize,
    target: Option<(&'static str, String)>,
    args: &ProjectionArgs,
) -> EconomicsProjection {
    let d = args.hypothetical_d.unwrap_or(0);
    let c = report_total_tokens as u64;
    let c_after = args.c_after.unwrap_or(c);
    let candidate = PrefixReductionCandidate::new(d, c_after, c);
    price_candidate(candidate, target, args, None)
}

/// Build the advisory economics projection for the REAL steady-state-trim
/// candidate of `req`, instead of an operator-supplied `--hypothetical-d`.
///
/// Runs the deterministic trimmer using `trim.to_params()` -- the SAME
/// shared constructor `Router::record_would_trim` calls on the live dispatch
/// path, so both resolve identical `SteadyStateTrimParams` from the same
/// `Config`. When it produces a cut, the cut's own `(d, c_after, c)`
/// candidate is priced and `steady_state_would_trim` is `Some(true)`. When
/// the trimmer declines (request too short / below trigger / no safe
/// elidable span), the projection reports `Some(false)` and a zero candidate
/// that the gate reads as "nothing to remove" -- a KEEP. The operator's
/// `--hypothetical-k` still applies.
fn build_steady_state_economics(
    req: &ChatRequest,
    target: Option<(&'static str, String)>,
    args: &ProjectionArgs,
    trim: &TrimConfig,
) -> EconomicsProjection {
    let params = trim.to_params();
    if let Some(plan) = propose_steady_state_trim(req, &params) {
        price_candidate(plan.candidate, target, args, Some(true))
    } else {
        let zero = PrefixReductionCandidate::new(0, 0, 0);
        price_candidate(zero, target, args, Some(false))
    }
}

/// Shared pricing core: look up the target provider kind's cache rules,
/// compute the break-even K* and (when `--hypothetical-k` was given) the
/// keep/break verdict, and assemble the projection. `would_trim` flags
/// whether this came from the steady-state trimmer (`Some`) or the
/// hypothetical-d path (`None`).
fn price_candidate(
    candidate: PrefixReductionCandidate,
    target: Option<(&'static str, String)>,
    args: &ProjectionArgs,
    would_trim: Option<bool>,
) -> EconomicsProjection {
    let (provider_kind, model) = match &target {
        Some((kind, model)) => (Some((*kind).to_string()), Some(model.clone())),
        None => (None, None),
    };
    let rules = target.as_ref().and_then(|(kind, _)| cache_rules(kind));

    let break_even = rules.and_then(|r| break_even_k(&r, &candidate));
    let verdict = args.hypothetical_k.map(|k| match rules {
        Some(r) => evaluate(&r, &candidate, k),
        None => GateDecision::Keep {
            reason: KeepReason::InsufficientData,
        },
    });

    EconomicsProjection {
        provider_kind,
        model,
        rules,
        break_even_k: break_even,
        candidate,
        verdict,
        steady_state_would_trim: would_trim,
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
    for m in &*req.messages {
        if matches!(m.role, Role::System) {
            bytes += serialized_len(m);
        }
    }
    bytes
}

/// TOOLS tier bytes: the `tools` array (absent -> 0).
fn tools_tier_bytes(req: &ChatRequest) -> usize {
    req.tools.as_ref().map_or(0, serialized_len)
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
    serde_json::to_string(value).map_or(0, |s| s.len())
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

/// Resolve `--alias` to its target `(provider_kind, upstream_model)` using the
/// SAME config-only precedence as `resolve_supports_top_level` -- no live
/// secret resolution, no provider build, no network. `provider_kind` is the
/// stable `kind_str()` discriminant the cache-economics projection prices
/// by; `model` is the resolved upstream id. Returns `None` when any hop
/// cannot be resolved offline.
fn resolve_target(config: &Config, alias: &str) -> Option<(&'static str, String)> {
    let value = resolve_alias_value(config, alias)?;
    let nickname = first_nickname(config, value, 0)?;
    let model = config.models.get(&nickname)?;
    let provider = config.providers.get(&model.provider)?;
    Some((provider.kind_str(), model.upstream.clone()))
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

pub fn run(
    config: Config,
    alias: &str,
    request_path: &Path,
    projection: ProjectionArgs,
) -> Result<()> {
    // Same cheap config-validation guards `routectl test` and the serve
    // path run, through the shared ordered suite, so a misconfigured
    // alias / override surfaces a clean error rather than a confusing
    // resolution miss or a silent bad-price. None of these resolve
    // secrets or touch the network. Fail-fast on the first error; the
    // collected strings are bare, so wrapping in `Error::Config` re-adds
    // the `config: ` prefix on render.
    let validation = collect_config_validation(&config);
    if let Some(first) = validation.errors.into_iter().next() {
        return Err(Error::Config(first));
    }

    let text = fs::read_to_string(request_path).map_err(|e| {
        Error::Config(format!(
            "cannot read request fixture `{}`: {e}",
            request_path.display()
        ))
    })?;
    let req: ChatRequest = serde_json::from_str(&text).map_err(|e| {
        Error::Validation(
            format!(
                "request fixture `{}` is not a valid ChatRequest body: {e}",
                request_path.display()
            ),
            RefusalReason::Unparseable,
        )
    })?;

    let supports_top_level = resolve_supports_top_level(&config, alias);
    let mut report = build_report(
        &req,
        supports_top_level,
        config.cache.auto_emit_top_level_breakpoint,
        config.reduction.enabled,
    );

    // The economics projection is opt-in. Two mutually-exclusive entry points:
    //   --steady-state   -> price the REAL deterministic trim candidate.
    //   --hypothetical-d -> price an operator-supplied hypothetical cut.
    // Without either, the report renders exactly as before.
    if projection.steady_state {
        let target = resolve_target(&config, alias);
        report.economics = Some(build_steady_state_economics(
            &req,
            target,
            &projection,
            &config.trim,
        ));
    } else if projection.hypothetical_d.is_some() {
        let target = resolve_target(&config, alias);
        report.economics = Some(build_economics(
            report.total.approx_tokens,
            target,
            &projection,
        ));
    } else if projection.c_after.is_some() || projection.hypothetical_k.is_some() {
        eprintln!(
            "warning: --c-after / --hypothetical-k have no effect without \
             --hypothetical-d or --steady-state"
        );
    }

    print_report(alias, &report);
    Ok(())
}

fn print_report(alias: &str, report: &Report) {
    print!("{}", render_report(alias, report));
}

/// Render the full offline report to a String (byte-identical to what
/// `print_report` emits). Split out so tests can assert on the rendered text
/// without capturing process stdout.
fn render_report(alias: &str, report: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "[prompt-size: alias `{alias}`]");
    let _ = writeln!(
        out,
        "--- size breakdown (approx tokens = bytes / 4, rough estimate, not billing) ---"
    );
    write_tier(&mut out, "SYSTEM  ", report.system);
    write_tier(&mut out, "TOOLS   ", report.tools);
    write_tier(&mut out, "MESSAGES", report.messages);
    write_tier(&mut out, "TOTAL   ", report.total);

    let _ = writeln!(out, "--- auto-emit ---");
    let _ = writeln!(out, "{}", report.auto_emit.describe());

    let _ = writeln!(out, "--- reduction ---");
    let _ = writeln!(
        out,
        "{}",
        describe_reduction(&report.reduction, report.reduction_enabled)
    );

    if let Some(economics) = &report.economics {
        out.push_str(&render_economics(economics));
    }
    out
}

/// Render the advisory cache-break economics projection to a String. Only
/// appended when the operator supplied `--hypothetical-d`, so the legacy
/// output stays unchanged for the no-flag invocation.
fn render_economics(economics: &EconomicsProjection) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "--- cache-break economics (advisory) ---");
    let provider_kind = economics.provider_kind.as_deref().unwrap_or("(unresolved)");
    let model = economics.model.as_deref().unwrap_or("(unresolved)");
    let _ = writeln!(out, "target: {provider_kind} / {model}");
    // For the steady-state path, lead with the deterministic trimmer's
    // would-trim decision before the candidate it priced.
    if let Some(would_trim) = economics.steady_state_would_trim {
        let _ = writeln!(
            out,
            "steady-state trim: would-trim {}",
            if would_trim { "yes" } else { "no" }
        );
    }
    let _ = writeln!(out, "cache rules: {}", describe_rules(economics.rules));
    let _ = writeln!(
        out,
        "candidate: D={} tokens  C_after={} tokens  C={} tokens",
        economics.candidate.d, economics.candidate.c_after, economics.candidate.c
    );

    match economics.break_even_k {
        Some(k_star) => {
            let _ = writeln!(out, "break-even K* = {k_star:.2} future reuses");
        }
        None if economics.rules.is_none() => {
            let _ = writeln!(out, "break-even K*: n/a (insufficient data)");
        }
        None => {
            let _ = writeln!(out, "break-even K*: n/a (nothing to remove)");
        }
    }

    if let Some(verdict) = &economics.verdict {
        let _ = writeln!(out, "verdict: {}", describe_verdict(verdict));
    }
    out
}

/// Operator-facing summary of the cache rules the projection priced with,
/// or `unpriced` when the target did not resolve or its kind has none.
fn describe_rules(rules: Option<CacheRules>) -> String {
    match rules {
        Some(r) => format!(
            "write {}x  read {}x  ttl {}s  min prefix {} tokens{}",
            r.wm,
            r.rm,
            r.ttl_seconds,
            r.min_prefix_tokens,
            if r.auto_cacher { "  (auto-cached)" } else { "" }
        ),
        None => "unpriced (no cache rules for this target)".to_string(),
    }
}

/// Map a `GateDecision` to a readable verdict line that includes the stable
/// ledger strategy token.
fn describe_verdict(decision: &GateDecision) -> String {
    let strategy = decision.strategy_str();
    match decision {
        GateDecision::Keep { reason } => {
            format!("KEEP ({}) [{strategy}]", describe_keep_reason(reason))
        }
        GateDecision::Break { delta_tokens } => {
            format!("BREAK (cut {delta_tokens} tokens) [{strategy}]")
        }
        GateDecision::FreeBreak {
            delta_tokens,
            reason,
        } => format!("FREE-BREAK (cut {delta_tokens} tokens; {reason}) [{strategy}]"),
        // `GateDecision` is `#[non_exhaustive]`; keep self-describing.
        _ => format!("{decision:?} [{strategy}]"),
    }
}

/// Human-readable text for a `KeepReason`.
const fn describe_keep_reason(reason: &KeepReason) -> &'static str {
    match reason {
        KeepReason::NetNegative => "net-negative at this reuse count",
        KeepReason::BelowMinPrefix => "remaining prefix below cacheable floor",
        KeepReason::InsufficientData => "insufficient data",
        KeepReason::NoCandidate => "nothing to remove",
        // `KeepReason` is `#[non_exhaustive]`; keep self-describing.
        _ => "keep",
    }
}

fn write_tier(out: &mut String, label: &str, size: TierSize) {
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
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
        ReductionOutcome::NothingToStrip(_) if enabled => {
            "mutable tail present but nothing to strip (already compact / non-JSON)".to_string()
        }
        ReductionOutcome::NothingToStrip(_) => {
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
    use routectl_router::CURRENT_CONFIG_VERSION;

    use routectl_core::SystemContent;
    use routectl_core::cache_control::CacheControl;
    use routectl_core::content_part::{ContentPart, KnownContentPart};
    use routectl_core::schema::{Message, MessageContent};
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
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
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
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                })]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
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
            messages: vec![user_text("hello there")].into(),
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
            messages: vec![user_text("hi")].into(),
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
            messages: vec![user_text("hi")].into(),
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
            messages: vec![user_text("hi")].into(),
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
            ]
            .into(),
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
    fn run_tags_a_request_fixture_that_is_not_a_chat_request_as_unparseable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("request.json");
        std::fs::write(&path, "{\"model\": 7}").expect("write fixture");
        let projection = ProjectionArgs {
            hypothetical_d: None,
            hypothetical_k: None,
            c_after: None,
            steady_state: false,
        };

        let err = run(capability_config(), "fast", &path, projection)
            .expect_err("the fixture is not a ChatRequest");

        assert!(err.to_string().contains("not a valid ChatRequest"), "{err}");
        assert_eq!(err.refusal_reason(), Some(RefusalReason::Unparseable));
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
            messages: vec![user_text("hi")].into(),
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
            messages: vec![tool_result_msg(json!(pretty), None)].into(),
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

    // -- cache-break economics projection -----------------------------

    /// A config resolving alias `heavy` -> model `opus` -> an anthropic-api
    /// provider, `mystery` -> model `mystery_model` -> an openai-compat
    /// provider, and `flash` -> model `gem_model` -> a gemini provider (a kind
    /// with no cache rules).
    fn economics_config() -> Config {
        toml::from_str(&economics_config_toml()).expect("test config should deserialize")
    }

    #[test]
    fn build_economics_prices_anthropic_target_from_its_kind_rules() {
        // Arrange: `heavy` resolves to anthropic-api. Use the cost-gate doc's
        // scenario (D=50k, C=C_after=200k) so the wiring must reproduce the
        // gate's own K* for the anthropic-api rules.
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        assert_eq!(
            target,
            Some(("anthropic-api", "claude-opus-4-8".to_string()))
        );
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: None,
            c_after: Some(200_000),
            steady_state: false,
        };

        // Act: C is the report's total approx tokens; pass 200_000 directly.
        let economics = build_economics(200_000, target, &args);

        // Assert: the projection carries the anthropic-api rules, and its K*
        // is the gate's own computation for them: (200000 * 1.25) /
        // (50000 * 0.1) == 50.
        let rules = cache_rules("anthropic-api").expect("anthropic-api has rules");
        assert_eq!(economics.rules, Some(rules));
        let candidate = PrefixReductionCandidate::new(50_000, 200_000, 200_000);
        let expected = break_even_k(&rules, &candidate).expect("d > 0");
        assert_eq!(economics.break_even_k, Some(expected));
        assert!((economics.break_even_k.unwrap() - 50.0).abs() < 1e-4);
        assert_eq!(economics.provider_kind.as_deref(), Some("anthropic-api"));
    }

    #[test]
    fn build_economics_verdict_breaks_above_break_even_k() {
        // Arrange: same anthropic-api target, K* == 50; assume k just above it.
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: Some(50.0001),
            c_after: Some(200_000),
            steady_state: false,
        };

        // Act
        let economics = build_economics(200_000, target, &args);

        // Assert: a BREAK verdict carrying the cut token count + ledger token.
        let verdict = economics.verdict.expect("k was supplied");
        assert_eq!(
            verdict,
            GateDecision::Break {
                delta_tokens: 50_000
            }
        );
        let rendered = describe_verdict(&verdict);
        assert!(rendered.contains("BREAK"), "rendered: {rendered}");
        assert!(rendered.contains("cost_gate:break"), "rendered: {rendered}");
    }

    #[test]
    fn build_economics_prices_any_model_of_a_kind_with_rules() {
        // Arrange: `mystery` resolves to openai-compat with a model no table
        // knows; pricing reads the provider kind alone, so it still prices.
        let config = economics_config();
        let target = resolve_target(&config, "mystery");
        assert_eq!(
            target,
            Some(("openai-compat", "totally-unknown-model-xyz".to_string()))
        );
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: Some(1_000_000.0),
            c_after: Some(200_000),
            steady_state: false,
        };

        // Act
        let economics = build_economics(200_000, target, &args);

        // Assert: the openai-compat rules price it (an auto-cacher: the
        // one-time cost is C_after * (1 - rm), so K* == 180000 / 5000 == 36);
        // a huge hypothetical k BREAKs.
        assert_eq!(economics.rules, cache_rules("openai-compat"));
        assert!((economics.break_even_k.expect("priced") - 36.0).abs() < 1e-4);
        let verdict = economics.verdict.expect("k was supplied");
        assert_eq!(
            verdict,
            GateDecision::Break {
                delta_tokens: 50_000
            }
        );
    }

    #[test]
    fn build_economics_no_target_prices_as_sentinel_keep_insufficient_data() {
        // Arrange: an alias that resolves to no target offline (None).
        let config = economics_config();
        let target = resolve_target(&config, "no-such-alias-and-no-default");
        assert_eq!(target, None);
        let args = ProjectionArgs {
            hypothetical_d: Some(10_000),
            hypothetical_k: Some(999.0),
            c_after: None,
            steady_state: false,
        };

        // Act: C defaults from the report total; C_after defaults to C.
        let economics = build_economics(80_000, target, &args);

        // Assert: unpriced -- no rules, no K*, KEEP/insufficient.
        assert_eq!(economics.rules, None);
        assert_eq!(economics.provider_kind, None);
        assert_eq!(economics.break_even_k, None);
        assert_eq!(economics.candidate.c, 80_000);
        assert_eq!(economics.candidate.c_after, 80_000, "c_after defaults to C");
        assert_eq!(
            economics.verdict,
            Some(GateDecision::Keep {
                reason: KeepReason::InsufficientData
            })
        );
    }

    #[test]
    fn build_economics_gemini_target_is_unpriced_keep_insufficient_data() {
        // Arrange: `flash` resolves to a gemini provider, a kind with no
        // cache rules.
        let config = economics_config();
        let target = resolve_target(&config, "flash");
        assert_eq!(target, Some(("gemini", "gemini-2.5-flash".to_string())));
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: Some(1_000_000.0),
            c_after: Some(200_000),
            steady_state: false,
        };

        // Act
        let economics = build_economics(200_000, target, &args);

        // Assert: the target resolved, yet nothing prices it.
        assert_eq!(economics.provider_kind.as_deref(), Some("gemini"));
        assert_eq!(economics.rules, None);
        assert_eq!(economics.break_even_k, None);
        assert_eq!(
            economics.verdict,
            Some(GateDecision::Keep {
                reason: KeepReason::InsufficientData
            })
        );
    }

    #[test]
    fn render_economics_shows_the_rules_and_break_even_or_the_unpriced_path() {
        let config = economics_config();
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: None,
            c_after: Some(200_000),
            steady_state: false,
        };
        let rows: &[(&str, &str, &[&str])] = &[
            (
                "anthropic-api prices from its kind's rules",
                "heavy",
                &[
                    "target: anthropic-api / claude-opus-4-8\n",
                    "cache rules: write 1.25x  read 0.1x  ttl 300s  min prefix 4096 tokens\n",
                    "break-even K* = 50.00 future reuses",
                ],
            ),
            (
                "openai-compat flags the auto-cacher",
                "mystery",
                &[
                    "cache rules: write 1x  read 0.1x  ttl 300s  min prefix 4096 tokens  (auto-cached)\n",
                    "break-even K* = 36.00 future reuses",
                ],
            ),
            (
                "gemini has no rules and prints the unpriced path",
                "flash",
                &[
                    "target: gemini / gemini-2.5-flash\n",
                    "cache rules: unpriced (no cache rules for this target)\n",
                    "break-even K*: n/a (insufficient data)",
                ],
            ),
        ];

        for (name, alias, expected) in rows {
            let economics = build_economics(200_000, resolve_target(&config, alias), &args);
            let rendered = render_economics(&economics);
            for line in *expected {
                assert!(
                    rendered.contains(line),
                    "{name}: missing {line:?} in:\n{rendered}"
                );
            }
            assert!(
                !rendered.contains("tier"),
                "{name}: no TTL tier in:\n{rendered}"
            );
        }
    }

    #[test]
    fn build_report_leaves_economics_none_for_backward_compatible_output() {
        // The no-flag invocation path: build_report never populates economics,
        // so the rendered report is identical to before this feature.
        let req = ChatRequest {
            model: "claude-opus-4-8".into(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            messages: vec![user_text("hi")].into(),
            ..Default::default()
        };

        let report = build_report(&req, Some(true), true, true);

        assert_eq!(report.economics, None);
    }

    #[test]
    fn run_level_guard_renders_no_economics_section_without_hypothetical_d() {
        // Arrange: mirror the run-level path for a priced target but with NO
        // --hypothetical-d. This exercises the same guard `run` applies before
        // rendering (the build_report-level test only covers build_report).
        let req = ChatRequest {
            model: "claude-opus-4-8".into(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            messages: vec![user_text("hi")].into(),
            ..Default::default()
        };
        let projection = ProjectionArgs {
            hypothetical_d: None,
            hypothetical_k: None,
            c_after: None,
            steady_state: false,
        };
        let mut report = build_report(&req, Some(true), true, true);

        // Act: the same run-level guard -- economics stays None without -d.
        if projection.hypothetical_d.is_some() {
            report.economics = Some(build_economics(
                report.total.approx_tokens,
                Some(("anthropic-api", "claude-opus-4-8".to_string())),
                &projection,
            ));
        }
        let out = render_report("heavy", &report);

        // Assert: the legacy sections render, but nothing from the economics
        // projection leaks into the output.
        assert!(
            out.contains("size breakdown"),
            "expected legacy report: {out}"
        );
        assert!(
            !out.contains("cache-break economics"),
            "economics header must be absent: {out}"
        );
        assert!(!out.contains("break-even"), "no break-even line: {out}");
        assert!(!out.contains("verdict"), "no verdict line: {out}");
    }

    #[test]
    fn build_economics_finite_k_but_below_min_prefix_keeps_below_floor() {
        // Arrange: an anthropic-api target (min_prefix 4096). A small
        // candidate (d=900, c=1500) leaves remaining 600 < 4096
        // -- a finite K* still exists, but the cut is permanently unreachable.
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        assert_eq!(
            target,
            Some(("anthropic-api", "claude-opus-4-8".to_string()))
        );
        let args = ProjectionArgs {
            hypothetical_d: Some(900),
            hypothetical_k: Some(1_000_000.0),
            c_after: Some(1_500),
            steady_state: false,
        };

        // Act
        let economics = build_economics(1_500, target, &args);

        // Assert: finite K* (the math is well-defined) AND a hard KEEP below
        // the cacheable floor even at a wildly favorable k.
        assert!(
            economics.break_even_k.is_some(),
            "a finite K* should still be computed: {:?}",
            economics.break_even_k
        );
        let verdict = economics.verdict.expect("k was supplied");
        assert_eq!(
            verdict,
            GateDecision::Keep {
                reason: KeepReason::BelowMinPrefix
            }
        );
        // The rendered output shows the K* number AND the floor reason.
        let k_star = economics.break_even_k.expect("finite K*");
        let rendered_k = format!("{k_star:.2}");
        assert!(
            !rendered_k.is_empty(),
            "K* should render to a number: {rendered_k}"
        );
        let rendered_verdict = describe_verdict(&verdict);
        assert!(
            rendered_verdict.contains("below cacheable floor"),
            "verdict should cite the floor: {rendered_verdict}"
        );
    }

    #[test]
    fn build_economics_keep_net_negative_renders_net_negative_verdict() {
        // Arrange: anthropic-api target, K* == 50 (D=50k, C_after=200k).
        // A hypothetical k just BELOW the threshold is net-negative to break.
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: Some(49.9999),
            c_after: Some(200_000),
            steady_state: false,
        };

        // Act
        let economics = build_economics(200_000, target, &args);

        // Assert: a KEEP/NetNegative verdict whose rendered text conveys the
        // net-negative reason.
        let verdict = economics.verdict.expect("k was supplied");
        assert_eq!(
            verdict,
            GateDecision::Keep {
                reason: KeepReason::NetNegative
            }
        );
        let rendered = describe_verdict(&verdict);
        assert!(
            rendered.contains("net-negative"),
            "verdict should convey net-negative: {rendered}"
        );
    }

    // -- steady-state trim projection ---------------------------------

    /// A bulky tool_result message carrying a large JSON-string payload, using
    /// the same canonical `KnownContentPart::ToolResult` shape the shared
    /// `test_utils` builders use.
    fn bulky_tool_result(payload: &str) -> Message {
        tool_result_msg(json!(payload), None)
    }

    /// An assistant tool_use turn carrying a large JSON-string input, the
    /// canonical `KnownContentPart::ToolUse` shape.
    fn bulky_tool_use(payload: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "search".into(),
                input: json!(payload),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Build a long tool-heavy request that clears the steady-state trigger:
    /// head + many bulky tool turns + a small recent tail.
    fn long_tool_heavy_request() -> ChatRequest {
        let payload = "z".repeat(48_000); // ~12k tokens each at 4 bytes/token
        let mut messages = vec![user_text("head one"), user_text("head two")];
        for _ in 0..6 {
            messages.push(bulky_tool_use(&payload));
            messages.push(bulky_tool_result(&payload));
        }
        for i in 0..6 {
            messages.push(user_text(&format!("recent {i}")));
        }
        ChatRequest {
            model: "claude-opus-4-8".into(),
            messages: messages.into(),
            ..Default::default()
        }
    }

    #[test]
    fn build_steady_state_economics_renders_real_candidate_for_long_request() {
        // Arrange: an anthropic-api target and a long
        // tool-heavy request the deterministic trimmer can cut.
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        let req = long_tool_heavy_request();
        let args = ProjectionArgs {
            hypothetical_d: None,
            hypothetical_k: Some(1_000.0),
            c_after: None,
            steady_state: true,
        };

        // Act
        let economics = build_steady_state_economics(&req, target, &args, &config.trim);

        // Assert: a real cut was proposed, a live K* exists, and the rendered
        // projection surfaces the would-trim line + the candidate.
        assert_eq!(economics.steady_state_would_trim, Some(true));
        assert!(
            economics.candidate.d > 0,
            "real candidate should free tokens"
        );
        assert!(economics.rules.is_some());
        assert!(economics.break_even_k.is_some());
        let rendered = render_economics(&economics);
        assert!(
            rendered.contains("steady-state trim: would-trim yes"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains(&format!("D={}", economics.candidate.d)),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn build_steady_state_economics_reports_no_trim_for_short_request() {
        // Arrange: a tiny request well below the trigger -- the trimmer declines.
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        let req = ChatRequest {
            model: "claude-opus-4-8".into(),
            messages: vec![user_text("hi"), user_text("there")].into(),
            ..Default::default()
        };
        let args = ProjectionArgs {
            hypothetical_d: None,
            hypothetical_k: Some(1_000.0),
            c_after: None,
            steady_state: true,
        };

        // Act
        let economics = build_steady_state_economics(&req, target, &args, &config.trim);

        // Assert: would-trim no, a zero candidate, and a KEEP verdict.
        assert_eq!(economics.steady_state_would_trim, Some(false));
        assert_eq!(economics.candidate.d, 0);
        assert_eq!(
            economics.verdict,
            Some(GateDecision::Keep {
                reason: KeepReason::NoCandidate
            })
        );
        let rendered = render_economics(&economics);
        assert!(
            rendered.contains("steady-state trim: would-trim no"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn hypothetical_d_path_leaves_steady_state_field_none() {
        // The --hypothetical-d path must not set the steady-state would-trim
        // field, so its rendered output is unchanged (no would-trim line).
        let config = economics_config();
        let target = resolve_target(&config, "heavy");
        let args = ProjectionArgs {
            hypothetical_d: Some(50_000),
            hypothetical_k: None,
            c_after: Some(200_000),
            steady_state: false,
        };

        let economics = build_economics(200_000, target, &args);

        assert_eq!(economics.steady_state_would_trim, None);
        let rendered = render_economics(&economics);
        assert!(
            !rendered.contains("steady-state trim:"),
            "hypothetical-d output must not show a steady-state line: {rendered}"
        );
    }

    /// The TOML text `economics_config()` parses.
    fn economics_config_toml() -> String {
        format!(
            r#"
version = {CURRENT_CONFIG_VERSION}
[providers.anthro]
kind = "anthropic-api"
api_key_ref = "literal:placeholder"

[providers.compat]
kind = "openai-compat"
base_url = "https://api.example.invalid/v1"
api_key_ref = "literal:placeholder"

[models.opus]
provider = "anthro"
upstream = "claude-opus-4-8"

[providers.gem]
kind = "gemini"
api_key_ref = "literal:placeholder"

[models.mystery_model]
provider = "compat"
upstream = "totally-unknown-model-xyz"

[models.gem_model]
provider = "gem"
upstream = "gemini-2.5-flash"

[aliases]
heavy = "opus"
mystery = "mystery_model"
flash = "gem_model"
"#
        )
    }
}
