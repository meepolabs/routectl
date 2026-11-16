//! Config schema root + re-exports.
//!
//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

mod pool;
pub(crate) mod provider_descriptor;
mod retired_keys;
mod schema;
pub mod tuning;
mod validate;

pub use pool::PoolEntry;
pub use provider_descriptor::{Auth, Format};
pub use retired_keys::{RetiredKeysError, retired_key_message};
pub(crate) use schema::DEFAULT_ANTHROPIC_VERSION;
pub use schema::{
    AliasValue, CANARY_INTERVAL, CONFIG_PROVIDER_KINDS, CacheCapability, CacheConfig,
    CapabilityConfig, Cloud, ENVELOPE_QUORUM, HistoryReasoning, MitmConfig, ModelEntry,
    NicknameIter, OverrideEntry, PREFIX_QUORUM, PricingConfig, ProviderEntry,
    ProviderRuntimePolicy, ReasoningDialect, ReductionConfig, RetryPolicy, SeatSelection,
    ServerAuth, ServerConfig, UsageConfig, WindowGateConfig, is_config_provider_kind,
};
pub use tuning::{BreakerThreshold, Resolved, ResolvedTuning, TuningSource, resolve_tuning};
use validate::default_config_version;
pub use validate::{
    CONFIG_VERSION_REMEDIATION, CURRENT_CONFIG_VERSION, ConfigVersionError,
    RawConfigPreflightError, VersionTooNewError, is_bare_key, preflight_config_version,
    preflight_raw_config,
};
pub(crate) use validate::{
    validate_model_pricing, validate_model_token_limits, validate_server_body_limit,
};

/// The parsed `config.toml`: server bind, providers, aliases, models, and
/// the cross-cutting policy tables the router reads at startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema-version stamp for this `config.toml`. [`CURRENT_CONFIG_VERSION`]
    /// is `5`. A file whose `version` is absent or does not equal
    /// [`CURRENT_CONFIG_VERSION`] is rejected before this struct is
    /// deserialized -- a too-old or versionless file is told to move the old
    /// file aside and run `routectl init`, a too-new file to upgrade the
    /// binary. See [`preflight_config_version`]. The loader never rewrites
    /// the file on load. An absent key still deserializes to `1` when this
    /// struct is parsed directly, without the preflight.
    ///
    /// NOTE: `Config::default()` (plain Rust construction, e.g. in tests)
    /// yields `0`, not `1` -- the `1` default applies only on the serde
    /// deserialize-from-TOML path (an absent key in the file).
    /// `#[derive(Default)]` cannot special-case one field's constant, and
    /// every caller that gates on `version < CURRENT_CONFIG_VERSION`
    /// treats `0` and `1` identically, so the two never diverge in
    /// practice.
    #[serde(default = "default_config_version")]
    pub version: u32,

    /// Server bind config.
    #[serde(default)]
    pub server: ServerConfig,

    /// Provider definitions keyed by operator-facing name. Carries
    /// transport-side knobs only (format, destination, credential, auth,
    /// headers, runtime gates). Per-model knobs (`reasoning_dialect`, `history_reasoning`)
    /// live on `[models.X]` in v0.6.0.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,

    /// Pool definitions keyed by operator-facing name: each groups
    /// same-kind `[providers.X]` entries (accounts) under one
    /// seat-selection strategy. A `[models.X] provider` value may name a
    /// provider entry OR a pool -- one namespace, so a name matching both
    /// is a validation error. A provider entry belongs to at most one
    /// pool.
    #[serde(default)]
    pub pools: BTreeMap<String, PoolEntry>,

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

    /// v0.6.0 model directory. Each entry binds a logical nickname
    /// (the table key) to a transport (`provider`), an upstream model
    /// id (`upstream`), and per-model knobs. The `[aliases]` table
    /// references entries by nickname. See the v0.6.0-rc.1 changelog.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,

    /// Operator-facing `[usage]` block. Controls the usage-accounting
    /// subsystem (per-request token/cost rows persisted to a local
    /// SQLite db). A missing block keeps all defaults: a db under the
    /// user config dir, 90-day retention.
    #[serde(default)]
    pub usage: UsageConfig,

    /// Operator-facing `[cache]` block. Controls the dispatch-path
    /// auto-emission of a top-level Anthropic `cache_control` breakpoint.
    /// A missing block keeps the default: auto-emit enabled.
    #[serde(default)]
    pub cache: CacheConfig,

    /// Operator-facing `[reduction]` block. Global switch for the
    /// dispatch-path token-reduction feature. A missing block keeps the
    /// default: reduction enabled.
    #[serde(default)]
    pub reduction: ReductionConfig,

    /// Operator-facing `[capability]` block. Kill switch plus tempo knobs
    /// for the learned-capability subsystem. A missing block keeps all
    /// defaults: enabled, a 48h decay window, a 1h inferred-signal window.
    /// The table is deliberately top-level (not nested under `[server]`)
    /// so a later override layer can nest per-target overrides under the
    /// same parent.
    #[serde(default)]
    pub capability: CapabilityConfig,

    /// Operator-facing `[window_gate]` block. Kill switch for the
    /// proactive context-window gate. A missing block leaves the gate
    /// disabled, which is byte-identical to no gate at all; `enabled = true`
    /// opts in. Hot-reloadable -- the flag is read per chain
    /// resolution, so a live config swap applies without a restart.
    #[serde(default)]
    pub window_gate: WindowGateConfig,

    /// Optional `[mitm]` block for the MITM front-proxy (TLS-terminating
    /// local listener that fronts a first-party upstream, e.g. Claude
    /// Code talking to `api.anthropic.com`). Presence gates the feature
    /// on, matching the `[server.auth]` convention -- `None` (the block
    /// omitted) keeps today's behavior with zero MITM proxy startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitm: Option<MitmConfig>,
}

/// A full config mirroring `examples/config.toml`'s non-feature-gated
/// structure, using ONLY the always-available formats (`openai-chat`,
/// `anthropic-messages`). The shipped example also documents feature-gated
/// formats, which validate only when those features are compiled in. This
/// fixture lets the shipped-example assertions run unchanged on a lean
/// build.
#[cfg(test)]
pub(crate) const LEAN_EXAMPLE_CONFIG: &str = r#"
version = 5

[server]
host = "127.0.0.1"
port = 8787

[providers.openrouter]
format     = "openai-chat"
base_url   = "https://openrouter.ai/api/v1"
credential = "env://OPENROUTER_API_KEY"
auth       = "bearer"

[providers.anthropic]
format     = "anthropic-messages"
base_url   = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth       = "x-api-key"

[models.opus-or]
provider = "openrouter"
upstream = "anthropic/claude-opus-4-7-20260301"

[models.opus-direct]
provider = "anthropic"
upstream = "claude-opus-4-7-20260301"

[aliases]
"claude-opus-*" = "opus-direct"
default         = "opus-or"

[retry]
max_attempts       = 3
initial_backoff_ms = 250

[capability]
enabled               = true
decay_hours           = 48
inferred_window_hours = 1
"#;
