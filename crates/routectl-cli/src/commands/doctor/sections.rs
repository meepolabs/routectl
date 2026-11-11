//! Doctor section producers.

use std::fmt::Write as _;

use routectl_auth::oauth::types::TokenRecord;
use routectl_core::sanitize_for_log_with_cap;
use routectl_router::{
    ActivationEntry, ActivationStatus, CONFIG_VERSION_REMEDIATION, CURRENT_CONFIG_VERSION,
    ConfigVersionError, FillSource, Finding, PriceTableDates, PricingConfig, Status, TuningRow,
    UnresolvedReason, compute_activation, preflight_config_version,
};

use routectl_router::prices::{PriceSources, TableDate};

use crate::commands::config::MAX_REPORTED_LINE_CHARS;
use crate::commands::config_effective::{tuning_cells, tuning_kind};
use crate::commands::pricing::{is_complete_or_absent, missing_equivalence_dimensions};
use crate::commands::probe::{login_id_for, probe_finding};
use crate::commands::seat_report::{
    PoolHealth, PoolRow, describe_pool, describe_row, pool_rows, safe, stored_seat_pool_rows,
};
use crate::server::reload_failure::ReloadFailureSnapshot;

use super::gather::{SecretCheck, SecretPresence};
use super::served::{
    SeatFix, parked_seat_finding, unknown_provider_sentence, unusable_seat_finding,
};
use super::{DoctorContext, KnobRow, LegacyFolder, PricingRow};

/// Probe section: one finding per configured provider, mapped from its
/// read-only reachability outcome through the shared `probe_finding` seam so
/// the doctor battery and `provider probe` never diverge on status, detail,
/// or remediation.
pub(super) fn section_probe(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.probe_results
        .iter()
        .map(|(name, outcome)| probe_finding(name, outcome, login_id_for(&ctx.config, name)))
        .collect()
}

pub(super) fn section_inventory(ctx: &DoctorContext) -> Vec<Finding> {
    let state = compute_activation(&ctx.probes, &ctx.config);
    state
        .iter()
        .map(|(id, entry)| inventory_finding(id, entry))
        .collect()
}

fn inventory_finding(id: &str, entry: &ActivationEntry) -> Finding {
    let (status, detail, remediation) = match entry.status {
        ActivationStatus::Activated => (
            Status::Pass,
            format!("credential present and usable ({})", entry.provider_kind),
            None,
        ),
        ActivationStatus::Unresolved { reason } => {
            if entry.referenced_by_aliases {
                (
                    Status::Warn,
                    format!(
                        "a configured route depends on this provider but it is not usable ({reason})"
                    ),
                    Some(inventory_remediation(id, reason)),
                )
            } else {
                (
                    Status::Pass,
                    format!("not activated ({reason}); no configured route depends on it"),
                    None,
                )
            }
        }
        _ => (
            Status::Warn,
            "activation state not recognized by this build".to_string(),
            Some("upgrade routectl to a newer build".to_string()),
        ),
    };
    Finding {
        section: "inventory",
        name: id.to_string(),
        status,
        detail,
        remediation,
    }
}

fn inventory_remediation(id: &str, reason: UnresolvedReason) -> String {
    match reason {
        UnresolvedReason::NotCataloged => {
            "no `[providers.X]` block in this build can declare this provider's kind; \
             rebuild routectl with the owning cargo feature enabled"
                .to_string()
        }
        _ => format!("run `routectl login {id}` to activate this provider"),
    }
}

