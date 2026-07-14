//! `routectl config migrate` -- bring a legacy `config.toml` forward to the
//! current schema version through the shared migration ladder, committing the
//! result through the same single write primitive as `config set`.
//!
//! The pipeline mirrors `config_edit`'s discipline: every transform runs in
//! memory, the candidate is validated through the SAME shared gate the reload
//! path runs, and the operator acknowledges the break BEFORE any on-disk
//! mutation -- the ladder's v1 rung writes `config.toml` and the overlay as it
//! runs, so authorization has to clear ahead of it, not after. A refusal, a
//! declined prompt, a gate failure, or a stale-bytes conflict leaves the file
//! byte-identical.
//!
//!   1. Snapshot the raw bytes and read the file's raw `version`.
//!   2. Authorize FIRST on a real migration below the current version:
//!      interactive `y`, or `--force` non-interactively; a non-interactive run
//!      without `--force` refuses. This precedes the ladder because the v1 rung
//!      mutates disk, so a declined prompt must never leave a half-migrated
//!      file. `--dry-run`, an already-current file, and a future-version file
//!      need no acknowledgement (none of them mutate the real config).
//!   3. Run the [`migrate_to_current`] ladder to produce the candidate. A
//!      [`Refusal`] (behavior-bearing / malformed retry lists) or a
//!      future-version file renders its reason plus an explicit "nothing was
//!      written" and exits non-zero.
//!   4. Gate the candidate through the shared `parse_config` +
//!      `validation_report` suite; a gate failure renders the report and
//!      writes nothing.
//!   5. `--dry-run` renders the exact candidate file text plus a change
//!      summary and stops here -- it cannot write by construction and needs no
//!      acknowledgement (see [`run_ladder_for_dry_run`] for how the v1 rung's
//!      IO is kept off the real files).
//!   6. Commit the v2->v3 rung through [`edit_config_toml`] (base-bytes
//!      revision check -> conflict = no write). For a v1 file the ladder has
//!      already atomically stamped `version = 2` on disk, so the base-bytes
//!      check re-snapshots the now-v2 file (a crash between the two rungs is
//!      recoverable: a rerun continues at v2).
//!
//! Audit events on the migrator surface carry from/to version, dry-run,
//! ack/force, outcome, refusal kind, and the config path -- never the
//! candidate bytes and never a config value.

use std::collections::BTreeMap;
use std::path::Path;

use routectl_core::{Error, Result};
use routectl_router::{
    CURRENT_CONFIG_VERSION, CachePricingOverride, Config, ConfigWriteError, EditOutcome,
    MigrateError, Refusal, StepOutcome, V1Migration, edit_config_toml, migrate_to_current,
    migrate_v2_to_v3, parse_config,
};
use toml_edit::DocumentMut;

use super::config::validation_report;
use super::parse_error_redaction::redact_parse_error;

/// A config with no `version` key predates the schema and is treated as v1,
/// matching `preflight_config_version`'s legacy default.
const LEGACY_CONFIG_VERSION: u32 = 1;

/// Outcome of a completed [`run`], for the caller and for tests. Hard failures
/// (a ladder refusal, a gate rejection, a future-version file, a write
/// conflict) surface as `Err` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateResult {
    /// The file was already at the current version; nothing was written.
    AlreadyCurrent,
    /// `--dry-run` rendered the candidate; nothing was written.
    DryRun,
    /// The migration was committed atomically.
    Migrated { from_version: u32 },
    /// The acknowledgement prompt was declined; nothing further was written.
    Aborted,
}

/// The `edit_fn` closure's error for the final commit. The ladder already ran
/// the v2->v3 transform and the shared gate once against the same content, so
/// both variants are belt-and-suspenders guards a deterministic re-run never
/// reaches.
#[derive(Debug, thiserror::Error)]
enum CommitError {
    #[error("migration refused under the write lock:\n{0}")]
    Refused(Refusal),
    #[error("migrated config failed re-validation under the write lock")]
    Revalidation,
}

/// Run the migrate pipeline against the default config + overlay paths.
pub fn run(config_path: &Path, dry_run: bool, force: bool) -> Result<MigrateResult> {
    run_at(
        config_path,
        &routectl_router::overlay_default_path(),
        dry_run,
        force,
    )
}

