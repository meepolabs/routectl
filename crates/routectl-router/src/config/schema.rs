//! Config value types.

use routectl_core::provider_kind::config as kind;
use std::collections::BTreeMap;
use std::path::PathBuf;

use routectl_providers::anthropic_api::{AuthKind, CloakMode};
#[cfg(feature = "gemini")]
use routectl_providers::gemini::GeminiAuthMode;
#[cfg(feature = "openai-responses")]
use routectl_providers::openai_responses::AuthKind as OpenaiResponsesAuthKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::class_policy::ConfigFailureClass;

use super::provider_descriptor::{
    Auth, Carrier, Format, format_row, is_vendor_host, vendor_default_base, vendor_row,
};

#[cfg(test)]
use super::Config;

/// Operator-facing `[cache]` config block. Global policy for the
/// dispatch-path auto-cache feature. A missing `[cache]` table
/// deserializes to `CacheConfig::default()` (auto-emit enabled), and the
/// per-field `#[serde(default)]` gives an omitted key that same default --
/// on for the two emission/normalization switches, off for
/// `k_gated_emission`.
///
/// This is the only operator switch for auto-emission; past it, each
/// provider kind's capability and default decide.
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
/// decay, 1h inferred window), and each per-field
/// `#[serde(default)]` keeps an omitted key at its default too.
///
/// `enabled` is the master switch: off leaves any learned entries
/// resident but inert (both the learn path and the act path are skipped).
/// `decay_hours` sets how long a learned negative acts before it lapses
/// into a single re-probe; `inferred_window_hours` bounds how long a
/// pending single-observation inferred signal waits for a confirming
/// second observation before it resets.
///
/// `#[non_exhaustive]` leaves room for later knobs without breaking
/// callers; `#[serde(deny_unknown_fields)]` rejects a typo'd key at
/// config-load time (matching the sibling feature blocks) instead of
/// silently ignoring it.
///
/// `overrides` is the operator capability-override map. It nests under
/// this existing `[capability]` parent as `[capability.overrides]` -- no
/// new top-level Config section is introduced, so the config-classify
/// coverage in routectl-cli stays exhaustive. `essential` is a sibling
/// GLOBAL list (no target-spec path segment -- that absence IS its
/// global-ness), deliberately not target-scoped: droppability is operator
/// intent that holds everywhere, while support is a per-target fact.
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
    /// Operator capability overrides, keyed by two-tier target spec:
    /// `"provider_name"` applies to every model on that provider, and
    /// `"provider_name:nickname"` targets a single model. An omitted
    /// `[capability.overrides]` table yields an empty map. The
    /// capability-value namespace is open -- values are not validated
    /// against any known-capability list here.
    #[serde(default)]
    pub overrides: BTreeMap<String, OverrideEntry>,
    /// Capability keys the operator declares ESSENTIAL globally: a target
    /// that only fails on one of them is routed away from rather than
    /// having the capability stripped in place. Absent (or a key absent
    /// from the list) means the baked strip-vs-route default applies.
    ///
    /// This surface only ever TIGHTENS: it can turn a baked strip into a
    /// route-away, never a baked route-away into a strip. Loosening needs
    /// a code-gated strip recipe, so there is no config path to it.
    ///
    /// It reclassifies a learned negative's RESPONSE, never the negative
    /// itself: it does not mask a negative the way `force_supported`
    /// does, and it does not hard-drop a target the way `unsupported`
    /// does.
    ///
    /// Accepts any known capability key EXCEPT `reasoning_replay`, whose
    /// strip is lane-managed rather than key-only: replay negatives never
    /// reach this consult, so listing it would be a meaningful-looking
    /// no-op. The dispatch-time replay strip-repair branch is out of this
    /// field's scope entirely -- it fires only after a proven upstream
    /// rejection, where the alternative is a hard failure.
    #[serde(default)]
    pub essential: Vec<String>,
}

impl Default for CapabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decay_hours: default_decay_hours(),
            inferred_window_hours: default_inferred_window_hours(),
            overrides: BTreeMap::new(),
            essential: Vec::new(),
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

/// Operator-facing `[window_gate]` config block. Kill switch for the
/// proactive context-window gate, which de-prioritizes routing targets
/// whose context window cannot hold the estimated request. A missing
/// `[window_gate]` table deserializes to `WindowGateConfig::default()`
/// (disabled); an operator opts in with `enabled = true`.
///
/// Off must be byte-identical to no gate at all: no estimate computed, no
/// chain reordering, no diagnostics movement.
///
/// One field deliberately: the gate's 1024-token headroom is a baked
/// constant, not an operator knob, because a margin
/// tuned per deployment turns a routing decision into a support surface.
/// `#[non_exhaustive]` leaves room for a later knob without breaking
/// callers; `#[serde(deny_unknown_fields)]` rejects a typo'd key at
/// config-load time instead of silently ignoring it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WindowGateConfig {
    /// Master switch for the proactive context-window gate. Default off.
    #[serde(default = "default_window_gate_enabled")]
    pub enabled: bool,
}

const fn default_window_gate_enabled() -> bool {
    false
}

impl Default for WindowGateConfig {
    fn default() -> Self {
        Self {
            enabled: default_window_gate_enabled(),
        }
    }
}

/// Canary re-verification cadence: every Nth eligible non-streaming
/// complete request for an acting learned verdict carries the tested
/// field unrepaired instead of applying the repair. A code constant, not
/// an operator knob or an environment override -- a re-verification
/// interval tuned per deployment turns a safety parameter into a support
/// surface.
pub const CANARY_INTERVAL: u32 = 100;

/// Confirmed repair cycles an ENVELOPE-class transform needs before it may act
/// pre-flight: one acknowledged confirmation.
///
/// Co-located with its prefix-impacting sibling and with [`CANARY_INTERVAL`]
/// deliberately. These three are one safety parameter set -- the quorum a
/// content rewrite needs, the quorum an envelope rewrite needs, and how often
/// either is re-verified -- and the ORDER between the two quorums is what makes
/// the classes distinguishable at all. Splitting them across modules is how one
/// gets raised without the reader of the other noticing.
pub const ENVELOPE_QUORUM: u32 = 1;

/// Confirmed repair-cycle quorum a prefix-impacting transform needs
/// before it may act pre-flight (and admission, which no target has). A code constant for the same reason as
/// [`CANARY_INTERVAL`], and strictly greater than [`ENVELOPE_QUORUM`] -- the
/// anonymous `const _` assertions below enforce that ORDER at build time, and
/// the named test `the_three_quorum_values_are_exactly_one_one_and_two` pins this
/// exact value (which the order assertion alone would not).
pub const PREFIX_QUORUM: u32 = 2;

