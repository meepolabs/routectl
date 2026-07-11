//! `routectl catalog` -- inspect, verify, import, and edit the
//! cache-economics catalog. `pricing` is a hidden alias kept for muscle
//! memory (dropped at 1.0).
//!
//! Subcommands:
//!   list    -- print the EFFECTIVE catalog (the two-layer merge of the
//!              baked table with the on-disk `catalog_overlay.json`,
//!              `routectl_router::merge`) as an aligned ASCII table, headed
//!              by an overlay summary line (revision + counts by source +
//!              disabled count -- see [`overlay_summary_line`]). Every row
//!              renders PRESENT (with derived provenance + a staleness
//!              marker) or DISABLED (overlay `null`); MISSING never
//!              appears in this catalog-only listing (see
//!              [`build_list_data`]'s doc) even though the render path
//!              still renders it correctly (see the `missing_state_renders`
//!              test) for a future consumer keyed on configured aliases.
//!   verify  -- stamp an EXISTING overlay cell's `verified_at` to today,
//!              flipping its `source` to `user` (verifying is a user act).
//!              Writes through the serialized, revision-checked overlay
//!              writer (`routectl_router::with_overlay_write_lock`). A
//!              selector with no overlay cell (baked-only, or entirely
//!              unknown) has nothing to stamp and is an error -- creating a
//!              new overlay cell is a `set` concern.
//!   import  -- opt-in bulk refresh from the vendored economics sources;
//!              see `commands::catalog_import`.
//!   set     -- write a `source: user` cell for a KNOWN selector (an
//!              existing baked row, or an existing overlay cell of either
//!              provenance), field by field. See [`set_at`] for the
//!              admission rule, the field syntax, and the value-validation
//!              contract it reuses.
//!   disable -- write a JSON-null overlay cell for a KNOWN selector,
//!              disabling it regardless of what it previously carried. See
//!              [`disable_at`].
//!
//! LEGACY SIDECAR (`pricing_verifications.json`): this module still carries
//! the READ side of the old sidecar format ([`PricingVerifications`],
//! [`load_verifications`], [`merge_verifications_into`],
//! [`load_and_merge_verifications`]) -- but ONLY as a read path consumed by
//! the v1 -> v2 config migration (`server::load_effective_config`, which
//! calls [`load_and_merge_verifications`] to fold any historical sidecar
//! stamps into `config.cache_pricing` before the migrator moves them into
//! the catalog overlay). Nothing in the CLI writes the sidecar anymore --
//! `verify` now stamps the overlay directly -- so the write side
//! (`save_verification` / the atomic sidecar writer) is gone. The read side
//! stays until v1 config support itself is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use routectl_router::{
    CachePricingOverride, CachePricingSelector, CatalogOverlay, CatalogRow, Config, EffectiveRow,
    OverlayCell, OverlayError, OverlaySource, Source, baked_table_rows, catalog_state_selector_key,
    is_stale_today, merge, overlay_default_path, overlay_revision, with_overlay_write_lock,
};
#[cfg(test)]
use routectl_router::{load_catalog_overlay, save_catalog_overlay};

// ---------------------------------------------------------------------------
// Legacy sidecar (read-only, migration-only)
// ---------------------------------------------------------------------------

/// On-disk shape for the legacy `pricing_verifications.json` sidecar.
///
/// Uses a wrapper struct (not a bare map) so future fields can be added
/// without a format break. Read-only: nothing writes this shape anymore.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingVerifications {
    /// Maps a selector string (`"provider_kind:model_glob"`) to a
    /// verification date (`"YYYY-MM-DD"`).
    #[serde(default)]
    pub verified: BTreeMap<String, String>,
}

/// Path to the legacy sidecar file. Mirrors the `resolve_config_path` dir
/// logic in `main.rs`.
pub fn verifications_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("routectl").join("pricing_verifications.json")
}

