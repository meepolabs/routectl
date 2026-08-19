//! `routectl login`'s config auto-surface: what config delta a freshly
//! minted seat implies, how that delta renders as pasteable TOML, and the
//! confirmed write that applies it.
//!
//! [`plan`] and [`render_delta`] are PURE over a parsed [`Config`] -- no
//! IO, no credential store, no clock -- so everything judgment-bearing is
//! testable as a table. [`surface`] is the one impure half: it owns the
//! byte snapshot, the preflights, the confirmation and the write, and it
//! decides nothing the planner could have decided.
//!
//! Reconciliation is by CREDENTIAL REF, never by generated name.
//! [`ref_matches`] finds the entries that already consume the seat's
//! `oauth://` ref through the same `secret_uris()` accessor `config check`
//! and the naming convention read. Name-only matching would mint a second
//! entry whenever an operator hand-named the one that already carries the
//! ref, and every later login would then face two candidates for one
//! credential.
//!
//! The resolution table [`plan`] implements:
//!
//! | ref matches | matched entry's pool | plan |
//! |---|---|---|
//! | 2+ | -- | refuse, name the candidates, write nothing |
//! | 1 | member of a growth pool | nothing (idempotent re-login) |
//! | 1 | member of a pinned pool | nothing, with a note |
//! | 1 | member of 2+ pools | refuse (config validation forbids it) |
//! | 1 | no pool, one growth pool for the family | join that pool |
//! | 1 | no pool, no growth pool | create the family pool |
//! | 1 | no pool, 2+ growth pools | refuse (ambiguous pool) |
//! | 0 | -- | new account entry, plus join or create |
//!
//! On the one-match arm the pool comes from the FAMILY's growth pools
//! directly, never from the convention's default-seat placement: that
//! placement also validates the generated entry name, and this arm writes
//! no entry, so an unrelated entry squatting `<family>-default` must not
//! turn a determined join into a pool creation.
//!
//! Every refusal is a PLAN VALUE, not an error: login already succeeded
//! and the credential is stored, so a refusal prints and exits clean. The
//! one `Err` is an oauth id with no known provider shape, which the CLI's
//! accepted set (the login registry) cannot produce.

use std::path::Path;

use routectl_auth::SecretRef;
use routectl_core::{Error, Result};
use routectl_router::seat_naming::{
    SeatNamingError, growth_pools_for_family, plan_new_seat, plan_pool_materialization,
    seat_secret_ref,
};
use routectl_router::{
    Config, ConfigWriteError, EditOutcome, ProviderEntry, parse_config,
    upsert_pool_members as upsert_pool,
};
use toml_edit::{DocumentMut, Item};

use super::edit_pipeline::{
    RelockValidationError, confirm_high_consequence, gate, insert_provider_block, parse_document,
    preflight, render_gate_errors,
};
use super::login_provider_block::{
    ProviderBlock, provider_block, required_auth_fields, toml_key, toml_string,
};
use super::login_surface_availability::availability_gap;
use super::parse_error_redaction::{CONFIG_UNREADABLE, redact_parse_error};
use crate::config_classify::collect_high_consequence_changes;

/// The config delta a freshly minted seat implies.
#[derive(Debug)]
pub enum SurfacePlan {
    /// Config already reaches this seat; nothing to write.
    Nothing {
        /// The entry that already consumes the seat's ref.
        entry_name: String,
        /// Why nothing is proposed, when the reason is not plain
        /// idempotence (a pinned pool is never auto-joined).
        note: Option<NothingNote>,
    },
    /// A delta to show and, on confirmation, write.
    Write(SurfaceWrite),
    /// Nothing is written and the reason is reported.
    Refuse(RefuseReason),
}

/// Why a [`SurfacePlan::Nothing`] proposes no write despite the entry not
/// being in a growth pool.
#[derive(Debug, PartialEq, Eq)]
pub enum NothingNote {
    /// The entry belongs to a pool carrying no `accepts_new_logins`
    /// marker. That marker is an operator statement that only an explicit
    /// edit grows the pool, so login never flips it.
    PinnedPool {
        /// The pool holding the entry.
        pool: String,
    },
}

impl std::fmt::Display for NothingNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PinnedPool { pool } => write!(
                f,
                "[pools.{pool}] does not accept new logins, so its membership was left \
                 alone; add `accepts_new_logins = true` to let future logins grow it"
            ),
        }
    }
}