/// The eligibility FLOOR cannot exceed the cheapest class's own quorum, or the
/// floor would refuse a verdict that class considers sufficient and
/// `below_quorum` would be unreachable for it.
///
/// Compares [`crate::field_verdict::MINIMUM_CONFIRMATIONS`] DIRECTLY -- not a
/// copy of its value -- so this is a real weld: the two cannot drift, because
/// there is only one definition in play. (`field_verdict` depends on this module
/// and not the reverse, but a `const` path is not a dependency edge, so reading
/// it here is fine.)
///
/// An anonymous `const _: ()`, the repo-standard shape for a compile-time
/// assertion. Rustc evaluates
/// the initializer of every `const` item in a crate it compiles, so this runs
/// with no consumer, no reference, and no name to keep alive -- and an anonymous
/// item cannot trip `dead_code`, so it needs no lint expectation either.
///
/// SCOPE, because it is narrower than "the literals are protected": an edit that
/// VIOLATES this inequality fails the BUILD (`error[E0080]: evaluation panicked`
/// here). An ORDER-PRESERVING retune does not -- `MINIMUM_CONFIRMATIONS = 0`
/// compiles clean. The exact values are pinned separately, by the named test
/// `the_three_quorum_values_are_exactly_one_one_and_two`.
const _: () = assert!(
    crate::field_verdict::MINIMUM_CONFIRMATIONS <= ENVELOPE_QUORUM,
    "the shared eligibility floor cannot exceed the envelope class's own quorum, or an \
     eligible envelope verdict would be refused by the floor and `below_quorum` would be \
     unreachable for that class",
);

/// The envelope quorum is STRICTLY smaller than the prefix-impacting one, or no
/// confirmation count could ever separate the two classes and the whole content
/// gate collapses into the envelope one.
///
/// Same anonymous-`const` mechanism, and the same SCOPE: an edit that VIOLATES
/// the inequality fails the build, while an order-preserving retune does not --
/// `PREFIX_QUORUM = 3` compiles clean, since 1 < 3 still holds. The exact values
/// are pinned by `the_three_quorum_values_are_exactly_one_one_and_two`.
const _: () = assert!(
    ENVELOPE_QUORUM < PREFIX_QUORUM,
    "the envelope quorum must be STRICTLY smaller than the prefix-impacting one, or no \
     confirmation count could ever separate the two classes and the content gate would \
     collapse into the envelope gate",
);

/// Operator-facing `[usage]` config block. Controls the
/// usage-accounting subsystem, which always records. Each field carries
/// a default, so a config with no `[usage]` block deserializes to the
/// same value as `UsageConfig::default()`.
///
/// Reload semantics: `db_path` and `retention_days` are restart-required
/// (the writer opens the db at boot and holds the handle, and pruning runs
/// only at startup); `poll` hot-reloads. See
/// `collect_restart_required_changes` in the CLI server module for the
/// classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    /// SQLite database path for persisted usage rows. Defaults to
    /// `<config-dir>/routectl/usage.db` with the user config dir
    /// resolved from `XDG_CONFIG_HOME` or `HOME` -- no literal `~`
    /// ever reaches SQLite. With neither variable an absolute path the
    /// default is empty, and every command that opens the database
    /// refuses it until this key is set.
    #[serde(default = "default_usage_db_path")]
    pub db_path: PathBuf,
    /// Rows older than this many days are pruned on daemon startup.
    /// Default 90.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Whether the daemon polls each managed Claude and Codex login's usage
    /// endpoint in the background. Default true. With `false` no usage
    /// poll request is sent, and placement reads response header readings
    /// only, so an idle account has no fresh reading. Hot-reloads: the
    /// poll reads it before each sweep and again before landing a result.
    #[serde(default = "default_true")]
    pub poll: bool,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            db_path: default_usage_db_path(),
            retention_days: default_retention_days(),
            poll: true,
        }
    }
}

const fn default_retention_days() -> u32 {
    90
}

/// `name` inside the routectl config folder
/// ([`routectl_core::config_dir::config_dir`]), or an empty path when no
/// folder resolves. The empty path is never joined onto: whoever consumes
/// the default refuses it, naming the key, instead of resolving it against
/// the working directory.
fn config_folder_child(name: &str) -> PathBuf {
    routectl_core::config_dir::config_dir().map_or_else(PathBuf::new, |dir| dir.join(name))
}

fn default_usage_db_path() -> PathBuf {
    config_folder_child("usage.db")
}

/// Operator-facing `[mitm]` config block for the MITM front-proxy: a
/// local TLS-terminating listener that fronts `api.anthropic.com` (e.g.
/// Claude Code's own traffic) so routectl can observe and translate the
/// decrypted traffic. Presence of the `[mitm]` block gates the feature
/// on, matching the `[server.auth]` convention -- `Config::mitm == None`
/// keeps zero proxy startup and zero behavior change.
///
/// Transport-only and local-only: the block names where the listener
/// binds and where its certificates live, never where traffic goes. The
/// upstream origin and TLS host are fixed in routectl-cli's proxy module,
/// because the proxy forwards the client's full-scope claude.ai token.
/// Which credential a forwarded egress uses is a per-provider choice
/// (`credential = "forwarded"` on a `[providers.X]` entry). Retired keys of
/// this block are refused by the pre-parse retired-key table
/// (`RETIRED_KEYS`), which names each replacement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MitmConfig {
    /// Local TCP port the MITM listener binds. Must differ from
    /// `[server] port` -- the listener and the routectl HTTP server
    /// are separate bound sockets on the same host (enforced by
    /// `validate_mitm_config`).
    #[serde(default = "default_mitm_listen_port")]
    pub listen_port: u16,
    /// Directory holding the locally-generated MITM CA + leaf
    /// certificates. Defaults to `mitm-certs` in the routectl config
    /// folder, alongside the usage db and `config.toml`. With no config
    /// folder the default is empty, and every command that loads the CA
    /// refuses it until this key is set.
    #[serde(default = "default_mitm_cert_dir")]
    pub cert_dir: PathBuf,
}

impl Default for MitmConfig {
    fn default() -> Self {
        Self {
            listen_port: default_mitm_listen_port(),
            cert_dir: default_mitm_cert_dir(),
        }
    }
}

const fn default_mitm_listen_port() -> u16 {
    8443
}

fn default_mitm_cert_dir() -> PathBuf {
    config_folder_child("mitm-certs")
}

/// Per-million-token pricing for a `[models.X]` entry or a price-table row.
/// All fields are USD per million tokens -- the CANONICAL operator-facing
/// price unit throughout routectl -- and all are optional: any field left
/// unset means "this dimension is unpriced" by this source. Cost is computed
/// at query time, never persisted, so a corrected price retroactively fixes
/// historical rows. A `[models.X.pricing]` block fills field by field over
/// the refreshed and shipped price tables.
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
    /// USD per million tokens written to the cache, keyed by cache lifetime
    /// (`"5m"`, `"30m"`, `"24h"`: a positive integer and one of `s`, `m`, `h`,
    /// `d`). A key outside that grammar is refused at load. Where
    /// `cache_write_5m_per_mtok` or `cache_write_1h_per_mtok` is also set, that
    /// key prices the 5-minute or 1-hour lifetime. `None` leaves every other
    /// lifetime unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<BTreeMap<String, f64>>,
}

impl PricingConfig {
    /// The cache lifetime, in seconds, `cache_write_5m_per_mtok` prices.
    pub const CACHE_WRITE_5M_SECONDS: u32 = 300;
    /// The cache lifetime, in seconds, `cache_write_1h_per_mtok` prices.
    pub const CACHE_WRITE_1H_SECONDS: u32 = 3600;

