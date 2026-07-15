//! `routectl config migrate` -- bring a legacy `config.toml` forward to the
//! current schema version through the shared migration ladder, committing the
//! result through the same single write primitive as `config set`.
//!
//! The pipeline is check-before-write end to end: the ladder runs as a PURE
//! planning phase ([`plan_migration`]) that produces a `MigrationPlan` whose
//! `write_kind` (`NoChange` / `ConfigOnly(text)` / `ConfigAndOverlay(text,
//! overlay)`) folds the config candidate and the pending overlay write INTO
//! the variant that needs them, so no cross-field invariant is left for the
//! caller to `.expect()`. Planning touches NO disk, so every refusal and
//! validation check clears before any mutation:
//!
//!   1. Snapshot the raw bytes and read the file's raw `version`.
//!   2. Plan the migration purely. A [`Refusal`] (behavior-bearing / malformed
//!      retry lists, or an egress allowlist) or a future-version file surfaces
//!      here with an explicit "nothing was written" -- nothing has been.
//!   3. Gate the candidate config text through the shared `parse_config` +
//!      `validation_report` suite; a gate failure renders the report and
//!      writes nothing.
//!   4. `--dry-run` renders the exact candidate plus a change summary and stops
//!      -- it needs no acknowledgement (nothing is written) and no temp copy
//!      (planning never touched the real files).
//!   5. Acknowledge EVERY real write (a version bump OR a same-version v3
//!      normalization): interactive `y`, or `--force` non-interactively; a
//!      non-interactive run without `--force` refuses. The acknowledgement runs
//!      AFTER the gate (only a valid migration is worth prompting for) and
//!      BEFORE any write.
//!   6. Commit in two phases: the overlay FIRST (revision-checked, idempotent),
//!      then `config.toml` LAST via [`edit_config_toml`] as the visible
//!      completion marker (base-bytes revision check -> conflict = no write).
//!      A crash between the phases is recoverable: a rerun re-plans (the
//!      overlay fold is now a no-op) and stamps config.toml.
//!
//! Two-file (config + overlay) is not literally atomic without a journal; the
//! honest target is recoverable two-phase + a truthful audit + an idempotent
//! rerun. The overlay commit goes through [`with_overlay_write_lock`] so a
//! concurrent `catalog` writer cannot slip between the revision check and the
//! rename and be silently overwritten. The audit event carries from/to
//! version, dry-run, ack/force, outcome, refusal kind, and the config path --
//! never the candidate bytes and never a config value. The outcome is one of
//! `no_change` / `refused` / `version_too_new` / `v1_migration_failed` /
//! `invalid` / `dry_run` / `aborted` / `written` / `incomplete` / `conflict` /
//! `write_failed`. The `acknowledged` field reflects a REAL prompt: it is true
//! only after an interactive `y`, never synthesized.

use std::collections::BTreeMap;
use std::path::Path;

