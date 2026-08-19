//! `routectl config migrate` -- bring a legacy `config.toml` forward to the
//! current schema version through the shared migration ladder, committing the
//! result through the same single write primitive as `config set`.
//!
//! The pipeline is check-before-write end to end. The ladder runs as a PURE
//! planning phase ([`plan_migration`]) that produces a `MigrationPlan` whose
//! `write_kind` (`NoChange` / `ConfigOnly(text)` / `ConfigAndOverlay(text,
//! overlay)`) folds the config candidate and the pending overlay write INTO
//! the variant that needs them, so no cross-field invariant is left for the
//! caller to `.expect()`. A SECOND, store-aware phase composes on top of it:
//! the pure ladder cannot read the credential store (it runs twice -- plan
//! and locked re-read -- and the committed bytes must reproduce what
//! planning gated), so materializing one explicit account entry per stored
//! seat lives here instead. Both phases plan without touching disk, so every
//! refusal clears before any mutation:
//!
//!   1. Snapshot the raw bytes and read the file's raw `version`.
//!   2. Plan the pure ladder. A [`Refusal`] (behavior-bearing / malformed
//!      retry lists, an egress allowlist, an unrelocatable `seat_selection`)
//!      or a future-version file surfaces here with an explicit "nothing was
//!      written" -- nothing has been.
//!   3. Enumerate stored OAuth seats READ-ONLY and plan phase 2 against the
//!      pure candidate: for each provider entry whose BARE `oauth://` ref
//!      covered more than one stored seat, the naming module derives the
//!      account entries and the pool that replace it. An unreadable store, a
//!      name collision, two labels generating one name, two entries sharing
//!      one OAuth family, or a derived pool name held by a pool the migration
//!      did not create refuses the whole migration -- a v4 file whose bare ref
//!      silently stopped covering its sibling seats is structurally excluded,
//!      and so is one that merges accounts the operator kept on separate
//!      egresses.
//!   4. Gate the COMBINED candidate (phase 1 + phase 2) through the shared
//!      `parse_config` + `validation_report` suite; a gate failure renders
//!      the report and writes nothing.
//!   5. `--dry-run` renders the exact combined candidate plus a change
//!      summary and stops -- it needs no acknowledgement (nothing is written)
//!      and no temp copy (planning never touched the real files).
//!   6. ONE acknowledgement for the combined change: interactive `y`, or
//!      `--yes` non-interactively; a non-interactive run without `--yes`
//!      refuses. It runs AFTER the gate (only a valid migration is worth
//!      prompting for) and BEFORE any write. Declining writes NOTHING.
//!   7. Commit as a unit: the overlay FIRST (revision-checked, idempotent),
//!      then `config.toml` LAST via [`edit_config_toml`] as the visible
//!      completion marker (base-bytes revision check -> conflict = no write).
//!      The seats are RE-ENUMERATED immediately before the locked write and
//!      phase 2 re-planned against them, so a login or logout between the
//!      shown diff and the commit refuses instead of landing a stale diff. A
//!      crash between the phases is recoverable: a rerun re-plans (the
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use routectl_auth::oauth::OAuthStore;
use routectl_core::{Error, Result};
use routectl_router::{
    BareOauthRef, CURRENT_CONFIG_VERSION, CachePricingOverride, CatalogOverlay, Config,
    ConfigWriteError, EditOutcome, MigrateError, MigrationPlan, OverlayError, OverlayWrite,
    Refusal, SeatPoolAccount, SeatPoolMove, WriteKind, apply_config_transforms,
    apply_seat_pool_move, bare_oauth_pool_candidates, edit_config_toml, models_routed_at,
    parse_config, plan_migration, with_overlay_write_lock,
};
use toml_edit::DocumentMut;

use super::config::validation_report;
use super::doctor::sanitize_store_open_error;
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
/// the pure transforms and the shared gate once against the same content, so
/// the first two variants are belt-and-suspenders guards a deterministic
/// re-run never reaches. `InventoryChanged` is different in kind: it closes a
/// real window, between phase 2's plan-time store read and the locked write.
#[derive(Debug, thiserror::Error)]
enum CommitError {
    #[error("migration refused under the write lock:\n{0}")]
    Refused(Refusal),
    #[error("migrated config failed re-validation under the write lock")]
    Revalidation,
    #[error(
        "the stored OAuth seats changed between planning this migration and committing it, so \
         the shown diff no longer describes the seats on disk; nothing was written -- rerun \
         `config migrate` to plan against the current seats"
    )]
    InventoryChanged,
}

/// Run the migrate pipeline against the default config + overlay +
/// credentials paths.
pub async fn run(config_path: &Path, dry_run: bool, yes: bool) -> Result<MigrateResult> {
    // A config dir that cannot be resolved is the store-unreadable refusal:
    // phase 2 cannot enumerate seats, and a version-stamp-only migration
    // would silently narrow every bare ref to the default seat.
    let credentials_path = routectl_auth::oauth::credentials_default_path()
        .map_err(|e| Error::Config(seat_enumeration_refusal(&sanitize_store_open_error(&e))))?;
    run_at(
        config_path,
        &routectl_router::overlay_default_path(),
        &credentials_path,
        dry_run,
        yes,
    )
    .await
}

/// The stored-seat inventory phase 2 plans against: one label list per
/// provider family, `None` standing for the family's default seat.
///
/// A `SeatInventory` is only ever built from a SUCCESSFUL read-only store
/// open, so its presence is itself the proof that phase 2 is allowed to
/// proceed -- an unreadable store refuses before one exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SeatInventory {
    /// Seat labels per family, sorted, `None` first when the default seat
    /// is stored. Sorted rather than store order so the same store yields
    /// the same generated names on every run.
    seats: BTreeMap<String, Vec<Option<String>>>,
}

impl SeatInventory {
    /// The labels stored for `family`, or an empty slice.
    fn labels_of(&self, family: &str) -> &[Option<String>] {
        self.seats.get(family).map_or(&[], Vec::as_slice)
    }
}

/// Enumerate the credential store's seats READ-ONLY, keyed by provider
/// family.
///
/// Opened through [`OAuthStore`] directly, the way every read-only seat
/// reader does: the composite store's `list_seats` echoes a pinned ref back
/// as a one-element list when its oauth arm is absent, which would report a
/// confident single seat for exactly the unreadable-store case phase 2 must
/// refuse on. A merely-absent credentials file opens as an EMPTY store, so
/// "nothing logged in" stays distinguishable from "cannot tell".
///
/// # Errors
///
/// A store OPEN failure, rendered through the shared path-free sanitizer --
/// never a raw store error, whose Display embeds the credentials-file path.
async fn enumerate_seats(credentials_path: &Path) -> Result<SeatInventory> {
    let store = OAuthStore::open(credentials_path)
        .await
        .map_err(|e| Error::Config(seat_enumeration_refusal(&sanitize_store_open_error(&e))))?;
    let mut seats: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    for (key, _) in store.list().await {
        let (family, label) = match key.split_once('#') {
            None => (key.clone(), None),
            Some((family, label)) => (family.to_string(), Some(label.to_string())),
        };
        seats.entry(family).or_default().push(label);
    }
    for labels in seats.values_mut() {
        labels.sort();
        labels.dedup();
    }
    Ok(SeatInventory { seats })
}

/// The refusal message for a phase-2 store read that could not happen.
/// Phase 2 cannot tell a single-seat family from a multi-seat one without
/// the store, and a v4 file whose bare ref silently stops covering the
/// sibling seats it used to reach is exactly the silent seat loss the
/// combined migration exists to prevent -- so an unreadable store refuses
/// the WHOLE migration rather than migrating the version stamp alone.
fn seat_enumeration_refusal(reason: &str) -> String {
    format!(
        "cannot enumerate stored OAuth seats, so this migration cannot tell which \
         `oauth://` refs stand for more than one seat: {reason}. A version-stamp-only \
         migration would silently narrow those refs to the default seat. Nothing was \
         written -- fix the credential store (or run `routectl login`) and rerun"
    )
}

