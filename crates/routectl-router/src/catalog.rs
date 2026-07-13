//! Per-(provider_kind, model) catalog data module: the baked reference
//! table for prompt-cache economics, context window, and capability
//! priors, plus the two-layer merge with the on-disk overlay
//! ([`crate::catalog_overlay`]).
//!
//! TWO LAYERS: layer 1 is this module's baked
//! table, compiled into the binary; layer 2 is [`crate::catalog_overlay`]'s
//! `catalog_overlay.json`. [`merge`] combines a baked row with its overlay
//! cell into an [`EffectiveRow`] -- the overlay wins over baked, JSON
//! `null` disables the entry, and provenance (`source` + `verified_at`) is
//! carried ONLY on the merge result, never stored on [`CatalogRow`]
//! itself.
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
//! provider `"*"` catch-all -> the conservative sentinel (only via
//! [`lookup`] / [`lookup_with_overrides`]; the two-layer merge uses
//! [`lookup_baked_with_overrides`], which reports a genuine catalog miss as
//! `None` instead of synthesizing a sentinel row). The requested `tier`
//! defaults to `"5m"` (routectl's auto-emit default and the common case); a
//! tier-agnostic cell matches any tier, a tiered cell matches only its own
//! tier.
//!
//! GENERATED BAKED TABLE: [`TABLE`] is populated at startup from
//! [`crate::catalog_baked::baked_cells`], the output of
//! `cargo run --bin gen_catalog` (see [`crate::catalog_codegen`] for the
//! derivation from vendored models.dev + litellm snapshots). Every
//! baked-matched row is treated as equally trustworthy
//! (`EffectiveRow::Present { source: Baked, .. }`); the old per-row
//! `verified = false` "PROBE" flag is gone along with the row fields it
//! lived on.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::catalog_overlay::{CatalogOverlay, OverlayCell, OverlaySource};
use crate::glob::AliasPattern;

/// Staleness horizon: a baked row whose snapshot date is more than this
/// many days before today triggers a startup WARN (never a panic).
const STALE_AFTER_DAYS: i64 = 90;

/// Seconds in a day, for epoch-day arithmetic off the system clock.
const SECONDS_PER_DAY: i64 = 86_400;

/// Conservative fallback minimum-prefix token count for the sentinel and
/// any provider whose real threshold is unknown. High on purpose: a high
/// `min_prefix_tokens` makes the (later) break-even gate fold the
/// min-prefix guard pessimistically, biasing toward KEEP.
pub(crate) const SENTINEL_MIN_PREFIX_TOKENS: u32 = 4096;

/// Snapshot date for the WHOLE baked table (per-row `verified_at` left the
/// row; a baked-sourced [`EffectiveRow::Present`] stamps this table-wide
/// date). Forwards [`crate::catalog_baked::CATALOG_SNAPSHOT_DATE`] -- the
/// generated file's display-only const -- under the name the rest of this
/// module already uses.
const BAKED_SNAPSHOT_DATE: &str = crate::catalog_baked::CATALOG_SNAPSHOT_DATE;

/// One row of prompt-cache economics, context window, and capability
/// priors for a `(provider_kind, model_glob)` cell. Multipliers are
/// relative to the base input price per token.
///
/// `#[non_exhaustive]`: more economics fields (storage-rent shape,
/// per-model convergence priors) are expected later, so construct rows
/// only through the baked table / [`CatalogRow::sentinel`] /
/// [`CatalogRow::with_overrides`]; struct-literal syntax is unavailable to
/// external crates.
///
/// Carries NO provenance (`verified` / `source` / `verified_at` LEFT the
/// row): a row is pure economics + capability data, and provenance is
/// a property of the two-layer merge result ([`EffectiveRow`]), never of
/// the row itself. `Copy` is deliberately NOT derived: `capabilities` is a
/// `BTreeMap`. `Eq` is deliberately NOT derived: the multipliers are `f32`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CatalogRow {
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
    /// TTL tier this cell applies to: `Some("5m")` or `Some("1h")` for the
    /// tiered Anthropic / Bedrock rows whose write economics differ by
    /// breakpoint TTL; `None` for every tier-agnostic row (matches any
    /// requested tier). The sentinel is tier-agnostic.
    pub tier: Option<&'static str>,
    /// The model's total context window in tokens, when confirmed against a
    /// primary vendor doc for this exact `(provider_kind, model_glob)` cell.
    /// `None` (fail-closed) when the window could not be confirmed -- e.g. a
    /// bare `"*"` catch-all that can match models with genuinely different
    /// windows, or a model/family whose window is not stated as an exact
    /// figure in current vendor docs. A `None` here is the correct, safe
    /// answer; a guessed window would be a silent data error downstream
    /// (`context_fraction`, deferred to the display work).
    pub max_context_tokens: Option<u32>,
    /// Capability priors keyed on the well-known namespace
    /// (`routectl_core::capability`). Absent key = NO PRIOR (distinct from
    /// `Some(false)`, an asserted absence). Empty on every transitional
    /// baked row (a later codegen pass populates this); an overlay cell can
    /// still set entries via [`crate::catalog_overlay::OverlayCell::capabilities`].
    pub capabilities: BTreeMap<String, bool>,
}

impl CatalogRow {
    /// The conservative SENTINEL row: the most-expensive-to-break shape, so
    /// an unknown / unpriced cell forces KEEP at the margin in the
    /// consuming gate. `wm = 2.0` (the 1h-premium write tax), `rm = 0.10`,
    /// 5-minute TTL, and a high min-prefix.
    #[must_use]
    pub const fn sentinel() -> Self {
        Self {
            wm: 2.0,
            rm: 0.10,
            ttl_seconds: 300,
            min_prefix_tokens: SENTINEL_MIN_PREFIX_TOKENS,
            has_storage_rent: false,
            storage_rent: 0.0,
            auto_cacher: false,
            tier: None,
            max_context_tokens: None,
            capabilities: BTreeMap::new(),
        }
    }

    /// Merge a field-level LEGACY `[cache_pricing]` operator override onto
    /// this row. Every override field is `Option`; `None` inherits this
    /// row's value (the operator restates only the cells they know are
    /// wrong).
    ///
    /// RELIABILITY GUARD: a degenerate override is rejected up front via
    /// [`CachePricingOverride::validate`] (a below-sentinel `wm` without the
    /// cost-risk ack, or a non-positive `rm`).
    ///
    /// PURE VALUE MERGE ONLY: this no longer stamps `verified` / `source`
    /// (this redesign killed the hardcoded provenance stamp -- provenance now comes
    /// exclusively from the two-layer merge result, [`EffectiveRow`]).
    /// `ov.verified_at` is still format-validated but has no row field to
    /// land on; a later legacy-config migration reads it directly off the
    /// raw override.
    pub fn with_overrides(&self, ov: &CachePricingOverride) -> Result<Self, String> {
        ov.validate()?;
        Ok(Self {
            wm: ov.wm.unwrap_or(self.wm),
            rm: ov.rm.unwrap_or(self.rm),
            ttl_seconds: ov.ttl_seconds.unwrap_or(self.ttl_seconds),
            min_prefix_tokens: ov.min_prefix_tokens.unwrap_or(self.min_prefix_tokens),
            has_storage_rent: ov.has_storage_rent.unwrap_or(self.has_storage_rent),
            storage_rent: ov.storage_rent.unwrap_or(self.storage_rent),
            auto_cacher: ov.auto_cacher.unwrap_or(self.auto_cacher),
            tier: self.tier,
            max_context_tokens: ov.max_context_tokens.or(self.max_context_tokens),
            capabilities: self.capabilities.clone(),
        })
    }
}