/// One config delta: at most one new provider entry, plus at most one pool
/// action.
#[derive(Debug)]
pub struct SurfaceWrite {
    /// The provider entry to create, or `None` when an entry already
    /// consumes the ref and only its pool membership changes.
    pub new_entry: Option<ProviderBlock>,
    /// The `[providers.<name>]` key that ends up serving the seat --
    /// either `new_entry`'s name or the matched entry's.
    pub entry_name: String,
    /// What happens to the pool, or `None` when the pool half was
    /// deliberately skipped: the family's pool exists but is PINNED, so the
    /// account entry surfaces while the operator's membership list is left
    /// exactly as written. `note` says so.
    pub pool: Option<PoolAction>,
    /// Why the pool half was skipped, when it was.
    pub note: Option<NothingNote>,
}

/// What a [`SurfaceWrite`] does to the pool that groups the family's seats.
#[derive(Debug)]
pub enum PoolAction {
    /// Add the member to a pool that already exists and already accepts
    /// new logins. Its `accepts_new_logins` marker is NOT rewritten -- the
    /// pool already carries it, and touching an operator's marker is not
    /// login's business.
    Join {
        /// The pool name.
        pool: String,
        /// The full member list AFTER the join, in write order.
        members: Vec<String>,
    },
    /// Create the pool, member list and growth marker included. The marker
    /// is written ONLY here: a pool login creates is one login may grow.
    Create {
        /// The pool name.
        pool: String,
        /// The pool's member list.
        members: Vec<String>,
    },
}

impl PoolAction {
    /// The pool name, whichever action this is.
    #[must_use]
    pub fn pool(&self) -> &str {
        match self {
            Self::Join { pool, .. } | Self::Create { pool, .. } => pool,
        }
    }
}

/// Why a plan writes nothing and reports instead.
#[derive(Debug)]
pub enum RefuseReason {
    /// More than one provider entry already consumes the seat's ref, so
    /// which one a pool would grow around is not determined by config.
    AmbiguousEntries {
        /// The entry names carrying the ref, sorted.
        candidates: Vec<String>,
    },
    /// A naming-convention refusal, surfaced verbatim.
    Naming(SeatNamingError),
    /// The entry that already carries the seat's ref is listed by more
    /// than one pool. Validation forbids that, but this planner is pure
    /// over a config it did not gate, so it refuses rather than letting
    /// map ordering decide which pool's growth marker governs.
    EntryInMultiplePools {
        /// The matched entry.
        entry_name: String,
        /// The pools listing it, in pool-name order.
        pools: Vec<String>,
    },
    /// The entry that already carries the seat's ref does not carry the
    /// auth shape this provider requires, so growing a pool around it
    /// would spread a misconfigured egress.
    ///
    /// Names the FIELD NAMES only. The current values are operator
    /// content of unknown provenance, and this message is printed.
    AuthFieldDrift {
        /// The matched entry.
        entry_name: String,
        /// The required fields that are absent or hold the wrong value.
        fields: Vec<&'static str>,
    },
}

impl std::fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousEntries { candidates } => write!(
                f,
                "{} provider entries already carry this credential ({}); which one a pool \
                 would grow around is not determined by config. Nothing was written",
                candidates.len(),
                candidates.join(", ")
            ),
            Self::Naming(err) => write!(f, "{err}. Nothing was written"),
            Self::EntryInMultiplePools { entry_name, pools } => write!(
                f,
                "[providers.{entry_name}] already carries this credential but is listed by \
                 {} pools ({}); a provider entry belongs to at most one pool, so which \
                 pool governs is not determined by config. Nothing was written",
                pools.len(),
                pools.join(", ")
            ),
            Self::AuthFieldDrift { entry_name, fields } => write!(
                f,
                "[providers.{entry_name}] already carries this credential but its \
                 required auth fields do not match what this provider needs ({}); \
                 fix those fields by hand, then rerun. Nothing was written",
                fields.join(", ")
            ),
        }
    }
}