    /// Every priced cache-write lifetime, seconds -> USD per million tokens.
    /// `cache_write_5m_per_mtok` and `cache_write_1h_per_mtok` win the 300 and
    /// 3600 cells over a `cache_write_per_mtok` key for the same lifetime. A
    /// key the duration grammar refuses prices nothing.
    pub fn cache_write_rates(&self) -> BTreeMap<u32, f64> {
        let mut rates: BTreeMap<u32, f64> = self
            .cache_write_per_mtok
            .iter()
            .flatten()
            .filter_map(|(key, rate)| Some((routectl_core::parse_cache_lifetime(key)?, *rate)))
            .collect();
        let legacy = [
            (Self::CACHE_WRITE_5M_SECONDS, self.cache_write_5m_per_mtok),
            (Self::CACHE_WRITE_1H_SECONDS, self.cache_write_1h_per_mtok),
        ];
        for (seconds, rate) in legacy {
            if let Some(rate) = rate {
                rates.insert(seconds, rate);
            }
        }
        rates
    }

    /// Price the cache-write lifetime `seconds` at `rate`: the 300 and 3600
    /// lifetimes in their own keys, any other in `cache_write_per_mtok` under
    /// its rendered duration.
    pub fn set_cache_write_rate(&mut self, seconds: u32, rate: f64) {
        match seconds {
            Self::CACHE_WRITE_5M_SECONDS => self.cache_write_5m_per_mtok = Some(rate),
            Self::CACHE_WRITE_1H_SECONDS => self.cache_write_1h_per_mtok = Some(rate),
            _ => {
                self.cache_write_per_mtok
                    .get_or_insert_with(BTreeMap::new)
                    .insert(routectl_core::format_cache_lifetime(seconds), rate);
            }
        }
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
/// PER-MODEL VALUE RELAY (read before adding a per-model value here).
/// A per-model value the egress reads passes through FOUR struct
/// definitions, each a deliberate layer, not a redundant copy:
///
///   1. `ModelEntry`                (this struct, config.rs) -- TOML
///      deserialization target; `#[serde(deny_unknown_fields)]`.
///   2. `crate::resolved::ResolvedModel`   -- startup-resolved per
///      nickname; adds the `Arc<dyn Provider>` and may transform the
///      type.
///   3. `crate::router::DispatchTarget`    -- per-request dispatch hop;
///      adds `state_key`, makes `provider` optional.
///   4. `routectl_core::RoutectlInternal`  -- the transport-internal
///      carrier on `ChatRequest`; the egress reads the value here.
///      `apply_layered_overlays` (router.rs) copies it from the
///      `DispatchTarget`.
///
/// `max_output_tokens` takes this relay: the factory stamps it onto
/// `ResolvedModel`, and a learned ceiling lowers what the egress sends at
/// dispatch. `context_window` stops at `ResolvedModel`, where the window
/// gate and `/v1/models` read it. The hop is the price
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

    /// Extra HTTP headers applied to every outbound request from this
    /// model's route. Merged with the provider's `header_extras` at
    /// dispatch time; model wins on key collision. The list-valued
    /// `anthropic-beta` header runs through a comma-split-union-rejoin
    /// post-pass (ingress -> provider -> model visit order) so all
    /// three sources land on one wire header. Auth-reserved keys
    /// (`authorization`, `x-api-key`, `anthropic-version`) drop, logged
    /// once as an advisory WARN at startup and per accepted config
    /// reload, and at DEBUG per request; managed-reserved keys (`host`,
    /// `content-type`, `content-length`) DEBUG + drop.
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
    /// with precedence per-model > per-provider > per-pool > global
    /// (`RetryPolicy::stream_first_byte_timeout_ms`) > shipped default.
    /// `0` is ignored with a warning and falls through to the next tier.
    ///
    /// Use case: opus xhigh adaptive thinking on large prompts can
    /// take >90s to emit the first token while non-thinking models
    /// (haiku, llama) start responding in <5s. A per-model override
    /// lets opus sit at 300s without forcing every other model to
    /// wait 5 min on a dead upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,

    /// The model's total context window in tokens (input plus output).
    /// The proactive window gate skips this model when a request cannot
    /// fit, and `GET /v1/models` reports it as `context_length`. Unset
    /// means unknown: the gate keeps the model and discovery omits the
    /// field. `0` is refused at load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub context_window: Option<u32>,

    /// The `max_tokens` value sent for this model when the client omits
    /// one, on the egresses that must send it (anthropic-api,
    /// bedrock-invoke). A ceiling learned from an upstream refusal lowers
    /// it at dispatch; unset falls back to 64000. A client value always
    /// wins. `0` is refused at load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_output_tokens: Option<u32>,

    /// Per-million-token prices for this model, each field optional. A set
    /// field wins over every other price source for that field; an unset
    /// field falls through to the refreshed, then the shipped price table. Prices feed cost display only.
    /// A non-finite or negative value is refused at load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<PricingConfig>,
}

impl ModelEntry {
    /// Build an entry binding the given upstream model id to a provider,
    /// with all other knobs at their defaults.
    pub fn new(provider: impl Into<String>, upstream: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            upstream: upstream.into(),
            reasoning_dialect: None,
            history_reasoning: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            stream_first_byte_timeout_ms: None,
            context_window: None,
            max_output_tokens: None,
            pricing: None,
        }
    }
    /// Redact every credential-bearing value in this entry, in place, for
    /// display: each `header_extras` value and every string leaf of
    /// `payload_extras` becomes `[REDACTED]`.
    /// Names every field, so a new one fails to compile here until someone
    /// decides whether it carries a secret.
    pub fn redact_secrets(&mut self) {
        let Self {
            header_extras,
            payload_extras,
            provider: _,
            upstream: _,
            reasoning_dialect: _,
            history_reasoning: _,
            stream_first_byte_timeout_ms: _,
            context_window: _,
            max_output_tokens: _,
            pricing: _,
        } = self;
        redact_extras(header_extras, payload_extras);
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

    /// Set the model's total context window in tokens.
    pub const fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// Set the `max_tokens` value sent when the client omits one.
    pub const fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    /// Set the per-model prices.
    pub fn with_pricing(mut self, pricing: PricingConfig) -> Self {
        self.pricing = Some(pricing);
        self
    }
}

const fn default_true() -> bool {
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

const fn default_max_body_bytes() -> u32 {
    32 * 1024 * 1024
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth: None,
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

impl ServerAuth {
    /// Redact each listener token ref for display, the way a provider key
    /// ref is redacted.
    pub fn redact_secrets(&mut self) {
        for token in &mut self.tokens {
            *token = redact_literal_secret(token);
        }
    }
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
    /// SEED-VERDICT: acceptance-seed -- an unhonored breakpoint is ignored
    /// rather than refused, so a wrong default never produces a rejection to
    /// learn from; the per-kind prior stands until an operator overrides it.
    pub fn for_provider_kind(kind: &str) -> Self {
        match kind {
            kind::ANTHROPIC_API => Self::new(true, true),
            // Conservative kind-level default used only when no format
            // is known (the kind token is shared by both Bedrock formats).
            // `ProviderEntry::cache_capability()` special-cases the
            // Bedrock formats and derives the live value from `format`
            // instead: `bedrock-invoke` -> supports_top_level = true, because the
            // egress lowers a routectl-injected top-level marker to the
            // per-block form Invoke caches on (commit 3e12f88); `bedrock-converse`
            // -> false, because a top-level marker is inert there (no
            // `cachePoint` translation). Hit usage is reported back on
            // both shapes, so cache_hit_observable stays true. This
            // kind-level value stays fail-closed as the no-shape fallback
            // so the dispatch path never auto-emits to a shape it cannot
            // reason about. Caller-supplied per-block markers cache
            // normally regardless; operators may override per-entry.
            kind::BEDROCK => Self::new(false, true),
            // OpenAI auto-caches server-side; there is no explicit
            // breakpoint to emit, but `cached_tokens` IS reported back.
            kind::OPENAI_RESPONSES => Self::new(false, true),
            // Gemini has implicit prefix caching (automatic, free writes)
            // and explicit context caching. No top-level breakpoint to
            // emit, but `cachedContentTokenCount` is reported back.
            kind::GEMINI => Self::new(false, true),
            // openai-compat and every unknown kind: emit nothing.
            _ => Self::new(false, false),
        }
    }
}

/// One `[providers.X]` entry. Every entry answers the same questions with
/// the same keys, in this order: `format` (the API), exactly one of
/// `base_url` or `cloud` (where requests go), `credential` (which secret)
/// and `auth` (how it is sent), then the companions `account_id_ref` and
/// `cloud_project_id`, then tuning. The identity keys are always written;
/// tuning keys default when omitted.
///
/// Which combinations are coherent (a format over a carrier with an auth,
/// a credential scheme with an auth, a managed login on its vendor's host)
/// is decided by the factory's provider plan, which every build path and
/// config validation run before any credential is read.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProviderEntry {
    /// The API this provider speaks: `anthropic-messages`, `openai-chat`,
    /// `openai-responses`, `bedrock-invoke`, `bedrock-converse`, `gemini`
    /// or `gemini-cloud-code`.
    pub format: Format,
    /// Endpoint base URL: a plain http(s) URL with a host. Exactly one of
    /// `base_url` and `cloud` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The cloud this provider is reached through, whose endpoint is derived
    /// from its location keys. Exactly one of `base_url` and `cloud` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<Cloud>,
    /// The one credential string: `env://VAR`, `file:///abs/path`,
    /// `oauth://<vendor>[#label]` (a routectl-managed login),
    /// `aws://profile/<name>`, `aws://chain`, or `forwarded` (the client's
    /// own credential is passed through).
    pub credential: String,
    /// How the credential goes on the wire: `x-api-key`, `bearer`,
    /// `x-goog-api-key` or `aws-sigv4`.
    pub auth: Auth,
    /// ChatGPT account id reference for the Codex host. Optional with a
    /// managed `oauth://codex` login (read from the login); required with a
    /// static credential there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id_ref: Option<String>,
    /// Cloud Code project id for a `gemini-cloud-code` seat. Give the BARE
    /// id (`my-project-1234`), never the `projects/`-prefixed resource name.
    ///
    /// Set it to skip discovery entirely: it is consulted before the
    /// credential store's cached project id and before `loadCodeAssist` /
    /// `onboardUser`, so a cold request goes straight to `generateContent`.
    /// The value also writes through to the seat's persisted project id, so
    /// later requests -- and other entries on the same seat -- read it from
    /// the cache. Two entries naming the same seat with different ids: last
    /// writer wins.
    ///
    /// If the host rejects the id as not applying to this seat, that request
    /// fails and the entry falls back to discovery for the rest of the
    /// process. Unset (or empty) leaves discovery as the only source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_project_id: Option<String>,
    /// `anthropic-version` header value sent upstream. Unset: `2023-06-01`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_version: Option<String>,
    /// Strict allowlist of inbound `x-claude-code-*` header names the egress
    /// may forward to api.anthropic.com. Unset or empty drops every captured
    /// `x-claude-code-*` header at the egress. Names match
    /// case-insensitively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_client_headers: Option<Vec<String>>,
    /// When true, routectl emulates Anthropic's
    /// context-management-2025-06-27 beta server-side for this provider. Set
    /// this for non-Anthropic `anthropic-messages` upstreams that do not
    /// honor the beta natively. Unset: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<bool>,
    /// Claude Code cloak mode: `auto` (the default), `always` or `never`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloak_mode: Option<CloakMode>,
    /// When true the cloak drops the client's own `system` content instead
    /// of relocating it into the first user message. Unset: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloak_strict_mode: Option<bool>,
    /// Codex CLI version this provider claims on the wire, threaded into the
    /// derived User-Agent and the `version` identity header. None -> the
    /// pinned default. The codex identity is process-global, so every
    /// provider that sets this must agree on one value (validation rejects
    /// divergence). RESTART-REQUIRED: a hot reload cannot flip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_version: Option<String>,
    /// Provider-level header extras. Merged with the per-model
    /// `header_extras` at dispatch time; model wins on key collision
    /// (list-valued `anthropic-beta` runs through a comma-split-union-rejoin
    /// post-pass instead).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header_extras: BTreeMap<String, String>,
    /// Provider-level payload extras. Deep-merged with the per-model
    /// `payload_extras` at dispatch time (model wins on leaf collision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_extras: Option<Value>,
    /// Override the outbound User-Agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Operator override for this entry's prompt-cache capability.
    /// `None` -> use the conservative per-format default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_capability: Option<CacheCapability>,
    /// Per-provider override for the dispatch-path token-reduction feature.
    /// `None` inherits the global `[reduction]` switch; `Some(false)`
    /// disables reduction for this provider even when global is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduction_enabled: Option<bool>,
    /// Shared runtime and rate-limit policy for this provider.
    #[serde(default, flatten)]
    pub runtime: ProviderRuntimePolicy,
}

/// The cloud a provider entry is reached through. Its endpoint host and
/// signing scope derive from the location keys, never from text the
/// operator writes as a URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "name", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Cloud {
    /// Amazon Web Services: Bedrock runtime for the `bedrock-*` formats,
    /// Bedrock's managed surface for the others.
    Aws {
        /// A canonical AWS region identifier (commercial, GovCloud or China
        /// partition), e.g. `us-east-1`.
        region: String,
    },
}

impl Cloud {
    /// The AWS region this cloud names.
    #[must_use]
    pub fn aws_region(&self) -> &str {
        match self {
            Self::Aws { region } => region,
        }
    }
}

/// Every provider-kind token a `[providers.X]` entry can resolve to through
/// its `format`, under the feature set this binary was built with. The
/// vocabulary [`ProviderEntry::kind_str`] draws from, gated by the same
/// `cfg`s -- a kind the build cannot construct is also a kind no config can
/// reach.
pub const CONFIG_PROVIDER_KINDS: &[&str] = &[
    kind::OPENAI_COMPAT,
    kind::ANTHROPIC_API,
    #[cfg(feature = "bedrock")]
    kind::BEDROCK,
    #[cfg(feature = "openai-responses")]
    kind::OPENAI_RESPONSES,
    #[cfg(feature = "gemini")]
    kind::GEMINI,
];

/// True when `kind` is a provider-kind token a `[providers.X]` entry can
/// resolve to in this build -- membership in [`CONFIG_PROVIDER_KINDS`].
#[must_use]
pub fn is_config_provider_kind(kind: &str) -> bool {
    CONFIG_PROVIDER_KINDS.contains(&kind)
}

/// The OpenAI API base the `api-key` Responses builder selects.
#[cfg(feature = "openai-responses")]
const OPENAI_API_BASE: &str = "https://api.openai.com/v1";

/// The public Gemini REST base a code-built `gemini` entry starts on.
#[cfg(feature = "gemini")]
const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The `anthropic-version` header value when the entry sets none.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

impl ProviderEntry {
    /// An entry over a base URL with every tuning key unset.
    fn over_base_url(format: Format, base_url: &str, credential: String, auth: Auth) -> Self {
        Self {
            format,
            base_url: Some(base_url.to_string()),
            cloud: None,
            credential,
            auth,
            account_id_ref: None,
            cloud_project_id: None,
            anthropic_version: None,
            forward_client_headers: None,
            context_management: None,
            cloak_mode: None,
            cloak_strict_mode: None,
            codex_version: None,
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            reduction_enabled: None,
            runtime: ProviderRuntimePolicy::default(),
        }
    }