pub(super) fn section_version(ctx: &DoctorContext) -> Vec<Finding> {
    let binary = ctx.binary_version;
    // A config the daemon accepted already passed the version preflight at
    // load, so its own version is the served schema.
    let version = match (ctx.accepted_config_version, &ctx.raw_config) {
        (Some(accepted), _) => Ok(accepted),
        (None, Some(raw)) => preflight_config_version(raw),
        (None, None) => {
            return vec![Finding {
                section: "version",
                name: "config schema".to_string(),
                status: Status::Warn,
                detail: format!("config file could not be read; binary routectl {binary}"),
                remediation: Some("run `routectl init` to create a config".to_string()),
            }];
        }
    };

    let finding = match version {
        Ok(found) => {
            if let Some(err) = &ctx.config_load_error {
                // The preflight only reads the `version` key and returns Ok
                // on a TOML syntax error or an unknown/legacy key, deferring
                // the real cause to the typed load doctor never runs. When
                // that load failed for a non-version reason, the config is
                // broken -- report it rather than pass on the raw version.
                Finding {
                    section: "version",
                    name: "config schema".to_string(),
                    status: Status::Fail,
                    detail: format!("config could not be loaded: {err}"),
                    remediation: Some(
                        "resolve the config error above, then re-run `routectl doctor`".to_string(),
                    ),
                }
            } else {
                Finding {
                    section: "version",
                    name: "config schema".to_string(),
                    status: Status::Pass,
                    detail: format!(
                        "config schema v{found}; binary routectl {binary} (expects v{CURRENT_CONFIG_VERSION})"
                    ),
                    remediation: None,
                }
            }
        }
        Err(ConfigVersionError::TooOld { found, supported }) => Finding {
            section: "version",
            name: "config schema".to_string(),
            status: Status::Fail,
            detail: format!("config schema v{found}, this binary expects v{supported}"),
            remediation: Some(CONFIG_VERSION_REMEDIATION.to_string()),
        },
        Err(ConfigVersionError::Missing { supported }) => Finding {
            section: "version",
            name: "config schema".to_string(),
            status: Status::Fail,
            detail: format!("config schema version missing, this binary expects v{supported}"),
            remediation: Some(CONFIG_VERSION_REMEDIATION.to_string()),
        },
        Err(ConfigVersionError::TooNew(err)) => Finding {
            section: "version",
            name: "config schema".to_string(),
            status: Status::Fail,
            detail: format!(
                "config schema v{}, newer than the v{} this binary supports",
                err.found, err.supported
            ),
            remediation: Some(
                "upgrade routectl to a build that understands this config".to_string(),
            ),
        },
    };
    vec![finding]
}

/// One remediation string for every validator advisory. The warning texts
/// carry their own fix instructions, so a per-warning remediation would
/// duplicate them; this points at the advisory itself instead.
const WARNING_REMEDIATION: &str =
    "review the advisory, then re-run `routectl doctor` after fixing the config";

/// Config-check section: the STATIC validator suite (reused via
/// `validation_report`) rendered as findings -- both halves, errors as Fail
/// and advisories as Warn -- plus a read-only secret-presence scan. Resolves
/// no secret value and refreshes no credential; every message names the
/// scheme, never the value or ref.
pub(super) fn section_config(ctx: &DoctorContext) -> Vec<Finding> {
    let mut findings: Vec<Finding> = ctx
        .reload_failure
        .map(reload_failure_finding)
        .into_iter()
        .chain(leftover_catalog_files_finding(&ctx.leftover_catalog_files))
        .collect();
    findings.extend(ctx.legacy_folders.iter().filter_map(legacy_folder_finding));
    if ctx.config_load_error.is_some() {
        findings.push(Finding {
            section: "config",
            name: "validation".to_string(),
            status: Status::Warn,
            detail: "config validation skipped: config could not be parsed".to_string(),
            remediation: Some(
                "resolve the config error above, then re-run `routectl doctor`".to_string(),
            ),
        });
        findings.extend(ctx.secret_checks.iter().map(secret_finding));
        return findings;
    }
    let report = crate::commands::config::validation_report(&ctx.config, ctx.raw_config.as_deref());
    if report.errors.is_empty() && report.warnings.is_empty() {
        findings.push(Finding {
            section: "config",
            name: "validation".to_string(),
            status: Status::Pass,
            detail: "config passes the static validator suite".to_string(),
            remediation: None,
        });
    }
    for err in report.errors {
        findings.push(Finding {
            section: "config",
            name: "validation".to_string(),
            status: Status::Fail,
            // Every validator formats operator-written table keys into its
            // message, so this one render point control-char-filters the
            // whole suite's output: a key bearing a newline plus an ANSI
            // sequence would otherwise forge a fabricated finding line in
            // the human render. The cap is shared with `config check` so a
            // long-but-legitimate advisory is not truncated on one surface
            // and whole on the other.
            detail: sanitize_for_log_with_cap(&err, MAX_REPORTED_LINE_CHARS),
            remediation: Some(
                "fix the config error above, then re-run `routectl doctor`".to_string(),
            ),
        });
    }
    for warning in report.warnings {
        findings.push(Finding {
            section: "config",
            name: "validation".to_string(),
            status: Status::Warn,
            detail: sanitize_for_log_with_cap(&warning, MAX_REPORTED_LINE_CHARS),
            remediation: Some(WARNING_REMEDIATION.to_string()),
        });
    }
    findings.extend(ctx.secret_checks.iter().map(secret_finding));
    findings
}

