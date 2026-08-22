//! Config value types.

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

#[cfg(test)]
use super::Config;

/// Operator-facing `[cache]` config block. Global policy for the
/// dispatch-path auto-cache feature. A missing `[cache]` table
/// deserializes to `CacheConfig::default()` (auto-emit enabled), and the
/// per-field `#[serde(default)]` gives an omitted key that same default --
/// on for the two emission/normalization switches, off for
/// `k_gated_emission`.
///
/// This is the GLOBAL kill-switch; each `[providers.X]` entry carries an
/// optional `auto_emit_top_level_breakpoint` override consulted only when
/// the global switch is on. The effective decision is "global on AND
/// provider not explicitly off".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CacheConfig {
    /// Master switch for dispatch-path auto-emission of a top-level
    /// `cache_control` ephemeral_5m breakpoint. Default on.
    #[serde(default = "default_true")]
    pub auto_emit_top_level_breakpoint: bool,
    /// Master switch for the anthropic-cloak tool-array canonicalization: an
    /// all-or-nothing stable sort of `tools[]` by name on the non-CC egress,
    /// so a client that shuffles tool order request-to-request presents a
    /// stable cache prefix. Default on. A two-way door: turning it off
    /// restores verbatim tool order.
    #[serde(default = "default_true")]
    pub normalize_tools: bool,
    /// Master switch for withholding auto-emitted cache breakpoints on a
    /// session whose measured per-turn reuse sits below the marker's
    /// break-even point. Default OFF: K windows recorded before front-marker
    /// emission shipped describe a world with less caching, so acting on them
    /// would suppress emission on evidence that predates the caching, and a
    /// wrong suppression costs money at HTTP 200 with no error signal.
    #[serde(default)]
    pub k_gated_emission: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            auto_emit_top_level_breakpoint: true,
            normalize_tools: true,
            k_gated_emission: false,
        }
    }
}

/// Operator-facing `[reduction]` config block. Global policy for the
/// dispatch-path token-reduction feature. A missing `[reduction]` table
/// deserializes to `ReductionConfig::default()` (reduction enabled), and
/// the per-field `#[serde(default)]` keeps an omitted key enabled too.
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReductionConfig {
    /// Master switch for the dispatch-path token-reduction feature.
    /// Default on: reduction applies unless the operator turns it off
    /// globally or per provider.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ReductionConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Operator-facing `[capability]` config block. Kill switch plus tempo
/// knobs for the learned-capability subsystem. A missing `[capability]`
/// table deserializes to `CapabilityConfig::default()` (enabled, 48h
/// decay, 1h inferred window, 14d staleness hint), and each per-field
/// `#[serde(default)]` keeps an omitted key at its default too.
///
/// `enabled` is the master switch: off leaves any learned entries
/// resident but inert (both the learn path and the act path are skipped).
/// `decay_hours` sets how long a learned negative acts before it lapses
/// into a single re-probe; `inferred_window_hours` bounds how long a
/// pending single-observation inferred signal waits for a confirming
/// second observation before it resets. `staleness_hint_days` sets the
/// age past which a verified capability reads as stale in diagnostics
/// (display-only; not wired into the act path).
///
/// `#[non_exhaustive]` leaves room for later knobs without breaking
/// callers; `#[serde(deny_unknown_fields)]` rejects a typo'd key at
/// config-load time (matching the sibling feature blocks) instead of
/// silently ignoring it.
///
/// `overrides` is the operator capability-override map. It nests under
/// this existing `[capability]` parent as `[capability.overrides]` -- no
/// new top-level Config section is introduced, so the config-classify
/// coverage in routectl-cli stays exhaustive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CapabilityConfig {
    /// Master switch for the learned-capability subsystem. Default on.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hours a learned negative acts before it lapses into a single
    /// re-probe. Default 48.
    #[serde(default = "default_decay_hours")]
    pub decay_hours: u64,
    /// Hours a pending single-observation inferred signal waits for a
    /// confirming second observation before it resets. Default 1.
    #[serde(default = "default_inferred_window_hours")]
    pub inferred_window_hours: u64,
    /// Days past which a verified capability stamp reads as stale in
    /// diagnostics. Display-only -- surfaced in doctor / CLI hints, never
    /// wired into router construction or the act path. Default 14.
    #[serde(default = "default_staleness_hint_days")]
    pub staleness_hint_days: u64,
    /// Operator capability overrides, keyed by two-tier target spec:
    /// `"provider_name"` applies to every model on that provider, and
    /// `"provider_name:nickname"` targets a single model. An omitted
    /// `[capability.overrides]` table yields an empty map. The
    /// capability-value namespace is open -- values are not validated
    /// against any known-capability list here.
    #[serde(default)]
    pub overrides: BTreeMap<String, OverrideEntry>,
}

impl Default for CapabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decay_hours: default_decay_hours(),
            inferred_window_hours: default_inferred_window_hours(),
            staleness_hint_days: default_staleness_hint_days(),
            overrides: BTreeMap::new(),
        }
    }
}

/// A single operator capability override, the value type of
/// `[capability.overrides]`. `unsupported` force-marks capabilities as
/// unavailable for the target; `force_supported` overrides a learned or
/// catalog negative back to available. Both default to empty.
///
/// `#[serde(deny_unknown_fields)]` rejects a typo'd key inside an entry at
/// config-load time rather than silently ignoring it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OverrideEntry {
    /// Capabilities force-marked unavailable for the target.
    #[serde(default)]
    pub unsupported: Vec<String>,
    /// Capabilities force-marked available for the target, overriding a
    /// learned or catalog negative.
    #[serde(default)]
    pub force_supported: Vec<String>,
}