    /// The wire format this entry speaks.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.format
    }

    /// Where this entry's requests are sent. An entry with a `cloud` rides
    /// that cloud; anything else (including an entry that sets neither,
    /// which the plan refuses) is read as the base-URL carrier.
    pub(crate) const fn carrier(&self) -> Carrier {
        if self.cloud.is_some() {
            Carrier::CloudAws
        } else {
            Carrier::BaseUrl
        }
    }

    /// The egress kind this entry sends with, read from the descriptor row
    /// for its [`format`](Self::format) over its carrier. `Local` for an
    /// entry the descriptor has no route for; the provider plan refuses
    /// such an entry before anything is built.
    pub const fn egress_kind(&self) -> routectl_core::wire_check::EgressKind {
        match format_row(self.format).route(self.carrier()) {
            Some(route) => route.egress,
            None => routectl_core::wire_check::EgressKind::Local,
        }
    }

    /// Stable token naming this entry's provider family: the family token
    /// of its [`format`](Self::format). Safe to surface in usage accounting
    /// and logs; persisted rows key on it.
    ///
    /// Every token this can return is in [`CONFIG_PROVIDER_KINDS`] for a
    /// format this build compiles.
    pub const fn kind_str(&self) -> &'static str {
        format_row(self.format).token
    }

    /// The entry's `credential` when it is a reference the credential store
    /// resolves (`env://`, `file://`, `oauth://`, or anything that is not one
    /// of the AWS forms or `forwarded`), else `None`. The usage CLI and the
    /// 401 self-heal path read this to find the store reference.
    pub fn api_key_ref(&self) -> Option<&str> {
        match routectl_auth::CredentialRef::parse(&self.credential) {
            Ok(
                routectl_auth::CredentialRef::AwsProfile(_)
                | routectl_auth::CredentialRef::AwsChain
                | routectl_auth::CredentialRef::Forwarded,
            ) => None,
            _ => Some(&self.credential),
        }
    }

    /// Get the runtime policy attached to this entry.
    pub const fn runtime(&self) -> &ProviderRuntimePolicy {
        &self.runtime
    }

    /// The operator-set `codex_version` for this entry, or `None` when
    /// unset. The resolved codex identity is derived from this across the
    /// whole config; see `factory::resolved_codex_version`.
    pub fn codex_version(&self) -> Option<&str> {
        self.codex_version.as_deref()
    }

    /// `true` iff this entry speaks the ChatGPT Codex dialect: an
    /// `openai-responses` entry whose `base_url` is on the Codex host. The
    /// codex-identity header-override warning applies only to it.
    pub fn is_chatgpt_oauth_responses(&self) -> bool {
        self.format == Format::OpenaiResponses
            && self.cloud.is_none()
            && self
                .base_url
                .as_deref()
                .is_some_and(|base| is_vendor_host("codex", base))
    }

    /// `Some(base_url)` iff this entry relays the client's own credential
    /// (`credential = "forwarded"`), `None` otherwise. The single place that
    /// recognizes "this is the forwarded provider". An entry that sets no
    /// `base_url` answers the empty string: it is still forwarded, and the
    /// plan refuses it.
    pub fn forwarded_base_url(&self) -> Option<&str> {
        (self.credential == FORWARDED_CREDENTIAL)
            .then(|| self.base_url.as_deref().unwrap_or_default())
    }

    /// `Some(base_url)` iff this entry is `anthropic-messages` over a base
    /// URL, `None` otherwise (another format, or the cloud carrier, whose
    /// endpoint is [`Self::classifiable_egress_base_url`]'s answer).
    pub fn anthropic_api_base_url(&self) -> Option<&str> {
        match (self.format, &self.cloud) {
            (Format::AnthropicMessages, None) => self.base_url.as_deref(),
            _ => None,
        }
    }

    /// The base URL this entry actually egresses to, when the configured
    /// fields alone determine it; `None` when they do not.
    ///
    /// - over a base URL: the configured `base_url`;
    /// - over the AWS cloud: the endpoint derived from `cloud.region`, the
    ///   same derivation the factory dials -- the Anthropic-vocabulary
    ///   managed base for `anthropic-messages`, the OpenAI-vocabulary managed
    ///   base for `openai-chat` and `openai-responses`, the runtime endpoint
    ///   for the `bedrock-*` formats.
    ///
    /// Every region-derived answer is `None` for a region that fails
    /// admission: it names no endpoint the factory could dial.
    pub fn classifiable_egress_base_url(&self) -> Option<std::borrow::Cow<'_, str>> {
        let Some(cloud) = &self.cloud else {
            return self.base_url.as_deref().map(std::borrow::Cow::Borrowed);
        };
        cloud_egress_base(self.format, cloud.aws_region()).map(std::borrow::Cow::Owned)
    }

    /// Per-provider `header_extras`.
    pub const fn header_extras(&self) -> &BTreeMap<String, String> {
        &self.header_extras
    }

    /// Per-provider `payload_extras` (None when the operator did not
    /// configure any).
    pub const fn payload_extras(&self) -> Option<&Value> {
        self.payload_extras.as_ref()
    }

    /// Prompt-cache capability for this entry. Returns the operator
    /// override when set, otherwise the conservative per-format default.
    ///
    /// `anthropic-messages` keeps the optimistic Anthropic default only on
    /// the Anthropic API host and on the AWS managed surface. Pointed at any
    /// other base URL it is an Anthropic-COMPATIBLE third party that may 400
    /// on or silently drop a top-level `cache_control` breakpoint, so the
    /// default fails closed -> (false, false).
    ///
    /// Bedrock is split by format: on `bedrock-invoke` the egress lowers a
    /// routectl-injected top-level marker to the per-block form Invoke caches
    /// on, so auto-emit is safe; on `bedrock-converse` a top-level marker is
    /// inert. Hit usage is reported back on both.
    pub fn cache_capability(&self) -> CacheCapability {
        self.cache_capability.unwrap_or_else(|| match self.format {
            Format::AnthropicMessages if self.has_anthropic_cache_defaults() => {
                CacheCapability::for_provider_kind(self.kind_str())
            }
            Format::AnthropicMessages => CacheCapability::new(false, false),
            Format::BedrockInvoke => CacheCapability::new(true, true),
            Format::BedrockConverse => CacheCapability::new(false, true),
            Format::OpenaiChat
            | Format::OpenaiResponses
            | Format::Gemini
            | Format::GeminiCloudCode => CacheCapability::for_provider_kind(self.kind_str()),
        })
    }

    /// True for an `anthropic-messages` entry on the Anthropic API host or on
    /// the AWS managed surface: the destinations whose caching behavior the
    /// Anthropic defaults describe.
    fn has_anthropic_cache_defaults(&self) -> bool {
        self.format == Format::AnthropicMessages
            && (self.cloud.is_some()
                || self
                    .base_url
                    .as_deref()
                    .is_some_and(routectl_core::identity::anthropic::is_anthropic_api_host))
    }

    /// Whether this entry's EGRESS can actually carry a per-block cache
    /// marker to the wire. Wire capability, not operator intent.
    ///
    /// True for `anthropic-messages` (native per-block `cache_control`) and
    /// for `bedrock-converse` (the egress translates a per-block marker into
    /// a sibling `cachePoint` block). False everywhere else: `bedrock-invoke`
    /// lowers the top-level marker itself, `openai-chat` drops a per-block
    /// marker with a WARN, and the Responses and Gemini formats cache
    /// server-side.
    ///
    /// Placement requires this AND [`Self::per_block_breakpoints_enabled`].
    pub const fn supports_per_block_breakpoints(&self) -> bool {
        matches!(
            self.format,
            Format::AnthropicMessages | Format::BedrockConverse
        )
    }

    /// Whether per-block front-marker auto-emission is enabled for this
    /// entry: `true` ONLY for an `anthropic-messages` entry on the Anthropic
    /// API host or the AWS managed surface -- the population whose terminal
    /// marker is already auto-emitted.
    ///
    /// Placement additionally requires
    /// [`Self::supports_per_block_breakpoints`].
    pub fn per_block_breakpoints_enabled(&self) -> bool {
        self.has_anthropic_cache_defaults()
    }

    /// Per-provider override for the dispatch-path token-reduction feature.
    /// `None` means "inherit the global `[reduction]` switch".
    pub const fn reduction_enabled(&self) -> Option<bool> {
        self.reduction_enabled
    }

    /// Construct the entry that consumes a routectl-managed login of
    /// `vendor`: the vendor's own format, auth and default base URL, with
    /// `credential` (an `oauth://<vendor>[#label]` reference), every tuning
    /// key unset. `None` for a vendor routectl does not manage.
    #[must_use]
    pub fn for_managed_login(vendor: &str, credential: impl Into<String>) -> Option<Self> {
        let row = vendor_row(vendor)?;
        Some(Self::over_base_url(
            row.format,
            row.default_base,
            credential.into(),
            row.auth,
        ))
    }

    /// Construct an `openai-chat` entry over `base_url` with the given
    /// credential sent as a bearer, every tuning key unset.
    pub fn openai_compat(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self::over_base_url(
            Format::OpenaiChat,
            &base_url.into(),
            credential.into(),
            Auth::Bearer,
        )
    }

    /// Construct an `anthropic-messages` entry on the Anthropic API with the
    /// given credential sent as `x-api-key`, every tuning key unset.
    pub fn anthropic_api(credential: impl Into<String>) -> Self {
        Self::over_base_url(
            Format::AnthropicMessages,
            vendor_default_base("anthropic"),
            credential.into(),
            Auth::XApiKey,
        )
    }

    /// Construct an `openai-responses` entry on the ChatGPT Codex host with
    /// the given credential sent as a bearer, every tuning key unset.
    #[cfg(feature = "openai-responses")]
    pub fn openai_responses(credential: impl Into<String>) -> Self {
        Self::over_base_url(
            Format::OpenaiResponses,
            vendor_default_base("codex"),
            credential.into(),
            Auth::Bearer,
        )
    }

    /// Construct a `gemini` entry on the public Gemini REST base with the
    /// given credential sent as `x-goog-api-key`, every tuning key unset.
    #[cfg(feature = "gemini")]
    pub fn gemini(credential: impl Into<String>) -> Self {
        Self::over_base_url(
            Format::Gemini,
            GEMINI_BASE,
            credential.into(),
            Auth::XGoogApiKey,
        )
    }

    /// Construct an entry of `format` over the AWS cloud in `region`, with
    /// `credential` sent as `auth`, every tuning key unset.
    pub fn cloud_aws(
        format: Format,
        region: impl Into<String>,
        credential: impl Into<String>,
        auth: Auth,
    ) -> Self {
        Self {
            base_url: None,
            cloud: Some(Cloud::Aws {
                region: region.into(),
            }),
            ..Self::over_base_url(format, "", credential.into(), auth)
        }
    }

    /// Set how an `anthropic-messages` entry sends its credential: `ApiKey`
    /// is `auth = "x-api-key"`, `OauthBearer` is `auth = "bearer"`. Panics on
    /// other formats.
    pub fn with_auth_kind(mut self, kind: AuthKind) -> Self {
        assert!(
            self.format == Format::AnthropicMessages,
            "ProviderEntry::with_auth_kind only applies to anthropic-messages"
        );
        self.auth = match kind {
            AuthKind::ApiKey => Auth::XApiKey,
            AuthKind::OauthBearer => Auth::Bearer,
        };
        self
    }

    /// Select the Gemini surface: `ApiKey` is `format = "gemini"` with
    /// `x-goog-api-key`, `CloudCode` is `format = "gemini-cloud-code"` with
    /// `bearer`. An entry still on the other surface's builder base moves to
    /// this surface's base. Panics on other formats.
    #[cfg(feature = "gemini")]
    pub fn with_gemini_auth_mode(mut self, mode: GeminiAuthMode) -> Self {
        assert!(
            matches!(self.format, Format::Gemini | Format::GeminiCloudCode),
            "ProviderEntry::with_gemini_auth_mode only applies to the gemini formats"
        );
        let (format, auth, from, to) = match mode {
            GeminiAuthMode::ApiKey => (
                Format::Gemini,
                Auth::XGoogApiKey,
                vendor_default_base("antigravity"),
                GEMINI_BASE,
            ),
            GeminiAuthMode::CloudCode => (
                Format::GeminiCloudCode,
                Auth::Bearer,
                GEMINI_BASE,
                vendor_default_base("antigravity"),
            ),
        };
        self.format = format;
        self.auth = auth;
        self.swap_builder_base(from, to);
        self
    }

    /// Replace a base URL still at the builder value `from` with `to`; any
    /// other value is the caller's and is kept.
    #[cfg(any(feature = "gemini", feature = "openai-responses"))]
    fn swap_builder_base(&mut self, from: &str, to: &str) {
        if self.base_url.as_deref() == Some(from) {
            self.base_url = Some(to.to_string());
        }
    }

    /// Set the `account_id_ref` companion.
    #[cfg(feature = "openai-responses")]
    pub fn with_account_id_ref(mut self, account_id_ref: impl Into<String>) -> Self {
        self.account_id_ref = Some(account_id_ref.into());
        self
    }

    /// Set the `base_url` on an `openai-responses` entry. Panics on other
    /// formats.
    #[cfg(feature = "openai-responses")]
    pub fn with_openai_responses_base_url(self, base_url: impl Into<String>) -> Self {
        assert!(
            self.format == Format::OpenaiResponses,
            "ProviderEntry::with_openai_responses_base_url only applies to openai-responses"
        );
        self.with_base_url(base_url)
    }

    /// Select the Responses surface by destination: `ChatgptOauth` moves an
    /// entry still on the OpenAI API builder base to the Codex host, and
    /// `ApiKey` moves one still on the Codex builder base to the OpenAI API.
    /// The Bedrock managed surface is a cloud; use [`Self::cloud_aws`].
    /// Panics on other formats and on `BedrockMantle`.
    #[cfg(feature = "openai-responses")]
    pub fn with_openai_responses_auth_kind(mut self, kind: OpenaiResponsesAuthKind) -> Self {
        assert!(
            self.format == Format::OpenaiResponses,
            "ProviderEntry::with_openai_responses_auth_kind only applies to openai-responses"
        );
        match kind {
            OpenaiResponsesAuthKind::ChatgptOauth => {
                self.swap_builder_base(OPENAI_API_BASE, vendor_default_base("codex"));
            }
            OpenaiResponsesAuthKind::ApiKey => {
                self.swap_builder_base(vendor_default_base("codex"), OPENAI_API_BASE)
            }
            OpenaiResponsesAuthKind::BedrockMantle => {
                panic!("the Bedrock managed surface is a cloud; build it with cloud_aws")
            }
        }
        self
    }

    /// Set the `codex_version` on an `openai-responses` entry. Panics on
    /// other formats.
    #[cfg(feature = "openai-responses")]
    pub fn with_openai_responses_codex_version(mut self, version: impl Into<String>) -> Self {
        assert!(
            self.format == Format::OpenaiResponses,
            "ProviderEntry::with_openai_responses_codex_version only applies to openai-responses"
        );
        self.codex_version = Some(version.into());
        self
    }

    /// Replace this entry's runtime/rate-limit policy.
    pub const fn with_runtime(mut self, rt: ProviderRuntimePolicy) -> Self {
        self.runtime = rt;
        self
    }

    /// Replace this entry's provider-level header extras.
    pub fn with_header_extras(mut self, headers: BTreeMap<String, String>) -> Self {
        self.header_extras = headers;
        self
    }

    /// Replace this entry's provider-level payload extras.
    pub fn with_payload_extras(mut self, extras: Value) -> Self {
        self.payload_extras = Some(extras);
        self
    }

    /// Point the entry at `url`, replacing any `cloud`.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self.cloud = None;
        self
    }

    /// Set the `anthropic-version` an `anthropic-messages` entry sends.
    /// Panics on other formats.
    pub fn with_anthropic_version(mut self, version: impl Into<String>) -> Self {
        assert!(
            self.format == Format::AnthropicMessages,
            "ProviderEntry::with_anthropic_version only applies to anthropic-messages"
        );
        self.anthropic_version = Some(version.into());
        self
    }

    /// Replace the entry's `credential` string.
    pub fn with_credential(mut self, credential: impl Into<String>) -> Self {
        self.credential = credential.into();
        self
    }

    /// Replace how the entry sends its credential.
    pub const fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    /// Set the `forward_client_headers` allowlist of an
    /// `anthropic-messages` entry. Panics on other formats.
    pub fn with_forward_client_headers(mut self, v: Vec<String>) -> Self {
        assert!(
            self.format == Format::AnthropicMessages,
            "ProviderEntry::with_forward_client_headers only applies to anthropic-messages"
        );
        self.forward_client_headers = Some(v);
        self
    }

    /// Redact every credential-bearing value in this entry, in place, so the
    /// entry is safe to serialize into diagnostics: a credential or
    /// account-id reference that is not an `env://` / `file://` / `oauth://`
    /// pointer, an AWS form or `forwarded` becomes a sentinel, every
    /// `header_extras` value and every string leaf of `payload_extras`
    /// becomes `[REDACTED]`, and `base_url` is reduced to its origin.
    ///
    /// The output is NOT round-trippable back into a config file. The
    /// destructure names every field (non-secret ones as `_`), so a new field
    /// fails to compile here until someone decides whether it carries a
    /// secret.
    pub fn redact_secrets(&mut self) {
        let Self {
            format: _,
            base_url,
            cloud: _,
            credential,
            auth: _,
            account_id_ref,
            cloud_project_id: _,
            anthropic_version: _,
            forward_client_headers: _,
            context_management: _,
            cloak_mode: _,
            cloak_strict_mode: _,
            codex_version: _,
            header_extras,
            payload_extras,
            user_agent: _,
            cache_capability: _,
            reduction_enabled: _,
            runtime: _,
        } = self;
        *credential = redact_credential(credential);
        if let Some(a) = account_id_ref {
            *a = redact_literal_secret(a);
        }
        if let Some(url) = base_url {
            *url = redact_base_url(url);
        }
        redact_extras(header_extras, payload_extras);
    }

    /// Every secret-URI reference this entry carries, for startup
    /// resolution: the credential when the store resolves it (never an AWS
    /// form or `forwarded`), then `account_id_ref`. Empty references are
    /// skipped; an empty string is not a parseable secret URI.
    pub fn secret_uris(&self) -> Vec<&str> {
        self.api_key_ref()
            .into_iter()
            .chain(self.account_id_ref.as_deref())
            .filter(|uri| !uri.is_empty())
            .collect()
    }
}

