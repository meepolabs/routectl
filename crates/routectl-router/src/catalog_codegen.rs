//! Codegen for the checked-in baked catalog table
//! (`crate::catalog_baked`).
//!
//! This module is the shared core between the `gen_catalog` binary
//! (`src/bin/gen_catalog.rs`, which writes `catalog_baked.rs` to disk) and
//! this module's own drift-guard test (which regenerates in-memory and
//! diffs against the committed file). It is `pub` -- not `pub(crate)` --
//! purely so the `gen_catalog` bin (a separate compilation unit under
//! `src/bin/`) can reach `render_catalog_baked_rs`; it carries no part of
//! the catalog's runtime contract and is hidden from rendered docs.
//!
//! SOURCES: two vendored JSON snapshots under `catalog_data/` (see
//! `catalog_data/NOTICE` for licenses and URLs), baked into this binary via
//! `include_str!` so generation needs no filesystem access beyond writing
//! its own output, and reruns identically regardless of the invoking
//! process's working directory.
//!
//!   - `models_dev.json` (models.dev) is the PRIMARY source for economics
//!     (`cost.input` / `cost.output` / `cost.cache_read` /
//!     `cost.cache_write` / `limit.context`) and for the
//!     `structured_output` capability tell -- its schema names these fields
//!     directly. NOTE its price UNIT: `cost.*` is dollars per MILLION
//!     tokens, where litellm's `*_cost_per_token` fields are already
//!     per-token (see `MODELS_DEV_PRICE_UNIT_TOKENS`).
//!   - `litellm_model_prices_and_context_window.json` (BerriAI/litellm) is
//!     the CROSS-CHECK source for those same fields, and the SOLE source
//!     for `web_search` / `computer_use` (models.dev has no equivalent
//!     fields) and for the 1-hour cache-write tier (models.dev has no
//!     separate 1h price at all).
//!
//! CROSS-CHECK: whenever both sources report a comparable value for the
//! same (selector, field) and they disagree, generation FAILS with a
//! message naming both values -- unless `catalog_data/cross_check_allowlist.json`
//! carries an entry for that exact key, in which case the allowlisted
//! `resolved` value is used instead. There is no silent fallback path.
//!
//! ECONOMICS NOT DERIVABLE FROM EITHER SOURCE (`ttl_seconds`,
//! `min_prefix_tokens`, `auto_cacher`, the provider-kind catch-all rows):
//! neither vendored feed publishes cache TTL, minimum-prefix, or
//! automatic-vs-explicit-cache-mode facts, so these stay curated constants
//! in this file's selector tables, matching the documented behavior of
//! each vendor's caching product. `max_context_tokens`,
//! `max_output_tokens`, and `capabilities`
//! are always derived from the snapshots; `wm` / `rm` are derived when a
//! source publishes cache pricing for the selector, else the selector is
//! marked `economics_unconfirmed` and its economics mirror
//! [`crate::catalog::CatalogRow::sentinel`] rather than a fabricated
//! number (see `OPENAI_COMPAT_SELECTORS`).
//!
//! BASE RATES: `input_cost_per_token` / `output_cost_per_token` are the
//! absolute dollar rates `wm` / `rm` are multipliers OF, extracted through
//! the same cross-check (see `base_rates_for`). They serve a query-time
//! cost ESTIMATE and never displace a provider-reported billed figure. A
//! selector whose glob spans models the sources price differently is marked
//! `price_ambiguous` and stays priced-ABSENT (`None`) -- the same
//! fail-closed discipline as `max_context_tokens`, since a wrong dollar
//! rate compounds per token. Operator config still WINS over any baked rate
//! (see [`crate::catalog::merge`]).
//!
//! OUTPUT CEILING: `max_output_tokens` is derived under STRICTER coherence
//! than any other field (see `output_ceiling_for`). The litellm side is not
//! read off the selector's one representative entry: EVERY litellm entry the
//! selector's own glob spans has to publish the same figure, mechanically
//! checked against the snapshot rather than trusted from a curated flag
//! (`output_ambiguous` is asserted against that same derivation -- see
//! `catalog_codegen_selectors`). On top of that unanimity, models.dev has to
//! publish a figure too and agree with it, and the result may not exceed the
//! selector's own confirmed context window. Single-source data is
//! deliberately NOT enough: the two feeds disagree on output ceilings far
//! more often than on windows (gateway and region re-listings of the same
//! model cap output lower than the direct API does), so an uncorroborated
//! figure is not evidence of a real limit. A wrong ceiling fails LOUDLY
//! upstream when too high and truncates SILENTLY when too low, which makes
//! absent strictly better than guessed -- so unlike every other field, a
//! bare source disagreement here yields NO VALUE instead of failing
//! generation. The allowlist still applies, as the way an operator records a
//! deliberate resolution of one known mismatch; an allowlist value that is
//! not itself a usable ceiling fails generation.

use std::collections::BTreeMap;
#[cfg(feature = "gen-catalog")]
use std::fmt::Write as _;

use routectl_core::capability::{COMPUTER_USE, STRUCTURED_OUTPUT, WEB_SEARCH};
use serde_json::Value;

use crate::catalog::CatalogRow;
use crate::catalog_codegen_selectors::{
    ANTHROPIC_SELECTORS, AutoCacherSelector, BEDROCK_SELECTORS, CATCH_ALL_ROWS,
    OPENAI_COMPAT_SELECTORS, OPENAI_RESPONSES_SELECTORS, TieredSelector,
};
use crate::glob::AliasPattern;

/// Baked catalog schema version. Bump whenever this module's output
/// changes materially (rows added/removed, a derivation rule changed) --
/// NOT on a vendor-snapshot refresh that leaves the generated shape
/// unchanged. Rendered into `catalog_baked.rs` as `CATALOG_VERSION: u32`.
#[cfg(feature = "gen-catalog")]
const CATALOG_VERSION: u32 = 4;

/// Display-only date the vendored snapshots under `catalog_data/` were
/// fetched, hand-maintained here (never the wall clock -- see the module
/// doc's determinism note) and rendered into `catalog_baked.rs` as a
/// separate `&str` const from `CATALOG_VERSION`.
#[cfg(feature = "gen-catalog")]
const CATALOG_SNAPSHOT_DATE: &str = "2026-08-04";

#[cfg(feature = "gen-catalog")]
const LITELLM_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/catalog_data/litellm_model_prices_and_context_window.json"
));
#[cfg(feature = "gen-catalog")]
const MODELS_DEV_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/catalog_data/models_dev.json"
));
#[cfg(feature = "gen-catalog")]
const ALLOWLIST_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/catalog_data/cross_check_allowlist.json"
));

/// One generated cell, pre-render. Mirrors [`CatalogRow`]'s fields plus
/// the `(provider_kind, model_glob)` key; `capabilities` is a `Vec` (not
/// the row's `BTreeMap`) purely to control rendered-source key order.
///
/// This type's derivation functions ([`derive_cells`] and everything it
/// calls) stay compiled and reachable regardless of `gen-catalog`:
/// `crate::catalog_import::build_import_candidate` is the runtime caller
/// that drives them unconditionally, reading `wm` / `rm` / `ttl_seconds`
/// / `min_prefix_tokens` / `max_context_tokens` / `max_output_tokens` /
/// `capabilities` off
/// every derived cell (via its own group-and-agree mapping). Only
/// `provider_kind` / `model_glob` / `auto_cacher` / `tier` stay
/// gen-catalog-gated -- the import path re-derives its own selector
/// attribution and never reads those four fields off `GeneratedCell`
/// itself, so they go unread outside the `gen-catalog` render pipeline
/// (`render_cell`, `tier_rank`) and outside tests, and are marked
/// `allow(dead_code)` individually rather than for the whole struct.
/// `pub(crate)` so that runtime caller in another module of this crate
/// can hold the cells [`derive_cells`] returns.
pub(crate) struct GeneratedCell {
    #[cfg_attr(not(feature = "gen-catalog"), allow(dead_code))]
    pub(crate) provider_kind: &'static str,
    #[cfg_attr(not(feature = "gen-catalog"), allow(dead_code))]
    pub(crate) model_glob: &'static str,
    pub(crate) wm: f32,
    pub(crate) rm: f32,
    pub(crate) ttl_seconds: u32,
    pub(crate) min_prefix_tokens: u32,
    #[cfg_attr(not(feature = "gen-catalog"), allow(dead_code))]
    pub(crate) auto_cacher: bool,
    #[cfg_attr(not(feature = "gen-catalog"), allow(dead_code))]
    pub(crate) tier: Option<&'static str>,
    pub(crate) max_context_tokens: Option<u32>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) input_cost_per_token: Option<f32>,
    pub(crate) output_cost_per_token: Option<f32>,
    pub(crate) capabilities: Vec<(&'static str, bool)>,
}

/// An allowlisted resolution for one cross-check mismatch, loaded from
/// `catalog_data/cross_check_allowlist.json`.
struct AllowlistEntry {
    resolved: Value,
}

/// `pub(crate)` so it can appear in [`derive_cells`]'s signature:
/// `crate::catalog_import::build_import_candidate` needs one too (see
/// [`Allowlist::empty`]), since the cross-check logic [`derive_cells`]
/// shares with the codegen path takes an allowlist regardless of the
/// source.
pub(crate) struct Allowlist(BTreeMap<String, AllowlistEntry>);

impl Allowlist {
    /// An allowlist with no entries: every cross-check mismatch fails
    /// closed. The import path (`crate::catalog_import`) always runs
    /// `derive_cells` with this -- the checked-in
    /// `cross_check_allowlist.json` resolves noise specific to the
    /// vendored codegen snapshots, which does not apply to freshly
    /// fetched sources.
    pub(crate) const fn empty() -> Self {
        Self(BTreeMap::new())
    }