use routectl_core::{Error, Result};
use routectl_router::{
    CURRENT_CONFIG_VERSION, CachePricingOverride, CatalogOverlay, Config, ConfigWriteError,
    EditOutcome, MigrateError, MigrationPlan, OverlayError, OverlayWrite, Refusal, WriteKind,
    apply_config_transforms, edit_config_toml, parse_config, plan_migration,
    with_overlay_write_lock,
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

    let doc = parse_document(&snapshot_text)?;
    let from_version = raw_version_of(&doc)?;

    // The v1 rung folds the operator's `[cache_pricing]` table (merged with
    // any legacy sidecar) into the catalog overlay; only a v1 file needs it.
    let cache_pricing = if from_version <= LEGACY_CONFIG_VERSION {
        load_v1_cache_pricing(&snapshot_text, config_path)?
    } else {
        BTreeMap::new()
    };

    // PURE planning phase: build the plan with NO on-disk mutation. Every
    // refusal / conflict / future-version check clears here, so a returned
    // plan means all of the migrator's validation has passed.
    let plan = plan_migration(&doc, from_version, &cache_pricing, overlay_path)
        .map_err(|e| render_ladder_error(e, config_path, from_version, dry_run))?;
    let to_version = plan.to;

    // A `NoChange` plan short-circuits; every other plan carries its config
    // candidate in `write_kind`, so the text is read straight off the enum
    // -- no separate `Option` field to `.expect()` against.
    let candidate_text = match plan.config_candidate() {
        None => {
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
        Some(text) => text,
    };

    // Validation gate -- still before any write.
    gate(candidate_text).map_err(|errors| {
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

    // `--dry-run` renders the candidate and stops -- no write, no ack.
    if dry_run {
        render_dry_run(candidate_text, from_version, to_version, &plan.removed_keys);
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

    // Acknowledge EVERY real write (a version bump OR a same-version v3
    // normalization), now that the candidate is known valid and a write is
    // known to be pending. A declined prompt leaves both files byte-identical.
    if !confirm_migration(from_version, to_version, force) {
        println!("aborted; nothing further written.");
        audit_event(
            config_path,
            from_version,
            to_version,
            false,
            false,
            force,
            "aborted",
            None,
        );
        return Ok(MigrateResult::Aborted);
    }

    commit_plan(plan, config_path, overlay_path, &snapshot, from_version).map_err(|failure| {
        // A commit failure AFTER the overlay was written is resumable and must
        // NEVER claim "nothing was written" (the overlay mutation is durable);
        // `failure.outcome` is labelled at the failure site by the underlying
        // error variant, so a revision conflict reads `conflict` while an
        // I/O / parse / revalidation failure reads the neutral `write_failed`.
        audit_event(
            config_path,
            from_version,
            to_version,
            false,
            !force,
            force,
            failure.outcome,
            None,
        );
        *failure.error
    })?;

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
    if from_version == to_version {
        println!(
            "normalized config at version {to_version} (folded legacy `unsupported_features` \
             into [capability.overrides]). Restart any running routectl daemon onto the matching \
             binary to pick up the change."
        );
    } else {
        println!(
            "migrated config to version {to_version}. Restart any running routectl daemon onto \
             the matching binary to pick up the change."
        );
    }
    Ok(MigrateResult::Migrated { from_version })
}

/// A commit failure, carrying the truthful audit outcome the caller should
/// record. The overlay half is committed FIRST, so a config-phase failure
/// after it lands is `incomplete` (resumable, the overlay mutation is
/// durable); a failure before anything lands is labelled by the underlying
/// error variant -- `conflict` for a genuine revision / base-bytes conflict,
/// the neutral `write_failed` for any other I/O / parse / revalidation
/// failure.
struct CommitFailure {
    error: Box<Error>,
    outcome: &'static str,
}

/// Commit a validated [`MigrationPlan`] in two phases: the overlay FIRST
/// (revision-checked, under the overlay write lock, idempotent), then
/// `config.toml` LAST as the visible completion marker. The config write
/// reproduces the SAME pure transform the plan gated, under the write lock
/// against the original snapshot bytes. Takes the plan BY VALUE so the
/// pending overlay cells move into the commit rather than being cloned.
///
/// Two-file commit is not literally atomic without a journal. It is
/// recoverable instead: a crash (or a config-side conflict) after the overlay
/// write leaves `config.toml` at its old version, so a rerun re-plans (the
/// overlay fold is now an idempotent no-op) and completes the config stamp.
fn commit_plan(
    plan: MigrationPlan,
    config_path: &Path,
    overlay_path: &Path,
    snapshot: &[u8],
    from_version: u32,
) -> std::result::Result<(), CommitFailure> {
    // Phase 1: the overlay, under the overlay write lock. The revision check
    // runs INSIDE the lock, so a concurrent writer can neither slip between
    // the check and the rename nor be silently overwritten. A conflict (or
    // any load failure) fails closed BEFORE any write -- a truthful "nothing
    // written".
    let overlay_written = match plan.write_kind {
        WriteKind::ConfigAndOverlay(_, overlay) => {
            commit_overlay(overlay_path, overlay).map_err(|e| CommitFailure {
                error: Box::new(Error::Config(format!(
                    "cache-pricing migration: overlay write failed, nothing was written: {e}"
                ))),
                outcome: match e {
                    OverlayError::RevisionConflict { .. } => "conflict",
                    _ => "write_failed",
                },
            })?;
            true
        }
        _ => false,
    };

    // Phase 2: config.toml LAST, the visible version marker.
    edit_config_toml::<CommitError, _>(config_path, snapshot, |d| {
        apply_config_transforms(d, from_version).map_err(CommitError::Refused)?;
        match gate(&d.to_string()) {
            Ok(_) => Ok(EditOutcome::Modified),
            Err(_) => Err(CommitError::Revalidation),
        }
    })
    .map_err(|e| {
        if overlay_written {
            CommitFailure {
                error: Box::new(resumable_commit_error(&e)),
                outcome: "incomplete",
            }
        } else {
            CommitFailure {
                outcome: match &e {
                    ConfigWriteError::Conflict { .. } => "conflict",
                    _ => "write_failed",
                },
                error: Box::new(render_write_error(e)),
            }
        }
    })?;

    Ok(())
}

/// Commit the pending overlay fold through [`with_overlay_write_lock`],
/// preserving the plan-time revision check: the closure runs under the
/// advisory write lock (closing the load->save race a bare `save` leaves
/// open) and refuses with a [`OverlayError::RevisionConflict`] if the on-disk
/// revision moved since the plan computed the merge against `base_revision`.
/// On a matching revision it persists the merged cells; the lock is released
/// on return either way.
fn commit_overlay(
    overlay_path: &Path,
    overlay: OverlayWrite,
) -> std::result::Result<(), OverlayError> {
    with_overlay_write_lock::<OverlayError, _>(overlay_path, |loaded| {
        if loaded.revision != overlay.base_revision {
            return Err(OverlayError::RevisionConflict {
                expected: overlay.base_revision,
                actual: loaded.revision,
            });
        }
        Ok(CatalogOverlay {
            cells: overlay.cells,
            ..loaded
        })
    })
    .map(|_| ())
}

/// A config-commit failure that lands AFTER the overlay was already written.
/// Phrased so it never claims "nothing was written" -- the overlay change is
/// durable and the migration is resumable by a rerun.
fn resumable_commit_error(e: &ConfigWriteError<CommitError>) -> Error {
    let reason = match e {
        ConfigWriteError::Conflict { .. } => {
            "config.toml changed on disk before it could be stamped".to_string()
        }
        other => other.to_string(),
    };
    Error::Config(format!(
        "the catalog overlay was migrated, but config.toml was not committed ({reason}); the \
         overlay change is durable -- rerun `config migrate` to finish stamping config.toml"
    ))
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
        Refusal::EgressAllowlist { .. } => "egress_allowlist",
    }
}

fn render_write_error(err: ConfigWriteError<CommitError>) -> Error {
    Error::Config(err.to_string())
}

fn render_dry_run(candidate_text: &str, from_version: u32, to_version: u32, removed: &[String]) {
    println!("--- candidate config.toml (version {to_version}) ---");
    print!("{candidate_text}");
    if !candidate_text.ends_with('\n') {
        println!();
    }
    println!("--- end candidate ---");
    if from_version == to_version {
        println!("summary: normalizes config at version {to_version} (no version bump)");
    } else {
        println!("summary: migrates config from version {from_version} to {to_version}");
    }
    if removed.is_empty() {
        println!("  (no keys removed; version stamp only)");
    } else {
        for key in removed {
            println!("  - removes `{key}`");
        }
    }
    println!("dry-run: nothing was written.");
}

/// Acknowledge the schema change before the write lock. `--force` bypasses the
/// prompt; a non-interactive run without `--force` reads EOF and refuses.
/// Never called while the write lock is held. Called for EVERY real write,
/// including a same-version v3 normalization (`from_version == to_version`).
fn confirm_migration(from_version: u32, to_version: u32, force: bool) -> bool {
    if force {
        return true;
    }
    use std::io::Write as _;
    if from_version == to_version {
        println!(
            "this normalizes config.toml at version {to_version}, folding legacy \
             `unsupported_features` lists into `[capability.overrides]` and removing the retired \
             keys. A running routectl daemon must be restarted onto the matching binary afterward."
        );
    } else {
        println!(
            "this migrates config.toml from version {from_version} to {to_version}. The break \
             retires per-status retry lists (and, from a v1 file, the `[cache_pricing]` table). A \
             running routectl daemon must be restarted onto the matching binary after migration."
        );
    }
    print!("proceed? [y/N] ");
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
            apply_config_transforms(d, 2).map_err(CommitError::Refused)?;
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

    // -----------------------------------------------------------------
    // Same-version v3 normalization: legacy unsupported_features fold into
    // [capability.overrides]; egress allowlists and conflicts refuse.
    // -----------------------------------------------------------------

    /// A v3 config carrying legacy provider AND model `unsupported_features`
    /// plus a valid provider/model/alias so the folded result passes the gate.
    const V3_WITH_LEGACY: &str = "\
# operator note: keep me
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"
unsupported_features = [\"web_search\"]

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"
unsupported_features = [\"computer_use\"]

[aliases]
default = \"gpt\"
";

    /// A plain v3 config with no legacy fields at all.
    const V3_CLEAN: &str = "\
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

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

    #[test]
    fn v3_legacy_lists_normalize_into_capability_overrides_and_keys_removed() {
        let f = fixture(V3_WITH_LEGACY);
        let result = run_at(&f.config, &f.overlay, false, true).expect("normalize");
        assert_eq!(result, MigrateResult::Migrated { from_version: 3 });

        let text = read(&f.config);
        assert!(!text.contains("unsupported_features"), "{text}");
        assert!(text.contains("version = 3"), "{text}");
        assert!(text.contains("[capability.overrides.fast]"), "{text}");
        assert!(
            text.contains("[capability.overrides.\"fast:gpt\"]"),
            "{text}"
        );
        assert!(text.contains("# operator note: keep me"), "{text}");
        // The committed file re-validates and loads with no legacy keys left.
        gate(&text).expect("normalized config must pass the gate");
    }

    #[test]
    fn v3_no_legacy_fields_is_already_current_and_writes_nothing() {
        let f = fixture(V3_CLEAN);
        let before = std::fs::read(&f.config).unwrap();

        let result = run_at(&f.config, &f.overlay, false, true).expect("already current");
        assert_eq!(result, MigrateResult::AlreadyCurrent);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a plain v3 file must not be rewritten"
        );
    }

    #[test]
    fn v3_egress_allowlist_refuses_byte_identical() {
        let body = V3_WITH_LEGACY.replace(
            "[server]\n",
            "[bedrock]\nallowed_betas = [\"beta-1\"]\n\n[server]\n",
        );
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        let err = run_at(&f.config, &f.overlay, false, true).expect_err("egress allowlist refuses");
        assert!(err.to_string().contains("allowed_betas"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a refused normalization must leave the file byte-identical"
        );
    }

    #[test]
    fn v3_egress_allowlist_refusal_audit_names_the_kind() {
        let body = V3_WITH_LEGACY.replace(
            "[server]\n",
            "[bedrock]\nallowed_betas = [\"beta-1\"]\n\n[server]\n",
        );
        let f = fixture(&body);
        let events = routectl_testkit::capture_events(|| {
            let _ = run_at(&f.config, &f.overlay, false, true);
        });
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("refused"));
        assert_eq!(audit.field("refusal_kind"), Some("egress_allowlist"));
    }

    #[test]
    fn v3_conflicting_cell_refuses_via_the_gate_byte_identical() {
        // Legacy provider list routes `web_search` away while a new
        // force_supported entry marks the SAME cell supported: after folding
        // the legacy list into `unsupported`, the shared gate's conflict
        // check rejects, and the file stays byte-identical.
        let body = V3_WITH_LEGACY.replace(
            "[aliases]\n",
            "[capability.overrides.fast]\nforce_supported = [\"web_search\"]\n\n[aliases]\n",
        );
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        let err = run_at(&f.config, &f.overlay, false, true).expect_err("conflict must refuse");
        assert!(err.to_string().contains("config error"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a conflicting normalization must leave the file byte-identical"
        );
    }

    #[test]
    fn v3_normalize_dry_run_renders_candidate_and_writes_nothing() {
        let f = fixture(V3_WITH_LEGACY);
        let before = std::fs::read(&f.config).unwrap();

        let result = run_at(&f.config, &f.overlay, true, false).expect("dry-run");
        assert_eq!(result, MigrateResult::DryRun);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "dry-run must not write"
        );
    }

    // -----------------------------------------------------------------
    // f02: a v1 file that hits a v2->v3 refusal DURING planning leaves BOTH
    // config.toml AND the overlay byte-untouched. The old impure ladder wrote
    // the overlay and stamped config.toml to v2 BEFORE the refusal, then
    // printed a false "nothing was written"; the pure planner refuses first.
    // -----------------------------------------------------------------

    /// A legacy v1 config carrying both a `[cache_pricing]` table AND a
    /// behavior-bearing `retry_allowlist`, so the ladder's v2->v3 rung refuses.
    const V1_CACHE_PRICING_AND_BEHAVIOR_BEARING: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[cache_pricing]
\"openai-compat:grok-*\" = { wm = 1.5, override_acknowledges_cost_risk = true }

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

    #[test]
    fn v1_refusal_leaves_config_and_overlay_byte_untouched() {
        let f = fixture(V1_CACHE_PRICING_AND_BEHAVIOR_BEARING);
        let before = std::fs::read(&f.config).unwrap();

        let err = run_at(&f.config, &f.overlay, false, true)
            .expect_err("a v1 file with a behavior-bearing list must refuse");
        assert!(err.to_string().contains("503"), "err: {err}");
        // Both files untouched -- the overlay was never even created, and the
        // config was never stamped to v2.
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a refused v1 migration must leave config.toml byte-identical"
        );
        assert!(
            !f.overlay.exists(),
            "a refused v1 migration must not fold the overlay"
        );
    }

    #[test]
    fn v1_refusal_audit_never_reports_written_and_names_the_kind() {
        let f = fixture(V1_CACHE_PRICING_AND_BEHAVIOR_BEARING);
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

    // -----------------------------------------------------------------
    // f20: a same-version v3 normalization is a REAL write and must be
    // prompt/force-gated like any other, and its audit must reflect the true
    // acknowledgement (never a synthesized acknowledged=true).
    // -----------------------------------------------------------------

    #[test]
    fn v3_normalize_non_interactive_without_force_aborts_byte_identical() {
        // stdin is not a TTY under the test harness: read_line hits EOF, so
        // the normalize prompt is declined and nothing is written.
        let f = fixture(V3_WITH_LEGACY);
        let before = std::fs::read(&f.config).unwrap();

        let result =
            run_at(&f.config, &f.overlay, false, false).expect("declining is not an error");
        assert_eq!(result, MigrateResult::Aborted);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "an unacknowledged v3 normalization must not write"
        );
    }

    #[test]
    fn v3_normalize_forced_audit_records_acknowledged_false_not_synthesized() {
        // A forced normalize was authorized by --force, NOT by an interactive
        // acknowledgement, so `acknowledged` must be false -- the f20 defect
        // was a synthesized acknowledged=true on this exact path.
        let f = fixture(V3_WITH_LEGACY);
        let events = routectl_testkit::capture_events(|| {
            run_at(&f.config, &f.overlay, false, true).expect("normalize");
        });
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("written"));
        assert_eq!(audit.field("forced"), Some("true"));
        assert_eq!(
            audit.field("acknowledged"),
            Some("false"),
            "a --force normalize must not synthesize acknowledged=true"
        );
    }

    // -----------------------------------------------------------------
    // The audit distinguishes aborted / refused / dry_run / written.
    // -----------------------------------------------------------------

    #[test]
    fn aborted_audit_event_names_aborted() {
        let f = fixture(V2_CLEAN);
        let events = routectl_testkit::capture_events(|| {
            // Non-interactive without --force declines at the prompt.
            let _ = run_at(&f.config, &f.overlay, false, false);
        });
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("aborted"));
    }

    #[test]
    fn dry_run_audit_event_names_dry_run() {
        let f = fixture(V2_CLEAN);
        let events = routectl_testkit::capture_events(|| {
            run_at(&f.config, &f.overlay, true, false).expect("dry-run");
        });
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("dry_run"));
        assert_eq!(audit.field("dry_run"), Some("true"));
    }

    // -----------------------------------------------------------------
    // A partial-commit state (overlay written, config still old) reruns
    // safely to a consistent result without a double overlay write.
    // -----------------------------------------------------------------

    #[test]
    fn partial_commit_state_reruns_safely_to_completion() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let v1_body = std::fs::read(&f.config).unwrap();

        // First run completes both phases (overlay folded, config -> v3).
        run_at(&f.config, &f.overlay, false, true).expect("first migrate");
        assert!(f.overlay.exists(), "first run folds the overlay");
        let overlay_after_first = std::fs::read(&f.overlay).unwrap();

        // Simulate a crash between the overlay commit and the config stamp:
        // config.toml is rolled back to its original v1 content while the
        // overlay's write is durable.
        std::fs::write(&f.config, &v1_body).unwrap();

        // Rerun completes safely: the overlay fold is now an idempotent no-op
        // (no double write) and config.toml is stamped forward to v3.
        let result = run_at(&f.config, &f.overlay, false, true).expect("rerun");
        assert_eq!(result, MigrateResult::Migrated { from_version: 1 });
        let text = read(&f.config);
        assert!(text.contains("version = 3"), "{text}");
        assert!(!text.contains("cache_pricing"), "{text}");
        gate(&text).expect("the completed config must pass the gate");
        assert_eq!(
            std::fs::read(&f.overlay).unwrap(),
            overlay_after_first,
            "the rerun must not write the overlay a second time"
        );
    }

    // -----------------------------------------------------------------
    // The overlay commit runs under the overlay write lock and keeps the
    // plan-time revision check: a concurrent writer that advanced the
    // on-disk revision between the plan and the commit is NOT silently
    // overwritten -- the commit conflicts and leaves the file byte-intact.
    // -----------------------------------------------------------------

    #[test]
    fn overlay_commit_refuses_a_stale_base_revision_without_clobbering() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        // Seed the overlay so its on-disk revision is 1 -- ahead of a plan
        // computed against revision 0 (a concurrent `catalog` write landed in
        // between the plan and the commit).
        routectl_router::save_catalog_overlay(&f.overlay, 0, BTreeMap::new())
            .expect("seed overlay at revision 1");
        let before = std::fs::read(&f.overlay).unwrap();

        let stale = OverlayWrite {
            base_revision: 0,
            cells: BTreeMap::new(),
        };
        let err = commit_overlay(&f.overlay, stale)
            .expect_err("a stale base_revision must conflict under the lock");
        assert!(
            matches!(
                err,
                OverlayError::RevisionConflict {
                    expected: 0,
                    actual: 1
                }
            ),
            "err: {err}"
        );
        assert_eq!(
            std::fs::read(&f.overlay).unwrap(),
            before,
            "a conflict must leave the concurrent write in place, not clobber it"
        );
    }

    // -----------------------------------------------------------------
    // A LIVE mid-commit failure: the overlay phase lands, then config.toml
    // is raced so the base-bytes revision check conflicts at the config
    // phase. The audit outcome is `incomplete` (never "nothing written"),
    // the error is the resumable message, and the overlay stays committed.
    // -----------------------------------------------------------------

    #[test]
    fn live_mid_commit_config_conflict_lands_overlay_and_reports_incomplete() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let snapshot = std::fs::read(&f.config).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();

        // Plan against the pristine v1 snapshot: a new `[cache_pricing]` cell
        // makes this a ConfigAndOverlay plan (overlay first, config last).
        let doc = parse_document(&snapshot_text).expect("parse");
        let cache_pricing =
            load_v1_cache_pricing(&snapshot_text, &f.config).expect("cache pricing");
        let plan = plan_migration(&doc, 1, &cache_pricing, &f.overlay).expect("plan");
        assert!(matches!(plan.write_kind, WriteKind::ConfigAndOverlay(..)));

        // Race config.toml: a concurrent writer rewrites it AFTER planning, so
        // the base-bytes check fails at the config phase -- but only after the
        // overlay phase has already landed.
        let concurrent = snapshot_text.replacen("port = 8787", "port = 8788", 1);
        std::fs::write(&f.config, &concurrent).unwrap();

        // `run_at` feeds `failure.outcome` verbatim to the audit event, so the
        // asserted outcome IS the audited outcome.
        let failure = commit_plan(plan, &f.config, &f.overlay, &snapshot, 1)
            .expect_err("a config-phase conflict after the overlay lands must fail");
        assert_eq!(failure.outcome, "incomplete");
        assert!(
            failure.error.to_string().contains("rerun `config migrate`"),
            "the resumable message must never claim nothing was written, got: {}",
            failure.error
        );

        // The overlay is left as committed (durable) ...
        let overlay = routectl_router::load_catalog_overlay(&f.overlay).expect("overlay loads");
        assert!(
            overlay.cells.contains_key("openai-compat:grok-*"),
            "the overlay fold must remain committed after the config conflict"
        );
        // ... and the concurrent config write was not clobbered.
        assert_eq!(
            std::fs::read_to_string(&f.config).unwrap(),
            concurrent,
            "a config conflict must leave the concurrent write in place"
        );
    }

    // -----------------------------------------------------------------
    // The SAME live mid-commit conflict, driven end-to-end through the
    // PUBLIC `run_at` entry point rather than `commit_plan` directly, so
    // the audit event `run_at` emits is asserted (not just the failure it
    // hands the caller). A background racer holds config.toml's advisory
    // write lock -- the very lock `edit_config_toml` acquires -- so
    // `run_at`'s config-phase re-read blocks until the file is raced out
    // from under it. Ordering is deterministic with no timing guesses:
    //   * `run_at` reads its snapshot, then folds the overlay, then reaches
    //     the (locked) config write -- so the overlay fold appearing on disk
    //     is a happens-before edge proving the snapshot was already captured
    //     pristine; the racer waits for that edge before touching config.toml.
    //   * the racer writes config.toml while STILL holding the lock and only
    //     then releases, so `run_at`'s re-read (which must first acquire the
    //     lock) observes the raced bytes and conflicts.
    // -----------------------------------------------------------------

    #[test]
    fn run_at_config_conflict_after_overlay_audits_incomplete_and_resumes() {
        use std::sync::mpsc;

        let f = fixture(V1_WITH_CACHE_PRICING);
        let pristine = std::fs::read(&f.config).unwrap();
        // A byte-distinct but still-valid rewrite: it re-parses fine, yet the
        // config writer's byte-for-byte base check treats it as a conflict.
        let raced = String::from_utf8(pristine.clone())
            .unwrap()
            .replacen("port = 8787", "port = 8788", 1)
            .into_bytes();

        // The config writer locks a `<path>.lock` sibling; mirror that path.
        let mut lock_name = f.config.clone().into_os_string();
        lock_name.push(".lock");
        let lock_path = std::path::PathBuf::from(lock_name);

        let overlay_path = f.overlay.clone();
        let config_path = f.config.clone();
        let raced_bytes = raced.clone();
        let (locked_tx, locked_rx) = mpsc::channel();

        let racer = std::thread::spawn(move || {
            let lock_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .expect("open config lock file");
            let mut rw = fd_lock::RwLock::new(lock_file);
            let _guard = rw.write().expect("hold config write lock");

            // Lock held: `run_at` may start. It cannot pass the config phase.
            locked_tx.send(()).expect("signal lock held");

            // Wait for the overlay fold to land -- the happens-before edge that
            // proves `run_at` already read its (pristine) snapshot.
            let mut folded = false;
            for _ in 0..500 {
                if routectl_router::load_catalog_overlay(&overlay_path)
                    .is_ok_and(|overlay| overlay.cells.contains_key("openai-compat:grok-*"))
                {
                    folded = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(folded, "the overlay fold must land before the config phase");

            std::fs::write(&config_path, &raced_bytes).expect("race config.toml");
            // `_guard` drops here, releasing the lock so `run_at`'s config
            // re-read acquires it, sees the raced bytes, and conflicts.
        });

        locked_rx.recv().expect("racer acquired the config lock");

        let mut outcome = None;
        let events = routectl_testkit::capture_events(|| {
            outcome = Some(run_at(&f.config, &f.overlay, false, true));
        });
        racer.join().expect("racer thread");

        // (b) the user-facing error is the resumable message, never a false
        // "nothing was written".
        let err = outcome
            .expect("run_at ran")
            .expect_err("a config conflict after the overlay lands must fail");
        assert!(
            err.to_string().contains("rerun `config migrate`"),
            "the resumable message must never claim nothing was written, got: {err}"
        );

        // (a) the AUDIT event `run_at` emitted records `incomplete`.
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("incomplete"));

        // (c) the overlay retains the committed fold ...
        let overlay = routectl_router::load_catalog_overlay(&f.overlay).expect("overlay loads");
        assert!(
            overlay.cells.contains_key("openai-compat:grok-*"),
            "the overlay fold must remain committed after the config conflict"
        );
        // ... and the racing writer's config bytes were not clobbered.
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            raced,
            "a config conflict must leave the concurrent write in place"
        );

        // (d) a rerun completes: the overlay fold is now an idempotent no-op
        // and config.toml is stamped forward to v3.
        let result = run_at(&f.config, &f.overlay, false, true).expect("rerun completes");
        assert_eq!(result, MigrateResult::Migrated { from_version: 1 });
        let text = read(&f.config);
        assert!(text.contains("version = 3"), "{text}");
        assert!(!text.contains("cache_pricing"), "{text}");
        gate(&text).expect("the completed config must pass the gate");
    }

    // -----------------------------------------------------------------
    // A config-phase conflict with NO overlay write (a ConfigOnly plan)
    // audits `conflict` -- labelled by the ConfigWriteError variant, not
    // the old hardcoded value.
    // -----------------------------------------------------------------

    #[test]
    fn config_phase_conflict_without_overlay_reports_conflict() {
        let f = fixture(V2_CLEAN);
        let snapshot = std::fs::read(&f.config).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();
        let doc = parse_document(&snapshot_text).expect("parse");
        let plan = plan_migration(&doc, 2, &BTreeMap::new(), &f.overlay).expect("plan");
        assert!(matches!(plan.write_kind, WriteKind::ConfigOnly(_)));

        std::fs::write(
            &f.config,
            snapshot_text.replacen("port = 8787", "port = 8788", 1),
        )
        .unwrap();

        let failure = commit_plan(plan, &f.config, &f.overlay, &snapshot, 2)
            .expect_err("a stale snapshot must conflict at the config phase");
        assert_eq!(failure.outcome, "conflict");
        assert!(
            !f.overlay.exists(),
            "a ConfigOnly plan must not write the overlay"
        );
    }
}