/// Field-level operator override for one `(provider_kind, model_glob)`
/// cell, deserialized from a LEGACY `[cache_pricing]` TOML entry (retired
/// in a later increment). Every
/// field is optional; an omitted field inherits the baked-in value (see
/// [`CatalogRow::with_overrides`]).
///
/// `Eq` is deliberately NOT derived: the multipliers are `f32`.
/// `#[serde(deny_unknown_fields)]` rejects typos at config-load time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Operator-supplied verification date (`"YYYY-MM-DD"`). Format-
    /// validated but no longer stamped onto the merged row (provenance
    /// lives on [`EffectiveRow`], not on [`CatalogRow`]). Retained so the
    /// legacy-config migration can read it when building an overlay
    /// candidate from an existing `[cache_pricing]` entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    /// Operator's explicit acknowledgement that a below-sentinel `wm` is
    /// intended. Required when `wm` is set below the sentinel; otherwise
    /// the merge is rejected.
    #[serde(default)]
    pub override_acknowledges_cost_risk: bool,
    /// Operator-supplied context window in tokens. `Some` wins over the
    /// baked window (or the baked `None`); `None` inherits the baked value
    /// unchanged. Set this when the baked table's `None` for a cell is
    /// wrong -- e.g. the operator has confirmed the vendor's real window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
}

impl CachePricingOverride {
    /// Reject a degenerate override before it is merged onto a baked row.
    ///
    /// RELIABILITY GUARD: a `wm` BELOW the sentinel's `wm` (2.0) is rejected
    /// unless `override_acknowledges_cost_risk = true` -- a too-cheap write
    /// multiplier makes a cache break look falsely profitable. A non-positive
    /// `rm` is rejected unconditionally (the ack flag does not exempt it): a
    /// zero or negative read multiplier makes the break-even math degenerate.
    /// A `verified_at` value that does not parse as `YYYY-MM-DD` is rejected
    /// so a malformed stamp fails fast at startup rather than silently going
    /// wrong later.
    /// Shared by the merge path ([`CatalogRow::with_overrides`]) and the
    /// startup validate-only pass ([`validate_overrides`]).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(wm) = self.wm
            && wm < CatalogRow::sentinel().wm
            && !self.override_acknowledges_cost_risk
        {
            return Err(format!(
                "cache-pricing override sets wm = {wm} below the conservative sentinel wm = \
                     {}, which can make a cache break look falsely profitable; set \
                     override_acknowledges_cost_risk = true to accept this risk",
                CatalogRow::sentinel().wm
            ));
        }
        if let Some(rm) = self.rm
            && rm <= 0.0
        {
            return Err(format!(
                "cache_pricing override: rm must be > 0.0 (got {rm}); a zero or negative read \
                     multiplier makes the break-even math degenerate"
            ));
        }
        if let Some(s) = &self.verified_at
            && parse_epoch_day(s).is_none()
        {
            return Err(format!(
                "cache-pricing override: verified_at = \"{s}\" is not a valid YYYY-MM-DD date"
            ));
        }
        if self.max_context_tokens == Some(0) {
            return Err(
                "cache-pricing override: max_context_tokens must not be Some(0); use None to \
                 leave the window unconfirmed"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// A parsed `"provider_kind:model_glob"` config-key selector for the
/// `[cache_pricing]` override table. The raw key is split on the FIRST
/// colon so a model glob may itself contain colons (real Bedrock ids do).
/// [`best_override`] uses this to apply `Config.cache_pricing` overrides
/// onto baked rows during [`lookup_with_overrides`] /
/// [`lookup_baked_with_overrides`].
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
    row: CatalogRow,
}

/// The baked catalog table. Keyed on `(provider_kind, model_glob)`. The
/// provider-kind tokens are the stable `kind_str()` discriminants
/// (`anthropic-api`, `bedrock`, `openai-responses`, `openai-compat`); the
/// openai-compat sub-providers are model_glob rows under `openai-compat`.
///
/// Populated from [`crate::catalog_baked::baked_cells`] -- the checked-in
/// output of `cargo run --bin gen_catalog` (see [`crate::catalog_codegen`]
/// for the derivation from vendored snapshots). A `LazyLock<Vec<..>>` (not
/// a `const` slice): `CatalogRow` is no longer `Copy` (it carries a
/// `BTreeMap`), and lookup is off the hot path (dispatch never calls into
/// this table directly -- see the module doc).
static TABLE: LazyLock<Vec<BakedCell>> = LazyLock::new(|| {
    crate::catalog_baked::baked_cells()
        .into_iter()
        .map(|cell| BakedCell {
            provider_kind: cell.provider_kind,
            model_glob: cell.model_glob,
            row: cell.row,
        })
        .collect()
});

/// Tier-1 lookup: the longest matching model glob within the given
/// provider kind and tier, excluding the provider `"*"` catch-all (tier 2,
/// see [`provider_catch_all`]). A tier-agnostic cell matches any `want`.
fn find_best_match(
    provider_kind: &str,
    model: &str,
    tier: Option<&str>,
) -> Option<&'static CatalogRow> {
    let want = tier.unwrap_or("5m");
    TABLE
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
            Ok(pat) if pat.matches(model) => Some((pat.prefix_len(), &cell.row)),
            _ => None,
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, row)| row)
}

/// Tier-2 lookup: the provider-kind `"*"` catch-all (tier-agnostic).
fn provider_catch_all(provider_kind: &str) -> Option<&'static CatalogRow> {
    TABLE
        .iter()
        .find(|cell| cell.provider_kind == provider_kind && cell.model_glob == "*")
        .map(|cell| &cell.row)
}

/// Look up a REAL baked-table cell for `(provider_kind, model, tier)`, with
/// NO sentinel fallback: `None` when nothing matches (both
/// [`find_best_match`] and [`provider_catch_all`] miss). Feeds the
/// two-layer merge ([`merge`]) -- a genuine catalog miss is `Missing`, not
/// a synthesized conservative row. See [`lookup`] for the sentinel-
/// fallback convenience wrapper.
fn lookup_baked(
    provider_kind: &str,
    model: &str,
    tier: Option<&str>,
) -> Option<&'static CatalogRow> {
    find_best_match(provider_kind, model, tier).or_else(|| provider_catch_all(provider_kind))
}

/// Look up the catalog row for a `(provider_kind, model, tier)` triple.
///
/// `tier` is the requested TTL tier (`Some("5m")` / `Some("1h")`);
/// `None` resolves to the `"5m"` default (routectl's auto-emit default and
/// the common case).
///
/// Delegates to [`lookup_baked`] (exact-or-glob model match, then the
/// provider `"*"` catch-all) and falls back to [`CatalogRow::sentinel`]
/// when neither matches. Model matching reuses [`AliasPattern`] (the
/// alias-table glob matcher); the `"*"` provider catch-all is handled
/// directly, not through the matcher (which rejects a bare `*`).
#[must_use]
pub fn lookup(provider_kind: &str, model: &str, tier: Option<&str>) -> CatalogRow {
    lookup_baked(provider_kind, model, tier)
        .cloned()
        .unwrap_or_else(CatalogRow::sentinel)
}

/// True when the baked catalog table carries at least one row for
/// `kind` -- the stable `kind_str()` provider-kind discriminant
/// (`anthropic-api`, `openai-compat`, ...). Derived from [`TABLE`]
/// itself, so it cannot drift from the cataloged kind set.
///
/// The coupling guard for callers (e.g. activation gating) that need to
/// ask "is this provider kind cataloged?" without reaching into the
/// baked-table internals ([`BakedCell`] / [`TABLE`] stay private).
#[must_use]
pub fn is_cataloged_provider_kind(kind: &str) -> bool {
    TABLE.iter().any(|cell| cell.provider_kind == kind)
}