/// Files left in the old config folder, or a folder that could not be listed.
/// The folder is named only by its home-free display: the served panel must
/// not carry the account's home directory. Basenames are operator-controlled,
/// so each is control-char-filtered before it reaches the one-line human
/// render.
fn legacy_folder_finding(folder: &LegacyFolder) -> Option<Finding> {
    // Named by variable, without `~`, `$` or `%`: one wording has to be
    // right for every old folder on every OS, home path never printed.
    const NEW_FOLDER: &str = "the folder routectl reads now (routectl under XDG_CONFIG_HOME \
                              when that is an absolute path, else .config/routectl in your \
                              home folder)";
    let (detail, remediation) = match folder {
        LegacyFolder::Absent => return None,
        LegacyFolder::Entries {
            display,
            names,
            more,
            has_config,
        } => (
            legacy_entries_detail(display, names, *more, *has_config),
            format!(
                "move what you still need from {display} to {NEW_FOLDER}, then remove the old folder"
            ),
        ),
        LegacyFolder::InspectError { display, kind } => (
            format!(
                "could not list {display} ({kind}), so files routectl no longer reads may be \
                 left there"
            ),
            format!("check that {display} is a readable folder"),
        ),
    };
    Some(Finding {
        section: "config",
        name: "legacy folder".to_string(),
        status: Status::Warn,
        detail,
        remediation: Some(remediation),
    })
}

fn legacy_entries_detail(display: &str, names: &[String], more: usize, has_config: bool) -> String {
    let listed = names
        .iter()
        .map(|name| sanitize_for_log_with_cap(name, MAX_REPORTED_LINE_CHARS))
        .collect::<Vec<_>>()
        .join(", ");
    let mut detail = format!("{display} still holds {listed}");
    if more > 0 {
        let _ = write!(detail, " and {more} more");
    }
    detail.push_str("; routectl does not read them");
    if has_config {
        detail.push_str(", and its config.toml is not the config in use");
    }
    detail
}

/// The daemon refused its last config reload and still serves the
/// config it loaded before. Names only the closed class and the age: the
/// loader's own error can carry config values and paths, and the CLI doctor,
/// which reads the files itself, is where the operator sees it.
fn reload_failure_finding(failure: ReloadFailureSnapshot) -> Finding {
    Finding {
        section: "config",
        name: "reload".to_string(),
        status: Status::Warn,
        detail: format!(
            "last config reload rejected ({}) {} ago; the daemon is serving the previously loaded config",
            failure.class.as_str(),
            format_age(failure.age_ms),
        ),
        remediation: Some(
            "run `routectl config check` to see why the on-disk config was refused, \
             fix it, and save it again to trigger a reload"
                .to_string(),
        ),
    }
}

/// The catalog files still in the config folder that routectl no longer
/// reads; `None` when there are none. Informational: a leftover file changes
/// nothing, so the finding is a Pass and never moves the exit code.
fn leftover_catalog_files_finding(present: &[&str]) -> Option<Finding> {
    if present.is_empty() {
        return None;
    }
    Some(Finding {
        section: "config",
        name: "leftover catalog files".to_string(),
        status: Status::Pass,
        detail: format!(
            "the config folder still holds {}; routectl no longer reads them",
            present.join(", ")
        ),
        remediation: Some(
            "prices now come from `routectl prices refresh` and `[models.X.pricing]`; \
             remove these files when you no longer need them"
                .to_string(),
        ),
    })
}

const MS_PER_SEC: i64 = 1_000;
const SECS_PER_MIN: i64 = 60;
const SECS_PER_HOUR: i64 = 60 * SECS_PER_MIN;
const SECS_PER_DAY: i64 = 24 * SECS_PER_HOUR;

/// A coarse human age: whole seconds, minutes, hours, or days.
fn format_age(age_ms: i64) -> String {
    let secs = age_ms / MS_PER_SEC;
    match secs {
        ..SECS_PER_MIN => format!("{secs}s"),
        SECS_PER_MIN..SECS_PER_HOUR => format!("{}m", secs / SECS_PER_MIN),
        SECS_PER_HOUR..SECS_PER_DAY => format!("{}h", secs / SECS_PER_HOUR),
        _ => format!("{}d", secs / SECS_PER_DAY),
    }
}