/// Load the sidecar. Missing file -> `Default` (first run, not an error).
/// Malformed file -> returns an error (do not silently wipe).
pub fn load_verifications(path: &Path) -> Result<PricingVerifications, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PricingVerifications::default());
        }
        Err(e) => {
            return Err(format!(
                "cannot read pricing verifications `{}`: {e}",
                path.display()
            ));
        }
    };
    serde_json::from_str(&text)
        .map_err(|e| format!("malformed pricing verifications `{}`: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Legacy config merge (migration input only)
// ---------------------------------------------------------------------------

/// For each `(selector, date)` in `v` whose selector is NOT already a key in
/// `config.cache_pricing`, validate the date and insert a pure verification
/// override (`verified_at = Some(date)`, all value fields `None`). Entries
/// with a malformed date are skipped and their selectors are returned so the
/// caller can warn. Config.toml entries always win (selectors already present
/// in `config.cache_pricing` are skipped silently -- not reported).
pub fn merge_verifications_into(config: &mut Config, v: &PricingVerifications) -> Vec<String> {
    let mut skipped: Vec<String> = Vec::new();
    for (selector, date) in &v.verified {
        if config.cache_pricing.contains_key(selector) {
            continue;
        }
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
            skipped.push(selector.clone());
            continue;
        }
        config.cache_pricing.insert(
            selector.clone(),
            CachePricingOverride {
                verified_at: Some(date.clone()),
                ..Default::default()
            },
        );
    }
    skipped
}

/// Resolve the sidecar path, load, and merge into `config`. A missing file
/// is silently ignored (first run). A malformed sidecar JSON logs a warning
/// and skips the merge. Individual entries with a malformed date are dropped
/// with a per-entry warning.
///
/// Called ONLY by the v1 -> v2 config migration path (`server::
/// load_effective_config`, gated on `config.version < CURRENT_CONFIG_VERSION`)
/// so any historical sidecar stamp reaches the migrator's `cache_pricing`
/// input exactly once, before it folds into the catalog overlay.
pub fn load_and_merge_verifications(config: &mut Config) {
    let path = verifications_path();
    match load_verifications(&path) {
        Ok(v) => {
            let skipped = merge_verifications_into(config, &v);
            for sel in &skipped {
                tracing::warn!(
                    selector = %sel,
                    "pricing verification for `{sel}` has a malformed date and was ignored"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "pricing verifications sidecar could not be loaded; skipping merge"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Effective-catalog display (two-layer merge)
// ---------------------------------------------------------------------------

/// Table column header, in order.
const HEADER: &[&str] = &[
    "provider_kind",
    "model_glob",
    "status",
    "tier",
    "wm",
    "rm",
    "ttl(s)",
    "min_prefix",
    "auto",
    "max_ctx",
    "source",
    "verified_at",
    "stale",
];

/// Split a `"provider_kind:model_glob"` selector key for display. Every key
/// in this table is drawn from the baked table or a loaded overlay, both of
/// which are already selector-shaped, so a parse failure should not occur in
/// practice; falls back to `("?", key)` rather than panic on a hand-edited
/// overlay with a malformed key.
fn split_selector(key: &str) -> (String, String) {
    match CachePricingSelector::parse(key) {
        Ok(sel) => (sel.provider_kind, sel.model_glob),
        Err(_) => ("?".to_string(), key.to_string()),
    }
}

/// Provenance label for a `Present` row's winning layer.
const fn source_str(source: Source) -> &'static str {
    match source {
        Source::Baked => "baked",
        Source::Import => "import",
        Source::User => "user",
    }
}

/// Render one selector's [`EffectiveRow`] to the table's column order.
/// `DISABLED` and `MISSING` share the same conservative "nothing else to
/// show" shape (dashes for every economics / provenance column) -- the two
/// states share the same downstream sentinel treatment, so the display
/// treats them identically apart from the status label itself.
fn render_row(key: &str, effective: &EffectiveRow) -> Vec<String> {
    let (provider_kind, model_glob) = split_selector(key);
    match effective {
        EffectiveRow::Present {
            row,
            source,
            verified_at,
        } => {
            let stale = if is_stale_today(verified_at) {
                "WARN"
            } else {
                "-"
            };
            vec![
                provider_kind,
                model_glob,
                "PRESENT".to_string(),
                row.tier.unwrap_or("-").to_string(),
                format!("{:.4}", row.wm),
                format!("{:.4}", row.rm),
                row.ttl_seconds.to_string(),
                row.min_prefix_tokens.to_string(),
                if row.auto_cacher { "yes" } else { "no" }.to_string(),
                row.max_context_tokens
                    .map_or_else(|| "-".to_string(), |n| n.to_string()),
                source_str(*source).to_string(),
                verified_at.clone(),
                stale.to_string(),
            ]
        }
        EffectiveRow::Disabled => dashed_row(provider_kind, model_glob, "DISABLED"),
        EffectiveRow::Missing => dashed_row(provider_kind, model_glob, "MISSING"),
    }
}

/// A row whose only meaningful columns are the selector and `status`; every
/// economics / provenance column is a dash placeholder.
fn dashed_row(provider_kind: String, model_glob: String, status: &str) -> Vec<String> {
    let mut row = vec![provider_kind, model_glob, status.to_string()];
    row.extend(std::iter::repeat_n("-".to_string(), HEADER.len() - 3));
    row
}

/// Build the table rows and the punch-list of selectors whose EFFECTIVE
/// `max_context_tokens` is unknown (`Present` with a `None` window; a
/// `Disabled` / `Missing` selector carries no window to be unknown about,
/// so it is never punch-listed).
///
/// Rows are the two-layer merge ([`merge`]) of every selector key appearing
/// in EITHER the baked table or the loaded overlay -- the union, keyed
/// exactly as `"provider_kind:model_glob"`. A selector appearing in
/// neither layer is not a row here (there is nothing to enumerate it from);
/// [`EffectiveRow::Missing`] is reachable from this table only when an
/// overlay entry is later removed leaving a dangling reference elsewhere --
/// day-to-day, every displayed row backs onto at least one layer by
/// construction. See the `missing state renders` test below for the
/// direct classification/render coverage `MISSING` still needs.
pub fn build_list_data(overlay: &CatalogOverlay) -> (Vec<Vec<String>>, Vec<String>) {
    let baked = baked_table_rows();
    let mut baked_map: BTreeMap<String, &CatalogRow> = BTreeMap::new();
    for cell in &baked {
        baked_map.insert(
            format!("{}:{}", cell.provider_kind, cell.model_glob),
            &cell.row,
        );
    }

    let mut keys: BTreeSet<String> = baked_map.keys().cloned().collect();
    keys.extend(overlay.cells.keys().cloned());

    let mut rows = vec![HEADER.iter().map(|s| s.to_string()).collect()];
    let mut punch_set: BTreeSet<String> = BTreeSet::new();
    for key in &keys {
        let baked_row = baked_map.get(key).copied();
        let overlay_cell = overlay.cells.get(key);
        let effective = merge(baked_row, overlay_cell);
        if let Some(row) = effective.priced()
            && row.max_context_tokens.is_none()
        {
            punch_set.insert(key.clone());
        }
        rows.push(render_row(key, &effective));
    }

    (rows, punch_set.into_iter().collect())
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

/// One-line overlay summary header for [`list`]: the on-disk revision plus
/// a provenance breakdown of every overlay cell (`source: user` /
/// `source: import` / disabled). Says nothing about the baked table --
/// only the overlay carries a revision.
fn overlay_summary_line(overlay: &CatalogOverlay) -> String {
    let mut user = 0usize;
    let mut import = 0usize;
    let mut disabled = 0usize;
    for cell in overlay.cells.values() {
        match cell {
            Some(c) => match c.source {
                OverlaySource::User => user += 1,
                OverlaySource::Import => import += 1,
            },
            None => disabled += 1,
        }
    }
    format!(
        "overlay revision {} -- {} cell(s): {user} user, {import} import, {disabled} disabled",
        overlay_revision(overlay),
        overlay.cells.len(),
    )
}

/// `routectl catalog list` -- print the effective catalog (baked + overlay).
pub fn list(overlay: &CatalogOverlay) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}\n", overlay_summary_line(overlay));
    let (table, punch_list) = build_list_data(overlay);
    print!("{}", render_table(&table));
    if !punch_list.is_empty() {
        println!(
            "\npunch-list: {} selector(s) with an unknown max_context_tokens \
             (context-fraction advisory falls back to absolute tokens only):",
            punch_list.len()
        );
        for name in &punch_list {
            println!("  {name}");
        }
    }
    Ok(())
}

/// Today's date (UTC), the stamp every catalog writer (`verify`, `set`,
/// `import`) uses for `verified_at` -- one shared UTC clock read so the
/// writers can never disagree about "today" across a timezone (this
/// replaces `verify_at`'s prior `chrono::Local` read, which could stamp a
/// different calendar date than `set_at`'s UTC read near a local
/// midnight).
pub(crate) fn today_verified_at() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// `routectl catalog verify <selector>` -- stamp an existing overlay cell
/// verified today. Resolves the default overlay path; see [`verify_at`] for
/// the testable core.
pub fn verify(selector_raw: &str) -> Result<(), Box<dyn std::error::Error>> {
    verify_at(selector_raw, &overlay_default_path())
}

/// Core of [`verify`], taking the overlay path explicitly so tests can point
/// it at a temp directory instead of the real `catalog_overlay.json`.
///
/// Verifying is a USER act: an existing cell -- whichever layer wrote it --
/// is rewritten with `source: user` and `verified_at` bumped to today; every
/// other field on the cell is carried through unchanged. A selector with no
/// overlay cell (baked-only, or entirely unknown to both layers) has nothing
/// to stamp: creating a NEW overlay cell is a `set` concern, so this
/// returns a clear error instead of silently pinning the current effective
/// values. A selector whose overlay cell is explicitly `null` (disabled) is
/// likewise nothing to stamp -- verify never resurrects a disabled row.
///
/// The load-modify-save runs through [`with_overlay_write_lock`], so a
/// concurrent verify/import/set against the same overlay file serializes
/// instead of racing; the "nothing to stamp" checks below abort the closure
/// (returning `Err` before it produces a mutated overlay), so a rejected
/// verify never reaches the write.
pub(crate) fn verify_at(selector_raw: &str, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    CachePricingSelector::parse(selector_raw).map_err(|e| format!("invalid selector: {e}"))?;

    let today = today_verified_at();
    let selector = selector_raw.to_string();
    with_overlay_write_lock::<Box<dyn std::error::Error>, _>(path, |overlay| {
        let existing = match overlay.cells.get(&selector) {
            None => {
                return Err(format!(
                    "no overlay cell for selector `{selector}`; nothing to stamp (baked-only or \
                     unknown to the catalog) -- creating a new overlay cell is a `set` concern"
                )
                .into());
            }
            Some(None) => {
                return Err(format!(
                    "selector `{selector}` is disabled in the overlay (null); nothing to stamp"
                )
                .into());
            }
            Some(Some(cell)) => cell.clone(),
        };

        let stamped = OverlayCell {
            source: OverlaySource::User,
            verified_at: today.clone(),
            wm: existing.wm,
            rm: existing.rm,
            ttl_seconds: existing.ttl_seconds,
            min_prefix_tokens: existing.min_prefix_tokens,
            max_context_tokens: existing.max_context_tokens,
            capabilities: existing.capabilities,
        };
        let mut next = overlay;
        next.cells.insert(selector.clone(), Some(stamped));
        Ok(next)
    })?;

    println!(
        "verified: selector={selector_raw}  date={today}  source=user  written to {}",
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// set / disable: user-edit verbs
// ---------------------------------------------------------------------------

/// Errors from `catalog set` / `catalog disable`. Distinguishes an
/// intentional admission/validation abort from a raw [`OverlayError`] so an
/// unknown selector, or a field the overlay cannot carry, never reads as a
/// storage failure.
#[derive(Debug, thiserror::Error)]
pub enum CatalogWriteError {
    #[error("invalid selector `{selector}`: {reason}")]
    InvalidSelector { selector: String, reason: String },

    #[error(
        "selector `{0}` is unknown to the catalog (no baked row, no existing overlay cell); \
         creating a brand-new selector is not supported by `set` / `disable`"
    )]
    UnknownSelector(String),

    #[error("field `{field}` is not supported by `catalog set`: {reason}")]
    UnsupportedField { field: String, reason: String },

    #[error("invalid field `{raw}`: {reason}")]
    InvalidField { raw: String, reason: String },

    #[error("{0}")]
    Validation(String),

    #[error(transparent)]
    Overlay(#[from] OverlayError),
}

fn parse_selector(selector_raw: &str) -> Result<(), CatalogWriteError> {
    CachePricingSelector::parse(selector_raw)
        .map(|_| ())
        .map_err(|reason| CatalogWriteError::InvalidSelector {
            selector: selector_raw.to_string(),
            reason,
        })
}

/// One parsed `field=value` pair from `catalog set`'s variadic field list.
#[derive(Debug)]
enum FieldUpdate {
    Wm(f32),
    Rm(f32),
    TtlSeconds(u32),
    MinPrefixTokens(u32),
    MaxContextTokens(u32),
    /// A `cap:<name>=true|false` capability flag.
    Capability(String, bool),
}

/// Fields [`OverlayCell`] structurally cannot carry: they live only on the
/// baked catalog table (settled: the import pipeline cannot produce a
/// differing `auto_cacher`, and the storage-rent fields are
/// reserved-unused on every baked row), so `set` hard-rejects an attempt to
/// set them rather than silently drop them.
const UNSUPPORTED_FIELDS: &[&str] = &["auto_cacher", "has_storage_rent", "storage_rent"];

/// Parse one `field=value` argument. Capability flags use the
/// `cap:<name>=true|false` syntax (documented in `--help`); every other
/// supported field is a bare name. `verified_at` and the baked-only fields
/// are named explicitly in the error so the operator knows why they were
/// rejected rather than getting a generic "unknown field".
fn parse_field(raw: &str) -> Result<FieldUpdate, CatalogWriteError> {
    let (field, value) = raw
        .split_once('=')
        .ok_or_else(|| CatalogWriteError::InvalidField {
            raw: raw.to_string(),
            reason: "expected `field=value`".to_string(),
        })?;

    if let Some(name) = field.strip_prefix("cap:") {
        if name.is_empty() {
            return Err(CatalogWriteError::InvalidField {
                raw: raw.to_string(),
                reason: "capability name must not be empty".to_string(),
            });
        }
        let flag = parse_bool_value(value).ok_or_else(|| CatalogWriteError::InvalidField {
            raw: raw.to_string(),
            reason: "capability value must be `true` or `false`".to_string(),
        })?;
        return Ok(FieldUpdate::Capability(name.to_string(), flag));
    }

    if field == "verified_at" {
        return Err(CatalogWriteError::UnsupportedField {
            field: field.to_string(),
            reason: "verified_at is stamped automatically to today; it cannot be set directly"
                .to_string(),
        });
    }
    if UNSUPPORTED_FIELDS.contains(&field) {
        return Err(CatalogWriteError::UnsupportedField {
            field: field.to_string(),
            reason: "this field lives only on the baked catalog table; the overlay has no field \
                     to carry it"
                .to_string(),
        });
    }

    match field {
        "wm" => parse_num(raw, value).map(FieldUpdate::Wm),
        "rm" => parse_num(raw, value).map(FieldUpdate::Rm),
        "ttl_seconds" => parse_num(raw, value).map(FieldUpdate::TtlSeconds),
        "min_prefix_tokens" => parse_num(raw, value).map(FieldUpdate::MinPrefixTokens),
        "max_context_tokens" => parse_num(raw, value).map(FieldUpdate::MaxContextTokens),
        other => Err(CatalogWriteError::InvalidField {
            raw: raw.to_string(),
            reason: format!(
                "unknown field `{other}`; supported fields are wm, rm, ttl_seconds, \
                 min_prefix_tokens, max_context_tokens, cap:<name>"
            ),
        }),
    }
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_num<T: std::str::FromStr>(raw: &str, value: &str) -> Result<T, CatalogWriteError> {
    value.parse().map_err(|_| CatalogWriteError::InvalidField {
        raw: raw.to_string(),
        reason: format!("`{value}` is not a valid number"),
    })
}

fn apply_field_update(cell: &mut OverlayCell, update: FieldUpdate) {
    match update {
        FieldUpdate::Wm(v) => cell.wm = Some(v),
        FieldUpdate::Rm(v) => cell.rm = Some(v),
        FieldUpdate::TtlSeconds(v) => cell.ttl_seconds = Some(v),
        FieldUpdate::MinPrefixTokens(v) => cell.min_prefix_tokens = Some(v),
        FieldUpdate::MaxContextTokens(v) => cell.max_context_tokens = Some(v),
        FieldUpdate::Capability(name, flag) => {
            cell.capabilities
                .get_or_insert_with(BTreeMap::new)
                .insert(name, flag);
        }
    }
}

/// True when `selector` is known to the catalog: either an EXACT
/// baked-table key (`"provider_kind:model_glob"`) or an existing overlay
/// cell (present or disabled). This is the synthetic-row poisoning guard:
/// `merge`'s own `Some(cell)` + no baked row still yields `Present` (over
/// the sentinel), so an unbounded selector would otherwise be silently
/// admitted the moment `set`/`disable` write it.
fn selector_known(selector: &str, overlay: &CatalogOverlay) -> bool {
    if overlay.cells.contains_key(selector) {
        return true;
    }
    baked_table_rows()
        .into_iter()
        .any(|row| catalog_state_selector_key(row.provider_kind, row.model_glob) == selector)
}

/// Reuse [`CachePricingOverride::validate`]'s degeneracy contract (`rm >
/// 0`, `max_context_tokens != 0`, below-sentinel `wm` needs the ack flag)
/// against ONLY the fields THIS call is setting -- never against a field
/// inherited unchanged from a prior cell. A baked/import cell can
/// legitimately carry a `wm` below the sentinel already (auto-cachers
/// commonly do); re-validating the whole merged cell on every edit would
/// force an unrelated `set rm=...` to also re-acknowledge a `wm` it never
/// touched.
fn validate_updates(
    updates: &[FieldUpdate],
    acknowledge_cost_risk: bool,
) -> Result<(), CatalogWriteError> {
    let mut ov = CachePricingOverride {
        override_acknowledges_cost_risk: acknowledge_cost_risk,
        ..Default::default()
    };
    for update in updates {
        match update {
            FieldUpdate::Wm(v) => ov.wm = Some(*v),
            FieldUpdate::Rm(v) => ov.rm = Some(*v),
            FieldUpdate::MaxContextTokens(v) => ov.max_context_tokens = Some(*v),
            FieldUpdate::TtlSeconds(_)
            | FieldUpdate::MinPrefixTokens(_)
            | FieldUpdate::Capability(..) => {}
        }
    }
    ov.validate().map_err(CatalogWriteError::Validation)
}

/// The same serve-pickup note `catalog import` prints after a write: a
/// running `routectl serve` re-reads the overlay via its file watch.
/// `pub(crate)` so `commands::catalog_import`'s own summary can print the
/// identical line without a second copy of the string.
pub(crate) fn print_pickup_note() {
    println!(
        "note: a running `routectl serve` picks up this change automatically via the overlay \
         watch."
    );
}

/// `routectl catalog set <selector> <field>=<value>...` -- resolves the
/// default overlay path; see [`set_at`] for the testable core.
pub fn set(
    selector_raw: &str,
    fields: &[String],
    acknowledge_cost_risk: bool,
) -> Result<(), CatalogWriteError> {
    set_at(
        selector_raw,
        fields,
        acknowledge_cost_risk,
        &overlay_default_path(),
    )
}

/// Core of [`set`], taking the overlay path explicitly so tests can point
/// it at a temp directory instead of the real `catalog_overlay.json`.
///
/// Selector SYNTAX and field PARSING are checked up front -- pure, no I/O.
/// ADMISSION -- is the selector actually known to the catalog (see
/// [`selector_known`]) -- and value VALIDATION (see [`validate_updates`])
/// both run INSIDE the write lock, admission first: a selector typo'd
/// alongside a bad value reads as "unknown selector", not a value error,
/// and neither check needs the loaded overlay for validation, but keeping
/// both under the same lock hold keeps the ordering simple. A brand-new
/// selector is rejected: creating one is a future explicit create path,
/// not this verb.
///
/// `set` on a selector that already carries a present overlay cell (either
/// provenance) starts from that cell's own fields, so an unset field in
/// `fields` still inherits whatever the prior cell -- import or user --
/// already had; naming a field overwrites it. `set` on a baked-only
/// selector, or on a currently-DISABLED one, starts from an all-`None`
/// sparse cell instead: a disabled cell carries no fields to inherit, and
/// re-enabling via `set` is exactly that -- a fresh cell. Every call always
/// stamps `source: user` and `verified_at` to today (UTC) -- editing a
/// cell is itself a ratification, even of one an import last wrote.
pub(crate) fn set_at(
    selector_raw: &str,
    fields: &[String],
    acknowledge_cost_risk: bool,
    path: &Path,
) -> Result<(), CatalogWriteError> {
    parse_selector(selector_raw)?;
    let updates: Vec<FieldUpdate> = fields
        .iter()
        .map(|f| parse_field(f))
        .collect::<Result<_, _>>()?;

    let today = today_verified_at();
    let selector = selector_raw.to_string();

    with_overlay_write_lock::<CatalogWriteError, _>(path, |overlay| {
        if !selector_known(&selector, &overlay) {
            return Err(CatalogWriteError::UnknownSelector(selector.clone()));
        }
        validate_updates(&updates, acknowledge_cost_risk)?;

        let mut cell = match overlay.cells.get(&selector) {
            Some(Some(existing)) => existing.clone(),
            _ => OverlayCell {
                source: OverlaySource::User,
                verified_at: today.clone(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            },
        };
        cell.source = OverlaySource::User;
        cell.verified_at = today.clone();
        for update in updates {
            apply_field_update(&mut cell, update);
        }

        let mut next = overlay;
        next.cells.insert(selector.clone(), Some(cell));
        Ok(next)
    })?;

    println!(
        "set: selector={selector_raw}  source=user  verified_at={today}  written to {}",
        path.display()
    );
    print_pickup_note();
    Ok(())
}

/// `routectl catalog disable <selector>` -- resolves the default overlay
/// path; see [`disable_at`] for the testable core.
pub fn disable(selector_raw: &str) -> Result<(), CatalogWriteError> {
    disable_at(selector_raw, &overlay_default_path())
}

/// Core of [`disable`]. Same admission rule as [`set_at`] (a brand-new
/// selector is rejected). A disable always writes JSON `null` regardless
/// of what the selector previously carried -- there is nothing to
/// preserve; disabling discards the cell's own field values by design, and
/// re-enabling is a fresh `set`.
pub(crate) fn disable_at(selector_raw: &str, path: &Path) -> Result<(), CatalogWriteError> {
    parse_selector(selector_raw)?;
    let selector = selector_raw.to_string();

    with_overlay_write_lock::<CatalogWriteError, _>(path, |overlay| {
        if !selector_known(&selector, &overlay) {
            return Err(CatalogWriteError::UnknownSelector(selector.clone()));
        }
        let mut next = overlay;
        next.cells.insert(selector.clone(), None);
        Ok(next)
    })?;

    println!(
        "disabled: selector={selector_raw}  written to {}",
        path.display()
    );
    print_pickup_note();
    Ok(())
}

// ---------------------------------------------------------------------------
// Table renderer (local copy; do not share the private fn from usage.rs)
// ---------------------------------------------------------------------------

/// Left-align column 0, right-align the rest, padded to the widest cell in
/// each column. ASCII spaces only. Callers must pass rows of uniform column
/// count; a ragged row renders misaligned (never panics). `pub(crate)` so
/// `commands::catalog_import` reuses the same rendering for its diff table
/// instead of duplicating the alignment logic.
pub(crate) fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(std::vec::Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            if i == 0 {
                line.push_str(&format!("{cell:<width$}", width = widths[i]));
            } else {
                line.push_str(&format!("{cell:>width$}", width = widths[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Legacy sidecar: read side only (round-trips a manually-written file --
    // the write side that used to produce it is gone).
    // -----------------------------------------------------------------------

    #[test]
    fn load_verifications_reads_a_manually_written_sidecar_file() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing_verifications.json");
        std::fs::write(
            &path,
            r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
        )
        .unwrap();

        // Act
        let loaded = load_verifications(&path).unwrap();

        // Assert
        assert_eq!(
            loaded
                .verified
                .get("openai-compat:grok-*")
                .map(String::as_str),
            Some("2026-06-30")
        );
    }

    #[test]
    fn load_verifications_missing_path_returns_empty_default() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");

        // Act
        let result = load_verifications(&path);

        // Assert -- not an error; the map is empty
        assert!(result.is_ok());
        assert!(result.unwrap().verified.is_empty());
    }

    #[test]
    fn load_verifications_malformed_json_returns_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not valid json {{{").unwrap();

        // Act
        let result = load_verifications(&path);

        // Assert
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("malformed"), "expected 'malformed' in: {msg}");
    }

    // -----------------------------------------------------------------------
    // merge_verifications_into: additive and config wins
    // -----------------------------------------------------------------------

    #[test]
    fn merge_adds_new_selector_with_verified_at_only() {
        // Arrange
        let mut config = minimal_config();
        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());

        // Act
        let skipped = merge_verifications_into(&mut config, &v);

        // Assert: the key was inserted as a pure verification override
        assert!(skipped.is_empty(), "no entries should be skipped");
        let ov = config
            .cache_pricing
            .get("openai-compat:grok-*")
            .expect("key should be inserted");
        assert_eq!(
            ov.verified_at.as_deref(),
            Some("2026-06-30"),
            "verified_at should be set"
        );
        assert!(ov.wm.is_none(), "wm should be None (pure verification)");
        assert!(ov.rm.is_none(), "rm should be None");
        assert!(ov.ttl_seconds.is_none(), "ttl_seconds should be None");
        assert!(
            ov.min_prefix_tokens.is_none(),
            "min_prefix_tokens should be None"
        );
    }

    #[test]
    fn merge_does_not_overwrite_existing_config_key() {
        // Arrange: the config already has an entry for this selector
        let mut config = minimal_config();
        let existing = CachePricingOverride {
            wm: Some(1.5),
            verified_at: Some("2025-01-01".to_string()),
            override_acknowledges_cost_risk: true,
            ..Default::default()
        };
        config
            .cache_pricing
            .insert("openai-compat:grok-*".to_string(), existing);

        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-06-30".to_string());

        // Act
        let skipped = merge_verifications_into(&mut config, &v);

        // Assert: the config entry is unchanged; existing key not in skipped
        assert!(
            skipped.is_empty(),
            "config-key wins should not appear in skipped"
        );
        let ov = config
            .cache_pricing
            .get("openai-compat:grok-*")
            .expect("key should still be present");
        assert_eq!(
            ov.verified_at.as_deref(),
            Some("2025-01-01"),
            "config entry should not be overwritten by sidecar"
        );
        assert_eq!(ov.wm, Some(1.5), "wm should be unchanged");
    }

    #[test]
    fn merge_skips_malformed_date_and_inserts_valid_sibling() {
        // Arrange: one bad date, one good date
        let mut config = minimal_config();
        let mut v = PricingVerifications::default();
        v.verified
            .insert("openai-compat:grok-*".to_string(), "2026-13-99".to_string());
        v.verified.insert(
            "openai-compat:mistral-*".to_string(),
            "2026-06-30".to_string(),
        );

        // Act
        let skipped = merge_verifications_into(&mut config, &v);

        // Assert: malformed-date entry is skipped and reported
        assert_eq!(skipped, vec!["openai-compat:grok-*".to_string()]);
        assert!(
            !config.cache_pricing.contains_key("openai-compat:grok-*"),
            "malformed entry should not be inserted"
        );
        assert!(
            config.cache_pricing.contains_key("openai-compat:mistral-*"),
            "valid sibling should be inserted"
        );
    }

    // -----------------------------------------------------------------------
    // overlay_summary_line: list's header (revision + counts by source).
    // -----------------------------------------------------------------------

    #[test]
    fn overlay_summary_line_counts_user_import_and_disabled_cells() {
        // Arrange: one of each state, at a non-zero revision.
        let mut overlay = CatalogOverlay {
            revision: 4,
            ..CatalogOverlay::default()
        };
        overlay.cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                ..blank_user_cell()
            }),
        );
        overlay.cells.insert(
            "openai-compat:mistral-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                ..blank_user_cell()
            }),
        );
        overlay
            .cells
            .insert("openai-compat:disabled-model".to_string(), None);

        // Act
        let line = overlay_summary_line(&overlay);

        // Assert
        assert!(line.contains("revision 4"), "line: {line}");
        assert!(line.contains("3 cell(s)"), "line: {line}");
        assert!(line.contains("1 user"), "line: {line}");
        assert!(line.contains("1 import"), "line: {line}");
        assert!(line.contains("1 disabled"), "line: {line}");
    }

    #[test]
    fn overlay_summary_line_on_an_empty_overlay_reports_zero_counts() {
        let line = overlay_summary_line(&CatalogOverlay::default());
        assert!(line.contains("revision 0"), "line: {line}");
        assert!(line.contains("0 cell(s)"), "line: {line}");
    }

    // -----------------------------------------------------------------------
    // build_list_data: PRESENT (baked / import / user), DISABLED, punch-list
    // -----------------------------------------------------------------------

    #[test]
    fn present_baked_row_shows_baked_source_and_no_stale_warn() {
        // Arrange: no overlay entry -- the baked cell wins as-is.
        let overlay = CatalogOverlay::default();

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert
        let row = find_row(&rows, "openai-compat", "grok-*").expect("baked row present");
        assert_eq!(row[2], "PRESENT");
        assert_eq!(row[10], "baked");
        assert_eq!(row[12], "-", "the fresh baked snapshot date is not stale");
    }

    #[test]
    fn present_import_cell_overrides_baked_and_shows_import_source() {
        // Arrange: an import cell for a real baked selector, overriding wm.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert
        let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
        assert_eq!(row[2], "PRESENT");
        assert_eq!(row[4], "0.5000", "wm overridden by the import cell");
        assert_eq!(row[10], "import");
        assert_eq!(row[11], "2026-07-01");
    }

    #[test]
    fn present_user_cell_with_no_baked_match_renders_from_sentinel_base() {
        // Arrange: a user cell naming a selector no baked cell backs.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "openai-compat:totally-new-model-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-05".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: Some(999),
                max_context_tokens: None,
                capabilities: None,
            }),
        );

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert: sentinel base (wm 2.0) with the user's min_prefix override.
        let row = find_row(&rows, "openai-compat", "totally-new-model-*").expect("row present");
        assert_eq!(row[2], "PRESENT");
        assert_eq!(row[4], "2.0000", "sentinel wm as the base");
        assert_eq!(row[7], "999", "user min_prefix override applied");
        assert_eq!(row[10], "user");
    }

    #[test]
    fn disabled_cell_renders_disabled_status_regardless_of_baked() {
        // Arrange: a null overlay entry for a real baked selector.
        let mut overlay = CatalogOverlay::default();
        overlay
            .cells
            .insert("openai-compat:grok-*".to_string(), None);

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert: every economics / provenance column dashes out.
        let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
        assert_eq!(row[2], "DISABLED");
        for col in &row[3..] {
            assert_eq!(col, "-");
        }
    }

    #[test]
    fn stale_verified_at_renders_warn_marker() {
        // Arrange: an import cell stamped far in the past.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2020-01-01".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );

        // Act
        let (rows, _) = build_list_data(&overlay);

        // Assert
        let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
        assert_eq!(row[12], "WARN");
    }

    #[test]
    fn round_trip_display_renders_import_user_and_disabled_states() {
        // Arrange: one import cell, one user cell, one null-disabled cell,
        // all round-tripped through the real overlay writer/loader.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-07-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        cells.insert(
            "anthropic-api:claude-opus-4-8*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-05".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: Some(1024),
                max_context_tokens: Some(200_000),
                capabilities: None,
            }),
        );
        cells.insert("openai-compat:disabled-model".to_string(), None);
        routectl_router::save_catalog_overlay(&path, 0, cells).expect("save");

        // Act
        let overlay = load_catalog_overlay(&path).expect("load");
        let (rows, _) = build_list_data(&overlay);

        // Assert: all three states render distinctly.
        let import_row = find_row(&rows, "openai-compat", "grok-*").expect("import row");
        assert_eq!(import_row[2], "PRESENT");
        assert_eq!(import_row[10], "import");

        let user_row = find_row(&rows, "anthropic-api", "claude-opus-4-8*").expect("user row");
        assert_eq!(user_row[2], "PRESENT");
        assert_eq!(user_row[10], "user");

        let disabled_row =
            find_row(&rows, "openai-compat", "disabled-model").expect("disabled row");
        assert_eq!(disabled_row[2], "DISABLED");
    }

    #[test]
    fn missing_state_renders_missing_status() {
        // MISSING requires calling `merge` with neither layer present, which
        // cannot arise from `build_list_data`'s own baked-union-overlay key
        // enumeration (every enumerated key backs onto at least one layer by
        // construction). Exercised directly here so the render path's
        // MISSING arm is covered.
        let effective = merge(None, None);
        assert_eq!(effective, EffectiveRow::Missing);

        let row = render_row("openai-compat:nowhere-*", &effective);
        assert_eq!(row[2], "MISSING");
        for col in &row[3..] {
            assert_eq!(col, "-");
        }
    }

    #[test]
    fn punch_list_names_present_row_with_unknown_max_context_tokens() {
        // Arrange: the anthropic-api "*" catch-all is baked with an unknown
        // window; no overlay entry supplies one.
        let overlay = CatalogOverlay::default();

        // Act
        let (_, punch_list) = build_list_data(&overlay);

        // Assert
        assert!(
            punch_list.contains(&"anthropic-api:*".to_string()),
            "punch_list: {punch_list:?}"
        );
        assert!(
            !punch_list.contains(&"anthropic-api:claude-opus-4-8*".to_string()),
            "a known-window model must not appear: {punch_list:?}"
        );
    }

    #[test]
    fn punch_list_cleared_when_overlay_supplies_a_window() {
        // Arrange: an overlay cell supplies the missing window for the
        // otherwise-unknown anthropic-api "*" catch-all.
        let mut overlay = CatalogOverlay::default();
        overlay.cells.insert(
            "anthropic-api:*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2026-07-05".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: Some(200_000),
                capabilities: None,
            }),
        );

        // Act
        let (_, punch_list) = build_list_data(&overlay);

        // Assert
        assert!(
            !punch_list.contains(&"anthropic-api:*".to_string()),
            "punch_list: {punch_list:?}"
        );
    }

    /// Locate a rendered row by `(provider_kind, model_glob)`.
    fn find_row<'a>(
        rows: &'a [Vec<String>],
        provider_kind: &str,
        model_glob: &str,
    ) -> Option<&'a Vec<String>> {
        rows.iter()
            .skip(1)
            .find(|r| r[0] == provider_kind && r[1] == model_glob)
    }

    // -----------------------------------------------------------------------
    // verify_at: stamps an EXISTING overlay cell -- verifying is a user
    // act; creating cells is a separate import/set concern.
    // -----------------------------------------------------------------------

    #[test]
    fn verify_at_stamps_existing_user_cell_updates_verified_at_only() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::User,
                verified_at: "2020-01-01".to_string(),
                wm: Some(1.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: Some(512),
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        verify_at("openai-compat:grok-*", &path).expect("verify");

        // Assert: source stays user, verified_at bumped to today, values kept.
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        let today = today_verified_at();
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.verified_at, today, "verified_at bumped to UTC today");
        assert_eq!(cell.wm, Some(1.5));
        assert_eq!(cell.min_prefix_tokens, Some(512));
    }

    #[test]
    fn verify_at_flips_import_cell_source_to_user() {
        // Arrange: an import cell -- verifying is a user act, so the source
        // flips even though the cell originated from an import.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2026-01-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        verify_at("openai-compat:grok-*", &path).expect("verify");

        // Assert
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.wm, Some(0.5), "value fields carry through unchanged");
    }

    #[test]
    fn verify_at_errors_when_no_overlay_cell_exists_for_selector() {
        // Arrange: an empty overlay (the selector is baked-only or unknown).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        save_catalog_overlay(&path, 0, BTreeMap::new()).expect("seed empty");

        // Act
        let err =
            verify_at("openai-compat:grok-*", &path).expect_err("no overlay cell must be an error");

        // Assert
        let msg = err.to_string();
        assert!(msg.contains("nothing to stamp"), "msg: {msg}");
        assert!(msg.contains("baked-only"), "msg: {msg}");
    }

    #[test]
    fn verify_at_errors_when_overlay_cell_is_disabled() {
        // Arrange: the selector is explicitly disabled (JSON null).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert("openai-compat:grok-*".to_string(), None);
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        let err =
            verify_at("openai-compat:grok-*", &path).expect_err("a disabled cell must be an error");

        // Assert: never resurrects a disabled row.
        let msg = err.to_string();
        assert!(msg.contains("disabled"), "msg: {msg}");
        let overlay = load_catalog_overlay(&path).expect("load");
        assert_eq!(
            overlay.cells.get("openai-compat:grok-*"),
            Some(&None),
            "the disabled cell must remain untouched"
        );
    }

    #[test]
    fn verify_at_rejects_malformed_selector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let err =
            verify_at("no-colon-here", &path).expect_err("malformed selector must be rejected");
        assert!(err.to_string().contains("invalid selector"), "msg: {err}");
        assert!(!path.exists(), "nothing should have been written");
    }

    // -----------------------------------------------------------------------
    // set_at / disable_at: user-edit verbs.
    // -----------------------------------------------------------------------

    fn blank_user_cell() -> OverlayCell {
        OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-01-01".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            capabilities: None,
        }
    }

    #[test]
    fn disable_writes_a_null_cell_that_merges_to_disabled_through_the_real_merge_and_never_reuses_a_prior_cell()
     {
        // Arrange: seed a present user cell so a disable's discard-on-write
        // behavior is observable (not just "there was nothing there").
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                min_prefix_tokens: Some(9_999),
                ..blank_user_cell()
            }),
        );
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act
        disable_at("openai-compat:grok-*", &path).expect("disable must succeed");

        // Assert: the prior cell's fields are gone -- disabling writes a
        // bare JSON null, not a null-flavored copy of the old values.
        let overlay = load_catalog_overlay(&path).expect("load");
        assert_eq!(overlay.cells.get("openai-compat:grok-*"), Some(&None));
    }

    #[test]
    fn set_at_writes_a_user_cell_for_a_baked_selector_with_the_field_landing() {
        // Arrange: an empty overlay -- "openai-compat:grok-*" is baked-known
        // but has no overlay cell yet.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        // Act
        set_at(
            "openai-compat:grok-*",
            &["min_prefix_tokens=777".to_string()],
            false,
            &path,
        )
        .expect("set must succeed for a known baked selector");

        // Assert
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.min_prefix_tokens, Some(777));
        let today = today_verified_at();
        assert_eq!(cell.verified_at, today, "verified_at auto-stamped to today");
    }

    #[test]
    fn set_at_on_an_import_cell_flips_source_to_user_and_keeps_unset_fields() {
        // Arrange: an existing import cell for a baked selector.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        let mut cells = BTreeMap::new();
        cells.insert(
            "openai-compat:grok-*".to_string(),
            Some(OverlayCell {
                source: OverlaySource::Import,
                verified_at: "2020-01-01".to_string(),
                wm: Some(0.5),
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                capabilities: None,
            }),
        );
        save_catalog_overlay(&path, 0, cells).expect("seed");

        // Act: set only rm -- this IS ratification-by-edit of the import cell.
        set_at(
            "openai-compat:grok-*",
            &["rm=0.2".to_string()],
            false,
            &path,
        )
        .expect("set must succeed");

        // Assert
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.source, OverlaySource::User);
        assert_eq!(cell.rm, Some(0.2));
        assert_eq!(
            cell.wm,
            Some(0.5),
            "the unset wm field carries through from the prior import cell"
        );
    }

    #[test]
    fn disable_writes_a_null_cell_that_merges_to_disabled_through_the_real_merge() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        // Act
        disable_at("openai-compat:grok-*", &path)
            .expect("disable must succeed for a known baked selector");

        // Assert: JSON null on disk, and the real two-layer merge reports
        // Disabled regardless of the baked row underneath.
        let overlay = load_catalog_overlay(&path).expect("load");
        assert_eq!(overlay.cells.get("openai-compat:grok-*"), Some(&None));

        let baked = baked_table_rows();
        let baked_map: BTreeMap<String, CatalogRow> = baked
            .into_iter()
            .map(|c| (format!("{}:{}", c.provider_kind, c.model_glob), c.row))
            .collect();
        let effective = merge(
            baked_map.get("openai-compat:grok-*"),
            overlay.cells.get("openai-compat:grok-*"),
        );
        assert_eq!(effective, EffectiveRow::Disabled);
    }

    #[test]
    fn set_at_rejects_an_unknown_selector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let err = set_at(
            "openai-compat:totally-unknown-model-xyz",
            &["min_prefix_tokens=1".to_string()],
            false,
            &path,
        )
        .expect_err("an unknown selector must be rejected");

        assert!(
            matches!(err, CatalogWriteError::UnknownSelector(_)),
            "{err}"
        );
        assert!(!path.exists(), "nothing should have been written");
    }

    #[test]
    fn set_at_reports_unknown_selector_even_when_the_value_would_also_fail_validation() {
        // Admission is checked before value validation: a typo'd selector
        // paired with a bad value reads as "unknown selector", not a
        // confusing validation error about a selector that will be
        // rejected anyway.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let err = set_at(
            "openai-compat:totally-unknown-model-xyz",
            &["wm=1.0".to_string()],
            false,
            &path,
        )
        .expect_err("an unknown selector must be rejected first");

        assert!(
            matches!(err, CatalogWriteError::UnknownSelector(_)),
            "{err}"
        );
        assert!(!path.exists(), "nothing should have been written");
    }

    #[test]
    fn disable_at_rejects_an_unknown_selector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let err = disable_at("openai-compat:totally-unknown-model-xyz", &path)
            .expect_err("an unknown selector must be rejected");

        assert!(
            matches!(err, CatalogWriteError::UnknownSelector(_)),
            "{err}"
        );
        assert!(!path.exists(), "nothing should have been written");
    }

    #[test]
    fn set_at_and_disable_at_surface_a_corrupt_overlay_as_a_transparent_overlay_error() {
        // Arrange: a corrupt overlay file -- `with_overlay_write_lock`'s own
        // `load` fails closed, and that `OverlayError` must propagate through
        // `CatalogWriteError::Overlay` rather than being swallowed or
        // misreported as an admission/validation failure.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");
        std::fs::write(&path, b"not json {{{").unwrap();

        let err = set_at(
            "openai-compat:grok-*",
            &["min_prefix_tokens=1".to_string()],
            false,
            &path,
        )
        .expect_err("a corrupt overlay must surface as an error");
        assert!(matches!(err, CatalogWriteError::Overlay(_)), "{err}");

        let err =
            disable_at("openai-compat:grok-*", &path).expect_err("a corrupt overlay must error");
        assert!(matches!(err, CatalogWriteError::Overlay(_)), "{err}");
    }

    #[test]
    fn set_at_rejects_auto_cacher_naming_the_limitation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        let err = set_at(
            "openai-compat:grok-*",
            &["auto_cacher=true".to_string()],
            false,
            &path,
        )
        .expect_err("auto_cacher must be hard-rejected");

        match err {
            CatalogWriteError::UnsupportedField { field, reason } => {
                assert_eq!(field, "auto_cacher");
                assert!(!reason.is_empty(), "the error must name the limitation");
            }
            other => panic!("expected UnsupportedField, got {other:?}"),
        }
        assert!(!path.exists(), "nothing should have been written");
    }

    #[test]
    fn set_at_rejects_storage_rent_fields_and_verified_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        for raw in [
            "has_storage_rent=true",
            "storage_rent=1.0",
            "verified_at=2020-01-01",
        ] {
            let err = set_at("openai-compat:grok-*", &[raw.to_string()], false, &path)
                .expect_err("field must be hard-rejected");
            assert!(
                matches!(err, CatalogWriteError::UnsupportedField { .. }),
                "raw={raw} err={err}"
            );
        }
        assert!(!path.exists());
    }

    #[test]
    fn set_at_rejects_below_sentinel_wm_without_ack_and_accepts_with_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog_overlay.json");

        // Act / Assert: rejected without the ack flag.
        let err = set_at(
            "openai-compat:grok-*",
            &["wm=1.0".to_string()],
            false,
            &path,
        )
        .expect_err("a below-sentinel wm without ack must be rejected");
        assert!(matches!(err, CatalogWriteError::Validation(_)), "{err}");
        assert!(!path.exists(), "nothing should have been written");

        // Act / Assert: the SAME wm, with the ack flag, is accepted.
        set_at("openai-compat:grok-*", &["wm=1.0".to_string()], true, &path)
            .expect("the same wm with the ack flag must be accepted");
        let overlay = load_catalog_overlay(&path).expect("load");
        let cell = overlay
            .cells
            .get("openai-compat:grok-*")
            .and_then(Option::as_ref)
            .expect("cell present");
        assert_eq!(cell.wm, Some(1.0));
    }

    #[test]
    fn validate_updates_enforces_the_override_validate_contract() {
        // rm <= 0 rejected unconditionally.
        assert!(validate_updates(&[FieldUpdate::Rm(0.0)], true).is_err());

        // max_context_tokens == 0 (the "window") rejected.
        assert!(validate_updates(&[FieldUpdate::MaxContextTokens(0)], true).is_err());

        // below-sentinel wm needs the ack flag; the same value with the ack
        // flag is accepted.
        assert!(validate_updates(&[FieldUpdate::Wm(1.0)], false).is_err());
        assert!(validate_updates(&[FieldUpdate::Wm(1.0)], true).is_ok());

        // A field that is not being touched at all needs no ack -- an
        // untouched, already-below-sentinel `wm` inherited from a prior
        // cell is never re-validated by this call.
        assert!(validate_updates(&[FieldUpdate::Rm(0.2)], false).is_ok());
    }

    #[test]
    fn parse_field_accepts_a_capability_flag_and_rejects_a_malformed_pair() {
        match parse_field("cap:web_search=true") {
            Ok(FieldUpdate::Capability(name, flag)) => {
                assert_eq!(name, "web_search");
                assert!(flag);
            }
            other => panic!("expected a Capability update, got {other:?}"),
        }

        let err = parse_field("no-equals-sign").expect_err("must reject a pair with no `=`");
        assert!(
            matches!(err, CatalogWriteError::InvalidField { .. }),
            "{err}"
        );

        let err = parse_field("wm=not-a-number").expect_err("must reject a malformed number");
        assert!(
            matches!(err, CatalogWriteError::InvalidField { .. }),
            "{err}"
        );
    }

    fn minimal_config() -> Config {
        let toml = r#"
[server]
host = "127.0.0.1"
port = 4000
"#;
        toml::from_str(toml).expect("minimal config should parse")
    }
}