/// Entry names whose credential refs include `secret_ref` exactly, sorted.
///
/// Walks `secret_uris()` -- the same accessor the naming convention and
/// `config check` read -- so an entry authenticating through a non-primary
/// slot is found too.
#[must_use]
pub fn ref_matches(config: &Config, secret_ref: &SecretRef) -> Vec<String> {
    let wanted = secret_ref.to_string();
    config
        .providers
        .iter()
        .filter(|(_, entry)| entry.secret_uris().contains(&wanted.as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Plan the config delta for a seat of `family` labelled `label`.
///
/// # Errors
///
/// `family` has no known provider shape. The CLI validates the login id
/// against the auth registry, which IS this table's domain, so this is
/// unreachable through the command surface.
pub fn plan(config: &Config, family: &str, label: Option<&str>) -> Result<SurfacePlan> {
    let block = provider_block(family, label).ok_or_else(|| {
        Error::Config(format!(
            "`{family}` has no known provider entry shape, so no config delta can be \
             proposed for it"
        ))
    })?;
    let secret_ref = seat_secret_ref(family, label);
    let parsed = SecretRef::parse(&secret_ref)
        .map_err(|e| Error::Config(format!("seat credential ref does not parse: {e}")))?;

    match ref_matches(config, &parsed).as_slice() {
        [] => plan_fresh_entry(config, family, label, block),
        [matched] => Ok(plan_existing_entry(config, family, matched)),
        candidates => Ok(SurfacePlan::Refuse(RefuseReason::AmbiguousEntries {
            candidates: candidates.to_vec(),
        })),
    }
}

/// The zero-match arm: a new account entry named by the convention, plus
/// the pool action its family's config state implies -- or no pool action at
/// all when the family's pool is pinned.
fn plan_fresh_entry(
    config: &Config,
    family: &str,
    label: Option<&str>,
    block: ProviderBlock,
) -> Result<SurfacePlan> {
    let placement = match plan_new_seat(config, family, label) {
        Ok(placement) => placement,
        Err(e) => return Ok(SurfacePlan::Refuse(RefuseReason::Naming(e))),
    };
    // The convention applied against THIS config is the naming authority,
    // not the block's own suggestion -- the two agree, and taking it from
    // here means a future divergence cannot write one name and print
    // another.
    let entry_name = placement.account.entry_name.clone();
    let block = block.with_entry_name(entry_name.clone());

    // `plan_new_seat` reports a pool ONLY when it is growth-marked, so a
    // `None` here means either no pool at all or a PINNED one -- and those
    // two need opposite handling. Distinguishing them is what
    // `pool_action_for_new_entry` does; conflating them is how a pinned pool
    // silently grows and gains a marker it never had.
    let (pool, note) = match placement.pool_name {
        Some(pool) => (Some(join_action(config, &pool, &entry_name)), None),
        None => match pool_action_for_new_entry(config, family, &entry_name) {
            Ok(pair) => pair,
            Err(e) => return Ok(SurfacePlan::Refuse(RefuseReason::Naming(e))),
        },
    };
    Ok(SurfacePlan::Write(SurfaceWrite {
        new_entry: Some(block),
        entry_name,
        pool,
        note,
    }))
}

/// The pool half for a seat with no growth-marked pool to join: create the
/// family pool when none exists, or skip the pool entirely when one exists
/// but is pinned.
///
/// A pinned pool is an operator statement that only an explicit edit grows
/// it. The account entry still surfaces -- the credential deserves to be
/// visible in config -- but the membership list and the absent
/// `accepts_new_logins` marker are both left exactly as written. This
/// mirrors the one-match arm, which has always refused to auto-join a
/// pinned pool.
fn pool_action_for_new_entry(
    config: &Config,
    family: &str,
    member: &str,
) -> std::result::Result<(Option<PoolAction>, Option<NothingNote>), SeatNamingError> {
    let plan = plan_pool_materialization(config, family, [])?;
    if plan.pool_exists {
        return Ok((
            None,
            Some(NothingNote::PinnedPool {
                pool: plan.pool_name,
            }),
        ));
    }
    Ok((
        Some(PoolAction::Create {
            pool: plan.pool_name,
            members: vec![member.to_string()],
        }),
        None,
    ))
}

/// The one-match arm: the entry already consumes the ref, so only its pool
/// membership can be missing -- and only if the entry's required auth
/// fields are actually right.
fn plan_existing_entry(config: &Config, family: &str, entry_name: &str) -> SurfacePlan {
    if let Some(fields) = drifted_auth_fields(config, family, entry_name) {
        return SurfacePlan::Refuse(RefuseReason::AuthFieldDrift {
            entry_name: entry_name.to_string(),
            fields,
        });
    }

    match pools_holding(config, entry_name).as_slice() {
        [] => {}
        [(pool, entry)] => {
            return SurfacePlan::Nothing {
                entry_name: entry_name.to_string(),
                note: (!entry.accepts_new_logins)
                    .then(|| NothingNote::PinnedPool { pool: pool.clone() }),
            };
        }
        holders => {
            return SurfacePlan::Refuse(RefuseReason::EntryInMultiplePools {
                entry_name: entry_name.to_string(),
                pools: holders.iter().map(|(pool, _)| pool.clone()).collect(),
            });
        }
    }

    // The pool question is answered from the growth pools of the FAMILY,
    // never through the convention's default-seat placement: that
    // derivation also checks the generated entry name, and an unrelated
    // entry squatting `<family>-default` would refuse a placement whose
    // pool half is perfectly determined -- turning a required join into a
    // pool creation, which makes every later login ambiguous.
    let candidates = growth_pools_for_family(config, family);
    let (pool, note) = match candidates.as_slice() {
        // No growth-marked pool: create the family pool, or -- if one
        // already exists and is pinned -- leave it alone. This arm reaches
        // `pool_exists == true` whenever an operator pinned the pool their
        // seat is not yet a member of.
        [] => match pool_action_for_new_entry(config, family, entry_name) {
            Ok(pair) => pair,
            Err(e) => return SurfacePlan::Refuse(RefuseReason::Naming(e)),
        },
        [pool] => (Some(join_action(config, pool, entry_name)), None),
        _ => {
            return SurfacePlan::Refuse(RefuseReason::Naming(SeatNamingError::AmbiguousPool {
                pools: candidates,
            }));
        }
    };
    // With no entry to write and no pool to touch there is no delta at all,
    // so this is a `Nothing` carrying the pinned note rather than an empty
    // write.
    if pool.is_none() {
        return SurfacePlan::Nothing {
            entry_name: entry_name.to_string(),
            note,
        };
    }
    SurfacePlan::Write(SurfaceWrite {
        new_entry: None,
        entry_name: entry_name.to_string(),
        pool,
        note,
    })
}

/// Join `member` to the existing `pool`, rendering the post-join member
/// list. A member the pool already lists is not repeated.
fn join_action(config: &Config, pool: &str, member: &str) -> PoolAction {
    let mut members: Vec<String> = config
        .pools
        .get(pool)
        .map(|entry| entry.members.clone())
        .unwrap_or_default();
    if !members.iter().any(|m| m == member) {
        members.push(member.to_string());
    }
    PoolAction::Join {
        pool: pool.to_string(),
        members,
    }
}

/// Every pool listing `entry_name` as a member, with that pool's entry, in
/// pool-name order.
///
/// Returns ALL holders rather than the first: a provider entry belongs to
/// at most one pool per validation, but this planner is pure over a config
/// it did not gate, and picking one of two holders would let map ordering
/// decide whether the seat looks pinned or growable.
fn pools_holding<'a>(
    config: &'a Config,
    entry_name: &str,
) -> Vec<(String, &'a routectl_router::config::PoolEntry)> {
    config
        .pools
        .iter()
        .filter(|(_, pool)| pool.members.iter().any(|m| m == entry_name))
        .map(|(name, pool)| (name.clone(), pool))
        .collect()
}

/// The required auth fields `entry_name` does not satisfy, or `None` when
/// it satisfies all of them.
///
/// Only the `kind` tag and the auth-selector field are checked -- the
/// fields whose drift makes the entry authenticate on the wrong surface.
/// An operator's `base_url` or header override is legitimate
/// configuration, and the full validation gate owns everything structural.
fn drifted_auth_fields(
    config: &Config,
    family: &str,
    entry_name: &str,
) -> Option<Vec<&'static str>> {
    let required = required_auth_fields(family)?;
    let entry = config.providers.get(entry_name)?;

    let mut drifted = Vec::new();
    if entry.kind_str() != required.kind {
        drifted.push("kind");
    }
    if let Some((key, value)) = required.auth_selector
        && entry_field_str(entry, key).as_deref() != Some(value)
    {
        drifted.push(key);
    }
    (!drifted.is_empty()).then_some(drifted)
}

/// One string-valued field of a provider entry, read through its own
/// serialization so the compared token is exactly what the parser accepts
/// (the auth-selector enums are kebab-case on the wire).
fn entry_field_str(entry: &ProviderEntry, key: &str) -> Option<String> {
    toml::Value::try_from(entry)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

/// Render `plan` as pasteable TOML: the provider entry it creates (when
/// any) followed by the pool block.
///
/// THE single renderer for the shown delta, the decline print and the
/// recovery block, so those three can never disagree. Derived entirely
/// from typed plan data plus the auth-shape table -- never a diff of file
/// bytes, which could carry unrelated literal values in its context lines.
///
/// A plan with no delta ([`SurfacePlan::Nothing`], any
/// [`SurfacePlan::Refuse`]) renders the empty string; the caller prints
/// its own reason line.
#[must_use]
pub fn render_delta(plan: &SurfacePlan) -> String {
    let SurfacePlan::Write(write) = plan else {
        return String::new();
    };
    let mut out = String::new();
    if let Some(entry) = &write.new_entry {
        out.push_str(&entry.render());
        out.push('\n');
    }
    // A skipped pool half renders NOTHING -- not an empty block. The caller
    // prints the pinned note instead, and a rendered block here would show
    // the operator a pool edit that is deliberately not happening.
    if let Some(action) = &write.pool {
        out.push_str(&render_pool_block(action));
    }
    out
}

/// Render one `[pools.<name>]` block. `accepts_new_logins = true` is
/// written ONLY for a pool the plan creates -- a joined pool already
/// carries whatever marker its operator wrote.
fn render_pool_block(action: &PoolAction) -> String {
    let members: Vec<String> = match action {
        PoolAction::Join { members, .. } | PoolAction::Create { members, .. } => {
            members.iter().map(|m| toml_string(m)).collect()
        }
    };
    let mut out = format!("[pools.{}]\n", toml_key(action.pool()));
    out.push_str(&format!("members = [{}]\n", members.join(", ")));
    if matches!(action, PoolAction::Create { .. }) {
        out.push_str("accepts_new_logins = true\n");
    }
    out
}

#[cfg(test)]
#[path = "login_surface_tests.rs"]
mod tests;

// ---------------------------------------------------------------------
// The impure half: snapshot, preflight, plan, render, confirm, commit.
// ---------------------------------------------------------------------

/// What [`surface`] settled on, for the caller and for tests.
///
/// Every variant except [`SurfaceOutcome::Written`] leaves `config.toml`
/// byte-identical, and NONE of them is an error: login already succeeded
/// and the credential is stored, so a refusal, a decline, an absent config
/// or a config this build cannot edit all exit clean. Only a failure AFTER
/// the confirmation was accepted is an `Err`.
#[derive(Debug, PartialEq, Eq)]
pub enum SurfaceOutcome {
    /// The delta was confirmed and committed.
    Written {
        /// The `[providers.<name>]` key that now serves the seat.
        entry_name: String,
    },
    /// Config already reaches the seat; nothing was proposed.
    Nothing,
    /// A plan refusal (ambiguity, naming, auth drift): reported, not written.
    Refused,
    /// The candidate config the delta would produce failed the shared
    /// validation gate, so it was never offered. Distinct from
    /// [`SurfaceOutcome::Refused`]: the planner was willing, the config as a
    /// whole was not.
    Rejected,
    /// The confirmation was declined.
    Declined,
    /// The auto-surface did not run against this config at all: no file, a
    /// file this build must not edit (version out of bounds, a legacy key),
    /// or one that does not parse.
    Skipped(SkipReason),
}

/// Why [`surface`] never got as far as planning.
///
/// Each case prints and exits 0: the operator asked for a credential and
/// got one, and a config routectl must not touch is not a login failure.
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// No config file exists. Login never creates one -- `config init`
    /// owns that decision, including where the file goes and what else it
    /// contains.
    NoConfigFile,
    /// The file exists but could not be read (permissions, a broken symlink,
    /// a device error). Never an error: a login is not the moment to fail on
    /// a config file routectl was not asked to write.
    ///
    /// Carries no detail on purpose. The path embeds the operator's home
    /// directory and the IO string adds nothing actionable, so this reports
    /// the CLASS -- the same wording `redact_config_load_error` collapses
    /// the loader's path-bearing read failure to.
    Unreadable,
    /// The file exists but this build must not edit it: its version is out
    /// of bounds, or it carries a removed key the migrator relocates. The
    /// preflight's own wording -- which already names `config migrate` for
    /// a too-old file -- is carried VERBATIM rather than re-stated, so the
    /// pointer cannot drift from the loader's.
    Unwritable {
        /// The preflight's message, printed as-is.
        detail: String,
    },
    /// The file exists but does not parse, so no delta can be planned
    /// against it. The parse error is redacted before it is carried: toml
    /// echoes the offending line, which may hold credential material.
    Unparseable {
        /// The redacted parse error.
        detail: String,
    },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigFile => write!(
                f,
                "no config file yet, so nothing was written -- run `routectl config init` \
                 to create one, then add the entry below"
            ),
            Self::Unwritable { detail } => write!(f, "{detail} Nothing was written"),
            Self::Unreadable => write!(
                f,
                "{CONFIG_UNREADABLE}, so no config change was proposed. Nothing was written"
            ),
            Self::Unparseable { detail } => write!(
                f,
                "the current config does not parse, so no config change was proposed \
                 ({detail}); fix it (or run `routectl config migrate` if it predates this \
                 build), then add the entry below. Nothing was written"
            ),
        }
    }
}

