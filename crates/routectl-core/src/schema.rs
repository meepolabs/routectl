//! OpenRouter-normalized request/response schema.
//!
//! Shape reference: <https://openrouter.ai/docs/guides/best-practices/reasoning-tokens>
//!
//! Key design choice: routectl's outward schema mirrors OpenRouter so
//! any client that speaks OpenRouter speaks routectl. Reasoning is first-class:
//! `reasoning` config in request, `reasoning_details` array in response.
//!
//! v0.4.0 extension: the canonical now carries Anthropic-shape
//! features (cache_control on every block, top-level system, anthropic_beta,
//! cache usage stats) so an Anthropic-in / Anthropic-out and Anthropic-in /
//! Bedrock-Invoke-out request round-trips losslessly. Typed `ContentPart`,
//! `SystemContent`, and `ToolDef` replace the earlier `Vec<Value>`
//! passthroughs. See `crate::content_part`, `crate::system_content`,
//! `crate::tool_def`, `crate::cache_control`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache_control::CacheControl;
use crate::content_part::ContentPart;
use crate::system_content::SystemContent;
use crate::tool_def::ToolDef;

/// Deserialize the OpenAI `stop` field, which the spec allows as EITHER
/// a bare string (`"###"`) OR an array of strings (`["A","B"]`). serde's
/// derive only accepts the array shape, so a bare string would fail and
/// 400 the whole request before any egress sees it. Normalizes both to
/// `Option<Vec<String>>`: a string becomes a one-element vec, an array
/// passes through, and absent/null stays `None`.
fn deserialize_stop<'de, D>(deserializer: D) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        One(String),
        Many(Vec<String>),
    }

    let opt = Option::<StringOrVec>::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        StringOrVec::One(s) => vec![s],
        StringOrVec::Many(many) => many,
    }))
}

/// The canonical (OpenRouter-normalized) chat completion request. Every
/// ingress parses into this shape and every egress reads from it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Target model identifier (alias or provider-native id).
    pub model: String,
    /// Conversation turns in order.
    ///
    /// Held behind `Arc<[Message]>` so the per-chain-entry `req.clone()`
    /// on the dispatch path is O(1) (a refcount bump), not a deep copy of
    /// every message body. Contract:
    /// 1. Cloning a `ChatRequest` shares this buffer; it does not copy it.
    /// 2. `Arc::make_mut(&mut req.messages)` is THE copy-on-write seam for
    ///    every in-place mutation. It pays one body copy only when the
    ///    buffer is shared (refcount > 1), keeping other clones pristine.
    ///    Mutate through `make_mut` exclusively.
    /// 3. Never reassign a freshly rebuilt `Arc::from(vec)` where a
    ///    `make_mut` edit is intended: doing so silently breaks the CoW
    ///    seam and forces an allocation on every call.
    pub messages: Arc<[Message]>,

    /// Top-level system prompt. Anthropic accepts a flat string or an
    /// array of typed text blocks with per-block `cache_control`. The
    /// OpenAI ingress lifts `Role::System` messages into this field at
    /// parse time; egresses read it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,

    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Maximum tokens to generate in the completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// OpenAI accepts `stop` as EITHER a bare string OR an array of
    /// strings; a bare string would otherwise serde-fail and 400 the
    /// whole request at the ingress. `deserialize_stop` normalizes both
    /// to `Vec<String>`. Serializes back out as an array (unchanged
    /// wire-out for the array form).
    #[serde(
        default,
        deserialize_with = "deserialize_stop",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop: Option<Vec<String>>,
    /// Whether to stream the response as SSE chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Number of completions to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// Sampling seed for reproducible outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Whether to return token log-probabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// Number of top alternatives to return per token when `logprobs` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    /// Per-token bias map applied to sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<Value>,
    /// Presence penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Frequency penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Opaque end-user identifier forwarded upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Tool definitions. Typed `ToolDef::Custom` for canonical custom
    /// tools (with `cache_control`, `defer_loading`, `strict`); typed
    /// `ToolDef::Other(Value)` for OpenAI-shape function tools,
    /// Anthropic builtins, and any future shape (passthrough).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    /// Tool-selection directive, passed through verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Structured-output / response-format directive, passed through verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,

    /// Top-level cache breakpoint (auto-cache mode). Counts toward the
    /// 4-breakpoint cap. Anthropic-only; egresses without prompt caching
    /// drop with a `tracing::warn!`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,

    /// Body-level Anthropic beta flags (e.g. `context-1m-2025-08-07`).
    /// Egresses to Anthropic-shape upstreams (Anthropic API,
    /// Bedrock-Invoke) merge this into the body's `anthropic_beta` array.
    /// Distinct from the `anthropic-beta` HTTP header which is configured
    /// per provider via `extra_headers`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_beta: Vec<String>,

    /// Unified reasoning controls. Translated per-provider in `Provider::normalize_request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,

    /// Server-side chat-template kwargs (vLLM, DashScope, some NIM endpoints).
    /// Forwarded as-is to providers that accept them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<Value>,

    /// Long-tail provider knobs we don't normalize. Merged into upstream body verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extras: Option<Value>,

    /// Transport-internal carrier for resolved-model knobs that the
    /// dispatch layer hands to the egress without bouncing through the
    /// wire. Never serialized -- `#[serde(skip)]` keeps the field
    /// invisible to TOML/JSON.
    ///
    /// Despite the "transport-internal" framing, this also ferries
    /// resolved per-model CONFIG values (the openai-compat
    /// reasoning_dialect + history_reasoning, merged header_extras, the
    /// adaptive-thinking flag, etc.) -- not just opaque transport state.
    /// The router populates it from `ResolvedModel` right before calling
    /// `provider.complete(req)` / `provider.stream(req)` so the
    /// `Provider` trait signature stays stable across all five concrete
    /// providers. The reasoning_dialect + history_reasoning knobs moved
    /// from `[providers.X]` to `[models.X]` in v0.6.0; future per-model
    /// transport knobs land here too.
    #[serde(skip)]
    pub routectl_internal: RoutectlInternal,
}