/// True when a selector's model glob matches `model`. A `"*"` glob is the
/// catch-all (treated as a match WITHOUT calling [`AliasPattern`], which
/// rejects a bare `*`); any other glob defers to the alias-glob matcher.
fn selector_glob_matches(model_glob: &str, model: &str) -> bool {
    if model_glob == "*" {
        return true;
    }
    AliasPattern::parse(model_glob).is_ok_and(|pat| pat.matches(model))
}

/// The glob specificity used to rank competing overrides: a `"*"` glob is
/// the least specific (length 0); any other glob ranks by its parsed
/// prefix length. A glob that fails to parse ranks 0 (it cannot have
/// matched a real model anyway).
fn selector_glob_specificity(model_glob: &str) -> usize {
    if model_glob == "*" {
        return 0;
    }
    AliasPattern::parse(model_glob).map_or(0, |pat| pat.prefix_len())
}

/// Pick the most-specific matching override for `(provider_kind, model)`,
/// mirroring [`lookup`]'s tier ordering: an exact-provider match beats any
/// provider `"*"` match; within a tier the longest model-glob prefix wins.
/// Keys that fail to parse are skipped (startup validation is the gate for
/// bad keys). Returns `None` when nothing matches.
///
/// Tie-break: equal-specificity matches within the same provider tier
/// resolve in ascending BTreeMap (lexicographic) key order -- the iteration
/// keeps the FIRST equal-length match seen, and `BTreeMap` iterates keys in
/// sorted order (so, e.g., a shorter exact key sorts before an equal-prefix
/// glob and wins).
fn best_override<'a>(
    provider_kind: &str,
    model: &str,
    overrides: &'a BTreeMap<String, CachePricingOverride>,
) -> Option<&'a CachePricingOverride> {
    let mut exact: Option<(usize, &'a CachePricingOverride)> = None;
    let mut star: Option<(usize, &'a CachePricingOverride)> = None;
    for (key, ov) in overrides {
        let Ok(selector) = CachePricingSelector::parse(key) else {
            continue;
        };
        if !selector_glob_matches(&selector.model_glob, model) {
            continue;
        }
        let len = selector_glob_specificity(&selector.model_glob);
        if selector.provider_kind == provider_kind {
            if exact.is_none_or(|(best, _)| len > best) {
                exact = Some((len, ov));
            }
        } else if selector.provider_kind == "*" && star.is_none_or(|(best, _)| len > best) {
            star = Some((len, ov));
        }
    }
    exact.or(star).map(|(_, ov)| ov)
}

/// Look up the catalog row for a `(provider_kind, model, tier)` triple,
/// then apply the most-specific matching LEGACY `[cache_pricing]` override
/// from `overrides`.
///
/// Resolves the baked row via [`lookup`] (sentinel fallback included),
/// then merges the best-matching override (see [`best_override`]). When NO
/// override matches, returns the baked row unchanged. When a matched
/// override is degenerate (it slipped past [`validate_overrides`] and
/// `with_overrides` rejects it), this falls back to the baked row and warns
/// -- it never panics.
#[must_use]
pub fn lookup_with_overrides(
    provider_kind: &str,
    model: &str,
    tier: Option<&str>,
    overrides: &BTreeMap<String, CachePricingOverride>,
) -> CatalogRow {
    let baked = lookup(provider_kind, model, tier);
    let Some(ov) = best_override(provider_kind, model, overrides) else {
        return baked;
    };
    match baked.with_overrides(ov) {
        Ok(row) => row,
        Err(reason) => {
            tracing::warn!(
                provider_kind,
                model,
                %reason,
                "cache-pricing override is degenerate; falling back to the baked row",
            );
            baked
        }
    }
}

/// Look up a REAL baked-table cell for `(provider_kind, model, tier)` (see
/// [`lookup_baked`] -- NO sentinel fallback) and apply the best-matching
/// LEGACY `[cache_pricing]` override, if any. Returns `None` when no baked
/// cell matches: an override targeting a wholly-unmatched selector no
/// longer synthesizes a row (the `[cache_pricing]` channel is retired in
/// later; this is its last transitional shape). Feed the result to
/// [`merge`] as the `baked` layer -- a `None` here is a genuine catalog
/// miss (`Missing`), not a row to price against.
#[must_use]
pub fn lookup_baked_with_overrides(
    provider_kind: &str,
    model: &str,
    tier: Option<&str>,
    overrides: &BTreeMap<String, CachePricingOverride>,
) -> Option<CatalogRow> {
    let baked = lookup_baked(provider_kind, model, tier)?;
    let Some(ov) = best_override(provider_kind, model, overrides) else {
        return Some(baked.clone());
    };
    match baked.with_overrides(ov) {
        Ok(row) => Some(row),
        Err(reason) => {
            tracing::warn!(
                provider_kind,
                model,
                %reason,
                "cache-pricing override is degenerate; falling back to the baked row",
            );
            Some(baked.clone())
        }
    }
}

/// Validate every `[cache_pricing]` override at startup, failing fast on a
/// bad selector key or a degenerate override so a misconfiguration surfaces
/// at boot rather than silently going inert at lookup time. A likely-typo
/// provider_kind (one not present in the baked table, and not `"*"`) is a
/// non-fatal WARN -- a custom upstream kind is legitimate.
///
/// Returns `Ok(())` on an empty map and on an all-valid table. Each error
/// is prefixed with the offending selector key so the operator knows which
/// entry to fix.
pub fn validate_overrides(
    overrides: &BTreeMap<String, CachePricingOverride>,
) -> Result<(), String> {
    for (key, ov) in overrides {
        let selector =
            CachePricingSelector::parse(key).map_err(|e| format!("[cache_pricing.{key}]: {e}"))?;
        ov.validate()
            .map_err(|e| format!("[cache_pricing.{key}]: {e}"))?;
        if selector.provider_kind != "*" && !is_cataloged_provider_kind(&selector.provider_kind) {
            tracing::warn!(
                selector = key.as_str(),
                provider_kind = selector.provider_kind.as_str(),
                "cache-pricing override provider_kind is not a known baked kind; \
                 likely a typo (the override will be inert unless the kind is real)",
            );
        }
    }
    Ok(())
}

/// Today's date as a proleptic-Gregorian epoch-day count (days since
/// 1970-01-01), derived from the system clock. Pure arithmetic, no date
/// library. Returns `0` if the clock is somehow before the epoch.
fn today_epoch_day() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        i64::try_from(d.as_secs()).unwrap_or(0) / SECONDS_PER_DAY
    })
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

/// True when a `verified_at` / snapshot date is more than
/// [`STALE_AFTER_DAYS`] before `today` (both epoch-days). A date that fails
/// to parse is treated as stale so a malformed stamp surfaces rather than
/// hides.
fn is_stale(verified_at: &str, today: i64) -> bool {
    match parse_epoch_day(verified_at) {
        Some(day) => today - day > STALE_AFTER_DAYS,
        None => true,
    }
}

/// Emit a `tracing::warn!` when the WHOLE baked table's snapshot date
/// ([`BAKED_SNAPSHOT_DATE`]) is more than 90 days stale. Never panics.
/// Called once at startup.
///
/// This redesign dropped the per-row `verified_at` field, so staleness is no
/// longer a per-cell check: every transitional baked row shares the SAME
/// table-wide snapshot date, so the table is either stale or it is not.
/// A later codegen pass restores per-build granularity via a generated
/// `CATALOG_VERSION` + snapshot date.
pub fn warn_if_stale() {
    warn_if_stale_at(today_epoch_day());
}