/// The reserved `credential` value that relays the client's own credential.
const FORWARDED_CREDENTIAL: &str = "forwarded";

/// The endpoint base the factory derives for `format` over the AWS cloud in
/// `region`, or `None` when the region fails admission or the build has no
/// AWS lanes.
#[cfg(feature = "bedrock")]
fn cloud_egress_base(format: Format, region: &str) -> Option<String> {
    let base = match format {
        Format::AnthropicMessages => routectl_providers::mantle::mantle_anthropic_base(region),
        Format::OpenaiChat | Format::OpenaiResponses => {
            routectl_providers::mantle::mantle_openai_base(region)
        }
        Format::BedrockInvoke | Format::BedrockConverse => {
            routectl_providers::bedrock::endpoint::bedrock_runtime_url(region)
        }
        Format::Gemini | Format::GeminiCloudCode => return None,
    };
    base.ok()
}

#[cfg(not(feature = "bedrock"))]
const fn cloud_egress_base(_format: Format, _region: &str) -> Option<String> {
    None
}

/// A credential as it may be displayed: the AWS forms and `forwarded` name
/// no secret and print as written; anything else is shown per
/// [`redact_literal_secret`].
fn redact_credential(credential: &str) -> String {
    match routectl_auth::CredentialRef::parse(credential) {
        Ok(
            routectl_auth::CredentialRef::AwsProfile(_)
            | routectl_auth::CredentialRef::AwsChain
            | routectl_auth::CredentialRef::Forwarded,
        ) => credential.to_string(),
        _ => redact_literal_secret(credential),
    }
}