/// Which ingress dialect produced this canonical request. `Library`
/// is the default for consumers that construct a `ChatRequest`
/// directly (no ingress in the loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RequestProvenance {
    /// Constructed directly, with no ingress in the loop.
    #[default]
    Library,
    /// Parsed by the Anthropic ingress.
    AnthropicIngress,
    /// Parsed by the OpenAI ingress.
    OpenaiIngress,
}

/// One preserved Responses `input[]` item plus the number of MODELED
/// (non-passthrough) input items that preceded it in the inbound array.
/// The Responses egress splices each entry back in after that many
/// modeled egress items so a preserved codex-only item keeps its
/// original conversation position instead of being shoved to the tail.
#[derive(Debug, Clone)]
pub struct ResponsesPassthroughItem {
    /// Count of modeled input items that appeared before this one in the
    /// inbound `input[]` array (the "modeled-prefix index").
    pub modeled_prefix: usize,
    /// The unmodeled Responses item, forwarded verbatim.
    pub item: Value,
}

/// Transport-internal carrier for resolved-model knobs the dispatch
/// layer hands to the egress without bouncing through the wire. In
/// practice it carries resolved per-model CONFIG values (reasoning
/// dialect, history-reasoning policy, merged header_extras,
/// adaptive-thinking flag) alongside any pure-transport state. See
/// `ChatRequest::routectl_internal` for the contract.
///
/// Hop 4 (final) of the per-model knob relay: a `[models.X]` value
/// reaches the egress here after passing through `ModelEntry` ->
/// `ResolvedModel` -> `DispatchTarget` in the `routectl-router` crate.
/// The router's `apply_layered_overlays` populates these fields right
/// before dispatch. Adding a knob the egress reads means editing all
/// four definitions; the relay exists because this crate (wire-
/// internal) and the config crate (TOML serde shape) stay decoupled,
/// and `reasoning_dialect` / `history_reasoning` even change enum type
/// at this boundary. The egress reads each field directly off this
/// struct, so the fields are flat (no shared sub-struct) by design.
///
/// Every field is `Option` so an egress can fall back to its own
/// `self.cfg.*` value when the carrier is empty (library consumers
/// constructing a `ChatRequest` directly never set the carrier).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RoutectlInternal {
    /// Per-model openai-compat reasoning dialect. `None` means the
    /// egress should fall back to its own `OpenAiCompatConfig`-side
    /// default (today: `ReasoningDialect::OpenAi`).
    pub reasoning_dialect: Option<crate::reasoning_dialect::ReasoningDialect>,
    /// Per-model openai-compat history-reasoning policy. `None` means
    /// fall back to the egress's own default.
    pub history_reasoning: Option<crate::reasoning_dialect::HistoryReasoning>,
    /// Merged header_extras map (provider + model, model-wins on key
    /// collision). The dispatch layer composes this and hands it to
    /// the egress; the egress's `build_headers` reads from here instead
    /// of `self.cfg.header_extras` so per-model headers reach the wire.
    /// `None` means the router was not in the loop (library consumer);
    /// the egress should fall back to its own `self.cfg.header_extras`.
    ///
    /// `anthropic-beta` is intentionally NOT in this map -- it rides on
    /// the canonical `ChatRequest.anthropic_beta` field and is composed
    /// from THREE sources (ingress lift + provider + model) by the
    /// dispatch layer. Keeping it out of `header_extras` here prevents
    /// double-handling by the Anthropic-API egress.
    pub header_extras: Option<std::collections::BTreeMap<String, String>>,
    /// Inbound `X-Claude-Code-*` headers captured by the Anthropic
    /// ingress (any header whose name, case-insensitive, starts with
    /// `x-claude-code-`). The Anthropic-API egress merges these into
    /// the outbound request for gateway cost attribution per the
    /// llm-gateway docs at <https://code.claude.com/docs/en/llm-gateway>.
    /// Other egresses ignore this. Order-preserving so multiple
    /// `X-Claude-Code-Agent-Id` headers (if a future shape ships them)
    /// are sent in inbound order. Empty when no matching headers were
    /// supplied; non-Anthropic ingresses (openai-compat) leave it empty.
    pub claude_code_headers: Vec<(String, String)>,

    /// Inbound `x-stainless-*` SDK fingerprint headers captured on the
    /// forwarded (pure-proxy) leg ONLY -- gated identically to
    /// [`Self::forwarded_bearer`] (the process's MITM seam nonce matches
    /// the inbound seam header AND a forwarded-credential provider is
    /// configured on the router). On that leg the Anthropic-API egress
    /// presents the CLIENT's real identity, so these client-supplied
    /// Stainless headers OVERRIDE routectl's minted cloak fingerprint
    /// (`default_claude_code_identity_headers`) on the outbound request.
    ///
    /// Deliberately a SEPARATE carrier from [`Self::claude_code_headers`]:
    /// that field is contractually `x-claude-code-*`-only (the egress
    /// iterates it as claude-code headers and the cloak `is_non_cc`
    /// heuristic scans it for `x-claude-code-session-id`), so overloading
    /// it would break those consumers. These are NON-secret SDK
    /// fingerprint values, so a plain `Vec` (no redacting wrapper) is
    /// correct. Order-preserving. Empty in own mode and for every
    /// non-forwarded path, keeping the carrier byte-identical to the
    /// pre-passthrough behavior.
    pub stainless_headers: Vec<(String, String)>,
    /// Whether the dispatched model supports adaptive (extended)
    /// thinking. Threaded through from `[models.X]
    /// supports_adaptive_thinking` via `ResolvedModel` ->
    /// `DispatchTarget`. Egresses that need to choose between a budget-
    /// tokens path and a flat enable/disable read this field.
    /// Defaults to `false` so library consumers constructing
    /// `ChatRequest` directly never see an unexpected budget-tokens path.
    pub supports_adaptive_thinking: bool,
    /// Operator-declared effort levels for this model (e.g.
    /// `["low", "medium", "high"]`). Threaded through from
    /// `[models.X] effort_levels` via `ResolvedModel` -> `DispatchTarget`.
    /// OpenAI-shape egresses clamp `req.reasoning.effort` to the nearest
    /// supported level before emitting. Empty means passthrough -- emit
    /// whatever the caller sent without validation.
    ///
    /// `Arc<[String]>` so cloning (once per dispatch attempt) is a
    /// refcount bump rather than a heap allocation. The default empty
    /// case uses `Arc::default()`, which is also zero-allocation.
    pub effort_levels: std::sync::Arc<[String]>,
    /// Maximum thinking-token budget the operator allows for this model,
    /// in tokens. Zero means no operator cap -- the egress applies only
    /// Anthropic's own `[1024, max_tokens-1]` window. Non-zero values
    /// are forwarded as the ceiling for the legacy `Enabled` budget
    /// negotiation; the budget is clamped DOWN to this value before
    /// Anthropic's window clamp runs.
    ///
    /// Threaded through from `[models.X] max_thinking_budget` via
    /// `ResolvedModel` -> `DispatchTarget`.
    pub max_thinking_budget: u32,
    /// Operator-declared per-model `max_tokens` ceiling resolved by
    /// the router from `[models.X].max_output_tokens`. Only consumed
    /// by Anthropic-shape egresses (anthropic-api, bedrock-invoke);
    /// other egresses forward `req.max_tokens` omission cleanly
    /// (good-translator principle: do not inject where the upstream
    /// already handles it).
    ///
    /// Sentinel `0` means "no per-model override"; the consuming
    /// egress falls through to its hardcoded baseline (64000).
    pub max_output_tokens: u32,

    /// Operator-configured `anthropic-beta` flags composed by the
    /// dispatch layer from provider `header_extras["anthropic-beta"]`
    /// plus model `header_extras["anthropic-beta"]` -- the
    /// client/ingress-supplied betas are deliberately excluded.
    ///
    /// Invariant: operator betas bypass the per-provider `allowed_betas`
    /// allowlist unconditionally. `allowed_betas` gates only the betas a
    /// client requests; an operator who pins a beta in config has
    /// already opted in, so the Anthropic-API egress re-adds these as a
    /// floor after filtering the client-supplied set.
    ///
    /// Empty for library consumers that construct a `ChatRequest`
    /// without the router; in that path the egress's own
    /// `cfg.header_extras` provider floor is the only operator source.
    pub operator_betas: Vec<String>,

    /// Which ingress dialect produced this canonical request. Set by the
    /// ingress adapter at parse time; defaults to `Library` for consumers
    /// that build a `ChatRequest` directly (no ingress in the loop).
    /// Pure observability metadata -- never serialized to any upstream.
    pub provenance: RequestProvenance,

    /// Responses `input[]` items whose `type` this hub does not model,
    /// captured verbatim by the OpenAI Responses ingress. The OpenAI
    /// Responses egress re-emits each entry unchanged so a codex
    /// multi-turn conversation round-trips its native item kinds
    /// (`local_shell_call`, `custom_tool_call(_output)`,
    /// `tool_search_call`, `agent_message`, ...) instead of losing them
    /// on replay. This is the item-level analogue of
    /// `ContentPart::Other`, which preserves unmodeled CONTENT blocks.
    ///
    /// Preserve-and-passthrough only: ONLY the Responses egress reads
    /// this, and it forwards the raw JSON verbatim -- no cross-dialect
    /// translation. Every other egress ignores the field, so a
    /// codex-only kind never corrupts a non-Responses upstream body.
    /// Each entry carries the count of MODELED input items that preceded
    /// it inbound (`modeled_prefix`), so the egress splices it back into
    /// its original conversation position instead of appending every
    /// preserved item to the tail. Like `claude_code_headers` this is
    /// inbound-request data, not a per-model knob; empty for library
    /// consumers and for every non-Responses ingress.
    pub responses_input_passthrough: Vec<ResponsesPassthroughItem>,

    /// INBOUND per-conversation key captured by the Anthropic ingress
    /// from the `x-claude-code-session-id` request header, falling back
    /// to the body `metadata.session_id`. This is the REAL per-conversation
    /// key -- it differs per conversation.
    ///
    /// This is the CANONICAL inbound session identity: both the usage
    /// ledger's `UsageRecord.session_id` column (`build_usage_draft` reads
    /// this field directly) and the K-estimator's per-session sample store
    /// (`record_k_sample`) key on this SAME value. A session identified
    /// only via the `metadata.session_id` fallback (no header) therefore
    /// still gets a durable ledger row and survives a K-store rebuild
    /// after a restart -- there is no separate, header-only derivation to
    /// drift out of sync with this one.
    ///
    /// Do NOT confuse this with the OUTBOUND per-credential
    /// `ClaudeCodeIdentity::session_id` value minted in
    /// `crates/routectl-providers/src/anthropic_api/cloak.rs` and stamped
    /// on the egress request: that one is stable for the provider's life
    /// (identical across every conversation on a seat) and is NOT a usable
    /// per-conversation key.
    ///
    /// `None` when the ingress dialect has no session-identity concept
    /// (OpenAI chat-completions, Responses) and for library consumers.
    /// Never serialized to any upstream (it rides on `routectl_internal`,
    /// which is `#[serde(skip)]`). Must not be logged raw.
    pub inbound_session_key: Option<String>,

    /// INBOUND first-party bearer token captured for opt-in passthrough
    /// to the upstream. Wrapped in [`ForwardedBearer`] so the token is
    /// never printed by this struct's derived `Debug`.
    ///
    /// `None` when no first-party bearer was captured (non-passthrough
    /// paths and library consumers). Never serialized upstream (it rides
    /// on `routectl_internal`, which is `#[serde(skip)]`), never logged
    /// raw -- read it only via [`ForwardedBearer::expose`].
    pub forwarded_bearer: Option<ForwardedBearer>,
}

