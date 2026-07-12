//! TOML config schema for routectl. Loaded from `~/.config/routectl/config.toml`
//! by default, overridable via `--config <path>` or `ROUTECTL_CONFIG`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use routectl_providers::anthropic_api::{AuthKind, CloakConfig};
#[cfg(feature = "gemini")]
use routectl_providers::gemini::GeminiAuthMode;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::class_policy::{ClassPolicy, ConfigFailureClass};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema-version stamp for this `config.toml`. Absent in the file
    /// (every pre-v0.9 config) deserializes to `1`. [`CURRENT_CONFIG_VERSION`]
    /// is `2`: a `version < 2` config's legacy `[cache_pricing]` table is
    /// retired into the catalog overlay by `crate::config_migrate` the
    /// first time it loads under a v2-aware binary, and this field is
    /// rewritten to `2` in place. A `version` greater than
    /// [`CURRENT_CONFIG_VERSION`] is rejected before this struct is even
    /// deserialized -- see [`preflight_config_version`].
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

    /// Operator-facing `[trim]` block. Tunes the deterministic steady-state
    /// advisory trimmer's four knobs. A missing block resolves (via
    /// `TrimConfig::to_params()`) to `SteadyStateTrimParams::default()` --
    /// the trimmer's current conservative behavior is unchanged.
    #[serde(default)]
    pub trim: TrimConfig,

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

/// Current config schema version this build writes and fully understands.
/// `1` -> `2` retires the legacy `[cache_pricing]` override table into the
/// catalog overlay (`crate::config_migrate`).
pub const CURRENT_CONFIG_VERSION: u32 = 2;

const fn default_config_version() -> u32 {
    1
}

/// Error from [`preflight_config_version`]: the config file names a schema
/// version newer than this build understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "config version {found} is newer than the {supported} this build supports; upgrade \
     routectl or downgrade the config's `version` key"
)]
pub struct VersionTooNewError {
    pub found: u32,
    pub supported: u32,
}

/// Read the `version` key straight off the RAW TOML text, before `Config`'s
/// full (`#[serde(deny_unknown_fields)]`) deserialize runs. A config written
/// by a newer routectl may carry fields this build does not know about;
/// deserializing it directly would fail with a confusing "unknown field"
/// error that buries the real cause. This preflight catches that case
/// explicitly: a `version` greater than [`CURRENT_CONFIG_VERSION`] fails
/// closed here with a clear message. Callers wire this in at both cold
/// startup (propagate the error, fail hard) and hot config reload (reject
/// the reload, keep the prior router live).
///
/// A missing `version` key, TOML that fails to parse at all, or a
/// `version` that is not a plain non-negative integer are all left for the
/// normal typed deserialize to report -- this function only ever returns
/// an error for the one case it exists to catch, so it never masks a
/// genuine syntax error behind a version message.
pub fn preflight_config_version(raw_toml: &str) -> Result<u32, VersionTooNewError> {
    let found = toml::from_str::<toml::Value>(raw_toml)
        .ok()
        .and_then(|v| v.get("version").and_then(toml::Value::as_integer))
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or_else(default_config_version);

    if found > CURRENT_CONFIG_VERSION {
        return Err(VersionTooNewError {
            found,
            supported: CURRENT_CONFIG_VERSION,
        });
    }
    Ok(found)
}

/// At `version >= CURRENT_CONFIG_VERSION`, a non-empty legacy
/// `[cache_pricing]` table is a startup-time misconfiguration, not
/// silently-ignored data: the v1->v2 migration
/// (`crate::config_migrate::migrate_v1_to_v2`) already folds
/// `[cache_pricing]` into the catalog overlay and clears it from
/// `config.toml`, so a non-empty table at v2+ means the file was
/// hand-edited back into an inconsistent state (or authored fresh from a
/// stale example). Names the migrator so the operator knows the fix.
pub fn validate_cache_pricing_retired(config: &Config) -> Result<(), String> {
    if config.version >= CURRENT_CONFIG_VERSION && !config.cache_pricing.is_empty() {
        return Err(format!(
            "config version {} carries a non-empty [cache_pricing] table ({} entries), but \
             [cache_pricing] is retired as of version {CURRENT_CONFIG_VERSION} -- it should \
             have been migrated into the catalog overlay by the v1->v2 migrator \
             (crate::config_migrate::migrate_v1_to_v2); set `version` back below \
             {CURRENT_CONFIG_VERSION} to re-run the migration, or remove [cache_pricing] by hand",
            config.version,
            config.cache_pricing.len(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod config_version_tests {
    use super::{
        CURRENT_CONFIG_VERSION, Config, preflight_config_version, validate_cache_pricing_retired,
    };

    #[test]
    fn absent_version_key_deserializes_to_one() {
        // Arrange / Act
        let config: Config = toml::from_str("[server]\nhost = \"127.0.0.1\"\n").expect("parse");

        // Assert
        assert_eq!(config.version, 1);
    }

    #[test]
    fn explicit_current_version_round_trips() {
        // Arrange / Act
        let config: Config =
            toml::from_str("version = 2\n[server]\nhost = \"127.0.0.1\"\n").expect("parse");

        // Assert
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
    }

    #[test]
    fn preflight_accepts_absent_version() {
        assert_eq!(preflight_config_version("[server]\nhost = \"x\"\n"), Ok(1));
    }

    #[test]
    fn preflight_accepts_current_version() {
        assert_eq!(
            preflight_config_version("version = 2\n[server]\nhost = \"x\"\n"),
            Ok(2)
        );
    }

    #[test]
    fn preflight_rejects_version_newer_than_current() {
        // Act
        let err = preflight_config_version("version = 3\n[server]\nhost = \"x\"\n")
            .expect_err("version 3 must be rejected");

        // Assert
        assert_eq!(err.found, 3);
        assert_eq!(err.supported, CURRENT_CONFIG_VERSION);
    }

    /// Preflight must catch a too-new version BEFORE the full deserialize
    /// runs, so a newer routectl's unknown fields never reach
    /// `deny_unknown_fields` and mask the version error behind a
    /// confusing "unknown field" message.
    #[test]
    fn preflight_rejects_newer_version_even_with_fields_this_build_does_not_know() {
        let raw = "version = 99\nsome_field_from_the_future = true\n[server]\nhost = \"x\"\n";

        let err = preflight_config_version(raw).expect_err("version 99 must be rejected");
        assert_eq!(err.found, 99);

        // The typed deserialize is never reached for this input in the
        // real load path; confirm it WOULD have failed with the confusing
        // unknown-field error preflight exists to avoid.
        let deny_unknown_err = toml::from_str::<Config>(raw).expect_err("must fail to parse");
        assert!(
            !deny_unknown_err.to_string().contains("newer"),
            "sanity: the raw deserialize error must NOT already read like a version message"
        );
    }

    #[test]
    fn validate_cache_pricing_retired_allows_nonempty_at_v1() {
        let mut config = Config {
            version: 1,
            ..Config::default()
        };
        config.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            crate::catalog::CachePricingOverride::default(),
        );

        assert!(validate_cache_pricing_retired(&config).is_ok());
    }

    #[test]
    fn validate_cache_pricing_retired_allows_empty_at_current_version() {
        let config = Config {
            version: CURRENT_CONFIG_VERSION,
            ..Config::default()
        };

        assert!(validate_cache_pricing_retired(&config).is_ok());
    }

    #[test]
    fn validate_cache_pricing_retired_rejects_nonempty_at_current_version() {
        let mut config = Config {
            version: CURRENT_CONFIG_VERSION,
            ..Config::default()
        };
        config.cache_pricing.insert(
            "openai-compat:grok-*".to_string(),
            crate::catalog::CachePricingOverride::default(),
        );

        let err = validate_cache_pricing_retired(&config).expect_err("must reject");
        assert!(err.contains("config_migrate"), "err: {err}");
        assert!(err.contains('1'), "err should name the entry count: {err}");
    }
}

/// Operator-facing `[cache]` config block. Global policy for the
/// dispatch-path auto-cache feature. A missing `[cache]` table
/// deserializes to `CacheConfig::default()` (auto-emit enabled), and the
/// per-field `#[serde(default)]` keeps an omitted key enabled too.
///
/// This is the GLOBAL kill-switch; each `[providers.X]` entry carries an
/// optional `auto_emit_top_level_breakpoint` override consulted only when
/// the global switch is on. The effective decision is "global on AND
/// provider not explicitly off".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CacheConfig {
    /// Master switch for dispatch-path auto-emission of a top-level
    /// `cache_control` ephemeral_5m breakpoint. Default on.
    #[serde(default = "default_true")]
    pub auto_emit_top_level_breakpoint: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            auto_emit_top_level_breakpoint: true,
        }
    }
}

/// Operator-facing `[reduction]` config block. Global policy for the
/// dispatch-path token-reduction feature. A missing `[reduction]` table
/// deserializes to `ReductionConfig::default()` (reduction disabled), and
/// the per-field `#[serde(default)]` keeps an omitted key disabled too.
///
/// This is the GLOBAL switch; each `[providers.X]` entry carries an
/// optional `reduction_enabled` override consulted only when the global
/// switch is on. The effective decision a later dispatch task will consume:
/// reduction applies when the global `enabled == true` AND the provider
/// override is not explicitly `Some(false)`. A provider `None` inherits
/// the global setting.
///
/// `#[non_exhaustive]` so later tactic sub-configs are non-breaking
/// additions to the Rust API. `#[serde(deny_unknown_fields)]` is the
/// complementary wire-side choice (matching `CacheConfig`): unknown TOML
/// keys are rejected so a typo surfaces at config-load time rather than
/// being silently ignored. The two do not conflict -- a config naming a
/// future field simply requires a binary new enough to know that field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReductionConfig {
    /// Master switch for the dispatch-path token-reduction feature.
    /// Default off (reduction is opt-in): `bool::default()` is `false`,
    /// and the derived `Default` keeps reduction disabled.
    #[serde(default)]
    pub enabled: bool,
}

/// Operator-facing `[trim]` config block. Wraps the deterministic
/// steady-state advisory trimmer's four knobs
/// (`crate::context_trim::SteadyStateTrimParams`) for TOML config. A missing
/// `[trim]` block deserializes to `TrimConfig::default()`; `to_params()` then
/// resolves that to the SAME `SteadyStateTrimParams` as
/// `SteadyStateTrimParams::default()`, because every per-field default fn
/// below delegates to the exact const `SteadyStateTrimParams::default()`
/// uses -- one source of truth for "missing block == current conservative
/// defaults", never a second hardcoded copy.
///
/// `to_params()` is the ONLY constructor for `SteadyStateTrimParams` from
/// config, and BOTH production consumers (`Router::record_would_trim`,
/// `prompt_size::build_steady_state_economics`) call it -- they can never
/// resolve to different params from the same `Config`.
///
/// `SteadyStateTrimParams` itself is deliberately NOT serde-derived: it is
/// the advisory trimmer's pure-function input (built ad hoc by tests too),
/// and coupling it to serde would leak a config-loading concern into a
/// module that has none today. This wrapper is the wire-side boundary.
///
/// No `enabled` switch, no band knobs, no per-provider override: the
/// steady-state trimmer is an always-on ADVISORY recorder that never mutates
/// a dispatched request, so there is no "off" state to represent.
/// `#[non_exhaustive]` leaves room for a later knob without breaking
/// callers; `#[serde(deny_unknown_fields)]` rejects a typo'd key at
/// config-load time instead of silently ignoring it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TrimConfig {
    /// Estimated total tokens at or below which no trim is proposed.
    #[serde(default = "default_trim_trigger_tokens")]
    pub trigger_tokens: u64,
    /// Minimum tokens the elided span must free for a trim to be proposed.
    #[serde(default = "default_trim_clear_at_least_tokens")]
    pub clear_at_least_tokens: u64,
    /// Number of leading messages kept fully intact (never elided).
    #[serde(default = "default_trim_head_keep_messages")]
    pub head_keep_messages: usize,
    /// Number of trailing messages protected from elision.
    #[serde(default = "default_trim_keep_recent_messages")]
    pub keep_recent_messages: usize,
}

impl Default for TrimConfig {
    fn default() -> Self {
        Self {
            trigger_tokens: default_trim_trigger_tokens(),
            clear_at_least_tokens: default_trim_clear_at_least_tokens(),
            head_keep_messages: default_trim_head_keep_messages(),
            keep_recent_messages: default_trim_keep_recent_messages(),
        }
    }
}

impl TrimConfig {
    /// Resolve this config block to the advisory trimmer's pure-function
    /// params. The single shared constructor -- see the struct doc for why
    /// both production consumers must call this instead of building
    /// `SteadyStateTrimParams` themselves.
    pub const fn to_params(&self) -> crate::context_trim::SteadyStateTrimParams {
        crate::context_trim::SteadyStateTrimParams {
            trigger_tokens: self.trigger_tokens,
            clear_at_least_tokens: self.clear_at_least_tokens,
            head_keep_messages: self.head_keep_messages,
            keep_recent_messages: self.keep_recent_messages,
        }
    }
}

const fn default_trim_trigger_tokens() -> u64 {
    crate::context_trim::DEFAULT_TRIGGER_TOKENS
}

const fn default_trim_clear_at_least_tokens() -> u64 {
    crate::context_trim::DEFAULT_CLEAR_AT_LEAST_TOKENS
}

const fn default_trim_head_keep_messages() -> usize {
    crate::context_trim::DEFAULT_HEAD_KEEP_MESSAGES
}

const fn default_trim_keep_recent_messages() -> usize {
    crate::context_trim::DEFAULT_KEEP_RECENT_MESSAGES
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

/// Operator-facing `[usage]` config block. Controls the
/// usage-accounting subsystem. Each field carries a default, so a
/// config with no `[usage]` block deserializes to the same value as
/// `UsageConfig::default()`.
///
/// Reload semantics: `db_path` is restart-required (the writer opens
/// the db at boot and holds the handle); `enabled` and
/// `retention_days` hot-reload on the next config swap. See
/// `collect_restart_required_changes` in the CLI server module for the
/// classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    /// Master switch for the usage-accounting subsystem. Default on.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// SQLite database path for persisted usage rows. Defaults to
    /// `<config-dir>/routectl/usage.db` with the user config dir
    /// resolved from `XDG_CONFIG_HOME` or `HOME` -- no literal `~`
    /// ever reaches SQLite.
    #[serde(default = "default_usage_db_path")]
    pub db_path: PathBuf,
    /// Rows older than this many days are pruned on daemon startup.
    /// Default 90.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: default_usage_db_path(),
            retention_days: default_retention_days(),
        }
    }
}