    #[cfg_attr(not(feature = "gen-catalog"), allow(dead_code))]
    fn parse(raw: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(raw)
            .map_err(|e| format!("parse cross_check_allowlist.json: {e}"))?;
        let Value::Object(map) = root else {
            return Err("cross_check_allowlist.json: expected a top-level JSON object".to_string());
        };
        let mut out = BTreeMap::new();
        for (key, entry) in map {
            let reason = entry.get("reason").and_then(Value::as_str);
            if reason.is_none_or(str::is_empty) {
                return Err(format!(
                    "cross_check_allowlist.json[\"{key}\"]: missing a non-empty \"reason\""
                ));
            }
            let resolved = entry
                .get("resolved")
                .ok_or_else(|| {
                    format!("cross_check_allowlist.json[\"{key}\"]: missing \"resolved\"")
                })?
                .clone();
            out.insert(key, AllowlistEntry { resolved });
        }
        Ok(Self(out))
    }

    fn resolved_f64(&self, key: &str) -> Option<Result<f64, String>> {
        self.0.get(key).map(|e| {
            e.resolved.as_f64().ok_or_else(|| {
                format!("cross_check_allowlist.json[\"{key}\"]: \"resolved\" is not a number")
            })
        })
    }

    fn resolved_bool(&self, key: &str) -> Option<Result<bool, String>> {
        self.0.get(key).map(|e| {
            e.resolved.as_bool().ok_or_else(|| {
                format!("cross_check_allowlist.json[\"{key}\"]: \"resolved\" is not a bool")
            })
        })
    }
}

/// Render the full checked-in `catalog_baked.rs` source, deriving every
/// row from the vendored snapshots (see the module doc). Panics with a
/// descriptive message on a parse failure or an un-allowlisted
/// cross-check mismatch -- there is no silent-fallback path, by design
/// (see the module doc).
///
/// Pipes the rendered text through `rustfmt` (using this repo's
/// `rustfmt.toml`) before returning: the raw text this module emits is
/// syntactically valid but not necessarily canonically formatted (e.g. a
/// `capabilities` slice literal that overflows `max_width`), and the
/// drift-guard test compares this output BYTE-FOR-BYTE against the
/// committed, `cargo fmt`-formatted `catalog_baked.rs`. Falls back to the
/// unformatted text if `rustfmt` is not on `PATH` -- the drift-guard test
/// then fails with an actionable diff instead of silently passing on
/// non-canonical output.
#[cfg(feature = "gen-catalog")]
#[must_use]
pub fn render_catalog_baked_rs() -> String {
    let raw = try_render().unwrap_or_else(|e| panic!("gen_catalog: {e}"));
    rustfmt(&raw).unwrap_or(raw)
}

