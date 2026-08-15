//! Config schema root + re-exports.
//!
//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

mod schema;
mod validate;

#[cfg(feature = "bedrock")]
pub(crate) use schema::default_anthropic_base;
#[cfg(feature = "gemini")]
pub(crate) use schema::default_gemini_base;
pub(crate) use schema::routectl_config_dir;
pub use schema::{
    AliasValue, BedrockGlobalConfig, CacheCapability, CacheConfig, CapabilityConfig,
    CredentialSource, HistoryReasoning, LogConfig, MitmConfig, ModelEntry, NicknameIter,
    OverrideEntry, PricingConfig, ProviderEntry, ProviderRuntimePolicy, ReasoningDialect,
    ReductionConfig, RegistryEntry, RetryPolicy, SeatSelection, ServerAuth, ServerConfig,
    TrimConfig, UsageConfig, WindowGateConfig,
};
#[cfg(feature = "bedrock")]
pub use schema::{BedrockApiShapeConfig, BedrockCredsConfig, BedrockMantleConfig};
use validate::default_config_version;
pub use validate::{
    CURRENT_CONFIG_VERSION, ConfigVersionError, LegacyMitmCredentialSourceError,
    VersionTooNewError, preflight_config_version, preflight_legacy_mitm_credential_source,
    validate_cache_pricing_retired,
};

/// The parsed `config.toml`: server bind, providers, aliases, models, and
/// the cross-cutting policy tables the router reads at startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema-version stamp for this `config.toml`. Absent in the file
    /// (every pre-v0.9 config) deserializes to `1`. [`CURRENT_CONFIG_VERSION`]
    /// is `3`. A `version` that does not equal [`CURRENT_CONFIG_VERSION`]
    /// is rejected before this struct is deserialized -- a too-old file is
    /// pointed at `config migrate` (which chains the v1->v2 and v2->v3
    /// transforms), a too-new file at a binary upgrade. See
    /// [`preflight_config_version`]. The loader never migrates on load.
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

    /// Operator-facing `[usage]` block. Controls the usage-accounting
    /// subsystem (per-request token/cost rows persisted to a local
    /// SQLite db). A missing block keeps all defaults: enabled, a db
    /// under the user config dir, 90-day retention.
    #[serde(default)]
    pub usage: UsageConfig,

    /// Operator-facing `[registry]` pricing table. Each key is an
    /// upstream-id glob (an exact id or a trailing-`*` prefix, parsed
    /// via `AliasPattern`); each value carries optional per-million-token
    /// pricing and an optional `provider` scope. routectl ships NO price
    /// defaults -- an unlisted upstream is simply unpriced. Cost is
    /// derived at QUERY time from this table, so correcting a price later
    /// retroactively fixes the cost of historical rows. The table is
    /// named `[registry.*]` deliberately (room to grow capability
    /// metadata later); only pricing is wired today.
    #[serde(default)]
    pub registry: BTreeMap<String, RegistryEntry>,

    /// Operator-facing `[cache]` block. Controls the dispatch-path
    /// auto-emission of a top-level Anthropic `cache_control` breakpoint.
    /// A missing block keeps the default: auto-emit enabled.
    #[serde(default)]
    pub cache: CacheConfig,

    /// Operator-facing `[reduction]` block. Global switch for the
    /// dispatch-path token-reduction feature. A missing block keeps the
    /// default: reduction disabled.
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

    /// Operator-facing `[trim]` block. Tunes the deterministic steady-state
    /// advisory trimmer's four knobs. A missing block resolves (via
    /// `TrimConfig::to_params()`) to `SteadyStateTrimParams::default()` --
    /// the trimmer's current conservative behavior is unchanged.
    #[serde(default)]
    pub trim: TrimConfig,

    /// Operator-facing `[window_gate]` block. Kill switch for the
    /// proactive context-window gate. A missing block leaves the gate
    /// enabled; setting `enabled = false` restores the pre-gate routing
    /// behavior exactly. Hot-reloadable -- the flag is read per chain
    /// resolution, so a live config swap applies without a restart.
    #[serde(default)]
    pub window_gate: WindowGateConfig,

    /// Operator-facing LEGACY `[cache_pricing]` field-level override table
    /// for the baked catalog economics rows (`crate::catalog`). Slated for
    /// retirement once its data migrates into the catalog overlay
    /// (`crate::catalog_overlay`). Each
    /// key is a `"provider_kind:model_glob"` selector (e.g.
    /// `"openai-compat:grok-*"`); each value is a sparse
    /// [`crate::catalog::CachePricingOverride`] -- only the fields the
    /// operator wants to correct, the rest inheriting the baked-in value.
    /// An empty / omitted block is the norm; this exists to patch a cell
    /// that drifted before a routectl release can re-bake it. An override
    /// that sets `wm` below the conservative sentinel must carry
    /// `override_acknowledges_cost_risk = true` or the merge is rejected by
    /// the consuming cost gate. The selector keys are NOT validated against
    /// the baked table here (an unmatched selector is simply inert).
    #[serde(default)]
    pub cache_pricing: BTreeMap<String, crate::catalog::CachePricingOverride>,

    /// Optional `[mitm]` block for the MITM front-proxy (TLS-terminating
    /// local listener that fronts a first-party upstream, e.g. Claude
    /// Code talking to `api.anthropic.com`). Presence gates the feature
    /// on, matching the `[server.auth]` convention -- `None` (the block
    /// omitted) keeps today's behavior with zero MITM proxy startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitm: Option<MitmConfig>,
}