const fn default_retention_days() -> u32 {
    90
}

/// Resolve routectl's own config dir the same way the codebase
/// resolves `credentials.json` / `config.toml`: `XDG_CONFIG_HOME` when
/// set, else `$HOME/.config`, else a relative `.config` fallback,
/// joined with `routectl`. Returns an absolute, `~`-free path so
/// callers can hand a child path straight to SQLite or a file writer.
pub(crate) fn routectl_config_dir() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home).join(".config"),
            _ => PathBuf::from(".config"),
        },
    };
    base.join("routectl")
}

/// Resolve the default usage-db path under `routectl_config_dir()`.
fn default_usage_db_path() -> PathBuf {
    routectl_config_dir().join("usage.db")
}

/// Which credential a provider egress authenticates with. `own` (the
/// default) preserves prior behavior byte-for-byte: the egress
/// authenticates with routectl's own managed credential. `forwarded`
/// instead relays the client's inbound credential straight through to
/// the upstream untouched. Used ONLY by `ProviderEntry::AnthropicApi`
/// (the forwarded-provider credential, pinned to `api.anthropic.com` by
/// `validate_provider_credential_sources`) -- the legacy `[mitm]
/// credential_source` field this enum also backed is removed; see
/// `preflight_legacy_mitm_credential_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSource {
    /// Authenticate to the upstream with routectl's own managed
    /// credential (f1 behavior).
    #[default]
    Own,
    /// Relay the client's inbound credential to the upstream untouched.
    Forwarded,
}

/// Operator-facing `[mitm]` config block for the MITM front-proxy: a
/// local TLS-terminating listener that fronts a first-party upstream
/// (e.g. Claude Code talking to `api.anthropic.com`) so routectl can
/// observe and translate the decrypted traffic. Presence of the
/// `[mitm]` block gates the feature on, matching the `[server.auth]`
/// convention -- `Config::mitm == None` keeps zero proxy startup and
/// zero behavior change.
///
/// Transport-only (the f1 shape): this block carries no credential
/// knob. Which credential a forwarded egress uses is a per-provider
/// choice (`ProviderEntry::AnthropicApi.credential_source`) -- see
/// `preflight_legacy_mitm_credential_source` for the pre-parse check
/// that catches a config still carrying the removed `[mitm]
/// credential_source` key and names the replacement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MitmConfig {
    /// Upstream origin the proxy forwards decrypted requests to. Must
    /// be EXACTLY `https://api.anthropic.com` -- no userinfo, path,
    /// query, fragment, explicit non-default port, or other host
    /// (enforced by `validate_mitm_config`). Pinned rather than
    /// pattern-matched: the MITM proxy forwards the client's full-scope
    /// claude.ai token, which must never reach a non-Anthropic egress.
    #[serde(default = "default_mitm_upstream_origin")]
    pub upstream_origin: String,
    /// Local TCP port the MITM listener binds. Must differ from
    /// `[server] port` -- the listener and the routectl HTTP server
    /// are separate bound sockets on the same host (enforced by
    /// `validate_mitm_config`).
    #[serde(default = "default_mitm_listen_port")]
    pub listen_port: u16,
    /// Directory holding the locally-generated MITM CA + leaf
    /// certificates. Defaults under `routectl_config_dir()`, alongside
    /// the usage db and `config.toml`.
    #[serde(default = "default_mitm_cert_dir")]
    pub cert_dir: PathBuf,
    /// TLS SNI / `Host` header the proxy expects from the client and
    /// presents to the upstream when dialing out. Must be EXACTLY
    /// `api.anthropic.com` -- a subdomain like `evil.api.anthropic.com`
    /// is rejected, not matched as a suffix (enforced by
    /// `validate_mitm_config`).
    #[serde(default = "default_mitm_host")]
    pub mitm_host: String,
    /// Free-form Claude Code version this `[mitm]` config was last
    /// verified against. Consulted at runtime by the MITM proxy: when
    /// the version actually observed on a decrypted request's
    /// `User-Agent` differs from this, it logs a WARNING (once per
    /// distinct observed version) but never refuses the request --
    /// `None` (the default) disables the check entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tested_cc_version: Option<String>,
}

impl Default for MitmConfig {
    fn default() -> Self {
        Self {
            upstream_origin: default_mitm_upstream_origin(),
            listen_port: default_mitm_listen_port(),
            cert_dir: default_mitm_cert_dir(),
            mitm_host: default_mitm_host(),
            tested_cc_version: None,
        }
    }
}

fn default_mitm_upstream_origin() -> String {
    "https://api.anthropic.com".into()
}

const fn default_mitm_listen_port() -> u16 {
    8443
}

fn default_mitm_cert_dir() -> PathBuf {
    routectl_config_dir().join("mitm-certs")
}

fn default_mitm_host() -> String {
    "api.anthropic.com".into()
}

/// The exact `[providers.X]` replacement block named in
/// [`LegacyMitmCredentialSourceError`] and the CHANGELOG: a forwarded
/// credential is now a provider-level choice, not a `[mitm]`-level one.
const LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK: &str = "[providers.anthropic-forwarded]\n\
     kind = \"anthropic-api\"\n\
     base_url = \"https://api.anthropic.com\"\n\
     credential_source = \"forwarded\"";

/// Error from [`preflight_legacy_mitm_credential_source`]: the config
/// still carries the removed `[mitm] credential_source` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "config carries the removed `[mitm] credential_source` key -- a forwarded credential is \
     now a per-provider choice, not a `[mitm]`-level one. Replace it with a provider block:\n\n\
     {LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK}\n\n\
     (no `api_key_ref` line -- a forwarded provider has no configured credential of its own)"
)]
pub struct LegacyMitmCredentialSourceError;

/// Read the `[mitm]` table off the RAW TOML text, before `Config`'s full
/// (`#[serde(deny_unknown_fields)]`) deserialize runs, and check for the
/// removed `credential_source` key. Without this preflight, an old
/// config carrying that key still fails at load (`deny_unknown_fields`
/// rejects it), but with serde's generic "unknown field" message, which
/// does not tell the operator what replaces it. This preflight exists
/// only to make that ONE failure actionable -- it never masks a genuine
/// parse error or any other unknown field, both of which fall through
/// to the normal typed deserialize untouched.
///
/// Same pattern as [`preflight_config_version`]; callers wire this in
/// alongside it, before the typed deserialize, at both cold startup
/// (propagate the error, fail hard) and hot config reload (reject the
/// reload, keep the prior router live).
pub fn preflight_legacy_mitm_credential_source(
    raw_toml: &str,
) -> Result<(), LegacyMitmCredentialSourceError> {
    let carries_legacy_key = toml::from_str::<toml::Value>(raw_toml)
        .ok()
        .and_then(|v| v.get("mitm").and_then(toml::Value::as_table).cloned())
        .is_some_and(|mitm| mitm.contains_key("credential_source"));

    if carries_legacy_key {
        return Err(LegacyMitmCredentialSourceError);
    }
    Ok(())
}

#[cfg(test)]
mod legacy_mitm_credential_source_preflight_tests {
    use super::{
        LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK, preflight_legacy_mitm_credential_source,
    };

    #[test]
    fn rejects_forwarded_value() {
        let err =
            preflight_legacy_mitm_credential_source("[mitm]\ncredential_source = \"forwarded\"\n")
                .expect_err("legacy key must be rejected regardless of its value");
        let msg = err.to_string();
        assert!(msg.contains("credential_source"), "msg: {msg}");
        assert!(
            msg.contains(LEGACY_MITM_CREDENTIAL_SOURCE_REPLACEMENT_BLOCK),
            "msg must name the exact replacement block: {msg}"
        );
    }

    #[test]
    fn rejects_own_value() {
        // Arrange / Act
        let result =
            preflight_legacy_mitm_credential_source("[mitm]\ncredential_source = \"own\"\n");

        // Assert: the key itself is the problem, not the value.
        assert!(result.is_err());
    }

    /// The replacement block is the actionable payload of the error --
    /// it must be the exact 4-line shape the provider-level schema
    /// accepts (kind, base_url, credential_source, no api_key_ref), not
    /// a paraphrase.
    #[test]
    fn error_names_the_exact_provider_replacement_shape() {
        let err =
            preflight_legacy_mitm_credential_source("[mitm]\ncredential_source = \"forwarded\"\n")
                .expect_err("legacy key must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("kind = \"anthropic-api\""), "msg: {msg}");
        assert!(
            msg.contains("base_url = \"https://api.anthropic.com\""),
            "msg: {msg}"
        );
        assert!(
            msg.contains("credential_source = \"forwarded\""),
            "msg: {msg}"
        );
        assert!(
            !msg.contains("api_key_ref ="),
            "msg must not suggest an api_key_ref: {msg}"
        );
    }

    #[test]
    fn allows_transport_only_mitm_block() {
        assert!(preflight_legacy_mitm_credential_source("[mitm]\n").is_ok());
    }

    #[test]
    fn allows_absent_mitm_block() {
        assert!(preflight_legacy_mitm_credential_source("").is_ok());
        assert!(
            preflight_legacy_mitm_credential_source("[server]\nhost = \"127.0.0.1\"\n").is_ok()
        );
    }

    /// Sanity mirror of `preflight_config_version`'s own sanity test: the
    /// raw `deny_unknown_fields` deserialize error for this exact input
    /// does NOT already carry the actionable replacement text -- this
    /// preflight is the reason the operator sees more than "unknown
    /// field `credential_source`".
    #[test]
    fn raw_deserialize_error_alone_lacks_the_actionable_replacement() {
        let raw = "[mitm]\ncredential_source = \"forwarded\"\n";
        let deny_unknown_err = toml::from_str::<crate::config::Config>(raw)
            .expect_err("legacy key must still fail the typed deserialize too");
        assert!(
            !deny_unknown_err
                .to_string()
                .contains("[providers.anthropic-forwarded]"),
            "sanity: the raw deserialize error must NOT already name the replacement block"
        );
    }
}

/// Per-million-token pricing for one `[registry.*]` entry. All fields
/// are USD per million tokens and all are optional -- routectl ships no
/// price defaults, so any field left unset means "this dimension is
/// unpriced" and contributes nothing to a derived cost. Cost is computed
/// at query time, never persisted, so a corrected price retroactively
/// fixes historical rows.
///
/// `Eq` is deliberately NOT derived: `f64` is not `Eq`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PricingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_per_mtok: Option<f64>,
}

/// One row in the `[registry]` table. Keyed by an upstream-id glob
/// (exact id or trailing-`*` prefix). Carries optional pricing and an
/// optional `provider` scope so the same upstream id served by two
/// providers can be priced differently. The block is named `[registry.*]`
/// deliberately to leave room for future capability metadata (e.g.
/// `[registry.*.capabilities]`); only `pricing` is built today.
///
/// `Eq` is deliberately NOT derived: `PricingConfig` carries `f64`.
///
/// Unknown fields are tolerated here (no `deny_unknown_fields`): the
/// block is intended to grow a future `[registry.*.capabilities]`
/// sub-table, so a newer config carrying that key must not fail against
/// an older binary. `PricingConfig` still rejects typos (its field set
/// is stable).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct RegistryEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<PricingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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

