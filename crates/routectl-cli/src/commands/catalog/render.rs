//! Catalog list rendering.

use std::collections::{BTreeMap, BTreeSet};

use routectl_router::{
    CachePricingSelector, CatalogOverlay, CatalogRow, EffectiveRow, OverlaySource, Source,
    baked_table_rows, is_stale_today, merge, overlay_revision,
};

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

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
