//! Per-(provider_kind, model) prompt-cache PRICING data module.
//!
//! This is a DATA module: a single-file baked table of cache-break
//! economics multipliers keyed on `(provider_kind, model_glob, tier)`,
//! plus a fallback lookup and a field-level operator TOML override merge.
//!
//! It carries NO decision logic. The break-even cost gate that consumes
//! these rows is a separate, later concern; nothing here touches the live
//! dispatch path. The numbers are the verified June 2026 provider-cache
//! mechanics; cells the research could not resolve from primary vendor
//! docs are baked `verified = false` so the gate that will read them
//! treats them as the conservative sentinel until a live probe confirms.
//!
//! Keying is `(provider_kind, model)` -- not provider alone -- because the
//! read multiplier `rm` is model-dependent WITHIN several providers (Grok,
//! Kimi, DeepSeek, MiniMax). Anthropic and Bedrock additionally key on a
//! 5m-vs-1h TTL `tier`: the same model has distinct write economics at the
//! 5-minute (`wm = 1.25`) and 1-hour (`wm = 2.0`) breakpoints, a per-request
//! choice modeled as data. Anthropic and Bedrock carry per-model
//! trailing-glob rows (one cell per tier) plus a provider catch-all;
//! openai-compat carries per-sub-provider trailing-glob rows (DeepSeek,
//! Grok, Gemini, Kimi, Mistral, Qwen, MiniMax, ...) plus a `"*"` catch-all;
//! openai-responses has a single glob catch-all. Every non-Anthropic /
//! non-Bedrock row is tier-agnostic (`tier = None`) and matches any
//! request. Model matching reuses the alias-glob matcher
//! ([`crate::glob::AliasPattern`]); longest-prefix-wins.
//!
//! Lookup is: exact-or-glob model match within the provider kind -> the
//! provider `"*"` catch-all -> the tier-agnostic sentinel. The requested
//! `tier` defaults to `"5m"` (routectl's auto-emit default and the common
//! case); a tier-agnostic cell matches any tier, a tiered cell matches only
//! its own tier.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::glob::AliasPattern;

/// Staleness horizon: a baked row whose `verified_at` is more than this
/// many days before today triggers a startup WARN (never a panic).
const STALE_AFTER_DAYS: i64 = 90;

/// Seconds in a day, for epoch-day arithmetic off the system clock.
const SECONDS_PER_DAY: i64 = 86_400;

/// Conservative fallback minimum-prefix token count for the sentinel and
/// any provider whose real threshold is unknown. High on purpose: a high
/// `min_prefix_tokens` makes the (later) break-even gate fold the
/// min-prefix guard pessimistically, biasing toward KEEP.
const SENTINEL_MIN_PREFIX_TOKENS: u32 = 4096;

/// One row of prompt-cache economics for a `(provider_kind, model_glob)`
/// cell. Multipliers are relative to the base input price per token.
///
/// `#[non_exhaustive]`: more economics fields (storage-rent shape,
/// per-model convergence priors) are expected later, so construct rows
/// only through the baked table / [`CachePricingRow::sentinel`] /
/// [`CachePricingRow::with_overrides`]; struct-literal syntax is
/// unavailable to external crates.
///
/// `Eq` is deliberately NOT derived: the multipliers are `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CachePricingRow {
    /// Write multiplier: cost to (re)write a cached prefix block, relative
    /// to base input price. `1.0` means no write premium (auto-cachers).
    pub wm: f32,
    /// Read multiplier: cost to read a warm cached prefix block, relative
    /// to base input price (typically ~`0.1`, far deeper for DeepSeek).
    pub rm: f32,
    /// Cache time-to-live in seconds (refresh-on-hit semantics noted in
    /// the source doc; not modeled here).
    pub ttl_seconds: u32,
    /// Minimum prefix-token count below which the upstream stops caching
    /// the prefix entirely. Folded into the break-even guard later.
    pub min_prefix_tokens: u32,
    /// Whether this provider charges per-hour storage rent on a held
    /// cache (Gemini-explicit). Reserved (unused): `false` on every baked
    /// row.
    pub has_storage_rent: bool,
    /// Per-hour storage-rent multiplier when `has_storage_rent`.
    /// Reserved (unused): `0.0` on every baked row.
    pub storage_rent: f32,
    /// Whether the upstream caches automatically (no explicit breakpoint
    /// to place) versus an explicit-breakpoint provider.
    pub auto_cacher: bool,
    /// Whether this cell's multipliers were confirmed against a primary
    /// vendor doc / live probe. `false` cells fall to sentinel treatment
    /// in the consuming gate.
    pub verified: bool,
    /// TTL tier this cell applies to: `Some("5m")` or `Some("1h")` for the
    /// tiered Anthropic / Bedrock rows whose write economics differ by
    /// breakpoint TTL; `None` for every tier-agnostic row (matches any
    /// requested tier). The sentinel is tier-agnostic.
    pub tier: Option<&'static str>,
    /// Verification date as `"YYYY-MM-DD"`. Parsed for the staleness check.
    pub verified_at: &'static str,
    /// Free-form provenance string (vendor doc, `"sentinel"`, etc.).
    pub source: &'static str,
}