const fn default_decay_hours() -> u64 {
    48
}

const fn default_inferred_window_hours() -> u64 {
    1
}

const fn default_staleness_hint_days() -> u64 {
    14
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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

/// Operator-facing `[window_gate]` config block. Kill switch for the
/// proactive context-window gate, which de-prioritizes routing targets
/// whose context window cannot hold the estimated request. A missing
/// `[window_gate]` table deserializes to `WindowGateConfig::default()`
/// (enabled), so an existing config needs no migration.
///
/// Off must be byte-identical to no gate at all: no estimate computed, no
/// chain reordering, no diagnostics movement.
///
/// One field deliberately: the gate's safety margin against estimator
/// error is a baked constant, not an operator knob, because a margin
/// tuned per deployment turns a routing decision into a support surface.
/// `#[non_exhaustive]` leaves room for a later knob without breaking
/// callers; `#[serde(deny_unknown_fields)]` rejects a typo'd key at
/// config-load time instead of silently ignoring it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WindowGateConfig {
    /// Master switch for the proactive context-window gate. Default on.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for WindowGateConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
}

/// Operator-facing `[calibration]` config block. Kill switch for the
/// learned per-lane correction of the router's token estimate. A missing
/// `[calibration]` table deserializes to `CalibrationConfig::default()`
/// (enabled), so an existing config needs no migration.
///
/// Default ON is safe because a lane produces no correction until it has
/// accumulated real evidence: a fresh install behaves exactly as it would
/// with the switch off, and every refusal path (thin evidence, stale
/// evidence, an out-of-band reduced ratio) falls back to the uncorrected
/// estimate.
///
/// Off stops the correction from being APPLIED and nothing else. The static
/// context-window gate keeps gating on the uncorrected estimate, and the
/// collected evidence is retained, so switching back on is instant rather
/// than a re-learn.
///
/// One field deliberately, following `WindowGateConfig`: the reduction's
/// sample floors and sane band are baked constants, not operator knobs,
/// because a per-deployment band turns a routing decision into a support
/// surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CalibrationConfig {
    /// Master switch for applying the learned correction. Default on.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
}

/// Operator-facing `[seat_quota]` config block. Kill switch for
/// subscription-quota-aware seat placement, which orders a NEW conversation's
/// birth seat by each credential account's remaining short-window budget. A
/// missing `[seat_quota]` table deserializes to `SeatQuotaConfig::default()`
/// (enabled), so an existing config needs no migration.
///
/// Default ON is safe for the same reason the learned correction's is: a seat
/// produces no reading until a real upstream response carries one, so a fresh
/// process places exactly as it would with the switch off, and every refusal
/// path (no reading, an expired one, a reading the trust rules declined, a
/// provider with no curated short window) falls back to the pre-quota chooser.
///
/// Off means the birth chooser for an unpinned session is byte-identical to
/// the same chooser with no quota placement compiled in: no quota state is
/// read, no cap orders a pick, no quota placement diagnostic is emitted, and
/// the dispatchability filter, the breaker health preference, the RPM headroom
/// ranking and the anti-herd tiebreak all decide exactly as before. Following
/// `CalibrationConfig`, off does NOT stop collecting or aging readings, so
/// switching back on is instant rather than a re-observe. Universal affinity
/// is unaffected in both positions: every pin is preserved and a one-time
/// migration off an unhealthy seat still happens, because a wrong placement
/// algorithm must never cost the warm-cache benefit pinning exists for.
///
/// One field deliberately, following `WindowGateConfig` and
/// `CalibrationConfig`: the per-provider caps, the long-window guard and the
/// freshness bounds are curated constants grounded in captured upstream
/// evidence, not operator knobs, because a cap tuned per deployment turns a
/// routing decision into a support surface.
/// `#[non_exhaustive]` leaves room for a later knob without breaking
/// callers; `#[serde(deny_unknown_fields)]` rejects a typo'd key at
/// config-load time instead of silently ignoring it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SeatQuotaConfig {
    /// Master switch for quota-aware birth placement. Default on.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for SeatQuotaConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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
