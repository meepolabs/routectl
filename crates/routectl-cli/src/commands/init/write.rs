//! The one final `init` config write: land the wizard's `[models.<nick>]`
//! wiring and the `aliases.default` route through the SAME single write path
//! (`routectl_router::edit_config_toml`) every config-mutating command uses,
//! mirroring [`super::super::provider_add`]'s `insert_provider_block` +
//! `commit` exactly.
//!
//! This is the routing-lands-here-and-only-here step: it consumes the
//! resolved [`ModelWiring`] set and the chosen default alias, it does NOT
//! build them. Providers are wired earlier; this step assumes an existing
//! `config.toml` (the edit primitive re-reads it under the advisory lock and
//! refuses on a stale snapshot).

use std::path::Path;

use routectl_core::{Error, Result};
use routectl_router::{EditOutcome, edit_config_toml};
use toml_edit::{DocumentMut, Item, Table, value};

use super::ModelWiring;
use crate::commands::edit_pipeline::{RelockValidationError, gate, render_write_error};

/// Insert every `[models.<nick>]` block (`provider` + `upstream` only;
/// serde defaults like `selectable` are omitted) and set
/// `aliases.default = <default_alias>`, descending into (or creating) the
/// `models` and `aliases` tables via `as_table_like_mut` so existing
/// providers', models', and aliases' comments and ordering survive. A model
/// whose nick already exists is replaced with the freshly-built block; the
/// default alias key is (re)set. Deterministic given the same input document
/// -- the commit closure relies on this to decide a no-op.
pub fn insert_models_and_default_alias(
    doc: &mut DocumentMut,
    models: &[ModelWiring],
    default_alias: &str,
) -> Result<()> {
    insert_models(doc, models)?;
    set_default_alias(doc, default_alias)?;
    Ok(())
}

fn insert_models(doc: &mut DocumentMut, models: &[ModelWiring]) -> Result<()> {
    let root = doc.as_table_mut();
    if !root.contains_key("models") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("models", Item::Table(table));
    }
    let models_tbl = root
        .get_mut("models")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config("`models` exists but is not a table".into()))?;
    for wiring in models {
        models_tbl.insert(wiring.nick.as_str(), Item::Table(model_table(wiring)));
    }
    Ok(())
}

fn set_default_alias(doc: &mut DocumentMut, default_alias: &str) -> Result<()> {
    let root = doc.as_table_mut();
    if !root.contains_key("aliases") {
        let mut table = Table::new();
        table.set_implicit(false);
        root.insert("aliases", Item::Table(table));
    }
    let aliases = root
        .get_mut("aliases")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| Error::Config("`aliases` exists but is not a table".into()))?;
    aliases.insert("default", value(default_alias));
    Ok(())
}

/// The minimal `[models.<nick>]` block: the two required fields as string
/// values, nothing else, so no serde default (`selectable`, empty overlays)
/// lands in the written config. The re-validate gate backstops the shape.
fn model_table(wiring: &ModelWiring) -> Table {
    let mut table = Table::new();
    table.set_implicit(false);
    table.insert("provider", value(wiring.provider.as_str()));
    table.insert("upstream", value(wiring.upstream.as_str()));
    table
}