/// What a redacted credential-bearing value prints as.
const REDACTED: &str = "[REDACTED]";

/// A secret ref as it may be displayed: an `env://`, `file://` or `oauth://`
/// pointer and an empty slot print as written; a `literal:` value keeps only
/// its scheme; anything else (a bare value is most likely a pasted key) is
/// withheld whole.
fn redact_literal_secret(uri: &str) -> String {
    if uri.is_empty() {
        return String::new();
    }
    match routectl_auth::SecretScheme::classify(uri) {
        Some(
            routectl_auth::SecretScheme::Env
            | routectl_auth::SecretScheme::File
            | routectl_auth::SecretScheme::OAuth,
        ) => uri.to_string(),
        Some(routectl_auth::SecretScheme::Literal) if uri == "literal:" => uri.to_string(),
        Some(routectl_auth::SecretScheme::Literal) => format!("literal:{REDACTED}"),
        _ => REDACTED.to_string(),
    }
}

/// Replace every header value with the sentinel (names stay) and every
/// string leaf of the payload extras, at any depth, with the sentinel (keys
/// and non-string scalars stay).
fn redact_extras(header_extras: &mut BTreeMap<String, String>, payload_extras: &mut Option<Value>) {
    for value in header_extras.values_mut() {
        *value = REDACTED.to_string();
    }
    if let Some(payload) = payload_extras {
        redact_json_strings(payload);
    }
}