/// Offer, and on confirmation apply, the config delta a freshly minted seat
/// of `family` labelled `label` implies.
///
/// Sequence: read the file's bytes as the write snapshot -> raw preflights
/// -> parse -> [`plan`] -> print [`render_delta`] -> confirm (`yes`
/// bypasses the PROMPT, never the print) -> commit through
/// `edit_config_toml` against that exact snapshot.
///
/// # Errors
///
/// EXACTLY ONE condition: the commit failed after the confirmation was
/// accepted (a snapshot conflict, a candidate that fails re-validation under
/// the lock, an IO failure). Every pre-acceptance condition -- no config
/// file, an unreadable or unwritable or unparseable one, a plan refusal, a
/// candidate the gate rejects, a decline -- is an `Ok` VALUE, because login
/// already succeeded and the credential is stored. A nonzero exit there
/// would fail every credential-only login run against a config routectl was
/// never asked to write.
///
/// The error message says the credential remains stored, because it does:
/// login ran to completion before this was called and nothing here rolls it
/// back. A conflict is NOT retried against fresh bytes -- the operator
/// confirmed one specific delta against one specific file state.
pub fn surface(
    config_path: &Path,
    family: &str,
    label: Option<&str>,
    yes: bool,
) -> Result<SurfaceOutcome> {
    let snapshot = match read_snapshot(config_path) {
        Ok(bytes) => bytes,
        Err(reason) => return Ok(skip(reason, family, label)),
    };
    surface_against(config_path, snapshot, family, label, yes)
}