/// Fixed redaction placeholder shared by `ForwardedBearer`'s `Debug` and
/// `Display` impls. The wrapped token is never rendered.
const FORWARDED_BEARER_REDACTED: &str = "<redacted>";

/// A first-party inbound bearer token captured for opt-in passthrough to
/// the upstream.
///
/// The workspace has no `secrecy`/`Secret` type, and `RoutectlInternal`
/// derives `Debug`, so a bare `String` here would be printed verbatim by
/// `{:?}`. This newtype's hand-written `Debug` and `Display` impls print
/// a FIXED placeholder (`FORWARDED_BEARER_REDACTED`) and NEVER the token;
/// the raw value is reachable only through [`ForwardedBearer::expose`]. It
/// deliberately does not derive `Serialize`/`Deserialize` (the carrier
/// field is `#[serde(skip)]` anyway) so the token can never reach a wire.
#[derive(Clone)]
pub struct ForwardedBearer(String);

impl ForwardedBearer {
    /// Wrap a raw inbound bearer token.
    pub const fn new(token: String) -> Self {
        Self(token)
    }

    /// Read the raw token. This is the ONLY path to the wrapped value;
    /// callers must not log or serialize what it returns.
    pub const fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for ForwardedBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ForwardedBearer({FORWARDED_BEARER_REDACTED})")
    }
}