/// Format `src` (a complete Rust source file) with `rustfmt`, using this
/// repo's `rustfmt.toml` explicitly (stdin-mode `rustfmt` does not walk up
/// from the process's cwd to discover it). Returns `None` on any failure
/// (rustfmt missing, non-UTF8 output, non-Rust input) so the caller can
/// fall back rather than panic.
#[cfg(feature = "gen-catalog")]
fn rustfmt(src: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let config_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../rustfmt.toml");
    let mut child = Command::new("rustfmt")
        .arg("--config-path")
        .arg(config_path)
        .arg("--emit")
        .arg("stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(src.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout).ok()
}

#[cfg(feature = "gen-catalog")]
fn try_render() -> Result<String, String> {
    let litellm: Value =
        serde_json::from_str(LITELLM_JSON).map_err(|e| format!("parse litellm snapshot: {e}"))?;
    let models_dev: Value = serde_json::from_str(MODELS_DEV_JSON)
        .map_err(|e| format!("parse models.dev snapshot: {e}"))?;
    let allowlist = Allowlist::parse(ALLOWLIST_JSON)?;

    let mut cells = Vec::new();
    for (_, result) in derive_cells(&litellm, &models_dev, &allowlist) {
        cells.extend(result?);
    }

    cells.sort_by(|a, b| {
        (a.provider_kind, a.model_glob, tier_rank(a.tier)).cmp(&(
            b.provider_kind,
            b.model_glob,
            tier_rank(b.tier),
        ))
    });

    Ok(render_source(&cells))
}

/// Derive every generated cell from the two source `Value`s, one entry per
/// static selector (see `catalog_codegen_selectors`), each tagged with its
/// selector key (`"provider_kind:model_glob"`, via
/// `crate::catalog_state::selector_key`) so a caller can attribute an `Err`
/// back to the selector that produced it without re-deriving the key from a
/// `GeneratedCell` an error variant never carries. A tiered Anthropic-shaped
/// selector's entry carries its 5m row and, when the source publishes a 1h
/// price, its 1h row together (see [`anthropic_like_cells`]); every other
/// selector's entry carries exactly one row. Unlike `try_render`'s prior
/// inline loop, this does not short-circuit on the first error: every
/// selector is derived so a caller can partition ok/err per selector (a
/// healthy source disagreement on one family should not hide the outcome of
/// every other family). `allowlist` is shared across every selector,
/// matching the codegen path's single vendored `cross_check_allowlist.json`;
/// a caller deriving from freshly fetched sources instead of the vendored
/// snapshots passes an empty one.
///
/// Compiled in regardless of `gen-catalog`: `try_render` calls this on
/// the include_str snapshots (feature-gated), and
/// `crate::catalog_import::build_import_candidate` (never feature-gated)
/// calls it on freshly fetched sources -- the actual runtime caller, not
/// a hypothetical future one.
pub(crate) fn derive_cells(
    litellm: &Value,
    models_dev: &Value,
    allowlist: &Allowlist,
) -> Vec<(String, Result<Vec<GeneratedCell>, String>)> {
    let mut out = Vec::new();
    for sel in ANTHROPIC_SELECTORS {
        let key = crate::catalog_state::selector_key("anthropic-api", sel.model_glob);
        out.push((
            key,
            anthropic_like_cells(
                "anthropic-api",
                sel,
                "anthropic",
                litellm,
                models_dev,
                allowlist,
            ),
        ));
    }
    for sel in BEDROCK_SELECTORS {
        let key = crate::catalog_state::selector_key("bedrock", sel.model_glob);
        out.push((
            key,
            anthropic_like_cells(
                "bedrock",
                sel,
                "amazon-bedrock",
                litellm,
                models_dev,
                allowlist,
            ),
        ));
    }
    for sel in OPENAI_RESPONSES_SELECTORS {
        let key = crate::catalog_state::selector_key("openai-responses", sel.model_glob);
        out.push((
            key,
            auto_cacher_cell("openai-responses", sel, litellm, models_dev, allowlist)
                .map(|cell| vec![cell]),
        ));
    }
    for sel in OPENAI_COMPAT_SELECTORS {
        let key = crate::catalog_state::selector_key("openai-compat", sel.model_glob);
        out.push((
            key,
            auto_cacher_cell("openai-compat", sel, litellm, models_dev, allowlist)
                .map(|cell| vec![cell]),
        ));
    }
    for catch_all in CATCH_ALL_ROWS {
        let key = crate::catalog_state::selector_key(catch_all.provider_kind, "*");
        out.push((
            key,
            Ok(vec![GeneratedCell {
                provider_kind: catch_all.provider_kind,
                model_glob: "*",
                wm: catch_all.wm,
                rm: catch_all.rm,
                ttl_seconds: catch_all.ttl_seconds,
                min_prefix_tokens: catch_all.min_prefix_tokens,
                auto_cacher: catch_all.auto_cacher,
                tier: None,
                max_context_tokens: None,
                // A provider-kind catch-all matches every model the kind
                // serves, at every ceiling, so there is no one output
                // ceiling to bake -- fail-closed, same as its window.
                max_output_tokens: None,
                // A provider-kind catch-all has no backing model to price:
                // it matches every model the kind serves, at every price
                // point. Fail-closed, same as its `max_context_tokens`.
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: Vec::new(),
            }]),
        ));
    }
    out
}

/// Sort key for a cell's tier: groups the tier-agnostic / catch-all rows
/// first, then 5-minute, then 1-hour, for a stable and readable order
/// within an equal `(provider_kind, model_glob)` pair.
#[cfg(feature = "gen-catalog")]
fn tier_rank(tier: Option<&str>) -> u8 {
    match tier {
        None => 0,
        Some("5m") => 1,
        Some("1h") => 2,
        Some(_) => 3,
    }
}

/// Derive both the 5-minute row (required) and, when the source publishes
/// an above-1h price, the 1-hour row for one Anthropic-shaped selector.
fn anthropic_like_cells(
    provider_kind: &'static str,
    sel: &TieredSelector,
    models_dev_provider: &str,
    litellm: &Value,
    models_dev: &Value,
    allowlist: &Allowlist,
) -> Result<Vec<GeneratedCell>, String> {
    let selector_id = format!("{provider_kind}:{}", sel.model_glob);
    let entry = litellm.get(sel.litellm_key).ok_or_else(|| {
        format!(
            "{selector_id}: litellm key `{}` not found in the vendored snapshot",
            sel.litellm_key
        )
    })?;
    let md_entry = models_dev
        .get(models_dev_provider)
        .and_then(|p| p.get("models"))
        .and_then(|m| m.get(sel.models_dev_model));

    let rm = resolve_f64(
        &selector_id,
        "rm",
        md_entry.and_then(models_dev_rm),
        litellm_rm(entry),
        allowlist,
    )?
    .ok_or_else(|| format!("{selector_id}: no source publishes a cache_read price"))?;
    let wm_5m = resolve_f64(
        &selector_id,
        "wm",
        md_entry.and_then(models_dev_wm),
        litellm_wm(entry, "cache_creation_input_token_cost"),
        allowlist,
    )?
    .ok_or_else(|| format!("{selector_id}: no source publishes a 5m cache-write price"))?;
    // The 1h price has no models.dev counterpart at all: litellm is the
    // sole source, and its absence means "no 1h tier", not "free".
    let wm_1h = litellm_wm(entry, "cache_creation_input_token_cost_above_1hr");

    let ctx = resolve_u32(
        &selector_id,
        "max_context_tokens",
        md_entry.and_then(models_dev_context),
        litellm_context(entry),
        allowlist,
    )?;
    let capabilities = capabilities_for(&selector_id, entry, md_entry, allowlist)?;
    let output_ceiling = output_ceiling_for(
        &selector_id,
        sel.model_glob,
        sel.output_ambiguous,
        ctx,
        litellm,
        md_entry,
        allowlist,
    )?;
    // Every tiered selector's glob is pinned to one model generation, so
    // (unlike the vendor-wide openai-compat prefixes) it uses the plain
    // representative-entry cross-check rather than the glob-wide unanimity
    // gate `base_rates_for` applies for an [`AutoCacherSelector`] -- see
    // `pinned_base_rates_for`.
    let (input_cost_per_token, output_cost_per_token) =
        pinned_base_rates_for(&selector_id, entry, md_entry, allowlist)?;

    let mut out = vec![GeneratedCell {
        provider_kind,
        model_glob: sel.model_glob,
        wm: wm_5m as f32,
        rm: rm as f32,
        ttl_seconds: 300,
        min_prefix_tokens: sel.min_prefix_tokens,
        auto_cacher: false,
        tier: Some("5m"),
        max_context_tokens: ctx,
        max_output_tokens: output_ceiling,
        input_cost_per_token,
        output_cost_per_token,
        capabilities: capabilities.clone(),
    }];
    if let Some(wm_1h) = wm_1h {
        out.push(GeneratedCell {
            provider_kind,
            model_glob: sel.model_glob,
            wm: wm_1h as f32,
            rm: rm as f32,
            ttl_seconds: 3_600,
            min_prefix_tokens: sel.min_prefix_tokens,
            auto_cacher: false,
            tier: Some("1h"),
            max_context_tokens: ctx,
            max_output_tokens: output_ceiling,
            input_cost_per_token,
            output_cost_per_token,
            capabilities,
        });
    }
    Ok(out)
}

/// Derive one tier-agnostic auto-cacher row.
fn auto_cacher_cell(
    provider_kind: &'static str,
    sel: &AutoCacherSelector,
    litellm: &Value,
    models_dev: &Value,
    allowlist: &Allowlist,
) -> Result<GeneratedCell, String> {
    let selector_id = format!("{provider_kind}:{}", sel.model_glob);
    let entry = litellm.get(sel.litellm_key).ok_or_else(|| {
        format!(
            "{selector_id}: litellm key `{}` not found in the vendored snapshot",
            sel.litellm_key
        )
    })?;
    let md_entry = models_dev
        .get(sel.models_dev_provider)
        .and_then(|p| p.get("models"))
        .and_then(|m| m.get(sel.models_dev_model));

    let ctx = context_window_for(
        &selector_id,
        sel.model_glob,
        sel.context_ambiguous,
        litellm,
        md_entry,
        allowlist,
    )?;
    let capabilities = capabilities_for(&selector_id, entry, md_entry, allowlist)?;
    let output_ceiling = output_ceiling_for(
        &selector_id,
        sel.model_glob,
        sel.output_ambiguous,
        ctx,
        litellm,
        md_entry,
        allowlist,
    )?;
    let (input_cost_per_token, output_cost_per_token) = base_rates_for(
        &selector_id,
        sel.price_ambiguous,
        sel.model_glob,
        litellm,
        md_entry,
        allowlist,
    )?;

    if sel.economics_unconfirmed {
        let sentinel = CatalogRow::sentinel();
        return Ok(GeneratedCell {
            provider_kind,
            model_glob: sel.model_glob,
            wm: sentinel.wm,
            rm: sentinel.rm,
            ttl_seconds: sentinel.ttl_seconds,
            min_prefix_tokens: sentinel.min_prefix_tokens,
            auto_cacher: sentinel.auto_cacher,
            tier: None,
            max_context_tokens: ctx,
            max_output_tokens: output_ceiling,
            input_cost_per_token,
            output_cost_per_token,
            capabilities,
        });
    }

    let rm = resolve_f64(
        &selector_id,
        "rm",
        md_entry.and_then(models_dev_rm),
        litellm_rm(entry),
        allowlist,
    )?
    .ok_or_else(|| format!("{selector_id}: no source publishes a cache_read price"))?;
    let wm = resolve_f64(
        &selector_id,
        "wm",
        md_entry.and_then(models_dev_wm),
        litellm_wm(entry, "cache_creation_input_token_cost"),
        allowlist,
    )?
    // No source publishes a separate cache-write price for an auto-cacher
    // family: the write is folded into ordinary input billing (no
    // premium), the documented behavior of every such vendor this table
    // covers -- see the module doc.
    .unwrap_or(1.0);

    Ok(GeneratedCell {
        provider_kind,
        model_glob: sel.model_glob,
        wm: wm as f32,
        rm: rm as f32,
        ttl_seconds: sel.ttl_seconds,
        min_prefix_tokens: sel.min_prefix_tokens,
        auto_cacher: sel.auto_cacher,
        tier: None,
        max_context_tokens: ctx,
        max_output_tokens: output_ceiling,
        input_cost_per_token,
        output_cost_per_token,
        capabilities,
    })
}

/// Derive a tiered (Anthropic-shaped) selector's base per-token rates from
/// its single representative litellm entry, cross-checked against
/// models.dev through the ordinary [`resolve_f64`] path: agreement or
/// single-source data passes through, a disagreement fails generation
/// unless the allowlist resolves it.
///
/// Unlike [`base_rates_for`] (which an [`AutoCacherSelector`]'s vendor-wide
/// glob needs the glob-scan unanimity gate for), a [`TieredSelector`]'s glob
/// is pinned to one model generation with no curated `price_ambiguous`
/// escape hatch of its own: `output_ceiling_for` derives its ceiling
/// mechanically for both selector types, but the base rates here still
/// cross-check only the one representative entry, with no glob-span
/// unanimity gate backing that cross-check.
fn pinned_base_rates_for(
    selector_id: &str,
    litellm_entry: &Value,
    models_dev_entry: Option<&Value>,
    allowlist: &Allowlist,
) -> Result<(Option<f32>, Option<f32>), String> {
    let rate = |field: &str, models_dev_field: &str| -> Result<Option<f32>, String> {
        Ok(resolve_f64(
            selector_id,
            field,
            models_dev_entry.and_then(|e| models_dev_rate(e, models_dev_field)),
            f64_field(litellm_entry, field),
            allowlist,
        )?
        .and_then(narrow_rate))
    };
    Ok((
        rate("input_cost_per_token", "input")?,
        rate("output_cost_per_token", "output")?,
    ))
}

/// Derive the base per-token input/output rates for one auto-cacher
/// selector through the SAME two-source cross-check the multipliers use:
/// agreement or single-source data passes through, a disagreement fails
/// generation unless the allowlist resolves it.
///
/// `price_ambiguous` forces both rates to `None`: the selector's glob
/// matches models the snapshots price very differently (a bare `"*"`
/// catch-all, or a vendor-wide prefix spanning an embedding model and a
/// flagship), so the representative model's rate would be confidently wrong
/// for most of what the glob serves. That mirrors
/// [`AutoCacherSelector::context_ambiguous`]'s posture for the window:
/// ABSENT beats a guess.
///
/// `price_ambiguous` is a curated SUPPRESSOR, never a source of truth: the
/// litellm side of each rate is derived from EVERY litellm entry the
/// selector's glob spans agreeing (see [`litellm_unanimous_rate`]), the same
/// mechanical unanimity [`output_ceiling_for`] applies to the output
/// ceiling. A non-unanimous glob span bakes nothing regardless of the flag;
/// the flag can only withhold a rate the snapshots would otherwise support,
/// so a stale flag can never bake an incoherently-priced rate.
/// `price_ambiguous` disagreeing with the mechanical verdict is caught by
/// `every_selectors_price_ambiguous_flag_matches_the_snapshots`.
fn base_rates_for(
    selector_id: &str,
    price_ambiguous: bool,
    model_glob: &str,
    litellm: &Value,
    models_dev_entry: Option<&Value>,
    allowlist: &Allowlist,
) -> Result<(Option<f32>, Option<f32>), String> {
    if price_ambiguous {
        return Ok((None, None));
    }
    let rate = |field: &str, models_dev_field: &str| -> Result<Option<f32>, String> {
        let Some(litellm_rate) = litellm_unanimous_rate(litellm, model_glob, field) else {
            return Ok(None);
        };
        Ok(resolve_f64(
            selector_id,
            field,
            models_dev_entry.and_then(|e| models_dev_rate(e, models_dev_field)),
            Some(litellm_rate),
            allowlist,
        )?
        .and_then(narrow_rate))
    };
    Ok((
        rate("input_cost_per_token", "input")?,
        rate("output_cost_per_token", "output")?,
    ))
}

/// The single per-token rate (`field`, one of `input_cost_per_token` /
/// `output_cost_per_token`) that EVERY litellm entry the selector's glob
/// spans publishes: `None` when two spanned entries disagree, or when no
/// spanned entry publishes a usable figure. Mirrors
/// [`litellm_unanimous_output_ceiling`] for the price fields, using
/// [`approx_eq`] rather than exact equality since these are floating-point
/// dollar rates independently rounded by the source.
fn litellm_unanimous_rate(litellm: &Value, model_glob: &str, field: &str) -> Option<f64> {
    let mut agreed: Option<f64> = None;
    for (key, entry) in litellm.as_object()? {
        if !glob_spans_litellm_key(model_glob, key) {
            continue;
        }
        let Some(rate) = f64_field(entry, field) else {
            continue;
        };
        match agreed {
            None => agreed = Some(rate),
            Some(previous) if approx_eq(previous, rate) => {}
            Some(_) => return None,
        }
    }
    agreed
}

/// Narrow a source `f64` per-token rate to the baked `f32`, or drop it
/// (priced-ABSENT, fail-closed) when the result would not be a usable
/// price. A dropped rate leaves the cell unpriced, which downstream
/// already handles; a silently wrong one poisons every estimate built on
/// it.
///
/// Rejects, in order: a non-finite or negative SOURCE value (corruption,
/// not a price); a value that OVERFLOWS to infinity on the narrowing cast
/// (`f64` carries rates `f32` cannot represent); and a positive value that
/// UNDERFLOWS to zero on the cast (`f32`'s smallest subnormal is ~1e-45,
/// and a rate that collapses to zero would read as a free tier). The
/// order matters: the finiteness check has to run on the NARROWED value,
/// since a finite `f64` is exactly what becomes `f32::INFINITY`.
///
/// A source zero passes through unchanged -- a genuinely free tier is a
/// real vendor offering, not a degenerate value.
fn narrow_rate(rate: f64) -> Option<f32> {
    if !rate.is_finite() || rate < 0.0 {
        return None;
    }
    let narrowed = rate as f32;
    if !narrowed.is_finite() {
        return None;
    }
    if rate > 0.0 && narrowed == 0.0 {
        return None;
    }
    Some(narrowed)
}

fn capabilities_for(
    selector_id: &str,
    litellm_entry: &Value,
    models_dev_entry: Option<&Value>,
    allowlist: &Allowlist,
) -> Result<Vec<(&'static str, bool)>, String> {
    let mut out = Vec::new();
    if let Some(v) = present_bool(litellm_entry, "supports_web_search") {
        out.push((WEB_SEARCH, v));
    }
    if let Some(v) = present_bool(litellm_entry, "supports_computer_use") {
        out.push((COMPUTER_USE, v));
    }
    let md_structured = models_dev_entry
        .and_then(|e| e.get("structured_output"))
        .and_then(Value::as_bool);
    let ll_structured = present_bool(litellm_entry, "supports_response_schema");
    if let Some(v) = resolve_bool(
        selector_id,
        "structured_output",
        md_structured,
        ll_structured,
        allowlist,
    )? {
        out.push((STRUCTURED_OUTPUT, v));
    }
    Ok(out)
}

/// `true` only when `field` is a PRESENT key on `entry` (an absent key is
/// "no data", never an implicit `false` -- see the module doc).
fn present_bool(entry: &Value, field: &str) -> Option<bool> {
    entry.get(field).and_then(Value::as_bool)
}

fn f64_field(entry: &Value, field: &str) -> Option<f64> {
    entry.get(field).and_then(Value::as_f64)
}

fn litellm_rm(entry: &Value) -> Option<f64> {
    let input = f64_field(entry, "input_cost_per_token")?;
    let cache_read = f64_field(entry, "cache_read_input_token_cost")?;
    (input > 0.0).then_some(cache_read / input)
}

fn litellm_wm(entry: &Value, field: &str) -> Option<f64> {
    let input = f64_field(entry, "input_cost_per_token")?;
    let cache_write = f64_field(entry, field)?;
    (input > 0.0).then_some(cache_write / input)
}

fn litellm_context(entry: &Value) -> Option<u32> {
    f64_field(entry, "max_input_tokens").map(|v| v as u32)
}

fn models_dev_rm(entry: &Value) -> Option<f64> {
    let cost = entry.get("cost")?;
    let input = cost.get("input")?.as_f64()?;
    let cache_read = cost.get("cache_read")?.as_f64()?;
    (input > 0.0).then_some(cache_read / input)
}

fn models_dev_wm(entry: &Value) -> Option<f64> {
    let cost = entry.get("cost")?;
    let input = cost.get("input")?.as_f64()?;
    let cache_write = cost.get("cache_write")?.as_f64()?;
    (input > 0.0).then_some(cache_write / input)
}

fn models_dev_context(entry: &Value) -> Option<u32> {
    entry
        .get("limit")?
        .get("context")?
        .as_f64()
        .map(|v| v as u32)
}

/// Upper sanity bound on a baked output ceiling. No shipped model's
/// output ceiling comes near a million tokens, so a larger figure is a
/// source-data error (a context window landing in an output field, a unit
/// slip) rather than a real limit -- and a wrongly-huge ceiling is exactly
/// the value that gets every request rejected upstream.
const MAX_OUTPUT_TOKENS_SAFETY_BOUND: u32 = 1_000_000;

/// The allowlist field name (and the label in a generation error) for the
/// output ceiling.
const OUTPUT_CEILING_FIELD: &str = "max_output_tokens";

/// One source's published output ceiling, narrowed to `u32` or dropped.
///
/// Rejects a non-finite, non-positive, or fractional figure (an output
/// ceiling is a whole positive token count -- a fraction means the field
/// held something other than a ceiling), and anything that overflows `u32`
/// or exceeds [`MAX_OUTPUT_TOKENS_SAFETY_BOUND`]. A dropped figure leaves
/// the source with no opinion, which the cross-check then treats as
/// single-source or absent data rather than as a disagreement.
fn narrow_output_ceiling(raw: f64) -> Option<u32> {
    if !raw.is_finite() || raw <= 0.0 || raw.fract() != 0.0 {
        return None;
    }
    let ceiling = u32::try_from(raw as u64).ok()?;
    (ceiling <= MAX_OUTPUT_TOKENS_SAFETY_BOUND).then_some(ceiling)
}

fn litellm_output_ceiling(entry: &Value) -> Option<u32> {
    f64_field(entry, OUTPUT_CEILING_FIELD).and_then(narrow_output_ceiling)
}

fn models_dev_output_ceiling(entry: &Value) -> Option<u32> {
    entry
        .get("limit")?
        .get("output")?
        .as_f64()
        .and_then(narrow_output_ceiling)
}

/// Whether `model_glob` spans the litellm snapshot entry keyed `key`.
///
/// A litellm key prefixes a HOSTING ROUTE onto the vendor model id
/// (`vertex_ai/claude-sonnet-4-6`,
/// `bedrock/us-gov-east-1/anthropic.claude-sonnet-4-5-20250929-v1:0`), and
/// a re-listing of the same model generation is precisely the entry a
/// selector's glob has to agree with -- a gateway that caps output lower
/// than the direct API is the disagreement this rule exists to catch. So
/// the id after the last route segment is matched as well as the whole
/// key. A bare `"*"` spans every entry (the provider catch-all shape
/// [`AliasPattern`] itself rejects, handled here the way
/// `crate::catalog`'s own selector matcher handles it).
fn glob_spans_litellm_key(model_glob: &str, key: &str) -> bool {
    if model_glob == "*" {
        return true;
    }
    let Ok(pattern) = AliasPattern::parse(model_glob) else {
        return false;
    };
    pattern.matches(key)
        || key
            .rsplit('/')
            .next()
            .is_some_and(|model_id| pattern.matches(model_id))
}

/// The single output ceiling that EVERY litellm entry the selector's glob
/// spans publishes: `None` when two spanned entries disagree, or when no
/// spanned entry publishes a usable figure.
///
/// Mechanically derived from the snapshot rather than read off the
/// selector's one representative entry -- one entry's figure says nothing
/// about the other ids the glob will match at dispatch time.
fn litellm_unanimous_output_ceiling(litellm: &Value, model_glob: &str) -> Option<u32> {
    let mut agreed: Option<u32> = None;
    for (key, entry) in litellm.as_object()? {
        if !glob_spans_litellm_key(model_glob, key) {
            continue;
        }
        let Some(ceiling) = litellm_output_ceiling(entry) else {
            continue;
        };
        match agreed {
            None => agreed = Some(ceiling),
            Some(previous) if previous == ceiling => {}
            Some(_) => return None,
        }
    }
    agreed
}

/// The single confirmed context window that EVERY litellm entry the
/// selector's glob spans publishes: `None` when two spanned entries
/// disagree, or when no spanned entry publishes a usable figure. Mirrors
/// [`litellm_unanimous_output_ceiling`] for `max_input_tokens`.
fn litellm_unanimous_context_window(litellm: &Value, model_glob: &str) -> Option<u32> {
    let mut agreed: Option<u32> = None;
    for (key, entry) in litellm.as_object()? {
        if !glob_spans_litellm_key(model_glob, key) {
            continue;
        }
        let Some(window) = litellm_context(entry) else {
            continue;
        };
        match agreed {
            None => agreed = Some(window),
            Some(previous) if previous == window => {}
            Some(_) => return None,
        }
    }
    agreed
}

/// Derive one auto-cacher selector's baked confirmed context window.
/// `context_ambiguous` short-circuits to `None`, but the mechanical
/// derivation runs regardless of the flag's value: every litellm entry the
/// glob spans has to agree on a figure ([`litellm_unanimous_context_window`])
/// before it is cross-checked against models.dev's single representative
/// figure through the ordinary [`resolve_f64`] path (single-source data is
/// still enough here, unlike the output ceiling -- see the module doc).
///
/// `context_ambiguous` is a curated SUPPRESSOR, never a source of truth: it
/// can only withhold a window the snapshots would otherwise support, so a
/// stale flag can never bake an incoherent window.
/// `context_ambiguous` disagreeing with the mechanical verdict is caught by
/// `every_selectors_context_ambiguous_flag_matches_the_snapshots`.
fn context_window_for(
    selector_id: &str,
    model_glob: &str,
    context_ambiguous: bool,
    litellm: &Value,
    models_dev_entry: Option<&Value>,
    allowlist: &Allowlist,
) -> Result<Option<u32>, String> {
    if context_ambiguous {
        return Ok(None);
    }
    let Some(litellm_window) = litellm_unanimous_context_window(litellm, model_glob) else {
        return Ok(None);
    };
    resolve_u32(
        selector_id,
        "max_context_tokens",
        models_dev_entry.and_then(models_dev_context),
        Some(litellm_window),
        allowlist,
    )
}

/// Derive one selector's baked output ceiling under STRICT coherence (see
/// the module doc's OUTPUT CEILING note). In order: every litellm entry the
/// glob spans must agree on a figure that survives
/// [`narrow_output_ceiling`]; models.dev must publish one too; the two must
/// agree; and the result may not exceed the selector's own confirmed
/// context window (a model cannot emit more than its window holds -- a
/// selector with no confirmed window has nothing to check against and keeps
/// the figure).
///
/// A cross-SOURCE disagreement yields NO VALUE rather than failing
/// generation, unless the allowlist records a deliberate resolution for
/// `"{selector_id}:max_output_tokens"` -- the one field where absent is
/// strictly better than a fail-closed halt, since neither source's figure
/// is more credible than the other's. An allowlist value that is not itself
/// a usable ceiling DOES fail generation: a resolution nobody can bake is a
/// mistake in the allowlist, not source noise.
///
/// `output_ambiguous` is a curated SUPPRESSOR, never a source of truth: it
/// can only withhold a figure the snapshots would otherwise support, so a
/// stale flag can never bake an incoherent ceiling. `output_ambiguous`
/// disagreeing with the mechanical verdict is caught by
/// `every_selectors_output_ambiguous_flag_matches_the_snapshots`.
fn output_ceiling_for(
    selector_id: &str,
    model_glob: &str,
    output_ambiguous: bool,
    max_context_tokens: Option<u32>,
    litellm: &Value,
    models_dev_entry: Option<&Value>,
    allowlist: &Allowlist,
) -> Result<Option<u32>, String> {
    if output_ambiguous {
        return Ok(None);
    }
    let Some(litellm_ceiling) = litellm_unanimous_output_ceiling(litellm, model_glob) else {
        return Ok(None);
    };
    // Single-source data is NOT enough here (unlike the context window):
    // the two feeds disagree on output ceilings far more often than on
    // windows, so an uncorroborated figure is not evidence of a real limit.
    let Some(models_dev_ceiling) = models_dev_entry.and_then(models_dev_output_ceiling) else {
        return Ok(None);
    };
    let resolved = if models_dev_ceiling == litellm_ceiling {
        Some(litellm_ceiling)
    } else {
        allowlisted_output_ceiling(selector_id, models_dev_ceiling, litellm_ceiling, allowlist)?
    };
    Ok(resolved.filter(|ceiling| max_context_tokens.is_none_or(|window| *ceiling <= window)))
}

/// The allowlist's deliberate resolution of one known cross-source output
/// -ceiling disagreement, or `None` when the allowlist does not carry the
/// selector (no value baked -- see [`output_ceiling_for`]). `Err` when the
/// recorded resolution is not a bakeable ceiling.
fn allowlisted_output_ceiling(
    selector_id: &str,
    models_dev: u32,
    litellm: u32,
    allowlist: &Allowlist,
) -> Result<Option<u32>, String> {
    let key = format!("{selector_id}:{OUTPUT_CEILING_FIELD}");
    let Some(resolved) = allowlist.resolved_f64(&key) else {
        return Ok(None);
    };
    let raw = resolved?;
    narrow_output_ceiling(raw).map(Some).ok_or_else(|| {
        format!(
            "cross_check_allowlist.json[\"{key}\"]: \"resolved\" {raw} is not a usable output \
             ceiling (a whole token count in 1..={MAX_OUTPUT_TOKENS_SAFETY_BOUND}); the sources \
             state models.dev={models_dev} litellm={litellm}"
        )
    })
}

/// models.dev publishes `cost.input` / `cost.output` in dollars per MILLION
/// tokens, while litellm's `input_cost_per_token` /
/// `output_cost_per_token` are already per-token. The cache MULTIPLIERS
/// divide two fields from the SAME source, so their units cancel and no
/// conversion is needed there -- these absolute base rates are the first
/// values where the mismatch is load-bearing, and cross-checking the two
/// sources against each other requires one common unit. Per-token is that
/// unit (it is what the row stores and what a cost estimate multiplies by a
/// token count).
const MODELS_DEV_PRICE_UNIT_TOKENS: f64 = 1.0e6;

fn models_dev_rate(entry: &Value, field: &str) -> Option<f64> {
    let rate = entry.get("cost")?.get(field)?.as_f64()?;
    Some(rate / MODELS_DEV_PRICE_UNIT_TOKENS)
}

/// The stable marker every cross-check-disagreement error message from
/// [`derive_cells`] carries -- produced ONLY by [`resolve_f64`] /
/// [`resolve_bool`] on a genuine source disagreement. A missing-source-key
/// or absent-data `Err` does not carry it, which is what lets
/// [`reason_is_cross_check_mismatch`] tell the two apart.
pub(crate) const CROSS_CHECK_MISMATCH_MARKER: &str = "cross-check mismatch";

/// Whether a [`derive_cells`] `Err` reason denotes a source cross-check
/// disagreement (as opposed to a missing source key or absent data): the
/// discriminator [`crate::catalog_import`] uses to tag a skip as
/// counted-toward-totals vs fail-safe not-counted.
#[must_use]
pub(crate) fn reason_is_cross_check_mismatch(reason: &str) -> bool {
    reason.contains(CROSS_CHECK_MISMATCH_MARKER)
}

/// Relative-tolerance float comparison (`1e-6`): the two sources round
/// their published prices independently, so a f64-noise-level difference
/// is not a real mismatch.
fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1e-9)
}