/// Testable core of [`warn_if_stale`]: takes "today" as an epoch-day so a
/// test can pin a deterministic clock.
fn warn_if_stale_at(today: i64) {
    if is_stale(BAKED_SNAPSHOT_DATE, today) {
        tracing::warn!(
            snapshot_date = BAKED_SNAPSHOT_DATE,
            stale_after_days = STALE_AFTER_DAYS,
            "baked catalog snapshot is stale; a newer catalog build may be available",
        );
    }
}

/// One entry from the baked catalog table, exposing the provider kind and
/// model glob alongside the row. Returned by [`baked_table_rows`].
pub struct BakedPricingRow {
    pub provider_kind: &'static str,
    pub model_glob: &'static str,
    pub row: CatalogRow,
}

/// Return a `Vec` of every entry in the baked catalog table in table order.
/// Each element carries the provider kind, model glob, and the full
/// [`CatalogRow`]. The vec length equals the number of baked cells.
#[must_use]
pub fn baked_table_rows() -> Vec<BakedPricingRow> {
    TABLE
        .iter()
        .map(|cell| BakedPricingRow {
            provider_kind: cell.provider_kind,
            model_glob: cell.model_glob,
            row: cell.row.clone(),
        })
        .collect()
}

/// True when `verified_at` is more than [`stale_after_days`] before today
/// (as measured by the system clock). Wraps [`is_stale`] with the live
/// clock for callers that do not need a pinned test clock.
#[must_use]
pub fn is_stale_today(verified_at: &str) -> bool {
    is_stale(verified_at, today_epoch_day())
}

/// The staleness horizon in days. A snapshot date more than this many days
/// before today triggers a startup WARN.
#[must_use]
pub const fn stale_after_days() -> i64 {
    STALE_AFTER_DAYS
}

// ---------------------------------------------------------------------------
// Two-layer merge: baked catalog + overlay -> EffectiveRow
// ---------------------------------------------------------------------------

/// Provenance of the value carried by an [`EffectiveRow::Present`] cell:
/// which layer won at merge time. Derived, never stored on [`CatalogRow`]
/// itself -- the row is pure economics / capability data; provenance
/// is a property of the MERGE result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The compiled-in baked catalog table.
    Baked,
    /// An overlay cell imported by a later refresh pipeline, or migrated
    /// from a legacy `[cache_pricing]` / `pricing_verifications.json`
    /// entry.
    Import,
    /// An overlay cell an operator wrote directly (later user-edit verbs).
    User,
}

/// The result of merging a baked catalog row with its overlay cell. Pure
/// data -- see [`merge`], the only constructor consumers should rely on.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectiveRow {
    /// A usable row, with the winning layer's provenance and staleness
    /// stamp attached.
    Present {
        row: CatalogRow,
        source: Source,
        verified_at: String,
    },
    /// The overlay cell for this selector is JSON `null`: an operator (or
    /// import) explicitly disabled this entry. Distinct from `Missing`.
    Disabled,
    /// Neither layer has a row for this selector: no baked cell matched
    /// and the overlay carries no entry either.
    Missing,
}

impl EffectiveRow {
    /// Fold to the row usable for pricing / capability decisions, or
    /// `None` when this cell is `Disabled` or `Missing` -- the two states
    /// share the SAME conservative sentinel behavior at every consumer: no
    /// break-even K, no context fraction, no enabling capability prior.
    #[must_use]
    pub const fn priced(&self) -> Option<&CatalogRow> {
        match self {
            Self::Present { row, .. } => Some(row),
            Self::Disabled | Self::Missing => None,
        }
    }
}

/// Merge a baked catalog row with its overlay cell. Pure function, no
/// I/O: callers resolve `baked` (e.g. via [`lookup_baked_with_overrides`])
/// and `overlay_cell` (a `CatalogOverlay.cells.get(key)` lookup) themselves.
///
/// - `overlay_cell = Some(Some(cell))` (present, non-null): the overlay
///   wins. Its sparse fields apply over `baked` (or over the conservative
///   sentinel when `baked` is `None`); provenance is the cell's own
///   `source` + `verified_at`.
/// - `overlay_cell = Some(None)` (present, JSON `null`): the entry is
///   explicitly DISABLED, regardless of `baked`.
/// - `overlay_cell = None` (absent key): falls through to `baked` --
///   `Present { source: Baked, .. }` when a baked row exists, `Missing`
///   otherwise.
#[must_use]
pub fn merge(
    baked: Option<&CatalogRow>,
    overlay_cell: Option<&Option<OverlayCell>>,
) -> EffectiveRow {
    match overlay_cell {
        Some(Some(cell)) => {
            let source = match cell.source {
                OverlaySource::Import => Source::Import,
                OverlaySource::User => Source::User,
            };
            let base = baked.cloned().unwrap_or_else(CatalogRow::sentinel);
            EffectiveRow::Present {
                row: apply_overlay_cell(&base, cell),
                source,
                verified_at: cell.verified_at.clone(),
            }
        }
        Some(None) => EffectiveRow::Disabled,
        None => match baked {
            Some(row) => EffectiveRow::Present {
                row: row.clone(),
                source: Source::Baked,
                verified_at: BAKED_SNAPSHOT_DATE.to_string(),
            },
            None => EffectiveRow::Missing,
        },
    }
}

/// Apply an overlay cell's sparse fields over `base`. Every value field on
/// [`OverlayCell`] is `Option`; an unset field inherits `base`'s value.
/// `capabilities` merges per-key: overlay entries win per key, base keys
/// the overlay does not mention pass through unchanged.
fn apply_overlay_cell(base: &CatalogRow, cell: &OverlayCell) -> CatalogRow {
    CatalogRow {
        wm: cell.wm.unwrap_or(base.wm),
        rm: cell.rm.unwrap_or(base.rm),
        ttl_seconds: cell.ttl_seconds.unwrap_or(base.ttl_seconds),
        min_prefix_tokens: cell.min_prefix_tokens.unwrap_or(base.min_prefix_tokens),
        has_storage_rent: base.has_storage_rent,
        storage_rent: base.storage_rent,
        auto_cacher: base.auto_cacher,
        tier: base.tier,
        max_context_tokens: cell.max_context_tokens.or(base.max_context_tokens),
        capabilities: merge_capabilities(&base.capabilities, cell.capabilities.as_ref()),
    }
}

/// Overlay capability entries win per-key; base keys the overlay omits
/// pass through unchanged.
fn merge_capabilities(
    base: &BTreeMap<String, bool>,
    overlay: Option<&BTreeMap<String, bool>>,
) -> BTreeMap<String, bool> {
    let mut merged = base.clone();
    if let Some(ov) = overlay {
        for (k, v) in ov {
            merged.insert(k.clone(), *v);
        }
    }
    merged
}