impl std::fmt::Display for ForwardedBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(FORWARDED_BEARER_REDACTED)
    }
}

/// Role of a message author on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System / developer instructions.
    System,
    /// End-user turn.
    User,
    /// Model turn.
    Assistant,
    /// Tool-result turn.
    Tool,
}

/// One conversation turn in canonical form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Author role for this turn.
    pub role: Role,
    /// Message body (text, typed parts, or null).
    #[serde(default)]
    pub content: MessageContent,

    /// Echoed reasoning from a prior assistant turn (legacy plaintext shape).
    /// Providers may strip before resending (DeepSeek 400s on this).
    /// Upstream `reasoning_content` (DeepSeek/vLLM/NIM shape) is coalesced
    /// into this field by the openai-compat normalizer's preprocess step.
    /// We don't use a serde alias here because NIM sometimes emits BOTH
    /// keys (one of them null), which would deserialize-fail with
    /// "duplicate field `reasoning`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,

    /// Echoed reasoning from a prior assistant turn (typed-blocks shape).
    /// Anthropic tool-use loops require these to be passed back unmodified.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_details: Vec<ReasoningDetail>,

    /// Optional author name (OpenAI function/tool naming).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Correlation id for a `Role::Tool` turn's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Assistant tool-call requests (OpenAI shape), passed through verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,

    /// OpenAI safety-refusal string. Returned on `choices[].message.refusal`
    /// alongside `content: null` when the model declines; canonical had no
    /// slot, so the client saw an empty assistant turn with no signal.
    /// Request-side this is always `None` (skip_serializing_if omits it,
    /// no wire change on outbound requests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

/// A message body: a flat string, typed content parts, or null.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Flat text content.
    Text(String),
    /// Typed content parts. Round-trips Anthropic and OpenAI-shape blocks
    /// losslessly via `ContentPart` (see `crate::content_part`). Unknown
    /// block types fall to `ContentPart::Other` which preserves the
    /// original `type` discriminant and arbitrary fields.
    Parts(Vec<ContentPart>),
    /// Some upstreams (Clarifai-hosted models on OpenRouter, vLLM trailers)
    /// return `"content": null` when the entire output is reasoning. We
    /// accept it on the wire and serialize back as null. Also the default:
    /// an assistant tool-call turn carries `tool_calls` and no `content`,
    /// so `#[serde(default)]` on `Message.content` lands here.
    #[default]
    Null,
}

/// Unified reasoning request config. See OpenRouter docs for provider mapping.
///
/// - OpenAI o-series: `effort` -> `reasoning_effort`
/// - Anthropic: `max_tokens` -> `thinking.budget_tokens` (or `effort` mapped)
/// - DeepSeek: model selection (`-reasoner` variant)
/// - Qwen / vLLM: `enabled` -> `chat_template_kwargs.enable_thinking`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// "minimal" | "low" | "medium" | "high" | "xhigh" | "none"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Anthropic/Gemini-style budget. Mutually exclusive with `effort` per OpenRouter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Suppress reasoning content from response (still billed for tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
    /// Enable reasoning with provider defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `id`, `model`, and `created` are tolerated as missing on the wire:
/// some upstreams (e.g. NIM's gemma-3) omit `created` entirely, and
/// `id`/`model` may be absent on minimal responses. Empty strings and a
/// zero timestamp serialize back out, which is acceptable for OpenAI-style
/// clients that treat these fields as informational.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Upstream response id (empty when the upstream omitted it).
    #[serde(default)]
    pub id: String,
    /// Model that produced the response (empty when omitted).
    #[serde(default)]
    pub model: String,
    /// Unix creation timestamp (zero when omitted).
    #[serde(default)]
    pub created: i64,
    /// Completion choices.
    pub choices: Vec<Choice>,
    /// Token usage tallies, when the upstream reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Which configured provider answered (routectl-specific extension; clients ignore).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routectl_provider: Option<String>,
    /// Forward-compat catchall for response top-level fields that
    /// canonical doesn't model. Mirrors the request-side
    /// `provider_extras` pattern: when an upstream returns a field
    /// routectl doesn't have a typed slot for (e.g. Anthropic's
    /// `context_management` from the `context-management-2025-06-27`
    /// beta, or any future spec field), it deserializes into
    /// `extras` and serializes back out alongside the typed fields.
    /// New Anthropic response fields ship through routectl with zero
    /// code edits.
    #[serde(default, flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: serde_json::Map<String, serde_json::Value>,
    /// Transport-internal carrier for non-canonical upstream metadata
    /// (today: Anthropic's `anthropic-ratelimit-unified-*` quota/overage
    /// family parsed off the anthropic-api egress response headers).
    /// Skip-serialized so the client-facing wire shape is unchanged for
    /// every consumer; populated only on the egress path and read by
    /// usage-accounting observability. See `crate::upstream_meta`.
    #[serde(skip)]
    pub upstream_meta: Option<crate::upstream_meta::UpstreamMeta>,
}