/// Resolve one numeric field from a primary (models.dev) and secondary
/// (litellm) value: agreement or single-source data pass through, a
/// disagreement fails generation unless `catalog_data/cross_check_allowlist.json`
/// carries a `"{selector_id}:{field}"` entry, whose `resolved` value wins.
fn resolve_f64(
    selector_id: &str,
    field: &str,
    primary: Option<f64>,
    secondary: Option<f64>,
    allowlist: &Allowlist,
) -> Result<Option<f64>, String> {
    match (primary, secondary) {
        (Some(p), Some(s)) if approx_eq(p, s) => Ok(Some(p)),
        (Some(p), Some(s)) => {
            let key = format!("{selector_id}:{field}");
            match allowlist.resolved_f64(&key) {
                Some(r) => r.map(Some),
                None => Err(format!(
                    "{CROSS_CHECK_MISMATCH_MARKER} at {key}: models.dev={p} litellm={s}; add an \
                     allowlist entry at catalog_data/cross_check_allowlist.json[\"{key}\"] to \
                     accept one of these, or fix the source data"
                )),
            }
        }
        (Some(v), None) | (None, Some(v)) => Ok(Some(v)),
        (None, None) => Ok(None),
    }
}

fn resolve_u32(
    selector_id: &str,
    field: &str,
    primary: Option<u32>,
    secondary: Option<u32>,
    allowlist: &Allowlist,
) -> Result<Option<u32>, String> {
    let resolved = resolve_f64(
        selector_id,
        field,
        primary.map(f64::from),
        secondary.map(f64::from),
        allowlist,
    )?;
    Ok(resolved.map(|v| v as u32))
}