impl CachePricingRow {
    /// The conservative SENTINEL row: the most-expensive-to-break shape, so
    /// an unknown / unverified cell forces KEEP at the margin in the
    /// consuming gate. `wm = 2.0` (the 1h-premium write tax), `rm = 0.10`,
    /// 5-minute TTL, a high min-prefix, and `verified = false`.
    pub const fn sentinel() -> Self {
        Self {
            wm: 2.0,
            rm: 0.10,
            ttl_seconds: 300,
            min_prefix_tokens: SENTINEL_MIN_PREFIX_TOKENS,
            has_storage_rent: false,
            storage_rent: 0.0,
            auto_cacher: false,
            verified: false,
            tier: None,
            verified_at: "1970-01-01",
            source: "sentinel",
        }
    }

    /// Merge a field-level operator override onto this baked row. Every
    /// override field is `Option`; `None` inherits the baked value (the
    /// operator restates only the cells they know are wrong).
    ///
    /// RELIABILITY GUARD: an override that sets `wm` BELOW the sentinel's
    /// `wm` (2.0) is rejected unless it also carries
    /// `override_acknowledges_cost_risk = true`. A too-cheap write
    /// multiplier makes a cache break look falsely profitable; the explicit
    /// ack flag is the operator asserting they understand the risk.
    pub fn with_overrides(&self, ov: &CachePricingOverride) -> Result<Self, String> {
        if let Some(wm) = ov.wm {
            if wm < CachePricingRow::sentinel().wm && !ov.override_acknowledges_cost_risk {
                return Err(format!(
                    "cache-pricing override sets wm = {wm} below the conservative sentinel wm = \
                     {}, which can make a cache break look falsely profitable; set \
                     override_acknowledges_cost_risk = true to accept this risk",
                    CachePricingRow::sentinel().wm
                ));
            }
        }
        if let Some(rm) = ov.rm {
            if rm <= 0.0 {
                return Err(format!(
                    "cache_pricing override: rm must be > 0.0 (got {rm}); a zero or negative read \
                     multiplier makes the break-even math degenerate"
                ));
            }
        }
        Ok(Self {
            wm: ov.wm.unwrap_or(self.wm),
            rm: ov.rm.unwrap_or(self.rm),
            ttl_seconds: ov.ttl_seconds.unwrap_or(self.ttl_seconds),
            min_prefix_tokens: ov.min_prefix_tokens.unwrap_or(self.min_prefix_tokens),
            has_storage_rent: ov.has_storage_rent.unwrap_or(self.has_storage_rent),
            storage_rent: ov.storage_rent.unwrap_or(self.storage_rent),
            auto_cacher: ov.auto_cacher.unwrap_or(self.auto_cacher),
            // An overridden cell is operator-asserted; treat it as verified
            // so the consuming gate trusts it, but keep the provenance
            // string honest.
            verified: true,
            tier: self.tier,
            verified_at: self.verified_at,
            source: "operator-override",
        })
    }
}

/// Field-level operator override for one `(provider_kind, model_glob)`
/// cell, deserialized from a `[cache_pricing]` TOML entry. Every field is
/// optional; an omitted field inherits the baked-in value (see
/// [`CachePricingRow::with_overrides`]).
///
/// `Eq` is deliberately NOT derived: the multipliers are `f32`.
/// `#[serde(deny_unknown_fields)]` rejects typos at config-load time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CachePricingOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wm: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rm: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_prefix_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_storage_rent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_rent: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_cacher: Option<bool>,
    /// Operator's explicit acknowledgement that a below-sentinel `wm` is
    /// intended. Required when `wm` is set below the sentinel; otherwise
    /// the merge is rejected.
    #[serde(default)]
    pub override_acknowledges_cost_risk: bool,
}

/// A parsed `"provider_kind:model_glob"` config-key selector for the
/// `[cache_pricing]` override table. The raw key is split on the FIRST
/// colon so a model glob may itself contain colons (real Bedrock ids do).
/// The override path uses this to apply `Config.cache_pricing` overrides onto baked rows;
/// it is intentionally not wired into a consumer here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePricingSelector {
    pub provider_kind: String,
    pub model_glob: String,
}

