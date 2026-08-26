//! Shared bits across the two replay test drivers (`replay_egress.rs`
//! and `replay_ingress.rs`): the captured root locator, the
//! loader-vector to `HeaderMap` bridge, the per-fixture outcome enum,
//! the ingress-adapter lookup keyed on `meta.ingress_kind`, the
//! model-enrichment reconstruction + residual skip-reason helper, and
//! the value-bounded divergence summary the drivers report failures
//! through.
//!
//! These were duplicated verbatim across both test files until they
//! grew in lockstep one too many times. Hoisting them removes the
//! "edit one, forget the other" failure mode without introducing a
//! provider-registry abstraction (over-engineering for three
//! providers; see the cross-reference comments in the egress / ingress
//! match arms).

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::openai::OpenAiIngress;
use routectl_cli::ingress::openai_responses::ResponsesIngress;
use routectl_core::{ChatRequest, Provider};
use routectl_router::ResolvedModel;
use serde_json::Value;

use super::json_diff::{Divergence, DivergenceKind, diff_all};
use super::loader::Fixture;

/// Default replay-fixture root. Per-contributor, local, gitignored at
/// the repo policy level. Populated by `scripts/capture_fixtures.sh`.
/// `discover_fixtures` returns an empty vector when the directory is
/// empty, which keeps the replay tests passing on a fresh checkout
/// before any fixtures have been captured.
pub fn captured_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/captured")
}

/// Build a `HeaderMap` from the `(name, value)` pairs persisted in a
/// fixture's `*.headers.json`. A pair the `http` crate refuses to
/// accept is logged to stderr (with the offending name) and skipped --
/// this is a fixture-authoring bug we want to surface, but failing the
/// whole test on it would mask the comparator output that pinpoints
/// the real wire-shape issue.
pub fn headers_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in pairs {
        let parsed_name = if let Ok(n) = HeaderName::from_bytes(name.as_bytes()) {
            n
        } else {
            eprintln!(
                "[replay] dropping malformed header name `{name}` (header value not echoed; \
                 fix the fixture's *.headers.json)",
            );
            continue;
        };
        let parsed_value = if let Ok(v) = HeaderValue::from_str(value) {
            v
        } else {
            eprintln!(
                "[replay] dropping malformed header value on `{name}` \
                 (value not echoed; fix the fixture's *.headers.json)",
            );
            continue;
        };
        out.insert(parsed_name, parsed_value);
    }
    out
}

/// Outcome of one fixture's run. `Skipped` carries a human-readable
/// reason so the test driver can surface it as an info log rather than
/// a failure. `Asserted` means the fixture was exercised end-to-end.
pub enum FixtureOutcome {
    Asserted,
    Skipped(String),
}

/// Model substrings whose replay needs an overlay value that NOTHING in
/// the fixture pins and the harness cannot derive -- the residual of the
/// former blanket enrichment filter.
///
/// The rule for membership: a knob stays here only when its value is a
/// free operator choice in `[models.X]` that `meta.json` does not record.
/// A knob the harness can reconstruct from the fixture (see
/// [`replay_resolved_model`]) is NOT grounds for a skip.
///
/// Today the sole member is the DeepSeek reasoning family, which depends
/// on `history_reasoning`: `Auto` strips outgoing reasoning history and
/// `Preserve` emits it, the captured body shows which one was in force,
/// and no fixture field says which the operator configured. Guessing
/// would turn a config difference into a wire-shape failure.
///
/// Matching is substring + case-insensitive so capture-rig variants
/// (`deepseek-v4`, `deepseek/deepseek-chat`, ...) all hit.
pub const ENRICHMENT_DEPENDENT_MODELS: &[&str] = &["deepseek"];

/// Model substrings that get `supports_adaptive_thinking = true` when the
/// replay rebuilds their enrichment.
///
/// Production does NOT derive this from the model id -- it is an explicit
/// `[models.X]` opt-in, because Anthropic's adaptive rollout has no clean
/// naming pattern. The replay can still reconstruct it per fixture: the
/// adaptive generation REJECTS the legacy `thinking.type=enabled` shape
/// with a 400, and the capture rig only writes a fixture for a request
/// whose trace shows a successful upstream response, so a fixture on one
/// of these models was necessarily captured with the flag on.
pub const ADAPTIVE_THINKING_MODELS: &[&str] = &["opus-4-7", "opus-4-8", "sonnet-4-7", "sonnet-4-8"];