fn resolve_bool(
    selector_id: &str,
    field: &str,
    primary: Option<bool>,
    secondary: Option<bool>,
    allowlist: &Allowlist,
) -> Result<Option<bool>, String> {
    match (primary, secondary) {
        (Some(p), Some(s)) if p == s => Ok(Some(p)),
        (Some(p), Some(s)) => {
            let key = format!("{selector_id}:{field}");
            match allowlist.resolved_bool(&key) {
                Some(r) => r.map(Some),
                None => Err(format!(
                    "{CROSS_CHECK_MISMATCH_MARKER} at {key}: models.dev={p} litellm={s}; add an \
                     allowlist entry at catalog_data/cross_check_allowlist.json[\"{key}\"] to \
                     accept one of these, or fix the source data"
                )),
            }
        }
        (Some(v), None) | (None, Some(v)) => Ok(Some(v)),
        (None, None) => Ok(None),
    }
}

#[cfg(feature = "gen-catalog")]
fn render_source(cells: &[GeneratedCell]) -> String {
    let mut out = String::new();
    out.push_str(
        "// @generated by `cargo run --bin gen_catalog`. DO NOT EDIT BY HAND.\n\
         //\n\
         // Regenerate: cargo run --bin gen_catalog (from the workspace or this\n\
         // crate), then `cargo fmt`. Source snapshots + provenance:\n\
         // `catalog_data/` (see `catalog_data/NOTICE`).\n\n\
         use std::collections::BTreeMap;\n\n\
         use crate::catalog::CatalogRow;\n\n",
    );
    let _ = writeln!(
        out,
        "/// See `crate::catalog_codegen` for how this is derived."
    );
    let _ = writeln!(out, "pub const CATALOG_VERSION: u32 = {CATALOG_VERSION};\n");
    let _ = writeln!(
        out,
        "/// Display-only; never used in economics. See `crate::catalog_codegen`."
    );
    let _ = writeln!(
        out,
        "pub const CATALOG_SNAPSHOT_DATE: &str = \"{CATALOG_SNAPSHOT_DATE}\";\n"
    );
    out.push_str(
        "/// One generated baked cell. Mirrors `crate::catalog`'s module-private\n\
         /// `BakedCell` shape as a public type, since `catalog_baked` has no\n\
         /// visibility into that module's private items.\n\
         pub struct BakedCatalogCell {\n\
         \x20   pub provider_kind: &'static str,\n\
         \x20   pub model_glob: &'static str,\n\
         \x20   pub row: CatalogRow,\n\
         }\n\n\
         fn capabilities_map(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {\n\
         \x20   pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()\n\
         }\n\n\
         pub fn baked_cells() -> Vec<BakedCatalogCell> {\n\
         \x20   vec![\n",
    );
    for cell in cells {
        render_cell(&mut out, cell);
    }
    out.push_str("    ]\n}\n");
    out
}