impl CachePricingSelector {
    /// Parse a `"provider_kind:model_glob"` selector key, splitting on the
    /// FIRST colon. Rejects a missing colon or an empty provider-kind /
    /// model-glob part with a clear, key-naming error.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (provider_kind, model_glob) = raw.split_once(':').ok_or_else(|| {
            format!(
                "cache-pricing selector `{raw}` is missing a `:`; expected \
                 `provider_kind:model_glob` (e.g. `openai-compat:grok-*`)"
            )
        })?;
        if provider_kind.is_empty() || model_glob.is_empty() {
            return Err(format!(
                "cache-pricing selector `{raw}` has an empty provider_kind or model_glob; \
                 expected `provider_kind:model_glob` (e.g. `openai-compat:grok-*`)"
            ));
        }
        Ok(Self {
            provider_kind: provider_kind.to_string(),
            model_glob: model_glob.to_string(),
        })
    }
}

/// One baked cell: a provider-kind token, a model glob, and its row.
struct BakedCell {
    provider_kind: &'static str,
    model_glob: &'static str,
    row: CachePricingRow,
}

/// Helper to build a baked verified row tersely. Reserved (unused) fields
/// (`has_storage_rent`, `storage_rent`) are always `false` / `0.0`. The
/// argument count is the price of a flat, scannable static table; the
/// helper is private and each call site reads positionally against the
/// doc-defined column order. `tier` is `None` for tier-agnostic rows and
/// `Some("5m")` / `Some("1h")` for the tiered Anthropic / Bedrock cells.
#[allow(clippy::too_many_arguments)]
const fn row(
    wm: f32,
    rm: f32,
    ttl_seconds: u32,
    min_prefix_tokens: u32,
    auto_cacher: bool,
    verified: bool,
    tier: Option<&'static str>,
    verified_at: &'static str,
    source: &'static str,
) -> CachePricingRow {
    CachePricingRow {
        wm,
        rm,
        ttl_seconds,
        min_prefix_tokens,
        has_storage_rent: false,
        storage_rent: 0.0,
        auto_cacher,
        verified,
        tier,
        verified_at,
        source,
    }
}

/// Verification stamp shared by the cells resolved in the 2026-06-24
/// primary-doc fan-out. Edit this when the table is re-verified.
const VERIFIED_AT: &str = "2026-06-24";

