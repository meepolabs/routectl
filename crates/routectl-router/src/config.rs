//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use routectl_providers::anthropic_api::AuthKind;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Server bind config.
    #[serde(default)]
    pub server: ServerConfig,

    /// Provider definitions keyed by operator-facing name. Carries
    /// transport-side knobs only (auth, base URL, headers, runtime
    /// gates). Per-model knobs (`supports_adaptive_thinking`,
    /// `effort_levels`, `max_thinking_budget`, `reasoning_dialect`,
    /// `history_reasoning`, `additional_request_fields`) live on
    /// `[models.X]` in v0.6.0.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,

    /// v0.6.0 alias table. Each key is a wire `model` value (or a
    /// suffix-glob pattern like `claude-opus-*`, or the literal
    /// `default` catch-all key); each value is either a single model
    /// nickname or an ordered fallback chain of nicknames -- both
    /// forms reference `[models.X]` table entries. The wire `model`
    /// field on incoming requests resolves through this table.
    ///
    /// v0.6.0 collapsed the legacy `[aliases.X.chain]` `provider:model`
    /// literals AND the `[ingress.X.aliases]` per-dialect maps AND the
    /// `default_model` field AND the suffix-glob convention (used
    /// primarily by Claude Code and the OpenAI SDK over our ingress)
    /// into this single table. See the v0.6.0-rc.1 CHANGELOG for the
    /// migration guide.
    #[serde(default)]
    pub aliases: BTreeMap<String, AliasValue>,

    /// Default retry policy applied per-provider attempt.
    #[serde(default)]
    pub retry: RetryPolicy,

    /// Bedrock-wide settings that apply to every Bedrock provider.
    /// Carries the operator-supplied `allowed_betas` and
    /// `allowed_body_fields` lists -- routectl ships no defaults so
    /// AWS schema drift does not require a routectl release. See
    /// `examples/bedrock.toml` for the empirical baseline. Future
    /// shared knobs (e.g. region-default, retry-default) land here too.
    #[serde(default)]
    pub bedrock: BedrockGlobalConfig,

    /// v0.6.0 model directory. Each entry binds a logical nickname
    /// (the table key) to a transport (`provider`), an upstream model
    /// id (`upstream`), and per-model knobs. The `[aliases]` table
    /// references entries by nickname. See the v0.6.0-rc.1 changelog.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,

    /// Operator-facing `[log]` block. Each field is an optional
    /// fallback consulted by the runtime log-safe knobs when the
    /// matching env var is unset. Env always wins over config; missing
    /// block keeps the pre-`[log]` env-only-or-hardcoded behavior. The
    /// env-filter directive (`ROUTECTL_LOG`) is intentionally NOT part
    /// of this block -- it stays env-only.
    #[serde(default)]
    pub log: LogConfig,
}

/// Operator-facing `[log]` config block. Each field mirrors a
/// well-known env var:
///
///   - `trace_headers` -> `ROUTECTL_TRACE_HEADERS`
///   - `trace_body_bytes` -> `ROUTECTL_TRACE_BODY_BYTES`
///   - `redact_prompts` -> `ROUTECTL_LOG_REDACT_PROMPTS`
///
/// Per-knob resolution: env wins when set; otherwise the field below
/// (when `Some(_)`); otherwise the hardcoded default. All fields are
/// optional. A missing `[log]` block leaves current behavior
/// unchanged.
///
/// The env-filter directive (`ROUTECTL_LOG`, e.g.
/// `routectl=info,routectl_core::log_safe=trace`) is intentionally
/// out of scope here. It stays env-only because it must reach the
/// tracing subscriber BEFORE any config load runs, and the
/// architect-validated design declines to reorder boot to introduce a
/// config-side fallback for it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// Opt-in for the four `trace_*_headers` directions (raw, no
    /// redaction). Default off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_headers: Option<bool>,
    /// Cap on the serialized body emitted at TRACE level by the four
    /// body-trace helpers. Zero or missing falls through to the
    /// hardcoded 16 KB default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_body_bytes: Option<usize>,
    /// Opt-in for prompt redaction in TRACE-level body logs. Default
    /// off (verbatim bodies in TRACE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact_prompts: Option<bool>,
}

/// One row in the `[models]` table. Carries the nickname-to-upstream
/// binding plus per-model knobs.
///
/// Fields that vary per-model belong here. Fields that vary per-
/// transport (auth, base URL, runtime gates) stay on `[providers.X]`.
/// Two fields, `header_extras` and `payload_extras`, live BOTH here and
/// on every provider variant; the dispatch layer merges them per
/// request (model wins on key collision; see `Router` merge helpers).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ModelEntry {
    /// Provider name (a key in the `[providers]` table). The router
    /// validates at startup that every model entry references a known
    /// provider.
    pub provider: String,

    /// Upstream model id. Forwarded to the provider verbatim. For
    /// Bedrock this becomes the `BedrockConfig.model_id` value (the
    /// AWS inference profile id, e.g.
    /// `us.anthropic.claude-haiku-4-5-20251001-v1:0`). For OpenAI-
    /// compatible egresses it is the wire `model` field.
    pub upstream: String,

    /// Whether this model is selectable from the `[aliases]` table.
    /// Defaults to `true`; flip to `false` to keep an entry around
    /// while wiring without making it servable. Disabled entries
    /// still load but `Router::new` errors when an alias chain
    /// references one.
    #[serde(default = "default_true")]
    pub selectable: bool,

    /// Whether this model supports the Anthropic adaptive thinking shape
    /// (Opus 4.7+). Projected via apply_layered_overlays into
    /// RoutectlInternal.supports_adaptive_thinking; the AnthropicApi
    /// egress reads it at request time. `false` (the default) uses the
    /// standard fixed-budget shape or no thinking at all.
    #[serde(default)]
    pub supports_adaptive_thinking: bool,

    /// Ordered list of effort levels the operator declares this model
    /// accepts. Validated at startup: every element must be one of
    /// `minimal`, `low`, `medium`, `high`, `xhigh`, `max` (the union
    /// of the Anthropic-shape vocabulary and the OpenAI-shape
    /// vocabulary; individual egresses clamp to their own subset).
    ///
    /// An empty list means pass-through -- the egress emits whatever
    /// effort the caller supplied without operator-side filtering.
    ///
    /// Default: `["low", "medium", "high"]`.
    #[serde(default = "default_effort_levels")]
    pub effort_levels: Vec<String>,

    /// Maximum thinking-token budget the operator allows for this
    /// model, in tokens. `0` (the default) means no operator cap --
    /// the egress uses whatever budget the caller requested or its own
    /// default. Non-zero values are forwarded as the ceiling for the
    /// egress budget negotiation.
    #[serde(default)]
    pub max_thinking_budget: u32,

    /// Per-model openai-compat reasoning dialect. Lives ONLY on
    /// `[models.X]` (no provider fallback) -- v0.6.0 moved the field
    /// off `[providers.X]` so two models on one provider can speak
    /// different dialects (DeepSeek's `reasoning_content` vs
    /// OpenAI's `reasoning_effort` vs OpenRouter's
    /// `reasoning_details`). Ignored by non-openai-compat egresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_dialect: Option<ReasoningDialect>,

    /// Per-model openai-compat outgoing-history reasoning policy.
    /// Lives ONLY on `[models.X]` (no provider fallback). See
    /// [`HistoryReasoning`] docs for v3-vs-v4 motivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_reasoning: Option<HistoryReasoning>,

    /// Bedrock Converse `additionalModelRequestFields` -- vendor-
    /// specific knobs that don't have a top-level Converse slot. The
    /// Bedrock Invoke egress also reads this map for non-routectl-
    /// managed body fields. Other egresses ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_request_fields: Option<Value>,

    /// Extra HTTP headers applied to every outbound request from this
    /// model's route. Merged with the provider's `header_extras` at
    /// dispatch time; model wins on key collision. The list-valued
    /// `anthropic-beta` header runs through a comma-split-union-rejoin
    /// post-pass (ingress -> provider -> model visit order) so all
    /// three sources land on one wire header. Auth-reserved keys
    /// (`authorization`, `x-api-key`, `anthropic-version`) WARN +
    /// drop; managed-reserved keys (`host`, `content-type`,
    /// `content-length`) DEBUG + drop.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header_extras: BTreeMap<String, String>,

    /// Extras merged into the outbound request body. Combined with the
    /// provider's `payload_extras` at dispatch time via a deep recursive
    /// merge (model wins on leaf collision; nested objects merge
    /// recursively). Lands on canonical `req.provider_extras`; each
    /// egress merges that into the wire body via its existing path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_extras: Option<Value>,

    /// Per-model first-byte timeout for streaming responses. Resolved
    /// with precedence per-model > per-provider > global, extending
    /// the existing two-tier provider > global resolution on
    /// `RetryPolicy::stream_first_byte_timeout_ms`.
    ///
    /// Use case: opus xhigh adaptive thinking on large prompts can
    /// take >90s to emit the first token while non-thinking models
    /// (haiku, llama) start responding in <5s. A per-model override
    /// lets opus sit at 300s without forcing every other model to
    /// wait 5 min on a dead upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,

    /// Operator-declared per-model ceiling on the `max_tokens` value
    /// the Anthropic-shape egresses (anthropic-api, bedrock-invoke)
    /// inject when the caller omits the field. `None` (the default)
    /// falls through to the hardcoded baseline of 64000.
    ///
    /// Set this for models whose upstream-documented ceiling is below
    /// the baseline to avoid a 400 on the upstream's per-model
    /// validation. Examples: Anthropic Opus 4 / 4.1 (32000), Sonnet 3.5
    /// (8000), DeepSeek V3 anthropic-api surface (8000). See
    /// docs/CONFIGURATION.md.
    ///
    /// Only consumed by Anthropic-shape egresses (anthropic-api +
    /// bedrock-invoke); openai-compat, openai-responses, and
    /// bedrock-converse forward omission cleanly (good-translator
    /// principle: do not inject where the upstream already handles it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