impl Config {
    /// Resolve the best pricing for an upstream id served by a given
    /// provider. Walks the `[registry]` table, keeps only eligible
    /// entries (pattern matches `upstream`, the entry HAS pricing, and
    /// the entry's provider scope is either this provider or agnostic),
    /// and returns the best match.
    ///
    /// Ordering: a provider-scoped match beats a provider-agnostic one;
    /// among equal scope, the longest matching prefix wins (an exact key
    /// has a prefix length equal to the full id, so it naturally beats a
    /// shorter prefix). When scope AND prefix length tie -- an Exact key
    /// and a same-length Prefix key both matching the same upstream -- the
    /// Exact key wins (it is the more specific intent). An entry scoped to
    /// some OTHER provider is never eligible. Keys that fail to parse are
    /// skipped here -- startup validation (`validate_registry_patterns`)
    /// rejects them.
    pub fn pricing_for(&self, upstream: &str, provider: &str) -> Option<&PricingConfig> {
        self.registry
            .iter()
            .filter_map(|(key, entry)| {
                let pricing = entry.pricing.as_ref()?;
                let pattern = crate::glob::AliasPattern::parse(key).ok()?;
                if !pattern.matches(upstream) {
                    return None;
                }
                let scoped = match entry.provider.as_deref() {
                    Some(p) if p == provider => true,
                    Some(_) => return None,
                    None => false,
                };
                let is_exact = matches!(pattern, crate::glob::AliasPattern::Exact(_));
                Some((scoped, pattern.prefix_len(), is_exact, pricing))
            })
            .max_by_key(|(scoped, prefix_len, is_exact, _)| (*scoped, *prefix_len, *is_exact))
            .map(|(_, _, _, pricing)| pricing)
    }
}

/// A full config mirroring `examples/config.toml`'s non-feature-gated
/// structure, using ONLY the always-available provider kinds
/// (`openai-compat`, `anthropic-api`). The shipped example documents
/// feature-gated kinds (`bedrock`, `openai-responses`), so it deserializes
/// as a `Config` only when those features are compiled in. This fixture
/// lets the shipped-example parse assertions run unchanged on a lean build.
#[cfg(test)]
pub(crate) const LEAN_EXAMPLE_CONFIG: &str = r#"
version = 3

[server]
host = "127.0.0.1"
port = 8787

[providers.openrouter]
kind        = "openai-compat"
base_url    = "https://openrouter.ai/api/v1"
api_key_ref = "env://OPENROUTER_API_KEY"

[providers.anthropic]
kind        = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"

[models.opus-or]
provider      = "openrouter"
upstream      = "anthropic/claude-opus-4-7-20260301"
effort_levels = []

[models.opus-direct]
provider                   = "anthropic"
upstream                   = "claude-opus-4-7-20260301"
supports_adaptive_thinking = true
effort_levels              = ["minimal", "low", "medium", "high", "xhigh", "max"]

[aliases]
"claude-opus-*" = "opus-direct"
default         = "opus-or"

[retry]
max_attempts       = 2
initial_backoff_ms = 250

[capability]
enabled               = true
decay_hours           = 48
inferred_window_hours = 1
staleness_hint_days   = 14
"#;
