//! Doctor section producers.

use routectl_auth::oauth::types::TokenRecord;
use routectl_router::{
    ActivationEntry, ActivationStatus, CURRENT_CONFIG_VERSION, ConfigVersionError, Finding, Status,
    UnresolvedReason, compute_activation, epoch_day_age, is_stale_days, preflight_config_version,
};

use crate::commands::capability_legacy::{
    LEGACY_ALLOWED_BETAS, LEGACY_ALLOWED_BODY_FIELDS, LEGACY_UNSUPPORTED_FEATURES,
};
use crate::commands::probe::{login_id_for, probe_finding};

use super::gather::{SecretCheck, SecretPresence};
use super::{DoctorContext, FreshnessInputs};

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
            "this provider has no built-in catalog entries yet and cannot be routed".to_string()
        }
        _ => format!("run `routectl login {id}` to activate this provider"),
    }
}

pub(super) fn section_version(ctx: &DoctorContext) -> Vec<Finding> {
    let binary = ctx.binary_version;
    let Some(raw) = &ctx.raw_config else {
        return vec![Finding {
            section: "version",
            name: "config schema".to_string(),
            status: Status::Warn,
            detail: format!("config file could not be read; binary routectl {binary}"),
            remediation: Some("run `routectl init` to create a config".to_string()),
        }];
    };

    let finding = match preflight_config_version(raw) {
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
            remediation: Some(
                "run `routectl config migrate` to bring the config forward".to_string(),
            ),
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

/// Config-check section: the STATIC validator suite (reused via
/// `validation_report`) rendered as findings, plus a read-only
/// secret-presence scan. Resolves no secret value and refreshes no
/// credential; every message names the scheme, never the value or ref.
pub(super) fn section_config(ctx: &DoctorContext) -> Vec<Finding> {
    let mut findings = Vec::new();
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
    if report.errors.is_empty() {
        findings.push(Finding {
            section: "config",
            name: "validation".to_string(),
            status: Status::Pass,
            detail: "config passes the static validator suite".to_string(),
            remediation: None,
        });
    } else {
        for err in report.errors {
            findings.push(Finding {
                section: "config",
                name: "validation".to_string(),
                status: Status::Fail,
                detail: err,
                remediation: Some(
                    "fix the config error above, then re-run `routectl doctor`".to_string(),
                ),
            });
        }
    }
    findings.extend(ctx.secret_checks.iter().map(secret_finding));
    findings
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
            Some(oauth_login_remediation(check)),
        ),
        SecretPresence::StoreUnavailable => (
            Status::Warn,
            "oauth credential store is unavailable".to_string(),
            Some(oauth_login_remediation(check)),
        ),
        SecretPresence::UnknownOauthProvider => (
            Status::Warn,
            "oauth provider is not recognized by this build".to_string(),
            Some("correct the provider id in this oauth:// reference".to_string()),
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
        "oauth://" => oauth_login_remediation(check),
        "env://" => "set the environment variable this provider references".to_string(),
        "file://" => "create the secret file this provider references".to_string(),
        _ => "provide a resolvable secret reference for this provider".to_string(),
    }
}

fn oauth_login_remediation(check: &SecretCheck) -> String {
    match &check.oauth_id {
        Some(id) => format!("run `routectl login {id}`"),
        None => "run `routectl login <provider>`".to_string(),
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

/// Orphan-seat section: each stored OAuth seat no provider entry's
/// `oauth://` ref reaches is a Warn. Read-only and non-destructive -- a
/// stored credential is never auto-deleted or refreshed. The finding names
/// the SEAT KEY only (provider id plus the operator's own label); no token
/// material, account data, or storage path is disclosed.
pub(super) fn section_seat_orphans(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.orphan_seats
        .iter()
        .map(|seat| Finding {
            section: "seats",
            name: seat.clone(),
            status: Status::Warn,
            detail: format!("seat `{seat}` has stored credentials but no provider entry uses it"),
            remediation: Some(format!(
                "reference this seat from a provider `api_key_ref` or run \
                 `routectl logout {}` to remove it",
                logout_target(seat)
            )),
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

fn auth_finding(seat_key: &str, rec: &TokenRecord, now: u64) -> Finding {
    if rec.is_locally_usable(now) {
        Finding {
            section: "auth",
            name: seat_key.to_string(),
            status: Status::Pass,
            detail: "logged in".to_string(),
            remediation: None,
        }
    } else {
        let provider = seat_provider(seat_key);
        Finding {
            section: "auth",
            name: seat_key.to_string(),
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
/// capability matrix panel. The operator override cells, the catalog priors,
/// and the runtime-only learned line are now structured cells on the matrix
/// panel; only the config-unavailable degradation line and the legacy-key
/// migrate nudge remain findings here. Every finding is `Pass` or `Warn`;
/// the section NEVER emits a `Fail`, so it can never flip the doctor exit
/// code.
pub(super) fn section_capability(ctx: &DoctorContext) -> Vec<Finding> {
    let Some(config) = &ctx.capability.config else {
        return vec![capability_unavailable(
            ctx.capability.panel_unavailable.as_deref(),
        )];
    };

    let mut findings = Vec::new();
    if let Some(nudge) = legacy_nudge(&config.legacy_keys) {
        findings.push(nudge);
    }
    findings
}

/// The panel-unavailable finding: the config layer could not be parsed. The
/// detail carries ONLY the already-redacted load error (routed through the
/// shared parse-error redactor at gather time) -- never raw loader text.
/// `Warn`, so it degrades honestly without flipping the exit code (the
/// version section owns the config-load `Fail`).
fn capability_unavailable(redacted: Option<&str>) -> Finding {
    let detail = match redacted {
        Some(err) if !err.trim().is_empty() => {
            format!("capability panel unavailable: {err}")
        }
        _ => "capability panel unavailable: the config could not be loaded".to_string(),
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

/// The legacy-key migrate nudge: ONE `Warn` finding when a legacy
/// capability-list key is present, naming the present keys and the `config
/// migrate` pointer with the guarded phrasing. Absent (returns `None`) when
/// no legacy list is set. `Warn`, so it never flips the exit code. The
/// phrasing is EXACT and MUST NOT imply the lists are safe to delete.
pub(super) fn legacy_nudge(legacy_keys: &[&'static str]) -> Option<Finding> {
    if legacy_keys.is_empty() {
        return None;
    }

    let mut clauses = Vec::new();
    if legacy_keys.contains(&LEGACY_UNSUPPORTED_FEATURES) {
        clauses.push("the override layer replaces these via config migrate");
    }
    if legacy_keys
        .iter()
        .any(|k| *k == LEGACY_ALLOWED_BETAS || *k == LEGACY_ALLOWED_BODY_FIELDS)
    {
        clauses.push(
            "the learner discovers use-time rejections automatically; these lists remain \
             operator-owned until the next schema version",
        );
    }

    let detail = format!(
        "deprecated capability-list keys are set ({}); {}",
        legacy_keys.join(", "),
        clauses.join("; "),
    );
    Some(Finding {
        section: "capability",
        name: "legacy keys".to_string(),
        status: Status::Warn,
        detail,
        remediation: Some(
            "run `routectl config migrate` to move deprecated keys under [capability.overrides]"
                .to_string(),
        ),
    })
}

/// Freshness section: three findings-shaped rows describing how current the
/// catalog data backing this install is. Every row is `Pass` or `Warn` --
/// NEVER `Fail`, so it can never flip the doctor exit code. Staleness is
/// advisory: an old overlay or import is a WARN, never an error.
pub(super) fn section_freshness(ctx: &DoctorContext) -> Vec<Finding> {
    freshness_findings(&ctx.freshness)
}

/// Pure mapping of the gathered [`FreshnessInputs`] to the three freshness
/// rows. The reserved `import_result` field is intentionally not read: no
/// durable import RESULT exists yet, so it renders nothing.
pub(super) fn freshness_findings(f: &FreshnessInputs) -> Vec<Finding> {
    vec![
        baked_catalog_finding(f),
        overlay_age_finding(f),
        last_import_finding(f),
    ]
}

/// Row 1: the compiled-in baked catalog version and its snapshot date.
/// Informational `Pass` -- the baked table is always present, and the
/// separate startup staleness WARN owns the "baked snapshot is old" signal.
fn baked_catalog_finding(f: &FreshnessInputs) -> Finding {
    Finding {
        section: "freshness",
        name: "baked catalog".to_string(),
        status: Status::Pass,
        detail: format!(
            "baked catalog v{} snapshot {}",
            f.catalog_version, f.snapshot_date
        ),
        remediation: None,
    }
}

/// Row 2: the freshest overlay verification stamp and its age. WARN when the
/// stamp is stale past the operator's staleness hint or does not parse;
/// PASS when fresh; an honest PASS when no overlay verification exists (the
/// operator is running on baked defaults).
fn overlay_age_finding(f: &FreshnessInputs) -> Finding {
    let Some(verified_at) = &f.overlay_verified_at else {
        return Finding {
            section: "freshness",
            name: "overlay".to_string(),
            status: Status::Pass,
            detail: "no overlay verified stamp present; running on the baked catalog".to_string(),
            remediation: None,
        };
    };
    let threshold = staleness_threshold_days(f.staleness_hint_days);
    match epoch_day_age(verified_at, f.today_epoch_day) {
        None => Finding {
            section: "freshness",
            name: "overlay".to_string(),
            status: Status::Warn,
            detail: "overlay verified stamp could not be parsed".to_string(),
            remediation: Some(
                "run `routectl catalog import` to refresh the catalog overlay".to_string(),
            ),
        },
        Some(age) if is_stale_days(verified_at, f.today_epoch_day, threshold) => Finding {
            section: "freshness",
            name: "overlay".to_string(),
            status: Status::Warn,
            detail: format!(
                "overlay verified {age} days ago (stale past {} days)",
                f.staleness_hint_days
            ),
            remediation: Some(
                "run `routectl catalog import` to refresh the catalog overlay".to_string(),
            ),
        },
        Some(age) => Finding {
            section: "freshness",
            name: "overlay".to_string(),
            status: Status::Pass,
            detail: format!("overlay verified {age} days ago"),
            remediation: None,
        },
    }
}

/// Row 3: the LAST SUCCESSFUL import's date, age, and row counts. WARN when
/// that import is stale past the operator's hint; PASS when fresh; an honest
/// PASS when no import has ever been recorded (`catalog_import_state.json`
/// missing or unreadable). Named "last successful import" on purpose: the
/// sidecar records only successes, so this is never a failed-attempt result.
fn last_import_finding(f: &FreshnessInputs) -> Finding {
    let Some(state) = &f.last_import else {
        return Finding {
            section: "freshness",
            name: "last successful import".to_string(),
            status: Status::Pass,
            detail: "no successful import recorded".to_string(),
            remediation: None,
        };
    };
    let rows: usize = state.per_source_counts.values().sum();
    let sources = state.per_source_counts.len();
    let threshold = staleness_threshold_days(f.staleness_hint_days);
    let counts = format!("{rows} rows across {sources} sources");
    match epoch_day_age(&state.last_import_date, f.today_epoch_day) {
        Some(age) if is_stale_days(&state.last_import_date, f.today_epoch_day, threshold) => {
            Finding {
                section: "freshness",
                name: "last successful import".to_string(),
                status: Status::Warn,
                detail: format!(
                    "last successful import {} ({age} days ago, stale past {} days): {counts}",
                    state.last_import_date, f.staleness_hint_days
                ),
                remediation: Some(
                    "run `routectl catalog import` to refresh the catalog".to_string(),
                ),
            }
        }
        Some(age) => Finding {
            section: "freshness",
            name: "last successful import".to_string(),
            status: Status::Pass,
            detail: format!(
                "last successful import {} ({age} days ago): {counts}",
                state.last_import_date
            ),
            remediation: None,
        },
        None => Finding {
            section: "freshness",
            name: "last successful import".to_string(),
            status: Status::Pass,
            detail: format!("last successful import recorded (undated stamp): {counts}"),
            remediation: None,
        },
    }
}

/// The operator staleness hint as the `i64` the epoch-day checks take. A
/// hint larger than `i64::MAX` days is nonsensical; saturate rather than
/// wrap so an absurd config never reads as fresh.
pub(super) fn staleness_threshold_days(hint_days: u64) -> i64 {
    i64::try_from(hint_days).unwrap_or(i64::MAX)
}
