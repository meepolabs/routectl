//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use routectl_providers::anthropic_api::AuthKind;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Server bind config.
    #[serde(default)]
    pub server: ServerConfig,

    /// Provider definitions keyed by operator-facing name. Carries
    /// transport-side knobs only (auth, base URL, headers, runtime
    /// gates). Per-model knobs (`thinking`, `enabled`,
    /// `adaptive_thinking`, `additional_request_fields`) live on
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

    /// Schema compatibility mode for the outward shape.
    /// `openrouter` (default): full reasoning_details surface.
    /// `openai`: strip routectl/openrouter extensions for paranoid clients.
    #[serde(default)]
    pub legacy_compat: LegacyCompat,

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

    /// Thinking wire-shape selector (v0.6.0 rewrite of the legacy
    /// `adaptive_thinking: bool` + effort-conflated `thinking: String`
    /// pair). `false` -> reasoning off; `true` -> legacy enabled-shape
    /// thinking; `"adaptive"` -> Opus 4.7+ adaptive shape. Caller
    /// `reasoning.enabled = false` still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingChoice>,

    /// Reasoning effort vocabulary. One of `minimal`, `low`, `medium`,
    /// `high`, `xhigh`, `max`. Validated at parse time. Lifts to
    /// `req.reasoning.effort` via the router's reasoning-defaults
    /// merge (caller-supplied wire value wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,

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
}

impl ModelEntry {
    pub fn new(provider: impl Into<String>, upstream: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            upstream: upstream.into(),
            selectable: true,
            thinking: None,
            effort: None,
            reasoning_dialect: None,
            history_reasoning: None,
            additional_request_fields: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            stream_first_byte_timeout_ms: None,
        }
    }

    pub fn with_thinking(mut self, t: ThinkingChoice) -> Self {
        self.thinking = Some(t);
        self
    }

    pub fn with_effort(mut self, e: EffortLevel) -> Self {
        self.effort = Some(e);
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

    /// True when the model has any thinking / effort knob set. Drives
    /// the router's reasoning-defaults merge (skip the work when both
    /// fields are unset).
    pub fn has_reasoning_overrides(&self) -> bool {
        self.thinking.is_some() || self.effort.is_some()
    }

    /// True when the model opts into the Opus 4.7+ adaptive thinking
    /// shape. Read by the AnthropicApi factory path so each
    /// adaptive-thinking model gets its own `AnthropicApiProvider`
    /// instance with `cfg.adaptive_thinking = Some(true)`.
    pub fn is_adaptive_thinking(&self) -> bool {
        matches!(self.thinking, Some(ThinkingChoice::Adaptive))
    }
}

/// v0.6.0 thinking wire-shape selector. Untagged so the TOML stays
/// terse:
///
/// ```toml
/// thinking = false        # reasoning off
/// thinking = true         # legacy "enabled" shape
/// thinking = "adaptive"   # Opus 4.7+ adaptive shape
/// ```
///
/// Anything else (`"true"`, integers, unknown strings, `"on"`,
/// `"enabled"`) rejects at parse time with a clear error so a typo
/// surfaces at startup rather than at request time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingChoice {
    Bool(bool),
    Adaptive,
}

impl<'de> Deserialize<'de> for ThinkingChoice {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{Error as DeError, Unexpected};
        let v = Value::deserialize(deserializer)?;
        match &v {
            Value::Bool(b) => Ok(ThinkingChoice::Bool(*b)),
            Value::String(s) => {
                if s == "adaptive" {
                    Ok(ThinkingChoice::Adaptive)
                } else {
                    Err(D::Error::invalid_value(
                        Unexpected::Str(s),
                        &"`true`, `false`, or the string `\"adaptive\"`",
                    ))
                }
            }
            other => Err(D::Error::invalid_type(
                serde_value_unexpected(other),
                &"`true`, `false`, or the string `\"adaptive\"`",
            )),
        }
    }
}

impl Serialize for ThinkingChoice {
    // Custom serialize: emit a JSON-friendly shape that matches what
    // Deserialize accepts. (Derived `Serialize` on the untagged enum
    // would serialize `Adaptive` as `null`.)
    //
    // This is the active impl; the derived `Serialize` above is
    // overridden via re-derivation suppression in the untagged
    // discriminator. To keep the source readable we define the body
    // here so future readers can see the wire shape directly.
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ThinkingChoice::Bool(b) => serializer.serialize_bool(*b),
            ThinkingChoice::Adaptive => serializer.serialize_str("adaptive"),
        }
    }
}

