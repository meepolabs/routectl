//! Doctor section producers.

use routectl_auth::oauth::types::TokenRecord;
use routectl_core::sanitize_for_log_with_cap;
use routectl_router::{
    ActivationEntry, ActivationStatus, CURRENT_CONFIG_VERSION, ConfigVersionError, Finding,
    PricingSource, Status, UnresolvedReason, compute_activation, epoch_day_age, is_stale_days,
    preflight_config_version,
};

use crate::commands::capability_legacy::{
    LEGACY_ALLOWED_BETAS, LEGACY_ALLOWED_BODY_FIELDS, LEGACY_UNSUPPORTED_FEATURES,
};
use crate::commands::config::MAX_REPORTED_LINE_CHARS;
use crate::commands::probe::{login_id_for, probe_finding};
use crate::commands::seat_report::{
    PoolHealth, PoolRow, describe_pool, describe_row, pool_rows, safe, stored_seat_pool_rows,
};

use super::gather::{SecretCheck, SecretPresence};
use super::{
    DoctorContext, EquivalenceBasis, FreshnessInputs, KnobRow, OutputCeilingSource, PricingRow,
    PricingRowSource,
};

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

/// OAuth seat-pool section: one finding per `[pools.<name>]` block, then one
/// per `oauth://` reference on a provider entry NO pool claims.
///
/// A pool is the unit an operator reasons about, so the pool line names its
/// members and their seats, its `seat_selection`, whether it accepts new
/// logins, and any member the credential store holds no seat for. A pool
/// missing a member's credential is a `Warn` naming the member; a pool no
/// member of which has a stored credential is a `Fail` -- every model naming
/// it is unroutable. Standalone ref rows stay purely informational `Pass`
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
                "log in the account each omitted member names \
                 (`routectl login <provider> --label <label>`) or remove it from the pool"
                    .to_string(),
            ),
        ),
        PoolHealth::Unusable => (
            Status::Fail,
            Some(
                "log in at least one of this pool's accounts \
                 (`routectl login <provider> --label <label>`); every model naming \
                 the pool is unroutable until one member resolves"
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

/// Pricing section: one deterministic row per configured model naming where
/// its cost rates come from. Purely informational -- an unpriced model is a
/// legitimate configuration (the operator may not care about cost accounting),
/// so no row here can Fail, and the section can never move the exit code.
///
/// An unresolvable overlay collapses the whole section to ONE unavailable
/// line: see [`pricing_unavailable`].
pub(super) fn section_pricing(ctx: &DoctorContext) -> Vec<Finding> {
    match &ctx.pricing {
        Some(rows) => rows.iter().map(pricing_finding).collect(),
        None => vec![pricing_unavailable()],
    }
}

/// The section-unavailable finding: the catalog overlay could not be loaded.
///
/// The overlay is the rate CORRECTION channel -- it disables entries and
/// supersedes baked rates -- so with it unreadable, the baked figure for any
/// selector may be exactly the one the effective pricing does NOT use. A
/// superseded dollar figure is worse than no figure, so the section reports
/// nothing resolved rather than the baked layer alone.
///
/// `Warn`, matching [`capability_unavailable`]: it degrades honestly without
/// flipping the exit code (the version section owns the overlay-load `Fail`).
fn pricing_unavailable() -> Finding {
    Finding {
        section: "pricing",
        name: "section".to_string(),
        status: Status::Warn,
        detail: "cost pricing unavailable: the catalog overlay could not be loaded, so no \
                 model's effective rates can be resolved (the overlay supersedes baked rates, \
                 so reporting the baked figure could name a rate the bill does not use)"
            .to_string(),
        remediation: Some(
            "resolve the catalog-overlay error reported above, then re-run `routectl doctor`"
                .to_string(),
        ),
    }
}

/// Pure mapping of one resolved [`PricingRow`] to its finding. The detail
/// names the model's provider, kind, and upstream in every state so an
/// operator can see WHICH selector was resolved, and the two priced states
/// additionally name the resolved per-million rates.
///
/// A subscription row carries a SECOND clause: the bill is by seat, but the
/// usage report still values that seat's traffic at API-equivalent rates, and
/// that equivalent is complete-or-absent. Without the clause the section is
/// silent on the one question the rule creates -- why an equivalent reads
/// absent -- since the billed-by-seat statement is true either way.
pub(super) fn pricing_finding(row: &PricingRow) -> Finding {
    let selector = format!(
        "provider {} (kind {}) upstream {}",
        safe(&row.provider),
        safe(&row.provider_kind),
        safe(&row.upstream)
    );
    let detail = match &row.source {
        PricingRowSource::Subscription(basis) => format!(
            "billed by subscription; no per-token rate applies -- {selector}; {}",
            render_equivalence_basis(basis)
        ),
        PricingRowSource::Registry {
            input_per_mtok,
            output_per_mtok,
        } => format!(
            "priced from the [registry] table {} -- {selector}",
            render_rates(*input_per_mtok, *output_per_mtok)
        ),
        PricingRowSource::Catalog {
            input_per_mtok,
            output_per_mtok,
        } => format!(
            "priced from the baked catalog {} -- {selector}",
            render_rates(*input_per_mtok, *output_per_mtok)
        ),
        PricingRowSource::Unpriced => format!(
            "unpriced: neither [registry] nor the catalog has rates -- {selector}; usage reports \
             no cost for it"
        ),
    };
    Finding {
        section: "pricing",
        name: safe(&row.nickname),
        status: Status::Pass,
        detail,
        remediation: None,
    }
}

/// The subscription row's equivalence clause: whether `usage` can value this
/// seat's traffic at API rates, and on which layer's rates.
///
/// The incomplete arm NAMES the dimensions without a rate, because a
/// catalog-basis subscription model is always incomplete -- the catalog supplies
/// base rates only -- and "incomplete" alone would leave the operator with
/// nowhere to look.
///
/// "unpriced" is deliberately absent from every arm: that word is the
/// [`PricingRowSource::Unpriced`] row's own vocabulary, and a subscription row
/// is never in that state.
fn render_equivalence_basis(basis: &EquivalenceBasis) -> String {
    match basis {
        EquivalenceBasis::Complete { source } => format!(
            "usage values it at API-equivalent rates, complete via {}",
            basis_layer(*source)
        ),
        EquivalenceBasis::Incomplete { source, missing } => format!(
            "usage withholds the API-equivalent value for traffic that uses unrated dimensions \
             ({}): {} carries no rate for them, and the equivalent is computed only when every \
             dimension the traffic used resolves one; add a complete [registry] pricing row for \
             this upstream to value cached traffic",
            missing.join(", "),
            basis_layer(*source)
        ),
        EquivalenceBasis::Unresolved => "usage reports NO API-equivalent value: neither \
             [registry] nor the catalog resolves rates for this upstream, so there is nothing to \
             value it at; add a [registry] pricing row for it"
            .to_string(),
    }
}

/// The operator-facing name of the layer an equivalence basis came from.
const fn basis_layer(source: PricingSource) -> &'static str {
    match source {
        PricingSource::Registry => "the [registry] table",
        // The catalog's cache dimensions are unset by construction (an
        // unconfirmed multiplier would fabricate a figure), so this layer can
        // only ever be a partial basis.
        PricingSource::Catalog => "the baked catalog (base rates only)",
    }
}

/// Render one row's resolved per-million rates. A dimension the winning layer
/// left unset renders as `unset`, never as `$0` -- an unpriced dimension
/// contributes nothing to a cost and must not read as free.
///
/// A winning row that priced NEITHER dimension is a distinct state from a
/// half-priced one, and only an operator `[registry]` row can reach it (a
/// catalog row pricing neither dimension resolves to `Unpriced` upstream of
/// here). "input unset / output unset" would read as a lookup that came up
/// empty; the row is instead a deliberate act that charges nothing, so it says
/// so.
fn render_rates(input_per_mtok: Option<f64>, output_per_mtok: Option<f64>) -> String {
    if input_per_mtok.is_none() && output_per_mtok.is_none() {
        return "but that row sets no per-token rate, so it prices nothing".to_string();
    }
    format!(
        "at input {} / output {} per Mtok",
        render_rate(input_per_mtok),
        render_rate(output_per_mtok)
    )
}

/// One per-million rate as money: two decimals like every other dollar
/// surface, or `unset` for a dimension the winning layer left absent.
fn render_rate(per_mtok: Option<f64>) -> String {
    per_mtok.map_or_else(|| "unset".to_string(), |v| format!("${v:.2}"))
}

/// The operator staleness hint as the `i64` the epoch-day checks take. A
/// hint larger than `i64::MAX` days is nonsensical; saturate rather than
/// wrap so an absurd config never reads as fresh.
pub(super) fn staleness_threshold_days(hint_days: u64) -> i64 {
    i64::try_from(hint_days).unwrap_or(i64::MAX)
}

/// Knobs section: one deterministic row per configured model naming where its
/// outbound `max_output_tokens` ceiling comes from. Purely informational --
/// every state is a legitimate configuration -- so no row here can Fail and the
/// section can never move the exit code.
///
/// An unresolvable overlay collapses the whole section to ONE unavailable line:
/// see [`knobs_unavailable`].
pub(super) fn section_knobs(ctx: &DoctorContext) -> Vec<Finding> {
    match &ctx.knobs {
        Some(rows) => rows.iter().map(knob_finding).collect(),
        None => vec![knobs_unavailable()],
    }
}

/// The section-unavailable finding: the catalog overlay could not be loaded.
///
/// The overlay both CORRECTS a baked ceiling and can disable a cell outright,
/// so with it unreadable the baked ceiling for any selector may be exactly the
/// one the served router does not fill from. Naming a superseded figure is
/// worse than naming none.
///
/// `Warn`, matching the pricing section: it degrades honestly without flipping
/// the exit code (the version section owns the overlay-load `Fail`).
fn knobs_unavailable() -> Finding {
    Finding {
        section: "knobs",
        name: "section".to_string(),
        status: Status::Warn,
        detail: "output-ceiling sources unavailable: the catalog overlay could not be loaded, so \
                 no model's effective max_output_tokens can be attributed (the overlay corrects \
                 and disables baked ceilings, so reporting the baked figure could name a ceiling \
                 the router does not use)"
            .to_string(),
        remediation: Some(
            "resolve the catalog-overlay error reported above, then re-run `routectl doctor`"
                .to_string(),
        ),
    }
}

/// Pure mapping of one resolved [`KnobRow`] to its finding. The detail names
/// the model's kind and upstream in every state so an operator can see WHICH
/// selector was resolved, and every state names the ceiling that results.
fn knob_finding(row: &KnobRow) -> Finding {
    let selector = format!(
        "kind {} upstream {}",
        safe(&row.provider_kind),
        safe(&row.upstream)
    );
    let detail = match row.source {
        OutputCeilingSource::Config(ceiling) => format!(
            "max_output_tokens {ceiling} from [models.{}] -- {selector}; the catalog never \
             overrides an operator value",
            safe(&row.nickname)
        ),
        OutputCeilingSource::Catalog(ceiling) => format!(
            "max_output_tokens {ceiling} filled from the catalog ([models.X] sets none) -- \
             {selector}; set max_output_tokens to pin your own"
        ),
        OutputCeilingSource::Default => format!(
            "max_output_tokens unset and the catalog confirms no ceiling -- {selector}; the \
             Anthropic-shape egresses fall back to their built-in baseline and every other \
             egress forwards caller omission untouched"
        ),
    };
    Finding {
        section: "knobs",
        name: safe(&row.nickname),
        status: Status::Pass,
        detail,
        remediation: None,
    }
}
