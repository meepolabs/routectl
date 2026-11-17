//! config.toml provider-block edit + commit.

use std::path::Path;

use ::toml_edit::Table;
use routectl_core::Result;
use routectl_router::{EditOutcome, ProviderEntry, edit_config_toml};

use crate::commands::edit_pipeline::{
    RelockValidationError, gate, insert_provider_block, render_write_error,
};
use crate::commands::provider_entry::provider_entry_table;

/// The `[providers.<name>]` table `provider add` writes for `entry`.
pub(super) fn provider_table(entry: &ProviderEntry) -> Result<Table> {
    provider_entry_table(entry)
}

/// Re-read `config_path` under the advisory lock + revision check and commit
/// the same deterministic insert atomically. The `snapshot` bytes MUST be the
/// bytes the caller read earlier; a mismatch is a stale-snapshot conflict and
/// nothing is written.
pub(super) fn commit(
    config_path: &Path,
    snapshot: &[u8],
    snapshot_text: &str,
    name: &str,
    block: Table,
) -> Result<EditOutcome> {
    let result = edit_config_toml::<RelockValidationError, _>(config_path, snapshot, |doc| {
        insert_provider_block(doc, name, block).map_err(|_| RelockValidationError)?;
        let text = doc.to_string();
        if text == snapshot_text {
            return Ok(EditOutcome::Unchanged);
        }
        match gate(&text) {
            Ok(_) => Ok(EditOutcome::Modified),
            Err(_) => Err(RelockValidationError),
        }
    })
    .map_err(render_write_error)?;
    Ok(result.outcome)
}
