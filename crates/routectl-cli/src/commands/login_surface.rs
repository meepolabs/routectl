//! The pure planner behind `routectl login`'s config auto-surface: what
//! config delta a freshly minted seat implies, and how that delta renders
//! as pasteable TOML.
//!
//! Both halves are PURE over a parsed [`Config`] -- no IO, no credential
//! store, no clock. The command layer owns the snapshot, the confirmation
//! and the write; everything judgment-bearing happens here so it is
//! testable as a table.
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

use routectl_auth::SecretRef;
use routectl_core::{Error, Result};
use routectl_router::seat_naming::{
    SeatNamingError, growth_pools_for_family, plan_new_seat, plan_pool_materialization,
    seat_secret_ref,
};
use routectl_router::{Config, ProviderEntry};

use super::login_provider_block::{
    ProviderBlock, provider_block, required_auth_fields, toml_key, toml_string,
};

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

/// One config delta: at most one new provider entry, plus one pool action.
#[derive(Debug)]
pub struct SurfaceWrite {
    /// The provider entry to create, or `None` when an entry already
    /// consumes the ref and only its pool membership changes.
    pub new_entry: Option<ProviderBlock>,
    /// The `[providers.<name>]` key that ends up serving the seat --
    /// either `new_entry`'s name or the matched entry's.
    pub entry_name: String,
    /// What happens to the pool.
    pub pool: PoolAction,
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
/// the pool action its family's config state implies.
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

    let pool = match placement.pool_name {
        Some(pool) => join_action(config, &pool, &entry_name),
        None => match create_action(config, family, &entry_name) {
            Ok(action) => action,
            Err(e) => return Ok(SurfacePlan::Refuse(RefuseReason::Naming(e))),
        },
    };
    Ok(SurfacePlan::Write(SurfaceWrite {
        new_entry: Some(block),
        entry_name,
        pool,
    }))
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
    let pool = match candidates.as_slice() {
        [] => match create_action(config, family, entry_name) {
            Ok(action) => action,
            Err(e) => return SurfacePlan::Refuse(RefuseReason::Naming(e)),
        },
        [pool] => join_action(config, pool, entry_name),
        _ => {
            return SurfacePlan::Refuse(RefuseReason::Naming(SeatNamingError::AmbiguousPool {
                pools: candidates,
            }));
        }
    };
    SurfacePlan::Write(SurfaceWrite {
        new_entry: None,
        entry_name: entry_name.to_string(),
        pool,
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

/// Create the family's pool around `member`.
///
/// The pool name and its namespace collision check come from
/// `plan_pool_materialization` with NO seats requested: the entry already
/// exists (or is planned separately), so only the pool half applies.
fn create_action(
    config: &Config,
    family: &str,
    member: &str,
) -> std::result::Result<PoolAction, SeatNamingError> {
    let plan = plan_pool_materialization(config, family, [])?;
    Ok(PoolAction::Create {
        pool: plan.pool_name,
        members: vec![member.to_string()],
    })
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
    out.push_str(&render_pool_block(&write.pool));
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