/// The baked pricing table. Keyed on `(provider_kind, model_glob)`. The
/// provider-kind tokens are the stable `kind_str()` discriminants
/// (`anthropic-api`, `bedrock`, `openai-responses`, `openai-compat`); the
/// openai-compat sub-providers are model_glob rows under `openai-compat`.
///
/// Cells the source doc marks `[PROBE]` / UNKNOWN / unverified are baked
/// `verified = false` (Grok, Kimi, Mistral, the openai-compat catch-all)
/// so the consuming gate treats them as sentinel for live cuts. The
/// `verified = true` cells carry the doc's exact multipliers.
const TABLE: &[BakedCell] = &[
    // -- Anthropic (explicit; 5m default ephemeral + 1h GA) ---------------
    // min-prefix is model-dependent: 1024 for Opus 4.8 / Sonnet 4.6 / 4.5,
    // 2048 for Haiku 3.5 / Opus 4.7, 4096 for Opus 4.6 / 4.5 / Haiku 4.5.
    // Each per-model row exists at both the 5m tier (wm=1.25, ttl=300) and
    // the 1h tier (wm=2.0, ttl=3600); 1h is GA and verified.
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-8*",
        row: row(
            1.25,
            0.10,
            300,
            1024,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-8*",
        row: row(
            2.0,
            0.10,
            3_600,
            1024,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-sonnet-4-6*",
        row: row(
            1.25,
            0.10,
            300,
            1024,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-sonnet-4-6*",
        row: row(
            2.0,
            0.10,
            3_600,
            1024,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-sonnet-4-5*",
        row: row(
            1.25,
            0.10,
            300,
            1024,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-sonnet-4-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            1024,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-haiku-3-5*",
        row: row(
            1.25,
            0.10,
            300,
            2048,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-haiku-3-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            2048,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-7*",
        row: row(
            1.25,
            0.10,
            300,
            2048,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-7*",
        row: row(
            2.0,
            0.10,
            3_600,
            2048,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-6*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-6*",
        row: row(
            2.0,
            0.10,
            3_600,
            4096,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-5*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-opus-4-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            4096,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-haiku-4-5*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            true,
            Some("5m"),
            VERIFIED_AT,
            "anthropic-5m",
        ),
    },
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "claude-haiku-4-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            4096,
            false,
            true,
            Some("1h"),
            VERIFIED_AT,
            "anthropic-1h",
        ),
    },
    // Anthropic provider-level catch-all: tier-agnostic so it backstops a
    // request at either TTL. The 5m write premium, conservative 4096
    // min-prefix. Verified shape, but a generic model id.
    BakedCell {
        provider_kind: "anthropic-api",
        model_glob: "*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            true,
            None,
            VERIFIED_AT,
            "anthropic-5m-default",
        ),
    },
    // -- Bedrock (Claude via cachePoint) ----------------------------------
    // Wm 1.25 (5m) / 2.0 (1h); rm ~0.1 PROBE on the pricing page (not yet
    // stamped) -> verified=false. Real Bedrock ids carry a vendor prefix
    // (anthropic.claude-...), so globs are trailing-only. 1h applies ONLY
    // to the 4.5-class (Opus 4.5 / Sonnet 4.5 / Haiku 4.5); Sonnet 4.6 is
    // 5m-only. min-prefix: 4096 for 4.5-class; 1024 for Sonnet 4.6.
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-sonnet-4-6*",
        row: row(
            1.25,
            0.10,
            300,
            1024,
            false,
            false,
            Some("5m"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-sonnet-4-5*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            false,
            Some("5m"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-sonnet-4-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            4096,
            false,
            false,
            Some("1h"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-haiku-4-5*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            false,
            Some("5m"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-haiku-4-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            4096,
            false,
            false,
            Some("1h"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-opus-4-5*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            false,
            Some("5m"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "anthropic.claude-opus-4-5*",
        row: row(
            2.0,
            0.10,
            3_600,
            4096,
            false,
            false,
            Some("1h"),
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    BakedCell {
        provider_kind: "bedrock",
        model_glob: "*",
        row: row(
            1.25,
            0.10,
            300,
            4096,
            false,
            false,
            None,
            VERIFIED_AT,
            "bedrock-probe-rm",
        ),
    },
    // -- OpenAI Responses (automatic prefix, no write premium) ------------
    // 24h DEFAULT retention on GPT-5.5+, free writes (Wm 1.0), rm 0.10.
    BakedCell {
        provider_kind: "openai-responses",
        model_glob: "*",
        row: row(
            1.0,
            0.10,
            86_400,
            1024,
            true,
            true,
            None,
            VERIFIED_AT,
            "openai-24h",
        ),
    },
    // -- openai-compat sub-providers (model_glob rows) --------------------
    // DeepSeek: automatic disk-KV prefix, free writes, very deep reads.
    // V4-Pro rm ~0.0083; V4-Flash rm ~0.02; min-prefix from token 0.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "deepseek-v4-pro*",
        row: row(
            1.0,
            0.0083,
            3_600,
            1,
            true,
            true,
            None,
            VERIFIED_AT,
            "deepseek-v4-pro",
        ),
    },
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "deepseek-*",
        row: row(
            1.0,
            0.02,
            3_600,
            1,
            true,
            true,
            None,
            VERIFIED_AT,
            "deepseek-v4-flash",
        ),
    },
    // Gemini implicit: automatic prefix, free writes, rm ~0.10. min-prefix
    // 2048 (2.5) / 4096 (3.1-Pro, 3.5-Flash) -> conservative 4096.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "gemini-*",
        row: row(
            1.0,
            0.10,
            300,
            4096,
            true,
            true,
            None,
            VERIFIED_AT,
            "gemini-implicit",
        ),
    },
    // Mistral: explicit-keyed prefix; caller must supply prompt_cache_key,
    // not automatic -> auto_cacher=false. Free writes, rm 0.10, 64-token
    // block. TTL UNKNOWN [PROBE] -> verified=false.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "mistral-*",
        row: row(
            1.0,
            0.10,
            300,
            64,
            false,
            false,
            None,
            VERIFIED_AT,
            "mistral-probe-ttl",
        ),
    },
    // xAI Grok: automatic prefix, free writes, rm model-dependent
    // (0.05-0.16). min-prefix + TTL UNKNOWN [PROBE] -> verified=false.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "grok-*",
        row: row(
            1.0,
            0.16,
            300,
            4096,
            true,
            false,
            None,
            VERIFIED_AT,
            "grok-probe",
        ),
    },
    // Moonshot Kimi: hybrid auto/explicit, free auto writes, rm ~0.16-0.20.
    // min-prefix + explicit ttl bounds UNKNOWN [PROBE] -> verified=false.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "kimi-*",
        row: row(
            1.0,
            0.20,
            300,
            4096,
            true,
            false,
            None,
            VERIFIED_AT,
            "kimi-probe",
        ),
    },
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "moonshot-*",
        row: row(
            1.0,
            0.20,
            300,
            4096,
            true,
            false,
            None,
            VERIFIED_AT,
            "kimi-probe",
        ),
    },
    // Qwen explicit: explicit cache_control ephemeral, Wm 1.25, rm 0.10,
    // 5-min TTL, 1024 explicit min-prefix.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "qwen-*",
        row: row(
            1.25,
            0.10,
            300,
            1024,
            false,
            true,
            None,
            VERIFIED_AT,
            "qwen-explicit",
        ),
    },
    // MiniMax M3 (flagship): passive auto, FREE writes, rm 0.2, 512 prefix.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "minimax-m3*",
        row: row(
            1.0,
            0.20,
            300,
            512,
            true,
            true,
            None,
            VERIFIED_AT,
            "minimax-m3",
        ),
    },
    // MiniMax 2.7 / 2.5 snapshots: passive + explicit, wm 1.25, rm 0.2, 512 prefix.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "minimax-*",
        row: row(
            1.25,
            0.20,
            300,
            512,
            false,
            true,
            None,
            VERIFIED_AT,
            "minimax-m2",
        ),
    },
    // openai-compat catch-all: unknown OpenAI-compatible upstream. The shape
    // varies wildly across pass-through gateways (OpenRouter, OpenCode Zen,
    // Fireworks, self-host vLLM/NIM/llama.cpp), so this stays unverified ->
    // the consuming gate falls through to the sentinel for live cuts.
    BakedCell {
        provider_kind: "openai-compat",
        model_glob: "*",
        row: row(
            1.0,
            0.10,
            300,
            SENTINEL_MIN_PREFIX_TOKENS,
            true,
            false,
            None,
            VERIFIED_AT,
            "openai-compat-default",
        ),
    },
];

/// Look up the pricing row for a `(provider_kind, model, tier)` triple.
///
/// `tier` is the requested TTL tier (`Some("5m")` / `Some("1h")`);
/// `None` resolves to the `"5m"` default (routectl's auto-emit default and
/// the common case). A tier-agnostic baked cell (`cell.tier == None`)
/// matches any request; a tiered cell matches only when its tier equals the
/// resolved `want`.
///
/// Three-tier fallback, every miss converging on the conservative
/// sentinel:
///   1. an exact-or-glob model match under the given `provider_kind` and
///      tier (longest matching prefix wins);
///   2. the `provider_kind` `"*"` catch-all row (tier-agnostic);
///   3. [`CachePricingRow::sentinel`] (tier-agnostic).
///
/// Model matching reuses [`AliasPattern`] (the alias-table glob matcher);
/// the `"*"` provider catch-all is handled directly, not through the
/// matcher (which rejects a bare `*`).
pub fn lookup(provider_kind: &str, model: &str, tier: Option<&str>) -> CachePricingRow {
    let want = tier.unwrap_or("5m");

    // Tier 1: longest-prefix model match within this provider kind and the
    // requested tier. A literal `"*"` glob is the provider catch-all
    // (tier 2), excluded here. A tier-agnostic cell matches any `want`.
    let best = TABLE
        .iter()
        .filter(|cell| {
            cell.provider_kind == provider_kind
                && cell.model_glob != "*"
                && match cell.row.tier {
                    Some(t) => t == want,
                    None => true,
                }
        })
        .filter_map(|cell| match AliasPattern::parse(cell.model_glob) {
            Ok(pat) if pat.matches(model) => Some((pat.prefix_len(), cell.row)),
            _ => None,
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, r)| r);
    if let Some(r) = best {
        return r;
    }

    // Tier 2: provider-kind catch-all (tier-agnostic).
    if let Some(cell) = TABLE
        .iter()
        .find(|cell| cell.provider_kind == provider_kind && cell.model_glob == "*")
    {
        return cell.row;
    }

    // Tier 3: conservative sentinel.
    CachePricingRow::sentinel()
}

/// Today's date as a proleptic-Gregorian epoch-day count (days since
/// 1970-01-01), derived from the system clock. Pure arithmetic, no date
/// library. Returns `0` if the clock is somehow before the epoch.
fn today_epoch_day() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0) / SECONDS_PER_DAY)
        .unwrap_or(0)
}