pub(super) fn secret_finding(check: &SecretCheck) -> Finding {
    let (status, detail, remediation) = match check.presence {
        SecretPresence::Present => (
            Status::Pass,
            format!("{} reference resolves", check.scheme),
            None,
        ),
        SecretPresence::Missing => (
            Status::Warn,
            format!("{} reference does not resolve", check.scheme),
            Some(missing_remediation(check)),
        ),
        SecretPresence::Expired => (
            Status::Warn,
            format!("{} credential is expired", check.scheme),
            Some(check_login_remediation(check)),
        ),
        SecretPresence::StoreUnavailable => (
            Status::Warn,
            "oauth credential store is unavailable".to_string(),
            Some(check_login_remediation(check)),
        ),
        SecretPresence::UnknownOauthProvider => (
            Status::Warn,
            "oauth provider is not recognized by this build".to_string(),
            Some(unknown_oauth_provider_remediation(check)),
        ),
        SecretPresence::Unreadable => (
            Status::Warn,
            format!("{} reference exists but is not readable", check.scheme),
            Some("check the permissions on the referenced secret file".to_string()),
        ),
        SecretPresence::Invalid => (
            Status::Fail,
            format!("{} reference could not be parsed", check.scheme),
            Some("correct this provider's secret reference".to_string()),
        ),
    };
    Finding {
        section: "config",
        name: check.provider.clone(),
        status,
        detail,
        remediation,
    }
}

fn missing_remediation(check: &SecretCheck) -> String {
    match check.scheme {
        "oauth://" => check_login_remediation(check),
        "env://" => "set the environment variable this provider references".to_string(),
        "file://" => "create the secret file this provider references".to_string(),
        _ => "provide a resolvable secret reference for this provider".to_string(),
    }
}

fn unknown_oauth_provider_remediation(check: &SecretCheck) -> String {
    check.oauth_id.as_deref().map_or_else(
        || "correct the provider id in this oauth:// reference".to_string(),
        |provider| unknown_provider_sentence(&check.provider, provider),
    )
}

fn check_login_remediation(check: &SecretCheck) -> String {
    check.oauth_id.as_ref().map_or_else(
        || oauth_login_remediation(None, None),
        |provider| {
            SeatFix::for_oauth_ref(&check.provider, provider.clone(), check.oauth_label.clone())
                .remediation()
        },
    )
}

/// The remediation line naming [`oauth_login_command`] for an
/// `oauth://<provider>[#<label>]` ref, or the generic form when the ref names
/// no provider.
pub(super) fn oauth_login_remediation(provider: Option<&str>, label: Option<&str>) -> String {
    provider.map_or_else(
        || "run `routectl login <provider>`".to_string(),
        |provider| format!("run `{}`", oauth_login_command(provider, label)),
    )
}

/// The `routectl login` command that stores a fresh credential for an
/// `oauth://<provider>[#<label>]` ref. A labeled seat needs `--label`, or the
/// bare form would log in the default seat and leave this one unchanged. The
/// label is operator-written, so both halves route through the log-safe
/// helper, and the label is shell-quoted, before they reach a copy-pasteable
/// command.
pub(super) fn oauth_login_command(provider: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!(
            "routectl login {} --label {}",
            safe(provider),
            shell_word(&safe(label))
        ),
        None => format!("routectl login {}", safe(provider)),
    }
}