/// One completion choice in a `ChatResponse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    /// Zero-based choice index.
    #[serde(default)]
    pub index: u32,
    /// The assistant message for this choice.
    pub message: Message,
    /// Why generation stopped (`stop`, `length`, `tool_calls`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// The matched stop sequence (when the upstream surfaced one). Set
    /// by Anthropic-shape egresses from the wire `stop_sequence` field,
    /// and by openai-compat egress via a suffix-match heuristic against
    /// the request's `stop` list. The Anthropic ingress uses this to
    /// emit `stop_reason:"stop_sequence"` + `stop_sequence:"<value>"`
    /// instead of the lossy `end_turn` it would otherwise produce when
    /// `finish_reason` is the canonical `"stop"`. None means either
    /// the upstream stopped for another reason or routectl couldn't
    /// recover a match (openai-compat without a recoverable suffix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_stop_sequence: Option<String>,
    /// OpenAI per-choice `logprobs` object (token log-probabilities).
    /// Opaque passthrough: canonical does not model the shape, so the
    /// openai-compat egress deserializes it here and the OpenAI ingress
    /// re-serializes it verbatim. Absent on Anthropic/Bedrock upstreams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<Value>,
}

/// Usage tallies. v0.4.0 extension: cache stats from Anthropic /
/// Bedrock-Invoke responses surface here (`cache_creation_input_tokens`,
/// `cache_read_input_tokens`, and the per-TTL breakdown).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the prompt.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Tokens generated in the completion.
    #[serde(default)]
    pub completion_tokens: u32,
    /// Sum of prompt and completion tokens.
    #[serde(default)]
    pub total_tokens: u32,
    /// Reasoning tokens billed, when the upstream reports them separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Tokens written to the prompt cache on this request (cache miss
    /// path). Anthropic / Bedrock-Invoke only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// Tokens read from the prompt cache on this request (cache hit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Per-TTL breakdown of cache creations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    /// Server-side tool invocation counts (e.g. Anthropic's
    /// `web_search_requests`). Anthropic reports this as a
    /// `server_tool_use` object on the usage payload; routectl keeps it
    /// as an opaque JSON value so new server-tool kinds flow through
    /// without a schema change. Absent on upstreams that don't report
    /// it -- skip-serialized when None so the wire output is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<serde_json::Value>,
    /// Forward-compat catchall for usage sub-fields canonical doesn't
    /// model. Anthropic's `service_tier` (returned on every response)
    /// and any future spec additions deserialize here and serialize
    /// back out alongside the typed fields. Same shape as
    /// `ChatResponse.extras`.
    #[serde(default, flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl Usage {
    /// Reconstruct `total_tokens` from the component counts when an
    /// upstream reported the parts but omitted (or zeroed) the
    /// aggregate. Some openai-compat hosts ship `prompt_tokens` /
    /// `completion_tokens` without `total_tokens`; without this,
    /// downstream accounting would record a zero total for a response
    /// that clearly consumed tokens. The nonzero-component guard keeps a
    /// genuinely empty usage object all-zero so a total is never
    /// invented. Must run at every normalize entry point before usage
    /// accounting reads the response, so the derived total is the single
    /// value the ledger ever sees.
    pub fn derive_total_if_absent(&mut self) {
        if self.total_tokens == 0 && (self.prompt_tokens > 0 || self.completion_tokens > 0) {
            self.total_tokens = self.prompt_tokens.saturating_add(self.completion_tokens);
            tracing::debug!(
                target: "routectl::usage",
                prompt_tokens = self.prompt_tokens,
                completion_tokens = self.completion_tokens,
                total_tokens = self.total_tokens,
                "derived absent usage total_tokens from component counts"
            );
        }
    }
}

/// Per-TTL breakdown of cache writes for one request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheCreation {
    /// Tokens written to the 5-minute-TTL cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u32>,
    /// Tokens written to the 1-hour-TTL cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u32>,
}

/// Result of a `count_tokens` probe call. Mirrors Anthropic's
/// `/v1/messages/count_tokens` response. Currently `input_tokens` is
/// the only required field; future cache breakdown fields (cache
/// creation/read tokens) ride in `extras` so the canonical can grow
/// without breaking serialization for existing callers.
///
/// Wire reference: <https://docs.anthropic.com/en/api/messages-count-tokens>
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenCount {
    /// Token count Anthropic reports for the supplied request.
    pub input_tokens: u32,
    /// Forward-compat catchall. Anthropic's response carries
    /// `cache_creation_input_tokens` and `cache_read_input_tokens`
    /// in some experimental beta surfaces; future adds land here
    /// and serialize back out alongside the typed field.
    #[serde(default, flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Streaming SSE chunk (delta).
///
/// `id` and `model` are tolerated as missing on the wire: some upstreams
/// emit cost/usage trailer chunks where these fields are absent. Empty
/// strings serialize back out, which is fine for OpenAI-style SSE clients
/// that only look at `choices[].delta`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatChunk {
    /// Upstream chunk id (empty when omitted).
    #[serde(default)]
    pub id: String,
    /// Model that produced the chunk (empty when omitted).
    #[serde(default)]
    pub model: String,
    /// Per-choice deltas carried by this chunk.
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Streaming usage update. Anthropic emits cache stats in
    /// `message_delta` events; routectl surfaces them here so OpenAI-SSE
    /// clients see the same totals at end-of-stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageDelta>,
    /// Transport-internal carrier for opaque SSE events that don't fit
    /// the canonical `ChunkDelta` shape. Populated by the Anthropic-API
    /// egress when an unknown `content_block` type is open; consumed by
    /// the matching Anthropic ingress for verbatim re-emission. Skip-
    /// serialized so the canonical wire shape is unchanged for library
    /// consumers and OpenAI-shape ingresses (which can't represent these
    /// blocks anyway).
    #[serde(skip)]
    pub opaque_events: Vec<crate::schema_opaque::OpaqueSseEvent>,
    /// Transport-internal carrier for non-canonical upstream metadata
    /// (today: Anthropic's `anthropic-ratelimit-unified-*` quota/overage
    /// family). On a stream this is set ONLY on the FIRST canonical chunk
    /// yielded (the response head is where the headers are available);
    /// consumers must NOT assume it on later chunks. Skip-serialized so
    /// the wire shape is unchanged. See `crate::upstream_meta`.
    #[serde(skip)]
    pub upstream_meta: Option<crate::upstream_meta::UpstreamMeta>,
}