fn serde_value_unexpected(v: &Value) -> serde::de::Unexpected<'_> {
    use serde::de::Unexpected;
    match v {
        Value::Null => Unexpected::Unit,
        Value::Bool(b) => Unexpected::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Unexpected::Signed(i)
            } else if let Some(u) = n.as_u64() {
                Unexpected::Unsigned(u)
            } else if let Some(f) = n.as_f64() {
                Unexpected::Float(f)
            } else {
                Unexpected::Other("number")
            }
        }
        Value::String(s) => Unexpected::Str(s),
        Value::Array(_) => Unexpected::Seq,
        Value::Object(_) => Unexpected::Map,
    }
}

/// Validated reasoning-effort vocabulary. v0.6.0 introduces a closed
/// enum at the config layer; unknown values reject at TOML parse with a
/// clear error. The egress side still passes the effort STRING through
/// verbatim so vendors can add new levels without a routectl release --
/// when that day comes, add a new variant here and route it through
/// `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl EffortLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            EffortLevel::Minimal => "minimal",
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }
}

fn default_true() -> bool {
    true
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

/// Operator-side reasoning defaults. Folds into ChatRequest.reasoning
/// per-attempt at the router; caller's non-None values always win.
///
/// Both fields are optional and default to None. Setting either populates
/// the corresponding `ChatRequest.reasoning.{effort,enabled}` field on
/// requests routing through this provider, but only when the caller did
/// not already supply that field on the wire. The router never
/// overwrites a caller-supplied value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReasoningDefaults {
    /// Maps to ChatRequest.reasoning.effort. Vocabulary is passthrough
    /// (egresses interpret); empty string rejected at startup. Common
    /// values: "minimal", "low", "medium", "high", "xhigh", "max",
    /// "none". Unknown values pass through verbatim for forward
    /// compatibility with vendor-specific levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Maps to ChatRequest.reasoning.enabled. `Some(true)` opts into
    /// reasoning by default for this provider; `Some(false)` pins it
    /// off; `None` defers to whatever the caller and provider's own
    /// defaults decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl ReasoningDefaults {
    /// Construct an empty `ReasoningDefaults`. Use the `with_thinking`
    /// / `with_enabled` builders to populate fields. Builder pattern
    /// matches the rest of the config surface.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the operator-side `thinking` (effort) value. Replaces any
    /// previously-set value.
    pub fn with_thinking(mut self, thinking: impl Into<String>) -> Self {
        self.thinking = Some(thinking.into());
        self
    }

    /// Set the operator-side `enabled` flag. Replaces any
    /// previously-set value.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// True when neither `thinking` nor `enabled` is set. Used by the
    /// router to skip the merge step entirely on providers without
    /// configured defaults.
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none() && self.enabled.is_none()
    }
}

impl ModelEntry {
    /// Project the per-model `thinking` + `effort` knobs into a
    /// `ReasoningDefaults` so the router's existing
    /// `merge_reasoning_defaults_into` helper can lift them onto
    /// `req.reasoning` per attempt. Returns an empty
    /// `ReasoningDefaults` when neither knob is set.
    pub fn reasoning_defaults_view(&self) -> ReasoningDefaults {
        let mut d = ReasoningDefaults::default();
        if let Some(effort) = self.effort {
            d.thinking = Some(effort.as_str().to_string());
        }
        match self.thinking {
            Some(ThinkingChoice::Bool(b)) => d.enabled = Some(b),
            // Adaptive means thinking is on; pair with effort. The
            // egress consults `req.routectl_internal` / config to
            // decide the wire shape (legacy vs adaptive); enabled = true
            // here is the canonical "thinking on" signal regardless of
            // wire shape.
            Some(ThinkingChoice::Adaptive) => d.enabled = Some(true),
            None => {}
        }
        d
    }
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
}