fn redact_json_strings(value: &mut Value) {
    match value {
        Value::String(s) => *s = REDACTED.to_string(),
        Value::Array(items) => items.iter_mut().for_each(redact_json_strings),
        Value::Object(map) => map.values_mut().for_each(redact_json_strings),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Reduce a configured `base_url` to something safe to display, with exactly
/// three outcomes:
///
/// - **empty stays EMPTY**, so a blank value still reads as blank and the
///   validator's own "set but empty" refusal stays recognizable.
/// - **a projectable value becomes its ORIGIN** -- scheme, host, and port only.
///   Userinfo, path, query, and fragment are dropped; each is a position a
///   credential is known to occupy in practice.
/// - **anything unprojectable becomes `[REDACTED]`** -- the fail-safe withhold,
///   never an empty string, which would read as the first bullet.
///
/// # When a surface must reduce
///
/// A `base_url` is allowed to carry a credential: the provider validator checks
/// scheme, link-local targets, and cleartext-on-non-loopback, not userinfo.
/// So `https://user:<secret>@upstream.example/v1` is an ACCEPTED provider config
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
/// breaker, timeouts, capability filters. An unset rate-limit, breaker or
/// timeout key inherits the claiming pool's value, then the shipped
/// default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub struct ProviderRuntimePolicy {
    /// Maximum requests per minute. When exceeded, the router treats
    /// this provider as a fallbackable failure and tries the next entry
    /// in the chain. Wins over the claiming pool's value. None = inherit
    /// the pool's value, else unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_limit: Option<u32>,
    /// Trip the circuit breaker after this many consecutive failed
    /// attempts within the failure window. Once tripped, the router
    /// skips this provider for `circuit_cooldown_ms`. Wins over the
    /// claiming pool's value. None = inherit the pool's value, else 5.
    /// `0` turns the breaker off for this provider, whatever the pool
    /// sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_failures: Option<u32>,
    /// How long to keep the circuit open (skip provider) once tripped.
    /// Wins over the claiming pool's value; defaults to 60s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_cooldown_ms: Option<u64>,
    /// Per-attempt timeout on complete and count-tokens requests for
    /// every alias chain entry routing through this provider. A stream
    /// that has committed content is not bounded by it.
    ///
    /// Resolution order (first set wins):
    ///   provider.request_timeout_ms (this field)
    ///     -> the claiming `[pools.X]` request_timeout_ms
    ///       -> `[retry]` request_timeout_ms (workspace global)
    ///         -> the shipped 30 min default
    ///
    /// Use this when many models share the same upstream and the
    /// timeout is an upstream characteristic (e.g., NIM cold-start),
    /// not a routing decision. v0.6 removed per-alias retry overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// Per-attempt first-content timeout for streaming responses through
    /// this provider. Resolution order: `[models.X]` > this field > the
    /// claiming `[pools.X]` > `[retry]` > the shipped 5 min default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_first_byte_timeout_ms: Option<u64>,
}

/// Seat-selection strategy for a set of interchangeable accounts, set on
/// the `pools.<name>` block that groups them. Default is
/// `sticky-least-loaded`: a pool that omits the key keeps each conversation
/// on one account for prompt-cache affinity and spreads new conversations
/// by load. A single-member pool behaves identically under either strategy.
///
/// Pace placement and the session-affinity layer reach `sticky-least-loaded`
/// ONLY. `fill-first` keeps its own contract: it neither pins a session nor
/// reads an account's usage.
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
    /// usage-aware spreading picks `sticky-least-loaded` instead.
    FillFirst,
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
    /// The ONLY variant pace placement applies to: a birth (and a keyless
    /// request, which mints no pin) ranks candidates by their accounts' usage
    /// readings.
    #[default]
    StickyLeastLoaded,
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
    /// Retry-attempts cap per provider in the chain for the five health
    /// classes (rate-limited, server-error, overloaded, timeout,
    /// network-error); the other classes never retry on the same
    /// provider. A `[retry.classes]` count overrides it per class.
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

    /// End-to-end per-attempt timeout on complete and count-tokens
    /// requests. The router wraps each such upstream call in
    /// `tokio::time::timeout` and treats expiry as a network error
    /// (status 0, retryable). A stream that has committed content is not
    /// bounded by it. The workspace-wide tier: `[providers.X]` and
    /// `[pools.X]` values win over this one. None here means inherit the
    /// shipped 30 min default, not "no timeout".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// First-content timeout for streaming responses. If the upstream
    /// hasn't emitted a content-bearing chunk in this window, the stream
    /// is abandoned and (if no content has been delivered yet) the next
    /// provider in the chain is tried. Content-free leading chunks (a
    /// `delta.role` opener, id/model metadata) neither reset nor satisfy
    /// this timeout.
    ///
    /// Precedence: `[models.X]` > `[providers.X]` > `[pools.X]` > this
    /// global > the shipped 5 min default. None here means inherit the
    /// default, not "no timeout".
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

    /// Same-target retry count per failure class, written
    /// `[retry.classes] server-error = 2`. A class named here takes this
    /// count as its retry cap in place of its built-in one; a class not
    /// named keeps the built-in cap. Fallback is never configured here:
    /// every class but an unknown failure walks on. See
    /// [`RetryPolicy::resolved_class`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub classes: BTreeMap<ConfigFailureClass, u32>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_backoff_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            jitter_ms: default_jitter_ms(),
            request_timeout_ms: None,
            stream_first_byte_timeout_ms: None,
            probe_max_tokens: default_probe_max_tokens(),
            max_honored_retry_after_ms: None,
            classes: BTreeMap::new(),
        }
    }
}

impl RetryPolicy {
    /// Maximum attempts a single provider can ever consume regardless
    /// of error class: the max over every cap the per-class resolver can
    /// return, folding the `[retry.classes]` counts, so it never sits
    /// below a class cap the resolver would otherwise honor.
    pub fn hard_retry_cap(&self) -> u32 {
        self.max_attempts
            .max(self.classes.values().copied().max().unwrap_or(0))
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
    3
}

/// Default ceiling on an honored upstream reset hint when
/// `RetryPolicy::max_honored_retry_after_ms` is unset: one hour. Caps
/// how long the circuit breaker will park a provider on a `Retry-After`
/// so a single hint cannot pin a provider out of rotation indefinitely.
const DEFAULT_MAX_HONORED_RETRY_AFTER_MS: u64 = 3_600_000;

const fn default_probe_max_tokens() -> u32 {
    1
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
#[cfg(test)]
#[path = "schema_tests.rs"]
mod schema_tests;

#[cfg(test)]
#[path = "managed_login_tests.rs"]
mod managed_login_tests;