/// One row in the `[models]` table. Carries the nickname-to-upstream
/// binding plus per-model knobs.
///
/// Fields that vary per-model belong here. Fields that vary per-
/// transport (auth, base URL, runtime gates) stay on `[providers.X]`.
/// Two fields, `header_extras` and `payload_extras`, live BOTH here and
/// on every provider variant; the dispatch layer merges them per
/// request (model wins on key collision; see `Router` merge helpers).
///
/// PER-MODEL KNOB RELAY (read before adding a knob here).
/// A per-model value the egress reads passes through FOUR struct
/// definitions, each a deliberate layer, not a redundant copy:
///
///   1. `ModelEntry`                (this struct, config.rs) -- TOML
///      deserialization target; `#[serde(deny_unknown_fields)]`.
///   2. `crate::resolved::ResolvedModel`   -- startup-resolved per
///      nickname; adds the `Arc<dyn Provider>` and may transform the
///      type (e.g. `Vec<String>` -> `Arc<[String]>` for `effort_levels`).
///   3. `crate::router::DispatchTarget`    -- per-request dispatch hop;
///      adds `state_key`, makes `provider` optional.
///   4. `routectl_core::RoutectlInternal`  -- the transport-internal
///      carrier on `ChatRequest`; the egress reads the value here.
///      `apply_layered_overlays` (router.rs) copies it from the
///      `DispatchTarget`.
///
/// Verbatim pass-through fields (`supports_adaptive_thinking`,
/// `effort_levels`, `max_thinking_budget`, `max_output_tokens`) are
/// relayed unchanged at each hop -- adding one means editing all four
/// definitions plus the `factory.rs` `ModelEntry` -> `ResolvedModel`
/// mapping and the `apply_layered_overlays` copy. The hop is the price
/// of the config-crate (TOML serde shape) / core-crate (wire-internal)
/// separation: `reasoning_dialect` / `history_reasoning` even change
/// type at the core boundary (config-side enum -> `Core*` enum via the
/// `From` impls below), so a single shared struct cannot span both
/// crates without inverting the `core <- router` dependency.
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

    /// Operator-declared label echoed back in the response `model`
    /// field for this model. `None` (the default) makes the response
    /// echo the client's requested alias (`req.model`); `Some(label)`
    /// overrides that with a fixed public-facing string. An empty
    /// string is treated as unset (falls through to `req.model`). Does
    /// not affect internal accounting / observability, which key off
    /// `DispatchMeta.served_model` / `served_upstream`, not `resp.model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,

    /// Whether the response `routectl_provider` field is exposed to the
    /// client for this model. `true` (the default) stamps the served
    /// upstream provider name onto the response; `false` suppresses it
    /// (serde drops the absent field). Does not affect internal
    /// accounting / observability, which key off
    /// `DispatchMeta.served_provider` / `served_upstream`.
    #[serde(default = "default_true")]
    pub visible_routectl_provider: bool,

    /// Operator-supplied list of feature keys this MODEL does not
    /// support, unioned with the per-provider list (`ProviderEntry::
    /// unsupported_features`) at filter time. Same key vocabulary and
    /// concept as the provider-side field -- a model listed here is
    /// skipped before dispatch when the request needs any of these
    /// features -- but keyed at the finer model scope: two nicknames on
    /// one provider (e.g. opus-4-8 vs opus-4-6 on Bedrock) filter
    /// independently. A feature is unsupported if EITHER the model OR
    /// the provider list contains it. Empty (the default) preserves the
    /// pre-existing provider-only behavior. See feature-key derivation
    /// in `crates/routectl-router/src/feature_keys.rs`.
    #[serde(default)]
    pub unsupported_features: Vec<String>,
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
            reported_model: None,
            visible_routectl_provider: true,
            unsupported_features: Vec::new(),
        }
    }
    /// Set whether this model supports the Anthropic adaptive thinking
    /// shape. Projected via apply_layered_overlays into
    /// RoutectlInternal.supports_adaptive_thinking; the AnthropicApi
    /// egress reads it at request time.
    pub const fn with_supports_adaptive_thinking(mut self, b: bool) -> Self {
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
    pub const fn with_max_thinking_budget(mut self, budget: u32) -> Self {
        self.max_thinking_budget = budget;
        self
    }

    pub const fn with_reasoning_dialect(mut self, d: ReasoningDialect) -> Self {
        self.reasoning_dialect = Some(d);
        self
    }

    pub const fn with_history_reasoning(mut self, h: HistoryReasoning) -> Self {
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
    pub const fn with_selectable(mut self, b: bool) -> Self {
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

    /// Set the client-visible label echoed in the response `model`
    /// field. Free-form string; an empty string is treated as unset
    /// at stamp time (falls through to the client's requested alias).
    pub fn with_reported_model(mut self, label: impl Into<String>) -> Self {
        self.reported_model = Some(label.into());
        self
    }

    /// Set whether the response `routectl_provider` field is exposed to
    /// the client for this model. Defaults to `true`; set `false` to
    /// suppress the served-provider name on the response.
    pub const fn with_visible_routectl_provider(mut self, b: bool) -> Self {
        self.visible_routectl_provider = b;
        self
    }

    /// Set the per-model `unsupported_features` list. Unioned with the
    /// provider-side list at filter time; an empty vec (the default)
    /// preserves provider-only behavior.
    pub fn with_unsupported_features(mut self, features: Vec<String>) -> Self {
        self.unsupported_features = features;
        self
    }
}

const fn default_true() -> bool {
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
            Self::Single(s) => NicknameIter::Single(Some(s.as_str())),
            Self::Chain(v) => NicknameIter::Chain(v.iter()),
        }
    }

    pub const fn is_empty(&self) -> bool {
        match self {
            Self::Single(_) => false,
            Self::Chain(v) => v.is_empty(),
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
            NicknameIter::Chain(iter) => iter.next().map(std::string::String::as_str),
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
#[serde(deny_unknown_fields)]
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

const fn default_allow_disable_fallbacks() -> bool {
    true
}

const fn default_max_body_bytes() -> u32 {
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
#[serde(deny_unknown_fields)]
pub struct ServerAuth {
    /// Allowed tokens, stored as SecretRef URIs. Empty list means
    /// "no auth required" (loopback dev default).
    #[serde(default)]
    pub tokens: Vec<String>,
}

fn default_host() -> String {
    "127.0.0.1".into()
}

const fn default_port() -> u16 {
    8787
}

/// Describes what a provider supports re: Anthropic-style prompt-cache
/// breakpoints. The dispatch path consults this to decide whether to
/// auto-emit a top-level `cache_control` breakpoint -- it does so only
/// for providers that actually honor one (anthropic-api) and never for
/// providers that ignore or silently drop it (OpenAI-shape; Bedrock,
/// which caches only off per-block markers, not a top-level one).
///
/// Per-kind defaults are deliberately conservative; an unknown provider
/// kind is treated as supporting nothing so routectl never emits a
/// breakpoint to an upstream whose behavior it cannot vouch for.
/// Operators override per-entry via the `cache_capability` TOML key
/// when a non-Anthropic upstream behind an anthropic-api-shaped provider
/// differs from the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CacheCapability {
    /// Whether the upstream honors an explicit top-level Anthropic
    /// `cache_control` breakpoint. When false the dispatch path must not
    /// auto-emit one.
    pub supports_top_level_cache_control: bool,
    /// Whether the upstream reports cache-hit usage (e.g.
    /// `cache_read_input_tokens` / `cached_tokens`) back in its response.
    pub cache_hit_observable: bool,
}

impl CacheCapability {
    /// Build a capability from its two flags. Use this for explicit
    /// construction from outside the crate (the struct is
    /// `#[non_exhaustive]`, so struct-literal syntax is unavailable there).
    pub const fn new(supports_top_level_cache_control: bool, cache_hit_observable: bool) -> Self {
        Self {
            supports_top_level_cache_control,
            cache_hit_observable,
        }
    }

    /// Conservative per-kind default for a provider kind token (the
    /// stable `kind = "..."` discriminant). An unrecognized kind maps to
    /// "supports nothing" so the dispatch path never auto-emits a
    /// breakpoint to an upstream routectl does not understand.
    pub fn for_provider_kind(kind: &str) -> Self {
        match kind {
            "anthropic-api" => Self::new(true, true),
            // Conservative kind-level default used only when no
            // `api_shape` is known (the kind token carries no shape).
            // `ProviderEntry::cache_capability()` special-cases the
            // Bedrock variant and derives the live value from `api_shape`
            // instead: Invoke -> supports_top_level = true, because the
            // egress lowers a routectl-injected top-level marker to the
            // per-block form Invoke caches on (commit 3e12f88); Converse
            // -> false, because a top-level marker is inert there (no
            // `cachePoint` translation). Hit usage is reported back on
            // both shapes, so cache_hit_observable stays true. This
            // kind-level value stays fail-closed as the no-shape fallback
            // so the dispatch path never auto-emits to a shape it cannot
            // reason about. Caller-supplied per-block markers cache
            // normally regardless; operators may override per-entry.
            "bedrock" => Self::new(false, true),
            // OpenAI auto-caches server-side; there is no explicit
            // breakpoint to emit, but `cached_tokens` IS reported back.
            "openai-responses" => Self::new(false, true),
            // Gemini has implicit prefix caching (automatic, free writes)
            // and explicit context caching. No top-level breakpoint to
            // emit, but `cachedContentTokenCount` is reported back.
            "gemini" => Self::new(false, true),
            // openai-compat and every unknown kind: emit nothing.
            _ => Self::new(false, false),
        }
    }
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
        /// Operator override for this entry's prompt-cache capability.
        /// `None` -> use the conservative per-kind default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_capability: Option<CacheCapability>,
        /// Per-provider override for dispatch-path auto-emission of a
        /// top-level `cache_control` breakpoint. `None` inherits the
        /// global `[cache]` switch (treated as enabled); `Some(false)`
        /// disables auto-emit for this provider even when global is on.
        /// Cache policy, NOT a runtime/rate knob -- lives outside
        /// `runtime`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_top_level_breakpoint: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider even when
        /// global is on. Reduction policy, NOT a runtime/rate knob --
        /// lives outside `runtime`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    #[non_exhaustive]
    AnthropicApi {
        /// Reference to the API key, resolved the same way as every
        /// other provider's `api_key_ref`. Defaulted (empty string) so a
        /// `credential_source = "forwarded"` block can omit it entirely
        /// -- `validate_provider_credential_sources` then REQUIRES it be
        /// empty for `forwarded` and non-empty for `own`.
        #[serde(default)]
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
        /// Which credential this provider's Anthropic egress
        /// authenticates with. Default `own`: the provider uses
        /// `api_key_ref`/`auth_kind` exactly as before. `forwarded`
        /// relays the client's captured claude.ai bearer instead --
        /// `validate_provider_credential_sources` requires `base_url`'s
        /// host be exactly `api.anthropic.com` and `api_key_ref` be
        /// empty whenever this is `forwarded`.
        #[serde(default)]
        credential_source: CredentialSource,
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
        /// Operator override for this entry's prompt-cache capability.
        /// `None` -> use the conservative per-kind default. Useful when a
        /// non-Anthropic upstream behind this anthropic-api-shaped
        /// provider does not honor a top-level breakpoint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_capability: Option<CacheCapability>,
        /// Per-provider override for dispatch-path auto-emission of a
        /// top-level `cache_control` breakpoint. `None` inherits the
        /// global `[cache]` switch; `Some(false)` disables auto-emit for
        /// this provider. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_top_level_breakpoint: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        /// Opt-in OAuth-cloak configuration. Omitting the `[cloak]`
        /// sub-table yields `CloakConfig::default()` (mode `auto`, no
        /// strict mode, no tool renames, no sensitive words) -- identical
        /// to the always-on `mcp_` normalization. TOML surface:
        /// `[providers.X.cloak]` with `mode` / `strict_mode` /
        /// `sensitive_words`, and tool renames as an array of tables
        /// `[[providers.X.cloak.tool_rename]] from=.. to=..`.
        #[serde(default)]
        cloak: CloakConfig,
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
        /// Operator override for this entry's prompt-cache capability.
        /// `None` -> use the conservative per-kind default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_capability: Option<CacheCapability>,
        /// Per-provider override for dispatch-path auto-emission of a
        /// top-level `cache_control` breakpoint. `None` inherits the
        /// global `[cache]` switch; `Some(false)` disables auto-emit for
        /// this provider. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_top_level_breakpoint: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
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
        /// Operator override for this entry's prompt-cache capability.
        /// `None` -> use the conservative per-kind default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_capability: Option<CacheCapability>,
        /// Per-provider override for dispatch-path auto-emission of a
        /// top-level `cache_control` breakpoint. `None` inherits the
        /// global `[cache]` switch; `Some(false)` disables auto-emit for
        /// this provider. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_top_level_breakpoint: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    /// Native Google Gemini provider (REST v1beta `generateContent`).
    #[cfg(feature = "gemini")]
    #[non_exhaustive]
    Gemini {
        /// Resolves to the Gemini API key (sent as `x-goog-api-key`).
        api_key_ref: String,
        /// Endpoint base URL. Default (api-key mode):
        /// `https://generativelanguage.googleapis.com/v1beta`. In
        /// `cloud-code` mode the effective default is
        /// `cloudcode-pa.googleapis.com`; leave this unset for cloud-code
        /// unless targeting a test/staging host (it is only forwarded to
        /// the provider when it differs from the public-surface default).
        #[serde(default = "default_gemini_base")]
        base_url: String,
        /// Provider-level header extras.
        #[serde(default)]
        header_extras: BTreeMap<String, String>,
        /// Provider-level payload extras.
        #[serde(default)]
        payload_extras: Option<Value>,
        /// Override the outbound User-Agent.
        #[serde(default)]
        user_agent: Option<String>,
        /// Selects the Gemini wire dialect. `api-key` (default) uses the
        /// public `generativelanguage.googleapis.com` REST surface with the
        /// `x-goog-api-key` header. `cloud-code` uses the Cloud Code
        /// ("antigravity") surface with an OAuth bearer; in that mode
        /// `api_key_ref` MUST be an `oauth://<provider>` reference.
        #[serde(default)]
        auth_mode: GeminiAuthMode,
        /// Operator override for this entry's prompt-cache capability.
        /// `None` -> use the conservative per-kind default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_capability: Option<CacheCapability>,
        /// Per-provider override for dispatch-path auto-emission of a
        /// top-level `cache_control` breakpoint. `None` inherits the
        /// global `[cache]` switch; `Some(false)` disables auto-emit for
        /// this provider. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_top_level_breakpoint: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
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
/// creds = { kind = "bearer-key", key_ref = "file://<local-path>" }
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
    /// Stable config-key token naming this entry's provider kind. Matches
    /// the `kind = "..."` discriminant in the TOML provider table, so the
    /// returned value round-trips with operator configuration and is safe
    /// to surface in usage accounting / logs.
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::OpenaiCompat { .. } => "openai-compat",
            Self::AnthropicApi { .. } => "anthropic-api",
            #[cfg(feature = "bedrock")]
            Self::Bedrock { .. } => "bedrock",
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { .. } => "openai-responses",
            #[cfg(feature = "gemini")]
            Self::Gemini { .. } => "gemini",
        }
    }

    /// The primary `api_key_ref` URI for this entry, or `None` for
    /// variants with no single canonical key slot (Bedrock). The usage
    /// CLI inspects this to detect a subscription provider (an
    /// `oauth://`-prefixed ref), which has no per-token dollar cost.
    pub fn api_key_ref(&self) -> Option<&str> {
        match self {
            Self::OpenaiCompat { api_key_ref, .. } => Some(api_key_ref),
            Self::AnthropicApi { api_key_ref, .. } => Some(api_key_ref),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { api_key_ref, .. } => Some(api_key_ref),
            #[cfg(feature = "bedrock")]
            Self::Bedrock { .. } => None,
            #[cfg(feature = "gemini")]
            Self::Gemini { api_key_ref, .. } => Some(api_key_ref),
        }
    }

    /// Get the runtime policy attached to this entry. Centralizes the
    /// match so the router doesn't repeat it.
    pub const fn runtime(&self) -> &ProviderRuntimePolicy {
        match self {
            Self::OpenaiCompat { runtime, .. } | Self::AnthropicApi { runtime, .. } => runtime,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { runtime, .. } => runtime,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { runtime, .. } => runtime,
            #[cfg(feature = "gemini")]
            Self::Gemini { runtime, .. } => runtime,
        }
    }

    /// `Some(base_url)` iff this entry is an `AnthropicApi` variant with
    /// `credential_source == Forwarded`, `None` otherwise. The single
    /// place that recognizes "this is the forwarded provider" so
    /// callers (a boolean scan, a dispatch-target flag, and the
    /// forwarded-proxy `/v1/models` handler) don't each hand-roll the
    /// same `matches!` against the enum's inner fields.
    pub fn forwarded_base_url(&self) -> Option<&str> {
        match self {
            Self::AnthropicApi {
                credential_source: CredentialSource::Forwarded,
                base_url,
                ..
            } => Some(base_url),
            _ => None,
        }
    }

    /// Per-provider `header_extras`. Returns a reference to the
    /// per-variant map so the dispatch-layer merge helpers can read
    /// without re-matching the enum.
    pub const fn header_extras(&self) -> &BTreeMap<String, String> {
        match self {
            Self::OpenaiCompat { header_extras, .. } => header_extras,
            Self::AnthropicApi { header_extras, .. } => header_extras,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { header_extras, .. } => header_extras,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { header_extras, .. } => header_extras,
            #[cfg(feature = "gemini")]
            Self::Gemini { header_extras, .. } => header_extras,
        }
    }

    /// Per-provider `payload_extras`. Returns a reference (None when
    /// the operator did not configure any) so the dispatch-layer deep
    /// merge can borrow without cloning on the no-op path.
    pub const fn payload_extras(&self) -> Option<&Value> {
        match self {
            Self::OpenaiCompat { payload_extras, .. } => payload_extras.as_ref(),
            Self::AnthropicApi { payload_extras, .. } => payload_extras.as_ref(),
            #[cfg(feature = "bedrock")]
            Self::Bedrock { payload_extras, .. } => payload_extras.as_ref(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { payload_extras, .. } => payload_extras.as_ref(),
            #[cfg(feature = "gemini")]
            Self::Gemini { payload_extras, .. } => payload_extras.as_ref(),
        }
    }

    /// Prompt-cache capability for this entry. Returns the operator
    /// override when set, otherwise the conservative per-kind default.
    ///
    /// AnthropicApi is special-cased on its `base_url`: a `kind =
    /// "anthropic-api"` entry pointed at a NON-default base_url is an
    /// Anthropic-COMPATIBLE third party (e.g. a vendor's `/anthropic`
    /// surface) that may 400 on or silently drop a top-level
    /// `cache_control` breakpoint. Since auto-emit is default-on, the
    /// optimistic per-kind default (true/true) would risk breaking such
    /// a host. So when there is no operator override and the base_url is
    /// not the default Anthropic base, fail closed -> (false, false). An
    /// operator who knows their custom-base host supports caching opts
    /// in via an explicit `cache_capability` (which always wins below).
    pub fn cache_capability(&self) -> CacheCapability {
        if let Self::AnthropicApi {
            base_url,
            cache_capability,
            ..
        } = self
        {
            return cache_capability.unwrap_or_else(|| {
                if base_url == &default_anthropic_base() {
                    CacheCapability::for_provider_kind(self.kind_str())
                } else {
                    CacheCapability::new(false, false)
                }
            });
        }

        // Bedrock is special-cased on its `api_shape`: the two shapes
        // cache a top-level `cache_control` marker differently. On
        // Invoke the egress lowers a routectl-injected top-level marker
        // to the per-block form Invoke caches on (commit 3e12f88), so
        // auto-emit is safe -> supports_top_level = true. On Converse a
        // top-level marker is inert (no `cachePoint` translation), so it
        // stays fail-closed. Hit usage is reported back on both shapes.
        // An explicit operator override always wins (mirrors the
        // AnthropicApi case above).
        #[cfg(feature = "bedrock")]
        if let Self::Bedrock {
            api_shape,
            cache_capability,
            ..
        } = self
        {
            return cache_capability.unwrap_or_else(|| match api_shape {
                BedrockApiShapeConfig::Invoke => CacheCapability::new(true, true),
                BedrockApiShapeConfig::Converse => CacheCapability::new(false, true),
            });
        }

        let override_value = match self {
            Self::OpenaiCompat {
                cache_capability, ..
            } => cache_capability,
            Self::AnthropicApi {
                cache_capability, ..
            } => cache_capability,
            #[cfg(feature = "bedrock")]
            Self::Bedrock {
                cache_capability, ..
            } => cache_capability,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                cache_capability, ..
            } => cache_capability,
            #[cfg(feature = "gemini")]
            Self::Gemini {
                cache_capability, ..
            } => cache_capability,
        };
        override_value.unwrap_or_else(|| CacheCapability::for_provider_kind(self.kind_str()))
    }

    /// Per-provider override for dispatch-path auto-emission of a
    /// top-level `cache_control` breakpoint. `None` means "inherit the
    /// global `[cache]` switch" (the dispatch path treats that as
    /// enabled); `Some(false)` disables auto-emit for this provider even
    /// when the global switch is on. Mirrors `cache_capability()`.
    pub const fn auto_emit_top_level_breakpoint(&self) -> Option<bool> {
        match self {
            Self::OpenaiCompat {
                auto_emit_top_level_breakpoint,
                ..
            } => *auto_emit_top_level_breakpoint,
            Self::AnthropicApi {
                auto_emit_top_level_breakpoint,
                ..
            } => *auto_emit_top_level_breakpoint,
            #[cfg(feature = "bedrock")]
            Self::Bedrock {
                auto_emit_top_level_breakpoint,
                ..
            } => *auto_emit_top_level_breakpoint,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                auto_emit_top_level_breakpoint,
                ..
            } => *auto_emit_top_level_breakpoint,
            #[cfg(feature = "gemini")]
            Self::Gemini {
                auto_emit_top_level_breakpoint,
                ..
            } => *auto_emit_top_level_breakpoint,
        }
    }

    /// Per-provider override for the dispatch-path token-reduction
    /// feature. `None` means "inherit the global `[reduction]` switch";
    /// `Some(false)` disables reduction for this provider even when the
    /// global switch is on. Mirrors `auto_emit_top_level_breakpoint()`.
    ///
    /// The effective decision rule a later dispatch task will consume:
    /// reduction applies when the global `[reduction] enabled == true` AND
    /// this override is not explicitly `Some(false)`. That decision logic
    /// is NOT implemented here -- this is the config surface only.
    pub const fn reduction_enabled(&self) -> Option<bool> {
        match self {
            Self::OpenaiCompat {
                reduction_enabled, ..
            } => *reduction_enabled,
            Self::AnthropicApi {
                reduction_enabled, ..
            } => *reduction_enabled,
            #[cfg(feature = "bedrock")]
            Self::Bedrock {
                reduction_enabled, ..
            } => *reduction_enabled,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                reduction_enabled, ..
            } => *reduction_enabled,
            #[cfg(feature = "gemini")]
            Self::Gemini {
                reduction_enabled, ..
            } => *reduction_enabled,
        }
    }

    pub fn openai_compat(base_url: impl Into<String>, api_key_ref: impl Into<String>) -> Self {
        Self::OpenaiCompat {
            base_url: base_url.into(),
            api_key_ref: api_key_ref.into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    pub fn anthropic_api(api_key_ref: impl Into<String>) -> Self {
        Self::AnthropicApi {
            api_key_ref: api_key_ref.into(),
            base_url: default_anthropic_base(),
            anthropic_version: default_anthropic_version(),
            auth_kind: AuthKind::ApiKey,
            credential_source: CredentialSource::Own,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            cloak: CloakConfig::default(),
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
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// Construct a `Gemini` entry with sane defaults. The only required
    /// field is `api_key_ref` (resolved and sent as the `x-goog-api-key`
    /// header); `base_url` defaults to the public v1beta endpoint and
    /// everything else defaults to empty / `None`. Use `with_header_extras`
    /// / `with_payload_extras` to populate the optional fields.
    #[cfg(feature = "gemini")]
    pub fn gemini(api_key_ref: impl Into<String>) -> Self {
        Self::Gemini {
            api_key_ref: api_key_ref.into(),
            base_url: default_gemini_base(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            auth_mode: GeminiAuthMode::default(),
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
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

    #[cfg(feature = "gemini")]
    pub fn with_gemini_auth_mode(mut self, mode: GeminiAuthMode) -> Self {
        match &mut self {
            Self::Gemini { auth_mode, .. } => *auth_mode = mode,
            _ => panic!("ProviderEntry::with_gemini_auth_mode only applies to gemini"),
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
                *runtime = rt;
            }
            #[cfg(feature = "bedrock")]
            Self::Bedrock { runtime, .. } => *runtime = rt,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { runtime, .. } => *runtime = rt,
            #[cfg(feature = "gemini")]
            Self::Gemini { runtime, .. } => *runtime = rt,
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
            #[cfg(feature = "gemini")]
            Self::Gemini { header_extras, .. } => *header_extras = headers,
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
            #[cfg(feature = "gemini")]
            Self::Gemini { payload_extras, .. } => *payload_extras = slot,
        }
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let u = url.into();
        match &mut self {
            Self::OpenaiCompat { base_url, .. } | Self::AnthropicApi { base_url, .. } => {
                *base_url = u;
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

    /// Set the AnthropicApi variant's `credential_source`. Panics on
    /// other variants -- the field is AnthropicApi-only.
    pub fn with_credential_source(mut self, source: CredentialSource) -> Self {
        match &mut self {
            Self::AnthropicApi {
                credential_source, ..
            } => *credential_source = source,
            _ => panic!("ProviderEntry::with_credential_source only applies to anthropic-api"),
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
            #[cfg(feature = "gemini")]
            Self::Gemini { api_key_ref, .. } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
            }
        }
    }

    pub fn secret_uris(&self) -> Vec<&str> {
        match self {
            Self::OpenaiCompat { api_key_ref, .. } => vec![api_key_ref.as_str()],
            // A `forwarded` entry's `api_key_ref` is intentionally empty
            // (validated by `validate_provider_credential_sources`, not
            // resolved through a `SecretStore`) -- an empty ref is not a
            // secret URI to resolve, so surfacing it would fail
            // `SecretRef::parse` with a spurious "unrecognized scheme"
            // error on an otherwise-clean forwarded provider.
            Self::AnthropicApi { api_key_ref, .. } if api_key_ref.is_empty() => Vec::new(),
            Self::AnthropicApi { api_key_ref, .. } => vec![api_key_ref.as_str()],
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
            #[cfg(feature = "gemini")]
            Self::Gemini { api_key_ref, .. } => vec![api_key_ref.as_str()],
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
    /// entry routing through this provider. Two-tier resolution
    /// (per-provider > global): this field fills in only when the
    /// global `[retry] request_timeout_ms` left the timeout unset.
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
    /// when the request needs any of these features. Tool-type keys are
    /// derived from built-in tool `type` strings (e.g. `web_search`,
    /// `computer_use`). The `structured_output` key is request-derived
    /// (NOT a tool type): it fires when the request carries
    /// `output_config.format` or any strict tool, both of which need
    /// constrained decoding some upstreams (e.g. a Bedrock Invoke leg)
    /// cannot enforce. See feature-key derivation in
    /// `crates/routectl-router/src/feature_keys.rs`.
    #[serde(default)]
    pub unsupported_features: Vec<String>,

    /// Operator remap of a raw upstream status code to a config-facing
    /// failure class, keyed by the numeric status. Empty (the default)
    /// leaves the built-in status-to-class classification untouched. The
    /// custom (de)serializer routes the numeric keys through string form
    /// so the map survives serde's `flatten` buffering (see
    /// `class_policy::status_class_overrides`).
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "crate::class_policy::status_class_overrides"
    )]
    pub class_overrides: BTreeMap<u16, ConfigFailureClass>,

    /// How dispatch picks among multiple OAuth seats configured for this
    /// provider's credential pool. `fill-first` (the default) drains one
    /// seat before moving to the next; `round-robin` spreads load across
    /// seats; `sticky-least-loaded` pins each conversation to one seat for
    /// prompt-cache affinity while balancing new conversations across seats
    /// by load. Applied per request at dispatch time whenever the
    /// provider's credential pool resolves to more than one seat.
    #[serde(default)]
    pub seat_selection: SeatSelection,
}

/// Per-provider seat-selection strategy for the OAuth credential pool.
/// Default is `fill-first` so a single-seat provider (the common case)
/// keeps its current behavior with no config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeatSelection {
    /// Drain one seat fully before advancing to the next.
    #[default]
    FillFirst,
    /// Rotate across seats to spread load.
    RoundRobin,
    /// Pin each conversation to one seat for prompt-cache affinity, and
    /// balance NEW conversations across seats by load. The contract: a
    /// conversation's first request picks the least-loaded healthy seat at
    /// birth; every subsequent request for that conversation routes back to
    /// the same seat so its warm prompt cache is preserved (avoiding a cold
    /// miss on every turn). If a pinned home goes unhealthy the conversation
    /// migrates once to a healthy sibling and stays there (no flapping back).
    /// The selection is a best-effort reorder of the walk: the per-seat
    /// dispatch gate and the fill-first fallback walk stay authoritative, so
    /// a stale or wrong pin only costs locality, never correctness.
    StickyLeastLoaded,
}

fn default_anthropic_base() -> String {
    "https://api.anthropic.com".into()
}

fn default_anthropic_version() -> String {
    "2023-06-01".into()
}

#[cfg(feature = "gemini")]
pub(crate) fn default_gemini_base() -> String {
    "https://generativelanguage.googleapis.com/v1beta".into()
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
    #[serde(
        default = "default_stream_first_byte_timeout_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub stream_first_byte_timeout_ms: Option<u64>,

    /// Requests with `max_tokens` <= this are treated as availability
    /// probes (Claude Code sends `max_tokens=1`); on a rate-limit /
    /// overload (429/529) they skip retry+fallback and return the
    /// status immediately, since walking the chain is futile and the
    /// probe output is unused. `0` disables. Real requests (max_tokens
    /// above this) are unaffected.
    #[serde(default = "default_probe_max_tokens")]
    pub probe_max_tokens: u32,

    /// Ceiling on how long the circuit breaker will park a provider in
    /// response to an upstream reset hint (a parsed `Retry-After` value
    /// carried on `Error::Upstream`). A reset hint longer than this
    /// ceiling is clamped down to it, so a misbehaving or hostile
    /// upstream cannot pin a provider out of rotation for an arbitrary
    /// duration. `None` (the default) uses
    /// `DEFAULT_MAX_HONORED_RETRY_AFTER_MS` (1 hour). Read via
    /// [`RetryPolicy::max_honored_retry_after`].
    #[serde(default)]
    pub max_honored_retry_after_ms: Option<u64>,

    /// Per-error-class policy overlay keyed by the config-facing failure
    /// class. A present entry overrides only the leaves it names (retry
    /// cap and/or fallback), layered over the baked class defaults; an
    /// absent entry keeps the baked defaults. An empty map (the default)
    /// leaves every class at its baked default. See
    /// [`RetryPolicy::resolved_class`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub classes: BTreeMap<ConfigFailureClass, ClassPolicy>,
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
            // F1's early-response inversion holds the client warm, so a
            // pinging-but-contentless upstream would otherwise be
            // unbounded (the client no longer bails at ~300s). 600000ms
            // mirrors the live deployment and sits well above the 300s
            // read_timeout as a total-silence backstop. Shares
            // `default_stream_first_byte_timeout_ms` with the serde
            // field default so a `[retry]` block that omits this key
            // gets the same backstop, not `None`.
            stream_first_byte_timeout_ms: default_stream_first_byte_timeout_ms(),
            probe_max_tokens: default_probe_max_tokens(),
            max_honored_retry_after_ms: None,
            classes: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheCapability, Config, ProviderEntry, ReductionConfig};
    #[cfg(feature = "gemini")]
    use routectl_providers::gemini::GeminiAuthMode;

    #[test]
    #[should_panic(expected = "with_anthropic_version")]
    fn wrong_variant_setter_panics() {
        let _ = ProviderEntry::openai_compat("https://example.com/v1", "literal:test")
            .with_anthropic_version("2023-06-01");
    }

    #[test]
    fn kind_str_returns_stable_config_tokens() {
        assert_eq!(
            ProviderEntry::openai_compat("https://example.com/v1", "literal:k").kind_str(),
            "openai-compat",
        );
        assert_eq!(
            ProviderEntry::anthropic_api("literal:k").kind_str(),
            "anthropic-api",
        );
        #[cfg(feature = "openai-responses")]
        assert_eq!(
            ProviderEntry::openai_responses("literal:k").kind_str(),
            "openai-responses",
        );
        #[cfg(feature = "gemini")]
        assert_eq!(ProviderEntry::gemini("literal:k").kind_str(), "gemini",);
        #[cfg(feature = "bedrock")]
        {
            let bedrock = ProviderEntry::Bedrock {
                region: "us-east-1".into(),
                api_shape: super::BedrockApiShapeConfig::default(),
                creds: super::BedrockCredsConfig::DefaultChain,
                user_agent: None,
                header_extras: std::collections::BTreeMap::new(),
                payload_extras: None,
                anthropic_beta: Vec::new(),
                cache_capability: None,
                auto_emit_top_level_breakpoint: None,
                reduction_enabled: None,
                runtime: Default::default(),
            };
            assert_eq!(bedrock.kind_str(), "bedrock");
        }
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_constructor_defaults() {
        let entry = ProviderEntry::gemini("env://GEMINI_API_KEY");
        assert_eq!(entry.kind_str(), "gemini");
        assert_eq!(entry.api_key_ref(), Some("env://GEMINI_API_KEY"));
        match entry {
            ProviderEntry::Gemini {
                base_url,
                header_extras,
                payload_extras,
                ..
            } => {
                assert_eq!(base_url, "https://generativelanguage.googleapis.com/v1beta",);
                assert!(header_extras.is_empty());
                assert!(payload_extras.is_none());
            }
            other => panic!("expected Gemini entry; got {other:?}"),
        }
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_auth_mode_defaults_to_api_key_when_omitted() {
        let toml_text = r#"
[providers.gemini]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse omitted auth_mode");
        let entry = cfg.providers.get("gemini").expect("gemini provider");
        match entry {
            ProviderEntry::Gemini { auth_mode, .. } => {
                assert_eq!(*auth_mode, GeminiAuthMode::ApiKey);
            }
            other => panic!("expected Gemini entry; got {other:?}"),
        }
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_auth_mode_parses_cloud_code() {
        let toml_text = r#"
[providers.gemini]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse cloud-code auth_mode");
        let entry = cfg.providers.get("gemini").expect("gemini provider");
        match entry {
            ProviderEntry::Gemini { auth_mode, .. } => {
                assert_eq!(*auth_mode, GeminiAuthMode::CloudCode);
            }
            other => panic!("expected Gemini entry; got {other:?}"),
        }
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

    #[test]
    fn cache_capability_per_kind_defaults_are_conservative() {
        let anthropic = CacheCapability::for_provider_kind("anthropic-api");
        assert!(anthropic.supports_top_level_cache_control);
        assert!(anthropic.cache_hit_observable);

        // Bedrock caches only off per-block markers, never a top-level one,
        // so auto-emit must fail-closed -- but hit usage is still reported.
        let bedrock = CacheCapability::for_provider_kind("bedrock");
        assert!(!bedrock.supports_top_level_cache_control);
        assert!(bedrock.cache_hit_observable);

        // OpenAI-shape: no explicit breakpoint, but cached_tokens reported.
        let responses = CacheCapability::for_provider_kind("openai-responses");
        assert!(!responses.supports_top_level_cache_control);
        assert!(responses.cache_hit_observable);

        // Gemini: implicit + explicit context caching; no top-level
        // breakpoint to emit, but cachedContentTokenCount is reported.
        let gemini = CacheCapability::for_provider_kind("gemini");
        assert!(!gemini.supports_top_level_cache_control);
        assert!(gemini.cache_hit_observable);

        let compat = CacheCapability::for_provider_kind("openai-compat");
        assert!(!compat.supports_top_level_cache_control);
        assert!(!compat.cache_hit_observable);

        // Unknown kind: never auto-emit.
        let unknown = CacheCapability::for_provider_kind("some-future-kind");
        assert!(!unknown.supports_top_level_cache_control);
        assert!(!unknown.cache_hit_observable);
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_provider_entry_parses_and_exposes_secret_uri() {
        // Minimal: only api_key_ref -> base_url defaults to v1beta.
        let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "literal:test-key"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse minimal");
        match cfg.providers.get("g").expect("gemini provider") {
            ProviderEntry::Gemini {
                api_key_ref,
                base_url,
                ..
            } => {
                assert_eq!(api_key_ref, "literal:test-key");
                assert_eq!(
                    base_url, "https://generativelanguage.googleapis.com/v1beta",
                    "base_url must default to the v1beta endpoint"
                );
            }
            other => panic!("expected Gemini entry; got {other:?}"),
        }

        // Explicit base_url + header_extras.
        let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "env://GEMINI_API_KEY"
base_url = "https://example.test/v1beta"

[providers.g.header_extras]
x-custom = "v"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse explicit");
        let entry = cfg.providers.get("g").expect("gemini provider");
        match entry {
            ProviderEntry::Gemini {
                base_url,
                header_extras,
                ..
            } => {
                assert_eq!(base_url, "https://example.test/v1beta");
                assert_eq!(header_extras.get("x-custom").map(String::as_str), Some("v"));
            }
            other => panic!("expected Gemini entry; got {other:?}"),
        }

        // kind discriminator + secret enumeration / redaction contract.
        assert_eq!(entry.kind_str(), "gemini");
        assert_eq!(entry.secret_uris(), vec!["env://GEMINI_API_KEY"]);
    }

    #[cfg(feature = "gemini")]
    #[test]
    fn gemini_redact_secrets_masks_literal_api_key() {
        let toml_text = r#"
[providers.g]
kind = "gemini"
api_key_ref = "literal:super-secret"
"#;
        let mut cfg: Config = toml::from_str(toml_text).expect("parse");
        let entry = cfg.providers.get_mut("g").expect("gemini provider");
        entry.redact_secrets();
        match entry {
            ProviderEntry::Gemini { api_key_ref, .. } => assert!(
                !api_key_ref.contains("super-secret"),
                "literal key must be redacted; got: {api_key_ref}"
            ),
            other => panic!("expected Gemini entry; got {other:?}"),
        }
    }

    #[test]
    fn cache_capability_falls_back_to_per_kind_default_when_unset() {
        let anthropic = ProviderEntry::anthropic_api("literal:sk-ant-test");
        assert_eq!(
            anthropic.cache_capability(),
            CacheCapability::for_provider_kind("anthropic-api"),
        );

        let compat = ProviderEntry::openai_compat("https://example.com/v1", "literal:k");
        assert_eq!(
            compat.cache_capability(),
            CacheCapability::for_provider_kind("openai-compat"),
        );
    }

    #[cfg(feature = "bedrock")]
    fn bedrock_entry(
        api_shape: super::BedrockApiShapeConfig,
        cache_capability: Option<CacheCapability>,
    ) -> ProviderEntry {
        ProviderEntry::Bedrock {
            region: "us-east-1".into(),
            api_shape,
            creds: super::BedrockCredsConfig::DefaultChain,
            user_agent: None,
            header_extras: std::collections::BTreeMap::new(),
            payload_extras: None,
            anthropic_beta: Vec::new(),
            cache_capability,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            runtime: Default::default(),
        }
    }

    /// The Bedrock Invoke egress lowers a top-level `cache_control`
    /// marker to the per-block form Invoke caches on, so auto-emit is
    /// safe there: `cache_capability()` derives supports_top_level = true
    /// from `api_shape = Invoke`.
    #[cfg(feature = "bedrock")]
    #[test]
    fn cache_capability_bedrock_invoke_supports_top_level() {
        let cap = bedrock_entry(super::BedrockApiShapeConfig::Invoke, None).cache_capability();
        assert!(cap.supports_top_level_cache_control);
        assert!(cap.cache_hit_observable);
    }

    /// A top-level marker is inert on Bedrock Converse (no `cachePoint`
    /// translation), so it stays fail-closed: supports_top_level = false,
    /// hit usage still observable.
    #[cfg(feature = "bedrock")]
    #[test]
    fn cache_capability_bedrock_converse_fails_closed() {
        let cap = bedrock_entry(super::BedrockApiShapeConfig::Converse, None).cache_capability();
        assert!(!cap.supports_top_level_cache_control);
        assert!(cap.cache_hit_observable);
    }

    /// An explicit operator override always wins over the api_shape-
    /// derived default, even when the shape would otherwise enable
    /// auto-emit.
    #[cfg(feature = "bedrock")]
    #[test]
    fn cache_capability_bedrock_override_beats_api_shape() {
        let cap = bedrock_entry(
            super::BedrockApiShapeConfig::Invoke,
            Some(CacheCapability::new(false, false)),
        )
        .cache_capability();
        assert!(!cap.supports_top_level_cache_control);
        assert!(!cap.cache_hit_observable);
    }

    #[test]
    fn cache_capability_operator_override_beats_per_kind_default() {
        // An anthropic-api entry whose upstream does NOT honor a
        // top-level breakpoint but DOES report cache hits.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
cache_capability = { supports_top_level_cache_control = false, cache_hit_observable = true }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse override");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");
        let cap = entry.cache_capability();
        assert!(!cap.supports_top_level_cache_control);
        assert!(cap.cache_hit_observable);
        // The override beats the per-kind default (which is true/true).
        assert_ne!(cap, CacheCapability::for_provider_kind("anthropic-api"));

        // Round-trips through serialize/deserialize.
        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
        let cap_out = cfg_out
            .providers
            .get("anthropic")
            .expect("anthropic")
            .cache_capability();
        assert_eq!(cap_out, cap);
    }

    #[test]
    fn cache_capability_omitted_uses_default_and_deny_unknown_fields_holds() {
        let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse default");
        let entry = cfg.providers.get("openai").expect("openai provider");
        assert_eq!(
            entry.cache_capability(),
            CacheCapability::for_provider_kind("openai-compat"),
        );

        // An unknown sub-field inside cache_capability must be rejected.
        let bad = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
cache_capability = { supports_top_level_cache_control = true, bogus = 1 }
"#;
        assert!(
            toml::from_str::<Config>(bad).is_err(),
            "deny_unknown_fields must reject an unknown CacheCapability field",
        );
    }

    /// An `anthropic-api` entry on the DEFAULT Anthropic base, with no
    /// operator override, gets the optimistic per-kind default (true/true)
    /// -- the real Anthropic server honors a top-level breakpoint.
    #[test]
    fn cache_capability_anthropic_default_base_uses_optimistic_default() {
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse default base");
        let entry = cfg.providers.get("anthropic").expect("anthropic provider");
        let cap = entry.cache_capability();
        assert_eq!(cap, CacheCapability::for_provider_kind("anthropic-api"));
        assert!(cap.supports_top_level_cache_control);
        assert!(cap.cache_hit_observable);
    }

    /// An `anthropic-api` entry on a NON-default base_url (an Anthropic-
    /// compatible third party), with no operator override, fails closed:
    /// auto-emit must never break a host that may not honor cache_control.
    #[test]
    fn cache_capability_anthropic_custom_base_fails_closed() {
        let toml_text = r#"
[providers.compat]
kind = "anthropic-api"
api_key_ref = "literal:sk-test"
base_url = "https://api.example.com/anthropic"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse custom base");
        let entry = cfg.providers.get("compat").expect("compat provider");
        let cap = entry.cache_capability();
        assert!(
            !cap.supports_top_level_cache_control,
            "custom-base anthropic-api must fail closed on cache_control"
        );
        assert!(!cap.cache_hit_observable);
        assert_eq!(cap, CacheCapability::new(false, false));
        // It diverges from the optimistic per-kind default precisely
        // because the base_url is not the default Anthropic base.
        assert_ne!(cap, CacheCapability::for_provider_kind("anthropic-api"));
    }

    /// An explicit operator `cache_capability` override always wins, even
    /// on a custom base_url: the operator knows their host supports it.
    #[test]
    fn cache_capability_anthropic_custom_base_override_wins() {
        let toml_text = r#"
[providers.compat]
kind = "anthropic-api"
api_key_ref = "literal:sk-test"
base_url = "https://api.example.com/anthropic"
cache_capability = { supports_top_level_cache_control = true, cache_hit_observable = true }
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse custom base override");
        let entry = cfg.providers.get("compat").expect("compat provider");
        let cap = entry.cache_capability();
        assert!(
            cap.supports_top_level_cache_control,
            "explicit override must win over the fail-closed custom-base default"
        );
        assert!(cap.cache_hit_observable);
    }

    /// An omitted `[reduction]` block deserializes to the default:
    /// reduction disabled.
    #[test]
    fn reduction_omitted_block_defaults_disabled() {
        // Arrange: a config with no [reduction] table at all.
        let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
"#;

        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse without reduction block");

        // Assert: omitted block == default == disabled.
        assert!(
            !cfg.reduction.enabled,
            "omitted [reduction] must default to disabled"
        );
    }

    /// A `[reduction]` block with `enabled = true` parses, and an unknown
    /// field inside it is rejected (deny_unknown_fields, mirroring
    /// CacheConfig).
    #[test]
    fn reduction_block_parses_and_rejects_unknown_fields() {
        // Arrange + Act: explicit enable.
        let toml_text = r"
[reduction]
enabled = true
";
        let cfg: Config = toml::from_str(toml_text).expect("parse enabled reduction block");

        // Assert
        assert!(cfg.reduction.enabled, "enabled = true must parse to true");

        // Arrange: an unknown key inside [reduction].
        let bad = r"
[reduction]
enabled = true
bogus = 1
";

        // Act + Assert: deny_unknown_fields must reject it.
        assert!(
            toml::from_str::<Config>(bad).is_err(),
            "deny_unknown_fields must reject an unknown ReductionConfig field",
        );
    }

    /// `ReductionConfig::default()` yields disabled (reduction is opt-in).
    #[test]
    fn reduction_config_default_is_disabled() {
        // Arrange + Act
        let cfg = ReductionConfig::default();

        // Assert
        assert!(!cfg.enabled, "ReductionConfig::default() must be disabled");
    }

    /// The per-provider `reduction_enabled()` accessor returns `None` when
    /// the override is unset, and the configured `Option<bool>` when a
    /// TOML override is present (round-tripping through serialize).
    #[test]
    fn reduction_enabled_per_provider_accessor() {
        // Arrange: unset -> None.
        let unset = ProviderEntry::openai_compat("https://example.com/v1", "literal:k");

        // Act + Assert
        assert_eq!(
            unset.reduction_enabled(),
            None,
            "unset per-provider override must read as None"
        );

        // Arrange: an explicit per-provider override of false.
        let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
reduction_enabled = false
"#;

        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse override");
        let entry = cfg.providers.get("openai").expect("openai provider");

        // Assert: Some(false) reads back through the accessor.
        assert_eq!(
            entry.reduction_enabled(),
            Some(false),
            "explicit reduction_enabled = false must read as Some(false)"
        );

        // Round-trip: serialize, re-parse, accessor still Some(false).
        let serialized = toml::to_string(&cfg).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
        assert_eq!(
            cfg_out
                .providers
                .get("openai")
                .expect("openai")
                .reduction_enabled(),
            Some(false),
            "per-provider override must round-trip through serde"
        );
    }

    /// A missing `[trim]` block must resolve, via `to_params()`, to params
    /// byte-identical to `SteadyStateTrimParams::default()` -- the whole
    /// point of driving both the per-field serde defaults and the struct's
    /// own `Default` impl off the SAME consts in `context_trim.rs`.
    #[test]
    fn trim_omitted_block_matches_steady_state_trim_params_default() {
        // Arrange: a config with no [trim] table at all.
        let toml_text = r#"
[providers.openai]
kind = "openai-compat"
base_url = "https://example.com/v1"
api_key_ref = "literal:k"
"#;

        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse without trim block");
        let resolved = cfg.trim.to_params();

        // Assert: byte-identical to the trimmer's own Default.
        assert_eq!(
            resolved,
            crate::context_trim::SteadyStateTrimParams::default(),
            "missing [trim] must resolve to SteadyStateTrimParams::default()"
        );
    }

    /// A `[trim]` block with explicit knobs parses and resolves through
    /// `to_params()`; an unknown key inside it is rejected
    /// (deny_unknown_fields, mirroring `reduction_block_parses_and_rejects_unknown_fields`).
    #[test]
    fn trim_block_parses_and_rejects_unknown_fields() {
        // Arrange + Act: explicit knobs.
        let toml_text = r"
[trim]
trigger_tokens = 50000
clear_at_least_tokens = 10000
head_keep_messages = 1
keep_recent_messages = 3
";
        let cfg: Config = toml::from_str(toml_text).expect("parse explicit trim block");

        // Assert
        let params = cfg.trim.to_params();
        assert_eq!(params.trigger_tokens, 50_000);
        assert_eq!(params.clear_at_least_tokens, 10_000);
        assert_eq!(params.head_keep_messages, 1);
        assert_eq!(params.keep_recent_messages, 3);

        // Arrange: an unknown key inside [trim].
        let bad = r"
[trim]
trigger_tokens = 50000
bogus = 1
";

        // Act + Assert: deny_unknown_fields must reject it.
        assert!(
            toml::from_str::<Config>(bad).is_err(),
            "deny_unknown_fields must reject an unknown TrimConfig field",
        );
    }

    /// PARITY: the router recording path (`Router::record_would_trim`) and
    /// the prompt-size path (`prompt_size::build_steady_state_economics`)
    /// both resolve `SteadyStateTrimParams` via `TrimConfig::to_params()`.
    /// Neither is directly callable from here -- `record_would_trim` is
    /// module-private to `router.rs`, and `build_steady_state_economics`
    /// lives in `routectl-cli`, which depends on this crate (not the other
    /// way around). So this test drives the router's PUBLIC dispatch entry
    /// point end-to-end and reads the OBSERVABLE it stamps onto
    /// `DispatchMeta`, then cross-checks it against a local recomputation
    /// using the prompt-size path's exact two-call shape (`trim.to_params()`
    /// then `propose_steady_state_trim`). Calling `to_params()` twice in
    /// isolation can never fail -- it is a pure deterministic mapping -- so
    /// that alone would prove nothing; this version fails if
    /// `record_would_trim` ever stops resolving params via
    /// `self.config.trim.to_params()` (e.g. a revert to
    /// `SteadyStateTrimParams::default()`), because the custom trigger
    /// below is tuned to fire ONLY under the configured value, not the
    /// stock default.
    #[tokio::test]
    async fn trim_to_params_is_identical_across_both_consumers() {
        use crate::resolved::ResolvedModel;
        use crate::router::{Router, RouterOptions};
        use routectl_core::{
            ChatRequest, ChatResponse, Choice, ContentPart, KnownContentPart, Message,
            MessageContent, Result as CoreResult, Role,
        };
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct EchoProvider;

        #[async_trait::async_trait]
        impl routectl_core::Provider for EchoProvider {
            fn id(&self) -> &'static str {
                "echo"
            }

            fn normalize_request(&self, _req: &ChatRequest) -> CoreResult<serde_json::Value> {
                Ok(serde_json::json!({}))
            }

            fn normalize_response(&self, _raw: serde_json::Value) -> CoreResult<ChatResponse> {
                Err(routectl_core::Error::normalize_response("echo", "unused"))
            }

            async fn complete(&self, req: ChatRequest) -> CoreResult<ChatResponse> {
                Ok(ChatResponse {
                    id: "ok".into(),
                    model: req.model,
                    choices: vec![Choice {
                        index: 0,
                        message: Message {
                            role: Role::Assistant,
                            content: MessageContent::Text("ok".into()),
                            reasoning: None,
                            reasoning_details: vec![],
                            name: None,
                            tool_call_id: None,
                            tool_calls: None,
                            refusal: None,
                        },
                        finish_reason: Some("stop".into()),
                        matched_stop_sequence: None,
                        logprobs: None,
                    }],
                    usage: Some(routectl_core::Usage::default()),
                    ..Default::default()
                })
            }

            async fn stream(
                &self,
                _req: ChatRequest,
            ) -> CoreResult<futures::stream::BoxStream<'static, CoreResult<routectl_core::ChatChunk>>>
            {
                use futures::stream::StreamExt;
                Ok(futures::stream::empty().boxed())
            }
        }

        fn text_msg(role: Role, text: &str) -> Message {
            Message {
                role,
                content: MessageContent::Text(text.into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            }
        }

        // A request sized to cross a LOW custom trigger but stay well
        // under the stock default trigger (100_000 tokens): a reversion to
        // the default in either consumer flips the observable outcome from
        // Some to None.
        fn parity_request() -> ChatRequest {
            let payload = "x".repeat(400);
            ChatRequest {
                model: "m".into(),
                messages: vec![
                    text_msg(Role::User, "head turn"),
                    Message {
                        role: Role::User,
                        content: MessageContent::Parts(vec![ContentPart::Known(
                            KnownContentPart::ToolResult {
                                tool_use_id: "toolu_1".into(),
                                content: serde_json::json!(payload),
                                is_error: None,
                                cache_control: None,
                            },
                        )]),
                        reasoning: None,
                        reasoning_details: vec![],
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                        refusal: None,
                    },
                    text_msg(Role::User, "recent turn"),
                ],
                ..Default::default()
            }
        }

        // Arrange: one Config with an explicit, non-default [trim] block.
        let toml_text = r"
[trim]
trigger_tokens = 50
clear_at_least_tokens = 20
head_keep_messages = 1
keep_recent_messages = 1
";
        let cfg: Config = toml::from_str(toml_text).expect("parse trim block");
        let params = cfg.trim.to_params();
        assert_ne!(
            params,
            crate::context_trim::SteadyStateTrimParams::default(),
            "sanity: the explicit block must differ from the stock default"
        );

        // Act: drive the REAL router recording path via the public
        // dispatch entry point (`record_would_trim` is module-private).
        let mut router = Router::new(Arc::new(cfg));
        let provider: Arc<dyn routectl_core::Provider> = Arc::new(EchoProvider);
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            "m".to_string(),
            Arc::new(ResolvedModel::new("m", "p", provider, "upstream-m")),
        );
        router.install_resolved_models(models);
        let dispatched = router
            .complete_with_options(parity_request(), RouterOptions::new())
            .await;
        dispatched.result.expect("dispatch succeeds");
        let router_observed_d = dispatched
            .meta
            .would_trim_tokens
            .expect("router path must propose a trim for this request under the custom trim block");

        // Act: recompute the prompt-size path's exact call shape --
        // `trim.to_params()` then `propose_steady_state_trim` -- since
        // `build_steady_state_economics` itself lives in a crate that
        // depends on this one and is not callable from here.
        let prompt_size_plan =
            crate::context_trim::propose_steady_state_trim(&parity_request(), &params).expect(
                "prompt-size path must propose a trim for this request under the custom trim block",
            );

        // Assert: both paths agree on the freed-token count, proving they
        // resolved the SAME params from the SAME Config. A reverted
        // consumer would either fail the `.expect(...)` calls above (no
        // trim proposed under the stock default) or, if it transformed the
        // params instead of reverting them outright, land here with a
        // mismatched `d`.
        assert_eq!(
            router_observed_d, prompt_size_plan.candidate.d,
            "router and prompt-size paths must resolve identical SteadyStateTrimParams from the same Config"
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
        let toml_text = r"
[log]
redact_prompts = true
";
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
        let toml_text = r"
[log]
trace_headers = true
trace_body_bytes = 32768
redact_prompts = true
";
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
        let toml_text = r"
[log]
trace_body_byte = 1024
";
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

    /// A `[providers.X.cloak]` block with mode + strict_mode + tool_rename
    /// (array of tables) + sensitive_words parses into the entry's
    /// `CloakConfig`, and round-trips through serialize + re-parse.
    #[test]
    fn anthropic_api_cloak_block_parses_and_round_trips() {
        use crate::config::ProviderEntry;
        use routectl_providers::anthropic_api::CloakMode;
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[providers.anthropic.cloak]
mode = "always"
strict_mode = true
sensitive_words = ["secret", "token"]

[[providers.anthropic.cloak.tool_rename]]
from = "foo"
to = "bar"

[[providers.anthropic.cloak.tool_rename]]
from = "baz"
to = "qux"
"#;
        let cfg_in: Config = toml::from_str(toml_text).expect("parse cloak block");
        let assert_cloak = |entry: &ProviderEntry| match entry {
            ProviderEntry::AnthropicApi { cloak, .. } => {
                assert_eq!(cloak.mode, CloakMode::Always);
                assert!(cloak.strict_mode);
                assert_eq!(cloak.sensitive_words, vec!["secret", "token"]);
                assert_eq!(cloak.tool_rename.len(), 2);
                assert_eq!(cloak.tool_rename[0].from, "foo");
                assert_eq!(cloak.tool_rename[0].to, "bar");
                assert_eq!(cloak.tool_rename[1].from, "baz");
                assert_eq!(cloak.tool_rename[1].to, "qux");
            }
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        };
        assert_cloak(cfg_in.providers.get("anthropic").expect("anthropic"));

        // Serialize + re-parse: the cloak surface must survive the round-trip.
        let serialized = toml::to_string(&cfg_in).expect("serialize");
        let cfg_out: Config = toml::from_str(&serialized).expect("re-parse");
        assert_cloak(cfg_out.providers.get("anthropic").expect("anthropic"));
    }

    /// Omitting the `[cloak]` block yields `CloakConfig::default()` (mode
    /// auto, no strict mode, empty tool_rename + sensitive_words).
    #[test]
    fn anthropic_api_cloak_omitted_yields_default() {
        use crate::config::ProviderEntry;
        use routectl_providers::anthropic_api::CloakMode;

        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        let cfg_in: Config = toml::from_str(toml_text).expect("parse without cloak");
        match cfg_in.providers.get("anthropic").expect("anthropic") {
            ProviderEntry::AnthropicApi { cloak, .. } => {
                assert_eq!(cloak.mode, CloakMode::Auto);
                assert!(!cloak.strict_mode);
                assert!(cloak.tool_rename.is_empty());
                assert!(cloak.sensitive_words.is_empty());
            }
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
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

    /// Raw-status escape hatch: when an operator has explicitly named a
    /// status in `retry_allowlist` or `retry_denylist`, that naming wins
    /// over whatever a failure-class policy (429 vs. 5xx vs. network)
    /// would otherwise decide for the same code. Centralizing the
    /// precedence check here means every future consumer that layers
    /// class-level retry policy on top of this one calls this method
    /// first and only falls back to class policy on `None` -- so the
    /// "an explicit list entry beats the class default" rule is encoded
    /// exactly once instead of re-derived at each call site.
    ///
    /// Returns:
    ///   - `Some(true)` -- `retry_allowlist` is non-empty and contains
    ///     `status`.
    ///   - `Some(false)` -- `retry_allowlist` is non-empty but does not
    ///     contain `status` (an allowlist that doesn't name a code is an
    ///     explicit exclude for it), OR `retry_allowlist` is empty and
    ///     `retry_denylist` is `Some` and contains `status`.
    ///   - `None` -- neither list applies to `status`, i.e. no explicit
    ///     override exists and the caller should fall through to class
    ///     policy.
    pub fn explicit_status_override(&self, status: u16) -> Option<bool> {
        if !self.retry_allowlist.is_empty() {
            return Some(self.retry_allowlist.contains(&status));
        }
        if let Some(denylist) = &self.retry_denylist {
            return if denylist.contains(&status) {
                Some(false)
            } else {
                None
            };
        }
        None
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

    /// Effective ceiling on an honored upstream reset hint. When
    /// `max_honored_retry_after_ms` is `Some(n)`, returns
    /// `Duration::from_millis(n)`; when `None`, returns the
    /// `DEFAULT_MAX_HONORED_RETRY_AFTER_MS` (1 hour) baseline. The
    /// circuit breaker clamps a parsed `Retry-After` to this ceiling
    /// before parking a provider.
    pub fn max_honored_retry_after(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.max_honored_retry_after_ms
                .unwrap_or(DEFAULT_MAX_HONORED_RETRY_AFTER_MS),
        )
    }
}

const fn default_max_attempts() -> u32 {
    2
}

/// Default ceiling on an honored upstream reset hint when
/// `RetryPolicy::max_honored_retry_after_ms` is unset: one hour. Caps
/// how long the circuit breaker will park a provider on a `Retry-After`
/// so a single hint cannot pin a provider out of rotation indefinitely.
const DEFAULT_MAX_HONORED_RETRY_AFTER_MS: u64 = 3_600_000;

const fn default_probe_max_tokens() -> u32 {
    1
}

/// Backstop for `RetryPolicy::stream_first_byte_timeout_ms` when a
/// `[retry]` block is present but omits this key. Serde's bare
/// `#[serde(default)]` would otherwise fill `Option::<u64>::default()`
/// (`None`), reintroducing the unbounded pinging-but-contentless hang
/// this field's `Some(600000)` default exists to prevent -- the struct
/// `Default` impl only applies when the whole `[retry]` table is
/// absent, not when individual keys within it are.
const fn default_stream_first_byte_timeout_ms() -> Option<u64> {
    Some(600_000)
}

const fn default_backoff_ms() -> u64 {
    250
}

const fn default_backoff_multiplier() -> f64 {
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

    /// 400-fallbackability under the SHIPPED Default impl is load-bearing for
    /// structured-output rescue on Bedrock: a fallback triggered by a 400
    /// carries the request to an alternate provider. A future Default that
    /// ships a denylist containing 400 would silently break SO rescue.
    /// This test pins the actual Default impl, not a hand-zeroed policy.
    #[test]
    fn default_policy_400_is_fallbackable() {
        assert!(
            RetryPolicy::default().is_fallbackable_status(400),
            "default RetryPolicy must treat 400 as fallbackable (load-bearing for SO rescue)"
        );
        // Companion: a policy with 400 in the denylist must yield false,
        // documenting the break mode so an operator who reaches for
        // retry_denylist = [400] understands the consequence.
        let blocking = RetryPolicy {
            retry_denylist: Some(vec![400]),
            ..Default::default()
        };
        assert!(
            !blocking.is_fallbackable_status(400),
            "a denylist containing 400 must block 400-fallback (breaks SO rescue)"
        );
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

        let toml_text = r"
[retry]
retry_denylist = [422]
";
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
        let toml_text = r"
[retry]
max_attempts = 3
";
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.retry.probe_max_tokens, 1);
        assert_eq!(cfg.retry.max_attempts, 3, "other fields unaffected");
    }

    #[test]
    fn probe_max_tokens_zero_parses_to_disable() {
        // `probe_max_tokens = 0` is the disable sentinel and round-trips.
        use crate::config::Config;
        let toml_text = r"
[retry]
probe_max_tokens = 0
";
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.retry.probe_max_tokens, 0);
    }

    #[test]
    fn default_retry_policy_has_probe_max_tokens_one() {
        // The Default impl (no `[retry]` block at all) also yields 1.
        assert_eq!(RetryPolicy::default().probe_max_tokens, 1);
    }

    /// The code default for `stream_first_byte_timeout_ms` is `Some`,
    /// not `None` -- a pinging-but-contentless upstream must have a
    /// bound even when the operator sets no `[retry]` block at all.
    #[test]
    fn default_retry_policy_has_stream_first_byte_timeout_backstop() {
        assert_eq!(
            RetryPolicy::default().stream_first_byte_timeout_ms,
            Some(600_000),
            "default must be Some(600000) as a total-silence backstop"
        );
    }

    /// An operator that sets `stream_first_byte_timeout_ms` explicitly
    /// must get exactly that value back, unaffected by the new default.
    #[test]
    fn stream_first_byte_timeout_ms_explicit_override_round_trips_unchanged() {
        use crate::config::Config;
        let toml_text = r"
[retry]
stream_first_byte_timeout_ms = 120000
";
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.retry.stream_first_byte_timeout_ms, Some(120_000));
    }

    /// A `[retry]` block that sets some OTHER field but omits
    /// `stream_first_byte_timeout_ms` must still get the `Some(600000)`
    /// backstop, not `None`. This is the case the struct-level
    /// `Default` impl does NOT cover, since that only applies when the
    /// whole `[retry]` table is absent.
    #[test]
    fn stream_first_byte_timeout_ms_defaults_to_backstop_when_omitted() {
        use crate::config::Config;
        let toml_text = r"
[retry]
max_attempts = 5
";
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(
            cfg.retry.stream_first_byte_timeout_ms,
            Some(600_000),
            "omitting the key inside a present [retry] block must not lose the backstop"
        );
        assert_eq!(cfg.retry.max_attempts, 5, "other fields unaffected");
    }

    /// A `[retry]` block omitting `max_honored_retry_after_ms` resolves
    /// to the documented 1h default via the getter.
    #[test]
    fn max_honored_retry_after_defaults_to_one_hour_when_omitted() {
        use crate::config::Config;
        use std::time::Duration;

        let toml_text = r"
[retry]
max_attempts = 3
";
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert!(
            cfg.retry.max_honored_retry_after_ms.is_none(),
            "field must default to None when omitted"
        );
        assert_eq!(
            cfg.retry.max_honored_retry_after(),
            Duration::from_hours(1),
            "None must resolve to the 1h ceiling"
        );
    }

    /// An explicit `max_honored_retry_after_ms` parses and the getter
    /// returns the configured duration.
    #[test]
    fn max_honored_retry_after_uses_configured_value() {
        use crate::config::Config;
        use std::time::Duration;

        let toml_text = r"
[retry]
max_honored_retry_after_ms = 90000
";
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        assert_eq!(cfg.retry.max_honored_retry_after_ms, Some(90_000));
        assert_eq!(
            cfg.retry.max_honored_retry_after(),
            Duration::from_secs(90),
            "Some(90000) must resolve to 90s"
        );
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
        let toml_text = r"
[server]
max_body_bytes = 67108864
";
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

    #[test]
    fn server_rejects_auths_typo_for_auth_block() {
        // `[server.auths]` (typo for `[server.auth]`) must be rejected.
        // Without deny_unknown_fields it parsed fine and left auth
        // disabled -- a silent auth-disable footgun.
        use crate::config::Config;
        let toml_text = r#"
[server]
host = "127.0.0.1"

[server.auths]
tokens = ["literal:abc"]
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("auths") || msg.contains("unknown field"),
            "expected unknown-field error naming `auths`; got: {msg}"
        );
    }

    #[test]
    fn server_auth_rejects_token_typo_for_tokens() {
        // `token` (singular, typo for `tokens`) under `[server.auth]`
        // must be rejected so a misspelled key cannot silently leave
        // the listener unauthenticated.
        use crate::config::Config;
        let toml_text = r#"
[server.auth]
token = ["literal:abc"]
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("token") || msg.contains("unknown field"),
            "expected unknown-field error naming the unknown key; got: {msg}"
        );
    }

    #[test]
    fn server_auth_tokens_round_trips() {
        // A valid `[server.auth]` with the correct `tokens` key still
        // deserializes after deny_unknown_fields is added.
        use crate::config::Config;
        let toml_text = r#"
[server.auth]
tokens = ["literal:abc", "env://TOK"]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("valid auth block parses");
        let auth = cfg.server.auth.expect("auth present");
        assert_eq!(auth.tokens, vec!["literal:abc", "env://TOK"]);
    }
}

#[cfg(test)]
mod explicit_status_override_tests {
    //! Cover every branch of `RetryPolicy::explicit_status_override`.
    use super::RetryPolicy;

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    #[test]
    fn allowlist_hit_returns_some_true() {
        let mut p = policy();
        p.retry_allowlist = vec![503];
        p.retry_denylist = None;
        assert_eq!(p.explicit_status_override(503), Some(true));
    }

    #[test]
    fn allowlist_set_but_miss_returns_some_false() {
        let mut p = policy();
        p.retry_allowlist = vec![503];
        p.retry_denylist = None;
        // 500 is a 5xx but not named in the allowlist -- an
        // allowlist-set-but-miss is an explicit exclude, not "defer to
        // class policy".
        assert_eq!(p.explicit_status_override(500), Some(false));
    }

    #[test]
    fn denylist_hit_returns_some_false() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = Some(vec![501]);
        assert_eq!(p.explicit_status_override(501), Some(false));
    }

    #[test]
    fn denylist_miss_returns_none() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = Some(vec![501]);
        // 503 is not named in the denylist -- no explicit override,
        // defer to class policy.
        assert_eq!(p.explicit_status_override(503), None);
    }

    #[test]
    fn neither_list_set_returns_none() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = None;
        assert_eq!(p.explicit_status_override(503), None);
    }

    #[test]
    fn allowlist_hit_outside_error_range_returns_some_true() {
        // The override is a pure list-membership check with no range
        // gating (unlike `is_fallbackable_status`), so a status outside
        // 400..=599 still resolves via allowlist/denylist membership.
        let mut p = policy();
        p.retry_allowlist = vec![200];
        p.retry_denylist = None;
        assert_eq!(p.explicit_status_override(200), Some(true));
    }

    #[test]
    fn allowlist_miss_outside_error_range_returns_some_false() {
        let mut p = policy();
        p.retry_allowlist = vec![200];
        p.retry_denylist = None;
        assert_eq!(p.explicit_status_override(201), Some(false));
    }

    #[test]
    fn denylist_hit_outside_error_range_returns_some_false() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = Some(vec![200]);
        assert_eq!(p.explicit_status_override(200), Some(false));
    }

    #[test]
    fn denylist_miss_outside_error_range_returns_none() {
        let mut p = policy();
        p.retry_allowlist = vec![];
        p.retry_denylist = Some(vec![200]);
        assert_eq!(p.explicit_status_override(201), None);
    }

    #[test]
    fn neither_list_set_outside_error_range_returns_none() {
        let neither = policy();
        assert_eq!(neither.explicit_status_override(200), None);
    }
}

#[cfg(test)]
mod mitm_config_tests {
    //! `[mitm]` schema round-trip: absence leaves the feature off,
    //! presence fills in every documented default, explicit values
    //! survive serde untouched, and an unknown key -- including the
    //! removed `credential_source` -- rejects at parse time (same
    //! `deny_unknown_fields` footgun-closing convention as
    //! `[server.auth]`). The actionable-error path for the legacy key
    //! specifically is pinned in
    //! `legacy_mitm_credential_source_preflight_tests` above.

    use crate::config::{Config, MitmConfig};

    #[test]
    fn mitm_absent_leaves_config_none() {
        let cfg: Config = toml::from_str("").expect("parse empty config");
        assert!(cfg.mitm.is_none(), "mitm must default to None when absent");
    }

    #[test]
    fn mitm_present_with_all_fields_omitted_uses_defaults() {
        let toml_text = "[mitm]\n";
        let cfg: Config = toml::from_str(toml_text).expect("parse bare [mitm] block");
        let mitm = cfg.mitm.expect("mitm present once the block is declared");
        assert_eq!(mitm.upstream_origin, "https://api.anthropic.com");
        assert_eq!(mitm.listen_port, 8443);
        assert_eq!(mitm.mitm_host, "api.anthropic.com");
        assert!(mitm.tested_cc_version.is_none());
        assert!(
            mitm.cert_dir.ends_with("routectl/mitm-certs"),
            "cert_dir: {:?}",
            mitm.cert_dir
        );
        assert_eq!(mitm, MitmConfig::default());
    }

    #[test]
    fn mitm_explicit_values_round_trip() {
        let toml_text = r#"
[mitm]
upstream_origin = "https://api.example.com"
listen_port = 9443
cert_dir = "/tmp/routectl-mitm-certs"
mitm_host = "api.example.com"
tested_cc_version = "2.1.143"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse explicit [mitm]");
        let mitm = cfg.mitm.expect("mitm present");
        assert_eq!(mitm.upstream_origin, "https://api.example.com");
        assert_eq!(mitm.listen_port, 9443);
        assert_eq!(
            mitm.cert_dir,
            std::path::PathBuf::from("/tmp/routectl-mitm-certs")
        );
        assert_eq!(mitm.mitm_host, "api.example.com");
        assert_eq!(mitm.tested_cc_version, Some("2.1.143".to_string()));
    }

    #[test]
    fn mitm_rejects_unknown_field() {
        let toml_text = r#"
[mitm]
upstream_origin = "https://api.anthropic.com"
listen_prot = 9443
"#;
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("listen_prot") || msg.contains("unknown field"),
            "expected unknown-field error naming the typo; got: {msg}"
        );
    }

    /// The removed `credential_source` key is exactly as unrepresentable
    /// on `[mitm]` as any other unknown field -- `deny_unknown_fields`
    /// rejects the typed deserialize regardless of the value. The
    /// actionable replacement text lives in the preflight check
    /// (`legacy_mitm_credential_source_preflight_tests`), not here.
    #[test]
    fn mitm_rejects_legacy_credential_source_field() {
        let toml_text = "[mitm]\ncredential_source = \"forwarded\"\n";
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("the removed credential_source key must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("credential_source") || msg.contains("unknown field"),
            "expected unknown-field error naming credential_source; got: {msg}"
        );
    }

    /// Acceptance: a transport-only `[mitm]` block -- the f1 shape, no
    /// credential knob -- still validates cleanly via the full config
    /// boundary (not a bare-struct construction).
    #[test]
    fn mitm_transport_only_block_still_validates() {
        let toml_text = r#"
[mitm]
upstream_origin = "https://api.anthropic.com"
listen_port = 8443
mitm_host = "api.anthropic.com"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("transport-only [mitm] must parse");
        assert!(cfg.mitm.is_some());
    }
}

#[cfg(test)]
mod provider_credential_source_schema_tests {
    //! Config-BOUNDARY (parse-via-`toml`) schema tests for
    //! `ProviderEntry::AnthropicApi.credential_source`: field-coherence
    //! (host pin, `api_key_ref` matrix) lives in
    //! `factory::validate_provider_credential_sources_tests` -- this
    //! module pins only the serde SHAPE: default value, round-trip, and
    //! that `deny_unknown_fields` makes the field unrepresentable on
    //! every other `[providers.X]` kind.

    use crate::config::{Config, CredentialSource, ProviderEntry};

    #[test]
    fn anthropic_api_credential_source_defaults_to_own_when_omitted() {
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse default");
        match cfg.providers.get("anthropic").expect("anthropic provider") {
            ProviderEntry::AnthropicApi {
                credential_source, ..
            } => assert_eq!(*credential_source, CredentialSource::Own),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// The 4-line forwarded block from the docs/spec: no `api_key_ref`
    /// line at all, `credential_source = "forwarded"`. Must parse
    /// cleanly -- `api_key_ref` is `#[serde(default)]` precisely so this
    /// shape is representable.
    #[test]
    fn anthropic_api_forwarded_block_with_no_api_key_ref_parses() {
        let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("forwarded block must parse");
        match cfg
            .providers
            .get("anthropic-forwarded")
            .expect("anthropic-forwarded provider")
        {
            ProviderEntry::AnthropicApi {
                credential_source,
                api_key_ref,
                ..
            } => {
                assert_eq!(*credential_source, CredentialSource::Forwarded);
                assert!(api_key_ref.is_empty(), "got: {api_key_ref:?}");
            }
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    /// An empty `api_key_ref` (the forwarded shape) must NOT surface as a
    /// secret URI to resolve -- `commands::config::check` iterates every
    /// `secret_uris()` entry through `SecretRef::parse`, which would
    /// reject an empty string as an unrecognized scheme and fail an
    /// otherwise-clean forwarded provider.
    #[test]
    fn forwarded_entry_reports_no_secret_uris() {
        let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let entry = cfg.providers.get("anthropic-forwarded").unwrap();
        assert!(
            entry.secret_uris().is_empty(),
            "got: {:?}",
            entry.secret_uris()
        );
    }

    #[test]
    fn anthropic_api_credential_source_round_trips_through_toml() {
        let toml_text = r#"
[providers.anthropic-forwarded]
kind = "anthropic-api"
base_url = "https://api.anthropic.com"
credential_source = "forwarded"
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        let reserialized = toml::to_string(&cfg).expect("re-serialize");
        let cfg2: Config = toml::from_str(&reserialized).expect("re-parse");
        match cfg2.providers.get("anthropic-forwarded").unwrap() {
            ProviderEntry::AnthropicApi {
                credential_source, ..
            } => assert_eq!(*credential_source, CredentialSource::Forwarded),
            other => panic!("expected AnthropicApi entry; got {other:?}"),
        }
    }

    #[test]
    fn anthropic_api_rejects_unknown_credential_source_value() {
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
credential_source = "borrowed"
"#;
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("unknown credential_source value must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("borrowed") || msg.contains("unknown variant") || msg.contains("expected"),
            "expected an unknown-variant parse error; got: {msg}"
        );
    }

    /// `deny_unknown_fields` at the enum level makes `credential_source`
    /// unrepresentable on the `openai-compat` kind -- the field lives
    /// ONLY on the `AnthropicApi` variant.
    #[test]
    fn credential_source_is_rejected_on_openai_compat() {
        let toml_text = r#"
[providers.example]
kind = "openai-compat"
base_url = "https://api.openai.com"
api_key_ref = "env://OPENAI_API_KEY"
credential_source = "forwarded"
"#;
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("credential_source must not parse on openai-compat");
        let msg = err.to_string();
        assert!(
            msg.contains("credential_source") || msg.contains("unknown field"),
            "expected unknown-field error naming credential_source; got: {msg}"
        );
    }

    /// Same guarantee as `credential_source_is_rejected_on_openai_compat`,
    /// pinned separately for the `openai-responses` kind -- the task's
    /// acceptance criteria name both kinds explicitly.
    #[cfg(feature = "openai-responses")]
    #[test]
    fn credential_source_is_rejected_on_openai_responses() {
        let toml_text = r#"
[providers.example]
kind = "openai-responses"
api_key_ref = "literal:test-jwt"
auth_kind = "api-key"
credential_source = "forwarded"
"#;
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("credential_source must not parse on openai-responses");
        let msg = err.to_string();
        assert!(
            msg.contains("credential_source") || msg.contains("unknown field"),
            "expected unknown-field error naming credential_source; got: {msg}"
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

#[cfg(test)]
mod seat_selection_tests {
    //! Pin the `seat_selection` per-provider knob: a default, an
    //! explicit `round-robin`, and a rejected unknown value. The field
    //! flattens off `ProviderRuntimePolicy` onto every `[providers.X]`.
    use crate::config::{Config, SeatSelection};

    fn runtime_of<'a>(cfg: &'a Config, name: &str) -> &'a super::ProviderRuntimePolicy {
        cfg.providers.get(name).expect("provider").runtime()
    }

    #[test]
    fn seat_selection_defaults_to_fill_first() {
        // Arrange: a provider entry omitting seat_selection.
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        // Assert
        assert_eq!(
            runtime_of(&cfg, "anthropic").seat_selection,
            SeatSelection::FillFirst
        );
    }

    #[test]
    fn seat_selection_parses_round_robin() {
        // Arrange
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
seat_selection = "round-robin"
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse");
        // Assert
        assert_eq!(
            runtime_of(&cfg, "anthropic").seat_selection,
            SeatSelection::RoundRobin
        );
    }

    #[test]
    fn seat_selection_rejects_unknown_value() {
        // Arrange
        let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
seat_selection = "bogus"
"#;
        // Act
        let result = toml::from_str::<Config>(toml_text);
        // Assert: an unknown variant is a clean deserialize Err.
        assert!(
            result.is_err(),
            "unknown seat_selection value must reject; got Ok"
        );
    }

    /// `ProviderRuntimePolicy::default()` carries `FillFirst`, so a
    /// programmatically-built provider matches the TOML-omitted default.
    #[test]
    fn provider_runtime_policy_default_is_fill_first() {
        assert_eq!(
            super::ProviderRuntimePolicy::default().seat_selection,
            SeatSelection::FillFirst
        );
    }

    /// A config with no `[usage]` block deserializes to the documented
    /// defaults: enabled, 90-day retention, and a db under the resolved
    /// user config dir with no literal `~` left in the path.
    #[test]
    fn usage_block_absent_yields_defaults() {
        // Arrange: a config that mentions usage nowhere.
        let toml_text = r#"
[server]
host = "127.0.0.1"
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse without usage block");

        // Assert
        assert!(cfg.usage.enabled, "enabled must default true");
        assert_eq!(cfg.usage.retention_days, 90);
        let db = cfg.usage.db_path.to_string_lossy();
        assert!(
            db.ends_with("routectl/usage.db"),
            "db_path must end with routectl/usage.db; got {db}"
        );
        assert!(
            !db.contains('~'),
            "no literal ~ may reach the path; got {db}"
        );
    }

    /// Explicit `[usage]` values override every default.
    #[test]
    fn usage_block_explicit_overrides_defaults() {
        // Arrange
        let toml_text = r#"
[usage]
enabled = false
db_path = "/var/lib/routectl/usage.db"
retention_days = 7
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse explicit usage block");

        // Assert
        assert!(!cfg.usage.enabled);
        assert_eq!(
            cfg.usage.db_path,
            std::path::PathBuf::from("/var/lib/routectl/usage.db")
        );
        assert_eq!(cfg.usage.retention_days, 7);
    }

    /// `deny_unknown_fields` rejects a typo'd key inside `[usage]`.
    #[test]
    fn usage_block_rejects_unknown_field() {
        // Arrange
        let toml_text = r"
[usage]
enabled = true
bogus_key = 1
";
        // Act
        let result = toml::from_str::<Config>(toml_text);
        // Assert
        assert!(result.is_err(), "unknown [usage] key must reject; got Ok");
    }
}

#[cfg(test)]
mod registry_tests {
    //! Tests for the `[registry.*]` pricing table: parsing, the
    //! `deny_unknown_fields` guard inside `[pricing]`, and the
    //! `Config::pricing_for` glob resolver.

    use super::Config;

    #[test]
    fn registry_pricing_block_parses() {
        // Arrange
        let toml_text = r#"
[registry."deepseek-*"]

[registry."deepseek-*".pricing]
input_per_mtok = 0.27
output_per_mtok = 1.1
cache_read_per_mtok = 0.07
cache_write_5m_per_mtok = 0.5
cache_write_1h_per_mtok = 0.9
"#;
        // Act
        let cfg: Config = toml::from_str(toml_text).expect("parse registry block");

        // Assert
        let entry = cfg.registry.get("deepseek-*").expect("entry present");
        let pricing = entry.pricing.as_ref().expect("pricing present");
        assert_eq!(pricing.input_per_mtok, Some(0.27));
        assert_eq!(pricing.output_per_mtok, Some(1.1));
        assert_eq!(pricing.cache_read_per_mtok, Some(0.07));
        assert_eq!(pricing.cache_write_5m_per_mtok, Some(0.5));
        assert_eq!(pricing.cache_write_1h_per_mtok, Some(0.9));
        assert!(entry.provider.is_none());
    }

    #[test]
    fn registry_pricing_rejects_unknown_field() {
        // Arrange: typo'd `inputs_per_mtok` inside [pricing].
        let toml_text = r#"
[registry."deepseek-*".pricing]
inputs_per_mtok = 0.27
"#;
        // Act
        let result = toml::from_str::<Config>(toml_text);

        // Assert
        assert!(
            result.is_err(),
            "unknown [registry.*.pricing] key must reject; got Ok"
        );
    }

    fn priced(input: f64) -> super::PricingConfig {
        super::PricingConfig {
            input_per_mtok: Some(input),
            ..super::PricingConfig::default()
        }
    }

    fn config_with_registry(entries: Vec<(&str, Option<&str>, super::PricingConfig)>) -> Config {
        let mut cfg = Config::default();
        for (key, provider, pricing) in entries {
            cfg.registry.insert(
                key.to_string(),
                super::RegistryEntry {
                    pricing: Some(pricing),
                    provider: provider.map(str::to_string),
                },
            );
        }
        cfg
    }

    #[test]
    fn pricing_for_exact_beats_prefix() {
        // Arrange
        let cfg = config_with_registry(vec![
            ("deepseek-*", None, priced(1.0)),
            ("deepseek-chat", None, priced(2.0)),
        ]);

        // Act
        let pricing = cfg.pricing_for("deepseek-chat", "any").expect("match");

        // Assert: the exact key wins over the prefix.
        assert_eq!(pricing.input_per_mtok, Some(2.0));
    }

    /// Equal-length Exact-vs-Prefix tie: key `"deepseek*"` parses to a
    /// Prefix with stored prefix "deepseek" (len 8) and key `"deepseek"`
    /// parses to an Exact (len 8). Both match upstream "deepseek" with an
    /// IDENTICAL prefix_len, so scope and length cannot break the tie --
    /// the Exact entry must win on the is_exact tie-break.
    #[test]
    fn pricing_for_exact_beats_equal_length_prefix() {
        // Arrange
        let cfg = config_with_registry(vec![
            ("deepseek*", None, priced(1.0)),
            ("deepseek", None, priced(2.0)),
        ]);

        // Act
        let pricing = cfg.pricing_for("deepseek", "any").expect("match");

        // Assert: the exact entry wins the equal-length tie.
        assert_eq!(pricing.input_per_mtok, Some(2.0));
    }

    #[test]
    fn pricing_for_longer_prefix_beats_shorter() {
        // Arrange
        let cfg = config_with_registry(vec![
            ("deep*", None, priced(1.0)),
            ("deepseek-*", None, priced(2.0)),
        ]);

        // Act
        let pricing = cfg.pricing_for("deepseek-chat", "any").expect("match");

        // Assert
        assert_eq!(pricing.input_per_mtok, Some(2.0));
    }

    #[test]
    fn pricing_for_provider_scoped_preferred_over_agnostic() {
        // Arrange: two entries match the same upstream -- one agnostic,
        // one scoped to `vendor-a`. They use distinct glob keys (the
        // table is keyed by pattern string, so a same-pattern collision
        // would dedupe; provider scoping rides on distinct keys).
        let cfg = config_with_registry(vec![
            ("deepseek-*", None, priced(1.0)),
            ("deepseek-c*", Some("vendor-a"), priced(2.0)),
        ]);

        // Act + Assert: matching provider gets the scoped price even
        // though it is the SHORTER-matching... here the scoped key is
        // longer, but scope is the primary key so verify scope wins by
        // making the agnostic key at least as long.
        let scoped = cfg
            .pricing_for("deepseek-chat", "vendor-a")
            .expect("scoped match");
        assert_eq!(scoped.input_per_mtok, Some(2.0));

        // A different provider falls back to the agnostic entry; the
        // entry scoped to vendor-a is NOT eligible for vendor-b.
        let agnostic = cfg
            .pricing_for("deepseek-chat", "vendor-b")
            .expect("agnostic match");
        assert_eq!(agnostic.input_per_mtok, Some(1.0));
    }

    #[test]
    fn pricing_for_scope_beats_longer_agnostic_prefix() {
        // Arrange: the agnostic entry has the LONGER prefix; scope must
        // still win because provider-scope is the primary sort key.
        let cfg = config_with_registry(vec![
            ("deepseek-chat-v3", None, priced(1.0)),
            ("deepseek-*", Some("vendor-a"), priced(2.0)),
        ]);

        // Act
        let scoped = cfg
            .pricing_for("deepseek-chat-v3", "vendor-a")
            .expect("scoped match");

        // Assert: scope beats the longer agnostic prefix.
        assert_eq!(scoped.input_per_mtok, Some(2.0));
    }

    #[test]
    fn pricing_for_no_match_returns_none() {
        // Arrange
        let cfg = config_with_registry(vec![("deepseek-*", None, priced(1.0))]);

        // Act
        let result = cfg.pricing_for("gpt-4o", "any");

        // Assert
        assert!(result.is_none(), "no glob matches => None");
    }
}