impl ModelEntry {
    pub fn new(provider: impl Into<String>, upstream: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            upstream: upstream.into(),
            selectable: true,
            supports_adaptive_thinking: false,
            effort_levels: default_effort_levels(),
            max_thinking_budget: 0,
            reasoning_dialect: None,
            history_reasoning: None,
            additional_request_fields: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            stream_first_byte_timeout_ms: None,
            max_output_tokens: None,
        }
    }

    /// Set whether this model supports the Anthropic adaptive thinking
    /// shape. Projected via apply_layered_overlays into
    /// RoutectlInternal.supports_adaptive_thinking; the AnthropicApi
    /// egress reads it at request time.
    pub fn with_supports_adaptive_thinking(mut self, b: bool) -> Self {
        self.supports_adaptive_thinking = b;
        self
    }

    /// Set the operator-declared effort-level allowlist for this model.
    /// An empty vec means pass-through. Elements are validated at startup
    /// by `validate_reasoning_defaults`.
    pub fn with_effort_levels(mut self, levels: Vec<String>) -> Self {
        self.effort_levels = levels;
        self
    }

    /// Set the maximum thinking-token budget cap for this model.
    /// `0` means no operator cap.
    pub fn with_max_thinking_budget(mut self, budget: u32) -> Self {
        self.max_thinking_budget = budget;
        self
    }

    pub fn with_reasoning_dialect(mut self, d: ReasoningDialect) -> Self {
        self.reasoning_dialect = Some(d);
        self
    }

    pub fn with_history_reasoning(mut self, h: HistoryReasoning) -> Self {
        self.history_reasoning = Some(h);
        self
    }

    pub fn with_header_extras(mut self, headers: BTreeMap<String, String>) -> Self {
        self.header_extras = headers;
        self
    }

    pub fn with_payload_extras(mut self, payload: Value) -> Self {
        self.payload_extras = Some(payload);
        self
    }

    /// Set the model's selectability flag.
    pub fn with_selectable(mut self, b: bool) -> Self {
        self.selectable = b;
        self
    }

    /// Set the per-model `stream_first_byte_timeout_ms`. Wins over
    /// the per-provider and global resolution. A value of 0 is an
    /// operator-error sentinel (every stream would time out before
    /// the first chunk arrived); flagged in debug builds.
    pub fn with_stream_first_byte_timeout_ms(mut self, ms: u64) -> Self {
        debug_assert!(
            ms > 0,
            "stream_first_byte_timeout_ms must be > 0; 0 would time out every stream",
        );
        self.stream_first_byte_timeout_ms = Some(ms);
        self
    }

    /// Set the per-model `max_output_tokens` ceiling consumed by
    /// Anthropic-shape egresses (anthropic-api, bedrock-invoke) when
    /// the caller omits `max_tokens`. `None` falls through to the
    /// hardcoded baseline of 64000. A value of 0 is operator error
    /// (the egress would produce a body the upstream 400s); flagged
    /// in debug builds.
    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        debug_assert!(
            tokens > 0,
            "max_output_tokens must be > 0; 0 would 400 every anthropic-api request",
        );
        self.max_output_tokens = Some(tokens);
        self
    }
}

fn default_true() -> bool {
    true
}

/// Default effort-level allowlist for a model entry. The three
/// mid-range values are the safe cross-provider baseline; operators
/// extend the list to unlock `minimal`, `xhigh`, or `max` for
/// specific models.
fn default_effort_levels() -> Vec<String> {
    vec!["low".into(), "medium".into(), "high".into()]
}

/// Value of one entry in the `[aliases]` table. Either a single model
/// nickname (the most common shape) or a fallback chain of nicknames.
/// Untagged so the operator-facing TOML stays terse:
///
/// ```toml
/// [aliases]
/// "claude-opus-4-7-20251022" = "heavy"      # single
/// "fast"                     = ["small", "smaller"]  # chain
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AliasValue {
    Single(String),
    Chain(Vec<String>),
}

impl AliasValue {
    /// Iterate over the model nicknames in this alias entry. A
    /// `Single` yields one name; a `Chain` yields each entry in
    /// order. Lifetimes here mean "the names live as long as the
    /// `AliasValue`."
    ///
    /// Implementation: a hand-rolled two-state iterator avoids the
    /// `Box<dyn Iterator>` heap allocation per call. Dispatch fires
    /// this on every alias-chain walk so the difference matters at
    /// scale.
    pub fn nicknames(&self) -> NicknameIter<'_> {
        match self {
            AliasValue::Single(s) => NicknameIter::Single(Some(s.as_str())),
            AliasValue::Chain(v) => NicknameIter::Chain(v.iter()),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            AliasValue::Single(_) => false,
            AliasValue::Chain(v) => v.is_empty(),
        }
    }
}

/// Zero-allocation iterator returned by `AliasValue::nicknames`.
/// Two states: the single-string variant yields its name once; the
/// chain variant wraps a slice iterator and yields each entry in
/// order. No heap allocation in either path -- contrast with the
/// previous `Box<dyn Iterator>` implementation that allocated per
/// call.
pub enum NicknameIter<'a> {
    Single(Option<&'a str>),
    Chain(std::slice::Iter<'a, String>),
}

impl<'a> Iterator for NicknameIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            NicknameIter::Single(opt) => opt.take(),
            NicknameIter::Chain(iter) => iter.next().map(|s| s.as_str()),
        }
    }
}

/// Bedrock-wide configuration shared by every `[providers.X]` entry of
/// `kind = "bedrock"`. Both allowlists below are operator-owned; the
/// routectl binary ships no defaults so AWS schema drift (Anthropic
/// adds a beta, Bedrock gates a body field) does not require a
/// routectl release. See `examples/bedrock.toml` for the empirical
/// 2026-05-12 baseline; copy and tune as your account's gating
/// evolves.
///
/// **Empty list = pass-through.** Either field, when empty (or the
/// entire `[bedrock]` section omitted), disables that filter -- the
/// upstream sees the assembled value unchanged. This is the
/// discovery-mode default: bring up routectl, observe actual traffic
/// via `ROUTECTL_LOG=routectl_providers::bedrock=trace`, then
/// populate the list with what you observe.
///
/// Startup validation in `crate::factory::validate_bedrock_global_config`
/// kicks in only when `allowed_body_fields` is non-empty, and rejects
/// a list missing routectl-mandatory keys (`messages`,
/// `anthropic_version`, `max_tokens`) or missing `anthropic_beta`
/// when a `[providers.X] anthropic_beta` floor is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BedrockGlobalConfig {
    /// Bedrock-accepted `anthropic_beta` flags. AWS validates each
    /// entry independently and 400s the request on the first
    /// unsupported flag. **Empty list = pass-through** (no filtering;
    /// every flag the ingress lifts in is forwarded). Populate via
    /// TOML to enable filtering. `examples/bedrock.toml` ships the
    /// empirical 2026-05-12 baseline.
    ///
    /// Per-provider `[providers.X] anthropic_beta` is unrelated and
    /// keeps its existing semantics (operator-asserted floor that is
    /// always sent and bypasses this filter).
    #[serde(default)]
    pub allowed_betas: Vec<String>,

    /// Bedrock-accepted top-level body fields (Invoke) /
    /// `additionalModelRequestFields` keys (Converse). Bedrock 400s
    /// any unrecognized field with `"Extra inputs are not permitted"`,
    /// so the Anthropic ingress's forward-compat sweep needs filtering
    /// on the Bedrock egress when this list is non-empty. **Empty
    /// list = pass-through** (no filtering; every key in the assembled
    /// body / bag is forwarded).
    ///
    /// When non-empty, must include the routectl-mandatory keys
    /// (`messages`, `anthropic_version`, `max_tokens`) for requests
    /// to construct successfully -- startup validation rejects an
    /// incomplete list with a copy-paste hint.
    #[serde(default)]
    pub allowed_body_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind host. Defaults to localhost. Refuses non-loopback unless
    /// `--unsafe-public` is passed on the CLI.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,

    /// Listener-side auth. When `tokens` is non-empty, every request
    /// must carry a matching `x-api-key` or `Authorization: Bearer
    /// <token>` header. Tokens are SecretRef URIs (env://, file://,
    /// literal:) and are resolved at startup.
    #[serde(default)]
    pub auth: Option<ServerAuth>,

    /// When true, lossy translation seams (e.g. cache_control on a
    /// canonical -> OpenAI-compat egress) return a 400 instead of
    /// emitting a `tracing::warn!`. Default false (warn-and-drop)
    /// preserves dev ergonomics; flip to true for production CI.
    #[serde(default)]
    pub strict_translation: bool,

    /// When true (default), the `x-routectl-disable-fallbacks` request
    /// header lets a client pin a request to a single provider with
    /// no fallback chain. Useful for tests, dev probing, and
    /// per-request triage. Set to `false` for hardened multi-tenant
    /// deployments where authenticated clients should not be able to
    /// disable the gateway's HA story or probe per-provider health.
    #[serde(default = "default_allow_disable_fallbacks")]
    pub allow_disable_fallbacks: bool,

    /// Maximum incoming JSON body size for `/v1/chat/completions` and
    /// `/v1/messages`, in bytes. Defaults to 32 MiB -- comfortably above
    /// the largest legitimate Anthropic Messages request (long system
    /// prompt + many tool defs + long history with cache_control
    /// breakpoints) while preventing trivial OOM-DoS via a multi-GB
    /// POST. No server-side ceiling is enforced; axum's
    /// `DefaultBodyLimit` and the OS allocator are the only guards.
    /// Prefer values under 1 GiB for normal deployments.
    /// Replaces the pre-v0.8 hardcoded `MAX_BODY_BYTES` (4 MiB) which
    /// was too tight for live-traffic claude-code sessions.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u32,
}