/// One streaming choice delta within a `ChatChunk`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    /// Zero-based choice index this delta applies to.
    #[serde(default)]
    pub index: u32,
    /// The incremental delta for this choice.
    pub delta: ChunkDelta,
    /// Why generation stopped, on the terminal chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// The matched stop sequence on the terminal chunk. Parallel to
    /// `Choice.matched_stop_sequence`; populated on the same chunk
    /// that carries the `finish_reason`. None on every non-terminal
    /// chunk and on terminal chunks where no stop sequence matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_stop_sequence: Option<String>,
}

/// Incremental content for one streaming choice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkDelta {
    /// Author role, set on the opening delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// Incremental text content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Upstream `reasoning_content` is coalesced here by the SSE chunk
    /// preprocessor; see `coalesce_reasoning_content` in openai_compat/sse.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Incremental typed reasoning blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_details: Vec<ReasoningDetail>,
    /// Incremental assistant tool-call requests, passed through verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
}

/// Streaming usage delta. Mirrors `Usage` but every field is optional
/// because chunks may carry partial info.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageDelta {
    /// Prompt token count, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    /// Completion token count, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    /// Total token count, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Reasoning token count, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Cache-write token count, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// Cache-read token count, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Per-TTL cache-write breakdown, when carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    /// Server-side tool invocation counts streamed in Anthropic's
    /// `message_delta.usage.server_tool_use`. Opaque JSON for
    /// forward-compat; skip-serialized when None so the wire shape is
    /// unchanged for the common no-server-tool path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<serde_json::Value>,
}

/// Top-level reasoning content on an assistant message. Mirrors OpenRouter's
/// dual-shape: legacy `reasoning` string + typed `reasoning_details` array.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reasoning {
    /// Legacy plaintext form. Suitable for single-turn / simple workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Typed blocks. Required for Anthropic tool-use continuity and
    /// any encrypted/redacted reasoning payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ReasoningDetail>,
}

/// One block of reasoning. `format` carries the provider-specific shape tag
/// (e.g. `"anthropic-claude-v1"`, `"openai-responses-v1"`, `"deepseek-v1"`).
///
/// `id`, `format`, and `index` are optional on the wire: OpenRouter omits
/// `id` for plain text reasoning blocks, and some upstreams never set
/// `format`/`index`. We accept the looser shape and let normalizers fill
/// defaults when they need them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningDetail {
    /// Discriminator selecting the payload shape and egress handling.
    #[serde(rename = "type")]
    pub kind: ReasoningDetailKind,
    /// Provider block id, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Provider-specific shape tag (e.g. `anthropic-claude-v1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Ordering index within a multi-block reasoning stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// The kind-specific fields (text, signature, encrypted content, ...).
    #[serde(flatten)]
    pub payload: Value,
}

