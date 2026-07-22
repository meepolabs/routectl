//! config.toml provider-block edit + commit.

use std::path::Path;

use ::toml_edit::{Array, DocumentMut, InlineTable, Item, Table};
use routectl_core::{Error, Result};
use routectl_router::{EditOutcome, ProviderEntry, edit_config_toml};

use crate::commands::edit_pipeline::{RelockValidationError, gate, render_write_error};

/// Serialize a [`ProviderEntry`] into a standard (non-inline) `toml_edit`
/// table, dropping the empty collection defaults serde emits (an empty
/// `header_extras` map, empty `allowed_betas` list) so the written block
/// stays minimal. The re-validate gate is the backstop for anything pruned.
pub(super) fn provider_table(entry: &ProviderEntry) -> Result<Table> {
    let text = toml::to_string(entry)
        .map_err(|e| Error::Config(format!("serialize provider entry: {e}")))?;
    let doc = parse_document(&text)?;
    let mut table = doc.as_table().clone();
    table.set_implicit(false);
    prune_empty_children(&mut table);
    Ok(table)
}

/// Drop top-level keys of `table` whose value is an empty table, array, or
/// inline table -- serde-emitted defaults that carry no operator intent.
fn prune_empty_children(table: &mut Table) {
    let empties: Vec<String> = table
        .iter()
        .filter(|(_, item)| is_empty_item(item))
        .map(|(k, _)| k.to_string())
        .collect();
    for key in empties {
        table.remove(&key);
    }
}

fn is_empty_item(item: &Item) -> bool {
    match item {
        Item::None => true,
        Item::Table(t) => t.is_empty(),
        Item::ArrayOfTables(a) => a.is_empty(),
        Item::Value(v) => v
            .as_array()
            .map(Array::is_empty)
            .or_else(|| v.as_inline_table().map(InlineTable::is_empty))
            .unwrap_or(false),
    }
}

pub(super) fn parse_document(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("config does not parse: {e}")))
}

/// Insert `block` at `[providers.<name>]`, descending into (or creating) the
/// `providers` table via `as_table_like_mut` so existing providers' comments
/// and ordering survive. A same-name insert replaces the whole block
/// (`--overwrite`). Deterministic given the same input document (the
/// write closure relies on this).
pub(super) fn insert_provider_block(doc: &mut DocumentMut, name: &str, block: Table) -> Result<()> {
    let root = doc.as_table_mut();
    if !root.contains_key("providers") {
        let mut providers = Table::new();
        providers.set_implicit(true);
        root.insert("providers", Item::Table(providers));
    }
    let providers = root
        .get_mut("providers")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config("`providers` exists but is not a table".into()))?;
    providers.insert(name, Item::Table(block));
    Ok(())
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