fn default_allow_disable_fallbacks() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: default_allow_disable_fallbacks(),
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
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    /// OpenAI Responses API provider. Three auth surfaces (CG.A wires
    /// the first; CG.D/E land the others):
    /// - `chatgpt-oauth`: ChatGPT subscription JWT.
    /// - `api-key`: standard OpenAI API key.
    /// - `bedrock-mantle`: AWS Mantle proxy over SigV4.
    ///
    /// `base_url` is optional: when unset, the factory picks the
    /// auth_kind-appropriate default at provider build time.
    #[cfg(feature = "openai-responses")]
    #[non_exhaustive]
    OpenaiResponses {
        /// Resolves to the bearer JWT (ChatgptOauth) or API key
        /// (ApiKey). Ignored for BedrockMantle which signs via SigV4.
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
        /// Override the `originator` header on the ChatgptOauth surface.
        /// None -> `codex_cli_rs` (codex's `DEFAULT_ORIGINATOR`).
        #[serde(default)]
        originator: Option<String>,
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    OpenaiCompat,
    AnthropicApi,
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
    /// listed codes are fallbackable. Mutually exclusive with
    /// `retry_denylist` -- setting both is a config-load error.
    #[serde(default = "default_retry_allowlist")]
    pub retry_allowlist: Vec<u16>,

    /// Inverse of `retry_allowlist`: when `Some`, every 4xx/5xx code
    /// EXCEPT those in the list triggers fallback. `None` defers to
    /// `retry_allowlist` (or, when both are empty/None, the default
    /// "all 4xx/5xx fall back" predicate). Mutually exclusive with a
    /// non-empty `retry_allowlist` -- setting both is a config-load
    /// error.
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
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            jitter_ms: 0,
            retry_allowlist: default_retry_allowlist(),
            retry_denylist: None,
            retry_on_429: None,
            retry_on_5xx: None,
            retry_on_network: None,
            request_timeout_ms: None,
            stream_first_byte_timeout_ms: None,
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
}

#[cfg(test)]
mod v0_6_config_tests {
    //! Tests for the v0.6.0 config shapes: `[models]` table and the
    //! untagged `AliasValue` enum.

    use super::{
        AliasValue, Config, EffortLevel, HistoryReasoning, ModelEntry, ReasoningDialect,
        ThinkingChoice,
    };
    use std::collections::BTreeMap;

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
        assert!(m.thinking.is_none());
        assert!(m.effort.is_none());
        assert!(m.reasoning_dialect.is_none());
        assert!(m.history_reasoning.is_none());
        assert!(m.header_extras.is_empty());
        assert!(m.payload_extras.is_none());
    }

    #[test]
    fn model_entry_thinking_bool_true_parses() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7"
thinking = true
effort = "high"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(m.thinking, Some(ThinkingChoice::Bool(true)));
        assert_eq!(m.effort, Some(EffortLevel::High));
    }

    #[test]
    fn model_entry_thinking_bool_false_parses() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7"
thinking = false
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(m.thinking, Some(ThinkingChoice::Bool(false)));
    }

    #[test]
    fn model_entry_thinking_adaptive_parses() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7"