/// `word` as one POSIX shell word: bare when every character is in
/// `[A-Za-z0-9_.-]`, else single-quoted with each `'` written as `'\''`.
fn shell_word(word: &str) -> String {
    let is_plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if is_plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Orphan-secret section: each managed secret file not referenced by any
/// provider is a Warn. Read-only and non-destructive -- a stored secret is
/// never auto-deleted.
pub(super) fn section_secret_orphans(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.orphan_secrets
        .iter()
        .map(|name| Finding {
            section: "secrets",
            name: name.clone(),
            status: Status::Warn,
            detail: "stored secret is not referenced by any provider".to_string(),
            remediation: Some(
                "reference this secret from a provider or remove it manually".to_string(),
            ),
        })
        .collect()
}

/// OAuth seat-pool section: one finding per `[pools.<name>]` block, then one
/// per `oauth://` reference on a provider entry NO pool claims.
///
/// A pool is the unit an operator reasons about, so the pool line names its
/// members and their seats, its `seat_selection`, whether it accepts new
/// logins, and any member the credential store holds no seat for. A pool
/// missing a member's credential is a `Warn` naming the member; a pool no
/// member of which has a stored credential is a `Fail` -- every model naming
/// it is refused locally until one member has a credential. Standalone ref rows stay purely informational `Pass`
/// findings with no remediation.
///
/// LIMIT, deliberate: this section runs on a doctor pass that builds NO
/// router, so the presence answers come from the config plus the read-only
/// store snapshot. A member whose credential IS stored but which the factory
/// could not compile into a dispatch seat (a refused refresh, a provider block
/// the factory rejects) is only observable at BUILD time -- the router retains
/// that report for a surface that has a built router. This section therefore
/// under-reports degradation rather than guessing at it, and never claims a
/// pool is healthy in build terms.
///
/// The join and wording come from the shared
/// [`crate::commands::seat_report`] module, so this section and `config check`
/// tell one story from one state. It is derived here from `ctx.seats` keys plus
/// `ctx.config`; no extra gathering plumbing exists for it. When the credential
/// store failed to open, the snapshot is `None` (unknown presence, strategy
/// still rendered, no Warn or Fail claimed) -- the auth section keeps sole
/// ownership of the store `Fail`.
pub(super) fn section_seat_pools(ctx: &DoctorContext) -> Vec<Finding> {
    let stored: Vec<String> = ctx.seats.iter().map(|(key, _)| key.clone()).collect();
    let snapshot = if ctx.auth_store_error.is_some() {
        None
    } else {
        Some(stored.as_slice())
    };
    let mut findings: Vec<Finding> = pool_rows(&ctx.config, snapshot)
        .iter()
        .map(pool_finding)
        .collect();
    findings.extend(
        stored_seat_pool_rows(&ctx.config, snapshot)
            .iter()
            .filter(|row| row.pool.is_none())
            .map(|row| Finding {
                section: "pools",
                name: row.entry.clone(),
                status: Status::Pass,
                detail: describe_row(row),
                remediation: None,
            }),
    );
    findings
}

/// One pool's finding: the shared pool sentence, with the status and the
/// remediation drawn from what the config plus the store snapshot can see.
fn pool_finding(row: &PoolRow) -> Finding {
    let (status, remediation) = match row.health {
        PoolHealth::Ready | PoolHealth::Unknown => (Status::Pass, None),
        PoolHealth::Degraded => (
            Status::Warn,
            Some(
                "log in the account each member without a stored credential names \
                 (`routectl login <provider> --label <label>`) or remove it from the pool"
                    .to_string(),
            ),
        ),
        PoolHealth::Unusable => (
            Status::Fail,
            Some(
                "log in at least one of this pool's accounts \
                 (`routectl login <provider> --label <label>`); requests to a model naming \
                 the pool are refused with `credential_unusable` until one member has a \
                 credential"
                    .to_string(),
            ),
        ),
    };
    Finding {
        section: "pools",
        name: row.pool.clone(),
        status,
        detail: describe_pool(row),
        remediation,
    }
}

/// Orphan-seat section: each stored OAuth seat no provider entry's
/// `oauth://` ref reaches is a Warn. Read-only and non-destructive -- a
/// stored credential is never auto-deleted or refreshed. The finding names
/// the SEAT KEY only (provider id plus the operator's own label); no token
/// material, account data, or storage path is disclosed.
///
/// The label half of the key is operator-written (`login --label`), so the key
/// and the `logout` remediation it composes are both rendered through the
/// shared log-safe helper: a label bearing a newline and an ANSI sequence would
/// otherwise forge a whole finding line in the human render, and a
/// copy-pasteable remediation is the worst place to hand one back.
pub(super) fn section_seat_orphans(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.orphan_seats
        .iter()
        .map(|seat| {
            let shown = safe(seat);
            Finding {
                section: "seats",
                name: shown.clone(),
                status: Status::Warn,
                detail: format!(
                    "seat `{shown}` has stored credentials but no provider entry uses it"
                ),
                remediation: Some(format!(
                    "reference this seat from a provider `api_key_ref` or run \
                     `routectl logout {}` to remove it",
                    safe(&logout_target(seat))
                )),
            }
        })
        .collect()
}

/// The `routectl logout` invocation that removes exactly this seat: a
/// labelled seat needs `--label`, or the bare form would remove the DEFAULT
/// seat instead and leave the orphan in place.
fn logout_target(seat_key: &str) -> String {
    match seat_key.split_once('#') {
        Some((provider, label)) => format!("{provider} --label {label}"),
        None => seat_key.to_string(),
    }
}

pub(super) fn section_auth(ctx: &DoctorContext) -> Vec<Finding> {
    let mut findings = oauth_seat_findings(ctx);
    findings.extend(ctx.parked_seats.iter().map(parked_seat_finding));
    findings.extend(ctx.unusable_seats.iter().map(unusable_seat_finding));
    findings
}

fn oauth_seat_findings(ctx: &DoctorContext) -> Vec<Finding> {
    if let Some(err) = &ctx.auth_store_error {
        return vec![Finding {
            section: "auth",
            name: "oauth credentials".to_string(),
            status: Status::Fail,
            detail: format!("credential store could not be opened: {err}"),
            remediation: Some(
                "repair or remove the credential store named above, then re-run `routectl login`"
                    .to_string(),
            ),
        }];
    }
    if ctx.seats.is_empty() {
        return vec![Finding {
            section: "auth",
            name: "oauth credentials".to_string(),
            status: Status::Warn,
            detail: "no oauth providers are logged in".to_string(),
            remediation: Some("run `routectl login <provider>` to authenticate".to_string()),
        }];
    }
    ctx.seats
        .iter()
        .map(|(key, rec)| auth_finding(key, rec, ctx.now_unix))
        .collect()
}

/// One seat's auth finding. The seat key's label half is operator-written, so
/// both the finding name and the `login` remediation route through the shared
/// log-safe helper -- same reason as the orphan section.
fn auth_finding(seat_key: &str, rec: &TokenRecord, now: u64) -> Finding {
    if rec.is_locally_usable(now) {
        Finding {
            section: "auth",
            name: safe(seat_key),
            status: Status::Pass,
            detail: "logged in".to_string(),
            remediation: None,
        }
    } else {
        let provider = safe(seat_provider(seat_key));
        Finding {
            section: "auth",
            name: safe(seat_key),
            status: Status::Warn,
            detail: "access token expired".to_string(),
            remediation: Some(format!(
                "run `routectl login {provider}` to renew this seat"
            )),
        }
    }
}

fn seat_provider(seat_key: &str) -> &str {
    seat_key.split_once('#').map_or(seat_key, |(p, _)| p)
}

/// Capability section: the config-derived findings NOT absorbed by the
/// capability matrix panel. The operator override cells and the runtime-only
/// learned line are structured cells on the matrix panel; what remains here
/// is the config-unavailable degradation line, or, for a config that loads,
/// one line when `[capability] enabled = false` switches learning off. The
/// section NEVER emits a `Fail`, so it can never flip the doctor exit code.
pub(super) fn section_capability(ctx: &DoctorContext) -> Vec<Finding> {
    match ctx.capability.panel_unavailable.as_deref() {
        Some(redacted) => vec![capability_unavailable(redacted)],
        None if !ctx.config.capability.enabled => vec![capability_learning_off()],
        None => Vec::new(),
    }
}

fn capability_learning_off() -> Finding {
    Finding {
        section: "capability",
        name: "learning".to_string(),
        status: Status::Warn,
        detail: "capability learning is off ([capability] enabled = false): nothing is learned, \
                 and no learned verdict or limit is applied"
            .to_string(),
        remediation: Some(
            "set `[capability] enabled = true` to learn and apply capability verdicts again"
                .to_string(),
        ),
    }
}

/// The panel-unavailable finding: the config layer could not be parsed. The
/// detail carries ONLY the already-redacted load error (routed through the
/// shared parse-error redactor at gather time) -- never raw loader text.
/// `Warn`, so it degrades honestly without flipping the exit code (the
/// version section owns the config-load `Fail`).
fn capability_unavailable(redacted: &str) -> Finding {
    let detail = if redacted.trim().is_empty() {
        "capability panel unavailable: the config could not be loaded".to_string()
    } else {
        format!("capability panel unavailable: {redacted}")
    };
    Finding {
        section: "capability",
        name: "panel".to_string(),
        status: Status::Warn,
        detail,
        remediation: Some(
            "resolve the config error reported above, then re-run `routectl doctor`".to_string(),
        ),
    }
}

/// Pricing section: one deterministic row per configured model naming each of
/// its five prices with the source that gave it, and the date of each price
/// table read. Purely informational -- an unpriced model is a legitimate
/// configuration -- so no row here can Fail and the section can never move
/// the exit code.
pub(super) fn section_pricing(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.pricing.iter().map(pricing_finding).collect()
}

/// Pure mapping of one resolved [`PricingRow`] to its finding. The detail
/// names the model's provider, kind and upstream from the config, then each
/// price as `<field> <value> (<source>)`, then the table dates.
///
/// Nothing from a price table is rendered beyond the prices and the table
/// dates: sources are fixed tokens, and the table provider that matched is
/// never named.
///
/// A subscription row is billed by seat; its prices are the basis of the
/// usage report's API-equivalent value, so it carries a clause saying whether
/// that value is complete or which prices it lacks.
pub(super) fn pricing_finding(row: &PricingRow) -> Finding {
    let selector = format!(
        "provider {} (kind {}) upstream {}",
        safe(&row.provider),
        safe(&row.provider_kind),
        safe(&row.upstream)
    );
    let fields = render_price_fields(&row.pricing, &row.sources);
    let tables = render_table_dates(row.tables);
    let unpriced = row.sources.is_unknown();
    let detail = if row.subscription {
        format!(
            "billed by subscription; no per-token rate applies -- {selector}; API-equivalent \
             prices: {fields}; {}; {tables}",
            render_equivalence_clause(&row.nickname, &row.pricing, unpriced)
        )
    } else if unpriced && row.registry_matched {
        format!(
            "unpriced: the matching [registry] row prices nothing -- {selector}; {fields}; \
             usage reports no cost for it; {tables}"
        )
    } else if unpriced {
        format!(
            "unpriced: no source prices it -- {selector}; {fields}; usage reports no cost for \
             it; {tables}"
        )
    } else {
        match render_unpriced_dimensions_clause(&row.nickname, &row.pricing, &row.sources) {
            Some(clause) => format!("priced -- {selector}; {fields}; {clause}; {tables}"),
            None => format!("priced -- {selector}; {fields}; {tables}"),
        }
    };
    Finding {
        section: "pricing",
        name: safe(&row.nickname),
        status: Status::Pass,
        detail,
        remediation: None,
    }
}

/// The five prices in a fixed order, each `<field> <value> (<source>)`. An
/// unknown price reads `unknown`, never `$0`; a known zero reads `$0.00`.
fn render_price_fields(pricing: &PricingConfig, sources: &PriceSources) -> String {
    [
        ("input", pricing.input_per_mtok, sources.input),
        ("output", pricing.output_per_mtok, sources.output),
        (
            "cache read",
            pricing.cache_read_per_mtok,
            sources.cache_read,
        ),
        (
            "cache write 5m",
            pricing.cache_write_5m_per_mtok,
            sources.cache_write_at(PricingConfig::CACHE_WRITE_5M_SECONDS),
        ),
        (
            "cache write 1h",
            pricing.cache_write_1h_per_mtok,
            sources.cache_write_at(PricingConfig::CACHE_WRITE_1H_SECONDS),
        ),
    ]
    .iter()
    .map(|(label, value, source)| format!("{label} {} ({})", render_rate(*value), source.as_str()))
    .collect::<Vec<_>>()
    .join(", ")
}

/// The date of each price table layer, or `none` for an absent layer.
fn render_table_dates(tables: PriceTableDates) -> String {
    let date = |d: Option<TableDate>| d.map_or_else(|| "none".to_string(), |d| d.to_string());
    format!(
        "price tables: refreshed {}, shipped {}",
        date(tables.refreshed),
        date(tables.shipped)
    )
}

/// The subscription row's equivalence clause: the usage report's
/// API-equivalent value is complete-or-absent, so this says whether every
/// price resolves, which ones do not, or that none does.
fn render_equivalence_clause(nickname: &str, pricing: &PricingConfig, unpriced: bool) -> String {
    let fix = format!("set them in [models.{}.pricing]", safe(nickname));
    if unpriced {
        return format!(
            "usage reports NO API-equivalent value: no source prices this upstream; {fix}"
        );
    }
    let missing = missing_equivalence_dimensions(pricing);
    if missing.is_empty() {
        "usage values it at API-equivalent rates; every price resolves".to_string()
    } else {
        format!(
            "usage withholds the API-equivalent value for traffic that uses unpriced \
             dimensions ({}); {fix}",
            missing.join(", ")
        )
    }
}

/// The priced row's missing-dimensions clause: usage is complete-or-absent
/// for a row a `[registry]` row does not price, so this names the dimensions
/// whose traffic usage reports as unpriced. `None` when every price resolves
/// or a `[registry]` row prices the row.
fn render_unpriced_dimensions_clause(
    nickname: &str,
    pricing: &PricingConfig,
    sources: &PriceSources,
) -> Option<String> {
    if !is_complete_or_absent(sources) {
        return None;
    }
    let missing = missing_equivalence_dimensions(pricing);
    (!missing.is_empty()).then(|| {
        format!(
            "usage reports no cost for traffic that uses unpriced dimensions ({}); set them in \
             [models.{}.pricing]",
            missing.join(", "),
            safe(nickname)
        )
    })
}

/// One per-million price as money: at least two decimals, up to four so a
/// sub-cent price is not rounded away, or `unknown` when no source gives it.
fn render_rate(per_mtok: Option<f64>) -> String {
    per_mtok.map_or_else(
        || "unknown".to_string(),
        |v| {
            let four = format!("{v:.4}");
            let trimmed = four.trim_end_matches('0');
            let decimals = trimmed.split_once('.').map_or(0, |(_, d)| d.len());
            if decimals < 2 {
                format!("${v:.2}")
            } else {
                format!("${trimmed}")
            }
        },
    )
}

/// The operator staleness hint as the `i64` the epoch-day checks take. A
/// hint larger than `i64::MAX` days is nonsensical; saturate rather than
/// wrap so an absurd config never reads as fresh.
pub(super) fn staleness_threshold_days(hint_days: u64) -> i64 {
    i64::try_from(hint_days).unwrap_or(i64::MAX)
}

/// Knobs section: one deterministic row per configured model naming where its
/// outbound `max_output_tokens` ceiling comes from and what it resolves to.
/// Purely informational -- every state is a legitimate configuration -- so no
/// row here can Fail and the section can never move the exit code.
pub(super) fn section_knobs(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.knobs.iter().map(knob_finding).collect()
}

/// Pure mapping of one resolved [`KnobRow`] to its finding. The detail leads
/// with `max_output_tokens <value> (<source>)`, the source a
/// [`FillSource`] token, then names the model's kind and upstream.
fn knob_finding(row: &KnobRow) -> Finding {
    let selector = format!(
        "kind {} upstream {}",
        safe(&row.provider_kind),
        safe(&row.upstream)
    );
    let detail = match row.fill {
        None => match row.configured {
            None => format!(
                "max_output_tokens unset -- {selector}; the egress forwards an omitted limit \
                 untouched"
            ),
            Some(tokens) => {
                let uses = if row.provider_kind == routectl_core::provider_kind::config::BEDROCK {
                    "the window-gate reserve and the limit Converse sends with legacy thinking"
                } else {
                    "the window-gate reserve"
                };
                format!(
                    "max_output_tokens {tokens} (model-config) -- {selector}; the egress \
                     forwards an omitted limit as omitted; this value sizes {uses}"
                )
            }
        },
        Some(fill) => {
            let lead = format!(
                "max_output_tokens {} ({})",
                fill.tokens,
                fill.source.as_str()
            );
            let tail = match (fill.source, row.lowers) {
                (FillSource::Learned, lowers) => format!(
                    "learned from an upstream refusal, it lowers {} until it decays",
                    lowers.map_or_else(|| "the fill".to_string(), |base| base.to_string())
                ),
                (FillSource::ModelConfig, _) => {
                    "a ceiling learned from an upstream refusal can lower it".to_string()
                }
                (FillSource::Default, _) => "the egress sends this baseline when the caller \
                     omits a limit; set `max_output_tokens` on the model for one whose ceiling \
                     is lower"
                    .to_string(),
            };
            format!("{lead} -- {selector}; {tail}")
        }
    };
    Finding {
        section: "knobs",
        name: safe(&row.nickname),
        status: Status::Pass,
        detail,
        remediation: None,
    }
}

/// Tuning section: one row per resolved-tuning target naming each breaker,
/// RPM and timeout cell with the tier that supplied it (model, provider,
/// pool, global or default), in the same `key=value(source)` form
/// `config show --effective` prints. Purely informational: every row is a
/// Pass with no remediation, so the section never moves the exit code.
pub(super) fn section_tuning(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.tuning.iter().map(tuning_finding).collect()
}

fn tuning_finding(row: &TuningRow) -> Finding {
    Finding {
        section: "tuning",
        name: format!("{} {}", tuning_kind(row.kind), safe(&row.target)),
        status: Status::Pass,
        detail: tuning_cells(&row.tuning).join(" "),
        remediation: None,
    }
}
