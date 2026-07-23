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
//!     (`cost.input` / `cost.cache_read` / `cost.cache_write` /
//!     `limit.context`) and for the `structured_output` capability tell --
//!     its schema names these fields directly.
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
//! each vendor's caching product. `max_context_tokens` and `capabilities`
//! are always derived from the snapshots; `wm` / `rm` are derived when a
//! source publishes cache pricing for the selector, else the selector is
//! marked `economics_unconfirmed` and its economics mirror
//! [`crate::catalog::CatalogRow::sentinel`] rather than a fabricated
//! number (see `OPENAI_COMPAT_SELECTORS`).

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

/// Baked catalog schema version. Bump whenever this module's output
/// changes materially (rows added/removed, a derivation rule changed) --
/// NOT on a vendor-snapshot refresh that leaves the generated shape
/// unchanged. Rendered into `catalog_baked.rs` as `CATALOG_VERSION: u32`.
#[cfg(feature = "gen-catalog")]
const CATALOG_VERSION: u32 = 1;

/// Display-only date the vendored snapshots under `catalog_data/` were
/// fetched, hand-maintained here (never the wall clock -- see the module
/// doc's determinism note) and rendered into `catalog_baked.rs` as a
/// separate `&str` const from `CATALOG_VERSION`.
#[cfg(feature = "gen-catalog")]
const CATALOG_SNAPSHOT_DATE: &str = "2026-07-11";

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
/// / `min_prefix_tokens` / `max_context_tokens` / `capabilities` off
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
/// selector's entry carries exactly one row. Unlike [`try_render`]'s prior
/// inline loop, this does not short-circuit on the first error: every
/// selector is derived so a caller can partition ok/err per selector (a
/// healthy source disagreement on one family should not hide the outcome of
/// every other family). `allowlist` is shared across every selector,
/// matching the codegen path's single vendored `cross_check_allowlist.json`;
/// a caller deriving from freshly fetched sources instead of the vendored
/// snapshots passes an empty one.
///
/// Compiled in regardless of `gen-catalog`: [`try_render`] calls this on
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

    let ctx = if sel.context_ambiguous {
        None
    } else {
        resolve_u32(
            &selector_id,
            "max_context_tokens",
            md_entry.and_then(models_dev_context),
            litellm_context(entry),
            allowlist,
        )?
    };
    let capabilities = capabilities_for(&selector_id, entry, md_entry, allowlist)?;

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
        capabilities,
    })
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
        caps,
    );
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
                "cache_read_input_token_cost": 1.0e-6,
                "cache_creation_input_token_cost": 1.25e-5,
                "cache_creation_input_token_cost_above_1hr": 2.0e-5,
                "max_input_tokens": 200_000.0,
            },
            auto_cacher.litellm_key: {
                "input_cost_per_token": 2.0e-6,
                "cache_read_input_token_cost": 2.0e-7,
                "max_input_tokens": 400_000.0,
            },
        });
        let models_dev = serde_json::json!({
            "anthropic": {
                "models": {
                    tiered.models_dev_model: {
                        "cost": {"input": 1.0e-5, "cache_read": 1.0e-6, "cache_write": 1.25e-5},
                        "limit": {"context": 200_000},
                    },
                },
            },
            auto_cacher.models_dev_provider: {
                "models": {
                    auto_cacher.models_dev_model: {
                        "cost": {"input": 2.0e-6, "cache_read": 2.0e-7},
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

        let auto_cacher_cells = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .find(|cells| cells[0].provider_kind == "openai-responses")
            .expect("the auto-cacher selector derives");
        assert_eq!(auto_cacher_cells.len(), 1, "single tier-agnostic row");
        assert!(auto_cacher_cells[0].tier.is_none());
        assert_eq!(auto_cacher_cells[0].max_context_tokens, Some(400_000));
    }
}