/// Plan phase 2: one [`SeatPoolMove`] per provider entry whose BARE
/// `oauth://` ref covered more than one stored seat under v3 semantics.
///
/// A single-seat family is a structural NO-OP by construction, not by a
/// special case: at v4 a bare ref means the default seat, which for a
/// one-seat family is the same credential the v3 ref resolved to, so there
/// is nothing to materialize and no pool to create.
///
/// Generated names come from `seat_naming::plan_pool_materialization`
/// against the candidate config, byte-for-byte the names the login writer
/// will later generate. `already_present` / `pool_exists` make a rerun over
/// the migration's own output a no-op.
///
/// `preexisting_pools` names the `[pools.*]` blocks the file carried BEFORE
/// the migration planned anything, so a pool this migration is about to
/// create is distinguishable from one it would grow.
///
/// # Errors
///
/// A [`SeatMaterializationRefusal`] -- two bare refs on one provider family,
/// or a derived pool name held by a pool the migration did not create -- or a
/// `SeatNamingError` from the naming module, surfaced verbatim: a generated
/// name held by an unrelated entry, two labels generating one name, an
/// unusable label token, the reserved `default` label, or a pool name held by
/// a provider entry or a model nickname. Never softened -- a lossy rewrite
/// would point a config entry at the wrong credential.
fn plan_seat_materialization(
    candidate: &Config,
    doc: &DocumentMut,
    inventory: &SeatInventory,
    preexisting_pools: &BTreeSet<String>,
) -> Result<Vec<SeatPoolMove>> {
    let candidates = bare_oauth_pool_candidates(doc);
    let mut moves = Vec::new();
    for BareOauthRef { entry, family } in candidates.clone() {
        let labels = inventory.labels_of(&family);
        if labels.len() <= 1 {
            continue;
        }
        let siblings = entries_of_family(&candidates, &family);
        if siblings.len() > 1 {
            return Err(Error::Config(
                SeatMaterializationRefusal::FamilyFanOut {
                    family,
                    entries: siblings,
                }
                .to_string(),
            ));
        }
        let plan = routectl_router::seat_naming::plan_pool_materialization(
            candidate,
            &family,
            labels.iter().map(Option::as_deref),
        )
        .map_err(|e| {
            Error::Config(format!(
                "cannot materialize the stored seats of `oauth://{family}` (referenced by \
                 [providers.{entry}]) as explicit accounts: {e}. Nothing was written"
            ))
        })?;
        // The entry itself IS the default-seat member and keeps its name, so
        // only the labelled seats materialize as new entries.
        let accounts: Vec<SeatPoolAccount> = plan
            .accounts
            .into_iter()
            .filter(|account| account.label.is_some())
            .map(|account| SeatPoolAccount {
                entry_name: account.entry_name,
                secret_ref: account.secret_ref,
                already_present: account.already_present,
            })
            .collect();
        // A move the config already satisfies is dropped, not planned: with
        // every account present and the pool listing them, applying it would
        // change nothing, and carrying it would make a rerun report a pending
        // write that does not exist.
        let satisfied = plan.pool_exists
            && accounts.iter().all(|a| a.already_present)
            && candidate.pools.get(&plan.pool_name).is_some_and(|pool| {
                pool.members.contains(&entry)
                    && accounts
                        .iter()
                        .all(|a| pool.members.contains(&a.entry_name))
            });
        if satisfied {
            continue;
        }
        // A pool the FILE already carried is not this migration's to grow: its
        // membership is an operator statement about which accounts share an
        // egress, and phase 2 cannot tell an intentionally pinned set from an
        // incomplete one.
        if preexisting_pools.contains(&plan.pool_name) {
            return Err(Error::Config(
                SeatMaterializationRefusal::ExistingPool {
                    pool: plan.pool_name,
                    entry,
                    family,
                }
                .to_string(),
            ));
        }
        moves.push(SeatPoolMove {
            entry,
            pool: plan.pool_name,
            accounts,
        });
    }
    Ok(moves)
}

/// The `[pools.*]` block names a document carries, for the
/// [`SeatMaterializationRefusal::ExistingPool`] check.
fn pool_block_names(doc: &DocumentMut) -> BTreeSet<String> {
    doc.get("pools")
        .and_then(toml_edit::Item::as_table_like)
        .map(|pools| pools.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default()
}

/// The bare-`oauth://` provider entries naming one provider family, sorted.
fn entries_of_family(candidates: &[BareOauthRef], family: &str) -> Vec<String> {
    let mut entries: Vec<String> = candidates
        .iter()
        .filter(|c| c.family == family)
        .map(|c| c.entry.clone())
        .collect();
    entries.sort();
    entries.dedup();
    entries
}

/// A phase-2 refusal class: the migration would have to guess which accounts
/// share one pool, and a wrong guess dispatches an account's OAuth bearer to
/// an egress the operator never paired it with.
///
/// Fail-closed like every sibling class: the caller writes nothing and the
/// file stays byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SeatMaterializationRefusal {
    /// Two or more provider entries carry a BARE `oauth://<family>` ref for
    /// the same family. Materializing them would fold entries that may name
    /// deliberately distinct egresses into ONE pool and repoint every model
    /// naming either entry at it.
    FamilyFanOut {
        /// The provider family both refs name.
        family: String,
        /// The provider entry names carrying the bare ref, sorted.
        entries: Vec<String>,
    },
    /// The derived pool name is held by a `[pools.*]` block the file already
    /// carried, so materializing would grow an operator-authored pool.
    ExistingPool {
        /// The pool block already present.
        pool: String,
        /// The provider entry whose seats would have been materialized.
        entry: String,
        /// The provider family the entry's bare ref names.
        family: String,
    },
}

impl std::fmt::Display for SeatMaterializationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FamilyFanOut { family, entries } => write!(
                f,
                "{} provider entries ({}) each carry a bare `oauth://{family}` ref, and \
                 `oauth://{family}` stands for more than one stored seat. Materializing them \
                 would group entries that may point at different egresses into one \
                 `[pools.{family}]` and dispatch every account's credential to all of them, \
                 so this migration has no single answer. Nothing was written -- pin all but \
                 one of those entries to a specific seat with `oauth://{family}#<label>`, or \
                 write the `[pools.<name>]` blocks for this provider by hand, then rerun",
                entries.len(),
                entries
                    .iter()
                    .map(|e| format!("[providers.{e}]"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Self::ExistingPool {
                pool,
                entry,
                family,
            } => write!(
                f,
                "a `[pools.{pool}]` block already exists, so materializing the stored seats of \
                 `oauth://{family}` (referenced by [providers.{entry}]) would add members to a \
                 pool this migration did not create -- its membership is your statement about \
                 which accounts share an egress. Nothing was written -- add the account entries \
                 to that pool by hand (or rename it), then rerun"
            ),
        }
    }
}

/// Apply every phase-2 move to `doc`, in plan order.
fn apply_seat_materialization(doc: &mut DocumentMut, moves: &[SeatPoolMove]) {
    for mv in moves {
        apply_seat_pool_move(doc, mv);
    }
}