#[cfg(feature = "gen-catalog")]
fn render_cell(out: &mut String, cell: &GeneratedCell) {
    let tier = match cell.tier {
        None => "None".to_string(),
        Some(t) => format!("Some({t:?})"),
    };
    let ctx = match cell.max_context_tokens {
        None => "None".to_string(),
        Some(v) => format!("Some({v})"),
    };
    let out_ceiling = match cell.max_output_tokens {
        None => "None".to_string(),
        Some(v) => format!("Some({v})"),
    };
    let caps = if cell.capabilities.is_empty() {
        "BTreeMap::new()".to_string()
    } else {
        let pairs: Vec<String> = cell
            .capabilities
            .iter()
            .map(|(k, v)| format!("({k:?}, {v})"))
            .collect();
        format!("capabilities_map(&[{}])", pairs.join(", "))
    };
    let _ = writeln!(
        out,
        "        BakedCatalogCell {{\n\
         \x20           provider_kind: {:?},\n\
         \x20           model_glob: {:?},\n\
         \x20           row: CatalogRow {{\n\
         \x20               wm: {}_f32,\n\
         \x20               rm: {}_f32,\n\
         \x20               ttl_seconds: {},\n\
         \x20               min_prefix_tokens: {},\n\
         \x20               has_storage_rent: false,\n\
         \x20               storage_rent: 0.0,\n\
         \x20               auto_cacher: {},\n\
         \x20               tier: {},\n\
         \x20               max_context_tokens: {},\n\
         \x20               max_output_tokens: {},\n\
         \x20               input_cost_per_token: {},\n\
         \x20               output_cost_per_token: {},\n\
         \x20               capabilities: {},\n\
         \x20           }},\n\
         \x20       }},",
        cell.provider_kind,
        cell.model_glob,
        cell.wm,
        cell.rm,
        cell.ttl_seconds,
        cell.min_prefix_tokens,
        cell.auto_cacher,
        tier,
        ctx,
        out_ceiling,
        render_rate(cell.input_cost_per_token),
        render_rate(cell.output_cost_per_token),
        caps,
    );
}