/// Re-read `config_path` under the advisory lock + base-bytes revision check
/// and commit the same deterministic models/aliases insert atomically.
///
/// No-op / partial-match semantics: the whole candidate document is compared
/// byte-for-byte (after the deterministic insert) against `snapshot_text`.
/// The write is idempotent -- [`EditOutcome::Unchanged`], nothing written --
/// only when EVERY planned model block and the default alias already match
/// exactly (the re-init contract). A partial match (some models already
/// present, others new, or a different default) differs from the snapshot and
/// so writes, landing the missing pieces while leaving the matching ones
/// byte-identical.
///
/// `snapshot` MUST be the bytes the caller read earlier; a mismatch is a
/// stale-snapshot conflict and nothing is written. A candidate that fails the
/// shared gate is refused and nothing is written.
pub fn commit_models_aliases(
    config_path: &Path,
    snapshot: &[u8],
    snapshot_text: &str,
    models: &[ModelWiring],
    default_alias: &str,
) -> Result<EditOutcome> {
    let result = edit_config_toml::<RelockValidationError, _>(config_path, snapshot, |doc| {
        insert_models_and_default_alias(doc, models, default_alias)
            .map_err(|_| RelockValidationError)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_router::CURRENT_CONFIG_VERSION;
    use routectl_router::parse_config;

    fn providers_only() -> String {
        format!("version = {CURRENT_CONFIG_VERSION}\n") + PROVIDERS_ONLY_BODY
    }

    const PROVIDERS_ONLY_BODY: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"
";

    fn with_model() -> String {
        format!("# operator note\nversion = {CURRENT_CONFIG_VERSION}\n") + WITH_MODEL_BODY
    }

    const WITH_MODEL_BODY: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
# keep this comment
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    fn wiring(nick: &str, provider: &str, upstream: &str) -> ModelWiring {
        ModelWiring {
            nick: nick.to_string(),
            provider: provider.to_string(),
            upstream: upstream.to_string(),
        }
    }

    fn apply(text: &str, models: &[ModelWiring], default_alias: &str) -> String {
        let mut doc = text.parse::<DocumentMut>().unwrap();
        insert_models_and_default_alias(&mut doc, models, default_alias).unwrap();
        doc.to_string()
    }

    fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    // -----------------------------------------------------------------
    // Insert writes provider + upstream, sets the default alias, and omits
    // serde defaults; the candidate parses and gates.
    // -----------------------------------------------------------------

    #[test]
    fn insert_writes_models_and_default_alias_omitting_defaults() {
        let out = apply(&providers_only(), &[wiring("gpt", "fast", "gpt-4o")], "gpt");

        assert!(out.contains("[models.gpt]"), "{out}");
        assert!(out.contains("provider = \"fast\""), "{out}");
        assert!(out.contains("upstream = \"gpt-4o\""), "{out}");
        assert!(out.contains("[aliases]"), "{out}");
        assert!(out.contains("default = \"gpt\""), "{out}");
        assert!(
            !out.contains("selectable"),
            "serde defaults must be omitted: {out}"
        );
        parse_config(&out).expect("the written candidate parses and validates");
    }

    // -----------------------------------------------------------------
    // Format preservation: pre-existing comments, section order, and an
    // untouched existing model all survive a surgical insert of a new model.
    // -----------------------------------------------------------------

    #[test]
    fn preserves_comments_section_order_and_existing_models() {
        let out = apply(
            &with_model(),
            &[wiring("claude", "fast", "claude-x")],
            "claude",
        );

        assert!(out.contains("# operator note"), "{out}");
        assert!(out.contains("# keep this comment"), "{out}");
        assert!(
            out.find("# operator note").unwrap() < out.find("[server]").unwrap(),
            "leading comment keeps its position: {out}"
        );
        // The pre-existing model is untouched; the new one is appended.
        assert!(out.contains("[models.gpt]"), "{out}");
        assert!(out.contains("[models.claude]"), "{out}");
        assert!(out.contains("upstream = \"claude-x\""), "{out}");
        assert!(out.contains("default = \"claude\""), "{out}");
    }

    // -----------------------------------------------------------------
    // Commit routes through the one write path: a valid plan lands.
    // -----------------------------------------------------------------

    #[test]
    fn commit_lands_models_and_default_alias() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &providers_only());
        let snapshot = std::fs::read(&path).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();

        let outcome = commit_models_aliases(
            &path,
            &snapshot,
            &snapshot_text,
            &[wiring("gpt", "fast", "gpt-4o")],
            "gpt",
        )
        .expect("commit");
        assert_eq!(outcome, EditOutcome::Modified);

        let text = std::fs::read_to_string(&path).unwrap();
        let config = parse_config(&text).expect("committed config parses");
        assert!(config.models.contains_key("gpt"), "{text}");
        assert!(text.contains("default = \"gpt\""), "{text}");
    }

    // -----------------------------------------------------------------
    // Re-init idempotence: re-applying an identical plan writes nothing and
    // leaves the file byte-identical.
    // -----------------------------------------------------------------

    #[test]
    fn identical_replan_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &providers_only());
        let snapshot = std::fs::read(&path).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();

        let plan = [wiring("gpt", "fast", "gpt-4o")];
        assert_eq!(
            commit_models_aliases(&path, &snapshot, &snapshot_text, &plan, "gpt")
                .expect("first commit"),
            EditOutcome::Modified
        );

        let after_first = std::fs::read(&path).unwrap();
        let after_first_text = String::from_utf8(after_first.clone()).unwrap();

        assert_eq!(
            commit_models_aliases(&path, &after_first, &after_first_text, &plan, "gpt")
                .expect("re-commit"),
            EditOutcome::Unchanged
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            after_first,
            "an identical re-plan must leave the file byte-identical"
        );
    }

    // -----------------------------------------------------------------
    // Partial match: an identical model plus a new one writes, landing only
    // the missing block while leaving the matching one byte-identical.
    // -----------------------------------------------------------------

    #[test]
    fn partial_match_writes_only_the_missing_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &providers_only());
        let snapshot = std::fs::read(&path).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();

        commit_models_aliases(
            &path,
            &snapshot,
            &snapshot_text,
            &[wiring("gpt", "fast", "gpt-4o")],
            "gpt",
        )
        .expect("first commit");

        let after_first = std::fs::read(&path).unwrap();
        let after_first_text = String::from_utf8(after_first.clone()).unwrap();
        let gpt_block = "[models.gpt]\nprovider = \"fast\"\nupstream = \"gpt-4o\"";
        assert!(after_first_text.contains(gpt_block), "{after_first_text}");

        let outcome = commit_models_aliases(
            &path,
            &after_first,
            &after_first_text,
            &[
                wiring("gpt", "fast", "gpt-4o"),
                wiring("claude", "fast", "claude-x"),
            ],
            "gpt",
        )
        .expect("partial commit");
        assert_eq!(outcome, EditOutcome::Modified);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(gpt_block),
            "the already-present model stays byte-identical: {text}"
        );
        assert!(text.contains("[models.claude]"), "{text}");
        let config = parse_config(&text).unwrap();
        assert!(config.models.contains_key("gpt"));
        assert!(config.models.contains_key("claude"));
    }

    // -----------------------------------------------------------------
    // A candidate that fails the shared gate (default points at an undefined
    // model) is refused and nothing is written.
    // -----------------------------------------------------------------

    #[test]
    fn gate_rejection_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &providers_only());
        let snapshot = std::fs::read(&path).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();
        let before = snapshot.clone();

        let err = commit_models_aliases(
            &path,
            &snapshot,
            &snapshot_text,
            &[wiring("gpt", "fast", "gpt-4o")],
            "ghost",
        )
        .expect_err("a default aliasing an undefined model must be rejected");
        assert!(!err.to_string().is_empty(), "err: {err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a gate rejection must leave the file byte-identical"
        );
    }

    // -----------------------------------------------------------------
    // Stale-snapshot conflict: the write refuses and the on-disk file is
    // untouched (mirrors provider_add's stale-snapshot test).
    // -----------------------------------------------------------------

    #[test]
    fn stale_snapshot_conflict_leaves_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), &providers_only());
        let stale = std::fs::read(&path).unwrap();
        let stale_text = String::from_utf8(stale.clone()).unwrap();

        // Something else rewrote the file after the caller snapshotted it.
        let rewritten = format!("{}# added out of band\n", providers_only());
        std::fs::write(&path, &rewritten).unwrap();

        let err = commit_models_aliases(
            &path,
            &stale,
            &stale_text,
            &[wiring("gpt", "fast", "gpt-4o")],
            "gpt",
        )
        .expect_err("a stale snapshot must conflict");
        assert!(err.to_string().contains("changed on disk"), "err: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            rewritten,
            "a conflict must leave the on-disk file untouched"
        );
    }
}
