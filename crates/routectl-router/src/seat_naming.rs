//! The single naming convention for OAuth-account config entries and the
//! pools that group them.
//!
//! Two writers must agree byte-for-byte on the names they generate: the
//! `config migrate` pass that materializes today's implicit multi-seat refs
//! into explicit accounts, and the login path that later grows the same
//! pool. Reconciliation of a login against existing config is by
//! `oauth://` ref, which is only correct while both writers derive the same
//! name from the same inputs -- so the derivation lives here once, as pure
//! functions over a `Config` and a seat label. Nothing in this module reads
//! the credential store.
//!
//! The convention:
//!
//! - the POOL takes the plain provider-family name (`pools.anthropic`),
//! - the account entry for the DEFAULT seat takes `<family>-default`,
//! - the account entry for a labelled seat takes `<family>-<label>`.
//!
//! One consequence has its own derivation ([`family_default_rename`]): a
//! provider entry NAMED after its own family holds the name the pool must
//! take, so materializing that family's seats moves the entry onto
//! `<family>-default` -- the name its own bare (default-seat) ref would
//! generate anyway.
//!
//! Mapping is EXACT or refused. A label is never normalized, truncated, or
//! case-folded to make it fit a name: a label that cannot appear verbatim
//! in a config entry name, two labels that would generate one name, and a
//! generated name already held by an unrelated entry are each a typed
//! refusal, because a lossy rewrite here silently points a config entry at
//! the wrong credential.

use std::collections::BTreeSet;

use routectl_auth::SecretRef;

use crate::config::Config;

/// Why a name could not be derived. Every variant is fail-closed: the
/// caller writes nothing and reports the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SeatNamingError {
    /// A provider family or seat label carries a character that cannot
    /// appear verbatim in a generated config entry name. Refused rather
    /// than normalized: rewriting the label would produce a name that no
    /// longer identifies the seat it came from.
    UnusableToken {
        /// What the token names (`provider family` / `seat label`).
        kind: &'static str,
        /// The offending token.
        token: String,
    },
    /// A seat label whose generated name would be indistinguishable from
    /// the default seat's (`<family>-default`).
    ReservedLabel {
        /// The offending label.
        label: String,
    },
    /// Two seats of one family generate the same entry name.
    DuplicateGeneratedName {
        /// The name both seats claim.
        name: String,
    },
    /// A generated entry name is already held by a provider entry that
    /// authenticates with a DIFFERENT credential.
    EntryNameTaken {
        /// The generated name.
        name: String,
    },
    /// The pool name is already held by a provider entry or a model
    /// nickname -- providers, pools, and nicknames share one namespace on
    /// a `[models.X] provider` value.
    PoolNameTaken {
        /// The generated pool name.
        name: String,
    },
    /// More than one growth-marked pool serves this provider family, so
    /// which pool a new seat joins is not determined by config.
    AmbiguousPool {
        /// The candidate pool names, sorted.
        pools: Vec<String>,
    },
}

impl std::fmt::Display for SeatNamingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnusableToken { kind, token } => write!(
                f,
                "{kind} `{token}` cannot be used verbatim in a config entry name; \
                 only ASCII letters, digits, `-` and `_` are usable"
            ),
            Self::ReservedLabel { label } => write!(
                f,
                "seat label `{label}` is reserved: its entry name would collide \
                 with the default seat's"
            ),
            Self::DuplicateGeneratedName { name } => {
                write!(f, "two seats both generate the entry name `{name}`")
            }
            Self::EntryNameTaken { name } => write!(
                f,
                "entry name `{name}` is already used by a provider entry with a \
                 different credential"
            ),
            Self::PoolNameTaken { name } => write!(
                f,
                "pool name `{name}` is already used by a provider entry or a model \
                 nickname; providers, pools and model nicknames share one namespace"
            ),
            Self::AmbiguousPool { pools } => write!(
                f,
                "{} pools accept new logins for this provider ({}); which one a new \
                 seat joins is not determined by config",
                pools.len(),
                pools.join(", ")
            ),
        }
    }
}