fn default_allow_disable_fallbacks() -> bool {
    true
}

fn default_max_body_bytes() -> u32 {
    32 * 1024 * 1024
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: default_allow_disable_fallbacks(),
            max_body_bytes: default_max_body_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerAuth {
    /// Allowed tokens, stored as SecretRef URIs. Empty list means
    /// "no auth required" (loopback dev default).
    #[serde(default)]
    pub tokens: Vec<String>,
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_port() -> u16 {
    8787
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProviderEntry {
    #[non_exhaustive]
    OpenaiCompat {
        base_url: String,
        /// Reference to the API key. One of:
        ///   - `env://VAR_NAME`             (process env var)
        ///   - `file:///abs/path/to/key`    (mode-600 file)
        ///   - `literal:plaintext`          (inline; placeholders only)
        api_key_ref: String,
        /// Provider-level header extras. Merged with the per-model
        /// `header_extras` at dispatch time; model wins on key
        /// collision (list-valued `anthropic-beta` runs through a
        /// comma-split-union-rejoin post-pass instead).
        #[serde(default)]
        header_extras: BTreeMap<String, String>,
        /// Provider-level payload extras. Deep-merged with the
        /// per-model `payload_extras` at dispatch time (model wins on
        /// leaf collision).
        #[serde(default)]
        payload_extras: Option<Value>,
        /// Override the outbound User-Agent.
        #[serde(default)]
        user_agent: Option<String>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    #[non_exhaustive]
    AnthropicApi {
        api_key_ref: String,
        #[serde(default = "default_anthropic_base")]
        base_url: String,
        #[serde(default = "default_anthropic_version")]
        anthropic_version: String,
        /// How the Messages API authenticates this provider. Default
        /// is `api-key` (the standard `x-api-key` header). Use
        /// `oauth-bearer` when `api_key_ref` resolves to a Claude Code
        /// subscription access token (`sk-ant-oat01-...`).
        #[serde(default)]
        auth_kind: AuthKind,
        /// Provider-level header extras. Merged with per-model entries
        /// at dispatch time.
        #[serde(default)]
        header_extras: BTreeMap<String, String>,
        /// Provider-level payload extras. Deep-merged with per-model
        /// entries at dispatch time.
        #[serde(default)]
        payload_extras: Option<Value>,
        /// Override the outbound User-Agent. Useful for IAM-gated upstreams.
        #[serde(default)]
        user_agent: Option<String>,
        /// Optional operator-supplied allowlist for `anthropic_beta`
        /// flags forwarded to api.anthropic.com. Default (empty) is
        /// pass-through.
        #[serde(default)]
        allowed_betas: Vec<String>,
        /// Strict allowlist of inbound `x-claude-code-*` header names
        /// the egress is permitted to forward to api.anthropic.com.
        /// Empty (default) drops every captured `x-claude-code-*`
        /// header at the egress -- secure-by-default for new
        /// providers. Names match case-insensitively. Capture happens
        /// at the Anthropic ingress (defense-in-depth, namespace-
        /// bounded); this list is the operator's filter on which
        /// captured names actually go upstream.
        #[serde(default)]
        forward_client_headers: Vec<String>,
        /// When true, routectl emulates Anthropic's
        /// context-management-2025-06-27 beta server-side for this
        /// provider. Set this for non-Anthropic anthropic-api providers
        /// (e.g. DeepSeek's /anthropic surface) that do not honor the
        /// beta natively. Default false: routectl forwards the body
        /// verbatim and the real Anthropic server handles the beta
        /// itself.
        #[serde(default)]
        context_management: bool,
        /// Per-entry byte cap on the thinking cache used by the
        /// `context_management` emulation path. Bounds: `>= 1024` (1
        /// KiB) and `<= 4 * 1024 * 1024` (4 MiB). When unset the
        /// runtime falls back to the
        /// `DEFAULT_MAX_THINKING_ENTRY_BYTES` baseline (1 MiB).
        /// Operators on memory-constrained hosts tune this down; the
        /// LRU's worst-case footprint is `THINKING_CACHE_CAP * cap`
        /// (10000 * 1 MiB ~ 10 GiB at the default).
        ///
        /// Setting `0` is treated as unset; a startup WARN is emitted and
        /// the default applies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_thinking_entry_bytes: Option<u32>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    /// OpenAI Responses API provider. Three auth surfaces:
    /// - `chatgpt-oauth`: ChatGPT subscription JWT.
    /// - `api-key`: standard OpenAI API key.
    /// - `bedrock-mantle`: Authorization: Bearer <bearer> using the
    ///   long-term Bedrock API key (resolved via api_key_ref, typically
    ///   env://AWS_BEARER_TOKEN_BEDROCK).
    ///
    /// `base_url` is optional: when unset, the factory picks the
    /// auth_kind-appropriate default at provider build time.
    #[cfg(feature = "openai-responses")]
    #[non_exhaustive]
    OpenaiResponses {
        /// Resolves to the bearer JWT (ChatgptOauth), API key (ApiKey),
        /// or long-term Bedrock API key (BedrockMantle, typically
        /// env://AWS_BEARER_TOKEN_BEDROCK).
        api_key_ref: String,
        /// ChatGPT account UUID. Required when `auth_kind =
        /// "chatgpt-oauth"`; must be absent for the other variants.
        #[serde(default)]
        account_id_ref: Option<String>,
        /// Endpoint base URL. None -> factory picks an auth_kind-
        /// appropriate default. Operators can pin a specific value
        /// to point at a staging shard, a localhost mock, etc.
        #[serde(default)]
        base_url: Option<String>,
        /// Which auth surface to dispatch on. Default
        /// `chatgpt-oauth`.
        #[serde(default)]
        auth_kind: OpenaiResponsesAuthKind,
        /// Provider-level header extras.
        #[serde(default)]
        header_extras: BTreeMap<String, String>,
        /// Provider-level payload extras.
        #[serde(default)]
        payload_extras: Option<Value>,
        /// Override the outbound User-Agent. None -> default
        /// `routectl/<version> codex-cli`.
        #[serde(default)]
        user_agent: Option<String>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    /// Native AWS Bedrock provider. Speaks SigV4 directly to
    /// `bedrock-runtime.<region>.amazonaws.com`. Pick `api_shape` to
    /// switch between vendor-specific InvokeModel (default) and
    /// vendor-neutral Converse.
    #[cfg(feature = "bedrock")]
    #[non_exhaustive]
    Bedrock {
        region: String,
        #[serde(default)]
        api_shape: BedrockApiShapeConfig,
        creds: BedrockCredsConfig,
        #[serde(default)]
        user_agent: Option<String>,
        /// Provider-level header extras.
        #[serde(default)]
        header_extras: BTreeMap<String, String>,
        /// Provider-level payload extras.
        #[serde(default)]
        payload_extras: Option<Value>,
        #[serde(default)]
        anthropic_beta: Vec<String>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
}

/// TOML-side mirror of `routectl_providers::bedrock::BedrockApiShape`.
/// We don't re-export the providers-side enum directly because TOML
/// configs need to parse cleanly even when the `bedrock` feature is
/// off (so non-Bedrock builds stay lean), and serde derives don't
/// like cfg-gated re-exports.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BedrockApiShapeConfig {
    #[default]
    Invoke,
    Converse,
}

/// TOML-side credentials descriptor for a Bedrock provider.
///
/// Each variant is tagged by `kind`. Secret-bearing fields hold raw
/// secret-URI strings (`env://`, `file://`, `literal:`) which the
/// factory parses + resolves at provider build time -- same pattern
/// as `api_key_ref` on the other provider variants.
///
/// Examples:
/// ```toml
/// # Bedrock console short-term API key
/// creds = { kind = "bearer-key", key_ref = "file:///home/me/.config/routectl/bedrock.key" }
///
/// # Static AWS access keys
/// creds = { kind = "static",
///           access_key_ref  = "env://AWS_ACCESS_KEY_ID",
///           secret_key_ref  = "env://AWS_SECRET_ACCESS_KEY",
///           session_token_ref = "env://AWS_SESSION_TOKEN" }
///
/// # Named profile in ~/.aws/credentials (incl. SSO)
/// creds = { kind = "profile", name = "bedrock-prod" }
///
/// # Standard AWS provider chain (env -> profile -> SSO -> IRSA -> IMDS)
/// creds = { kind = "default-chain" }
/// ```
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum BedrockCredsConfig {
    BearerKey {
        key_ref: String,
    },
    Static {
        access_key_ref: String,
        secret_key_ref: String,
        #[serde(default)]
        session_token_ref: Option<String>,
    },
    Profile {
        name: String,
    },
    DefaultChain,
}

impl ProviderEntry {
    /// Get the runtime policy attached to this entry. Centralizes the
    /// match so the router doesn't repeat it.
    pub fn runtime(&self) -> &ProviderRuntimePolicy {
        match self {
            Self::OpenaiCompat { runtime, .. } | Self::AnthropicApi { runtime, .. } => runtime,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { runtime, .. } => runtime,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { runtime, .. } => runtime,
        }
    }

    /// Per-provider `header_extras`. Returns a reference to the
    /// per-variant map so the dispatch-layer merge helpers can read
    /// without re-matching the enum.
    pub fn header_extras(&self) -> &BTreeMap<String, String> {
        match self {
            Self::OpenaiCompat { header_extras, .. } => header_extras,
            Self::AnthropicApi { header_extras, .. } => header_extras,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { header_extras, .. } => header_extras,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { header_extras, .. } => header_extras,
        }
    }

    /// Per-provider `payload_extras`. Returns a reference (None when
    /// the operator did not configure any) so the dispatch-layer deep
    /// merge can borrow without cloning on the no-op path.
    pub fn payload_extras(&self) -> Option<&Value> {
        match self {
            Self::OpenaiCompat { payload_extras, .. } => payload_extras.as_ref(),
            Self::AnthropicApi { payload_extras, .. } => payload_extras.as_ref(),
            #[cfg(feature = "bedrock")]
            Self::Bedrock { payload_extras, .. } => payload_extras.as_ref(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { payload_extras, .. } => payload_extras.as_ref(),
        }
    }

    pub fn openai_compat(base_url: impl Into<String>, api_key_ref: impl Into<String>) -> Self {
        Self::OpenaiCompat {
            base_url: base_url.into(),
            api_key_ref: api_key_ref.into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    pub fn anthropic_api(api_key_ref: impl Into<String>) -> Self {
        Self::AnthropicApi {
            api_key_ref: api_key_ref.into(),
            base_url: default_anthropic_base(),
            anthropic_version: default_anthropic_version(),
            auth_kind: AuthKind::ApiKey,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// Construct an `OpenaiResponses` entry with sane defaults. The
    /// only required field is `api_key_ref`; everything else defaults
    /// to `None` / `Default::default()`. Use the variant-specific
    /// setters below to populate optional fields.
    #[cfg(feature = "openai-responses")]
    pub fn openai_responses(api_key_ref: impl Into<String>) -> Self {
        Self::OpenaiResponses {
            api_key_ref: api_key_ref.into(),
            account_id_ref: None,
            base_url: None,
            auth_kind: OpenaiResponsesAuthKind::default(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    pub fn with_auth_kind(mut self, kind: AuthKind) -> Self {
        match &mut self {
            Self::AnthropicApi { auth_kind, .. } => *auth_kind = kind,
            _ => panic!("ProviderEntry::with_auth_kind only applies to anthropic-api"),
        }
        self
    }

    /// Set the `account_id_ref` on an `OpenaiResponses` entry. Panics
    /// on other variants -- the field is OpenaiResponses-only.
    #[cfg(feature = "openai-responses")]
    pub fn with_account_id_ref(mut self, account_id_ref: impl Into<String>) -> Self {
        match &mut self {
            Self::OpenaiResponses {
                account_id_ref: slot,
                ..
            } => {
                *slot = Some(account_id_ref.into());
            }
            _ => panic!("ProviderEntry::with_account_id_ref only applies to openai-responses"),
        }
        self
    }

    /// Set the `base_url` on an `OpenaiResponses` entry. Panics on
    /// other variants. Named `with_openai_responses_base_url` to avoid
    /// colliding with `with_base_url` (which serves the api-backed
    /// providers whose `base_url` is `String`, not `Option<String>`).
    #[cfg(feature = "openai-responses")]
    pub fn with_openai_responses_base_url(mut self, base_url: impl Into<String>) -> Self {
        match &mut self {
            Self::OpenaiResponses { base_url: slot, .. } => {
                *slot = Some(base_url.into());
            }
            _ => panic!(
                "ProviderEntry::with_openai_responses_base_url only applies to openai-responses"
            ),
        }
        self
    }

    /// Set the `auth_kind` on an `OpenaiResponses` entry. Panics on
    /// other variants. Named `with_openai_responses_auth_kind` to
    /// avoid colliding with `with_auth_kind` (which targets the
    /// AnthropicApi variant and takes a different enum).
    #[cfg(feature = "openai-responses")]
    pub fn with_openai_responses_auth_kind(mut self, kind: OpenaiResponsesAuthKind) -> Self {
        match &mut self {
            Self::OpenaiResponses { auth_kind, .. } => *auth_kind = kind,
            _ => panic!(
                "ProviderEntry::with_openai_responses_auth_kind only applies to openai-responses"
            ),
        }
        self
    }

    pub fn with_runtime(mut self, rt: ProviderRuntimePolicy) -> Self {
        match &mut self {
            Self::OpenaiCompat { runtime, .. } | Self::AnthropicApi { runtime, .. } => {
                *runtime = rt
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { runtime, .. } => *runtime = rt,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { runtime, .. } => *runtime = rt,
        }
        self
    }

    pub fn with_header_extras(mut self, headers: BTreeMap<String, String>) -> Self {
        match &mut self {
            Self::OpenaiCompat { header_extras, .. } => *header_extras = headers,
            Self::AnthropicApi { header_extras, .. } => *header_extras = headers,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { header_extras, .. } => *header_extras = headers,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { header_extras, .. } => *header_extras = headers,
        }
        self
    }

    pub fn with_payload_extras(mut self, extras: Value) -> Self {
        let slot = Some(extras);
        match &mut self {
            Self::OpenaiCompat { payload_extras, .. } => *payload_extras = slot,
            Self::AnthropicApi { payload_extras, .. } => *payload_extras = slot,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { payload_extras, .. } => *payload_extras = slot,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { payload_extras, .. } => *payload_extras = slot,
        }
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let u = url.into();
        match &mut self {
            Self::OpenaiCompat { base_url, .. } | Self::AnthropicApi { base_url, .. } => {
                *base_url = u
            }
            _ => panic!("ProviderEntry::with_base_url only applies to api-backed providers"),
        }
        self
    }

    pub fn with_anthropic_version(mut self, version: impl Into<String>) -> Self {
        match &mut self {
            Self::AnthropicApi {
                anthropic_version, ..
            } => *anthropic_version = version.into(),
            _ => {
                panic!("ProviderEntry::with_anthropic_version only applies to anthropic-api")
            }
        }
        self
    }

    /// Set the AnthropicApi variant's `forward_client_headers`
    /// allowlist (names of inbound `x-claude-code-*` headers the
    /// egress may forward to api.anthropic.com). Panics on other
    /// variants -- the field is AnthropicApi-only.
    pub fn with_forward_client_headers(mut self, v: Vec<String>) -> Self {
        match &mut self {
            Self::AnthropicApi {
                forward_client_headers,
                ..
            } => *forward_client_headers = v,
            _ => panic!("ProviderEntry::with_forward_client_headers only applies to anthropic-api"),
        }
        self
    }

    /// Set the AnthropicApi variant's `max_thinking_entry_bytes` knob
    /// (per-entry byte cap on the `context_management` emulation
    /// thinking cache). `None` falls through to
    /// `AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES` at provider
    /// build time. Panics on other variants -- the field is
    /// AnthropicApi-only.
    pub fn with_max_thinking_entry_bytes(mut self, bytes: Option<u32>) -> Self {
        match &mut self {
            Self::AnthropicApi {
                max_thinking_entry_bytes,
                ..
            } => *max_thinking_entry_bytes = bytes,
            _ => {
                panic!("ProviderEntry::with_max_thinking_entry_bytes only applies to anthropic-api")
            }
        }
        self
    }

    pub fn redact_secrets(&mut self) {
        match self {
            Self::OpenaiCompat { api_key_ref, .. } | Self::AnthropicApi { api_key_ref, .. } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { creds, .. } => creds.redact(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                api_key_ref,
                account_id_ref,
                ..
            } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
                if let Some(a) = account_id_ref {
                    *a = redact_literal_secret(a);
                }
            }
        }
    }

    pub fn secret_uris(&self) -> Vec<&str> {
        match self {
            Self::OpenaiCompat { api_key_ref, .. } | Self::AnthropicApi { api_key_ref, .. } => {
                vec![api_key_ref.as_str()]
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { creds, .. } => creds.secret_uris(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                api_key_ref,
                account_id_ref,
                ..
            } => {
                let mut v = vec![api_key_ref.as_str()];
                if let Some(a) = account_id_ref {
                    v.push(a.as_str());
                }
                v
            }
        }
    }
}

#[cfg(feature = "bedrock")]
impl BedrockCredsConfig {
    /// Replace literal-prefixed secret values with `literal:[REDACTED]`.
    /// Other URI schemes are already non-secret pointers.
    pub fn redact(&mut self) {
        match self {
            Self::BearerKey { key_ref } => {
                *key_ref = redact_literal_secret(key_ref);
            }
            Self::Static {
                access_key_ref,
                secret_key_ref,
                session_token_ref,
            } => {
                *access_key_ref = redact_literal_secret(access_key_ref);
                *secret_key_ref = redact_literal_secret(secret_key_ref);
                if let Some(t) = session_token_ref {
                    *t = redact_literal_secret(t);
                }
            }
            Self::Profile { .. } | Self::DefaultChain => {}
        }
    }

    /// Enumerate every secret-URI string a config check should resolve.
    pub fn secret_uris(&self) -> Vec<&str> {
        match self {
            Self::BearerKey { key_ref } => vec![key_ref.as_str()],
            Self::Static {
                access_key_ref,
                secret_key_ref,
                session_token_ref,
            } => {
                let mut v = vec![access_key_ref.as_str(), secret_key_ref.as_str()];
                if let Some(t) = session_token_ref {
                    v.push(t.as_str());
                }
                v
            }
            Self::Profile { .. } | Self::DefaultChain => Vec::new(),
        }
    }
}

fn redact_literal_secret(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("literal:") {
        if rest.is_empty() {
            "literal:".into()
        } else {
            "literal:[REDACTED]".into()
        }
    } else {
        uri.to_string()
    }
}

/// Per-provider runtime knobs that gate dispatch: rate limits, circuit
/// breaker, timeouts, capability filters. All fields default to "off"
/// so omitting the block leaves provider behavior unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderRuntimePolicy {
    /// Maximum requests per minute. When exceeded, the router treats
    /// this provider as a fallbackable failure and tries the next entry
    /// in the chain. None = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// Trip the circuit breaker after this many consecutive failed
    /// attempts within the failure window. Once tripped, the router
    /// skips this provider for `circuit_cooldown_ms`.
    /// None = breaker disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_failures: Option<u32>,
    /// How long to keep the circuit open (skip provider) once tripped.
    /// Defaults to 30s when `circuit_failures` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_cooldown_ms: Option<u64>,
    /// Per-attempt request timeout that applies to every alias chain
    /// entry routing through this provider. Only used when the alias's
    /// `[aliases.X.retry] request_timeout_ms` is unset; the alias-level
    /// override always wins.
    ///
    /// Resolution order (provider > global):
    ///   provider.request_timeout_ms (this field)
    ///     -> [retry] request_timeout_ms (workspace global)
    ///       -> None (no cap, reqwest's default)
    ///
    /// Use this when many models share the same upstream and the
    /// timeout is an upstream characteristic (e.g., NIM cold-start),
    /// not a routing decision. v0.6 removed per-alias retry overrides;
    /// only the two tiers above remain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Per-attempt first-byte timeout for streaming responses through
    /// this provider. Same provider > global resolution as
    /// `request_timeout_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,
    /// Operator-supplied list of feature keys this provider does not
    /// support. Router pre-filters the alias chain before dispatch -- a
    /// provider listed here is skipped entirely (not tried-and-fallback)
    /// when the request needs any of these features. Examples:
    /// `web_search`, `computer_use`. See feature-key derivation in
    /// `crates/routectl-router/src/feature_keys.rs`.
    #[serde(default)]
    pub unsupported_features: Vec<String>,
}

fn default_anthropic_base() -> String {
    "https://api.anthropic.com".into()
}

fn default_anthropic_version() -> String {
    "2023-06-01".into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningDialect {
    #[default]
    Openai,
    Deepseek,
    Vllm,
    RawThinkTag,
    Openrouter,
    Passthrough,
}

impl From<ReasoningDialect> for routectl_core::CoreReasoningDialect {
    fn from(d: ReasoningDialect) -> Self {
        match d {
            ReasoningDialect::Openai => Self::Openai,
            ReasoningDialect::Deepseek => Self::Deepseek,
            ReasoningDialect::Vllm => Self::Vllm,
            ReasoningDialect::RawThinkTag => Self::RawThinkTag,
            ReasoningDialect::Openrouter => Self::Openrouter,
            ReasoningDialect::Passthrough => Self::Passthrough,
        }
    }
}

/// Outgoing-history reasoning policy for openai-compat providers.
///   - `auto` (default): use the dialect's default. DeepSeek and vLLM
///     strip; OpenAI and OpenRouter pass through.
///   - `strip`: force-drop reasoning fields from outgoing assistant
///     messages, regardless of dialect. Use for DeepSeek v3 / vLLM <=
///     0.6 hosts that 400 on echo-back.
///   - `preserve`: emit the dialect-native preserve shape. Required by
///     DeepSeek v4+, which 400s on missing echo-back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryReasoning {
    /// Use the dialect's default (DeepSeek/vLLM strip; OpenAI/OpenRouter
    /// pass through). Backward-compatible.
    #[default]
    Auto,
    /// Force-strip reasoning fields from outgoing assistant messages.
    Strip,
    /// Force-emit the dialect's preserve shape on outgoing assistant
    /// messages.
    Preserve,
}

impl From<HistoryReasoning> for routectl_core::CoreHistoryReasoning {
    fn from(h: HistoryReasoning) -> Self {
        match h {
            HistoryReasoning::Auto => Self::Auto,
            HistoryReasoning::Strip => Self::Strip,
            HistoryReasoning::Preserve => Self::Preserve,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Default retry-attempts cap per provider in the chain. Used when
    /// no per-error-class override below is set.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Initial backoff in ms.
    #[serde(default = "default_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Backoff multiplier per attempt.
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Random additional ms added to each backoff sleep (0..jitter_ms).
    /// Prevents thundering-herd retries when many clients fail at once.
    #[serde(default)]
    pub jitter_ms: u64,
    /// Status codes that trigger fallback to the next provider in the
    /// chain (in addition to network errors). When non-empty, only the
    /// listed codes are fallbackable. When empty (the default), the
    /// resolution falls through to `retry_denylist` or, if that is also
    /// unset, to "every 4xx/5xx is fallbackable". Mutually exclusive
    /// with `retry_denylist` -- setting both is a config-load error.
    #[serde(default)]
    pub retry_allowlist: Vec<u16>,

    /// Inverse of `retry_allowlist`: when `Some`, every 4xx/5xx code
    /// EXCEPT those in the list triggers fallback. `None` means no
    /// denylist is active; if `retry_allowlist` is also empty, the
    /// default "all 4xx/5xx fall back" predicate applies. Mutually
    /// exclusive with a non-empty `retry_allowlist` -- setting both
    /// is a config-load error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_denylist: Option<Vec<u16>>,

    /// Per-error-class retry caps. When set, override `max_attempts` for
    /// that specific class. Useful because rate-limits often clear in
    /// a single retry while flaky 5xx may need more attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_429: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_5xx: Option<u32>,
    /// Network errors (status 0): DNS, TCP connect, TLS handshake,
    /// request body, request timeout. Default is `max_attempts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_network: Option<u32>,

    /// End-to-end per-attempt timeout. `None` means rely on the
    /// reqwest client's default (no explicit cap). When set, the
    /// router wraps each upstream call in `tokio::time::timeout`
    /// and treats expiry as a network error (status 0, retryable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// First-byte timeout for streaming responses. If the upstream
    /// hasn't emitted any bytes in this window, the stream is
    /// abandoned and (if no chunk has been delivered yet) the next
    /// provider in the chain is tried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,

    /// Requests with `max_tokens` <= this are treated as availability
    /// probes (Claude Code sends `max_tokens=1`); on a rate-limit /
    /// overload (429/529) they skip retry+fallback and return the
    /// status immediately, since walking the chain is futile and the
    /// probe output is unused. `0` disables. Real requests (max_tokens
    /// above this) are unaffected.
    #[serde(default = "default_probe_max_tokens")]
    pub probe_max_tokens: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            jitter_ms: 50,
            retry_allowlist: Vec::new(),
            retry_denylist: None,
            retry_on_429: None,
            retry_on_5xx: None,
            retry_on_network: None,
            request_timeout_ms: None,
            stream_first_byte_timeout_ms: None,
            probe_max_tokens: default_probe_max_tokens(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ProviderEntry};

    #[test]
    #[should_panic(expected = "with_anthropic_version")]
    fn wrong_variant_setter_panics() {
        let _ = ProviderEntry::openai_compat("https://example.com/v1", "literal:test")
            .with_anthropic_version("2023-06-01");
    }

    #[test]
    fn redact_secrets_redacts_literal_only() {
        let mut entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-test");
        entry.redact_secrets();
        assert_eq!(entry.secret_uris(), vec!["literal:[REDACTED]"]);
    }

    /// `forward_client_headers` defaults to an empty list when the
    /// field is omitted from the TOML (secure-by-default: drop every
    /// captured `x-claude-code-*` header). Explicit lists round-trip
    /// through serialize/deserialize so the operator's allowlist is
    /// preserved end-to-end.
    #[test]
    fn anthropic_api_forward_client_headers_round_trips() {
        // Default: omitted -> empty Vec.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse default");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");
        match entry {
            ProviderEntry::AnthropicApi {
                forward_client_headers,
                ..
            } => assert!(
                forward_client_headers.is_empty(),
                "default must be empty; got: {forward_client_headers:?}"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }

        // Explicit list of two names.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
forward_client_headers = ["x-claude-code-session-id", "x-claude-code-agent-id"]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse explicit");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");
        match entry {
            ProviderEntry::AnthropicApi {
                forward_client_headers,
                ..
            } => assert_eq!(
                forward_client_headers,
                &vec![
                    "x-claude-code-session-id".to_string(),
                    "x-claude-code-agent-id".to_string(),
                ],
                "explicit list must round-trip"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }

        // Round-trip: serialize, deserialize, compare.
        let cfg_in: Config = toml::from_str(toml_text).expect("parse in");
        let serialized = toml::to_string(&cfg_in).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("parse out");
        match cfg_out.providers.get("anthropic").expect("anthropic") {
            ProviderEntry::AnthropicApi {
                forward_client_headers,
                ..
            } => assert_eq!(
                forward_client_headers,
                &vec![
                    "x-claude-code-session-id".to_string(),
                    "x-claude-code-agent-id".to_string(),
                ],
                "round-trip must preserve list"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// `RetryPolicy::default()` ships with `jitter_ms = 50` so
    /// multi-client deployments get retry spread out of the box without
    /// any explicit operator configuration.
    #[test]
    fn retry_policy_default_jitter_is_50() {
        use super::RetryPolicy;
        assert_eq!(
            RetryPolicy::default().jitter_ms,
            50,
            "default jitter_ms must be 50 for out-of-the-box retry spread"
        );
    }
}

#[cfg(test)]
mod v0_6_config_tests {
    //! Tests for the v0.6.0+ config shapes: `[models]` table and the
    //! untagged `AliasValue` enum.
    //!
    //! Breaking change: `thinking` and `effort` fields were removed from
    //! `ModelEntry`. TOMLs carrying those keys must fail at parse time.
    //! The new capability fields are `supports_adaptive_thinking`,
    //! `effort_levels`, and `max_thinking_budget`.

    use super::{AliasValue, Config, HistoryReasoning, ModelEntry, ReasoningDialect};
    use std::collections::BTreeMap;

    /// A model entry with only the two required fields gets the correct
    /// defaults: supports_adaptive_thinking=false,
    /// effort_levels=["low","medium","high"], max_thinking_budget=0.
    #[test]
    fn model_entry_required_fields_only() {
        let toml_text = r#"
[models.haiku]
provider = "anthropic"
upstream = "claude-haiku-4-5-20251001"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("haiku").expect("haiku entry");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.upstream, "claude-haiku-4-5-20251001");
        assert!(m.selectable, "default selectable = true");
        assert!(!m.supports_adaptive_thinking, "default false");
        assert_eq!(
            m.effort_levels,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()],
            "default effort_levels"
        );
        assert_eq!(m.max_thinking_budget, 0, "default max_thinking_budget");
        assert!(m.reasoning_dialect.is_none());
        assert!(m.history_reasoning.is_none());
        assert!(m.header_extras.is_empty());
        assert!(m.payload_extras.is_none());
    }

    /// New capability fields parse correctly and round-trip through serde.
    #[test]
    fn model_entry_new_capability_fields_round_trip() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7"
supports_adaptive_thinking = true
effort_levels = ["low", "medium", "high", "xhigh"]
max_thinking_budget = 8000
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert!(m.supports_adaptive_thinking);
        assert_eq!(
            m.effort_levels,
            vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
            ]
        );
        assert_eq!(m.max_thinking_budget, 8000);
    }

    /// TOMLs carrying the old `thinking` key must fail at parse time.
    /// `deny_unknown_fields` on `ModelEntry` surfaces the old key as a
    /// parse error so misconfigurations are caught at startup.
    #[test]
    fn model_entry_rejects_old_thinking_field() {
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
thinking = "adaptive"
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("thinking"),
            "expected error to name the removed field 'thinking'; got: {msg}"
        );
    }

    /// TOMLs carrying the old `effort` key must fail at parse time.
    #[test]
    fn model_entry_rejects_old_effort_field() {
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
effort = "high"
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("effort"),
            "expected error to name the removed field 'effort'; got: {msg}"
        );
    }

    #[test]
    fn model_entry_rejects_removed_adaptive_thinking_field() {
        // v0.6.0 dropped `adaptive_thinking`; deny_unknown_fields makes
        // the old key reject at startup so the upgrade isn't silent.
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
adaptive_thinking = true
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("adaptive_thinking"),
            "expected error to name the removed field; got: {msg}"
        );
    }

    #[test]
    fn model_entry_rejects_removed_anthropic_beta_field() {
        // v0.6.0 dropped the per-model `anthropic_beta: Vec<String>`
        // field; operators set `anthropic-beta` via `header_extras`.
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
anthropic_beta = ["context-1m-2025-08-07"]
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("anthropic_beta"),
            "expected error to name the removed field; got: {msg}"
        );
    }

    #[test]
    fn model_entry_rejects_removed_default_extras_field() {
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
default_extras = { foo = "bar" }
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("default_extras"),
            "expected error to name the removed field; got: {msg}"
        );
    }

    #[test]
    fn model_entry_header_extras_round_trip() {
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
header_extras = { "anthropic-beta" = "context-1m-2025-08-07", "x-app" = "cli" }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("entry");
        assert_eq!(
            m.header_extras.get("anthropic-beta"),
            Some(&"context-1m-2025-08-07".to_string())
        );
        assert_eq!(m.header_extras.get("x-app"), Some(&"cli".to_string()));
    }

    #[test]
    fn model_entry_payload_extras_round_trip() {
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
payload_extras = { nested = { key = "value" }, scalar = 42 }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("entry");
        let extras = m.payload_extras.as_ref().expect("payload_extras set");
        assert_eq!(
            extras.get("nested").and_then(|v| v.get("key")),
            Some(&serde_json::json!("value"))
        );
        assert_eq!(extras.get("scalar"), Some(&serde_json::json!(42)));
    }

    #[test]
    fn model_entry_reasoning_dialect_round_trip() {
        let toml_text = r#"
[models.m]
provider = "p"
upstream = "u"
reasoning_dialect = "deepseek"
history_reasoning = "preserve"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("m").expect("entry");
        assert_eq!(m.reasoning_dialect, Some(ReasoningDialect::Deepseek));
        assert_eq!(m.history_reasoning, Some(HistoryReasoning::Preserve));
    }

    #[test]
    fn provider_entry_rejects_removed_extra_headers_field() {
        // Provider-side `extra_headers` was renamed to `header_extras`;
        // deny_unknown_fields surfaces the old key as a parse error.
        let toml_text = r#"
[providers.bad]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
extra_headers = { "x-foo" = "bar" }
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("extra_headers"),
            "expected error to name the removed field; got: {msg}"
        );
    }

    #[test]
    fn provider_entry_rejects_reasoning_dialect_on_provider() {
        // Moved to [models.X]; provider-side key must reject.
        let toml_text = r#"
[providers.bad]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
reasoning_dialect = "deepseek"
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("reasoning_dialect"),
            "expected error to name the removed field; got: {msg}"
        );
    }

    #[test]
    fn alias_value_parses_single_string() {
        let toml_text = r#"
"claude-opus-4-7" = "heavy"
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
        let entry = v.get("claude-opus-4-7").expect("entry");
        match entry {
            AliasValue::Single(s) => assert_eq!(s, "heavy"),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn alias_value_parses_chain_list() {
        let toml_text = r#"
"fast" = ["nano", "mini"]
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
        let entry = v.get("fast").expect("entry");
        match entry {
            AliasValue::Chain(c) => assert_eq!(c, &vec!["nano".to_string(), "mini".to_string()]),
            other => panic!("expected Chain, got {other:?}"),
        }
    }

    #[test]
    fn alias_value_default_special_key() {
        let toml_text = r#"
default = "small"
"claude-opus-4-7" = "heavy"
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
        let default_entry = v.get("default").expect("default entry");
        match default_entry {
            AliasValue::Single(s) => assert_eq!(s, "small"),
            other => panic!("expected Single, got {other:?}"),
        }
        assert!(v.contains_key("claude-opus-4-7"));
    }

    #[test]
    fn alias_value_suffix_glob_parses() {
        let toml_text = r#"
"claude-opus-*" = "opus"
"claude-*" = "fallback"
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
        assert!(v.contains_key("claude-opus-*"));
        assert!(v.contains_key("claude-*"));
    }

    #[test]
    fn provider_kind_field_is_kind_not_type() {
        let toml_text = r#"
[providers.deepseek]
kind = "openai-compat"
base_url = "https://api.deepseek.com/v1"
api_key_ref = "literal:k"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert!(cfg.providers.contains_key("deepseek"));
    }

    #[test]
    fn model_entry_disabled_field() {
        let toml_text = r#"
[models.shelved]
provider = "p"
upstream = "u"
selectable = false
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("shelved").expect("entry");
        assert!(!m.selectable);
    }

    /// `ModelEntry::new` defaults match the TOML defaults: selectable=true,
    /// supports_adaptive_thinking=false, effort_levels=["low","medium","high"],
    /// max_thinking_budget=0.
    #[test]
    fn model_entry_builder_defaults_match_toml_defaults() {
        let m = ModelEntry::new("p", "u");
        assert_eq!(m.provider, "p");
        assert_eq!(m.upstream, "u");
        assert!(m.selectable);
        assert!(!m.supports_adaptive_thinking);
        assert_eq!(
            m.effort_levels,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        assert_eq!(m.max_thinking_budget, 0);
    }

    /// Builder methods for the new capability fields work correctly.
    #[test]
    fn model_entry_capability_builders() {
        let m = ModelEntry::new("p", "u")
            .with_supports_adaptive_thinking(true)
            .with_effort_levels(vec!["low".into(), "high".into(), "max".into()])
            .with_max_thinking_budget(16000);
        assert!(m.supports_adaptive_thinking);
        assert_eq!(
            m.effort_levels,
            vec!["low".to_string(), "high".to_string(), "max".to_string()]
        );
        assert_eq!(m.max_thinking_budget, 16000);
    }

    #[test]
    fn alias_value_chain_iter_yields_in_order() {
        let v = AliasValue::Chain(vec!["a".into(), "b".into(), "c".into()]);
        let names: Vec<&str> = v.nicknames().collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn alias_value_single_iter_yields_one() {
        let v = AliasValue::Single("solo".into());
        let names: Vec<&str> = v.nicknames().collect();
        assert_eq!(names, vec!["solo"]);
    }

    #[test]
    fn model_entry_stream_first_byte_timeout_ms_round_trip() {
        let toml_text = r#"
[models.opus]
provider = "p"
upstream = "u"
stream_first_byte_timeout_ms = 300000
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(m.stream_first_byte_timeout_ms, Some(300_000));
    }

    /// A config without a `[log]` block parses cleanly and yields a
    /// default `LogConfig` with every field `None`. Missing block ==
    /// "current behavior unchanged" (env-only or hardcoded default).
    #[test]
    fn log_block_absent_yields_all_none() {
        let toml_text = r#"
[server]
host = "127.0.0.1"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse default");
        assert!(cfg.log.trace_headers.is_none());
        assert!(cfg.log.trace_body_bytes.is_none());
        assert!(cfg.log.redact_prompts.is_none());
    }

    /// A `[log]` block carrying only `redact_prompts` parses with the
    /// other two fields left as `None`. Round-trips through serde so
    /// the operator's partial config survives a serialize/deserialize
    /// loop (e.g. `config show`).
    #[test]
    fn log_block_partial_redact_only_round_trips() {
        let toml_text = r#"
[log]
redact_prompts = true
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse partial");
        assert!(cfg.log.trace_headers.is_none());
        assert!(cfg.log.trace_body_bytes.is_none());
        assert_eq!(cfg.log.redact_prompts, Some(true));

        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
        assert!(cfg_out.log.trace_headers.is_none());
        assert!(cfg_out.log.trace_body_bytes.is_none());
        assert_eq!(cfg_out.log.redact_prompts, Some(true));
    }

    /// Every `[log]` field present parses, every value reaches the
    /// `LogConfig`, and the round-trip stays stable across one
    /// serialize/deserialize loop. Pins field-name spelling so a
    /// rename here surfaces against `docs/CONFIGURATION.md`.
    #[test]
    fn log_block_full_round_trips() {
        let toml_text = r#"
[log]
trace_headers = true
trace_body_bytes = 32768
redact_prompts = true
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse full");
        assert_eq!(cfg.log.trace_headers, Some(true));
        assert_eq!(cfg.log.trace_body_bytes, Some(32768));
        assert_eq!(cfg.log.redact_prompts, Some(true));

        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
        assert_eq!(cfg_out.log.trace_headers, cfg.log.trace_headers);
        assert_eq!(cfg_out.log.trace_body_bytes, cfg.log.trace_body_bytes);
        assert_eq!(cfg_out.log.redact_prompts, cfg.log.redact_prompts);
    }

    /// Unknown fields in `[log]` reject at parse time so a typo
    /// (`trace_body_byte` vs `trace_body_bytes`) surfaces at startup
    /// rather than silently dropping the override.
    #[test]
    fn log_block_rejects_unknown_field() {
        let toml_text = r#"
[log]
trace_body_byte = 1024
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("trace_body_byte") || msg.contains("unknown field"),
            "expected unknown-field error; got: {msg}"
        );
    }

    /// `max_thinking_entry_bytes` round-trips through TOML and defaults
    /// to `None` when omitted (the runtime falls back to the default
    /// 1 MiB cap).
    #[test]
    fn anthropic_api_max_thinking_entry_bytes_round_trip() {
        use crate::config::{Config, ProviderEntry};

        // Default: omitted -> None.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse default");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");
        match entry {
            ProviderEntry::AnthropicApi {
                max_thinking_entry_bytes,
                ..
            } => assert!(
                max_thinking_entry_bytes.is_none(),
                "default must be None; got: {max_thinking_entry_bytes:?}"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }

        // Explicit value round-trips through serde.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
max_thinking_entry_bytes = 2097152
"#;
        let cfg_in: Config = toml::from_str(toml_text).expect("parse explicit");
        match cfg_in.providers.get("anthropic").expect("anthropic") {
            ProviderEntry::AnthropicApi {
                max_thinking_entry_bytes,
                ..
            } => assert_eq!(*max_thinking_entry_bytes, Some(2_097_152)),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
        let serialized = toml::to_string(&cfg_in).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
        match cfg_out.providers.get("anthropic").expect("anthropic") {
            ProviderEntry::AnthropicApi {
                max_thinking_entry_bytes,
                ..
            } => assert_eq!(*max_thinking_entry_bytes, Some(2_097_152)),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// When `max_thinking_entry_bytes` is unset on the TOML, the
    /// runtime resolution lands on the
    /// `AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES` baseline (1 MiB).
    #[test]
    fn anthropic_api_max_thinking_entry_bytes_unset_resolves_to_default() {
        use crate::config::ProviderEntry;
        use crate::factory::resolve_max_thinking_entry_bytes_for_test;
        use routectl_providers::anthropic_api::AnthropicApiConfig;

        let entry = ProviderEntry::anthropic_api("literal:sk-ant-test");
        let configured = match &entry {
            ProviderEntry::AnthropicApi {
                max_thinking_entry_bytes,
                ..
            } => *max_thinking_entry_bytes,
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        };
        assert!(configured.is_none(), "constructor must default to None");
        let resolved = resolve_max_thinking_entry_bytes_for_test("test", configured);
        assert_eq!(
            resolved,
            AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            "None must resolve to the 1 MiB default"
        );
        assert_eq!(resolved, 1024 * 1024, "default must be 1 MiB");
    }

    #[test]
    fn max_thinking_entry_bytes_zero_resolves_to_default() {
        let resolved = crate::factory::resolve_max_thinking_entry_bytes_for_test("test", Some(0));
        assert_eq!(
            resolved,
            routectl_providers::anthropic_api::AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            "Some(0) must fall back to the default cap, not zero"
        );
    }
}

impl RetryPolicy {
    /// True when the given upstream HTTP status (>= 400) is eligible
    /// for fallback to the next provider in the chain. Status 0
    /// (network errors) is handled separately in `should_fallback` and
    /// always falls back regardless of this predicate.
    ///
    /// Resolution order:
    ///
    ///   1. `retry_allowlist` non-empty -- contains check (any code in
    ///      the list is fallbackable; everything else is terminal).
    ///   2. `retry_denylist` is `Some` -- 400..=599 minus the list is
    ///      fallbackable; everything else is terminal.
    ///   3. otherwise -- every 400..=599 is fallbackable.
    ///
    /// `retry_allowlist` non-empty AND `retry_denylist` `Some` is a
    /// config-load error (`validate_retry_policy`); this method
    /// preserves the allowlist's outcome if both are nevertheless
    /// constructed in code.
    pub fn is_fallbackable_status(&self, status: u16) -> bool {
        if !(400..=599).contains(&status) {
            return false;
        }
        if !self.retry_allowlist.is_empty() {
            return self.retry_allowlist.contains(&status);
        }
        if let Some(denylist) = &self.retry_denylist {
            return !denylist.contains(&status);
        }
        true
    }

    /// Resolve the retry cap for a given upstream HTTP status code.
    /// Returns 0 for non-retryable errors.
    ///
    /// Both the 429 arm and the 5xx arm are gated on
    /// `is_fallbackable_status`. A status excluded by the allowlist
    /// (or named in the denylist) is treated as non-retryable here AND
    /// as non-fallbackable in `should_fallback`, so it propagates
    /// immediately to the caller. This is intentional: an operator who
    /// excludes a status from the fallback predicate is asking routectl
    /// to surface the error verbatim, and silently retrying anyway
    /// would contradict that.
    pub fn retries_for_status(&self, status: u16) -> u32 {
        match status {
            0 => self.retry_on_network.unwrap_or(self.max_attempts),
            429 if self.is_fallbackable_status(429) => {
                self.retry_on_429.unwrap_or(self.max_attempts)
            }
            s if (500..600).contains(&s) && self.is_fallbackable_status(s) => {
                self.retry_on_5xx.unwrap_or(self.max_attempts)
            }
            _ => 0,
        }
    }

    /// Maximum attempts a single provider can ever consume regardless
    /// of error class. The router uses this as a hard ceiling so a
    /// misconfigured policy can't loop forever.
    pub fn hard_retry_cap(&self) -> u32 {
        self.max_attempts
            .max(self.retry_on_429.unwrap_or(0))
            .max(self.retry_on_5xx.unwrap_or(0))
            .max(self.retry_on_network.unwrap_or(0))
            .max(1)
    }
}

fn default_max_attempts() -> u32 {
    2
}

fn default_probe_max_tokens() -> u32 {
    1
}

fn default_backoff_ms() -> u64 {
    250
}

fn default_backoff_multiplier() -> f64 {
    2.0
}

#[cfg(test)]
mod is_fallbackable_status_tests {
    //! Cover every branch of `RetryPolicy::is_fallbackable_status`
    //! plus the mutually-exclusive `validate_retry_policy` guard.
    //! Each test names the input shape it exercises.
    use super::RetryPolicy;

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    #[test]
    fn allowlist_hit_returns_true() {
        let mut p = policy();
        p.retry_allowlist = vec![503];
        p.retry_denylist = None;
        assert!(p.is_fallbackable_status(503));
    }

    #[test]
    fn allowlist_miss_returns_false() {
        let mut p = policy();
        p.retry_allowlist = vec![503];
        p.retry_denylist = None;
        // 500 is a 5xx but not in the allowlist -- terminal.
        assert!(!p.is_fallbackable_status(500));
    }

    #[test]
    fn denylist_hit_returns_false() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = Some(vec![501]);
        assert!(!p.is_fallbackable_status(501));
    }

    #[test]
    fn denylist_miss_returns_true_for_4xx_5xx() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = Some(vec![501]);
        // 503 is in 4xx..=5xx and not in the denylist -- fallbackable.
        assert!(p.is_fallbackable_status(503));
    }

    #[test]
    fn neither_set_returns_true_for_4xx_and_5xx() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = None;
        assert!(p.is_fallbackable_status(400), "400 must fall back");
        assert!(p.is_fallbackable_status(429), "429 must fall back");
        assert!(p.is_fallbackable_status(500), "500 must fall back");
        assert!(p.is_fallbackable_status(599), "599 must fall back");
    }

    #[test]
    fn neither_set_returns_false_for_2xx() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = None;
        // 2xx are never fallbackable; the predicate is gated on
        // 400..=599 even when neither list is configured.
        assert!(!p.is_fallbackable_status(200));
        assert!(!p.is_fallbackable_status(204));
        assert!(!p.is_fallbackable_status(301));
    }

    #[test]
    fn default_policy_retries_520_via_fallthrough() {
        // With the default RetryPolicy (empty allowlist + None denylist),
        // 520 is fallbackable because the predicate's "every 4xx/5xx"
        // branch fires, not because 520 is on a hard-coded list.
        let policy = RetryPolicy::default();
        let retries = policy.retries_for_status(520);
        assert_eq!(retries, policy.max_attempts);
    }

    #[test]
    fn denylist_only_toml_parses_and_validates() {
        // Regression: a config containing only `retry_denylist = [422]`
        // (no `retry_allowlist`) must deserialize, validate, and yield
        // the expected predicate behavior. Before the fix, the implicit
        // non-empty default for `retry_allowlist` collided with
        // `retry_denylist` in `validate_retry_policy`, breaking
        // denylist-only configs.
        use crate::config::Config;
        use crate::factory::validate_retry_policy;

        let toml_text = r#"
[retry]
retry_denylist = [422]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse denylist-only");
        let p = &cfg.retry;
        assert!(
            p.retry_allowlist.is_empty(),
            "default retry_allowlist must be empty; got: {:?}",
            p.retry_allowlist
        );
        assert_eq!(p.retry_denylist, Some(vec![422]));

        // Predicate semantics: every 4xx/5xx except 422 falls back.
        assert!(p.is_fallbackable_status(503), "503 must fall back");
        assert!(p.is_fallbackable_status(500), "500 must fall back");
        assert!(p.is_fallbackable_status(404), "404 must fall back");
        assert!(!p.is_fallbackable_status(422), "422 must NOT fall back");

        validate_retry_policy(&cfg).expect("denylist-only config must validate");
    }

    #[test]
    fn validate_retry_policy_rejects_both_set() {
        // Mutually exclusive: non-empty allowlist + Some denylist is
        // a config-load error.
        use crate::config::Config;
        use crate::factory::validate_retry_policy;

        let mut cfg = Config::default();
        cfg.retry.retry_allowlist = vec![503];
        cfg.retry.retry_denylist = Some(vec![501]);
        let err = validate_retry_policy(&cfg).expect_err("must reject both-set");
        let msg = err.to_string();
        assert!(
            msg.contains("retry_allowlist") && msg.contains("retry_denylist"),
            "error must name both fields; got: {msg}"
        );

        // Sanity: each on its own is fine.
        let mut cfg2 = Config::default();
        cfg2.retry.retry_allowlist = vec![503];
        cfg2.retry.retry_denylist = None;
        validate_retry_policy(&cfg2).expect("allowlist alone must validate");

        let mut cfg3 = Config::default();
        cfg3.retry.retry_allowlist = vec![];
        cfg3.retry.retry_denylist = Some(vec![501]);
        validate_retry_policy(&cfg3).expect("denylist alone must validate");

        // Default config (empty allowlist, None denylist) must validate.
        let cfg4 = Config::default();
        validate_retry_policy(&cfg4).expect("default config must validate");
    }

    #[test]
    fn probe_max_tokens_defaults_to_one_when_omitted() {
        // A `[retry]` block that omits `probe_max_tokens` defaults to 1
        // (Claude Code's max_tokens=1 probe is detected out of the box).
        use crate::config::Config;
        let toml_text = r#"
[retry]
max_attempts = 3
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.retry.probe_max_tokens, 1);
        assert_eq!(cfg.retry.max_attempts, 3, "other fields unaffected");
    }

    #[test]
    fn probe_max_tokens_zero_parses_to_disable() {
        // `probe_max_tokens = 0` is the disable sentinel and round-trips.
        use crate::config::Config;
        let toml_text = r#"
[retry]
probe_max_tokens = 0
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.retry.probe_max_tokens, 0);
    }

    #[test]
    fn default_retry_policy_has_probe_max_tokens_one() {
        // The Default impl (no `[retry]` block at all) also yields 1.
        assert_eq!(RetryPolicy::default().probe_max_tokens, 1);
    }

    /// context_management = true round-trips through TOML deserialization.
    #[test]
    fn provider_entry_anthropic_api_context_management_round_trips_true() {
        use crate::config::{Config, ProviderEntry};
        // Arrange
        let toml_text = r#"
[providers.deepseek]
kind = "anthropic-api"
base_url = "https://api.deepseek.com/anthropic"
api_key_ref = "env://DS_KEY"
auth_kind = "oauth-bearer"
context_management = true
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let entry = cfg.providers.get("deepseek").expect("deepseek provider");

        // Assert
        match entry {
            ProviderEntry::AnthropicApi {
                context_management, ..
            } => assert!(
                *context_management,
                "context_management = true must deserialize as true"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// context_management omitted from TOML defaults to false.
    #[test]
    fn provider_entry_anthropic_api_context_management_defaults_false() {
        use crate::config::{Config, ProviderEntry};
        // Arrange: no context_management key in TOML.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");

        // Assert
        match entry {
            ProviderEntry::AnthropicApi {
                context_management, ..
            } => assert!(
                !context_management,
                "context_management must default to false when omitted; got {context_management}"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// context_management = false round-trips through TOML deserialization.
    #[test]
    fn provider_entry_anthropic_api_context_management_round_trips_false() {
        use crate::config::{Config, ProviderEntry};
        // Arrange
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
context_management = false
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");

        // Assert
        match entry {
            ProviderEntry::AnthropicApi {
                context_management, ..
            } => assert!(
                !context_management,
                "context_management = false must deserialize as false"
            ),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    // v0.8 cap-relaxation knobs: serde round-trip pins so a default,
    // an explicit override, and a typo all surface correctly.

    /// Server-level `max_body_bytes` defaults to the documented value
    /// when omitted from `[server]`.
    #[test]
    fn server_cap_knobs_default_when_omitted() {
        use crate::config::Config;
        let toml_text = r#"
[server]
host = "127.0.0.1"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.server.max_body_bytes, 32 * 1024 * 1024);
    }

    /// Explicit value for the `[server] max_body_bytes` knob parses
    /// and round-trips through serde.
    #[test]
    fn server_cap_knobs_explicit_values_round_trip() {
        use crate::config::Config;
        let toml_text = r#"
[server]
max_body_bytes = 67108864
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.server.max_body_bytes, 67_108_864);
    }

    /// Per-model `max_output_tokens` defaults to None when omitted and
    /// round-trips when set.
    #[test]
    fn model_entry_max_output_tokens_round_trip() {
        use crate::config::Config;
        let toml_text = r#"
[models.opus4]
provider = "anthropic"
upstream = "claude-opus-4"
max_output_tokens = 32000
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus4").expect("entry");
        assert_eq!(m.max_output_tokens, Some(32000));

        let toml_default = r#"
[models.haiku]
provider = "anthropic"
upstream = "claude-haiku-4-5"
"#;
        let cfg: Config = toml::from_str(toml_default).expect("parse");
        let m = cfg.models.get("haiku").expect("entry");
        assert!(m.max_output_tokens.is_none(), "default must be None");
    }

    /// A typo on the per-model `max_output_tokens` knob surfaces at
    /// parse time (the per-model table opts into `deny_unknown_fields`).
    #[test]
    fn model_entry_rejects_typo_on_max_output_tokens() {
        use crate::config::Config;
        let toml_text = r#"
[models.x]
provider = "p"
upstream = "u"
max_output_token = 32000
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("max_output_token") || msg.contains("unknown field"),
            "expected unknown-field error; got: {msg}"
        );
    }
}

#[cfg(test)]
mod retries_for_status_tests {
    //! Pin the symmetry contract: both the 429 arm and the 5xx arm of
    //! `retries_for_status` honor `is_fallbackable_status`. A status
    //! excluded from the fallback predicate (via allowlist miss or
    //! denylist hit) must also return 0 from `retries_for_status`.
    use super::RetryPolicy;

    /// Regression guard: default policy (no allowlist/denylist
    /// restrictions) must still retry 429 up to max_attempts.
    #[test]
    fn default_policy_retries_429_unchanged() {
        let p = RetryPolicy::default();
        assert!(
            p.is_fallbackable_status(429),
            "default policy: 429 must be fallbackable"
        );
        assert_eq!(
            p.retries_for_status(429),
            p.max_attempts,
            "default policy: retries_for_status(429) must equal max_attempts"
        );
    }

    /// When an operator puts 429 in `retry_denylist`, both
    /// `is_fallbackable_status` and `retries_for_status` must return
    /// false / 0 -- the error propagates verbatim.
    #[test]
    fn denylist_excludes_429_makes_it_non_retryable() {
        let p = RetryPolicy {
            retry_allowlist: vec![],
            retry_denylist: Some(vec![429]),
            ..RetryPolicy::default()
        };
        assert!(
            !p.is_fallbackable_status(429),
            "denylist=[429]: is_fallbackable_status must be false"
        );
        assert_eq!(
            p.retries_for_status(429),
            0,
            "denylist=[429]: retries_for_status(429) must be 0"
        );
    }

    /// When an operator uses an explicit `retry_allowlist` that does
    /// not include 429 (e.g. only [500, 502]), 429 is excluded from
    /// the fallback predicate and must also be non-retryable.
    #[test]
    fn allowlist_without_429_makes_it_non_retryable() {
        let p = RetryPolicy {
            retry_allowlist: vec![500, 502],
            retry_denylist: None,
            ..RetryPolicy::default()
        };
        assert!(
            !p.is_fallbackable_status(429),
            "allowlist=[500,502]: 429 not in list, must not be fallbackable"
        );
        assert_eq!(
            p.retries_for_status(429),
            0,
            "allowlist=[500,502]: retries_for_status(429) must be 0"
        );
    }
}