fn model_matches(model: &str, needles: &[&str]) -> bool {
    let lc = model.to_ascii_lowercase();
    needles.iter().any(|needle| lc.contains(needle))
}

/// Residual enrichment filter: skip a fixture whose model needs an
/// overlay value the replay can neither read off the fixture nor derive.
/// Returns `None` for everything [`replay_resolved_model`] can rebuild.
pub fn enrichment_skip_reason(fixture: &Fixture) -> Option<String> {
    let model = fixture.meta.model.as_deref()?;
    if !model_matches(model, ENRICHMENT_DEPENDENT_MODELS) {
        return None;
    }
    Some(format!(
        "model `{model}` needs a `history_reasoning` overlay that no fixture field pins; \
         replay cannot tell a captured `Preserve` from a captured `Auto`",
    ))
}

/// Provider binding for the enrichment-only [`ResolvedModel`] below. The
/// resolved model exists to carry the four per-model knobs into
/// [`with_replay_enrichment`]; nothing dispatches through this provider,
/// so every trait method that would reach a network is unreachable.
struct EnrichmentOnlyProvider;

#[async_trait::async_trait]
impl Provider for EnrichmentOnlyProvider {
    fn id(&self) -> &'static str {
        "replay-enrichment-only"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<serde_json::Value> {
        unreachable!("replay enrichment never normalizes through this provider")
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        unreachable!("replay enrichment never normalizes through this provider")
    }
    async fn complete(&self, _: ChatRequest) -> routectl_core::Result<routectl_core::ChatResponse> {
        unreachable!("replay enrichment never dispatches through this provider")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        unreachable!("replay enrichment never dispatches through this provider")
    }
}

/// Rebuild the `ResolvedModel` the router would have resolved for this
/// fixture's model, so the replay's canonical request carries the same
/// per-model knobs the egress saw at capture time.
///
/// Only the four knobs the egress reads off `routectl_internal` are
/// reconstructed; every other field stays at its `ResolvedModel::new`
/// default, which is also the config default:
///
/// - `supports_adaptive_thinking`: derived per [`ADAPTIVE_THINKING_MODELS`].
/// - `max_thinking_budget`: left at `0` (no operator cap), the default an
///   unconfigured `[models.X]` produces.
/// - `reasoning_dialect` / `history_reasoning`: left at `None` (fall back
///   to the egress default). The one family whose captured body depends
///   on a non-default value is filtered by [`enrichment_skip_reason`]
///   instead of guessed at here.
pub fn replay_resolved_model(fixture: &Fixture) -> ResolvedModel {
    let model = fixture.meta.model.as_deref().unwrap_or_default();
    let provider: Arc<dyn Provider> = Arc::new(EnrichmentOnlyProvider);
    ResolvedModel::new("replay", "replay", provider, model)
        .with_supports_adaptive_thinking(model_matches(model, ADAPTIVE_THINKING_MODELS))
        .with_max_thinking_budget(0)
}

/// Project a resolved model's per-model knobs onto a freshly parsed
/// canonical request, mirroring the four fields the router's dispatch-time
/// overlay writes before the egress sees the request. Without this the
/// replay drives every fixture through a `routectl_internal` at its
/// library-consumer defaults, and a fixture captured under any non-default
/// knob diverges for a reason that is not a bug.
///
/// Reads the SAME `ResolvedModel` fields the router overlay reads, so a
/// knob added there and not here shows up as a replay divergence rather
/// than as silent drift.
pub fn with_replay_enrichment(model: &ResolvedModel, mut req: ChatRequest) -> ChatRequest {
    req.routectl_internal.supports_adaptive_thinking = model.supports_adaptive_thinking;
    req.routectl_internal.max_thinking_budget = model.max_thinking_budget;
    req.routectl_internal.effort_levels = model.effort_levels.clone();
    req.routectl_internal.reasoning_dialect = model.reasoning_dialect.map(Into::into);
    req.routectl_internal.history_reasoning = model.history_reasoning.map(Into::into);
    req
}