pub fn routectl_config_dir() -> PathBuf {
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSource {
    /// Authenticate to the upstream with routectl's own managed
    /// credential (the default behavior).
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
/// Transport-only (the original shape): this block carries no credential
/// knob. Which credential a forwarded egress uses is a per-provider
/// choice (`ProviderEntry::AnthropicApi.credential_source`) -- see
/// `preflight_legacy_mitm_credential_source` for the pre-parse check
/// that catches a config still carrying the removed `[mitm]
/// credential_source` key and names the replacement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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

/// Per-million-token pricing for one `[registry.*]` entry. All fields
/// are USD per million tokens -- the CANONICAL operator-facing price unit
/// throughout routectl -- and all are optional: routectl ships no price
/// defaults, so any field left unset means "this dimension is unpriced" and
/// contributes nothing to a derived cost. Cost is computed at query time,
/// never persisted, so a corrected price retroactively fixes historical rows.
///
/// A row here wins WHOLE over the baked catalog's own rates, which fill in
/// only when no row prices an upstream: this struct has no
/// explicitly-unpriced sentinel, so a per-field fill could not tell a
/// deliberate omission from an absent value.
///
/// `Eq` is deliberately NOT derived: `f64` is not `Eq`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PricingConfig {
    /// USD per million input tokens. `None` leaves the input dimension unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    /// USD per million output tokens. `None` leaves the output dimension unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    /// USD per million tokens read from the prompt cache. `None` leaves it unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<f64>,
    /// USD per million tokens written to the 5-minute cache tier. `None` leaves it unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_per_mtok: Option<f64>,
    /// USD per million tokens written to the 1-hour cache tier. `None` leaves it unpriced.
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
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RegistryEntry {
    /// Per-million-token pricing for this registry row. `None` leaves the row unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<PricingConfig>,
    /// Provider scope. When set, the row applies only to the named provider,
    /// letting the same upstream id be priced differently per provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

    /// Per-model first-content timeout for streaming responses. Resolved
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
    /// Build an entry binding the given upstream model id to a provider,
    /// with all other knobs at their defaults.
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

    /// Set the reasoning dialect that governs how thinking is expressed on the wire.
    pub const fn with_reasoning_dialect(mut self, d: ReasoningDialect) -> Self {
        self.reasoning_dialect = Some(d);
        self
    }

    /// Set how prior-turn reasoning is carried forward in conversation history.
    pub const fn with_history_reasoning(mut self, h: HistoryReasoning) -> Self {
        self.history_reasoning = Some(h);
        self
    }

    /// Set the per-model header extras merged into each outbound request.
    pub fn with_header_extras(mut self, headers: BTreeMap<String, String>) -> Self {
        self.header_extras = headers;
        self
    }

    /// Set the per-model payload extras merged into each outbound request body.
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
    /// operator-error sentinel (every stream would time out before its
    /// first content-bearing chunk arrived); flagged in debug builds.
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AliasValue {
    /// A single model nickname.
    Single(String),
    /// An ordered fallback chain of model nicknames.
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

    /// True when this alias resolves to no nicknames (an empty chain).
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
    /// Yields the single nickname once.
    Single(Option<&'a str>),
    /// Yields each nickname in the chain in order.
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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

/// Listener bind config: host, port, auth, and request-size / translation
/// posture for the HTTP server.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind host. Defaults to localhost. Refuses non-loopback unless
    /// `--unsafe-public` is passed on the CLI.
    #[serde(default = "default_host")]
    pub host: String,
    /// Bind port.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Listener-side auth. When `tokens` is non-empty, every request
    /// must carry a matching `x-api-key` or `Authorization: Bearer
    /// <token>` header. Tokens are SecretRef URIs (env://, file://)
    /// and are resolved at startup. Inline `literal:` refs are rejected
    /// at parse.
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

/// Listener-side authentication: the set of tokens a client must present.
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

/// One `[providers.X]` entry, tagged by `kind`. Each variant carries the
/// transport-side knobs (auth, base URL, header/payload extras, cache and
/// runtime policy) for one upstream shape.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProviderEntry {
    /// OpenAI-compatible chat-completions provider.
    #[non_exhaustive]
    OpenaiCompat {
        /// Endpoint base URL. Required (non-empty) on the standard lane
        /// -- validation rejects an omitted / empty value. Defaulted
        /// (empty) only so the mantle lane may omit it: when
        /// `bedrock_mantle` is set the factory derives the URL from
        /// `bedrock_mantle.region`, and validation then REQUIRES this be
        /// left unset.
        #[serde(default)]
        base_url: String,
        /// Reference to the API key. One of:
        ///   - `env://VAR_NAME`             (process env var)
        ///   - `file:///abs/path/to/key`    (mode-600 file)
        ///   - `oauth://<provider>[#seat]`  (routectl-managed OAuth token)
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
        /// Per-provider override for dispatch-path auto-emission of
        /// per-block cache breakpoints (the FRONT marker). `None` takes
        /// the kind-level default: `true` for an anthropic-api entry on
        /// the default base URL, `false` everywhere else. Independent of
        /// `auto_emit_top_level_breakpoint`, which governs the terminal
        /// top-level marker alone. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_per_block_breakpoints: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider even when
        /// global is on. Reduction policy, NOT a runtime/rate knob --
        /// lives outside `runtime`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        /// Opt-in AWS Bedrock mantle lane (OpenAI Chat Completions shape).
        /// Present -> this provider egresses through Bedrock's managed
        /// OpenAI-compatible surface: the factory derives `base_url` from
        /// `bedrock_mantle.region` and authenticates with
        /// `bedrock_mantle.creds`. Omitted (default) -> the standard
        /// OpenAI-compatible lane. When set, `api_key_ref` must be empty
        /// and `base_url` left unset -- validation rejects every other
        /// combination (region is the single source of truth for the
        /// endpoint, and the credential lives in `creds`).
        #[cfg(feature = "bedrock")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bedrock_mantle: Option<BedrockMantleConfig>,
        /// Shared runtime and rate-limit policy for this provider.
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    /// Native Anthropic Messages API provider.
    #[non_exhaustive]
    AnthropicApi {
        /// Reference to the API key, resolved the same way as every
        /// other provider's `api_key_ref`. Defaulted (empty string) so a
        /// `credential_source = "forwarded"` block can omit it entirely
        /// -- `validate_provider_credential_sources` then REQUIRES it be
        /// empty for `forwarded` and non-empty for `own`.
        #[serde(default)]
        api_key_ref: String,
        /// Endpoint base URL.
        #[serde(default = "default_anthropic_base")]
        base_url: String,
        /// `anthropic-version` header value sent upstream.
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
        /// Per-provider override for dispatch-path auto-emission of
        /// per-block cache breakpoints (the FRONT marker). `None` takes
        /// the kind-level default: `true` for an anthropic-api entry on
        /// the default base URL, `false` everywhere else. Independent of
        /// `auto_emit_top_level_breakpoint`, which governs the terminal
        /// top-level marker alone. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_per_block_breakpoints: Option<bool>,
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
        /// Opt-in AWS Bedrock mantle lane. Present -> this provider
        /// egresses through Bedrock's managed Anthropic Messages surface:
        /// the factory derives `base_url` from `bedrock_mantle.region` and
        /// authenticates with `bedrock_mantle.creds`. Omitted (default) ->
        /// the standard direct-to-Anthropic lane. When set, `auth_kind`
        /// must be `api-key`, `credential_source` `own`, `api_key_ref`
        /// empty, and `base_url` left at its default -- validation rejects
        /// every other combination (region is the single source of truth
        /// for the endpoint, and the credential lives in `creds`).
        #[cfg(feature = "bedrock")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bedrock_mantle: Option<BedrockMantleConfig>,
        /// Shared runtime and rate-limit policy for this provider.
        #[serde(default, flatten)]
        runtime: ProviderRuntimePolicy,
    },
    /// OpenAI Responses API provider. Three auth surfaces:
    /// - `chatgpt-oauth`: ChatGPT subscription JWT.
    /// - `api-key`: standard OpenAI API key.
    /// - `bedrock-mantle`: `Authorization: Bearer <bearer>` using the
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
        /// Codex CLI version this provider claims on the wire, threaded
        /// into the derived User-Agent and the `version` identity header
        /// (chatgpt-oauth surface). None -> the pinned default. The codex
        /// identity is process-global, so every openai-responses provider
        /// that sets this must agree on one value (validation rejects
        /// divergence). RESTART-REQUIRED: a hot reload cannot flip it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_version: Option<String>,
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
        /// Per-provider override for dispatch-path auto-emission of
        /// per-block cache breakpoints (the FRONT marker). `None` takes
        /// the kind-level default: `true` for an anthropic-api entry on
        /// the default base URL, `false` everywhere else. Independent of
        /// `auto_emit_top_level_breakpoint`, which governs the terminal
        /// top-level marker alone. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_per_block_breakpoints: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        /// Opt-in AWS Bedrock mantle lane (OpenAI Responses shape).
        /// Present -> this provider egresses through Bedrock's managed
        /// Responses surface: the factory derives `base_url` from
        /// `bedrock_mantle.region` and authenticates with
        /// `bedrock_mantle.creds`. Omitted (default) -> the standard
        /// Responses lane. When set, `auth_kind` must be `bedrock-mantle`
        /// (or omitted -- the factory sets the marker), `account_id_ref`
        /// and `api_key_ref` empty, and `base_url` left unset --
        /// validation rejects every other combination. Setting
        /// `auth_kind = "bedrock-mantle"` WITHOUT this block is a hard
        /// error (the legacy bearer-only surface is closed).
        #[cfg(feature = "bedrock")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bedrock_mantle: Option<BedrockMantleConfig>,
        /// Shared runtime and rate-limit policy for this provider.
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
        /// AWS region for the Bedrock runtime endpoint.
        region: String,
        /// Wire shape: InvokeModel (default) or Converse.
        #[serde(default)]
        api_shape: BedrockApiShapeConfig,
        /// AWS credential source for SigV4 signing.
        creds: BedrockCredsConfig,
        /// Override the outbound User-Agent.
        #[serde(default)]
        user_agent: Option<String>,
        /// Provider-level header extras.
        #[serde(default)]
        header_extras: BTreeMap<String, String>,
        /// Provider-level payload extras.
        #[serde(default)]
        payload_extras: Option<Value>,
        /// `anthropic_beta` flags forwarded on Anthropic-family models.
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
        /// Per-provider override for dispatch-path auto-emission of
        /// per-block cache breakpoints (the FRONT marker). `None` takes
        /// the kind-level default: `true` for an anthropic-api entry on
        /// the default base URL, `false` everywhere else. Independent of
        /// `auto_emit_top_level_breakpoint`, which governs the terminal
        /// top-level marker alone. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_per_block_breakpoints: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        /// Shared runtime and rate-limit policy for this provider.
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
        /// `cloud-code` mode the effective default is the daily Cloud Code
        /// host `https://daily-cloudcode-pa.googleapis.com`; set this only to
        /// reach the production host `https://cloudcode-pa.googleapis.com`,
        /// an enterprise mirror, or a test/staging host. One value carries
        /// the whole cloud-code lane -- generateContent, loadCodeAssist, and
        /// onboardUser all go to it -- and it is forwarded to the provider
        /// only when it differs from the api-key default.
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
        /// Cloud Code project id for this seat, in `cloud-code` mode. Give
        /// the BARE id (`my-project-1234`), never the `projects/`-prefixed
        /// resource name.
        ///
        /// Set it to skip discovery entirely: it is consulted before the
        /// credential store's cached project id and before `loadCodeAssist`
        /// / `onboardUser`, so a cold request goes straight to
        /// `generateContent`. The value also writes through to the seat's
        /// persisted project id, so later requests -- and other entries on
        /// the same seat -- read it from the cache. Two entries naming the
        /// same seat with different ids: last writer wins.
        ///
        /// If the host rejects the id as not applying to this seat, that
        /// request fails and the entry falls back to discovery for the rest
        /// of the process. Unset (or empty) leaves discovery as the only
        /// source. Ignored in `api-key` mode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cloud_project_id: Option<String>,
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
        /// Per-provider override for dispatch-path auto-emission of
        /// per-block cache breakpoints (the FRONT marker). `None` takes
        /// the kind-level default: `true` for an anthropic-api entry on
        /// the default base URL, `false` everywhere else. Independent of
        /// `auto_emit_top_level_breakpoint`, which governs the terminal
        /// top-level marker alone. Cache policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_emit_per_block_breakpoints: Option<bool>,
        /// Per-provider override for the dispatch-path token-reduction
        /// feature. `None` inherits the global `[reduction]` switch;
        /// `Some(false)` disables reduction for this provider. Reduction
        /// policy, not a runtime/rate knob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reduction_enabled: Option<bool>,
        /// Shared runtime and rate-limit policy for this provider.
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum BedrockApiShapeConfig {
    /// The vendor-specific InvokeModel wire shape.
    #[default]
    Invoke,
    /// The vendor-neutral Converse wire shape.
    Converse,
}

/// TOML-side credentials descriptor for a Bedrock provider.
///
/// Each variant is tagged by `kind`. Secret-bearing fields hold raw
/// secret-URI strings (`env://`, `file://`) which the
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum BedrockCredsConfig {
    /// A long-term Bedrock API key (bearer token).
    BearerKey {
        /// Reference to the bearer key.
        key_ref: String,
    },
    /// Static AWS access/secret keys, optionally with a session token.
    Static {
        /// Reference to the AWS access key id.
        access_key_ref: String,
        /// Reference to the AWS secret access key.
        secret_key_ref: String,
        /// Reference to an optional AWS session token.
        #[serde(default)]
        session_token_ref: Option<String>,
    },
    /// A named profile from the AWS shared-credentials file.
    Profile {
        /// The profile name.
        name: String,
    },
    /// The AWS default credential provider chain.
    DefaultChain,
}

/// Shared mantle sub-config for AWS Bedrock's managed inference lane. Its
/// PRESENCE on a provider entry selects the mantle lane: the factory
/// derives the endpoint base URL from `region` (region is the single
/// source of truth -- no manual `base_url`) and authenticates with
/// `creds`. Naming is deliberately lane-neutral so the OpenAI-shape lanes
/// reuse this exact type.
#[cfg(feature = "bedrock")]
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub struct BedrockMantleConfig {
    /// AWS region the mantle endpoint lives in (e.g. `us-east-1`). The
    /// factory derives both the endpoint host and the SigV4 signing scope
    /// from this single value, so it must be non-empty.
    pub region: String,
    /// Credential descriptor for this lane: a long-term bearer key or a
    /// SigV4 credential source (static keys, named profile, or the AWS
    /// default provider chain).
    pub creds: BedrockCredsConfig,
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

    /// The operator-set `codex_version` for this entry, or `None` when
    /// unset or the variant carries no such knob (only `OpenaiResponses`
    /// does). The resolved codex identity is derived from this across the
    /// whole config; see `factory::resolved_codex_version`.
    ///
    /// Not `const`: with `openai-responses` enabled the body calls
    /// `Option::as_deref`, which is not a const operation. Only the
    /// reduced build collapses to the constant `None` arm, and constness
    /// is part of the recorded public API -- so it cannot vary by feature.
    #[allow(clippy::missing_const_for_fn)]
    pub fn codex_version(&self) -> Option<&str> {
        match self {
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses { codex_version, .. } => codex_version.as_deref(),
            _ => None,
        }
    }

    /// `true` iff this entry is an `OpenaiResponses` provider on the
    /// `chatgpt-oauth` surface -- the only shape that emits the codex
    /// identity fingerprint, so the only one the codex-identity
    /// header-override warning applies to.
    #[cfg(feature = "openai-responses")]
    pub const fn is_chatgpt_oauth_responses(&self) -> bool {
        matches!(
            self,
            Self::OpenaiResponses {
                auth_kind: OpenaiResponsesAuthKind::ChatgptOauth,
                ..
            }
        )
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

    /// Operator-configured `anthropic_beta` floor for this entry: the
    /// betas the egress always sends, bypassing the client-beta
    /// allowlist. Only the Bedrock variant carries one today (the
    /// invoke/converse adapters re-add it on the wire after the
    /// canonical request build); every other variant has no such floor
    /// and returns an empty slice. Read by the dispatch-layer
    /// operator-floor-pin guard so a capability whose beta token the
    /// operator pins is never stripped (a stripped-then-re-added token
    /// is a false success).
    #[allow(clippy::missing_const_for_fn)]
    pub fn anthropic_beta_floor(&self) -> &[String] {
        match self {
            #[cfg(feature = "bedrock")]
            Self::Bedrock { anthropic_beta, .. } => anthropic_beta,
            _ => &[],
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

    /// Per-provider override for dispatch-path auto-emission of
    /// per-block cache breakpoints (the FRONT marker). `None` means
    /// "take the kind-level default" -- [`Self::per_block_breakpoints_enabled`]
    /// resolves it. Mirrors [`Self::auto_emit_top_level_breakpoint`].
    pub const fn auto_emit_per_block_breakpoints(&self) -> Option<bool> {
        match self {
            Self::OpenaiCompat {
                auto_emit_per_block_breakpoints,
                ..
            } => *auto_emit_per_block_breakpoints,
            Self::AnthropicApi {
                auto_emit_per_block_breakpoints,
                ..
            } => *auto_emit_per_block_breakpoints,
            #[cfg(feature = "bedrock")]
            Self::Bedrock {
                auto_emit_per_block_breakpoints,
                ..
            } => *auto_emit_per_block_breakpoints,
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                auto_emit_per_block_breakpoints,
                ..
            } => *auto_emit_per_block_breakpoints,
            #[cfg(feature = "gemini")]
            Self::Gemini {
                auto_emit_per_block_breakpoints,
                ..
            } => *auto_emit_per_block_breakpoints,
        }
    }

    /// Whether this entry's EGRESS can actually carry a per-block cache
    /// marker to the wire. Wire capability, not operator intent -- the
    /// counterpart of `CacheCapability::supports_top_level_cache_control`
    /// for the front marker, and the reason that struct is not widened
    /// (its `new()` is public with fixed arity).
    ///
    /// True for `anthropic-api` (native per-block `cache_control`) and for
    /// `bedrock` with `api_shape = "converse"` (the egress translates a
    /// per-block marker into a sibling `cachePoint` block). False
    /// everywhere else:
    ///
    /// - `bedrock` / `api_shape = "invoke"` -- no front-marker path; the
    ///   egress lowers the TOP-LEVEL marker to per-block itself, so an
    ///   additionally-placed front marker is redundant at best.
    /// - `openai-compat` -- the egress DROPS a per-block marker with a
    ///   WARN, and under `[server] strict_translation` rejects the whole
    ///   request with a 400.
    /// - `openai-responses` / `gemini` -- no per-block breakpoint surface
    ///   (both cache server-side automatically).
    ///
    /// Placement requires this AND [`Self::per_block_breakpoints_enabled`],
    /// so an explicit `auto_emit_per_block_breakpoints = true` on an
    /// unsupported kind stays INERT rather than emitting a marker the wire
    /// discards -- which would also record a false `auto_emitted` decision.
    pub const fn supports_per_block_breakpoints(&self) -> bool {
        match self {
            Self::AnthropicApi { .. } => true,
            #[cfg(feature = "bedrock")]
            Self::Bedrock { api_shape, .. } => {
                matches!(api_shape, BedrockApiShapeConfig::Converse)
            }
            _ => false,
        }
    }

    /// Whether per-block front-marker auto-emission is enabled for this
    /// entry: the operator override when set, otherwise the kind-level
    /// default.
    ///
    /// The kind-level default is `true` ONLY for an `anthropic-api` entry
    /// on the default Anthropic base URL -- the population whose terminal
    /// marker is already auto-emitted. Everything else, including both
    /// Bedrock shapes and a custom-base `anthropic-api` entry, defaults
    /// to `false` so the feature fails toward current behavior.
    ///
    /// Operator INTENT only. Placement additionally requires
    /// [`Self::supports_per_block_breakpoints`], so a `true` here on a
    /// kind whose egress cannot carry the marker does not emit one.
    ///
    /// Independent of `auto_emit_top_level_breakpoint`, which governs the
    /// terminal top-level marker alone.
    pub fn per_block_breakpoints_enabled(&self) -> bool {
        self.auto_emit_per_block_breakpoints()
            .unwrap_or_else(|| match self {
                Self::AnthropicApi { base_url, .. } => base_url == &default_anthropic_base(),
                _ => false,
            })
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

    /// Construct an `OpenaiCompat` entry with the given base URL and key
    /// reference and all other knobs at their defaults.
    pub fn openai_compat(base_url: impl Into<String>, api_key_ref: impl Into<String>) -> Self {
        Self::OpenaiCompat {
            base_url: base_url.into(),
            api_key_ref: api_key_ref.into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// Construct an `AnthropicApi` entry with the given key reference and
    /// all other knobs at their defaults.
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
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            cloak: CloakConfig::default(),
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
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
            codex_version: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
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
            cloud_project_id: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// Set the AnthropicApi variant's `auth_kind`. Panics on other variants.
    pub fn with_auth_kind(mut self, kind: AuthKind) -> Self {
        match &mut self {
            Self::AnthropicApi { auth_kind, .. } => *auth_kind = kind,
            _ => panic!("ProviderEntry::with_auth_kind only applies to anthropic-api"),
        }
        self
    }

    /// Set the Gemini variant's `auth_mode`. Panics on other variants.
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

    /// Set the `codex_version` on an `OpenaiResponses` entry. Panics on
    /// other variants -- the field is OpenaiResponses-only.
    #[cfg(feature = "openai-responses")]
    pub fn with_openai_responses_codex_version(mut self, version: impl Into<String>) -> Self {
        match &mut self {
            Self::OpenaiResponses { codex_version, .. } => *codex_version = Some(version.into()),
            _ => panic!(
                "ProviderEntry::with_openai_responses_codex_version only applies to openai-responses"
            ),
        }
        self
    }

    /// Replace this entry's runtime/rate-limit policy.
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

    /// Replace this entry's provider-level header extras.
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

    /// Replace this entry's provider-level payload extras.
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

    /// Set the `base_url` on an api-backed variant. Panics on others.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let u = url.into();
        match &mut self {
            Self::OpenaiCompat { base_url, .. } | Self::AnthropicApi { base_url, .. } => {
                *base_url = u;
            }
            #[cfg(any(feature = "bedrock", feature = "openai-responses", feature = "gemini"))]
            _ => panic!("ProviderEntry::with_base_url only applies to api-backed providers"),
        }
        self
    }

    /// Set the AnthropicApi variant's `anthropic_version`. Panics on others.
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

    /// Redact any inline literal secrets in this entry's key references, and
    /// reduce `base_url` to its origin, in place, so the entry is safe to
    /// serialize into diagnostics.
    ///
    /// The output is NOT round-trippable back into a config file: `base_url`
    /// loses its path, query, and any embedded credential (reduced to its
    /// origin), and a literal key ref becomes a sentinel. Every arm binds
    /// `base_url` explicitly rather than through `..` so a new variant, or a new
    /// URL-bearing field on an existing one, fails to compile here instead of
    /// silently shipping the raw value.
    pub fn redact_secrets(&mut self) {
        match self {
            Self::OpenaiCompat {
                api_key_ref,
                base_url,
                ..
            }
            | Self::AnthropicApi {
                api_key_ref,
                base_url,
                ..
            } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
                *base_url = redact_base_url(base_url);
            }
            // Carries no `base_url` at all: the region derives the endpoint.
            #[cfg(feature = "bedrock")]
            Self::Bedrock { creds, .. } => creds.redact(),
            #[cfg(feature = "openai-responses")]
            Self::OpenaiResponses {
                api_key_ref,
                account_id_ref,
                base_url,
                ..
            } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
                if let Some(a) = account_id_ref {
                    *a = redact_literal_secret(a);
                }
                // Optional here: `None` means "the factory picks the default"
                // and must stay `None`, not become a redaction sentinel.
                if let Some(url) = base_url {
                    *url = redact_base_url(url);
                }
            }
            #[cfg(feature = "gemini")]
            Self::Gemini {
                api_key_ref,
                base_url,
                ..
            } => {
                *api_key_ref = redact_literal_secret(api_key_ref);
                *base_url = redact_base_url(base_url);
            }
        }
    }

    /// Every secret-URI reference this entry carries, for startup
    /// resolution. A `forwarded` entry's empty `api_key_ref` is omitted.
    pub fn secret_uris(&self) -> Vec<&str> {
        match self {
            // The Bedrock mantle lane authenticates with
            // `bedrock_mantle.creds`, not `api_key_ref` (validation REQUIRES
            // the latter empty), so the empty ref is not surfaced -- doing so
            // would fail `SecretRef::parse` with a spurious "unrecognized
            // scheme" error on an otherwise-clean mantle provider. The creds
            // descriptor IS walked so a malformed creds ref scheme fails at
            // config check rather than only at build/probe.
            #[cfg(feature = "bedrock")]
            Self::OpenaiCompat {
                bedrock_mantle: Some(mantle),
                ..
            } => mantle.creds.secret_uris(),
            Self::OpenaiCompat { api_key_ref, .. } => vec![api_key_ref.as_str()],
            // A mantle AnthropicApi entry also authenticates with
            // `bedrock_mantle.creds` and REQUIRES an empty `api_key_ref`;
            // walk the creds descriptor (not the empty ref) so its scheme is
            // checked. Must precede the empty-ref guard below, which the
            // mantle shape would otherwise fall into.
            #[cfg(feature = "bedrock")]
            Self::AnthropicApi {
                bedrock_mantle: Some(mantle),
                ..
            } => mantle.creds.secret_uris(),
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
            #[cfg(all(feature = "openai-responses", feature = "bedrock"))]
            Self::OpenaiResponses {
                bedrock_mantle: Some(mantle),
                ..
            } => mantle.creds.secret_uris(),
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
    /// Empty ref slots are skipped: an empty string fed to `SecretRef::parse`
    /// fails as an "unrecognized scheme", a spurious error on the config-check
    /// walk. A genuinely-required-but-empty ref is caught by the validator.
    pub fn secret_uris(&self) -> Vec<&str> {
        let refs: Vec<&str> = match self {
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
        };
        refs.into_iter().filter(|r| !r.is_empty()).collect()
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

/// Reduce a configured `base_url` to something safe to display, with exactly
/// three outcomes:
///
/// - **empty stays EMPTY.** This is load-bearing, not a formatting nicety: the
///   bedrock-mantle lane REQUIRES an empty `base_url` (the factory carries a
///   `debug_assert!` on it, because `region` is the single source of truth for
///   the endpoint there). Rewriting `""` to a sentinel would render a correct
///   mantle config as broken.
/// - **a projectable value becomes its ORIGIN** -- scheme, host, and port only.
///   Userinfo, path, query, and fragment are dropped; each is a position a
///   credential is known to occupy in practice.
/// - **anything unprojectable becomes `[REDACTED]`** -- the fail-safe withhold,
///   never an empty string, because empty now means the mantle lane per the
///   first bullet.
///
/// # When a surface must reduce
///
/// A `base_url` is allowed to carry a credential: the provider validator checks
/// scheme, link-local targets, and cleartext-on-non-loopback, but only the
/// `[mitm]` origin validator rejects userinfo. So
/// `https://user:<secret>@upstream.example/v1` is an ACCEPTED provider config
/// and the raw string sits in the entry. Classify each surface that touches the
/// value into one of three buckets:
///
/// - **EMIT** -- the value reaches stdout/stderr, a tracing field, an HTTP
///   response body, or an error string that reaches either. REDUCE, through
///   this function.
/// - **WRITE or DIAL** -- the value travels config-file -> memory ->
///   config-file, or is parsed as a network target. EXEMPT: reducing there
///   CORRUPTS behavior, silently rewriting an operator's config or dialing the
///   wrong endpoint.
/// - **NAME-ONLY** -- the surface touches the identifier but never the value
///   (key labels, allowlists, fixtures). EXEMPT, nothing to do.
fn redact_base_url(base_url: &str) -> String {
    if base_url.trim().is_empty() {
        return base_url.to_string();
    }
    crate::config_effective::endpoint_origin(base_url).unwrap_or_else(|| "[REDACTED]".into())
}

/// Per-provider runtime knobs that gate dispatch: rate limits, circuit
/// breaker, timeouts, capability filters. All fields default to "off"
/// so omitting the block leaves provider behavior unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
    ///     -> `[retry]` request_timeout_ms (workspace global)
    ///       -> None (no cap, reqwest's default)
    ///
    /// Use this when many models share the same upstream and the
    /// timeout is an upstream characteristic (e.g., NIM cold-start),
    /// not a routing decision. v0.6 removed per-alias retry overrides;
    /// only the two tiers above remain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Per-attempt first-content timeout for streaming responses through
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
    #[schemars(
        with = "std::collections::BTreeMap<String, crate::class_policy::ConfigFailureClass>"
    )]
    pub class_overrides: BTreeMap<u16, ConfigFailureClass>,
}

/// Seat-selection strategy for a set of interchangeable accounts, set on
/// the `pools.<name>` block that groups them. Default is `fill-first` so a
/// single-member pool (the common case) keeps its current behavior with no
/// config.
///
/// Subscription-quota-aware placement and the session-affinity layer reach
/// `sticky-least-loaded` ONLY. The other two variants keep their own contracts
/// unchanged: neither pins a session nor reads a seat's remaining budget.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum SeatSelection {
    /// Drain one seat fully before advancing to the next.
    ///
    /// The drain IS the contract, not a shortcoming of it: holding one seat
    /// keeps its prompt cache warm, and running that seat down until the
    /// upstream refuses is the price of that locality. An operator wanting
    /// budget-aware spreading picks `sticky-least-loaded` instead.
    #[default]
    FillFirst,
    /// Rotate across seats to spread load. The starting seat advances once per
    /// REQUEST, so consecutive requests of one conversation land on different
    /// seats.
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
    ///
    /// The ONLY variant subscription-quota-aware placement applies to: a birth
    /// (and a keyless request, which mints no pin) ranks candidates by
    /// remaining short-window budget when `[seat_quota]` is on and the evidence
    /// suffices.
    StickyLeastLoaded,
}

pub fn default_anthropic_base() -> String {
    "https://api.anthropic.com".into()
}

fn default_anthropic_version() -> String {
    "2023-06-01".into()
}

#[cfg(feature = "gemini")]
pub fn default_gemini_base() -> String {
    "https://generativelanguage.googleapis.com/v1beta".into()
}

/// How an openai-compat provider expresses reasoning/thinking on the wire.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningDialect {
    /// OpenAI's native reasoning shape.
    #[default]
    Openai,
    /// DeepSeek's reasoning shape.
    Deepseek,
    /// vLLM's reasoning shape.
    Vllm,
    /// A raw `<think>` tag embedded in message content.
    RawThinkTag,
    /// OpenRouter's reasoning shape.
    Openrouter,
    /// Forward reasoning fields verbatim without translation.
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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
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

/// Retry and backoff policy for a provider chain: attempt caps, backoff
/// timing, and optional per-error-class overrides.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    #[serde(default = "default_jitter_ms")]
    pub jitter_ms: u64,

    /// Per-error-class retry caps. When set, override `max_attempts` for
    /// that specific class. Useful because rate-limits often clear in
    /// a single retry while flaky 5xx may need more attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_429: Option<u32>,
    /// Retry cap for `5xx` responses. Overrides `max_attempts` when set.
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
    /// First-content timeout for streaming responses. If the upstream
    /// hasn't emitted a content-bearing chunk in this window, the stream
    /// is abandoned and (if no content has been delivered yet) the next
    /// provider in the chain is tried. Content-free leading chunks (a
    /// `delta.role` opener, id/model metadata) neither reset nor satisfy
    /// this timeout.
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
            jitter_ms: default_jitter_ms(),
            retry_on_429: None,
            retry_on_5xx: None,
            retry_on_network: None,
            request_timeout_ms: None,
            // The early-response inversion holds the client warm, so a
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

impl RetryPolicy {
    /// Maximum attempts a single provider can ever consume regardless
    /// of error class. The router uses this as a hard ceiling so a
    /// misconfigured policy can't loop forever. Folds the per-class
    /// `[retry.classes]` overlay so the ceiling can never sit below a
    /// class cap the resolver would otherwise honor.
    pub fn hard_retry_cap(&self) -> u32 {
        self.max_attempts
            .max(self.retry_on_429.unwrap_or(0))
            .max(self.retry_on_5xx.unwrap_or(0))
            .max(self.retry_on_network.unwrap_or(0))
            .max(
                self.classes
                    .values()
                    .filter_map(|c| c.retry)
                    .max()
                    .unwrap_or(0),
            )
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

/// Backstop for `RetryPolicy::jitter_ms` when a `[retry]` block is present
/// but omits this key. Serde's bare `#[serde(default)]` would otherwise
/// fill `u64::default()` (`0`), disabling the anti-thundering-herd jitter
/// the moment an operator adds `[retry]` to tune any other knob -- the
/// struct `Default` impl only applies when the whole `[retry]` table is
/// absent, not when individual keys within it are.
const fn default_jitter_ms() -> u64 {
    50
}

const fn default_backoff_multiplier() -> f64 {
    2.0
}

#[cfg(test)]
#[path = "capability_config_tests.rs"]
mod capability_config_tests;

#[cfg(test)]
#[path = "window_gate_config_tests.rs"]
mod window_gate_config_tests;

#[cfg(test)]
#[path = "calibration_config_tests.rs"]
mod calibration_config_tests;

#[cfg(test)]
#[path = "seat_quota_config_tests.rs"]
mod seat_quota_config_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "v0_6_config_tests.rs"]
mod v0_6_config_tests;

#[cfg(test)]
#[path = "retry_policy_field_tests.rs"]
mod retry_policy_field_tests;

#[cfg(test)]
#[path = "mitm_config_tests.rs"]
mod mitm_config_tests;

#[cfg(test)]
#[path = "provider_credential_source_schema_tests.rs"]
mod provider_credential_source_schema_tests;

#[cfg(test)]
#[path = "seat_selection_tests.rs"]
mod seat_selection_tests;

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;

#[cfg(test)]
#[path = "schema_tests.rs"]
mod schema_tests;