/// [`surface`] against an already-captured `snapshot`.
///
/// Split out so the conflict path is reachable without a thread race: the
/// snapshot is what the commit's byte comparison is made against, so
/// passing bytes the file no longer holds IS the out-of-band-write case,
/// through the same code.
fn surface_against(
    config_path: &Path,
    snapshot: Vec<u8>,
    family: &str,
    label: Option<&str>,
    yes: bool,
) -> Result<SurfaceOutcome> {
    let snapshot_text = match String::from_utf8(snapshot.clone()) {
        Ok(text) => text,
        Err(e) => {
            return Ok(skip(
                SkipReason::Unparseable {
                    detail: format!("config is not UTF-8: {e}"),
                },
                family,
                label,
            ));
        }
    };
    if let Err(e) = preflight(&snapshot_text) {
        return Ok(skip(
            SkipReason::Unwritable {
                detail: e.to_string(),
            },
            family,
            label,
        ));
    }
    let prev = match parse_config(&snapshot_text) {
        Ok(config) => config,
        Err(e) => {
            return Ok(skip(
                SkipReason::Unparseable {
                    detail: redact_parse_error(&e),
                },
                family,
                label,
            ));
        }
    };

    // `plan`'s only `Err` is an oauth id with no known provider shape, which
    // the CLI's accepted set (the login registry) cannot produce. It is still
    // reported as a value rather than propagated: this is post-login, and no
    // unreachable planner branch may turn a successful login into a nonzero
    // exit.
    let planned = match plan(&prev, family, label) {
        Ok(planned) => planned,
        Err(e) => {
            println!("\n{e}. Nothing was written.");
            return Ok(SurfaceOutcome::Rejected);
        }
    };
    let write = match &planned {
        SurfacePlan::Nothing { entry_name, note } => {
            if let Some(note) = note {
                println!("\n{note}.");
            } else {
                println!("\nConfig already routes this account; nothing to change.");
            }
            print_availability(&prev, entry_name);
            return Ok(SurfaceOutcome::Nothing);
        }
        SurfacePlan::Refuse(reason) => {
            println!("\n{reason}. Add or fix the entry by hand:");
            println!("\n{}", render_delta_or_block(&planned, family, label));
            return Ok(SurfaceOutcome::Refused);
        }
        SurfacePlan::Write(write) => write,
    };

    let delta = render_delta(&planned);
    println!("\nCredential stored. This config change would route to it:\n\n{delta}");
    // A write whose pool half was skipped still says WHY, next to the delta
    // it is qualifying: the operator sees the entry being added and, in the
    // same breath, that their pinned pool is deliberately not being grown.
    if let Some(note) = &write.note {
        println!("{note}.");
    }

    // Everything from here to the confirmation is PRE-acceptance, so a
    // failure is a reported value, never a nonzero exit: nothing was
    // written and the operator never agreed to anything. Only the commit
    // below can fail nonzero.
    let candidate = apply_delta_text(&snapshot_text, write)
        .map_err(|e| vec![e.to_string()])
        .and_then(|text| gate(&text).map(|next| (text, next)));
    let (_, next) = match candidate {
        Ok(pair) => pair,
        Err(errors) => {
            render_gate_errors(&errors);
            println!("\nAdd the entry by hand once the config is valid:\n\n{delta}");
            return Ok(SurfaceOutcome::Rejected);
        }
    };

    // A new provider entry sets the credential source, and a pool membership
    // change redirects which account serves a model, so this edit is always
    // egress-defining. The collector names the specific fields; the fallback
    // mirrors `provider add`'s, for the case where it comes back empty.
    let mut high = collect_high_consequence_changes(&prev, &next);
    if high.is_empty() {
        high.push("pools.members");
    }
    if !confirm_high_consequence(&high, yes) {
        println!("nothing written. Add it by hand when you are ready:\n\n{delta}");
        return Ok(SurfaceOutcome::Declined);
    }

    let outcome = commit(config_path, &snapshot, &snapshot_text, write).map_err(|e| {
        Error::Config(format!(
            "the credential is stored and remains valid, but the config was NOT changed \
             ({e}). Nothing was rolled back -- the login stands; only `config.toml` is \
             unchanged. Add the entry by hand, or re-run `routectl login` to be offered \
             it again:\n\n{delta}"
        ))
    })?;

    let entry_name = write.entry_name.clone();
    // A skipped pool half audits as no pool rather than as the pool's name:
    // the record must not read as though this edit touched it.
    let pool_field = write
        .pool
        .as_ref()
        .map_or_else(|| "<none>".to_string(), |action| action.pool().to_string());
    tracing::info!(
        surface = "cli",
        verb = "login-config-surface",
        entry = %routectl_core::sanitize_for_log(&entry_name),
        pool = %routectl_core::sanitize_for_log(&pool_field),
        config_changed = outcome == EditOutcome::Modified,
        "login config surface committed",
    );
    println!(
        "config updated: [providers.{}] now serves this account.",
        toml_key(&entry_name)
    );

    // The availability scan reads the config that was just committed, not
    // the pre-write one -- the entry it asks about only exists in the
    // candidate.
    print_availability(&next, &entry_name);
    Ok(SurfaceOutcome::Written { entry_name })
}

