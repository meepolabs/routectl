//! config.toml provider-block (and optional pool) edit + commit.

use std::path::Path;

use ::toml_edit::{DocumentMut, Table};
use routectl_core::Result;
use routectl_router::{Config, EditOutcome, ProviderEntry, edit_config_toml};

use crate::commands::edit_pipeline::{
    RelockValidationError, gate, insert_provider_block, parse_document, render_write_error,
};
use crate::commands::login_surface::{
    PoolAction, apply_pool_action, join_action, render_pool_block,
};
use crate::commands::provider_entry::{provider_entry_table, render_provider_block};

/// The `[providers.<name>]` table `provider add` writes for `entry`.
pub(super) fn provider_table(entry: &ProviderEntry) -> Result<Table> {
    provider_entry_table(entry)
}

/// The one candidate `provider add` writes: the provider block, plus the
/// pool edit `--pool` asks for. Applied once for the pre-confirmation gate
/// and again under the write lock, so both see the same document.
pub(super) struct Candidate<'a> {
    pub(super) name: &'a str,
    pub(super) block: Table,
    /// `false` when the file already holds this exact block and only the
    /// pool half is owed, so the operator's block keeps its bytes.
    pub(super) writes_block: bool,
    pub(super) pool: Option<PoolAction>,
}

impl Candidate<'_> {
    /// Insert the block, then grow or create the pool. Deterministic given
    /// the same input document.
    pub(super) fn apply(&self, doc: &mut DocumentMut) -> Result<()> {
        if self.writes_block {
            insert_provider_block(doc, self.name, self.block.clone())?;
        }
        match &self.pool {
            Some(action) => apply_pool_action(doc, action),
            None => Ok(()),
        }
    }

    /// The pasteable delta shown before confirmation: the provider block
    /// it writes, then the pool block it edits.
    pub(super) fn render(&self) -> String {
        let entry = self
            .writes_block
            .then(|| render_provider_block(self.name, &self.block));
        let pool = self.pool.as_ref().map(render_pool_block);
        entry.into_iter().chain(pool).collect::<Vec<_>>().join("\n")
    }

    /// The candidate config text over `snapshot_text`, for the gate.
    pub(super) fn text(&self, snapshot_text: &str) -> Result<String> {
        let mut doc = parse_document(snapshot_text)?;
        self.apply(&mut doc)?;
        Ok(doc.to_string())
    }
}

/// The pool edit that puts `member` in `pool`: none when the pool already
/// lists it, a join when the pool exists, else a creation. Joining an
/// existing pool is an explicit edit, so its `accepts_new_logins` marker is
/// not consulted.
pub(super) fn plan_pool(config: &Config, pool: &str, member: &str) -> Option<PoolAction> {
    match config.pools.get(pool) {
        Some(entry) if entry.members.iter().any(|m| m == member) => None,
        Some(_) => Some(join_action(config, pool, member)),
        None => Some(PoolAction::Create {
            pool: pool.to_string(),
            members: vec![member.to_string()],
        }),
    }
}

/// Re-read `config_path` under the advisory lock + revision check and commit
/// the same deterministic candidate atomically. The `snapshot` bytes MUST be
/// the bytes the caller read earlier; a mismatch is a stale-snapshot conflict
/// and nothing is written.
pub(super) fn commit(
    config_path: &Path,
    snapshot: &[u8],
    snapshot_text: &str,
    candidate: &Candidate<'_>,
) -> Result<EditOutcome> {
    let result = edit_config_toml::<RelockValidationError, _>(config_path, snapshot, |doc| {
        candidate.apply(doc).map_err(|_| RelockValidationError)?;
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