static ANTHROPIC_INGRESS: AnthropicIngress = AnthropicIngress;
static OPENAI_INGRESS: OpenAiIngress = OpenAiIngress;
static OPENAI_RESPONSES_INGRESS: ResponsesIngress = ResponsesIngress;

/// Resolve `meta.ingress_kind` to the adapter that parsed the captured
/// inbound body. The rig writes the `IngressAdapter::id()` vocabulary
/// verbatim, so this is a lookup and not a mapping table.
///
/// `Ok(None)` for the EMPTY value, which the meta contract uses for "the
/// capture could not observe the ingress token" -- the caller skips that
/// fixture with a reason. An unpinned fixture cannot be recaptured, and
/// the loader's stance on an unpinned field is to refuse the individual
/// fixture, not the corpus; defaulting it to `anthropic` would be a
/// silent wrong answer for a fixture captured on any other dialect.
///
/// An unknown NON-empty value is a fixture-authoring bug or a vocabulary
/// drift and fails closed naming the value, matching
/// `replay_egress.rs::normalize_for_kind`'s treatment of `provider_kind`.
pub fn ingress_for_kind(kind: &str) -> Result<Option<&'static dyn IngressAdapter>, String> {
    match kind {
        "" => Ok(None),
        "anthropic" => Ok(Some(&ANTHROPIC_INGRESS)),
        "openai" => Ok(Some(&OPENAI_INGRESS)),
        "openai-responses" => Ok(Some(&OPENAI_RESPONSES_INGRESS)),
        other => Err(format!("unknown ingress_kind `{other}`")),
    }
}

/// Parse a fixture's captured inbound request through the ingress the
/// fixture was captured on, then overlay the per-model enrichment the
/// router would have applied. `Ok(None)` when the fixture pins no
/// `ingress_kind`; the caller turns that into a skip.
pub fn parse_enriched_canonical(fixture: &Fixture) -> Result<Option<ChatRequest>, String> {
    let Some(ingress) = ingress_for_kind(&fixture.meta.ingress_kind)? else {
        return Ok(None);
    };
    let headers = headers_from_pairs(&fixture.ingress_request_headers);
    let canonical = ingress
        .parse_request(
            &headers,
            &serde_json::to_vec(&fixture.ingress_request).expect("wire serializes"),
        )
        .map_err(|e| format!("{} ingress parse_request failed: {e}", ingress.id()))?;
    Ok(Some(with_replay_enrichment(
        &replay_resolved_model(fixture),
        canonical,
    )))
}

/// Skip reason for a fixture that pins no `ingress_kind`. One phrasing
/// shared by both drivers so the two cannot describe the same skip
/// differently.
pub fn unpinned_ingress_skip_reason() -> String {
    "meta.ingress_kind is empty; the capture did not observe the ingress dialect".to_string()
}

// ---------------------------------------------------------------------------
// Bounded divergence reporting
// ---------------------------------------------------------------------------

/// Longest string value echoed VERBATIM into a failure message.
///
/// A replay failure's diagnostic is the PATH and the KIND; the value is
/// corroboration. Captured fixtures are real prompt traffic -- third-party
/// source, addresses, whatever the operator was working on -- and a test
/// log is not a confidential sink: it lands in CI output, terminal
/// scrollback, and pasted bug reports. Echoing a whole subtree turned one
/// failing fixture into a six-figure-byte log line dumping content
/// unrelated to the wire shape under test.
///
/// A string longer than this is reported by LENGTH ONLY -- never as a
/// prefix. A truncated prefix is still disclosure (the leading bytes of a
/// prompt are exactly the system preamble), so the cap is a gate on
/// whether the value prints at all, not a substring length. Short values
/// are the ones where the value IS the diagnostic (`"sonnet"` vs
/// `"claude-sonnet-4-5"`), and those are wire identifiers, not prose.
const MAX_VERBATIM_VALUE_CHARS: usize = 48;