/// Core of [`run`], taking the overlay path explicitly so tests point both the
/// config and the overlay at a temp directory instead of the real files.
pub fn run_at(
    config_path: &Path,
    overlay_path: &Path,
    dry_run: bool,
    force: bool,
) -> Result<MigrateResult> {
    let snapshot = std::fs::read(config_path).map_err(|e| {
        Error::Config(format!(
            "cannot read config `{}`: {e}",
            config_path.display()
        ))
    })?;
    let snapshot_text = String::from_utf8(snapshot.clone()).map_err(|e| {
        Error::Config(format!(
            "config `{}` is not UTF-8: {e}",
            config_path.display()
        ))
    })?;

    let mut doc = parse_document(&snapshot_text)?;
    let from_version = raw_version_of(&doc)?;

    // The v1 rung folds the operator's `[cache_pricing]` table (merged with
    // any legacy sidecar) into the catalog overlay; only a v1 file needs it.
    let cache_pricing = if from_version <= LEGACY_CONFIG_VERSION {
        load_v1_cache_pricing(&snapshot_text, config_path)?
    } else {
        BTreeMap::new()
    };

    // Authorization must precede ANY on-disk mutation. On a real migration
    // below the current version, the ladder's v1 rung rewrites `config.toml`
    // and folds the overlay as it runs, so the acknowledgement has to clear
    // BEFORE the ladder is invoked -- a declined prompt leaves the file
    // byte-identical. `from_version` is known from the cheap snapshot read;
    // the prompt states `from_version -> CURRENT_CONFIG_VERSION` without
    // needing the ladder's output. An already-current or future-version file
    // never enters this branch (both are ladder no-ops / errors that touch
    // nothing), and `--dry-run` acknowledges nothing by construction.
    if !dry_run
        && from_version < CURRENT_CONFIG_VERSION
        && !confirm_migration(from_version, CURRENT_CONFIG_VERSION, force)
    {
        println!("aborted; nothing further written.");
        audit_event(
            config_path,
            from_version,
            CURRENT_CONFIG_VERSION,
            false,
            false,
            force,
            "aborted",
            None,
        );
        return Ok(MigrateResult::Aborted);
    }

    let steps = if dry_run && from_version <= LEGACY_CONFIG_VERSION {
        run_ladder_for_dry_run(&mut doc, from_version, &snapshot, &cache_pricing)?
    } else {
        let v1 = V1Migration {
            cache_pricing: &cache_pricing,
            config_path,
            overlay_path,
        };
        migrate_to_current(&mut doc, from_version, &v1)
            .map_err(|e| render_ladder_error(e, config_path, from_version, dry_run))?
    };

    if steps.is_empty() {
        println!("config is already at version {CURRENT_CONFIG_VERSION}; nothing to migrate.");
        audit_event(
            config_path,
            from_version,
            from_version,
            dry_run,
            false,
            false,
            "no_change",
            None,
        );
        return Ok(MigrateResult::AlreadyCurrent);
    }

    let to_version = steps
        .last()
        .map_or(CURRENT_CONFIG_VERSION, |s| s.to_version);
    let candidate_text = doc.to_string();

    gate(&candidate_text).map_err(|errors| {
        render_gate_errors(&errors);
        audit_event(
            config_path,
            from_version,
            to_version,
            dry_run,
            false,
            false,
            "invalid",
            None,
        );
        Error::Config(format!("{} config error(s)", errors.len()))
    })?;

    let removed = removed_keys(&snapshot_text, from_version);

    if dry_run {
        render_dry_run(&candidate_text, from_version, to_version, &removed);
        audit_event(
            config_path,
            from_version,
            to_version,
            true,
            false,
            false,
            "dry_run",
            None,
        );
        return Ok(MigrateResult::DryRun);
    }

    // The break was acknowledged before the ladder ran (see the authorization
    // gate above), so the acked path commits directly. For a v1 file the
    // ladder already atomically stamped `version = 2` on disk, so the
    // base-bytes revision check must run against the current (v2) file, not
    // the original v1 snapshot.
    let touched_v1 = steps
        .iter()
        .any(|s| s.from_version == LEGACY_CONFIG_VERSION);
    let base = if touched_v1 {
        std::fs::read(config_path).map_err(|e| {
            Error::Config(format!(
                "cannot re-read config `{}` after the v1 migration step: {e}",
                config_path.display()
            ))
        })?
    } else {
        snapshot
    };

    let result = edit_config_toml::<CommitError, _>(config_path, &base, |d| {
        migrate_v2_to_v3(d).map_err(CommitError::Refused)?;
        match gate(&d.to_string()) {
            Ok(_) => Ok(EditOutcome::Modified),
            Err(_) => Err(CommitError::Revalidation),
        }
    })
    .map_err(render_write_error)?;

    if result.outcome == EditOutcome::Unchanged {
        println!("config is already at version {CURRENT_CONFIG_VERSION}; nothing to migrate.");
        audit_event(
            config_path,
            from_version,
            to_version,
            false,
            !force,
            force,
            "no_change",
            None,
        );
        return Ok(MigrateResult::AlreadyCurrent);
    }

    audit_event(
        config_path,
        from_version,
        to_version,
        false,
        !force,
        force,
        "written",
        None,
    );
    println!(
        "migrated config to version {to_version}. Restart any running routectl daemon onto the \
         matching binary to pick up the change."
    );
    Ok(MigrateResult::Migrated { from_version })
}