impl std::error::Error for SeatNamingError {}

/// The seat label reserved for the default seat's generated entry name.
const DEFAULT_SEAT_SUFFIX: &str = "default";

/// One account provider entry the convention derives for one seat.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GeneratedAccount {
    /// The seat this entry authenticates as: `None` is the default seat.
    pub label: Option<String>,
    /// The `[providers.<name>]` key the entry takes.
    pub entry_name: String,
    /// The credential ref the entry carries.
    pub secret_ref: String,
    /// Whether a provider entry under `entry_name` already exists and
    /// already carries `secret_ref`, making the write a no-op for it.
    pub already_present: bool,
}

/// The full set of names one provider family's seats materialize into.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolMaterialization {
    /// The `[pools.<name>]` key the pool takes.
    pub pool_name: String,
    /// Whether that pool block already exists in the config.
    pub pool_exists: bool,
    /// One account per requested seat, in the order requested.
    pub accounts: Vec<GeneratedAccount>,
}

/// One provider entry's full move into its family's pool: the pool and
/// accounts its seats materialize into, plus the rename the entry itself
/// needs when it holds the name the pool must take.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntryMaterialization {
    /// The `[providers.<name>]` key the source entry moves to, or `None`
    /// when it keeps the name it has. Set only for an entry named after its
    /// own provider family -- see [`family_default_rename`].
    pub renamed_entry: Option<String>,
    /// The pool and per-seat accounts the family materializes into.
    pub pool: PoolMaterialization,
}

/// Where a newly logged-in seat lands: the entry name it takes and the
/// growth-marked pool that would receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewSeatPlacement {
    /// The account entry the seat materializes as.
    pub account: GeneratedAccount,
    /// The growth-marked pool that would gain the member, or `None` when
    /// no existing pool accepts new logins for this family.
    pub pool_name: Option<String>,
    /// Whether that pool already lists this entry as a member, making the
    /// membership write a no-op.
    pub already_member: bool,
}

/// The `[pools.<name>]` key for a provider family: the plain family name.
///
/// Refuses a family token that cannot appear verbatim in a config key. Does
/// NOT check the config for a collision -- [`plan_pool_materialization`]
/// does that against the namespace providers, pools and model nicknames
/// share.
pub fn pool_name(family: &str) -> Result<String, SeatNamingError> {
    check_token("provider family", family)?;
    Ok(family.to_string())
}

/// The `[providers.<name>]` key for one seat of a provider family:
/// `<family>-default` for the default seat, `<family>-<label>` otherwise.
///
/// Refuses a family or label that cannot appear verbatim in a config key,
/// and refuses the label `default` outright -- it would generate the
/// default seat's own name and so silently alias two distinct seats.
pub fn account_entry_name(family: &str, label: Option<&str>) -> Result<String, SeatNamingError> {
    check_token("provider family", family)?;
    match label {
        None => Ok(format!("{family}-{DEFAULT_SEAT_SUFFIX}")),
        Some(label) => {
            check_token("seat label", label)?;
            if label == DEFAULT_SEAT_SUFFIX {
                return Err(SeatNamingError::ReservedLabel {
                    label: label.to_string(),
                });
            }
            Ok(format!("{family}-{label}"))
        }
    }
}

/// The account-entry name a provider entry must move to so the POOL can
/// claim the plain family name, or `None` when the entry does not hold that
/// name.
///
/// A config authored before pools existed commonly names its single OAuth
/// entry after the family itself (`[providers.anthropic]` carrying
/// `oauth://anthropic`). The pool needs that exact name, and providers,
/// pools and model nicknames share one namespace -- so materializing the
/// family's seats means moving the entry onto the DEFAULT-seat account name.
/// Its bare ref IS the default seat's credential, so `<family>-default` is
/// the only name it can take, and deriving it here rather than at the two
/// call sites is what keeps the migrator and the login writer in agreement.
///
/// PURE: it answers what the convention requires, not whether the target
/// name is free. Each caller checks occupancy against the namespace it can
/// see (a typed `Config` for [`plan_pool_materialization`], the raw document
/// for the pure migration rung) and refuses rather than displacing an entry.
///
/// # Errors
///
/// [`SeatNamingError::UnusableToken`] for a family token that cannot appear
/// verbatim in a config key.
pub fn family_default_rename(
    entry_name: &str,
    family: &str,
) -> Result<Option<String>, SeatNamingError> {
    if entry_name == pool_name(family)? {
        return account_entry_name(family, None).map(Some);
    }
    Ok(None)
}