/// Maximum divergences enumerated per fixture. A body-wide misalignment
/// produces one per element; the first handful identify the transform,
/// and the tail is noise. The full count is always reported.
///
/// TRIAGE WARNING: on a fixture that hits this cap the log shows only the
/// leading divergences, so the log is NOT a complete inventory of which
/// PATH CLASSES a fixture diverges on -- a class that appears only past
/// the fifth divergence is invisible there. Counting classes by grepping
/// the log therefore undercounts. Per-class triage reads the fixtures and
/// calls [`bounded_body_diff`]'s underlying `diff_all` directly, which
/// returns the full set. The cap bounds the LOG, not the comparison.
const MAX_DIVERGENCES_REPORTED: usize = 5;

/// Terminal path segments whose values are wire IDENTIFIERS or enums --
/// never caller prose -- and are therefore safe to echo verbatim when
/// short.
///
/// An ALLOWLIST, deliberately, because the alternative fails open: a
/// denylist of prose-bearing names prints any field it has not heard of,
/// and a captured body carries forward-compat keys nobody enumerated
/// (`provider_extras` sweeps whatever the client sent). A wire-shape
/// divergence on a field outside this list is still fully diagnosable
/// from its path, kind, and value shape.
const VERBATIM_SAFE_LEAF_SEGMENTS: &[&str] = &[
    "model",
    "role",
    "type",
    "effort",
    "stream",
    "name",
    "id",
    "stop_reason",
    "finish_reason",
    "object",
    "format",
    "anthropic_version",
    "service_tier",
];

/// Whether a divergence path's value may be echoed verbatim. Reads the
/// LAST path segment (the leaf field name), with array indices stripped,
/// so `messages[3].role` is judged as `role`.
fn is_verbatim_safe_path(path: &str) -> bool {
    let leaf = path.rsplit('.').next().unwrap_or(path);
    let leaf = leaf.split('[').next().unwrap_or(leaf);
    VERBATIM_SAFE_LEAF_SEGMENTS.contains(&leaf.to_ascii_lowercase().as_str())
}

/// Render one side of a divergence as its TYPE plus a size measure.
///
/// Containers NEVER render their contents -- an object shows its key count
/// and names, an array its length. A string renders verbatim only when its
/// leaf field is on [`VERBATIM_SAFE_LEAF_SEGMENTS`] AND it is short (per
/// [`MAX_VERBATIM_VALUE_CHARS`]); every other string reports length only.
/// Object KEYS are wire field names rather than payload, so naming a few
/// aids diagnosis without echoing content.
fn summarize_value(value: Option<&Value>, allow_verbatim: bool) -> String {
    let Some(value) = value else {
        return "<absent>".to_string();
    };
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let len = s.chars().count();
            if allow_verbatim && len <= MAX_VERBATIM_VALUE_CHARS {
                format!("string(len={len}, {s:?})")
            } else {
                format!("string(len={len}, elided)")
            }
        }
        Value::Array(a) => format!("array(len={})", a.len()),
        Value::Object(o) => {
            let mut keys: Vec<&str> = o.keys().map(String::as_str).take(4).collect();
            if o.len() > keys.len() {
                keys.push("...");
            }
            format!("object(keys={}, [{}])", o.len(), keys.join(", "))
        }
    }
}

/// One-line, value-bounded rendering of a divergence: full path, full
/// kind, summarized sides.
fn summarize_divergence(divergence: &Divergence) -> String {
    let noun = if divergence.path.ends_with(']') {
        "element mismatch"
    } else {
        "key mismatch"
    };
    let path = if divergence.path.is_empty() {
        "<root>"
    } else {
        &divergence.path
    };
    let verbatim = is_verbatim_safe_path(&divergence.path);
    match divergence.kind {
        DivergenceKind::Changed => format!(
            "value mismatch at {path}: actual={}, expected={}",
            summarize_value(divergence.actual.as_ref(), verbatim),
            summarize_value(divergence.expected.as_ref(), verbatim)
        ),
        DivergenceKind::Added => format!(
            "{noun} at {path}: present in actual only, actual={}",
            summarize_value(divergence.actual.as_ref(), verbatim)
        ),
        DivergenceKind::Removed => format!(
            "{noun} at {path}: present in expected only, expected={}",
            summarize_value(divergence.expected.as_ref(), verbatim)
        ),
    }
}