/// Read the file's raw `version` off the document: an absent key is legacy v1,
/// a present-but-non-integer value is a malformed file the ladder cannot act on.
fn raw_version_of(doc: &DocumentMut) -> Result<u32> {
    match doc.get("version") {
        None => Ok(LEGACY_CONFIG_VERSION),
        Some(item) => match item.as_integer().and_then(|i| u32::try_from(i).ok()) {
            Some(v) => Ok(v),
            None => Err(Error::Config(
                "config `version` is not a non-negative integer; fix it before migrating".into(),
            )),
        },
    }
}

/// Build the v1 rung's `cache_pricing` input: the file's `[cache_pricing]`
/// table merged with any legacy `pricing_verifications.json` stamp. The
/// sidecar is resolved as a sibling of the config file so the merge is
/// hermetic in tests (a temp dir has no sidecar -> empty) and correct in
/// production (the sidecar lives beside `config.toml` in the config dir).
fn load_v1_cache_pricing(
    snapshot_text: &str,
    config_path: &Path,
) -> Result<BTreeMap<String, CachePricingOverride>> {
    let mut config: Config = parse_config(snapshot_text).map_err(|e| {
        Error::Config(format!(
            "legacy config does not parse; fix it before migrating: {e}"
        ))
    })?;
    let sidecar = config_path.with_file_name("pricing_verifications.json");
    match super::catalog::load_verifications(&sidecar) {
        Ok(v) => {
            let skipped = super::catalog::merge_verifications_into(&mut config, &v);
            for sel in &skipped {
                tracing::warn!(
                    selector = %sel,
                    "pricing verification has a malformed date and was ignored during migration"
                );
            }
        }
        Err(e) => tracing::warn!(
            path = %sidecar.display(),
            error = %e,
            "pricing verifications sidecar could not be loaded; skipping merge"
        ),
    }
    Ok(config.cache_pricing)
}

/// Run the ladder for a `--dry-run` on a v1 file against a throwaway copy of
/// the config (and a fresh temp overlay), so the v1 rung's atomic
/// `config.toml` rewrite and overlay fold land on temp files that vanish with
/// the `TempDir` -- the real config and overlay are provably untouched. `doc`
/// is left holding the fully-migrated candidate the caller renders.
fn run_ladder_for_dry_run(
    doc: &mut DocumentMut,
    from_version: u32,
    snapshot: &[u8],
    cache_pricing: &BTreeMap<String, CachePricingOverride>,
) -> Result<Vec<StepOutcome>> {
    let tmp = tempfile::tempdir().map_err(|e| {
        Error::Config(format!(
            "cannot create a scratch directory for dry-run: {e}"
        ))
    })?;
    let tmp_config = tmp.path().join("config.toml");
    let tmp_overlay = tmp.path().join("catalog_overlay.json");
    std::fs::write(&tmp_config, snapshot)
        .map_err(|e| Error::Config(format!("cannot stage dry-run config copy: {e}")))?;

    let v1 = V1Migration {
        cache_pricing,
        config_path: &tmp_config,
        overlay_path: &tmp_overlay,
    };
    migrate_to_current(doc, from_version, &v1)
        .map_err(|e| render_ladder_error(e, &tmp_config, from_version, true))
}