/// Print the skip reason plus the block the operator would add by hand.
/// The block comes from the same renderer every other surface prints, so a
/// skipped login and an accepted one can never describe different entries.
fn skip(reason: SkipReason, family: &str, label: Option<&str>) -> SurfaceOutcome {
    println!("\n{reason}.");
    if let Some(block) = provider_block(family, label) {
        println!("\n{}", block.render());
    }
    SurfaceOutcome::Skipped(reason)
}

/// The seat's config bytes, or the reason there are none.
///
/// Every read failure -- absent, unreadable, permission-denied -- is a skip,
/// not an error: this runs AFTER a successful login, and no pre-acceptance
/// condition may turn a stored credential into a nonzero exit.
fn read_snapshot(config_path: &Path) -> std::result::Result<Vec<u8>, SkipReason> {
    match std::fs::read(config_path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(SkipReason::NoConfigFile),
        // The path is deliberately NOT echoed: it embeds the operator's home
        // directory, and this text is printed by a command whose output ends
        // up pasted into bug reports. The class of failure is what makes the
        // message actionable; `config check` names the file it read.
        Err(_) => Err(SkipReason::Unreadable),
    }
}

/// The delta for a plan that has one, else the bare provider block the
/// login's own success print would show. A refusal has no plan-derived
/// delta (nothing is determined), but the operator still needs the entry
/// shape in front of them.
fn render_delta_or_block(plan: &SurfacePlan, family: &str, label: Option<&str>) -> String {
    let delta = render_delta(plan);
    if !delta.is_empty() {
        return delta;
    }
    provider_block(family, label).map_or_else(String::new, |block| block.render())
}