/// Compare two bodies and return a bounded failure summary, or `None`
/// when they agree structurally.
///
/// Deliberately NOT `assert_json_equal_structural`: that renders both
/// sides in full via `Display`, which is what leaked whole prompt bodies
/// into the log. This drives the same `diff_all` walk and formats the
/// result under the caps above. The path and kind survive intact -- they
/// are the diagnostic and carry no payload.
pub fn bounded_body_diff(
    actual: &Value,
    expected: &Value,
    ignore_paths: &[&str],
) -> Option<String> {
    let divergences = diff_all(actual, expected, ignore_paths);
    if divergences.is_empty() {
        return None;
    }
    let shown: Vec<String> = divergences
        .iter()
        .take(MAX_DIVERGENCES_REPORTED)
        .map(summarize_divergence)
        .collect();
    let mut summary = format!("{} divergence(s)", divergences.len());
    if divergences.len() > shown.len() {
        summary.push_str(&format!(" (first {} shown)", shown.len()));
    }
    summary.push_str(": ");
    summary.push_str(&shown.join("; "));
    Some(summary)
}

// ---------------------------------------------------------------------------
// System-turn lift detection
// ---------------------------------------------------------------------------

/// Paths whose divergence indicates the system-turn lift rather than a
/// wire-shape regression. The lift removes `role:"system"` turns from the
/// middle of `messages[]`, which positional array pairing reports at
/// `messages[N]` and every path beneath it.
fn is_messages_path(path: &str) -> bool {
    path == "messages" || path.starts_with("messages[")
}

/// Whether EVERY divergence between the two bodies falls inside
/// `messages[]`, i.e. the fixture's only unexplained difference is the
/// one the system-turn lift produces.
///
/// The lift's normalizer is not built here (it is the conservation
/// harness's surface, and `json_diff`'s own docs spell out why it must be
/// a pre-diff transform on the caller's inputs rather than a whitelist).
/// Until it exists this driver cannot distinguish a lift-shifted array
/// from a real `messages[]` regression, so it declines to judge the
/// fixture instead of asserting a comparison it knows is misaligned.
///
/// Scoped deliberately narrowly: a divergence ANYWHERE outside
/// `messages[]` still fails, so a `model` or `output_config` regression is
/// never swallowed by a fixture that also happens to be lift-affected.
///
/// That narrowness decides most of the corpus. Measured on the
/// anthropic-ingress lane, 133 of 250 fixtures diverge only inside
/// `messages[]` and skip here; the other 115 carry a `model` or
/// `output_config` divergence alongside and fail. Multi-class fixtures are
/// the norm rather than the exception, so widening this predicate to
/// "diverges at all in `messages[]`" would mute the majority of the real
/// findings -- see the divergence-class list in `docs/REPLAY-FIXTURES.md`.
pub fn diverges_only_in_messages(actual: &Value, expected: &Value, ignore_paths: &[&str]) -> bool {
    let divergences = diff_all(actual, expected, ignore_paths);
    !divergences.is_empty() && divergences.iter().all(|d| is_messages_path(&d.path))
}

/// Skip reason naming the prerequisite a lift-affected fixture is waiting
/// on. Names the missing normalizer explicitly so the skip reads as a
/// blocked assertion, never as a pass.
pub fn system_turn_lift_skip_reason(divergence_count: usize) -> String {
    format!(
        "all {divergence_count} divergence(s) are inside messages[]; the system-turn lift \
         shifts array positions and the pre-diff normalizer that realigns them is not yet \
         implemented, so this fixture's body cannot be compared",
    )
}

/// Count divergences for the skip-reason message. Separate from
/// [`diverges_only_in_messages`] so the reason can report the real number
/// rather than a recomputed guess.
pub fn divergence_count(actual: &Value, expected: &Value, ignore_paths: &[&str]) -> usize {
    diff_all(actual, expected, ignore_paths).len()
}