/// Core of [`run`], taking the overlay and credentials paths explicitly so
/// tests point the config, the overlay AND the credential store at a temp
/// directory instead of the real files.
pub async fn run_at(
    config_path: &Path,
    overlay_path: &Path,
    credentials_path: &Path,
    dry_run: bool,
    yes: bool,
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

    // Phase 2, store-aware, composed ON TOP of the pure plan: the seat
    // inventory decides which bare refs stood for more than one seat. It runs
    // for every plan shape, including a `NoChange` one -- a file already at
    // the current version can still carry a bare multi-seat ref if it was
    // hand-authored -- and, like phase 1, writes nothing.
    let inventory = enumerate_seats(credentials_path).await?;
    let phase_two = plan_phase_two(&plan, &doc, from_version, &inventory)?;

    // Nothing to do only when BOTH phases are empty.
    let Some(candidate_text) = combined_candidate(&plan, &doc, &phase_two)? else {
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
    };

    // Validation gate on the COMBINED candidate -- still before any write.
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

    // `--dry-run` renders the combined candidate and stops -- no write, no ack.
    if dry_run {
        render_dry_run(
            &candidate_text,
            from_version,
            to_version,
            &plan.removed_keys,
            &doc,
            &phase_two,
        );
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

    // ONE acknowledgement for the combined change, now that the candidate is
    // known valid and a write is known to be pending. A declined prompt
    // leaves both files byte-identical.
    if !confirm_migration(from_version, to_version, &doc, &phase_two, yes) {
        println!("aborted; nothing further written.");
        audit_event(
            config_path,
            from_version,
            to_version,
            false,
            false,
            yes,
            "aborted",
            None,
        );
        return Ok(MigrateResult::Aborted);
    }

    commit_plan(
        plan,
        &phase_two,
        config_path,
        overlay_path,
        credentials_path,
        &snapshot,
        from_version,
    )
    .await
    .map_err(|failure| {
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
            !yes,
            yes,
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
        !yes,
        yes,
        "written",
        None,
    );
    render_success(from_version, to_version, &doc, &phase_two);
    Ok(MigrateResult::Migrated { from_version })
}

/// Plan phase 2 against the config the PURE plan produced (or against the
/// file as it stands, for a `NoChange` plan): the naming module checks
/// generated names against a real `Config`, so the candidate it sees must be
/// the one phase 1 would leave behind.
///
/// The pre-existing pool set is read off `original`, NOT off the transformed
/// document: phase 1's own `seat_selection` relocation creates a pool block
/// that phase 2 is expected to grow, while a pool the FILE carried is one the
/// migration must refuse to touch.
fn plan_phase_two(
    plan: &MigrationPlan,
    original: &DocumentMut,
    from_version: u32,
    inventory: &SeatInventory,
) -> Result<Vec<SeatPoolMove>> {
    let preexisting_pools = pool_block_names(original);
    let mut doc = original.clone();
    if plan.config_candidate().is_none() {
        // A NoChange plan leaves the document as-is; phase 2 plans against
        // exactly those bytes.
    } else {
        // Reproduce the pure ladder rather than re-parsing the plan's text:
        // the candidate a `WriteKind` carries is the same transform, and
        // running it here keeps phase 2's input a `DocumentMut` throughout.
        apply_config_transforms(&mut doc, from_version)
            .map_err(|refusal| Error::Config(refusal.to_string()))?;
    }
    let candidate = parse_config(&doc.to_string()).map_err(|e| {
        Error::Config(format!(
            "migrated config does not parse: {}",
            redact_parse_error(&e)
        ))
    })?;
    plan_seat_materialization(&candidate, &doc, inventory, &preexisting_pools)
}

/// The COMBINED candidate text (phase 1 then phase 2), or `None` when
/// neither phase changes anything.
///
/// A `NoChange` plan carries no candidate text, so phase 2 composes onto the
/// ORIGINAL document -- an already-current file can still carry a bare
/// multi-seat ref if it was hand-authored.
fn combined_candidate(
    plan: &MigrationPlan,
    original: &DocumentMut,
    phase_two: &[SeatPoolMove],
) -> Result<Option<String>> {
    if phase_two.is_empty() {
        return Ok(plan.config_candidate().map(str::to_string));
    }
    let mut doc = match plan.config_candidate() {
        Some(text) => parse_document(text)?,
        None => original.clone(),
    };
    apply_seat_materialization(&mut doc, phase_two);
    Ok(Some(doc.to_string()))
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

/// Commit a validated [`MigrationPlan`] plus its phase-2 moves in two
/// phases: the overlay FIRST (revision-checked, under the overlay write
/// lock, idempotent), then `config.toml` LAST as the visible completion
/// marker. The config write reproduces the SAME transforms the combined
/// candidate was gated on, under the write lock against the original
/// snapshot bytes. Takes the plan BY VALUE so the pending overlay cells move
/// into the commit rather than being cloned.
///
/// The stored seats are RE-ENUMERATED immediately before the locked write and
/// phase 2 is re-planned against them: a login or logout between the shown
/// diff and the commit would otherwise land a file describing seats that no
/// longer exist. A mismatch refuses; the file side is covered by
/// `edit_config_toml`'s own byte-snapshot check.
///
/// Two-file commit is not literally atomic without a journal. It is
/// recoverable instead: a crash (or a config-side conflict) after the overlay
/// write leaves `config.toml` at its old version, so a rerun re-plans (the
/// overlay fold is now an idempotent no-op) and completes the config stamp.
async fn commit_plan(
    plan: MigrationPlan,
    phase_two: &[SeatPoolMove],
    config_path: &Path,
    overlay_path: &Path,
    credentials_path: &Path,
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

    // Re-read the seat inventory as late as possible: everything after this
    // point is synchronous, so nothing can await between the check and the
    // write.
    let fresh = enumerate_seats(credentials_path)
        .await
        .map_err(|e| CommitFailure {
            error: Box::new(e),
            outcome: if overlay_written {
                "incomplete"
            } else {
                "write_failed"
            },
        })?;

    // Phase 2: config.toml LAST, the visible version marker.
    edit_config_toml::<CommitError, _>(config_path, snapshot, |d| {
        let preexisting_pools = pool_block_names(d);
        apply_config_transforms(d, from_version).map_err(CommitError::Refused)?;
        let candidate = parse_config(&d.to_string()).map_err(|_| CommitError::Revalidation)?;
        let replanned = plan_seat_materialization(&candidate, d, &fresh, &preexisting_pools)
            .map_err(|_| CommitError::InventoryChanged)?;
        if replanned != phase_two {
            return Err(CommitError::InventoryChanged);
        }
        apply_seat_materialization(d, phase_two);
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
        Refusal::SeatSelectionRelocation { .. } => "seat_selection_relocation",
    }
}

fn render_write_error(err: ConfigWriteError<CommitError>) -> Error {
    Error::Config(err.to_string())
}

/// The candidate block goes to stdout BYTE-EXACT -- that is this surface's
/// contract (the operator is shown exactly what would be written), so there is
/// no masking path: re-rendering through `toml::to_string_pretty` to redact
/// would change key order and formatting too. The credential warning therefore
/// goes to STDERR, where it cannot contaminate the bytes it warns about. Note
/// stdout also carries the framing and summary lines around the block, so it is
/// the delimited block that is byte-exact, not the whole stream.
fn render_dry_run(
    candidate_text: &str,
    from_version: u32,
    to_version: u32,
    removed: &[String],
    original: &DocumentMut,
    phase_two: &[SeatPoolMove],
) {
    eprintln!(
        "warning: the candidate below is printed byte-exact and may carry credentials \
         anywhere the file does -- e.g. userinfo, query, or fragment in a `base_url`, \
         `literal:` key refs, or a secret placed in `header_extras`. Do not paste it into \
         a bug report; if you already did, ROTATE the exposed credentials -- deleting the \
         report is not enough."
    );
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
    if removed.is_empty() && phase_two.is_empty() {
        println!("  (no keys removed; version stamp only)");
    } else {
        for key in removed {
            println!("  - removes `{key}`");
        }
        for line in seat_materialization_summary(original, phase_two) {
            println!("  - {line}");
        }
    }
    println!("dry-run: nothing was written.");
}

/// One summary line per phase-2 move, naming the pool and the account
/// entries it materializes. Names only -- a seat LABEL is operator-authored
/// but a generated entry name carries no token, account id, or storage path,
/// and the surrounding candidate block already renders the file verbatim.
/// One summary line per phase-2 move, naming the pool, the account entries
/// it materializes, and the models it repoints onto the pool.
///
/// `original` is the PRE-migration document, so the repointed nicknames are
/// the ones that named the entry before the rewrite -- which is what the
/// operator is being asked to confirm.
///
/// Names only -- a seat LABEL is operator-authored but a generated entry
/// name, a pool name and a model nickname carry no token, account id, or
/// storage path, and the surrounding candidate block already renders the
/// file verbatim.
fn seat_materialization_summary(original: &DocumentMut, phase_two: &[SeatPoolMove]) -> Vec<String> {
    phase_two
        .iter()
        .map(|mv| {
            let added: Vec<&str> = mv
                .accounts
                .iter()
                .filter(|a| !a.already_present)
                .map(|a| a.entry_name.as_str())
                .collect();
            let mut line = if added.is_empty() {
                format!(
                    "adds `[pools.{}]` grouping the stored seats of `[providers.{}]` \
                     (every account entry already present)",
                    mv.pool, mv.entry
                )
            } else {
                format!(
                    "adds `[pools.{}]` plus account entr{} {} so each stored seat of \
                     `[providers.{}]` is addressable (a bare `oauth://` ref means the \
                     default seat at version {CURRENT_CONFIG_VERSION})",
                    mv.pool,
                    if added.len() == 1 { "y" } else { "ies" },
                    added
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    mv.entry,
                )
            };
            if !mv.accounts.is_empty() {
                let repointed = models_routed_at(original, &mv.entry);
                if !repointed.is_empty() {
                    line.push_str(&format!(
                        "; repoints model{} {} onto `{}` so they keep dispatching across \
                         every seat",
                        if repointed.len() == 1 { "" } else { "s" },
                        repointed
                            .iter()
                            .map(|nickname| format!("`{nickname}`"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        mv.pool,
                    ));
                }
            }
            line
        })
        .collect()
}

/// The completion line, naming the seat materialization when phase 2 did
/// any: an operator who just gained provider entries needs to know before
/// pointing a model at the pool.
fn render_success(
    from_version: u32,
    to_version: u32,
    original: &DocumentMut,
    phase_two: &[SeatPoolMove],
) {
    if from_version == to_version {
        println!(
            "normalized config at version {to_version} (folded legacy `unsupported_features` \
             into [capability.overrides])."
        );
    } else {
        println!("migrated config to version {to_version}.");
    }
    for line in seat_materialization_summary(original, phase_two) {
        println!("  - {line}");
    }
    if !phase_two.is_empty() {
        println!(
            "point a `[models.X] provider` at the pool to route across its accounts; a \
             provider entry still names one account."
        );
    }
    println!("Restart any running routectl daemon onto the matching binary to pick up the change.");
}

/// Acknowledge the schema change before the write lock. `--yes` bypasses the
/// prompt; a non-interactive run (no TTY on stdin) without `--yes` declines
/// immediately without reading, so a silent pipe cannot hang it.
/// Never called while the write lock is held. Called ONCE for the combined
/// change (version stamp, key relocation, and any seat materialization),
/// including a same-version normalization (`from_version == to_version`).
fn confirm_migration(
    from_version: u32,
    to_version: u32,
    original: &DocumentMut,
    phase_two: &[SeatPoolMove],
    yes: bool,
) -> bool {
    if yes {
        return true;
    }
    use std::io::{IsTerminal as _, Write as _};
    // A non-interactive caller with an open-but-silent stdin (a pipe that
    // never sends a line or EOF) would otherwise block `read_line`
    // forever. With no TTY there is no one to answer the prompt, so
    // decline immediately -- the documented non-interactive contract is
    // `--yes`.
    if !std::io::stdin().is_terminal() {
        return false;
    }
    if from_version == to_version {
        println!(
            "this normalizes config.toml at version {to_version}, folding legacy \
             `unsupported_features` lists into `[capability.overrides]` and removing the retired \
             keys. A running routectl daemon must be restarted onto the matching binary afterward."
        );
    } else {
        println!(
            "this migrates config.toml from version {from_version} to {to_version}. The break \
             retires per-status retry lists (and, from a v1 file, the `[cache_pricing]` table), \
             and moves `seat_selection` onto the `[pools.<name>]` block that groups the \
             accounts. A running routectl daemon must be restarted onto the matching binary \
             after migration."
        );
    }
    for line in seat_materialization_summary(original, phase_two) {
        println!("  - {line}");
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
        credentials: std::path::PathBuf,
    }

    impl Fixture {
        /// Drive the pipeline against this fixture's own config, overlay and
        /// credential-store paths. A fixture with no credentials file opens
        /// as an EMPTY store, which is the accurate "nothing logged in"
        /// answer and leaves phase 2 a structural no-op.
        async fn migrate(&self, dry_run: bool, yes: bool) -> Result<MigrateResult> {
            run_at(&self.config, &self.overlay, &self.credentials, dry_run, yes).await
        }

        /// The COMBINED candidate text the pipeline would show and write,
        /// built through the same planning path `run_at` uses. Lets a test
        /// assert on the exact bytes the operator confirms without scraping
        /// stdout.
        async fn plan_combined(&self) -> String {
            let snapshot_text = std::fs::read_to_string(&self.config).unwrap();
            let doc = parse_document(&snapshot_text).expect("parse");
            let from_version = raw_version_of(&doc).expect("raw version");
            let plan = plan_migration(&doc, from_version, &BTreeMap::new(), &self.overlay)
                .expect("pure plan");
            let inventory = enumerate_seats(&self.credentials)
                .await
                .expect("enumerate seats");
            let phase_two =
                plan_phase_two(&plan, &doc, from_version, &inventory).expect("phase 2 plans");
            combined_candidate(&plan, &doc, &phase_two)
                .expect("candidate composes")
                .expect("a migration is pending")
        }
    }

    fn fixture(body: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, body).unwrap();
        let overlay = dir.path().join("catalog_overlay.json");
        let credentials = dir.path().join("credentials.json");
        Fixture {
            _dir: dir,
            config,
            overlay,
            credentials,
        }
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// A v3 config whose provider entry carries a BARE `oauth://` ref plus
    /// the retired provider-level `seat_selection`, and a model routed at it.
    /// This is the shape phase 2 materializes when the store holds more than
    /// one seat for the family.
    const V3_BARE_OAUTH: &str = "\
# operator note: keep me
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.anthropic-managed]
kind = \"anthropic-api\"
api_key_ref = \"oauth://anthropic\"
seat_selection = \"round-robin\"

[models.opus]
provider = \"anthropic-managed\"
upstream = \"claude-opus-4-8\"

[aliases]
default = \"opus\"
";

    /// A plausible token record. Values are inert: nothing in the migration
    /// path resolves or presents a token, and the seat KEYS are all phase 2
    /// reads.
    fn token_record() -> routectl_auth::oauth::types::TokenRecord {
        serde_json::from_value(serde_json::json!({
            "access_token": "not-a-real-token",
            "refresh_token": "not-a-real-token",
            "expires_at_unix": 4_000_000_000_u64,
            "obtained_at_unix": 1_000_u64,
        }))
        .expect("token record fixture parses")
    }

    /// Seed the fixture's credential store with one record per seat key, at
    /// the `0o600` the store requires. Written directly rather than through a
    /// login flow: phase 2 only ever reads seat KEYS.
    fn seed_seats(f: &Fixture, seat_keys: &[&str]) {
        let providers: serde_json::Map<String, serde_json::Value> = seat_keys
            .iter()
            .map(|key| {
                (
                    (*key).to_string(),
                    serde_json::to_value(token_record()).expect("record serializes"),
                )
            })
            .collect();
        let body = serde_json::json!({
            "schema_version": routectl_auth::oauth::SCHEMA_VERSION,
            "providers": providers,
        });
        std::fs::write(&f.credentials, body.to_string()).unwrap();
        set_owner_only(&f.credentials);
    }

    /// Tighten a file to `0o600`; the credential store refuses to open
    /// anything wider.
    fn set_owner_only(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // -----------------------------------------------------------------
    // Phase 2: a bare multi-seat `oauth://` ref materializes into explicit
    // accounts plus a pool, as ONE combined change with ONE confirmation.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn a_bare_multi_seat_ref_materializes_accounts_and_a_pool() {
        // Arrange: three stored seats for the family the bare ref names.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work", "anthropic#personal"]);

        // Act
        let result = f.migrate(false, true).await.expect("combined migrate");

        // Assert
        assert_eq!(result, MigrateResult::Migrated { from_version: 3 });
        let text = read(&f.config);
        assert!(
            text.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "{text}"
        );
        // One account entry per labelled seat, under the naming convention.
        assert!(text.contains("[providers.anthropic-work]"), "{text}");
        assert!(text.contains("[providers.anthropic-personal]"), "{text}");
        assert!(
            text.contains("api_key_ref = \"oauth://anthropic#work\""),
            "{text}"
        );
        // The pool groups them with the original (default-seat) entry.
        assert!(text.contains("[pools.anthropic]"), "{text}");
        for member in ["anthropic-managed", "anthropic-work", "anthropic-personal"] {
            assert!(text.contains(member), "pool must list `{member}`: {text}");
        }
        // The relocated knob landed on the pool, not the provider entry.
        assert!(text.contains("seat_selection = \"round-robin\""), "{text}");
        // The model routed at the entry now names the POOL. Without this the
        // model would keep naming an entry whose bare ref is the DEFAULT SEAT
        // at v4, cutting a 3-seat model down to 1 through the migration.
        let migrated = parse_config(&text).expect("migrated config parses");
        assert_eq!(
            migrated.models["opus"].provider, "anthropic",
            "the model must route at the pool so it keeps every seat: {text}"
        );
        // Comments survive, and the result passes the shared gate.
        assert!(text.contains("# operator note: keep me"), "{text}");
        gate(&text).expect("the migrated config must pass the gate");
    }

    /// Generated names are the naming module's, asserted AGAINST it rather
    /// than re-derived here -- the migration and the login writer must agree
    /// byte-for-byte or reconciliation-by-ref points an entry at the wrong
    /// credential.
    #[tokio::test]
    async fn generated_names_match_the_naming_module_byte_for_byte() {
        // Arrange
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        f.migrate(false, true).await.expect("migrate");

        // Act
        let text = read(&f.config);
        let config = parse_config(&text).expect("migrated config parses");
        let expected = routectl_router::seat_naming::plan_pool_materialization(
            &config,
            "anthropic",
            [None, Some("work")],
        )
        .expect("the convention re-derives over its own output");

        // Assert: every name the convention derives is already present, and
        // re-deriving reports the whole shape as a no-op.
        assert!(config.pools.contains_key(&expected.pool_name));
        assert!(expected.pool_exists, "the pool the convention names exists");
        for account in &expected.accounts {
            assert!(
                config.providers.contains_key(&account.entry_name)
                    || account.entry_name == "anthropic-default",
                "entry `{}` must exist: {text}",
                account.entry_name
            );
        }
        assert!(
            expected
                .accounts
                .iter()
                .filter(|a| a.label.is_some())
                .all(|a| a.already_present),
            "re-deriving over the migration's output must be a no-op: {expected:?}"
        );
    }

    /// CONVERGENCE PIN over the REAL pipeline: run the migration, then run
    /// the login planner over its COMMITTED output. Every seat the
    /// migration materialized must plan `Nothing`.
    ///
    /// Asserted against the actual migrated file rather than a
    /// hand-assembled fixture, so a drift in what the migration WRITES
    /// (a different entry name, a member the pool omits, an auth field the
    /// clone drops) fails here instead of passing a fixture that agrees
    /// with neither writer.
    ///
    /// The source entry states `auth_kind` because convergence is a claim
    /// about a WELL-FORMED config: an entry consuming an `oauth://`
    /// subscription ref under the default `api-key` selector sends
    /// `x-api-key` with a bearer token, and the sibling test below pins
    /// that login SURFACES that (pre-existing) drift rather than pooling
    /// around it.
    #[tokio::test]
    async fn the_login_planner_proposes_nothing_over_the_migrations_committed_output() {
        // Arrange
        let f = fixture(&V3_BARE_OAUTH.replace(
            "api_key_ref = \"oauth://anthropic\"",
            "api_key_ref = \"oauth://anthropic\"\nauth_kind = \"oauth-bearer\"",
        ));
        seed_seats(&f, &["anthropic", "anthropic#work", "anthropic#personal"]);
        f.migrate(false, true).await.expect("combined migrate");
        let text = read(&f.config);
        let config = parse_config(&text).expect("migrated config parses");

        // Act / Assert: one arm per stored seat, the default seat included.
        for label in [None, Some("work"), Some("personal")] {
            let planned = crate::commands::login_surface::plan(&config, "anthropic", label)
                .expect("the planner accepts a known login id");
            assert!(
                matches!(
                    planned,
                    crate::commands::login_surface::SurfacePlan::Nothing { .. }
                ),
                "a login right after a migration must write nothing; seat {label:?} got \
                 {planned:?} over:\n{text}"
            );
            assert!(
                crate::commands::login_surface::render_delta(&planned).is_empty(),
                "seat {label:?} rendered a delta"
            );
        }
    }

    /// The other direction, pinned so neither writer's behavior can change
    /// silently: when the pre-migration entry consumed an `oauth://` ref
    /// under the DEFAULT `api-key` selector, the migration faithfully
    /// preserves that (it clones what the operator wrote), and the login
    /// planner then REFUSES rather than growing a pool around an egress
    /// that would 401. The refusal names the field, never its value.
    #[tokio::test]
    async fn a_migrated_entry_missing_its_auth_selector_refuses_the_login_write() {
        // Arrange: V3_BARE_OAUTH states no `auth_kind`, so every migrated
        // account defaults to `api-key` while carrying a subscription ref.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        f.migrate(false, true).await.expect("combined migrate");
        let config = parse_config(&read(&f.config)).expect("migrated config parses");

        // Act
        let planned = crate::commands::login_surface::plan(&config, "anthropic", None)
            .expect("the planner accepts a known login id");

        // Assert
        let rendered = match &planned {
            crate::commands::login_surface::SurfacePlan::Refuse(reason) => reason.to_string(),
            other => panic!("expected an auth-drift refusal, got {other:?}"),
        };
        assert!(rendered.contains("auth_kind"), "{rendered}");
        assert!(rendered.contains("Nothing was written"), "{rendered}");
        assert!(
            crate::commands::login_surface::render_delta(&planned).is_empty(),
            "a refusal must render no delta"
        );
    }

    #[tokio::test]
    async fn a_single_seat_bare_ref_migrates_without_a_structural_rewrite() {
        // Arrange: exactly one stored seat -- at v4 a bare ref means the
        // default seat, which IS that one seat, so there is nothing to pool.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic"]);

        // Act
        f.migrate(false, true).await.expect("migrate");

        // Assert: the version stamp plus the knob relocation the pure rung
        // owns, and NO materialized account entries.
        let text = read(&f.config);
        assert!(!text.contains("[providers.anthropic-work]"), "{text}");
        assert!(
            !text.contains("[providers.anthropic-default]"),
            "a single-seat ref must not materialize accounts: {text}"
        );
        assert!(
            text.contains("members = [\"anthropic-managed\"]"),
            "the pool the knob relocation creates lists only the entry: {text}"
        );
        // No accounts materialized means no repoint: the pool has one member,
        // so breadth is identical and the member inherits the pool's strategy.
        let migrated = parse_config(&text).expect("migrated config parses");
        assert_eq!(
            migrated.models["opus"].provider, "anthropic-managed",
            "a single-seat migration must not churn the model reference: {text}"
        );
        gate(&text).expect("the migrated config must pass the gate");
    }

    #[tokio::test]
    async fn a_combined_migration_is_idempotent_on_a_rerun() {
        // Arrange
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        f.migrate(false, true).await.expect("first migrate");
        let once = read(&f.config);

        // Act
        let result = f.migrate(false, true).await.expect("rerun");

        // Assert: nothing left to do, file byte-identical.
        assert_eq!(result, MigrateResult::AlreadyCurrent);
        assert_eq!(read(&f.config), once);
    }

    #[tokio::test]
    async fn declining_the_combined_migration_writes_nothing() {
        // stdin is not a TTY under the test harness, so the single
        // acknowledgement is declined without reading.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        let result = f
            .migrate(false, false)
            .await
            .expect("decline is not an error");

        assert_eq!(result, MigrateResult::Aborted);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a declined combined migration must write nothing"
        );
    }

    #[tokio::test]
    async fn a_combined_dry_run_renders_the_candidate_and_writes_nothing() {
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        let result = f.migrate(true, false).await.expect("dry-run");

        assert_eq!(result, MigrateResult::DryRun);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a combined dry-run must write nothing"
        );
    }

    /// The dry-run candidate is what the operator is asked to approve, so the
    /// model repoint has to be IN it -- a diff that silently omitted the
    /// repoint would show a migration that preserved capacity while writing
    /// one that did not.
    #[tokio::test]
    async fn a_combined_dry_run_candidate_carries_the_model_repoint() {
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);

        let plan = f.plan_combined().await;

        assert!(
            plan.contains("provider = \"anthropic\""),
            "the candidate must repoint the model at the pool: {plan}"
        );
        assert!(
            !plan.contains("provider = \"anthropic-managed\""),
            "no model may still name the single-seat entry: {plan}"
        );
    }

    /// The dry-run summary names the pool, the account entries and the models
    /// it repoints, but never a token, an account id, or the store path --
    /// phase 2's inputs are seat KEYS, and its output is config names.
    #[test]
    fn the_seat_materialization_summary_carries_no_secret_material() {
        let original = V3_BARE_OAUTH
            .parse::<DocumentMut>()
            .expect("fixture parses");
        let moves = vec![SeatPoolMove {
            entry: "anthropic-managed".to_string(),
            pool: "anthropic".to_string(),
            accounts: vec![SeatPoolAccount {
                entry_name: "anthropic-work".to_string(),
                secret_ref: "oauth://anthropic#work".to_string(),
                already_present: false,
            }],
        }];

        let lines = seat_materialization_summary(&original, &moves);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("[pools.anthropic]"), "{lines:?}");
        assert!(lines[0].contains("anthropic-work"), "{lines:?}");
        // The repoint is part of what the operator confirms.
        assert!(lines[0].contains("opus"), "{lines:?}");
        assert!(lines[0].contains("repoints model"), "{lines:?}");
        for forbidden in ["not-a-real-token", "credentials.json", "/home/"] {
            assert!(!lines[0].contains(forbidden), "{lines:?}");
        }
    }

    // -----------------------------------------------------------------
    // Fail-closed refusals: each leaves config.toml byte-identical.
    // -----------------------------------------------------------------

    /// A v3 config with TWO provider entries on one OAuth family, each
    /// carrying a bare ref and each naming a DIFFERENT egress host, plus a
    /// model routed at each. Phase 2 must refuse: both entries derive the
    /// pool name `anthropic`, and grouping them would dispatch each account's
    /// bearer to both hosts.
    const V3_TWO_BARE_ENTRIES_ONE_FAMILY: &str = "\
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.anthropic-primary]
kind = \"anthropic-api\"
base_url = \"https://one.example\"
api_key_ref = \"oauth://anthropic\"

[providers.anthropic-secondary]
kind = \"anthropic-api\"
base_url = \"https://two.example\"
api_key_ref = \"oauth://anthropic\"

[models.opus]
provider = \"anthropic-primary\"
upstream = \"claude-opus-4-8\"

[models.sonnet]
provider = \"anthropic-secondary\"
upstream = \"claude-sonnet-4-8\"

[aliases]
default = \"opus\"
";

    /// Two bare refs on one family means the migration would have to guess
    /// which accounts share an egress. It refuses instead, naming both
    /// entries -- the merge would send every account's OAuth bearer to every
    /// egress in the merged set.
    #[tokio::test]
    async fn two_bare_entries_on_one_family_refuse_byte_identical() {
        // Arrange
        let f = fixture(V3_TWO_BARE_ENTRIES_ONE_FAMILY);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("two bare refs on one family must refuse");

        // Assert: both entries named, the remedy stated, nothing written.
        let rendered = err.to_string();
        assert!(
            rendered.contains("[providers.anthropic-primary]"),
            "err: {rendered}"
        );
        assert!(
            rendered.contains("[providers.anthropic-secondary]"),
            "err: {rendered}"
        );
        assert!(rendered.contains("oauth://anthropic#"), "err: {rendered}");
        assert!(rendered.contains("Nothing was written"), "err: {rendered}");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    /// The same shape with a hand-authored PINNED pool already present: the
    /// refusal still fires and the operator's pool keeps its exact members
    /// and its `accepts_new_logins = false` marker.
    #[tokio::test]
    async fn two_bare_entries_with_a_hand_authored_pinned_pool_refuse_untouched() {
        // Arrange
        let body = V3_TWO_BARE_ENTRIES_ONE_FAMILY.replace(
            "[models.opus]",
            "[pools.anthropic]\n\
             members = [\"anthropic-primary\"]\n\
             accepts_new_logins = false\n\n\
             [models.opus]",
        );
        let f = fixture(&body);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("a pinned pool must not be grown by the migration");

        // Assert
        let rendered = err.to_string();
        assert!(rendered.contains("Nothing was written"), "err: {rendered}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "the hand-authored pool must be byte-untouched"
        );
    }

    /// A SINGLE bare entry whose derived pool name is held by a
    /// hand-authored pool block: the migration would grow a pool it did not
    /// create, whose membership is the operator's statement about which
    /// accounts share an egress. Refused, pool byte-untouched.
    #[tokio::test]
    async fn a_hand_authored_pool_is_never_grown_by_the_migration() {
        // Arrange: one bare entry, plus a pinned `[pools.anthropic]` the
        // operator wrote. No provider-level `seat_selection`, so the pure rung
        // has nothing to relocate and phase 2 is the only claimant.
        let body = V3_BARE_OAUTH
            .replace("seat_selection = \"round-robin\"\n", "")
            .replace(
                "[models.opus]",
                "[pools.anthropic]\n\
                 members = [\"anthropic-managed\"]\n\
                 accepts_new_logins = false\n\n\
                 [models.opus]",
            );
        let f = fixture(&body);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("an existing pool must refuse");

        // Assert
        let rendered = err.to_string();
        assert!(rendered.contains("[pools.anthropic]"), "err: {rendered}");
        assert!(rendered.contains("Nothing was written"), "err: {rendered}");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    /// The seat_selection-overwrite path is unreachable: when both same-family
    /// entries also carry the retired knob, the PURE rung refuses before phase
    /// 2 ever plans, so no relocated `seat_selection` can overwrite another's.
    #[tokio::test]
    async fn two_same_family_entries_with_seat_selection_refuse_before_any_relocation() {
        // Arrange: distinct strategies, so a silent overwrite would be a
        // behavior change and not merely a redundant write.
        let body = V3_TWO_BARE_ENTRIES_ONE_FAMILY
            .replace(
                "api_key_ref = \"oauth://anthropic\"\n\n[providers.anthropic-secondary]",
                "api_key_ref = \"oauth://anthropic\"\nseat_selection = \"round-robin\"\n\n\
                 [providers.anthropic-secondary]",
            )
            .replace(
                "api_key_ref = \"oauth://anthropic\"\n\n[models.opus]",
                "api_key_ref = \"oauth://anthropic\"\nseat_selection = \"sticky\"\n\n\
                 [models.opus]",
            );
        let f = fixture(&body);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f.migrate(false, true).await.expect_err("must refuse");

        // Assert: the pure rung's class fires, naming both entries; neither
        // strategy was relocated anywhere.
        let rendered = err.to_string();
        assert!(rendered.contains("seat_selection"), "err: {rendered}");
        assert!(
            rendered.contains("[providers.anthropic-primary]")
                && rendered.contains("[providers.anthropic-secondary]"),
            "err: {rendered}"
        );
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    /// A dry-run on the two-entry shape prints the refusal and writes
    /// nothing: the operator learns the migration cannot proceed BEFORE being
    /// asked to approve anything.
    #[tokio::test]
    async fn a_two_entry_dry_run_refuses_and_writes_nothing() {
        let f = fixture(V3_TWO_BARE_ENTRIES_ONE_FAMILY);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        let err = f
            .migrate(true, false)
            .await
            .expect_err("dry-run must also refuse");

        assert!(
            err.to_string().contains("[providers.anthropic-primary]"),
            "err: {err}"
        );
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    /// The refusal is rendered from config NAMES only: no seat label, no
    /// token, no store path, no filesystem path.
    #[test]
    fn the_family_fan_out_refusal_carries_no_credential_material() {
        let rendered = SeatMaterializationRefusal::FamilyFanOut {
            family: "anthropic".to_string(),
            entries: vec![
                "anthropic-primary".to_string(),
                "anthropic-secondary".to_string(),
            ],
        }
        .to_string();

        assert!(rendered.contains("anthropic-primary"), "{rendered}");
        assert!(rendered.contains("anthropic-secondary"), "{rendered}");
        for forbidden in [
            "not-a-real-token",
            "credentials.json",
            "/home/",
            "oauth://anthropic#work",
        ] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
    }

    /// Same hygiene contract for the existing-pool class.
    #[test]
    fn the_existing_pool_refusal_carries_no_credential_material() {
        let rendered = SeatMaterializationRefusal::ExistingPool {
            pool: "anthropic".to_string(),
            entry: "anthropic-managed".to_string(),
            family: "anthropic".to_string(),
        }
        .to_string();

        assert!(rendered.contains("[pools.anthropic]"), "{rendered}");
        assert!(rendered.contains("anthropic-managed"), "{rendered}");
        for forbidden in ["not-a-real-token", "credentials.json", "/home/"] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_store_refuses_the_whole_migration_byte_identical() {
        // Arrange: a credentials file the store cannot open (wider than the
        // `0o600` it requires). Distinct from a MISSING file, which opens as
        // an empty store and is an accurate "nothing logged in".
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&f.credentials, std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("an unreadable store must refuse");

        // Assert: the refusal explains the seat-loss risk, names no path, and
        // wrote nothing.
        let rendered = err.to_string();
        assert!(rendered.contains("stored OAuth seats"), "err: {rendered}");
        assert!(rendered.contains("Nothing was written"), "err: {rendered}");
        assert!(
            !rendered.contains(f.credentials.to_string_lossy().as_ref()),
            "the refusal must not disclose the store path: {rendered}"
        );
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    #[tokio::test]
    async fn a_generated_name_held_by_an_unrelated_entry_refuses_byte_identical() {
        // Arrange: `anthropic-work` already exists carrying a DIFFERENT
        // credential, so writing the generated entry would repoint it.
        let body = V3_BARE_OAUTH.replace(
            "[models.opus]",
            "[providers.anthropic-work]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#other\"\n\n\
             [models.opus]",
        );
        let f = fixture(&body);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("a taken entry name must refuse");

        // Assert: the naming module's own wording surfaces unsoftened.
        let rendered = err.to_string();
        assert!(rendered.contains("anthropic-work"), "err: {rendered}");
        assert!(rendered.contains("different credential"), "err: {rendered}");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    #[tokio::test]
    async fn a_seat_label_that_cannot_be_a_config_key_refuses_byte_identical() {
        // Arrange: a label carrying characters no generated entry name can
        // hold verbatim. Refused rather than normalized -- a rewritten name
        // no longer identifies the seat it came from.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work seat"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("an unusable label must refuse");

        // Assert
        assert!(
            err.to_string().contains("cannot be used verbatim"),
            "err: {err}"
        );
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    #[tokio::test]
    async fn the_reserved_default_label_refuses_byte_identical() {
        // Arrange: a seat literally labelled `default` would generate the
        // default seat's own entry name, aliasing two credentials onto one
        // entry.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#default"]);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("the reserved label must refuse");

        // Assert
        assert!(err.to_string().contains("reserved"), "err: {err}");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    #[tokio::test]
    async fn an_unrelocatable_seat_selection_refuses_byte_identical() {
        // Arrange: the pure rung's own refusal, surfaced through the command
        // -- an API-key provider carrying the retired knob has no provider
        // family to name a pool after.
        let body = V3_BARE_OAUTH
            .replace(
                "api_key_ref = \"oauth://anthropic\"",
                "api_key_ref = \"env://K\"",
            )
            .replace(
                "kind = \"anthropic-api\"",
                "kind = \"openai-compat\"\nbase_url = \"https://x\"",
            );
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        // Act
        let err = f
            .migrate(false, true)
            .await
            .expect_err("an unrelocatable knob must refuse");

        // Assert
        assert!(err.to_string().contains("seat_selection"), "err: {err}");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    #[tokio::test]
    async fn a_refused_combined_migration_audits_the_refusal_kind() {
        let body = V3_BARE_OAUTH
            .replace(
                "api_key_ref = \"oauth://anthropic\"",
                "api_key_ref = \"env://K\"",
            )
            .replace(
                "kind = \"anthropic-api\"",
                "kind = \"openai-compat\"\nbase_url = \"https://x\"",
            );
        let f = fixture(&body);

        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;

        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("refused"));
        assert_eq!(
            audit.field("refusal_kind"),
            Some("seat_selection_relocation")
        );
    }

    /// The seat inventory is re-read under the write lock: a login or logout
    /// between the shown diff and the commit must refuse rather than land a
    /// file describing seats that no longer exist.
    #[tokio::test]
    async fn an_inventory_change_between_plan_and_commit_refuses_without_writing() {
        // Arrange: plan against two seats.
        let f = fixture(V3_BARE_OAUTH);
        seed_seats(&f, &["anthropic", "anthropic#work"]);
        let snapshot = std::fs::read(&f.config).unwrap();
        let snapshot_text = String::from_utf8(snapshot.clone()).unwrap();
        let doc = parse_document(&snapshot_text).expect("parse");
        let plan = plan_migration(&doc, 3, &BTreeMap::new(), &f.overlay).expect("plan");
        let inventory = enumerate_seats(&f.credentials).await.expect("enumerate");
        let phase_two = plan_phase_two(&plan, &doc, 3, &inventory).expect("phase 2 plans");
        assert!(!phase_two.is_empty(), "the fixture must plan a pool");

        // A third seat appears after planning -- the shown diff no longer
        // describes the store.
        seed_seats(&f, &["anthropic", "anthropic#work", "anthropic#personal"]);

        // Act
        let failure = commit_plan(
            plan,
            &phase_two,
            &f.config,
            &f.overlay,
            &f.credentials,
            &snapshot,
            3,
        )
        .await
        .expect_err("a changed inventory must refuse at the commit");

        // Assert: nothing written, and the message says to rerun.
        assert_eq!(failure.outcome, "write_failed");
        assert!(
            failure.error.to_string().contains("changed between"),
            "err: {}",
            failure.error
        );
        assert_eq!(std::fs::read(&f.config).unwrap(), snapshot);
    }

    // -----------------------------------------------------------------
    // Ack matrix: --yes writes; non-interactive without --yes refuses;
    // dry-run needs neither.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn yes_migrates_v2_to_the_current_version_and_the_result_revalidates() {
        let f = fixture(V2_CLEAN);
        let result = f.migrate(false, true).await.expect("yes migrate");
        assert_eq!(result, MigrateResult::Migrated { from_version: 2 });

        let text = read(&f.config);
        assert!(
            text.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "{text}"
        );
        assert!(!text.contains("retry_allowlist"), "{text}");
        assert!(!text.contains("retry_denylist"), "{text}");
        // Comments and unrelated content survive.
        assert!(text.contains("# operator note: keep me"), "{text}");
        // The committed file re-validates through the shared gate.
        gate(&text).expect("migrated config must pass the gate");
    }

    #[tokio::test]
    async fn non_interactive_without_yes_refuses_with_nothing_written() {
        let f = fixture(V2_CLEAN);
        let before = std::fs::read(&f.config).unwrap();

        // stdin is not a TTY under the test harness, so the prompt is
        // declined immediately without reading.
        let result = f
            .migrate(false, false)
            .await
            .expect("decline is not an error");
        assert_eq!(result, MigrateResult::Aborted);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a declined migration must not write"
        );
    }

    #[tokio::test]
    async fn v1_non_interactive_without_yes_refuses_before_any_mutation() {
        // A v1 file's migration mutates disk INSIDE the ladder (the v1 rung
        // rewrites config.toml to v2 and folds the overlay). Authorization
        // runs before the ladder, so a declined non-interactive run (no TTY)
        // must leave the file byte-identical at v1 AND never create the
        // overlay -- the regression the batch gate flagged.
        let f = fixture(V1_WITH_CACHE_PRICING);
        let before = std::fs::read(&f.config).unwrap();

        let result = f
            .migrate(false, false)
            .await
            .expect("decline is not an error");
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

    #[tokio::test]
    async fn confirm_migration_declines_immediately_on_non_tty_without_yes() {
        // Under the test harness stdin is not a TTY, so the terminal gate
        // must fire and decline WITHOUT reaching read_line -- a silent
        // pipe can no longer hang the prompt.
        use std::io::IsTerminal as _;
        assert!(
            !std::io::stdin().is_terminal(),
            "test harness stdin must be non-interactive for this assertion",
        );
        assert!(
            !confirm_migration(3, 4, &DocumentMut::new(), &[], false),
            "non-TTY without --yes must decline",
        );
        assert!(
            confirm_migration(3, 4, &DocumentMut::new(), &[], true),
            "--yes must still proceed byte-identically",
        );
    }

    #[tokio::test]
    async fn dry_run_renders_the_candidate_and_writes_nothing() {
        let f = fixture(V2_CLEAN);
        let before = std::fs::read(&f.config).unwrap();

        let result = f.migrate(true, false).await.expect("dry-run");
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

    #[tokio::test]
    async fn behavior_bearing_list_is_refused_byte_identical() {
        let f = fixture(V2_BEHAVIOR_BEARING);
        let before = std::fs::read(&f.config).unwrap();

        let err = f.migrate(false, true).await.expect_err("must refuse");
        assert!(err.to_string().contains("503"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a refused migration must leave the file byte-identical"
        );
    }

    #[tokio::test]
    async fn behavior_bearing_dry_run_is_also_refused_and_writes_nothing() {
        let f = fixture(V2_BEHAVIOR_BEARING);
        let before = std::fs::read(&f.config).unwrap();

        f.migrate(true, false)
            .await
            .expect_err("dry-run must also refuse");
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    // -----------------------------------------------------------------
    // v1 chains v1->v2->v3: cache_pricing folded to the overlay AND the
    // retry lists gone; comments preserved.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn v1_file_chains_to_the_current_version_folding_cache_pricing_into_the_overlay() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let result = f.migrate(false, true).await.expect("v1 migrate");
        assert_eq!(result, MigrateResult::Migrated { from_version: 1 });

        let text = read(&f.config);
        assert!(
            text.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "{text}"
        );
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

    #[tokio::test]
    async fn v1_dry_run_touches_neither_config_nor_overlay() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let before = std::fs::read(&f.config).unwrap();

        let result = f.migrate(true, false).await.expect("v1 dry-run");
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

    #[tokio::test]
    async fn already_at_the_current_version_is_a_no_op() {
        let current = V2_CLEAN.replacen(
            "version = 2",
            &format!("version = {CURRENT_CONFIG_VERSION}"),
            1,
        );
        let f = fixture(&current);
        let before = std::fs::read(&f.config).unwrap();

        let result = f.migrate(false, true).await.expect("no-op");
        assert_eq!(result, MigrateResult::AlreadyCurrent);
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    // -----------------------------------------------------------------
    // Gate failure: an invalid candidate writes nothing.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn invalid_candidate_writes_nothing() {
        // A v2 config whose alias points at an undefined model migrates
        // cleanly (empty retry lists) but fails the shared cross-field gate.
        let body = V2_CLEAN.replace("default = \"gpt\"", "default = \"no-such-model\"");
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        let err = f.migrate(false, true).await.expect_err("gate must reject");
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

    #[tokio::test]
    async fn gate_parse_failure_does_not_echo_a_secret_bearing_source_line() {
        const SECRET: &str = "sk-THIS-IS-A-FAKE-CREDENTIAL-value";
        // An unknown field under a known table: parse_config rejects it, and
        // toml's diagnostic would frame the offending line -- carrying the
        // secret -- unless the preview is redacted.
        let candidate = format!(
            "version = {CURRENT_CONFIG_VERSION}\n\n[server]\nhost = \"127.0.0.1\"\nport = \
             8787\nbogus_secret_key = \"{SECRET}\"\n"
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

    #[tokio::test]
    async fn gate_type_mismatch_in_non_string_field_does_not_survive() {
        // A fake secret mistyped into the numeric `port` field: serde renders
        // `invalid type: string "...", expected u16`, embedding it verbatim.
        const SECRET: &str = "sk-THIS-IS-A-FAKE-CREDENTIAL-value";
        let candidate = format!(
            "version = {CURRENT_CONFIG_VERSION}\n\n[server]\nhost = \"127.0.0.1\"\nport = \
                 \"{SECRET}\"\n"
        );

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

    #[tokio::test]
    async fn stale_base_bytes_conflict_writes_nothing() {
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

    #[tokio::test]
    async fn emits_audit_event_with_versions_and_no_bytes() {
        let f = fixture(V2_CLEAN);
        let (result, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;
        result.expect("migrate");

        let audit: Vec<_> = events
            .iter()
            .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("migrate"))
            .collect();
        assert_eq!(audit.len(), 1, "exactly one migrate audit event expected");

        let event = audit[0];
        assert_eq!(event.field("from_version"), Some("2"));
        assert_eq!(
            event.field("to_version"),
            Some(CURRENT_CONFIG_VERSION.to_string().as_str())
        );
        assert_eq!(event.field("dry_run"), Some("false"));
        assert_eq!(event.field("forced"), Some("true"));
        assert_eq!(event.field("outcome"), Some("written"));
        // No candidate bytes / config values are ever fields.
        assert!(event.field("candidate").is_none());
        assert!(event.field("value").is_none());
    }

    #[tokio::test]
    async fn refusal_audit_event_names_the_kind() {
        let f = fixture(V2_BEHAVIOR_BEARING);
        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("refused"));
        assert_eq!(audit.field("refusal_kind"), Some("behavior_bearing"));
    }

    // -----------------------------------------------------------------
    // Same-version normalization: legacy unsupported_features fold into
    // [capability.overrides]; egress allowlists and conflicts refuse.
    // -----------------------------------------------------------------

    /// A current-version config carrying legacy provider AND model
    /// `unsupported_features` plus a valid provider/model/alias so the folded
    /// result passes the gate.
    fn latest_with_legacy() -> String {
        format!("# operator note: keep me\nversion = {CURRENT_CONFIG_VERSION}\n") + LEGACY_BODY
    }

    const LEGACY_BODY: &str = "\
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

    /// A plain current-version config with no legacy fields at all.
    fn latest_clean() -> String {
        format!("version = {CURRENT_CONFIG_VERSION}\n") + CLEAN_BODY
    }

    const CLEAN_BODY: &str = "\
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

    #[tokio::test]
    async fn legacy_lists_normalize_into_capability_overrides_and_keys_removed() {
        let f = fixture(&latest_with_legacy());
        let result = f.migrate(false, true).await.expect("normalize");
        assert_eq!(
            result,
            MigrateResult::Migrated {
                from_version: CURRENT_CONFIG_VERSION
            }
        );

        let text = read(&f.config);
        assert!(!text.contains("unsupported_features"), "{text}");
        assert!(
            text.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "{text}"
        );
        assert!(text.contains("[capability.overrides.fast]"), "{text}");
        assert!(
            text.contains("[capability.overrides.\"fast:gpt\"]"),
            "{text}"
        );
        assert!(text.contains("# operator note: keep me"), "{text}");
        // The committed file re-validates and loads with no legacy keys left.
        gate(&text).expect("normalized config must pass the gate");
    }

    #[tokio::test]
    async fn no_legacy_fields_is_already_current_and_writes_nothing() {
        let f = fixture(&latest_clean());
        let before = std::fs::read(&f.config).unwrap();

        let result = f.migrate(false, true).await.expect("already current");
        assert_eq!(result, MigrateResult::AlreadyCurrent);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a plain v3 file must not be rewritten"
        );
    }

    #[tokio::test]
    async fn egress_allowlist_refuses_byte_identical() {
        let body = latest_with_legacy().replace(
            "[server]\n",
            "[bedrock]\nallowed_betas = [\"beta-1\"]\n\n[server]\n",
        );
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        let err = f
            .migrate(false, true)
            .await
            .expect_err("egress allowlist refuses");
        assert!(err.to_string().contains("allowed_betas"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a refused normalization must leave the file byte-identical"
        );
    }

    #[tokio::test]
    async fn egress_allowlist_refusal_audit_names_the_kind() {
        let body = latest_with_legacy().replace(
            "[server]\n",
            "[bedrock]\nallowed_betas = [\"beta-1\"]\n\n[server]\n",
        );
        let f = fixture(&body);
        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("refused"));
        assert_eq!(audit.field("refusal_kind"), Some("egress_allowlist"));
    }

    #[tokio::test]
    async fn conflicting_cell_refuses_via_the_gate_byte_identical() {
        // Legacy provider list routes `web_search` away while a new
        // force_supported entry marks the SAME cell supported: after folding
        // the legacy list into `unsupported`, the shared gate's conflict
        // check rejects, and the file stays byte-identical.
        let body = latest_with_legacy().replace(
            "[aliases]\n",
            "[capability.overrides.fast]\nforce_supported = [\"web_search\"]\n\n[aliases]\n",
        );
        let f = fixture(&body);
        let before = std::fs::read(&f.config).unwrap();

        let err = f
            .migrate(false, true)
            .await
            .expect_err("conflict must refuse");
        assert!(err.to_string().contains("config error"), "err: {err}");
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "a conflicting normalization must leave the file byte-identical"
        );
    }

    #[tokio::test]
    async fn normalize_dry_run_renders_candidate_and_writes_nothing() {
        let f = fixture(&latest_with_legacy());
        let before = std::fs::read(&f.config).unwrap();

        let result = f.migrate(true, false).await.expect("dry-run");
        assert_eq!(result, MigrateResult::DryRun);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "dry-run must not write"
        );
    }

    // -----------------------------------------------------------------
    // A v1 file that hits a v2->v3 refusal DURING planning leaves BOTH
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

    #[tokio::test]
    async fn v1_refusal_leaves_config_and_overlay_byte_untouched() {
        let f = fixture(V1_CACHE_PRICING_AND_BEHAVIOR_BEARING);
        let before = std::fs::read(&f.config).unwrap();

        let err = f
            .migrate(false, true)
            .await
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

    #[tokio::test]
    async fn v1_refusal_audit_never_reports_written_and_names_the_kind() {
        let f = fixture(V1_CACHE_PRICING_AND_BEHAVIOR_BEARING);
        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("refused"));
        assert_eq!(audit.field("refusal_kind"), Some("behavior_bearing"));
    }

    // -----------------------------------------------------------------
    // A same-version v3 normalization is a REAL write and must be
    // prompt/force-gated like any other, and its audit must reflect the true
    // acknowledgement (never a synthesized acknowledged=true).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn normalize_non_interactive_without_yes_aborts_byte_identical() {
        // stdin is not a TTY under the test harness: read_line hits EOF, so
        // the normalize prompt is declined and nothing is written.
        let f = fixture(&latest_with_legacy());
        let before = std::fs::read(&f.config).unwrap();

        let result = f
            .migrate(false, false)
            .await
            .expect("declining is not an error");
        assert_eq!(result, MigrateResult::Aborted);
        assert_eq!(
            std::fs::read(&f.config).unwrap(),
            before,
            "an unacknowledged v3 normalization must not write"
        );
    }

    #[tokio::test]
    async fn normalize_forced_audit_records_acknowledged_false_not_synthesized() {
        // A forced normalize was authorized by --yes, NOT by an interactive
        // acknowledgement, so `acknowledged` must be false -- the defect
        // was a synthesized acknowledged=true on this exact path.
        let f = fixture(&latest_with_legacy());
        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("written"));
        assert_eq!(audit.field("forced"), Some("true"));
        assert_eq!(
            audit.field("acknowledged"),
            Some("false"),
            "a --yes normalize must not synthesize acknowledged=true"
        );
    }

    // -----------------------------------------------------------------
    // The audit distinguishes aborted / refused / dry_run / written.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn aborted_audit_event_names_aborted() {
        let f = fixture(V2_CLEAN);
        // Non-interactive without --yes declines at the prompt.
        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(false, false).await }).await;
        let audit = events
            .iter()
            .find(|e| e.field("verb") == Some("migrate"))
            .expect("a migrate audit event");
        assert_eq!(audit.field("outcome"), Some("aborted"));
    }

    #[tokio::test]
    async fn dry_run_audit_event_names_dry_run() {
        let f = fixture(V2_CLEAN);
        let (_, events) =
            routectl_testkit::with_capture(async { f.migrate(true, false).await }).await;
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

    #[tokio::test]
    async fn partial_commit_state_reruns_safely_to_completion() {
        let f = fixture(V1_WITH_CACHE_PRICING);
        let v1_body = std::fs::read(&f.config).unwrap();

        // First run completes both phases (overlay folded, config -> v3).
        f.migrate(false, true).await.expect("first migrate");
        assert!(f.overlay.exists(), "first run folds the overlay");
        let overlay_after_first = std::fs::read(&f.overlay).unwrap();

        // Simulate a crash between the overlay commit and the config stamp:
        // config.toml is rolled back to its original v1 content while the
        // overlay's write is durable.
        std::fs::write(&f.config, &v1_body).unwrap();

        // Rerun completes safely: the overlay fold is now an idempotent no-op
        // (no double write) and config.toml is stamped forward to v3.
        let result = f.migrate(false, true).await.expect("rerun");
        assert_eq!(result, MigrateResult::Migrated { from_version: 1 });
        let text = read(&f.config);
        assert!(
            text.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "{text}"
        );
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

    #[tokio::test]
    async fn overlay_commit_refuses_a_stale_base_revision_without_clobbering() {
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

    #[tokio::test]
    async fn live_mid_commit_config_conflict_lands_overlay_and_reports_incomplete() {
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
        let failure = commit_plan(
            plan,
            &[],
            &f.config,
            &f.overlay,
            &f.credentials,
            &snapshot,
            1,
        )
        .await
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

    #[tokio::test]
    async fn run_at_config_conflict_after_overlay_audits_incomplete_and_resumes() {
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

        let (outcome, events) =
            routectl_testkit::with_capture(async { f.migrate(false, true).await }).await;
        racer.join().expect("racer thread");

        // (b) the user-facing error is the resumable message, never a false
        // "nothing was written".
        let err = outcome.expect_err("a config conflict after the overlay lands must fail");
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
        let result = f.migrate(false, true).await.expect("rerun completes");
        assert_eq!(result, MigrateResult::Migrated { from_version: 1 });
        let text = read(&f.config);
        assert!(
            text.contains(&format!("version = {CURRENT_CONFIG_VERSION}")),
            "{text}"
        );
        assert!(!text.contains("cache_pricing"), "{text}");
        gate(&text).expect("the completed config must pass the gate");
    }

    // -----------------------------------------------------------------
    // A config-phase conflict with NO overlay write (a ConfigOnly plan)
    // audits `conflict` -- labelled by the ConfigWriteError variant, not
    // the old hardcoded value.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn config_phase_conflict_without_overlay_reports_conflict() {
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

        let failure = commit_plan(
            plan,
            &[],
            &f.config,
            &f.overlay,
            &f.credentials,
            &snapshot,
            2,
        )
        .await
        .expect_err("a stale snapshot must conflict at the config phase");
        assert_eq!(failure.outcome, "conflict");
        assert!(
            !f.overlay.exists(),
            "a ConfigOnly plan must not write the overlay"
        );
    }
}