/// Print the routing gap between the seat and a servable request, if any.
fn print_availability(config: &Config, entry_name: &str) {
    let pool = config
        .pools
        .iter()
        .find(|(_, pool)| pool.members.iter().any(|m| m == entry_name))
        .map(|(name, _)| name.clone());
    if let Some(gap) = availability_gap(config, entry_name, pool.as_deref()) {
        println!("\n{gap}\n");
    }
}

/// Apply `write` to a config document, in the deterministic order the
/// commit closure repeats under the lock.
///
/// `accepts_new_logins = true` is set ONLY for a pool this edit CREATES --
/// `upsert_pool_members` deliberately leaves the marker alone, and flipping
/// an operator's absent marker would grow a pool they pinned. A `None` pool
/// touches the `[pools]` table not at all. This mirrors [`render_delta`]
/// exactly; the two must agree or the shown delta is a lie.
fn apply_delta(doc: &mut DocumentMut, write: &SurfaceWrite) -> Result<()> {
    if let Some(entry) = &write.new_entry {
        insert_provider_block(doc, &write.entry_name, entry.entry_table())?;
    }
    let Some(action) = &write.pool else {
        return Ok(());
    };
    let members: Vec<&str> = match action {
        PoolAction::Join { members, .. } | PoolAction::Create { members, .. } => {
            members.iter().map(String::as_str).collect()
        }
    };
    upsert_pool(doc, action.pool(), &members);
    if matches!(action, PoolAction::Create { .. }) {
        set_accepts_new_logins(doc, action.pool());
    }
    Ok(())
}