/// Discriminator on a `ReasoningDetail`. Determines what fields the
/// detail's `payload` object carries and how downstream egresses
/// interpret it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningDetailKind {
    /// OpenAI Responses reasoning summary block. `payload.text`
    /// carries a one-paragraph summary the model surfaces alongside
    /// the answer; not the full chain-of-thought.
    #[serde(rename = "reasoning.summary")]
    Summary,
    /// OpenAI Responses encrypted reasoning. `payload.encrypted_content`
    /// is an opaque blob the model emits and expects back verbatim on
    /// follow-up turns for chain-of-thought continuity. Round-trip
    /// only; never displayed to the user.
    #[serde(rename = "reasoning.encrypted")]
    Encrypted,
    /// Anthropic-shape thinking block. `payload.text` is the visible
    /// thinking content; `payload.signature` is mandatory for
    /// multi-turn replay (Anthropic 400s on follow-ups missing it).
    /// Format string `anthropic-claude-v1` distinguishes from other
    /// `Text`-kind details.
    #[serde(rename = "reasoning.text")]
    Text,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// serde_json emits object keys in alphabetical order because the
    /// workspace does NOT enable serde_json's `preserve_order` feature (its
    /// `Map` is a `BTreeMap`). This byte-ordering premise is load-bearing for
    /// egress prompt-cache affinity: anthropic request bodies are serialized
    /// through a `to_value` sort/merge buffer and the resulting bytes are
    /// cached for prompt-cache prefix reuse. If `preserve_order` were ever
    /// enabled (directly or pulled in transitively by any dependency), keys
    /// would follow insertion order instead, request bytes would drift, and
    /// upstream cache affinity would break silently -- no semantic test would
    /// catch it, since the egress contract tests are order-blind. This test
    /// fails the instant that feature is turned on anywhere in the tree.
    #[test]
    fn serde_json_object_keys_serialize_alphabetically() {
        assert_eq!(
            serde_json::to_string(&json!({ "b": 1, "a": 2 })).unwrap(),
            r#"{"a":2,"b":1}"#,
            "serde_json must emit alphabetical object keys; a failure here \
             means the preserve_order feature has been enabled and the egress \
             cache-affinity byte premise is broken"
        );
    }

    /// A partial `usage` object that omits `total_tokens` must not sink
    /// the whole `ChatResponse` deserialization -- some openai-compat
    /// upstreams ship only the component counts. The missing aggregate
    /// defaults to 0 (the derive step fills it in downstream).
    #[test]
    fn chatresponse_with_partial_usage_deserializes() {
        let resp: ChatResponse = serde_json::from_value(json!({
            "choices": [],
            "usage": { "prompt_tokens": 12, "completion_tokens": 8 }
        }))
        .expect("partial usage (no total_tokens) must deserialize");
        let usage = resp.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.total_tokens, 0, "absent aggregate defaults to 0");
    }

    /// The derive step reconstructs an absent aggregate from the
    /// component counts via `saturating_add`.
    #[test]
    fn derive_total_fills_absent_aggregate_from_components() {
        let mut usage = Usage {
            prompt_tokens: 12,
            completion_tokens: 8,
            total_tokens: 0,
            ..Usage::default()
        };
        usage.derive_total_if_absent();
        assert_eq!(usage.total_tokens, 20);
    }

    /// The nonzero-component guard keeps a genuinely empty usage object
    /// all-zero -- a total is never invented.
    #[test]
    fn derive_total_leaves_all_zero_usage_untouched() {
        let mut usage = Usage::default();
        usage.derive_total_if_absent();
        assert_eq!(usage.total_tokens, 0);
    }

    /// An already-present aggregate is authoritative and never overwritten,
    /// even when it diverges from the component sum.
    #[test]
    fn derive_total_preserves_present_aggregate() {
        let mut usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 99,
            ..Usage::default()
        };
        usage.derive_total_if_absent();
        assert_eq!(usage.total_tokens, 99);
    }

    /// The reconstruction saturates rather than overflowing when the
    /// component counts sum past `u32::MAX`.
    #[test]
    fn derive_total_saturates_on_overflow() {
        let mut usage = Usage {
            prompt_tokens: u32::MAX,
            completion_tokens: 5,
            total_tokens: 0,
            ..Usage::default()
        };
        usage.derive_total_if_absent();
        assert_eq!(usage.total_tokens, u32::MAX);
    }

    /// A `RoutectlInternal` built via `Default` carries `Library`
    /// provenance, and a directly-constructed `ChatRequest` (no ingress)
    /// inherits that default.
    #[test]
    fn provenance_defaults_to_library() {
        assert_eq!(
            RoutectlInternal::default().provenance,
            RequestProvenance::Library
        );
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": []
        }))
        .unwrap();
        assert_eq!(req.routectl_internal.provenance, RequestProvenance::Library);
    }

    /// OpenAI permits `stop` as a bare string; canonical must accept it
    /// and normalize to a one-element vec instead of 400-ing the request.
    #[test]
    fn stop_accepts_bare_string() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [],
            "stop": "END"
        }))
        .unwrap();
        assert_eq!(req.stop, Some(vec!["END".to_string()]));
    }

    /// The array form deserializes to the multi-element vec unchanged.
    #[test]
    fn stop_accepts_string_array() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [],
            "stop": ["A", "B"]
        }))
        .unwrap();
        assert_eq!(req.stop, Some(vec!["A".to_string(), "B".to_string()]));
    }

    /// Absent `stop` stays `None` (the `default` path).
    #[test]
    fn stop_absent_is_none() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": []
        }))
        .unwrap();
        assert!(req.stop.is_none());
    }

    /// Regardless of inbound shape, `stop` serializes back out as an
    /// array (unchanged wire-out for the array form).
    #[test]
    fn stop_round_trips_out_as_array() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [],
            "stop": "END"
        }))
        .unwrap();
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["stop"], json!(["END"]));
    }

    /// A safety refusal arrives as `message.refusal` with `content: null`.
    /// Canonical must carry it so the client still sees the signal.
    #[test]
    fn message_refusal_round_trips() {
        let raw = json!({
            "role": "assistant",
            "content": null,
            "refusal": "I can't help with that."
        });
        let msg: Message = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.refusal.as_deref(), Some("I can't help with that."));
        let out = serde_json::to_value(&msg).unwrap();
        assert_eq!(out["refusal"], "I can't help with that.");
    }

    /// On the request side `refusal` is `None` and skip_serializing_if
    /// omits it -- no wire change on outbound messages.
    #[test]
    fn message_refusal_absent_is_omitted_on_serialize() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        };
        let out = serde_json::to_value(&msg).unwrap();
        assert!(out.get("refusal").is_none(), "got {out}");
    }

    /// `Choice.logprobs` is an opaque OpenAI passthrough; it must
    /// deserialize and serialize back verbatim.
    #[test]
    fn choice_logprobs_round_trips() {
        let raw = json!({
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop",
            "logprobs": {"content": [{"token": "hi", "logprob": -0.1}]}
        });
        let choice: Choice = serde_json::from_value(raw).unwrap();
        assert!(choice.logprobs.is_some());
        let out = serde_json::to_value(&choice).unwrap();
        assert_eq!(out["logprobs"]["content"][0]["token"], "hi");
    }

    /// Absent `logprobs` is omitted on serialize (no wire change).
    #[test]
    fn choice_logprobs_absent_is_omitted_on_serialize() {
        let choice = Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        };
        let out = serde_json::to_value(&choice).unwrap();
        assert!(out.get("logprobs").is_none(), "got {out}");
    }

    /// `Usage.server_tool_use` absent must serialize to NO `server_tool_use`
    /// key, keeping the wire output byte-identical for the common path
    /// where the upstream did not invoke a server-side tool.
    #[test]
    fn usage_server_tool_use_absent_is_omitted_on_serialize() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        };
        let out = serde_json::to_value(&usage).unwrap();
        assert!(
            out.get("server_tool_use").is_none(),
            "absent server_tool_use must stay absent on the wire, got {out}"
        );
    }

    /// When present, `server_tool_use` round-trips through the canonical
    /// `Usage` as an opaque JSON object (forward-compatible with new
    /// server-tool kinds).
    #[test]
    fn usage_server_tool_use_round_trips() {
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            server_tool_use: Some(json!({"web_search_requests": 3})),
            ..Default::default()
        };
        let out = serde_json::to_value(&usage).unwrap();
        assert_eq!(out["server_tool_use"]["web_search_requests"], 3);
        let back: Usage = serde_json::from_value(out).unwrap();
        assert_eq!(
            back.server_tool_use,
            Some(json!({"web_search_requests": 3}))
        );
    }

    /// An assistant tool-call turn arrives with `tool_calls` and NO
    /// `content` key (the OpenAI Chat Completions shape on a function
    /// call). `#[serde(default)]` on `Message.content` + the
    /// `MessageContent::Null` default must accept it as Null rather than
    /// failing deserialization on a missing required field.
    #[test]
    fn message_without_content_deserializes_to_null() {
        let raw = json!({
            "role": "assistant",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "f", "arguments": "{}"}
            }]
        });
        let msg: Message = serde_json::from_value(raw).unwrap();
        assert!(
            matches!(msg.content, MessageContent::Null),
            "got {:?}",
            msg.content
        );
        assert!(msg.tool_calls.is_some());
    }

    /// `MessageContent::default()` is now `Null` (was `Text("")`).
    #[test]
    fn message_content_default_is_null() {
        assert!(matches!(MessageContent::default(), MessageContent::Null));
    }

    /// `Choice.index` is `#[serde(default)]`; an upstream omitting it
    /// (some minimal response shapes) deserializes to 0 rather than
    /// 400-ing the whole response.
    #[test]
    fn choice_without_index_defaults_to_zero() {
        let raw = json!({
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        });
        let choice: Choice = serde_json::from_value(raw).unwrap();
        assert_eq!(choice.index, 0);
    }

    /// `ChunkChoice.index` is `#[serde(default)]`; an SSE chunk omitting
    /// it deserializes to 0.
    #[test]
    fn chunk_choice_without_index_defaults_to_zero() {
        let raw = json!({
            "delta": {"content": "tok"}
        });
        let choice: ChunkChoice = serde_json::from_value(raw).unwrap();
        assert_eq!(choice.index, 0);
    }

    /// A multi-turn message array where one message omits `content`
    /// (an assistant tool-call turn) deserializes cleanly -- the
    /// `#[serde(default)]` annotation must not regress the common
    /// content-present path either.
    #[test]
    fn message_array_with_one_content_omission_deserializes() {
        let raw = json!([
            {"role": "user", "content": "what files?"},
            {"role": "assistant", "tool_calls": [{
                "id": "c1", "type": "function",
                "function": {"name": "ls", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "c1", "content": "a b c"}
        ]);
        let msgs: Vec<Message> = serde_json::from_value(raw).unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0].content, MessageContent::Text(_)));
        assert!(matches!(msgs[1].content, MessageContent::Null));
        assert!(matches!(msgs[2].content, MessageContent::Text(_)));
    }

    /// A distinctive sentinel that must never appear in any Debug,
    /// Display, or serialized output of a `ForwardedBearer`.
    const SECRET_TOKEN: &str = "sk-live-DO-NOT-LEAK-abc123XYZ";

    /// A freshly-defaulted carrier has no forwarded bearer.
    #[test]
    fn forwarded_bearer_defaults_to_none() {
        assert!(RoutectlInternal::default().forwarded_bearer.is_none());
    }

    /// A freshly-defaulted carrier has no forwarded Stainless headers, so
    /// own mode and every non-forwarded path stay byte-identical to the
    /// pre-passthrough carrier state.
    #[test]
    fn stainless_headers_default_to_empty() {
        assert!(RoutectlInternal::default().stainless_headers.is_empty());
    }

    /// The Stainless carrier rides on `routectl_internal` (`#[serde(skip)]`),
    /// so even a populated set leaves no trace on the serialized wire.
    #[test]
    fn stainless_headers_never_serialized_to_wire() {
        let mut req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": []
        }))
        .unwrap();
        req.routectl_internal.stainless_headers =
            vec![("x-stainless-package-version".into(), "9.9.9-canary".into())];

        let wire = serde_json::to_string(&req).unwrap();

        assert!(
            !wire.contains("9.9.9-canary"),
            "stainless header value leaked to the wire: {wire}"
        );
        assert!(
            !wire.contains("stainless_headers"),
            "carrier field name leaked to the wire: {wire}"
        );
    }

    /// Debug of the newtype alone prints a fixed placeholder and never
    /// the wrapped token -- the redaction contract is security-critical.
    #[test]
    fn forwarded_bearer_debug_redacts_token() {
        let bearer = ForwardedBearer::new(SECRET_TOKEN.to_string());

        let rendered = format!("{bearer:?}");

        assert!(
            !rendered.contains(SECRET_TOKEN),
            "Debug leaked the raw token: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "Debug missing the redaction placeholder: {rendered}"
        );
    }

    /// Display of the newtype alone prints a fixed placeholder and never
    /// the wrapped token (logs that use `{}` must stay safe too).
    #[test]
    fn forwarded_bearer_display_redacts_token() {
        let bearer = ForwardedBearer::new(SECRET_TOKEN.to_string());

        let rendered = format!("{bearer}");

        assert!(
            !rendered.contains(SECRET_TOKEN),
            "Display leaked the raw token: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "Display missing the redaction placeholder: {rendered}"
        );
    }

    /// Debug of a `RoutectlInternal` carrying `Some(ForwardedBearer(..))`
    /// must not leak the token via the derived struct Debug either.
    #[test]
    fn routectl_internal_debug_redacts_forwarded_bearer() {
        let internal = RoutectlInternal {
            forwarded_bearer: Some(ForwardedBearer::new(SECRET_TOKEN.to_string())),
            ..Default::default()
        };

        let rendered = format!("{internal:?}");

        assert!(
            !rendered.contains(SECRET_TOKEN),
            "carrier Debug leaked the raw token: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "carrier Debug missing the redaction placeholder: {rendered}"
        );
    }

    /// The raw token is reachable only through the explicit accessor.
    #[test]
    fn forwarded_bearer_expose_returns_raw_token() {
        let bearer = ForwardedBearer::new(SECRET_TOKEN.to_string());

        assert_eq!(bearer.expose(), SECRET_TOKEN);
    }

    /// The carrier field rides on `routectl_internal`, which is
    /// `#[serde(skip)]`, so a request carrying a bearer serializes with
    /// no trace of the token, the field, or the carrier on the wire.
    #[test]
    fn forwarded_bearer_never_serialized_to_wire() {
        let mut req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": []
        }))
        .unwrap();
        req.routectl_internal.forwarded_bearer =
            Some(ForwardedBearer::new(SECRET_TOKEN.to_string()));

        let wire = serde_json::to_string(&req).unwrap();

        assert!(
            !wire.contains(SECRET_TOKEN),
            "token leaked to the wire: {wire}"
        );
        assert!(
            !wire.contains("forwarded_bearer"),
            "carrier field name leaked to the wire: {wire}"
        );
        assert!(
            !wire.contains("routectl_internal"),
            "skipped carrier leaked to the wire: {wire}"
        );
    }
}