/// The `oauth://` ref for one seat of a provider family.
pub fn seat_secret_ref(family: &str, label: Option<&str>) -> String {
    match label {
        None => format!("oauth://{family}"),
        Some(label) => format!("oauth://{family}#{label}"),
    }
}

/// Derive every name one provider family's seats materialize into, checked
/// against `config`.
///
/// `labels` names the seats, `None` being the default seat; order is
/// preserved in the returned accounts. Refuses when any generated name is
/// unusable, when two seats claim one name, when a generated entry name is
/// held by an entry carrying a different credential, or when the pool name
/// is held by a provider entry or a model nickname.
///
/// An entry that already exists AND already carries the right ref is
/// reported with `already_present` set rather than refused: re-running the
/// derivation over a config it already produced is a no-op, which is what
/// makes the migration and the login writer agree.
pub fn plan_pool_materialization<'a>(
    config: &Config,
    family: &str,
    labels: impl IntoIterator<Item = Option<&'a str>>,
) -> Result<PoolMaterialization, SeatNamingError> {
    plan_pool_names(config, family, labels, None)
}

/// Derive one provider entry's full move into its family's pool: the pool
/// and per-seat accounts of [`plan_pool_materialization`], plus the rename
/// `entry_name` itself needs when it holds the plain family name the pool
/// must take.
///
/// This is the derivation the migration's store-aware phase uses, and the
/// only one that can answer the family-named shape (`[providers.anthropic]`
/// carrying `oauth://anthropic`): the entry vacates the family name for the
/// pool and moves to `<family>-default`, so the pool-name occupancy check
/// discounts the entry that is vacating.
///
/// # Errors
///
/// Every class [`plan_pool_materialization`] refuses on, plus
/// [`SeatNamingError::EntryNameTaken`] when the rename target
/// `<family>-default` is already held by a DIFFERENT provider entry -- the
/// rename would have to displace a second credential, which is never a
/// rewrite this module performs.
pub fn plan_entry_materialization<'a>(
    config: &Config,
    entry_name: &str,
    family: &str,
    labels: impl IntoIterator<Item = Option<&'a str>>,
) -> Result<EntryMaterialization, SeatNamingError> {
    let renamed_entry = family_default_rename(entry_name, family)?;
    if let Some(renamed) = &renamed_entry
        && config.providers.contains_key(renamed)
    {
        return Err(SeatNamingError::EntryNameTaken {
            name: renamed.clone(),
        });
    }
    let vacating = renamed_entry.as_ref().map(|_| entry_name);
    let pool = plan_pool_names(config, family, labels, vacating)?;
    Ok(EntryMaterialization {
        renamed_entry,
        pool,
    })
}

/// Shared body of the two derivations. `vacating` names the provider entry
/// that is being renamed away from the pool name, and so must not count as
/// holding it.
fn plan_pool_names<'a>(
    config: &Config,
    family: &str,
    labels: impl IntoIterator<Item = Option<&'a str>>,
    vacating: Option<&str>,
) -> Result<PoolMaterialization, SeatNamingError> {
    let pool = pool_name(family)?;
    let pool_exists = config.pools.contains_key(&pool);
    let held_by_provider = config.providers.contains_key(&pool) && vacating != Some(pool.as_str());
    if !pool_exists && (held_by_provider || config.models.contains_key(&pool)) {
        return Err(SeatNamingError::PoolNameTaken { name: pool });
    }

    let mut accounts: Vec<GeneratedAccount> = Vec::new();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    for label in labels {
        let account = plan_account(config, family, label)?;
        if !claimed.insert(account.entry_name.clone()) {
            return Err(SeatNamingError::DuplicateGeneratedName {
                name: account.entry_name,
            });
        }
        accounts.push(account);
    }

    Ok(PoolMaterialization {
        pool_name: pool,
        pool_exists,
        accounts,
    })
}