/// Select the overlay cell for `(provider_kind, model)`, mirroring
/// [`best_override`]'s selector-match precedence: an exact-provider match
/// beats the `"*"` catch-all, and within a tier the longest model-glob
/// prefix wins. Overlay keys share the SAME `"provider_kind:model_glob"`
/// shape as `[cache_pricing]` selector keys, so [`CachePricingSelector`]
/// parses them too.
///
/// Returns `None` when no key matches -- the caller then passes `None` to
/// [`merge`], which falls through to the baked layer. Returns
/// `Some(&Option<OverlayCell>)` on a match; the inner `None` is the
/// null-disable sentinel `merge` reads directly (see the module's overlay
/// docs on [`crate::catalog_overlay`]).
#[must_use]
pub fn lookup_overlay_cell<'a>(
    provider_kind: &str,
    model: &str,
    overlay: &'a CatalogOverlay,
) -> Option<&'a Option<OverlayCell>> {
    let mut exact: Option<(usize, &'a Option<OverlayCell>)> = None;
    let mut star: Option<(usize, &'a Option<OverlayCell>)> = None;
    for (key, cell) in &overlay.cells {
        let Ok(selector) = CachePricingSelector::parse(key) else {
            continue;
        };
        if !selector_glob_matches(&selector.model_glob, model) {
            continue;
        }
        let len = selector_glob_specificity(&selector.model_glob);
        if selector.provider_kind == provider_kind {
            if exact.is_none_or(|(best, _)| len > best) {
                exact = Some((len, cell));
            }
        } else if selector.provider_kind == "*" && star.is_none_or(|(best, _)| len > best) {
            star = Some((len, cell));
        }
    }
    exact.or(star).map(|(_, cell)| cell)
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
    }

    #[test]
    fn lookup_falls_back_to_provider_catch_all_row() {
        // Arrange / Act: an Anthropic model with no specific cell.
        let r = lookup("anthropic-api", "claude-future-9-9", None);

        // Assert: the anthropic-api "*" row (4096 min-prefix default).
        assert_eq!(r.wm, 1.25);
        assert_eq!(r.min_prefix_tokens, 4096);
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
    }

    #[test]
    fn is_cataloged_provider_kind_matches_baked_kinds_only() {
        // Known stable kind_str discriminants present in the baked table.
        assert!(is_cataloged_provider_kind("anthropic-api"));
        assert!(is_cataloged_provider_kind("openai-compat"));

        // A nonsense kind is not cataloged.
        assert!(!is_cataloged_provider_kind("not-a-real-kind"));
    }

    #[test]
    fn lookup_baked_returns_none_for_unknown_provider_and_model() {
        // Unlike `lookup`, the no-sentinel-fallback variant reports a
        // genuine catalog miss as `None` -- this is what makes `Missing`
        // reachable through the two-layer merge.
        assert!(lookup_baked("some-future-kind", "whatever-model", None).is_none());
    }

    #[test]
    fn sentinel_has_the_documented_conservative_shape() {
        // Arrange / Act
        let s = CatalogRow::sentinel();

        // Assert
        assert_eq!(s.wm, 2.0);
        assert_eq!(s.rm, 0.10);
        assert_eq!(s.ttl_seconds, 300);
        assert!(!s.auto_cacher);
        assert!(s.capabilities.is_empty());
    }

    #[test]
    fn catalog_row_carries_no_provenance_fields() {
        // Pin: `CatalogRow` has no `verified` / `source` / `verified_at`
        // field -- provenance lives on `EffectiveRow` only. This test
        // exists to fail loudly (compile error) if those fields ever creep
        // back onto the row.
        let row = CatalogRow::sentinel();
        let CatalogRow {
            wm: _,
            rm: _,
            ttl_seconds: _,
            min_prefix_tokens: _,
            has_storage_rent: _,
            storage_rent: _,
            auto_cacher: _,
            tier: _,
            max_context_tokens: _,
            capabilities: _,
        } = row;
    }

    #[test]
    fn anthropic_5m_loads_exact_multipliers() {
        let r = lookup("anthropic-api", "claude-sonnet-4-6", None);
        assert_eq!(r.wm, 1.25);
        assert_eq!(r.rm, 0.10);
        assert_eq!(r.ttl_seconds, 300);
    }

    #[test]
    fn openai_responses_loads_24h_ttl_and_auto_cacher() {
        let r = lookup("openai-responses", "gpt-5.5", None);
        assert_eq!(r.wm, 1.25);
        assert_eq!(r.rm, 0.10);
        assert_eq!(r.ttl_seconds, 86_400);
        assert!(r.auto_cacher);
    }

    #[test]
    fn deepseek_loads_deep_read_multiplier() {
        // V4-Pro: the deepest read discount.
        let pro = lookup("openai-compat", "deepseek-v4-pro", None);
        assert_eq!(pro.wm, 0.0);
        assert!((pro.rm - 0.008_333_333).abs() < 1e-6);

        // V4 (non-pro) falls to the flash row via the broader glob.
        let flash = lookup("openai-compat", "deepseek-v4-flash", None);
        assert_eq!(flash.rm, 0.02);
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
        // break-even math degenerate. Rejected unconditionally (no ack flag
        // exempts it).
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
        assert!(
            (r.rm - 0.008_333_333).abs() < 1e-6,
            "the deepseek pro glob must win"
        );

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
        // match (a leading-wildcard glob would be rejected and silently
        // dropped, falling through to the catch-all).
        let r = lookup(
            "bedrock",
            "anthropic.claude-sonnet-4-6-20260401-v1:0",
            Some("5m"),
        );
        assert_eq!(r.min_prefix_tokens, 1024);
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
        // Smoke test: the startup hook runs without panicking, at a
        // deterministic "today" near the baked snapshot date.
        let today = parse_epoch_day(BAKED_SNAPSHOT_DATE).expect("parse BAKED_SNAPSHOT_DATE");
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

    /// Build a one-entry override map tersely.
    fn ov_map(key: &str, ov: CachePricingOverride) -> BTreeMap<String, CachePricingOverride> {
        let mut m = BTreeMap::new();
        m.insert(key.to_string(), ov);
        m
    }

    #[test]
    fn lookup_with_overrides_applies_a_matching_override() {
        // Arrange: an override on the Opus 4.8 cell that sets min_prefix_tokens
        // (a field that does not trip the wm / rm degeneracy guards) and leaves
        // everything else unset.
        let baked = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let overrides = ov_map(
            "anthropic-api:claude-opus-4-8*",
            CachePricingOverride {
                min_prefix_tokens: Some(512),
                ..Default::default()
            },
        );

        // Act
        let merged =
            lookup_with_overrides("anthropic-api", "claude-opus-4-8", Some("5m"), &overrides);

        // Assert: the overridden field changed; unset fields inherited from
        // the baked row.
        assert_eq!(merged.min_prefix_tokens, 512);
        assert_eq!(merged.wm, baked.wm);
        assert_eq!(merged.rm, baked.rm);
        assert_eq!(merged.ttl_seconds, baked.ttl_seconds);
        assert_eq!(merged.tier, baked.tier);
    }

    #[test]
    fn lookup_with_overrides_returns_baked_row_when_no_selector_matches() {
        // Arrange: an override keyed on a DIFFERENT provider / model.
        let overrides = ov_map(
            "openai-compat:grok-*",
            CachePricingOverride {
                rm: Some(0.5),
                ..Default::default()
            },
        );

        // Act
        let merged =
            lookup_with_overrides("anthropic-api", "claude-opus-4-8", Some("5m"), &overrides);
        let baked = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));

        // Assert: byte-identical to the un-overridden lookup.
        assert_eq!(merged, baked);
    }

    #[test]
    fn lookup_with_overrides_prefers_longer_model_glob_within_a_tier() {
        // Arrange: two overlapping exact-provider overrides for a v4-pro id.
        // The longer glob (deepseek-v4-pro*) must win over the broad one.
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "openai-compat:deepseek-*".to_string(),
            CachePricingOverride {
                min_prefix_tokens: Some(100),
                ..Default::default()
            },
        );
        overrides.insert(
            "openai-compat:deepseek-v4-pro*".to_string(),
            CachePricingOverride {
                min_prefix_tokens: Some(200),
                ..Default::default()
            },
        );

        // Act
        let merged =
            lookup_with_overrides("openai-compat", "deepseek-v4-pro-0610", None, &overrides);

        // Assert: the longer, more specific glob's value applied.
        assert_eq!(merged.min_prefix_tokens, 200);
    }

    #[test]
    fn lookup_with_overrides_prefers_exact_provider_over_star_provider() {
        // Arrange: a provider "*" catch-all override AND an exact-provider
        // override both match. The exact-provider one must win, mirroring
        // lookup's own tier ordering.
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "*:*".to_string(),
            CachePricingOverride {
                min_prefix_tokens: Some(1),
                ..Default::default()
            },
        );
        overrides.insert(
            "anthropic-api:*".to_string(),
            CachePricingOverride {
                min_prefix_tokens: Some(7),
                ..Default::default()
            },
        );

        // Act
        let merged =
            lookup_with_overrides("anthropic-api", "claude-opus-4-8", Some("5m"), &overrides);

        // Assert: the exact-provider override wins even though both match and
        // the "*" provider key sorts first in the BTreeMap.
        assert_eq!(merged.min_prefix_tokens, 7);
    }

    #[test]
    fn lookup_with_overrides_falls_back_to_baked_row_on_degenerate_override() {
        // Arrange: an override that fails with_overrides (rm <= 0.0). This
        // models a degenerate override that slipped past startup validation;
        // lookup must fail-closed to the baked row, never panic.
        let baked = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let overrides = ov_map(
            "anthropic-api:claude-opus-4-8*",
            CachePricingOverride {
                rm: Some(0.0),
                ..Default::default()
            },
        );

        // Act
        let merged =
            lookup_with_overrides("anthropic-api", "claude-opus-4-8", Some("5m"), &overrides);

        // Assert: the baked row is returned unchanged.
        assert_eq!(merged, baked);
    }

    #[test]
    fn lookup_with_overrides_skips_unparseable_selector_keys() {
        // Arrange: a key with no colon never parses; lookup must ignore it (the
        // startup validator is the gate for bad keys) and return the baked row.
        let baked = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));
        let overrides = ov_map(
            "no-colon-here",
            CachePricingOverride {
                min_prefix_tokens: Some(1),
                ..Default::default()
            },
        );

        // Act
        let merged =
            lookup_with_overrides("anthropic-api", "claude-opus-4-8", Some("5m"), &overrides);

        // Assert
        assert_eq!(merged, baked);
    }

    #[test]
    fn lookup_baked_with_overrides_returns_none_for_wholly_unmatched_selector() {
        // Arrange: an override targeting a selector with no baked backing at
        // all. Unlike the legacy `lookup_with_overrides`, this must NOT
        // synthesize a sentinel-based row.
        let overrides = ov_map(
            "some-future-kind:whatever-*",
            CachePricingOverride {
                rm: Some(0.5),
                ..Default::default()
            },
        );

        // Act / Assert
        assert!(
            lookup_baked_with_overrides("some-future-kind", "whatever-model", None, &overrides)
                .is_none()
        );
    }

    #[test]
    fn lookup_baked_with_overrides_applies_override_onto_a_real_match() {
        let overrides = ov_map(
            "anthropic-api:claude-opus-4-8*",
            CachePricingOverride {
                min_prefix_tokens: Some(512),
                ..Default::default()
            },
        );

        let merged =
            lookup_baked_with_overrides("anthropic-api", "claude-opus-4-8", Some("5m"), &overrides)
                .expect("real baked cell must resolve");
        assert_eq!(merged.min_prefix_tokens, 512);
    }

    #[test]
    fn validate_overrides_accepts_empty_map() {
        let empty: BTreeMap<String, CachePricingOverride> = BTreeMap::new();
        assert!(validate_overrides(&empty).is_ok());
    }

    #[test]
    fn validate_overrides_accepts_a_valid_override() {
        let overrides = ov_map(
            "openai-compat:grok-*",
            CachePricingOverride {
                rm: Some(0.12),
                ..Default::default()
            },
        );
        assert!(validate_overrides(&overrides).is_ok());
    }

    #[test]
    fn validate_overrides_rejects_unparseable_key_naming_the_selector() {
        let overrides = ov_map(
            "no-colon-key",
            CachePricingOverride {
                rm: Some(0.1),
                ..Default::default()
            },
        );
        let err = validate_overrides(&overrides).expect_err("unparseable key must fail");
        assert!(err.contains("no-colon-key"), "msg: {err}");
        assert!(err.contains("missing a `:`"), "msg: {err}");
    }

    #[test]
    fn validate_overrides_rejects_non_positive_rm_naming_the_selector() {
        let overrides = ov_map(
            "openai-compat:grok-*",
            CachePricingOverride {
                rm: Some(0.0),
                ..Default::default()
            },
        );
        let err = validate_overrides(&overrides).expect_err("rm <= 0 must fail");
        assert!(err.contains("openai-compat:grok-*"), "msg: {err}");
        assert!(err.contains("rm must be > 0.0"), "msg: {err}");
    }

    #[test]
    fn validate_overrides_rejects_below_sentinel_wm_without_ack_naming_selector() {
        let overrides = ov_map(
            "anthropic-api:claude-opus-4-8*",
            CachePricingOverride {
                wm: Some(1.0),
                ..Default::default()
            },
        );
        let err = validate_overrides(&overrides).expect_err("below-sentinel wm must fail");
        assert!(err.contains("anthropic-api:claude-opus-4-8*"), "msg: {err}");
        assert!(
            err.contains("override_acknowledges_cost_risk"),
            "msg: {err}"
        );
    }

    #[test]
    fn validate_overrides_accepts_below_sentinel_wm_with_ack() {
        let overrides = ov_map(
            "anthropic-api:claude-opus-4-8*",
            CachePricingOverride {
                wm: Some(1.0),
                override_acknowledges_cost_risk: true,
                ..Default::default()
            },
        );
        assert!(validate_overrides(&overrides).is_ok());
    }

    #[test]
    fn validate_overrides_accepts_unknown_provider_kind_as_non_fatal_warn() {
        // A likely-typo provider_kind that still parses is accepted (the typo
        // hint is a non-fatal warn, never a hard failure).
        let overrides = ov_map(
            "openai-compatt:grok-*",
            CachePricingOverride {
                rm: Some(0.1),
                ..Default::default()
            },
        );
        assert!(validate_overrides(&overrides).is_ok());
    }

    #[test]
    fn validate_overrides_accepts_star_provider_selector_without_warn() {
        // A "*" provider_kind is a legitimate catch-all selector, not a typo.
        let overrides = ov_map(
            "*:claude-opus-4-8*",
            CachePricingOverride {
                rm: Some(0.1),
                ..Default::default()
            },
        );
        assert!(validate_overrides(&overrides).is_ok());
    }

    #[test]
    fn pricing_row_reserved_fields_are_zeroed_on_baked_rows() {
        for cell in TABLE.iter() {
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

    // -- baked_table_rows tests ---------------------------------------------

    #[test]
    fn baked_table_rows_is_non_empty_covers_all_four_provider_kinds() {
        // Arrange / Act
        let rows = baked_table_rows();

        // Assert: non-empty and all four known provider kinds present.
        assert!(!rows.is_empty());
        let kinds: Vec<&str> = rows.iter().map(|r| r.provider_kind).collect();
        for kind in &[
            "anthropic-api",
            "bedrock",
            "openai-responses",
            "openai-compat",
        ] {
            assert!(
                kinds.contains(kind),
                "provider kind {kind} missing from baked_table_rows"
            );
        }
    }

    #[test]
    fn baked_table_rows_length_matches_table_constant() {
        assert_eq!(baked_table_rows().len(), TABLE.len());
    }

    // -- is_stale_today / stale_after_days tests ----------------------------

    #[test]
    fn is_stale_today_returns_false_for_recent_baked_stamp() {
        let stamp = parse_epoch_day(BAKED_SNAPSHOT_DATE).expect("parse BAKED_SNAPSHOT_DATE");
        assert!(!is_stale(BAKED_SNAPSHOT_DATE, stamp + 10));
    }

    #[test]
    fn stale_after_days_returns_ninety() {
        // The constant must match the documented 90-day horizon.
        assert_eq!(stale_after_days(), 90);
    }

    #[test]
    fn is_stale_today_boundary_exactly_stale_after_days_is_not_stale() {
        // is_stale uses strict `>`, so exactly STALE_AFTER_DAYS is still fresh;
        // one day more is stale. Mirrors the existing boundary test but via the
        // public is_stale_today wrapper (backed by a pinned inner call).
        let stamp = parse_epoch_day("2026-01-01").expect("parse");
        // The public is_stale_today reads the real clock; test the inner helper
        // directly with the same pinned logic the other staleness tests use.
        assert!(!is_stale("2026-01-01", stamp + stale_after_days()));
        assert!(is_stale("2026-01-01", stamp + stale_after_days() + 1));
    }

    #[test]
    fn is_stale_today_public_real_clock_smoke() {
        // Far-future stamp: never stale against any reasonable real clock.
        assert!(!is_stale_today("2099-01-01"));
        // Ancient stamp: always stale.
        assert!(is_stale_today("1971-01-01"));
    }

    // -- max_context_tokens tests --------------------------------------------

    #[test]
    fn known_anthropic_model_lookup_returns_confirmed_window() {
        // Arrange / Act: Sonnet 4.6, a narrow version-pinned glob with a
        // confirmed 1M window.
        let r = lookup("anthropic-api", "claude-sonnet-4-6", None);

        // Assert
        assert_eq!(r.max_context_tokens, Some(1_000_000));
    }

    #[test]
    fn unknown_provider_and_model_lookup_returns_none_via_sentinel() {
        // Arrange / Act
        let r = lookup("some-future-kind", "whatever-model", None);

        // Assert: fail-closed sentinel carries no window.
        assert_eq!(r.max_context_tokens, None);
    }

    #[test]
    fn sentinel_max_context_tokens_is_none() {
        assert_eq!(CatalogRow::sentinel().max_context_tokens, None);
    }

    #[test]
    fn broad_ambiguous_glob_bakes_none_despite_shorthand_figures() {
        // grok-* also matches a differently-windowed code model, so the
        // family-wide row must not bake a single number.
        let r = lookup("openai-compat", "grok-4-3", None);
        assert_eq!(r.max_context_tokens, None);
    }

    #[test]
    fn with_overrides_some_wins_over_baked_some() {
        // Arrange: baked Sonnet 4.6 row already carries Some(1_000_000).
        let baked = lookup("anthropic-api", "claude-sonnet-4-6", None);
        assert_eq!(baked.max_context_tokens, Some(1_000_000));
        let ov = CachePricingOverride {
            max_context_tokens: Some(500_000),
            ..Default::default()
        };

        // Act
        let merged = baked.with_overrides(&ov).expect("accepted");

        // Assert: the override value wins.
        assert_eq!(merged.max_context_tokens, Some(500_000));
    }

    #[test]
    fn with_overrides_some_wins_over_baked_none() {
        // Arrange: baked grok-* row carries None.
        let baked = lookup("openai-compat", "grok-4-3", None);
        assert_eq!(baked.max_context_tokens, None);
        let ov = CachePricingOverride {
            max_context_tokens: Some(256_000),
            ..Default::default()
        };

        // Act
        let merged = baked.with_overrides(&ov).expect("accepted");

        // Assert
        assert_eq!(merged.max_context_tokens, Some(256_000));
    }

    #[test]
    fn with_overrides_none_inherits_baked_value_unchanged() {
        // Arrange: an override touching an unrelated field only; the baked
        // window (Some or None) must pass through unchanged either way.
        let baked_some = lookup("anthropic-api", "claude-sonnet-4-6", None);
        let ov = CachePricingOverride {
            ttl_seconds: Some(3_600),
            ..Default::default()
        };
        let merged_some = baked_some.with_overrides(&ov).expect("accepted");
        assert_eq!(
            merged_some.max_context_tokens,
            baked_some.max_context_tokens
        );

        let baked_none = lookup("openai-compat", "grok-4-3", None);
        let merged_none = baked_none.with_overrides(&ov).expect("accepted");
        assert_eq!(
            merged_none.max_context_tokens,
            baked_none.max_context_tokens
        );
    }

    #[test]
    fn validate_rejects_zero_max_context_tokens() {
        // Arrange
        let ov = CachePricingOverride {
            max_context_tokens: Some(0),
            ..Default::default()
        };

        // Act
        let err = ov.validate().expect_err("Some(0) must be rejected");

        // Assert
        assert!(err.contains("max_context_tokens"), "msg: {err}");
    }

    #[test]
    fn validate_accepts_positive_max_context_tokens() {
        let ov = CachePricingOverride {
            max_context_tokens: Some(128_000),
            ..Default::default()
        };
        assert!(ov.validate().is_ok());
    }

    #[test]
    fn override_toml_deny_unknown_fields_rejects_typo() {
        // Arrange: a plausible-looking typo on the new field name.
        let toml_src = r"max_context_token = 128000";

        // Act
        let result = toml::from_str::<CachePricingOverride>(toml_src);

        // Assert: deny_unknown_fields still catches the typo.
        assert!(result.is_err(), "typo'd field must be rejected");
    }

    #[test]
    fn override_toml_accepts_correctly_spelled_field() {
        // Arrange
        let toml_src = r"max_context_tokens = 128000";

        // Act
        let ov: CachePricingOverride = toml::from_str(toml_src).expect("parse");

        // Assert
        assert_eq!(ov.max_context_tokens, Some(128_000));
    }

    #[test]
    fn baked_table_pins_representative_max_context_tokens_cells() {
        // Guards specific bake-vs-None classifications against silent drift.
        // Distinct from `known_anthropic_model_lookup_returns_confirmed_window`
        // and `broad_ambiguous_glob_bakes_none_despite_shorthand_figures`
        // above -- this pins a different, non-overlapping slice: confirmed
        // windows on other providers, an explicit (non-catch-all) None row,
        // a broad-but-not-bare glob that fails closed, and each provider's
        // bare `"*"` catch-all.
        let cases: &[(&str, &str, Option<u32>)] = &[
            // Confirmed windows.
            ("anthropic-api", "claude-opus-4-8", Some(1_000_000)),
            ("anthropic-api", "claude-haiku-4-5", Some(200_000)),
            ("bedrock", "anthropic.claude-sonnet-4-5", Some(200_000)),
            ("openai-compat", "deepseek-v4-pro", Some(1_000_000)),
            // A model with no dedicated baked cell falls through to the
            // provider `"*"` catch-all, which carries no confirmed window.
            ("anthropic-api", "claude-haiku-3-5", None),
            // Broad glob, fails closed (the family spans genuinely
            // different confirmed windows -- see `catalog_codegen`'s
            // `context_ambiguous` selectors), not a bare "*".
            ("openai-compat", "mistral-large", None),
            // Bare "*" provider catch-alls.
            ("bedrock", "anthropic.claude-nonexistent-9", None),
            ("openai-compat", "some-unrecognized-vendor-model", None),
        ];

        for (provider_kind, model, expected) in cases {
            let actual = lookup(provider_kind, model, None).max_context_tokens;
            assert_eq!(
                actual, *expected,
                "lookup({provider_kind:?}, {model:?}) max_context_tokens"
            );
        }
    }

    // -- two-layer merge -------------------------------------------------

    fn baked_fixture() -> CatalogRow {
        lookup("anthropic-api", "claude-opus-4-8", Some("5m"))
    }

    fn import_cell() -> OverlayCell {
        OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-06-01".to_string(),
            wm: Some(1.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: None,
        }
    }

    fn user_cell() -> OverlayCell {
        OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-01".to_string(),
            wm: Some(1.75),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: None,
        }
    }

    #[test]
    fn merge_overlay_present_wins_and_carries_source_through() {
        let baked = baked_fixture();
        let cell = Some(import_cell());
        let effective = merge(Some(&baked), Some(&cell));

        match effective {
            EffectiveRow::Present {
                row,
                source,
                verified_at,
            } => {
                assert_eq!(source, Source::Import);
                assert_eq!(verified_at, "2026-06-01");
                // wm applied from the overlay; unset fields inherit baked.
                assert_eq!(row.wm, 1.5);
                assert_eq!(row.rm, baked.rm);
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn merge_overlay_null_disables_regardless_of_baked() {
        let baked = baked_fixture();
        let effective = merge(Some(&baked), Some(&None));
        assert_eq!(effective, EffectiveRow::Disabled);
    }

    #[test]
    fn merge_absent_overlay_key_falls_through_to_baked_present() {
        let baked = baked_fixture();
        let effective = merge(Some(&baked), None);
        match effective {
            EffectiveRow::Present {
                row,
                source,
                verified_at,
            } => {
                assert_eq!(source, Source::Baked);
                assert_eq!(verified_at, BAKED_SNAPSHOT_DATE);
                assert_eq!(row, baked);
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn merge_no_baked_and_absent_overlay_is_missing() {
        let effective = merge(None, None);
        assert_eq!(effective, EffectiveRow::Missing);
    }

    #[test]
    fn merge_overlay_field_applies_over_baked_field() {
        let baked = baked_fixture();
        let cell = Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-01".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: Some(9_999),
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: None,
        });
        let effective = merge(Some(&baked), Some(&cell));
        let row = effective.priced().expect("present");
        assert_eq!(row.ttl_seconds, 9_999);
        // Untouched fields still inherit the baked row.
        assert_eq!(row.wm, baked.wm);
        assert_eq!(row.rm, baked.rm);
    }

    #[test]
    fn merge_overlay_capabilities_apply_per_key_over_baked() {
        let baked = baked_fixture();
        // Precondition: the baked row already carries capability priors
        // (derived from the vendored snapshots) but not `web_search`.
        assert!(!baked.capabilities.is_empty());
        assert_eq!(baked.capabilities.get("web_search"), None);
        let cell = Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-01".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: Some(BTreeMap::from([("web_search".to_string(), true)])),
        });
        let effective = merge(Some(&baked), Some(&cell));
        let row = effective.priced().expect("present");
        // The overlay's key is added; the baked row's own keys pass through
        // unchanged (per-key merge, not a wholesale replacement).
        assert_eq!(row.capabilities.get("web_search"), Some(&true));
        for (key, value) in &baked.capabilities {
            assert_eq!(row.capabilities.get(key), Some(value));
        }
    }

    #[test]
    fn merge_no_baked_with_overlay_present_uses_sentinel_as_base() {
        // An overlay cell naming a selector with no baked backing still
        // resolves `Present` -- the sentinel is the base, and the cell's
        // fields apply over it.
        let cell = Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-01".to_string(),
            wm: Some(1.0),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: None,
        });
        let effective = merge(None, Some(&cell));
        let row = effective.priced().expect("present");
        assert_eq!(row.wm, 1.0);
        assert_eq!(row.rm, CatalogRow::sentinel().rm);
    }

    #[test]
    fn effective_row_priced_folds_disabled_and_missing_to_none() {
        assert!(EffectiveRow::Disabled.priced().is_none());
        assert!(EffectiveRow::Missing.priced().is_none());
        let present = merge(Some(&baked_fixture()), None);
        assert!(present.priced().is_some());
    }

    // -------------------------------------------------------------------
    // lookup_overlay_cell: selector-match precedence over CatalogOverlay.
    // -------------------------------------------------------------------

    fn overlay_with(cells: Vec<(&str, Option<OverlayCell>)>) -> CatalogOverlay {
        CatalogOverlay {
            schema_version: crate::catalog_overlay::CATALOG_OVERLAY_SCHEMA_VERSION,
            revision: 0,
            cells: cells.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    #[test]
    fn lookup_overlay_cell_returns_none_when_no_key_matches() {
        let overlay = overlay_with(vec![("openai-compat:grok-*", Some(import_cell()))]);
        assert!(lookup_overlay_cell("anthropic-api", "claude-opus-4-8", &overlay).is_none());
    }

    #[test]
    fn lookup_overlay_cell_exact_provider_beats_star_catch_all() {
        let overlay = overlay_with(vec![
            ("*:claude-opus-4-8*", Some(import_cell())),
            ("anthropic-api:claude-opus-4-8*", Some(user_cell())),
        ]);
        let found = lookup_overlay_cell("anthropic-api", "claude-opus-4-8", &overlay)
            .expect("a cell must match");
        assert_eq!(
            found.as_ref().map(|c| c.source),
            Some(OverlaySource::User),
            "the exact-provider cell must win over the `*` catch-all",
        );
    }

    #[test]
    fn lookup_overlay_cell_longest_glob_prefix_wins() {
        let overlay = overlay_with(vec![
            ("anthropic-api:claude-*", Some(import_cell())),
            ("anthropic-api:claude-opus-4-8*", Some(user_cell())),
        ]);
        let found = lookup_overlay_cell("anthropic-api", "claude-opus-4-8", &overlay)
            .expect("a cell must match");
        assert_eq!(found.as_ref().map(|c| c.source), Some(OverlaySource::User));
    }

    #[test]
    fn lookup_overlay_cell_surfaces_null_disable_entry() {
        let overlay = overlay_with(vec![("anthropic-api:claude-opus-4-8*", None)]);
        let found = lookup_overlay_cell("anthropic-api", "claude-opus-4-8", &overlay)
            .expect("the disabled key itself must be found");
        assert!(found.is_none(), "a null cell must surface as Some(None)");
    }

    #[test]
    fn breadth_floor_every_adapter_kind_has_a_dedicated_flagship_row() {
        // Breadth floor (spec): every shipped adapter kind's current
        // flagship model resolves to a DEDICATED baked cell -- not just the
        // provider `"*"` catch-all every model falls through to. Each
        // `find_best_match` call below excludes the catch-all by
        // construction, so a `None` here means the generated table is
        // missing real per-model coverage for that adapter kind.
        let flagships: &[(&str, &str)] = &[
            ("anthropic-api", "claude-opus-4-8"),
            ("bedrock", "anthropic.claude-sonnet-4-6-20260401-v1:0"),
            ("openai-compat", "deepseek-v4-pro"),
            ("openai-compat", "deepseek-v4-flash"),
            ("openai-compat", "gemini-3.5-flash"),
            ("openai-compat", "grok-4.5"),
            ("openai-compat", "kimi-k2-thinking"),
            ("openai-compat", "moonshot-kimi-k2-thinking"),
            ("openai-compat", "mistral-large-latest"),
            ("openai-compat", "qwen-max"),
            ("openai-compat", "minimax-m3"),
            ("openai-compat", "minimax-m2"),
        ];
        for (provider_kind, model) in flagships {
            assert!(
                find_best_match(provider_kind, model, None).is_some(),
                "breadth floor: no dedicated (non-catch-all) baked row matches \
                 {provider_kind}/{model}",
            );
        }
    }

    #[test]
    fn breadth_floor_openai_responses_catch_all_carries_real_flagship_data() {
        // openai-responses has exactly one row by design (a single glob
        // catch-all covers the whole adapter kind -- see the module doc),
        // so the breadth floor here is that the row itself carries REAL
        // derived data for the current flagship, not a placeholder.
        let r = lookup("openai-responses", "gpt-5.6", None);
        assert!(
            r.max_context_tokens.is_some(),
            "openai-responses catch-all must carry a real (derived) context window",
        );
    }
}