/// Render one base per-token rate as Rust source. `{:e}` (not `Display`)
/// because these rates are small enough that decimal notation runs to a
/// dozen leading zeros -- an unreadable literal in a checked-in file that a
/// reviewer is expected to diff. The exponent form round-trips exactly: it
/// is emitted from, and parsed back into, the same `f32`.
#[cfg(feature = "gen-catalog")]
fn render_rate(rate: Option<f32>) -> String {
    match rate {
        None => "None".to_string(),
        Some(v) => format!("Some({v:e}_f32)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "gen-catalog")]
    #[test]
    fn regenerating_matches_the_committed_catalog_baked_file() {
        let fresh = render_catalog_baked_rs();
        let committed = include_str!("catalog_baked.rs");
        assert_eq!(
            fresh, committed,
            "catalog_baked.rs is stale; run `cargo run --bin gen_catalog`, `cargo fmt`, and \
             commit the result"
        );
    }

    #[cfg(feature = "gen-catalog")]
    #[test]
    fn regenerating_twice_is_byte_identical() {
        assert_eq!(render_catalog_baked_rs(), render_catalog_baked_rs());
    }

    #[test]
    fn approx_eq_treats_float_noise_as_equal_but_real_gaps_as_different() {
        assert!(approx_eq(0.1, 0.099_999_999_999_999_99));
        assert!(!approx_eq(0.1, 0.2));
    }

    #[test]
    fn resolve_f64_uses_allowlist_on_a_real_mismatch() {
        let allowlist =
            Allowlist::parse(r#"{"k:f": {"reason": "test", "resolved": 42.0}}"#).expect("parse");
        let resolved = resolve_f64("k", "f", Some(1.0), Some(2.0), &allowlist).expect("resolved");
        assert_eq!(resolved, Some(42.0));
    }

    #[test]
    fn resolve_f64_fails_closed_on_an_unallowlisted_mismatch() {
        let allowlist = Allowlist::parse("{}").expect("parse");
        let err = resolve_f64("k", "f", Some(1.0), Some(2.0), &allowlist)
            .expect_err("must fail without an allowlist entry");
        assert!(err.contains("cross-check mismatch"), "msg: {err}");
    }

    #[test]
    fn reason_is_cross_check_mismatch_only_matches_a_genuine_disagreement() {
        let allowlist = Allowlist::parse("{}").expect("parse");
        let mismatch = resolve_f64("anthropic-api:m", "wm", Some(1.0), Some(2.0), &allowlist)
            .expect_err("mismatch");
        assert!(reason_is_cross_check_mismatch(&mismatch));
        // Representative NON-disagreement errors from `derive_cells`: a
        // missing source key and absent data must NOT be classified as a
        // cross-check disagreement (so they stay uncounted, fail-safe).
        assert!(!reason_is_cross_check_mismatch(
            "anthropic-api:m: litellm key `foo` not found in the vendored snapshot"
        ));
        assert!(!reason_is_cross_check_mismatch(
            "anthropic-api:m: no source publishes a cache_read price"
        ));
    }

    /// Drives `derive_cells` on small in-memory `Value`s keyed to one real
    /// tiered selector (`ANTHROPIC_SELECTORS[0]`) and one real auto-cacher
    /// selector (`OPENAI_RESPONSES_SELECTORS[0]`), leaving every other
    /// static selector's source data absent (their `Err`s are expected and
    /// ignored here). Every source field both selectors need agrees
    /// exactly between the two `Value`s so no allowlist entry is needed.
    #[test]
    fn derive_cells_splits_a_tiered_family_and_derives_an_auto_cacher_family() {
        let tiered = &ANTHROPIC_SELECTORS[0];
        let auto_cacher = &OPENAI_RESPONSES_SELECTORS[0];

        let litellm = serde_json::json!({
            tiered.litellm_key: {
                "input_cost_per_token": 1.0e-5,
                "output_cost_per_token": 5.0e-5,
                "cache_read_input_token_cost": 1.0e-6,
                "cache_creation_input_token_cost": 1.25e-5,
                "cache_creation_input_token_cost_above_1hr": 2.0e-5,
                "max_input_tokens": 200_000.0,
            },
            auto_cacher.litellm_key: {
                "input_cost_per_token": 2.0e-6,
                "output_cost_per_token": 8.0e-6,
                "cache_read_input_token_cost": 2.0e-7,
                "max_input_tokens": 400_000.0,
            },
        });
        let models_dev = serde_json::json!({
            "anthropic": {
                "models": {
                    tiered.models_dev_model: {
                        // models.dev prices are per MILLION tokens, so these
                        // are the same rates the litellm fixture states
                        // per-token (see `MODELS_DEV_PRICE_UNIT_TOKENS`).
                        "cost": {
                            "input": 10.0,
                            "output": 50.0,
                            "cache_read": 1.0,
                            "cache_write": 12.5,
                        },
                        "limit": {"context": 200_000},
                    },
                },
            },
            auto_cacher.models_dev_provider: {
                "models": {
                    auto_cacher.models_dev_model: {
                        "cost": {"input": 2.0, "output": 8.0, "cache_read": 0.2},
                        "limit": {"context": 400_000},
                    },
                },
            },
        });
        let allowlist = Allowlist::parse("{}").expect("parse");

        let results = derive_cells(&litellm, &models_dev, &allowlist);

        let tiered_cells = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .find(|cells| cells[0].model_glob == tiered.model_glob)
            .expect("the tiered selector derives");
        assert_eq!(tiered_cells.len(), 2, "5m/1h split");
        assert_eq!(tiered_cells[0].tier, Some("5m"));
        assert_eq!(tiered_cells[0].wm, 1.25);
        assert_eq!(tiered_cells[1].tier, Some("1h"));
        assert_eq!(tiered_cells[1].wm, 2.0);
        assert_eq!(tiered_cells[0].rm, tiered_cells[1].rm);
        // Base rates cross-check clean across the two unit conventions and
        // are shared by both tiers.
        assert_eq!(tiered_cells[0].input_cost_per_token, Some(1.0e-5));
        assert_eq!(tiered_cells[0].output_cost_per_token, Some(5.0e-5));
        assert_eq!(
            tiered_cells[1].input_cost_per_token,
            tiered_cells[0].input_cost_per_token
        );

        let auto_cacher_cells = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .find(|cells| cells[0].provider_kind == "openai-responses")
            .expect("the auto-cacher selector derives");
        assert_eq!(auto_cacher_cells.len(), 1, "single tier-agnostic row");
        assert!(auto_cacher_cells[0].tier.is_none());
        // The openai-responses `*` glob is `context_ambiguous`: it serves
        // every OpenAI model, so it stays window-ABSENT even though both
        // fixtures publish a window for the representative model.
        assert_eq!(auto_cacher_cells[0].max_context_tokens, None);
        // The openai-responses `*` glob is `price_ambiguous`: it serves every
        // OpenAI model, so it stays priced-ABSENT even though both fixtures
        // publish a rate for the representative model.
        assert_eq!(auto_cacher_cells[0].input_cost_per_token, None);
        assert_eq!(auto_cacher_cells[0].output_cost_per_token, None);
    }

    #[test]
    fn base_rates_normalize_the_two_sources_price_units_before_cross_checking() {
        // Arrange: the SAME rate stated in each source's own unit --
        // models.dev per million tokens, litellm per token. Agreement here
        // is only reachable through the conversion.
        let litellm = serde_json::json!({
            "m": {
                "input_cost_per_token": 3.0e-6,
                "output_cost_per_token": 1.5e-5,
            },
        });
        let models_dev = serde_json::json!({"cost": {"input": 3.0, "output": 15.0}});
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let rates = base_rates_for("k:m", false, "m", &litellm, Some(&models_dev), &allowlist)
            .expect("agreeing rates must not trip the cross-check");

        // Assert
        assert_eq!(rates, (Some(3.0e-6), Some(1.5e-5)));
    }

    #[test]
    fn base_rates_fail_closed_on_an_unallowlisted_price_disagreement() {
        // Arrange: models.dev says $3/M, litellm says $4/M -- a real gap,
        // not a unit artifact.
        let litellm = serde_json::json!({"m": {"input_cost_per_token": 4.0e-6}});
        let models_dev = serde_json::json!({"cost": {"input": 3.0}});
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let err = base_rates_for("k:m", false, "m", &litellm, Some(&models_dev), &allowlist)
            .expect_err("a genuine price disagreement must fail generation");

        // Assert
        assert!(reason_is_cross_check_mismatch(&err), "reason: {err}");
        assert!(err.contains("input_cost_per_token"), "reason: {err}");
    }

    #[test]
    fn base_rates_drop_a_negative_published_rate_rather_than_baking_it() {
        // Arrange: a single source publishing a nonsense negative price (no
        // cross-check to run, so the filter is the only guard).
        let litellm = serde_json::json!({"m": {"input_cost_per_token": -1.0e-6}});
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let rates = base_rates_for("k:m", false, "m", &litellm, None, &allowlist).expect("derives");

        // Assert: priced-ABSENT, not a negative rate.
        assert_eq!(rates, (None, None));
    }

    #[test]
    fn base_rates_keep_a_zero_rate_since_a_free_tier_is_real() {
        // Arrange
        let litellm = serde_json::json!({"m": {"input_cost_per_token": 0.0}});
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let rates = base_rates_for("k:m", false, "m", &litellm, None, &allowlist).expect("derives");

        // Assert
        assert_eq!(rates, (Some(0.0), None));
    }

    // -----------------------------------------------------------------------
    // f64 -> f32 narrowing: a value that only becomes degenerate ON the
    // cast is dropped, not baked.
    // -----------------------------------------------------------------------

    #[test]
    fn base_rates_drop_a_rate_that_overflows_f32_on_the_narrowing_cast() {
        // Arrange: finite as f64, but past f32::MAX (~3.4e38), so the cast
        // yields f32::INFINITY -- a source-value finiteness check alone
        // would wave this through.
        let litellm = serde_json::json!({"m": {"input_cost_per_token": 1.0e300}});
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let rates = base_rates_for("k:m", false, "m", &litellm, None, &allowlist).expect("derives");

        // Assert: priced-ABSENT, never an infinite baked rate.
        assert_eq!(rates, (None, None));
    }

    #[test]
    fn base_rates_drop_a_positive_rate_that_underflows_to_zero_on_the_narrowing_cast() {
        // Arrange: a positive f64 far below f32's smallest subnormal
        // (~1e-45), so the cast collapses it to 0.0 -- which would
        // otherwise read as a free tier rather than a lost value.
        let litellm = serde_json::json!({"m": {"input_cost_per_token": 1.0e-300}});
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let rates = base_rates_for("k:m", false, "m", &litellm, None, &allowlist).expect("derives");

        // Assert: priced-ABSENT, never a spurious zero.
        assert_eq!(rates, (None, None));
    }

    #[test]
    fn narrow_rate_keeps_a_representable_positive_rate_and_an_exact_zero() {
        assert_eq!(narrow_rate(3.0e-6), Some(3.0e-6_f32));
        assert_eq!(narrow_rate(0.0), Some(0.0_f32));
    }

    #[test]
    fn narrow_rate_rejects_non_finite_and_negative_sources() {
        assert_eq!(narrow_rate(f64::NAN), None);
        assert_eq!(narrow_rate(f64::INFINITY), None);
        assert_eq!(narrow_rate(-1.0e-6), None);
    }

    // -----------------------------------------------------------------------
    // Output ceiling: glob-wide litellm unanimity, then two-source
    // agreement, then the sanity and above-context guards. Every case drives
    // `output_ceiling_for` on in-memory source `Value`s, so the assertions
    // pin the RULE rather than whichever figures today's vendored snapshot
    // happens to carry.
    // -----------------------------------------------------------------------

    /// A litellm-shaped SNAPSHOT holding one entry (`"m"`, the id the test
    /// globs match) that publishes just an output ceiling.
    fn litellm_ceiling(raw: Value) -> Value {
        serde_json::json!({"m": {"max_output_tokens": raw}})
    }

    /// A models.dev-shaped entry publishing just an output ceiling.
    fn models_dev_ceiling(raw: Value) -> Value {
        serde_json::json!({"limit": {"output": raw}})
    }

    #[test]
    fn output_ceiling_bakes_the_figure_both_sources_agree_on() {
        // Arrange
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            false,
            Some(200_000),
            &litellm_ceiling(serde_json::json!(64_000)),
            Some(&models_dev_ceiling(serde_json::json!(64_000))),
            &allowlist,
        )
        .expect("agreeing sources must derive");

        // Assert
        assert_eq!(ceiling, Some(64_000));
    }

    #[test]
    fn output_ceiling_is_absent_when_the_two_sources_disagree() {
        // Arrange: a real gap between the two feeds, no allowlist entry.
        // Neither figure is more credible than the other, and a wrong
        // ceiling is worse than an absent one -- so this yields NO VALUE
        // rather than halting generation the way a price mismatch does.
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            false,
            Some(1_000_000),
            &litellm_ceiling(serde_json::json!(64_000)),
            Some(&models_dev_ceiling(serde_json::json!(128_000))),
            &allowlist,
        )
        .expect("a ceiling disagreement withholds a value rather than failing");

        // Assert
        assert_eq!(ceiling, None);
    }

    #[test]
    fn output_ceiling_uses_the_allowlists_resolution_for_a_known_disagreement() {
        // Arrange
        let allowlist = Allowlist::parse(
            r#"{"k:m:max_output_tokens": {"reason": "test", "resolved": 128000}}"#,
        )
        .expect("parse");

        // Act
        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            false,
            Some(1_000_000),
            &litellm_ceiling(serde_json::json!(64_000)),
            Some(&models_dev_ceiling(serde_json::json!(128_000))),
            &allowlist,
        )
        .expect("an allowlisted mismatch must resolve");

        // Assert
        assert_eq!(ceiling, Some(128_000));
    }

    #[test]
    fn output_ceiling_fails_generation_on_an_allowlist_resolution_that_is_not_a_ceiling() {
        // Arrange: a resolution nobody can bake is a mistake in the
        // allowlist, not source noise -- so it halts rather than silently
        // withholding the value.
        let allowlist =
            Allowlist::parse(r#"{"k:m:max_output_tokens": {"reason": "test", "resolved": 0}}"#)
                .expect("parse");

        // Act
        let err = output_ceiling_for(
            "k:m",
            "m",
            false,
            None,
            &litellm_ceiling(serde_json::json!(64_000)),
            Some(&models_dev_ceiling(serde_json::json!(128_000))),
            &allowlist,
        )
        .expect_err("an unbakeable allowlist resolution must fail generation");

        // Assert
        assert!(err.contains("max_output_tokens"), "reason: {err}");
        assert!(err.contains("not a usable output ceiling"), "reason: {err}");
    }

    #[test]
    fn output_ceiling_is_absent_for_an_output_ambiguous_selector_even_when_sources_agree() {
        // Arrange: both sources agree, but the selector is flagged ambiguous,
        // which suppresses the fill regardless of what the sources say.
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            true,
            Some(200_000),
            &litellm_ceiling(serde_json::json!(64_000)),
            Some(&models_dev_ceiling(serde_json::json!(64_000))),
            &allowlist,
        )
        .expect("an ambiguous selector derives without failing");

        // Assert
        assert_eq!(ceiling, None, "an ambiguous glob must contribute nothing");
    }

    #[test]
    fn output_ceiling_is_absent_when_two_glob_matched_litellm_entries_disagree() {
        // Arrange: the selector's own glob spans a direct listing at 64000
        // and a gateway re-listing at 16384. models.dev corroborates the
        // higher figure, so trusting the representative entry alone would
        // bake 64000 for every id the glob matches -- including the one the
        // snapshot itself caps four times lower.
        let allowlist = Allowlist::parse("{}").expect("parse");
        let litellm = serde_json::json!({
            "claude-sonnet-4-6": {"max_output_tokens": 64_000},
            "snowflake/claude-sonnet-4-6": {"max_output_tokens": 16_384},
        });

        // Act
        let ceiling = output_ceiling_for(
            "anthropic-api:claude-sonnet-4-6*",
            "claude-sonnet-4-6*",
            false,
            Some(1_000_000),
            &litellm,
            Some(&models_dev_ceiling(serde_json::json!(64_000))),
            &allowlist,
        )
        .expect("a disagreeing glob derives without failing");

        // Assert
        assert_eq!(
            ceiling, None,
            "a glob spanning two different published ceilings must contribute nothing"
        );
    }

    #[test]
    fn output_ceiling_bakes_when_every_glob_matched_litellm_entry_agrees() {
        // The positive control for the unanimity rule: the same glob shape,
        // the same route-prefixed re-listings, all publishing one figure.
        let allowlist = Allowlist::parse("{}").expect("parse");
        let litellm = serde_json::json!({
            "claude-opus-4-7": {"max_output_tokens": 128_000},
            "claude-opus-4-7-20260416": {"max_output_tokens": 128_000},
            "vertex_ai/claude-opus-4-7": {"max_output_tokens": 128_000},
            "claude-haiku-4-5": {"max_output_tokens": 8_192},
        });

        let ceiling = output_ceiling_for(
            "anthropic-api:claude-opus-4-7*",
            "claude-opus-4-7*",
            false,
            Some(1_000_000),
            &litellm,
            Some(&models_dev_ceiling(serde_json::json!(128_000))),
            &allowlist,
        )
        .expect("derives");

        assert_eq!(ceiling, Some(128_000));
    }

    #[test]
    fn output_ceiling_needs_both_sources_since_one_alone_is_uncorroborated() {
        // Arrange
        let allowlist = Allowlist::parse("{}").expect("parse");
        let litellm_only = litellm_ceiling(serde_json::json!(64_000));
        let models_dev_only = models_dev_ceiling(serde_json::json!(64_000));

        // Act / Assert: litellm alone.
        assert_eq!(
            output_ceiling_for("k:m", "m", false, None, &litellm_only, None, &allowlist)
                .expect("derives"),
            None,
        );

        // Act / Assert: models.dev alone (no litellm entry publishes a ceiling).
        assert_eq!(
            output_ceiling_for(
                "k:m",
                "m",
                false,
                None,
                &serde_json::json!({"m": {}}),
                Some(&models_dev_only),
                &allowlist,
            )
            .expect("derives"),
            None,
        );
    }

    #[test]
    fn output_ceiling_rejects_zero_negative_fractional_and_non_finite_figures() {
        // Arrange: each degenerate figure is published by BOTH sources, so a
        // disagreement can never be what drops it -- only the sanity filter.
        let allowlist = Allowlist::parse("{}").expect("parse");

        for raw in [
            serde_json::json!(0),
            serde_json::json!(-64_000),
            serde_json::json!(64_000.5),
            serde_json::json!(f64::MAX),
        ] {
            // Act
            let ceiling = output_ceiling_for(
                "k:m",
                "m",
                false,
                None,
                &litellm_ceiling(raw.clone()),
                Some(&models_dev_ceiling(raw.clone())),
                &allowlist,
            )
            .expect("a degenerate figure drops rather than failing generation");

            // Assert
            assert_eq!(ceiling, None, "figure {raw} must not bake");
        }
    }

    #[test]
    fn output_ceiling_rejects_a_figure_above_the_safety_bound() {
        // Arrange: a plausible-looking figure that is nonetheless far past
        // any real model's output ceiling -- a context window that landed in
        // an output field.
        let allowlist = Allowlist::parse("{}").expect("parse");
        let absurd = serde_json::json!(MAX_OUTPUT_TOKENS_SAFETY_BOUND + 1);

        // Act
        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            false,
            None,
            &litellm_ceiling(absurd.clone()),
            Some(&models_dev_ceiling(absurd)),
            &allowlist,
        )
        .expect("derives");

        // Assert
        assert_eq!(ceiling, None);
    }

    #[test]
    fn output_ceiling_rejects_a_ceiling_above_the_confirmed_context_window() {
        // Arrange: both sources agree on 256000, but the selector's confirmed
        // window is 200000 -- a model cannot emit more than its window holds,
        // so the pair is incoherent regardless of their agreement.
        let allowlist = Allowlist::parse("{}").expect("parse");

        // Act
        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            false,
            Some(200_000),
            &litellm_ceiling(serde_json::json!(256_000)),
            Some(&models_dev_ceiling(serde_json::json!(256_000))),
            &allowlist,
        )
        .expect("derives");

        // Assert
        assert_eq!(ceiling, None);
    }

    #[test]
    fn output_ceiling_keeps_a_ceiling_exactly_at_the_confirmed_context_window() {
        // The window guard is `<=`: a ceiling equal to the window is
        // coherent (every output token fits), only a larger one is not.
        let allowlist = Allowlist::parse("{}").expect("parse");

        let ceiling = output_ceiling_for(
            "k:m",
            "m",
            false,
            Some(200_000),
            &litellm_ceiling(serde_json::json!(200_000)),
            Some(&models_dev_ceiling(serde_json::json!(200_000))),
            &allowlist,
        )
        .expect("derives");

        assert_eq!(ceiling, Some(200_000));
    }

    #[cfg(feature = "gen-catalog")]
    #[test]
    fn every_selectors_output_ambiguous_flag_matches_the_snapshots() {
        // The curated `output_ambiguous` flag is an ASSERTION about the
        // vendored snapshots, so it has to be checkable against them: a flag
        // that says "ambiguous" where every spanned entry now agrees hides a
        // derivable ceiling, and one that says "coherent" where they disagree
        // documents a coherence the data does not have. Only the mechanical
        // verdict ever drives the fill (see `output_ceiling_for`); this test
        // is what keeps the annotation honest.
        let litellm: Value = serde_json::from_str(LITELLM_JSON).expect("parse litellm snapshot");

        let flagged: Vec<(String, bool)> = ANTHROPIC_SELECTORS
            .iter()
            .map(|sel| ("anthropic-api", sel.model_glob, sel.output_ambiguous))
            .chain(
                BEDROCK_SELECTORS
                    .iter()
                    .map(|sel| ("bedrock", sel.model_glob, sel.output_ambiguous)),
            )
            .chain(
                OPENAI_RESPONSES_SELECTORS
                    .iter()
                    .map(|sel| ("openai-responses", sel.model_glob, sel.output_ambiguous)),
            )
            .chain(
                OPENAI_COMPAT_SELECTORS
                    .iter()
                    .map(|sel| ("openai-compat", sel.model_glob, sel.output_ambiguous)),
            )
            .map(|(kind, glob, flag)| {
                let derived_ambiguous = litellm_unanimous_output_ceiling(&litellm, glob).is_none();
                (
                    crate::catalog_state::selector_key(kind, glob),
                    flag == derived_ambiguous,
                )
            })
            .collect();

        let disagreeing: Vec<&String> = flagged
            .iter()
            .filter_map(|(selector, agrees)| (!agrees).then_some(selector))
            .collect();
        assert!(
            disagreeing.is_empty(),
            "each selector's output_ambiguous flag disagrees with the vendored snapshots: \
             {disagreeing:?}"
        );
    }

    #[cfg(feature = "gen-catalog")]
    #[test]
    fn every_selectors_context_ambiguous_flag_matches_the_snapshots() {
        // Same weld as `every_selectors_output_ambiguous_flag_matches_the_snapshots`,
        // applied to `context_ambiguous`: only the auto-cacher-shaped
        // selectors carry the flag (a tiered selector's glob is pinned to
        // one model generation, never ambiguous by design).
        let litellm: Value = serde_json::from_str(LITELLM_JSON).expect("parse litellm snapshot");

        let flagged: Vec<(String, bool)> = OPENAI_RESPONSES_SELECTORS
            .iter()
            .map(|sel| ("openai-responses", sel.model_glob, sel.context_ambiguous))
            .chain(
                OPENAI_COMPAT_SELECTORS
                    .iter()
                    .map(|sel| ("openai-compat", sel.model_glob, sel.context_ambiguous)),
            )
            .map(|(kind, glob, flag)| {
                let derived_ambiguous = litellm_unanimous_context_window(&litellm, glob).is_none();
                (
                    crate::catalog_state::selector_key(kind, glob),
                    flag == derived_ambiguous,
                )
            })
            .collect();

        let disagreeing: Vec<&String> = flagged
            .iter()
            .filter_map(|(selector, agrees)| (!agrees).then_some(selector))
            .collect();
        assert!(
            disagreeing.is_empty(),
            "each selector's context_ambiguous flag disagrees with the vendored snapshots: \
             {disagreeing:?}"
        );
    }

    #[cfg(feature = "gen-catalog")]
    #[test]
    fn every_selectors_price_ambiguous_flag_matches_the_snapshots() {
        // Same weld, applied to `price_ambiguous`: a selector is coherent
        // only when EVERY litellm entry its glob spans agrees on both base
        // rates. Only `AutoCacherSelector` carries this flag (a
        // `TieredSelector`'s glob uses `pinned_base_rates_for`'s plain
        // representative-entry cross-check instead -- see its doc).
        let litellm: Value = serde_json::from_str(LITELLM_JSON).expect("parse litellm snapshot");

        let flagged: Vec<(String, bool)> = OPENAI_RESPONSES_SELECTORS
            .iter()
            .map(|sel| ("openai-responses", sel.model_glob, sel.price_ambiguous))
            .chain(
                OPENAI_COMPAT_SELECTORS
                    .iter()
                    .map(|sel| ("openai-compat", sel.model_glob, sel.price_ambiguous)),
            )
            .map(|(kind, glob, flag)| {
                let derived_ambiguous =
                    litellm_unanimous_rate(&litellm, glob, "input_cost_per_token").is_none()
                        || litellm_unanimous_rate(&litellm, glob, "output_cost_per_token")
                            .is_none();
                (
                    crate::catalog_state::selector_key(kind, glob),
                    flag == derived_ambiguous,
                )
            })
            .collect();

        let disagreeing: Vec<&String> = flagged
            .iter()
            .filter_map(|(selector, agrees)| (!agrees).then_some(selector))
            .collect();
        assert!(
            disagreeing.is_empty(),
            "each selector's price_ambiguous flag disagrees with the vendored snapshots: \
             {disagreeing:?}"
        );
    }

    #[test]
    fn baked_table_fills_the_output_ceiling_only_where_both_sources_cohere() {
        // Guards the shipped classification against silent drift: the
        // vendored snapshots cohere on the output ceiling for exactly the
        // version-pinned Claude globs, and every vendor-wide prefix stays
        // absent. Derived from the baked table itself, so it moves with a
        // deliberate regeneration and fails on an accidental one.
        let filled: Vec<String> = crate::catalog::baked_table_rows()
            .into_iter()
            .filter(|row| row.row.max_output_tokens.is_some())
            .map(|row| crate::catalog_state::selector_key(row.provider_kind, row.model_glob))
            .collect();

        for selector in [
            "anthropic-api:claude-opus-4-8*",
            "anthropic-api:claude-opus-4-7*",
            "anthropic-api:claude-opus-4-6*",
            "anthropic-api:claude-opus-4-5*",
            "bedrock:anthropic.claude-sonnet-4-6*",
            "bedrock:anthropic.claude-haiku-4-5*",
            "bedrock:anthropic.claude-opus-4-5*",
        ] {
            assert!(
                filled.contains(&selector.to_string()),
                "{selector} must carry a baked output ceiling; filled: {filled:?}"
            );
        }

        // No vendor-wide prefix, and no provider catch-all, may carry one.
        for selector in &filled {
            assert!(
                selector.starts_with("anthropic-api:claude-")
                    || selector.starts_with("bedrock:anthropic.claude-"),
                "{selector} must not carry a baked output ceiling"
            );
        }
    }
}