/// Parse a `"YYYY-MM-DD"` string into a proleptic-Gregorian epoch-day
/// count (days since 1970-01-01). Returns `None` on a malformed string.
/// Pure arithmetic; mirrors the civil-from-days algorithm in reverse.
fn parse_epoch_day(date: &str) -> Option<i64> {
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days-from-civil algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

/// True when a `verified_at` date is more than [`STALE_AFTER_DAYS`] before
/// `today` (both epoch-days). A row whose date fails to parse is treated
/// as stale so a malformed stamp surfaces rather than hides.
fn is_stale(verified_at: &str, today: i64) -> bool {
    match parse_epoch_day(verified_at) {
        Some(day) => today - day > STALE_AFTER_DAYS,
        None => true,
    }
}

/// Emit a `tracing::warn!` for every baked, `verified` row whose
/// `verified_at` is more than 90 days stale. Never panics. Called once at
/// startup. Unverified rows are skipped -- they are already sentinel-
/// treated, so their date carries no economic weight.
pub fn warn_if_stale() {
    warn_if_stale_at(today_epoch_day());
}

/// Testable core of [`warn_if_stale`]: takes "today" as an epoch-day so a
/// test can pin a deterministic clock.
fn warn_if_stale_at(today: i64) {
    for cell in TABLE {
        if cell.row.verified && is_stale(cell.row.verified_at, today) {
            tracing::warn!(
                provider_kind = cell.provider_kind,
                model_glob = cell.model_glob,
                verified_at = cell.row.verified_at,
                stale_after_days = STALE_AFTER_DAYS,
                "cache-pricing row is stale; re-verify the multipliers against the vendor doc",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_exact_model_row_when_present() {
        // Arrange / Act
        let r = lookup("anthropic-api", "claude-opus-4-8", None);

        // Assert: the Opus 4.8 5m cell (1024 min-prefix), not the 4096
        // provider catch-all.
        assert_eq!(r.wm, 1.25);
        assert_eq!(r.rm, 0.10);
        assert_eq!(r.ttl_seconds, 300);
        assert_eq!(r.min_prefix_tokens, 1024);
        assert!(r.verified);
    }

    #[test]
    fn lookup_falls_back_to_provider_catch_all_row() {
        // Arrange / Act: an Anthropic model with no specific cell.
        let r = lookup("anthropic-api", "claude-future-9-9", None);

        // Assert: the anthropic-api "*" row (4096 min-prefix default).
        assert_eq!(r.wm, 1.25);
        assert_eq!(r.min_prefix_tokens, 4096);
        assert_eq!(r.source, "anthropic-5m-default");
        assert!(r.verified);
    }

    #[test]
    fn lookup_unknown_provider_and_model_returns_sentinel() {
        // Arrange / Act
        let r = lookup("some-future-kind", "whatever-model", None);

        // Assert: conservative sentinel.
        assert_eq!(r.wm, 2.0);
        assert_eq!(r.rm, 0.10);
        assert_eq!(r.ttl_seconds, 300);
        assert_eq!(r.min_prefix_tokens, SENTINEL_MIN_PREFIX_TOKENS);
        assert!(!r.verified);
        assert_eq!(r.source, "sentinel");
    }

    #[test]
    fn sentinel_has_the_documented_conservative_shape() {
        // Arrange / Act
        let s = CachePricingRow::sentinel();

        // Assert
        assert_eq!(s.wm, 2.0);
        assert_eq!(s.rm, 0.10);
        assert_eq!(s.ttl_seconds, 300);
        assert!(!s.verified);
        assert!(!s.auto_cacher);
    }

    #[test]
    fn verified_anthropic_5m_loads_exact_multipliers() {
        let r = lookup("anthropic-api", "claude-sonnet-4-6", None);
        assert_eq!(r.wm, 1.25);
        assert_eq!(r.rm, 0.10);
        assert_eq!(r.ttl_seconds, 300);
        assert!(r.verified);
    }

    #[test]
    fn verified_openai_loads_24h_ttl_and_free_writes() {
        let r = lookup("openai-responses", "gpt-5.5", None);
        assert_eq!(r.wm, 1.0);
        assert_eq!(r.rm, 0.10);
        assert_eq!(r.ttl_seconds, 86_400);
        assert!(r.auto_cacher);
        assert!(r.verified);
    }

    #[test]
    fn verified_deepseek_loads_deep_read_multiplier() {
        // V4-Pro: the deepest read discount.
        let pro = lookup("openai-compat", "deepseek-v4-pro", None);
        assert_eq!(pro.wm, 1.0);
        assert_eq!(pro.rm, 0.0083);
        assert!(pro.verified);

        // V4 (non-pro) falls to the flash row via the broader glob.
        let flash = lookup("openai-compat", "deepseek-v4-flash", None);
        assert_eq!(flash.rm, 0.02);
        assert!(flash.verified);
    }

    #[test]
    fn unverified_probe_cell_loads_with_verified_false() {
        // Grok is a [PROBE] cell: researched shape, but not stamped.
        let r = lookup("openai-compat", "grok-4-3", None);
        assert!(!r.verified);
        assert_eq!(r.source, "grok-probe");
    }

    #[test]
    fn override_inherits_unset_fields_from_baked_row() {
        // Arrange: override only ttl_seconds; baked Anthropic 5m row. wm is
        // left unset so the below-sentinel guard is not in play here.
        let baked = lookup("anthropic-api", "claude-opus-4-8", None);
        let ov = CachePricingOverride {
            ttl_seconds: Some(3_600),
            ..Default::default()
        };

        // Act
        let merged = baked.with_overrides(&ov).expect("accepted");

        // Assert: ttl overridden; wm / rm / min_prefix inherited (None).
        assert_eq!(merged.ttl_seconds, 3_600);
        assert_eq!(merged.wm, baked.wm);
        assert_eq!(merged.rm, baked.rm);
        assert_eq!(merged.min_prefix_tokens, baked.min_prefix_tokens);
        assert_eq!(merged.has_storage_rent, baked.has_storage_rent);
        assert_eq!(merged.source, "operator-override");
    }

    #[test]
    fn override_below_sentinel_wm_without_ack_is_rejected() {
        // Arrange: wm below the sentinel's 2.0, no ack flag.
        let baked = lookup("anthropic-api", "claude-opus-4-8", None);
        let ov = CachePricingOverride {
            wm: Some(1.0),
            ..Default::default()
        };

        // Act
        let result = baked.with_overrides(&ov);

        // Assert: rejected with a clear error.
        let err = result.expect_err("must reject below-sentinel wm without ack");
        assert!(
            err.contains("override_acknowledges_cost_risk"),
            "msg: {err}"
        );
    }

    #[test]
    fn override_below_sentinel_wm_with_ack_is_accepted() {
        // Arrange: wm below the sentinel's 2.0, ack flag set.
        let baked = lookup("anthropic-api", "claude-opus-4-8", None);
        let ov = CachePricingOverride {
            wm: Some(1.0),
            override_acknowledges_cost_risk: true,
            ..Default::default()
        };

        // Act
        let merged = baked.with_overrides(&ov).expect("accepted with ack");

        // Assert
        assert_eq!(merged.wm, 1.0);
    }

    #[test]
    fn override_at_or_above_sentinel_wm_needs_no_ack() {
        let baked = lookup("anthropic-api", "claude-opus-4-8", None);
        let ov = CachePricingOverride {
            wm: Some(2.0),
            ..Default::default()
        };
        assert!(baked.with_overrides(&ov).is_ok());
    }

    #[test]
    fn override_rejects_non_positive_rm() {
        // Arrange: a zero read multiplier is never valid -- it makes the
        // break-even math degenerate and could flip verified=true on a bogus
        // row. Rejected unconditionally (no ack flag exempts it).
        let baked = lookup("anthropic-api", "claude-opus-4-8", None);

        // Act / Assert: rm == 0.0 is rejected.
        let zero = CachePricingOverride {
            rm: Some(0.0),
            ..Default::default()
        };
        let err = baked
            .with_overrides(&zero)
            .expect_err("must reject rm == 0.0");
        assert!(err.contains("rm must be > 0.0"), "msg: {err}");

        // Act / Assert: a negative rm is rejected even with the cost-risk ack.
        let negative = CachePricingOverride {
            rm: Some(-0.1),
            override_acknowledges_cost_risk: true,
            ..Default::default()
        };
        assert!(
            baked.with_overrides(&negative).is_err(),
            "must reject negative rm regardless of ack",
        );
    }

    #[test]
    fn model_glob_longest_prefix_wins_for_overlapping_globs() {
        // deepseek-v4-pro* (longer literal prefix) must beat the broad
        // deepseek-* row for a pro model id.
        let r = lookup("openai-compat", "deepseek-v4-pro-0610", None);
        assert_eq!(r.rm, 0.0083, "the deepseek pro glob must win");

        // A non-pro deepseek id falls to the broader deepseek-* row.
        let flash = lookup("openai-compat", "deepseek-v4-0610", None);
        assert_eq!(flash.rm, 0.02, "the broad deepseek glob handles non-pro");
    }

    #[test]
    fn staleness_warn_fires_for_a_synthetically_stale_date() {
        // Arrange: a fixed "today" 200 days after a known stamp.
        let stamp = parse_epoch_day("2026-01-01").expect("parse");
        let today = stamp + 200;

        // Assert: the row is stale (> 90 days).
        assert!(is_stale("2026-01-01", today));
    }

    #[test]
    fn staleness_does_not_fire_for_a_fresh_date() {
        // Arrange: a fixed "today" 10 days after the stamp.
        let stamp = parse_epoch_day("2026-06-14").expect("parse");
        let today = stamp + 10;

        // Assert: fresh (within 90 days).
        assert!(!is_stale("2026-06-14", today));
    }

    #[test]
    fn malformed_verified_at_is_treated_as_stale() {
        assert!(is_stale("not-a-date", 20_000));
    }

    #[test]
    fn staleness_boundary_exactly_90_days_is_not_stale() {
        // The comparison is strict `>`, so exactly STALE_AFTER_DAYS old is
        // still fresh; the day after is stale.
        let stamp = parse_epoch_day("2026-01-01").expect("parse");
        assert!(!is_stale("2026-01-01", stamp + STALE_AFTER_DAYS));
        assert!(is_stale("2026-01-01", stamp + STALE_AFTER_DAYS + 1));
    }

    #[test]
    fn bedrock_real_model_id_matches_trailing_glob() {
        // Real Bedrock ids carry a vendor prefix; the trailing-glob row must
        // match (the old leading-wildcard glob was rejected and silently
        // dropped, falling through to the catch-all).
        let r = lookup(
            "bedrock",
            "anthropic.claude-sonnet-4-6-20260401-v1:0",
            Some("5m"),
        );
        assert_eq!(r.min_prefix_tokens, 1024);
        assert_eq!(r.source, "bedrock-probe-rm");
        assert!(!r.verified);
    }

    #[test]
    fn anthropic_tier_selects_5m_vs_1h_write_multiplier() {
        // 5m tier: wm 1.25; 1h tier: wm 2.0; None defaults to the 5m row.
        let five_min = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        assert_eq!(five_min.wm, 1.25);
        assert_eq!(five_min.ttl_seconds, 300);
        assert_eq!(five_min.tier, Some("5m"));

        let one_hour = lookup("anthropic-api", "claude-opus-4-8", Some("1h"));
        assert_eq!(one_hour.wm, 2.0);
        assert_eq!(one_hour.ttl_seconds, 3_600);
        assert_eq!(one_hour.tier, Some("1h"));

        let defaulted = lookup("anthropic-api", "claude-opus-4-8", None);
        assert_eq!(defaulted.wm, 1.25);
        assert_eq!(defaulted.tier, Some("5m"));
    }

    #[test]
    fn selector_parse_splits_on_first_colon() {
        let s = CachePricingSelector::parse("openai-compat:grok-*").expect("parse");
        assert_eq!(s.provider_kind, "openai-compat");
        assert_eq!(s.model_glob, "grok-*");

        // A model glob may itself contain colons (real Bedrock ids do); only
        // the FIRST colon splits.
        let b =
            CachePricingSelector::parse("bedrock:anthropic.claude-sonnet-4-6-v1:0").expect("parse");
        assert_eq!(b.provider_kind, "bedrock");
        assert_eq!(b.model_glob, "anthropic.claude-sonnet-4-6-v1:0");
    }

    #[test]
    fn selector_parse_rejects_missing_colon() {
        let err = CachePricingSelector::parse("openai-compat-grok")
            .expect_err("must reject a key with no colon");
        assert!(err.contains("missing a `:`"), "msg: {err}");
    }

    #[test]
    fn selector_parse_rejects_empty_part() {
        let empty_kind =
            CachePricingSelector::parse(":grok-*").expect_err("must reject an empty provider_kind");
        assert!(empty_kind.contains("empty"), "msg: {empty_kind}");

        let empty_glob = CachePricingSelector::parse("openai-compat:")
            .expect_err("must reject an empty model_glob");
        assert!(empty_glob.contains("empty"), "msg: {empty_glob}");
    }

    #[test]
    fn warn_if_stale_does_not_panic() {
        // Smoke test: the startup hook runs over the real table without
        // panicking, at a deterministic "today" near the baked stamp.
        let today = parse_epoch_day(VERIFIED_AT).expect("parse VERIFIED_AT");
        warn_if_stale_at(today);
    }

    #[test]
    fn parse_epoch_day_round_trips_known_dates() {
        // 1970-01-01 is epoch day 0; 1970-01-02 is day 1.
        assert_eq!(parse_epoch_day("1970-01-01"), Some(0));
        assert_eq!(parse_epoch_day("1970-01-02"), Some(1));
        // A full year later.
        assert_eq!(parse_epoch_day("1971-01-01"), Some(365));
    }

    #[test]
    fn pricing_row_reserved_fields_are_zeroed_on_baked_rows() {
        for cell in TABLE {
            assert!(
                !cell.row.has_storage_rent,
                "{} {} must not set has_storage_rent (reserved; kept zero in the baked table)",
                cell.provider_kind, cell.model_glob,
            );
            assert_eq!(
                cell.row.storage_rent, 0.0,
                "{} {} must keep storage_rent zero in the baked table",
                cell.provider_kind, cell.model_glob,
            );
        }
    }
}