/// Where a new seat of `family` labelled `label` would land in `config`.
///
/// The pool is the one growth-marked (`accepts_new_logins`) pool whose
/// members authenticate against this provider family. Zero such pools
/// yields `pool_name: None` (the caller proposes creating one, which is
/// where the pool-name derivation applies); two or more is
/// [`SeatNamingError::AmbiguousPool`], because config does not determine
/// which pool grows. The pool is found by MEMBERSHIP, not by name, so a
/// pool the operator renamed still receives its family's new seats.
pub fn plan_new_seat(
    config: &Config,
    family: &str,
    label: Option<&str>,
) -> Result<NewSeatPlacement, SeatNamingError> {
    let account = plan_account(config, family, label)?;

    let candidates = growth_pools_for_family(config, family);
    let pool_name = match candidates.len() {
        0 => None,
        1 => Some(candidates[0].clone()),
        _ => return Err(SeatNamingError::AmbiguousPool { pools: candidates }),
    };
    let already_member = pool_name.as_ref().is_some_and(|pool| {
        config
            .pools
            .get(pool)
            .is_some_and(|entry| entry.members.contains(&account.entry_name))
    });

    Ok(NewSeatPlacement {
        account,
        pool_name,
        already_member,
    })
}

/// The account entry one seat materializes as, checked against the entries
/// `config` already carries.
fn plan_account(
    config: &Config,
    family: &str,
    label: Option<&str>,
) -> Result<GeneratedAccount, SeatNamingError> {
    let entry_name = account_entry_name(family, label)?;
    let secret_ref = seat_secret_ref(family, label);
    let already_present = match config.providers.get(&entry_name) {
        None => false,
        Some(existing) if entry_carries_ref(existing, &secret_ref) => true,
        Some(_) => return Err(SeatNamingError::EntryNameTaken { name: entry_name }),
    };
    Ok(GeneratedAccount {
        label: label.map(str::to_owned),
        entry_name,
        secret_ref,
        already_present,
    })
}

/// Names of the growth-marked pools whose members authenticate against
/// `family`, sorted. A pool with no `accepts_new_logins` marker is pinned
/// and never listed; a pool with no member referencing this family does not
/// serve it.
pub fn growth_pools_for_family(config: &Config, family: &str) -> Vec<String> {
    config
        .pools
        .iter()
        .filter(|(_, pool)| pool.accepts_new_logins)
        .filter(|(_, pool)| {
            pool.members.iter().any(|member| {
                config
                    .providers
                    .get(member)
                    .is_some_and(|entry| entry_serves_family(entry, family))
            })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Whether a provider entry carries `secret_ref` as one of its credential
/// refs.
fn entry_carries_ref(entry: &crate::config::ProviderEntry, secret_ref: &str) -> bool {
    entry.secret_uris().contains(&secret_ref)
}

/// Whether a provider entry authenticates against `family`'s OAuth
/// credentials, at any seat.
fn entry_serves_family(entry: &crate::config::ProviderEntry, family: &str) -> bool {
    entry.secret_uris().iter().any(|uri| {
        matches!(
            SecretRef::parse(uri),
            Ok(SecretRef::OAuth { provider, .. }) if provider == family
        )
    })
}

/// Whether every byte of `token` can appear verbatim in a generated config
/// entry name. Deliberately narrow: a bare TOML key and a dotted `config
/// set` path both address these names, so a token outside this set is
/// refused rather than quoted or rewritten.
fn check_token(kind: &'static str, token: &str) -> Result<(), SeatNamingError> {
    let usable = !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if usable {
        Ok(())
    } else {
        Err(SeatNamingError::UnusableToken {
            kind,
            token: token.to_string(),
        })
    }
}

#[cfg(test)]
#[path = "seat_naming_tests.rs"]
mod seat_naming_tests;