/// Mark a pool this edit created as one future logins may grow.
fn set_accepts_new_logins(doc: &mut DocumentMut, pool: &str) {
    if let Some(block) = doc
        .get_mut("pools")
        .and_then(Item::as_table_like_mut)
        .and_then(|pools| pools.get_mut(pool))
        .and_then(Item::as_table_like_mut)
    {
        block.insert("accepts_new_logins", toml_edit::value(true));
    }
}

/// The candidate config text, for the pre-lock gate.
fn apply_delta_text(snapshot_text: &str, write: &SurfaceWrite) -> Result<String> {
    let mut doc = parse_document(snapshot_text)?;
    apply_delta(&mut doc, write)?;
    Ok(doc.to_string())
}

/// Re-read under the advisory lock against `snapshot`, re-apply the SAME
/// deterministic edit, re-gate, and commit atomically.
fn commit(
    config_path: &Path,
    snapshot: &[u8],
    snapshot_text: &str,
    write: &SurfaceWrite,
) -> std::result::Result<EditOutcome, String> {
    let result = routectl_router::edit_config_toml::<RelockValidationError, _>(
        config_path,
        snapshot,
        |doc| {
            apply_delta(doc, write).map_err(|_| RelockValidationError)?;
            let text = doc.to_string();
            if text == snapshot_text {
                return Ok(EditOutcome::Unchanged);
            }
            match gate(&text) {
                Ok(_) => Ok(EditOutcome::Modified),
                Err(_) => Err(RelockValidationError),
            }
        },
    )
    .map_err(write_failure_class)?;
    Ok(result.outcome)
}

/// The path-free class of a write failure.
///
/// `ConfigWriteError`'s own `Display` names the file in every variant, which
/// is right for `config set` (the operator passed that path on argv) and
/// wrong here: login resolves the path itself, so echoing it discloses the
/// operator's home directory in output that gets pasted into bug reports.
/// Each class already implies the remedy, so nothing actionable is lost.
fn write_failure_class(err: ConfigWriteError<RelockValidationError>) -> String {
    match err {
        ConfigWriteError::Conflict { .. } => {
            "the config file changed on disk after this change was shown, so it was not \
             applied -- another writer got there first"
                .to_string()
        }
        ConfigWriteError::Io { .. } => "the config file could not be written".to_string(),
        ConfigWriteError::Parse { .. } => {
            "the config file no longer parses, so it was not modified".to_string()
        }
        ConfigWriteError::Edit(_) => {
            "the resulting config failed validation under the write lock, so it was not \
             applied"
                .to_string()
        }
    }
}

#[cfg(test)]
#[path = "login_surface_command_tests.rs"]
mod command_tests;