/// Shared validation gate: `parse_config` then the centralized validator suite
/// the reload path runs. Returns the rendered error lines on failure. The
/// `parse_config` error is stripped of its verbatim source-line preview first
/// -- toml echoes the offending config line into the diagnostic, and that line
/// could carry a `literal:` credential.
fn gate(candidate_text: &str) -> std::result::Result<Config, Vec<String>> {
    let config = parse_config(candidate_text).map_err(|e| vec![redact_parse_error(&e)])?;
    let report = validation_report(&config, Some(candidate_text));
    if report.errors.is_empty() {
        Ok(config)
    } else {
        Err(report.errors)
    }
}

/// Redact a `parse_config` error down to provably-safe content before it
/// reaches the terminal. The shared allowlist/fail-safe implementation lives in
/// [`super::parse_error_redaction`] so `doctor` and this command never diverge.
fn render_gate_errors(errors: &[String]) {
    eprintln!(
        "migrated config rejected ({} error(s)); nothing was written:",
        errors.len()
    );
    for e in errors {
        eprintln!("  - {e}");
    }
}

/// Map a ladder error to a user-facing error, emitting the matching audit
/// event. A [`MigrateError::Refused`] renders the source-located guidance plus
/// an explicit "nothing was written"; a future-version file explains the
/// upgrade path.
fn render_ladder_error(
    err: MigrateError,
    config_path: &Path,
    from_version: u32,
    dry_run: bool,
) -> Error {
    match &err {
        MigrateError::Refused(refusal) => {
            let kind = refusal_kind(refusal);
            eprintln!("{err}");
            eprintln!("nothing was written.");
            audit_event(
                config_path,
                from_version,
                from_version,
                dry_run,
                false,
                false,
                "refused",
                Some(kind),
            );
        }
        MigrateError::VersionTooNew { .. } => {
            eprintln!("{err}");
            eprintln!("nothing was written.");
            audit_event(
                config_path,
                from_version,
                from_version,
                dry_run,
                false,
                false,
                "version_too_new",
                None,
            );
        }
        MigrateError::V1ToV2(_) => {
            eprintln!("{err}");
            eprintln!("nothing was written.");
            audit_event(
                config_path,
                from_version,
                from_version,
                dry_run,
                false,
                false,
                "v1_migration_failed",
                None,
            );
        }
    }
    Error::Config(err.to_string())
}

const fn refusal_kind(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::BehaviorBearing { .. } => "behavior_bearing",
        Refusal::Malformed { .. } => "malformed",
    }
}

fn render_write_error(err: ConfigWriteError<CommitError>) -> Error {
    Error::Config(err.to_string())
}

/// The keys this migration removes, for the dry-run change summary. Derived
/// from the ORIGINAL document so the summary names exactly what leaves.
fn removed_keys(snapshot_text: &str, from_version: u32) -> Vec<String> {
    let mut removed = Vec::new();
    if let Ok(doc) = snapshot_text.parse::<DocumentMut>() {
        if let Some(retry) = doc.get("retry").and_then(|i| i.as_table_like()) {
            if retry.contains_key("retry_allowlist") {
                removed.push("retry.retry_allowlist".to_string());
            }
            if retry.contains_key("retry_denylist") {
                removed.push("retry.retry_denylist".to_string());
            }
        }
        if from_version <= LEGACY_CONFIG_VERSION && doc.contains_key("cache_pricing") {
            removed.push("[cache_pricing] (folded into the catalog overlay)".to_string());
        }
    }
    removed
}

fn render_dry_run(candidate_text: &str, from_version: u32, to_version: u32, removed: &[String]) {
    println!("--- candidate config.toml (version {to_version}) ---");
    print!("{candidate_text}");
    if !candidate_text.ends_with('\n') {
        println!();
    }
    println!("--- end candidate ---");
    println!("summary: migrates config from version {from_version} to {to_version}");
    if removed.is_empty() {
        println!("  (no keys removed; version stamp only)");
    } else {
        for key in removed {
            println!("  - removes `{key}`");
        }
    }
    println!("dry-run: nothing was written.");
}