thinking = "adaptive"
effort = "xhigh"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(m.thinking, Some(ThinkingChoice::Adaptive));
        assert_eq!(m.effort, Some(EffortLevel::Xhigh));
        assert!(m.is_adaptive_thinking());
    }

    #[test]
    fn model_entry_rejects_unknown_thinking_string() {
        // `"true"` (string), `"on"`, `"enabled"`, anything that isn't
        // the literal `"adaptive"` rejects.
        for bad in ["\"true\"", "\"on\"", "\"enabled\"", "\"high\"", "42"] {
            let toml_text = format!(
                r#"
[models.bad]
provider = "p"
upstream = "u"
thinking = {bad}
"#
            );
            let err = toml::from_str::<Config>(&toml_text).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("thinking") || msg.contains("adaptive"),
                "expected error to mention thinking/adaptive for input {bad:?}; got: {msg}"
            );
        }
    }

    #[test]
    fn model_entry_rejects_unknown_effort_value() {
        let toml_text = r#"
[models.bad]
provider = "p"
upstream = "u"
effort = "unknown"
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("effort") || msg.contains("unknown"),
            "expected error to mention effort; got: {msg}"
        );
    }

    #[test]
    fn model_entry_accepts_all_effort_levels() {
        for level in ["minimal", "low", "medium", "high", "xhigh", "max"] {
            let toml_text = format!(
                r#"
[models.m]
provider = "p"
upstream = "u"
effort = "{level}"
"#
            );
            let cfg: Config = toml::from_str(&toml_text).expect("parse");
            let m = cfg.models.get("m").expect("entry");
            assert!(m.effort.is_some(), "effort = {level:?} should parse");
        }
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

    #[test]
    fn model_entry_builder_helper_matches_required_only_parse() {
        let m = ModelEntry::new("p", "u");
        assert_eq!(m.provider, "p");
        assert_eq!(m.upstream, "u");
        assert!(m.selectable);
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

    #[test]
    fn reasoning_defaults_view_projects_thinking_and_effort() {
        // The router lifts `thinking` + `effort` into ReasoningDefaults
        // so the existing merge helper handles per-request injection.
        let m = ModelEntry::new("p", "u")
            .with_thinking(ThinkingChoice::Adaptive)
            .with_effort(EffortLevel::Xhigh);
        let d = m.reasoning_defaults_view();
        assert_eq!(d.thinking.as_deref(), Some("xhigh"));
        assert_eq!(d.enabled, Some(true));
    }

    #[test]
    fn reasoning_defaults_view_thinking_false_disables() {
        let m = ModelEntry::new("p", "u").with_thinking(ThinkingChoice::Bool(false));
        let d = m.reasoning_defaults_view();
        assert_eq!(d.enabled, Some(false));
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
    /// Note: `retry_on_5xx` only applies to 5xx codes that are ALSO
    /// fallbackable per `is_fallbackable_status`. A 5xx code excluded
    /// by the allowlist (or named in the denylist) is treated as
    /// non-retryable here AND as non-fallbackable in `should_fallback`,
    /// so it propagates immediately to the caller. This is intentional:
    /// an operator who excludes a status from the fallback predicate
    /// is asking routectl to surface the error verbatim, and silently
    /// retrying anyway would contradict that.
    pub fn retries_for_status(&self, status: u16) -> u32 {
        match status {
            0 => self.retry_on_network.unwrap_or(self.max_attempts),
            429 => self.retry_on_429.unwrap_or(self.max_attempts),
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

fn default_backoff_ms() -> u64 {
    250
}

fn default_backoff_multiplier() -> f64 {
    2.0
}

fn default_retry_allowlist() -> Vec<u16> {
    // Standard 5xx codes plus Cloudflare extended 5xx (520-527, 530).
    // Cloudflare-fronted upstreams (opencode.ai, openrouter.ai, etc.)
    // surface upstream-origin failures via this range; without it,
    // a single 520 from a Cloudflare-fronted provider kills the
    // request even though a sibling provider could have served it.
    vec![
        408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 525, 526, 527, 530,
    ]
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyCompat {
    #[default]
    Openrouter,
    Openai,
}

#[cfg(test)]
mod default_retry_allowlist_tests {
    //! Pin the default `retry_allowlist` vocabulary. Cloudflare
    //! extended 5xx codes (520-527, 530) belong on the default list
    //! because Cloudflare-fronted upstreams (opencode.ai, openrouter.ai,
    //! etc.) surface upstream-origin failures via this range; without
    //! them, a single 520 from a Cloudflare-fronted provider kills the
    //! request even though a sibling provider could have served it.
    use super::{default_retry_allowlist, RetryPolicy};

    #[test]
    fn default_retry_allowlist_contains_legacy_codes() {
        // Pin: existing operator configs depend on these codes being
        // present. Pre-extension list, all six must survive.
        let list = default_retry_allowlist();
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(
                list.contains(&code),
                "expected default retry allowlist to contain legacy code {code}; got {list:?}"
            );
        }
    }

    #[test]
    fn default_retry_allowlist_contains_cloudflare_codes() {
        // Pin: Cloudflare-extended 5xx range (520-527 + 530) is on
        // the default fallback list.
        let list = default_retry_allowlist();
        for code in [520, 521, 522, 523, 524, 525, 526, 527, 530] {
            assert!(
                list.contains(&code),
                "expected default retry allowlist to contain Cloudflare code {code}; got {list:?}"
            );
        }
    }

    #[test]
    fn default_retry_allowlist_does_not_contain_unrelated_5xx() {
        // Pin: codes that are NOT eligible for retry stay off the
        // default list. 501 (Not Implemented) is terminal and must
        // never be retried automatically.
        let list = default_retry_allowlist();
        assert!(
            !list.contains(&501),
            "501 (Not Implemented) is terminal and must not be on the default retry allowlist"
        );
    }

    #[test]
    fn retries_for_status_treats_cloudflare_520_as_retryable() {
        // Pin: with the default RetryPolicy, a 520 upstream error
        // gets `max_attempts` retries (since 520 is in the default
        // fallback list AND in the 5xx class).
        let policy = RetryPolicy::default();
        let retries = policy.retries_for_status(520);
        assert_eq!(retries, policy.max_attempts);
    }
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

        // Default config (allowlist populated by default, denylist None)
        // must validate.
        let cfg4 = Config::default();
        validate_retry_policy(&cfg4).expect("default config must validate");
    }
}
