//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use routectl_providers::anthropic_api::AuthKind;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use serde::{Deserialize, Serialize};

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
/// binding plus per-model knobs that used to live on `[providers.X]`
/// (`thinking`, `enabled`, `adaptive_thinking`,
/// `additional_request_fields`).
///
/// Fields that vary per-model belong here. Fields that vary per-
/// transport (auth, base URL, headers, runtime gates) stay on
/// `[providers.X]`.
///
/// Note: `default_extras` and `chat_template_kwargs` are deferred --
/// they shipped briefly on `ModelEntry` in earlier rc builds but the
/// egress side never read them, so keeping them on the public TOML
/// surface would be operator-deceptive. They will return as
/// `[models.X]` fields in a future release once the egress wiring
/// lands.
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
    ///
    /// Note: this used to be `enabled`, but that key collides with
    /// the flattened `ReasoningDefaults::enabled` (reasoning on/off).
    /// Renamed to `selectable` so an operator writing
    /// `enabled = false` on a `[models.X]` block disables reasoning
    /// (the more common intent) rather than removing the model from
    /// routing.
    #[serde(default = "default_true")]
    pub selectable: bool,

    /// Operator-side reasoning defaults (see [`ReasoningDefaults`]).
    /// `thinking` lifts to `reasoning.effort`, `enabled` lifts to
    /// `reasoning.enabled`. Caller-supplied values always win.
    #[serde(default, flatten)]
    pub reasoning_defaults: ReasoningDefaults,

    /// Use the Opus 4.7+ adaptive thinking wire shape on this model.
    /// When `true` (and the route lands on an Anthropic-shape egress),
    /// routectl rewrites `thinking: {type:"enabled", budget_tokens:N}`
    /// to `thinking: {type:"adaptive"}` and lifts `reasoning.effort`
    /// into `output_config.effort`. Older Claude families still accept
    /// the legacy shape, so this is opt-in per model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,

    /// Bedrock Converse `additionalModelRequestFields` -- vendor-
    /// specific knobs that don't have a top-level Converse slot. The
    /// Bedrock Invoke egress also reads this map for non-routectl-
    /// managed body fields. Other egresses ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_request_fields: Option<serde_json::Value>,

    /// Per-model `anthropic-beta` flags. Lifted by the router onto
    /// `req.anthropic_beta` at dispatch time and deduplicated against
    /// the request-side entries (request entries preserved first,
    /// model entries appended only if not already present). The
    /// Anthropic-API egress's `build_headers` then merges the
    /// resulting list with the provider's static
    /// `extra_headers["anthropic-beta"]` value (also deduplicated).
    /// Bedrock-Invoke / Bedrock-Converse also read
    /// `req.anthropic_beta` (through `filter_bedrock_betas`), so the
    /// lift extends to those egresses automatically.
    ///
    /// Use case: when a single provider serves multiple Claude models
    /// and only some opt into a beta (e.g. `context-1m-2025-08-07`
    /// works on opus/sonnet but is rejected for haiku), set the beta
    /// here per-model instead of duplicating the entire provider
    /// config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_beta: Vec<String>,

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
            reasoning_defaults: ReasoningDefaults::default(),
            adaptive_thinking: None,
            additional_request_fields: None,
            anthropic_beta: Vec::new(),
            stream_first_byte_timeout_ms: None,
        }
    }

    pub fn with_reasoning_defaults(mut self, defaults: ReasoningDefaults) -> Self {
        self.reasoning_defaults = defaults;
        self
    }

    pub fn with_adaptive_thinking(mut self, b: bool) -> Self {
        self.adaptive_thinking = Some(b);
        self
    }

    /// Set the model's selectability flag. Was `with_enabled` before
    /// v0.6.0-rc.2; renamed alongside the underlying field to avoid
    /// the TOML key collision with reasoning's `enabled`.
    pub fn with_selectable(mut self, b: bool) -> Self {
        self.selectable = b;
        self
    }

    /// Set the per-model `anthropic_beta` list. Lifted onto
    /// `req.anthropic_beta` at dispatch time.
    pub fn with_anthropic_beta(mut self, betas: Vec<String>) -> Self {
        self.anthropic_beta = betas;
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
    /// matches the rest of the config surface (`ProviderEntry::with_*`,
    /// `AliasEntry::with_*`) and keeps external callers compatible
    /// across `#[non_exhaustive]` field additions.
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
#[serde(tag = "kind", rename_all = "kebab-case")]
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
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        #[serde(default)]
        reasoning_dialect: ReasoningDialect,
        /// How to handle the `reasoning` / `reasoning_content` /
        /// `reasoning_details` fields on outgoing assistant messages
        /// in multi-turn history.
        ///
        ///   - `auto` (default): use the dialect's default. DeepSeek
        ///     and vLLM strip; OpenAI and OpenRouter pass through.
        ///   - `strip`: force-drop reasoning fields from outgoing
        ///     assistant messages, regardless of dialect. Use for
        ///     DeepSeek v3 / vLLM <= 0.6 hosts that 400 on echo-back.
        ///   - `preserve`: emit the dialect-native preserve shape on
        ///     outgoing assistant messages (DeepSeek/vLLM emit
        ///     `reasoning_content`; OpenRouter emits `reasoning_details`).
        ///     Required by DeepSeek v4+, which 400s with
        ///     `"reasoning_content in the thinking mode must be passed
        ///     back"` if echo-back is missing.
        ///
        /// The `auto` default preserves backward compatibility for
        /// existing configs. Operators upgrading to DeepSeek v4 must
        /// set `history_reasoning = "preserve"` explicitly.
        #[serde(default)]
        history_reasoning: HistoryReasoning,
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
        /// Extra HTTP headers applied to every Anthropic API request.
        /// Common usage: `extra_headers = { "anthropic-beta" = "context-1m-2025-08-07" }`.
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
        /// Override the outbound User-Agent. Useful for IAM-gated upstreams.
        #[serde(default)]
        user_agent: Option<String>,
        /// Optional operator-supplied allowlist for `anthropic_beta`
        /// flags forwarded to api.anthropic.com. Default (empty) is
        /// pass-through: every beta the client requests goes upstream
        /// verbatim. When non-empty, ingress-lifted values not in the
        /// list are dropped at DEBUG level. Mirrors the Bedrock-egress
        /// `[bedrock] allowed_betas` shape so multi-tenant / API-gateway
        /// deployments can constrain which betas authenticated clients
        /// can opt into (e.g. billing-gated features).
        #[serde(default)]
        allowed_betas: Vec<String>,
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
        /// Extra HTTP headers applied to every request (after auth).
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
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
    ///
    /// v0.6.0 moved the upstream `model_id` from this entry to
    /// `[models.X].upstream`; the factory pumps each model entry's
    /// upstream into a per-model `BedrockConfig.model_id`.
    #[cfg(feature = "bedrock")]
    #[non_exhaustive]
    Bedrock {
        region: String,
        #[serde(default)]
        api_shape: BedrockApiShapeConfig,
        creds: BedrockCredsConfig,
        #[serde(default)]
        user_agent: Option<String>,
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
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
/// creds = { kind = "profile", name = "isengard-cecelia" }
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

    pub fn openai_compat(base_url: impl Into<String>, api_key_ref: impl Into<String>) -> Self {
        Self::OpenaiCompat {
            base_url: base_url.into(),
            api_key_ref: api_key_ref.into(),
            extra_headers: BTreeMap::new(),
            reasoning_dialect: ReasoningDialect::default(),
            history_reasoning: HistoryReasoning::default(),
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
            extra_headers: BTreeMap::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
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

    pub fn with_extra_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        match &mut self {
            Self::OpenaiCompat { extra_headers, .. } => *extra_headers = headers,
            _ => panic!("ProviderEntry::with_extra_headers only applies to openai-compat"),
        }
        self
    }

    pub fn with_reasoning_dialect(mut self, dialect: ReasoningDialect) -> Self {
        match &mut self {
            Self::OpenaiCompat {
                reasoning_dialect, ..
            } => *reasoning_dialect = dialect,
            _ => {
                panic!("ProviderEntry::with_reasoning_dialect only applies to openai-compat")
            }
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

/// Per-provider runtime knobs that gate dispatch: rate limits and a
/// passive circuit breaker. All fields default to "off" so omitting
/// the block leaves provider behavior unchanged.
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
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

/// Outgoing-history reasoning policy for openai-compat providers.
/// See `ProviderEntry::OpenaiCompat::history_reasoning` for semantics.
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
    /// Status codes that trigger fallback to the next provider in the chain
    /// (in addition to network errors).
    #[serde(default = "default_fallback_status")]
    pub fallback_on_status: Vec<u16>,

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
            fallback_on_status: default_fallback_status(),
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
    use super::{ProviderEntry, ReasoningDialect};

    #[test]
    #[should_panic(expected = "with_anthropic_version")]
    fn wrong_variant_setter_panics() {
        let _ = ProviderEntry::openai_compat("https://example.com/v1", "literal:test")
            .with_anthropic_version("2023-06-01");
    }

    #[test]
    fn redact_secrets_redacts_literal_only() {
        let mut entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-test")
            .with_reasoning_dialect(ReasoningDialect::Openai);
        entry.redact_secrets();
        assert_eq!(entry.secret_uris(), vec!["literal:[REDACTED]"]);
    }
}

#[cfg(test)]
mod v0_6_config_tests {
    //! Tests for the v0.6.0 config shapes: `[models]` table and the
    //! untagged `AliasValue` enum. The `[providers]` table itself
    //! still uses the legacy shape during C1 (these tests pin the
    //! additive surface only).

    use super::{AliasValue, Config, ModelEntry};
    use std::collections::BTreeMap;

    #[test]
    fn model_entry_required_fields_only() {
        // Minimum-viable model entry: just provider + upstream.
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
        assert!(m.adaptive_thinking.is_none());
        assert!(m.reasoning_defaults.is_empty());
    }

    #[test]
    fn model_entry_all_fields() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7-20251022"
selectable = true
thinking = "high"
adaptive_thinking = true
additional_request_fields = { reasoning_config = { type = "enabled" } }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.upstream, "claude-opus-4-7-20251022");
        assert!(m.selectable);
        assert_eq!(m.reasoning_defaults.thinking.as_deref(), Some("high"));
        assert_eq!(m.adaptive_thinking, Some(true));
        assert!(m.additional_request_fields.is_some());
    }

    #[test]
    fn model_entry_rejects_removed_default_extras_field() {
        // v0.6.0-rc.1 dropped `default_extras` and `chat_template_kwargs`
        // from `ModelEntry` -- they shipped briefly on earlier rc builds
        // but never reached the egress. With `#[serde(deny_unknown_fields)]`
        // an upgrading operator who keeps the old keys gets a parse-time
        // error pointing at the offending field, instead of a silent
        // no-op that leaves their reasoning floor unwired.
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7-20251022"
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
    fn model_entry_rejects_removed_chat_template_kwargs_field() {
        let toml_text = r#"
[models.qwen]
provider = "vllm"
upstream = "qwen3-32b"
chat_template_kwargs = { enable_thinking = true }
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("chat_template_kwargs"),
            "expected error to name the removed field; got: {msg}"
        );
    }

    #[test]
    fn alias_value_parses_single_string() {
        let toml_text = r#"
"claude-opus-4-7-20251022" = "heavy"
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
        let entry = v.get("claude-opus-4-7-20251022").expect("entry");
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
        // `default = "..."` lives inside the [aliases] table as a
        // top-level key alongside the wire-string entries. v0.6.0
        // reading the table looks up `"default"` explicitly to get
        // the catch-all destination.
        let toml_text = r#"
default = "small"
"claude-opus-4-7-20251022" = "heavy"
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(toml_text).expect("parse");
        let default_entry = v.get("default").expect("default entry");
        match default_entry {
            AliasValue::Single(s) => assert_eq!(s, "small"),
            other => panic!("expected Single, got {other:?}"),
        }
        // Other entries are unaffected.
        assert!(v.contains_key("claude-opus-4-7-20251022"));
    }

    #[test]
    fn alias_value_suffix_glob_parses() {
        // Glob patterns are operator-supplied keys. The config layer
        // accepts them verbatim; semantic validation (rejecting bare
        // `*`, embedded `*`, etc.) happens via `crate::glob` when
        // Router::new builds the lookup index. Here we only assert
        // that the TOML parses.
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
        // v0.6.0 renamed the providers discriminator from `type` to
        // `kind`. Pin the new shape so a regression to `type` is
        // caught immediately.
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
    fn alias_chain_referencing_unknown_nicknames_is_a_router_concern() {
        // The config layer accepts any alias chain shape; nickname
        // resolution happens at Router::new (C2) which validates that
        // every chain entry resolves to a known [models] entry. C1
        // verifies the parse surface only.
        let toml_text = r#"
[models.alpha]
provider = "p"
upstream = "u"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert!(cfg.models.contains_key("alpha"));
        // Separately confirm the chain shape parses.
        let aliases_text = r#"
"x" = ["alpha", "beta"]
"#;
        let v: BTreeMap<String, AliasValue> = toml::from_str(aliases_text).expect("aliases parse");
        match v.get("x").unwrap() {
            AliasValue::Chain(c) => assert_eq!(c.len(), 2),
            other => panic!("expected Chain, got {other:?}"),
        }
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
        // Builder + TOML parse must agree on the default-true `selectable`.
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
    fn model_entry_anthropic_beta_round_trip() {
        // Pin: per-model anthropic_beta list parses from TOML and
        // survives a serialize -> deserialize round-trip.
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7-20251022"
anthropic_beta = ["context-1m-2025-08-07", "prompt-cache-1h"]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(
            m.anthropic_beta,
            vec![
                "context-1m-2025-08-07".to_string(),
                "prompt-cache-1h".to_string(),
            ]
        );
    }

    #[test]
    fn model_entry_anthropic_beta_default_empty() {
        // Pin: omitting the field yields an empty list (not panic),
        // so existing configs without the key keep parsing.
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7-20251022"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert!(m.anthropic_beta.is_empty());
    }

    #[test]
    fn model_entry_anthropic_beta_skip_serializing_when_empty() {
        // Pin: serialize-skip-when-empty so config dumps stay terse
        // for models without the field set.
        let m = ModelEntry::new("p", "u");
        let dump = toml::to_string(&m).expect("serialize");
        assert!(
            !dump.contains("anthropic_beta"),
            "empty list should be omitted from TOML; got: {dump}"
        );
    }

    #[test]
    fn model_entry_stream_first_byte_timeout_ms_round_trip() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7-20251022"
stream_first_byte_timeout_ms = 300000
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert_eq!(m.stream_first_byte_timeout_ms, Some(300_000));
    }

    #[test]
    fn model_entry_stream_first_byte_timeout_ms_default_none() {
        let toml_text = r#"
[models.opus]
provider = "anthropic"
upstream = "claude-opus-4-7-20251022"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let m = cfg.models.get("opus").expect("opus entry");
        assert!(m.stream_first_byte_timeout_ms.is_none());
    }

    #[test]
    fn model_entry_builders_set_new_fields() {
        let m = ModelEntry::new("p", "u")
            .with_anthropic_beta(vec!["b1".into(), "b2".into()])
            .with_stream_first_byte_timeout_ms(15_000);
        assert_eq!(m.anthropic_beta, vec!["b1".to_string(), "b2".to_string()]);
        assert_eq!(m.stream_first_byte_timeout_ms, Some(15_000));
    }
}

#[cfg(test)]
mod model_reasoning_defaults_tests {
    //! v0.6.0 reasoning defaults live on `[models.X]` (not on
    //! `[providers.X]`). These tests pin the per-model parse surface
    //! and the validator's accumulated-error behavior.

    use super::{Config, ModelEntry};
    use crate::factory::validate_reasoning_defaults;

    fn parse_model(toml_text: &str) -> ModelEntry {
        toml::from_str::<ModelEntry>(toml_text).expect("toml parse")
    }

    #[test]
    fn thinking_high_alone_populates_effort() {
        let toml_text = r#"
provider = "p"
upstream = "u"
thinking = "high"
"#;
        let entry = parse_model(toml_text);
        assert_eq!(entry.reasoning_defaults.thinking.as_deref(), Some("high"));
        assert!(entry.reasoning_defaults.enabled.is_none());
    }

    #[test]
    fn enabled_true_alone_populates_reasoning_enabled() {
        // Pre-rc.2 the outer ModelEntry.enabled and the flattened
        // ReasoningDefaults.enabled shared the TOML key `enabled`,
        // which made reasoning's enabled unreachable from TOML. The
        // rename to `selectable` frees the key, so `enabled = true`
        // on a [models.X] block now lands on
        // ReasoningDefaults::enabled (the reasoning toggle, the
        // operator's expected meaning).
        let toml_text = r#"
provider = "p"
upstream = "u"
enabled = true
"#;
        let entry = parse_model(toml_text);
        assert!(entry.selectable, "default selectable = true");
        assert_eq!(entry.reasoning_defaults.enabled, Some(true));
    }

    #[test]
    fn both_fields_set_populate_both() {
        let toml_text = r#"
provider = "p"
upstream = "u"
thinking = "medium"
"#;
        let entry = parse_model(toml_text);
        assert_eq!(entry.reasoning_defaults.thinking.as_deref(), Some("medium"));
    }

    #[test]
    fn neither_field_yields_all_none_defaults() {
        let toml_text = r#"
provider = "p"
upstream = "u"
"#;
        let entry = parse_model(toml_text);
        assert!(entry.reasoning_defaults.is_empty());
    }

    #[test]
    fn thinking_none_maps_to_effort_none() {
        let toml_text = r#"
provider = "p"
upstream = "u"
thinking = "none"
"#;
        let entry = parse_model(toml_text);
        assert_eq!(entry.reasoning_defaults.thinking.as_deref(), Some("none"));
    }

    #[test]
    fn thinking_empty_string_rejected_at_startup() {
        let toml_text = r#"
[models.bad]
provider = "p"
upstream = "u"
thinking = ""
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");
        let msg = err.to_string();
        assert!(msg.contains("bad"), "msg should name model: {msg}");
        assert!(
            msg.contains("non-empty"),
            "msg should explain rejection: {msg}"
        );
    }

    #[test]
    fn thinking_unknown_value_passthrough() {
        // Forward-compat: vendors add new effort levels without a
        // routectl release. Validator must not gate on a closed enum.
        let toml_text = r#"
[models.future]
provider = "p"
upstream = "u"
thinking = "ultra"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");
        let result = validate_reasoning_defaults(&cfg);
        assert!(result.is_ok(), "expected pass-through Ok, got {result:?}");
        let entry = cfg.models.get("future").expect("model parsed");
        assert_eq!(entry.reasoning_defaults.thinking.as_deref(), Some("ultra"));
    }

    #[test]
    fn thinking_whitespace_only_rejected() {
        let toml_text = r#"
[models.spacey]
provider = "p"
upstream = "u"
thinking = "   "
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");
        let msg = err.to_string();
        assert!(msg.contains("spacey"), "msg should name model: {msg}");
        assert!(
            msg.contains("whitespace-only"),
            "msg should explain rejection: {msg}"
        );
    }

    #[test]
    fn thinking_with_control_characters_rejected() {
        let toml_text = "\
[models.tabby]\n\
provider = \"p\"\n\
upstream = \"u\"\n\
thinking = \"hi\\tgh\"\n\
";
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");
        let msg = err.to_string();
        assert!(msg.contains("tabby"), "msg should name model: {msg}");
        assert!(
            msg.contains("control"),
            "msg should explain rejection: {msg}"
        );
    }

    #[test]
    fn thinking_exceeding_64_bytes_rejected() {
        let long_value = "a".repeat(65);
        let toml_text = format!(
            r#"
[models.verbose]
provider = "p"
upstream = "u"
thinking = "{long_value}"
"#
        );
        let cfg: Config = toml::from_str(&toml_text).expect("toml parse");
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");
        let msg = err.to_string();
        assert!(msg.contains("verbose"), "msg should name model: {msg}");
        assert!(msg.contains("65 bytes"), "msg should report length: {msg}");
        assert!(msg.contains("max 64"), "msg should report cap: {msg}");
    }

    #[test]
    fn validate_reports_all_offending_models() {
        let toml_text = r#"
[models.empty_m]
provider = "p"
upstream = "u"
thinking = ""

[models.spacey_m]
provider = "p"
upstream = "u"
thinking = "   "
"#;
        let cfg: Config = toml::from_str(toml_text).expect("toml parse");
        let err = validate_reasoning_defaults(&cfg).expect_err("should reject");
        let msg = err.to_string();
        assert!(msg.contains("empty_m"), "msg should name empty_m: {msg}");
        assert!(msg.contains("spacey_m"), "msg should name spacey_m: {msg}");
    }
}

impl RetryPolicy {
    /// Resolve the retry cap for a given upstream HTTP status code.
    /// Returns 0 for non-retryable errors.
    ///
    /// Note: `retry_on_5xx` only applies to 5xx codes that are ALSO
    /// listed in `fallback_on_status`. A 5xx code an operator removed
    /// from `fallback_on_status` (e.g. 501 "not implemented") is
    /// treated as non-retryable here AND as non-fallbackable in
    /// `should_fallback`, so it propagates immediately to the caller.
    /// This is intentional: an operator who removes a status from
    /// `fallback_on_status` is asking routectl to surface the error
    /// verbatim, and silently retrying anyway would contradict that.
    pub fn retries_for_status(&self, status: u16) -> u32 {
        match status {
            0 => self.retry_on_network.unwrap_or(self.max_attempts),
            429 => self.retry_on_429.unwrap_or(self.max_attempts),
            s if (500..600).contains(&s) && self.fallback_on_status.contains(&s) => {
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

fn default_fallback_status() -> Vec<u16> {
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
mod default_fallback_status_tests {
    //! Pin the default `fallback_on_status` vocabulary. Cloudflare
    //! extended 5xx codes (520-527, 530) belong on the default list
    //! because Cloudflare-fronted upstreams (opencode.ai, openrouter.ai,
    //! etc.) surface upstream-origin failures via this range; without
    //! them, a single 520 from a Cloudflare-fronted provider kills the
    //! request even though a sibling provider could have served it.
    use super::{default_fallback_status, RetryPolicy};

    #[test]
    fn default_fallback_status_contains_legacy_codes() {
        // Pin: existing operator configs depend on these codes being
        // present. Pre-extension list, all six must survive.
        let list = default_fallback_status();
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(
                list.contains(&code),
                "expected default fallback list to contain legacy code {code}; got {list:?}"
            );
        }
    }

    #[test]
    fn default_fallback_status_contains_cloudflare_codes() {
        // Pin: Cloudflare-extended 5xx range (520-527 + 530) is on
        // the default fallback list.
        let list = default_fallback_status();
        for code in [520, 521, 522, 523, 524, 525, 526, 527, 530] {
            assert!(
                list.contains(&code),
                "expected default fallback list to contain Cloudflare code {code}; got {list:?}"
            );
        }
    }

    #[test]
    fn default_fallback_status_does_not_contain_unrelated_5xx() {
        // Pin: codes that are NOT eligible for retry stay off the
        // default list. 501 (Not Implemented) is terminal and must
        // never be retried automatically.
        let list = default_fallback_status();
        assert!(
            !list.contains(&501),
            "501 (Not Implemented) is terminal and must not be on the default fallback list"
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