/// Acknowledge the schema break before the write lock. `--force` bypasses the
/// prompt; a non-interactive run without `--force` reads EOF and refuses.
/// Never called while the write lock is held.
fn confirm_migration(from_version: u32, to_version: u32, force: bool) -> bool {
    if force {
        return true;
    }
    use std::io::Write as _;
    println!(
        "this migrates config.toml from version {from_version} to {to_version}. The break \
         retires per-status retry lists (and, from a v1 file, the `[cache_pricing]` table). A \
         running routectl daemon must be restarted onto the matching binary after migration."
    );
    print!("proceed with the migration? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[allow(clippy::too_many_arguments)]
fn audit_event(
    config_path: &Path,
    from_version: u32,
    to_version: u32,
    dry_run: bool,
    acknowledged: bool,
    forced: bool,
    outcome: &str,
    refusal_kind: Option<&str>,
) {
    tracing::info!(
        surface = "cli",
        verb = "migrate",
        from_version,
        to_version,
        dry_run,
        acknowledged,
        forced,
        outcome,
        refusal_kind = refusal_kind.unwrap_or(""),
        path = %config_path.display(),
        "config migrate",
    );
}

fn parse_document(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("config does not parse: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean v2 config (empty retry lists) with a valid provider/model/alias
    /// so the migrated v3 result passes the shared gate.
    const V2_CLEAN: &str = "\
# operator note: keep me
version = 2

[server]
host = \"127.0.0.1\"
port = 8787

[retry]
max_attempts = 2
retry_allowlist = []
retry_denylist = []

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    /// A v2 config carrying a behavior-bearing `retry_allowlist` -> refused.
    const V2_BEHAVIOR_BEARING: &str = "\
version = 2

[server]
host = \"127.0.0.1\"
port = 8787

[retry]
retry_allowlist = [503]

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    /// A legacy v1 config (no version key) with a `[cache_pricing]` table and
    /// a valid provider/model/alias.
    const V1_WITH_CACHE_PRICING: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[cache_pricing]
\"openai-compat:grok-*\" = { wm = 1.5, override_acknowledges_cost_risk = true }

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

    struct Fixture {
        _dir: tempfile::TempDir,
        config: std::path::PathBuf,
        overlay: std::path::PathBuf,
    }

    fn fixture(body: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, body).unwrap();
        let overlay = dir.path().join("catalog_overlay.json");
        Fixture {
            _dir: dir,
            config,
            overlay,
        }
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    // -----------------------------------------------------------------
    // Ack matrix: --force writes; non-interactive without --force refuses;
    // dry-run needs neither.
    // -----------------------------------------------------------------

    #[test]
    fn force_migrates_v2_to_v3_and_the_result_revalidates() {
        let f = fixture(V2_CLEAN);
        let result = run_at(&f.config, &f.overlay, false, true).expect("force migrate");
        assert_eq!(result, MigrateResult::Migrated { from_version: 2 });

        let text = read(&f.config);
        assert!(text.contains("version = 3"), "{text}");
        assert!(!text.contains("retry_allowlist"), "{text}");
        assert!(!text.contains("retry_denylist"), "{text}");
        // Comments and unrelated content survive.
        assert!(text.contains("# operator note: keep me"), "{text}");
        // The committed file re-validates through the shared gate.
        gate(&text).expect("migrated config must pass the gate");
    }

    #[test]
    fn non_interactive_without_force_refuses_with_nothing_written() {
        let f = fixture(V2_CLEAN);
        let before = std::fs::read(&f.config).unwrap();

        // stdin is not a TTY under the test harness: read_line hits EOF, so
        // the prompt is declined.
        let result = run_at(&f.config, &f.overlay, false, false).expect("decline is not an error");
        assert_eq!(result, MigrateResult::Aborted);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a declined migration must not write"
        );
    }

    #[test]
    fn v1_non_interactive_without_force_refuses_before_any_mutation() {
        // A v1 file's migration mutates disk INSIDE the ladder (the v1 rung
        // rewrites config.toml to v2 and folds the overlay). Authorization
        // runs before the ladder, so a declined non-interactive run (EOF)
        // must leave the file byte-identical at v1 AND never create the
        // overlay -- the regression the batch gate flagged.
        let f = fixture(V1_WITH_CACHE_PRICING);
        let before = std::fs::read(&f.config).unwrap();

        let result = run_at(&f.config, &f.overlay, false, false).expect("decline is not an error");
        assert_eq!(result, MigrateResult::Aborted);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a declined v1 migration must not stamp version = 2"
        );
        assert!(
            !f.overlay.exists(),
            "a declined v1 migration must not fold the overlay"
        );
    }

    #[test]
    fn dry_run_renders_v3_candidate_and_writes_nothing() {
        let f = fixture(V2_CLEAN);
        let before = std::fs::read(&f.config).unwrap();

        let result = run_at(&f.config, &f.overlay, true, false).expect("dry-run");
        assert_eq!(result, MigrateResult::DryRun);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "dry-run must not write or stamp"
        );
    }

    // -----------------------------------------------------------------
    // Refusal: a behavior-bearing v2 list is refused, byte-identical.
    // -----------------------------------------------------------------

    #[test]
    fn behavior_bearing_list_is_refused_byte_identical() {
        let f = fixture(V2_BEHAVIOR_BEARING);
        let before = std::fs::read(&f.config).unwrap();

        let err = run_at(&f.config, &f.overlay, false, true).expect_err("must refuse");
        assert!(err.to_string().contains("503"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a refused migration must leave the file byte-identical"
        );
    }

    #[test]
    fn behavior_bearing_dry_run_is_also_refused_and_writes_nothing() {
        let f = fixture(V2_BEHAVIOR_BEARING);
        let before = std::fs::read(&f.config).unwrap();

        run_at(&f.config, &f.overlay, true, false).expect_err("dry-run must also refuse");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    // -----------------------------------------------------------------
    // v1 chains v1->v2->v3: cache_pricing folded to the overlay AND the
    // retry lists gone; comments preserved.
    // -----------------------------------------------------------------

    #[test]
    fn v1_file_chains_to_v3_folding_cache_pricing_into_the_overlay() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let result = run_at(&f.config, &f.overlay, false, true).expect("v1 migrate");
        assert_eq!(result, MigrateResult::Migrated { from_version: 1 });

        let text = read(&f.config);
        assert!(text.contains("version = 3"), "{text}");
        assert!(!text.contains("cache_pricing"), "{text}");
        gate(&text).expect("migrated v1 config must pass the gate");

        // The cache_pricing entry landed in the overlay.
        let overlay = routectl_router::load_catalog_overlay(&f.overlay).expect("load overlay");
        assert!(
            overlay.cells.contains_key("openai-compat:grok-*"),
            "overlay cells: {:?}",
            overlay.cells
        );
    }

    #[test]
    fn v1_dry_run_touches_neither_config_nor_overlay() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let before = std::fs::read(&f.config).unwrap();

        let result = run_at(&f.config, &f.overlay, true, false).expect("v1 dry-run");
        assert_eq!(result, MigrateResult::DryRun);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "v1 dry-run must not write config.toml"
        );
        assert!(
            !f.overlay.exists(),
            "v1 dry-run must not create the real overlay"
        );
    }

    // -----------------------------------------------------------------
    // Already-current is a no-op.
    // -----------------------------------------------------------------

    #[test]
    fn already_v3_is_a_no_op() {
        let v3 = V2_CLEAN.replacen("version = 2", "version = 3", 1);
        let f = fixture(&v3);
        let before = std::fs::read(&f.config).unwrap();

        let result = run_at(&f.config, &f.overlay, false, true).expect("no-op");
        assert_eq!(result, MigrateResult::AlreadyCurrent);
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    // -----------------------------------------------------------------
    // Gate failure: an invalid candidate writes nothing.
    // -----------------------------------------------------------------

    #[test]
    fn invalid_candidate_writes_nothing() {
        // A v2 config whose alias points at an undefined model migrates
        // cleanly (empty retry lists) but fails the shared cross-field gate.
        let body = V2_CLEAN.replace("default = \"gpt\"", "default = \"no-such-model\"");
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        let err = run_at(&f.config, &f.overlay, false, true).expect_err("gate must reject");
        assert!(err.to_string().contains("config error"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a gate failure must leave the file byte-identical"
        );
    }

    // -----------------------------------------------------------------
    // Secret hygiene: a gate parse failure never echoes the offending
    // source line (which may carry a `literal:` credential).
    // -----------------------------------------------------------------

    #[test]
    fn gate_parse_failure_does_not_echo_a_secret_bearing_source_line() {
        const SECRET: &str = "sk-THIS-IS-A-FAKE-CREDENTIAL-value";
        // An unknown field under a known table: parse_config rejects it, and
        // toml's diagnostic would frame the offending line -- carrying the
        // secret -- unless the preview is redacted.
        let candidate = format!(
            "version = 3\n\n[server]\nhost = \"127.0.0.1\"\nport = 8787\nbogus_secret_key = \"{SECRET}\"\n"
        );

        let errors = gate(&candidate).expect_err("an unknown field must fail the gate");
        for e in &errors {
            assert!(
                !e.contains("FAKE-CREDENTIAL"),
                "the secret-bearing source line must be redacted, got: {e}"
            );
        }
        // The offending field name is user-controlled (a TOML key can be a
        // quoted secret), so it is dropped; the diagnostic still classes the
        // failure and the header keeps the line/column for locating it.
        assert!(
            errors.iter().any(|e| e.contains("unknown field")),
            "the redacted error must still class the failure, got: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| !e.contains("bogus_secret_key")),
            "the user-controlled field name must be dropped, got: {errors:?}"
        );
    }

    #[test]
    fn gate_type_mismatch_in_non_string_field_does_not_survive() {
        // A fake secret mistyped into the numeric `port` field: serde renders
        // `invalid type: string "...", expected u16`, embedding it verbatim.
        const SECRET: &str = "sk-THIS-IS-A-FAKE-CREDENTIAL-value";
        let candidate =
            format!("version = 3\n\n[server]\nhost = \"127.0.0.1\"\nport = \"{SECRET}\"\n");

        let errors = gate(&candidate).expect_err("a type mismatch must fail the gate");
        for e in &errors {
            assert!(
                !e.contains("FAKE-CREDENTIAL"),
                "the mistyped secret must not survive redaction, got: {e}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Conflict: stale base bytes (concurrent edit) refuse with no write.
    // -----------------------------------------------------------------

    #[test]
    fn stale_base_bytes_conflict_writes_nothing() {
        // Snapshot v2 bytes, then commit an already-v3 candidate against them
        // through the primitive directly: the on-disk file was concurrently
        // replaced, so the base-bytes revision check must refuse.
        let f = fixture(V2_CLEAN);
        let stale = std::fs::read(&f.config).unwrap();

        // Concurrent writer replaces the file.
        std::fs::write(
            &f.config,
            V2_CLEAN.replacen("port = 8787", "port = 8788", 1),
        )
        .unwrap();
        let after_concurrent = std::fs::read(&f.config).unwrap();

        let err = edit_config_toml::<CommitError, _>(&f.config, &stale, |d| {
            migrate_v2_to_v3(d).map_err(CommitError::Refused)?;
            Ok(EditOutcome::Modified)
        })
        .expect_err("stale base bytes must conflict");
        assert!(
            matches!(err, ConfigWriteError::Conflict { .. }),
            "err: {err}"
        );
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            after_concurrent,
            "a conflict must leave the concurrent write in place, not clobber it"
        );
    }

    // -----------------------------------------------------------------
    // Audit event: carries from/to version, dry_run, ack/force, outcome,
    // path -- and never a value or candidate bytes.
    // -----------------------------------------------------------------

    #[test]
    fn emits_audit_event_with_versions_and_no_bytes() {
        let f = fixture(V2_CLEAN);
        let events = routectl_testkit::capture_events(|| {
            run_at(&f.config, &f.overlay, false, true).expect("migrate");
        });

        let audit: Vec<_> = events
            .iter()
            .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("migrate"))
            .collect();
        assert_eq!(audit.len(), 1, "exactly one migrate audit event expected");

        let event = audit[0];
        assert_eq!(event.field("from_version"), Some("2"));
        assert_eq!(event.field("to_version"), Some("3"));
        assert_eq!(event.field("dry_run"), Some("false"));
        assert_eq!(event.field("forced"), Some("true"));
        assert_eq!(event.field("outcome"), Some("written"));
        // No candidate bytes / config values are ever fields.
        assert!(event.field("candidate").is_none());
        assert!(event.field("value").is_none());
    }

    #[test]
    fn refusal_audit_event_names_the_kind() {
        let f = fixture(V2_BEHAVIOR_BEARING);
        let events = routectl_testkit::capture_events(|| {
            let _ = run_at(&f.config, &f.overlay, false, true);
        });
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("refused"));
        assert_eq!(audit.field("refusal_kind"), Some("behavior_bearing"));
    }
}
