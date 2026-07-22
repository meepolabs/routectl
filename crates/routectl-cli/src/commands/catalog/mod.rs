//! Catalog command entry + shared verified-at helper.
//!
//! `routectl catalog` -- inspect, verify, import, and edit the
//! cache-economics catalog. `pricing` is a hidden alias kept for muscle
//! memory (dropped at 1.0).
//!
//! Subcommands:
//!   list    -- print the EFFECTIVE catalog (the two-layer merge of the
//!              baked table with the on-disk `catalog_overlay.json`,
//!              `routectl_router::merge`) as an aligned ASCII table, headed
//!              by an overlay summary line (revision + counts by source +
//!              disabled count -- see [`render::overlay_summary_line`]). Every
//!              row renders PRESENT (with derived provenance + a staleness
//!              marker) or DISABLED (overlay `null`); MISSING never
//!              appears in this catalog-only listing (see
//!              [`render::build_list_data`]'s doc) even though the render path
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
//!              provenance), field by field. See [`write::set_at`] for the
//!              admission rule, the field syntax, and the value-validation
//!              contract it reuses.
//!   disable -- write a JSON-null overlay cell for a KNOWN selector,
//!              disabling it regardless of what it previously carried. See
//!              [`write::disable_at`].
//!
//! LEGACY SIDECAR (`pricing_verifications.json`): this command still carries
//! the READ side of the old sidecar format ([`verifications::PricingVerifications`],
//! [`verifications::load_verifications`], [`verifications::merge_verifications_into`],
//! [`verifications::load_and_merge_verifications`]) -- but ONLY as a read path
//! consumed by the v1 -> v2 config migration
//! ([`verifications::load_and_merge_verifications`] folds any historical
//! sidecar stamps into `config.cache_pricing` before the migrator moves them
//! into the catalog overlay). The config LOADER no longer runs that migration
//! -- it preflight-rejects a too-old config and points the operator at
//! `config migrate`, which owns the ladder that consumes this read path.
//! Nothing in the CLI writes the sidecar anymore -- `verify` now stamps the
//! overlay directly -- so the write side (`save_verification` / the atomic
//! sidecar writer) is gone. The read side stays until v1 config support itself
//! is dropped.

use std::path::PathBuf;

pub mod render;
pub mod verifications;
pub mod write;

pub use render::{build_list_data, list};
pub use verifications::{
    PricingVerifications, load_and_merge_verifications, load_verifications,
    merge_verifications_into,
};
pub use write::{CatalogWriteError, disable, export, set, verify};

pub(crate) use render::render_table;
pub(crate) use write::print_pickup_note;
#[cfg(test)]
pub(crate) use write::{set_at, verify_at};

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

/// Today's date (UTC), the stamp every catalog writer (`verify`, `set`,
/// `import`) uses for `verified_at` -- one shared UTC clock read so the
/// writers can never disagree about "today" across a timezone (this
/// replaces `verify_at`'s prior `chrono::Local` read, which could stamp a
/// different calendar date than `set_at`'s UTC read near a local
/// midnight).
pub(crate) fn today_verified_at() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}
