//! `routectl doctor` -- a read-only health report. A doctor run mutates
//! NOTHING: config, credentials, catalog overlay, and usage DB are all
//! byte-identical afterward. It loads config through the unvalidated,
//! never-migrating loader, reads the raw config bytes for a schema-version
//! preflight that never stamps the file, and probes the OAuth store with
//! read-only calls only.
//!
//! The aggregator is a FIXED SEQUENCE of section-producer functions, not a
//! check registry. Each producer maps read-only inputs to `Finding`s; the
//! ordered `SECTIONS` list is the single extension point where a new
//! section is added with a one-line edit (producer + render title).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::Local;
use routectl_auth::LocalProbe;
use routectl_auth::oauth::types::{TokenRecord, unix_now};
use routectl_auth::{OAuthError, OAuthStore};
use routectl_auth::{SecretRef, default_secret_dir};
use routectl_core::ProbeOutcome;
use routectl_router::{
    ActivationEntry, ActivationStatus, CURRENT_CONFIG_VERSION, CatalogOverlay, Config,
    ConfigVersionError, DoctorPanels, DoctorReport, EffectiveRow, Finding, OverrideProvenance,
    OverrideRegistry, OverrideRow, OverrideVerdict, Source, Status, UnresolvedReason,
    WouldTrimPanel, compute_activation, derive_effective_view, is_stale_today, overall_exit,
    preflight_config_version,
};

use crate::commands::capability_legacy::{
    LEGACY_ALLOWED_BETAS, LEGACY_ALLOWED_BODY_FIELDS, LEGACY_UNSUPPORTED_FEATURES,
    present_legacy_capability_keys,
};
use crate::commands::doctor_panels::{compute_would_trim_panel, render_would_trim_panel};
use crate::commands::parse_error_redaction::redact_config_load_error;
use crate::commands::probe::{PROBE_DEADLINE, login_id_for, probe_all, probe_finding};
use crate::server::CompositeStore;

/// UNSTABLE report schema version. Bumped on ANY structural or semantic
/// change a consumer would care about -- including an ADDITIVE one, since the
/// report JSON is explicitly human-facing and non-contractual. Bump when a
/// section's finding shape, a panel field, or the meaning of an existing
/// field changes.
///
/// v1 -> v2: the reserved `capability` section became a real producer
/// (override rows, catalog priors, the runtime-only learned line, and the
/// legacy-key migrate nudge).
const SCHEMA_VERSION: u32 = 2;

/// A section-producer: pure mapping of the read-only [`DoctorContext`] to a
/// section's findings.
type SectionFn = fn(&DoctorContext) -> Vec<Finding>;

/// The ordered aggregator sequence. THE extension point: a later section
/// (probe, config-check, orphan-scan, ...) plugs in by adding one row here
/// plus its render title in [`section_title`]. Order here is the render
/// order; the flat findings list is sorted independently for deterministic
/// output and exit codes.
const SECTIONS: &[(&str, SectionFn)] = &[
    ("inventory", section_inventory),
    ("version", section_version),
    ("config", section_config),
    ("auth", section_auth),
    ("secrets", section_secret_orphans),
    ("probe", section_probe),
    ("capability", section_capability),
];

/// The no-network subset of [`SECTIONS`]: [`SECTIONS`] MINUS the `probe`
/// entry. Every producer here is pure over [`DoctorContext`] and dials
/// nothing -- inventory, version, config, auth (`probe_local`, no refresh),
/// secrets, and capability. A report built from these needs no
/// [`gather_probe_results`] pass, so a status surface can produce a full
/// doctor report offline and derive reachability from an already-observed
/// circuit phase rather than an upstream dial.
// Consumed by the offline status surface, not the CLI `doctor` command; the
// CLI path stays on the full `SECTIONS` list.
const NO_NETWORK_SECTIONS: &[(&str, SectionFn)] = &[
    ("inventory", section_inventory),
    ("version", section_version),
    ("config", section_config),
    ("auth", section_auth),
    ("secrets", section_secret_orphans),
    ("capability", section_capability),
];

/// The read-only inputs every section producer draws from, gathered once
/// per run so producers stay pure and the run is a single filesystem pass.
pub(crate) struct DoctorContext {
    config: Config,
    raw_config: Option<String>,
    /// Set when the read-only typed load failed for a reason the raw-bytes
    /// version preflight does not catch (TOML syntax error, unknown field,
    /// legacy key, overlay failure). The version section surfaces it as a
    /// Fail so a present-but-broken config never reports all-Pass. Already
    /// redacted at gather time (see [`redact_config_load_error`]) -- a
    /// toml/serde parse error can inline a `literal:` credential, so the
    /// stored string is never the raw loader error.
    config_load_error: Option<String>,
    probes: Vec<(&'static str, LocalProbe)>,
    seats: Vec<(String, TokenRecord)>,
    /// Set when the credential store failed to open (schema mismatch,
    /// corrupted file). Distinct from an absent store: the auth section
    /// surfaces this as a Fail rather than the generic "no seats" message.
    auth_store_error: Option<String>,
    /// Per-reference presence classification for every provider secret ref,
    /// resolved read-only in the single gather pass so the config section
    /// stays a pure mapping. No entry ever carries a secret value or a full
    /// ref string -- only the scheme label and a discriminant.
    secret_checks: Vec<SecretCheck>,
    /// Basenames of managed secret files not referenced by any provider.
    /// A read-only directory-vs-refs diff; the files are never removed.
    orphan_secrets: Vec<String>,
    /// One read-only reachability outcome per configured provider, gathered
    /// through the shared `probe_all` orchestration (oauth via `probe_local`,
    /// no refresh; forwarded short-circuited). The probe section maps each
    /// via the shared `probe_finding` seam so it never drifts from the
    /// standalone `provider probe` classification.
    probe_results: Vec<(String, ProbeOutcome)>,
    /// The steady-state would-trim opportunity panel, computed read-only from
    /// the usage DB. `None` when there is no DB / no migrated schema to read.
    would_trim: Option<WouldTrimPanel>,
    now_unix: u64,
    binary_version: &'static str,
    /// The capability section's read-only inputs, resolved per-layer so the
    /// section producer stays a pure mapping. The config layer and the
    /// catalog overlay degrade independently (see [`CapabilityInputs`]).
    capability: CapabilityInputs,
}

/// Per-layer inputs the capability section maps to findings. The config
/// layer (override rows + legacy keys + prior cells) and the catalog overlay
/// (the prior cells' source) are gathered independently, so an unreadable
/// overlay degrades the priors alone while the config-derived override rows
/// still render.
struct CapabilityInputs {
    /// `Some` when the config parsed: the config-derived capability view.
    /// `None` when the config could not be parsed -- the section then renders
    /// a single redacted "panel unavailable" line (keyed on this flag) rather
    /// than a misleading empty-from-default-config view.
    config: Option<CapabilityConfig>,
    /// The already-redacted config load error, set only when `config` is
    /// `None`. Redacted at gather time through the shared parse-error
    /// redactor (see [`redact_config_load_error`]).
    panel_unavailable: Option<String>,
}

/// The config-derived capability view: everything the section renders when
/// the config parsed.
struct CapabilityConfig {
    /// Flattened operator override cells, in the registry's snapshot order.
    override_rows: Vec<OverrideRow>,
    /// Present legacy capability-list key NAMES (never values) driving the
    /// migrate nudge.
    legacy_keys: Vec<&'static str>,
    /// The catalog/overlay capability prior cells, or unavailable when the
    /// overlay could not be read.
    priors: PriorLayer,
}

/// The catalog capability prior layer, degraded independently of the config.
enum PriorLayer {
    /// The overlay loaded: the capability prior cells (empty when no cell
    /// carries capability data -- baked cells are empty today).
    Present(Vec<PriorCell>),
    /// The overlay could not be read: priors are unavailable. Override rows
    /// still render.
    Unavailable,
}

/// One catalog/overlay capability prior cell: a `[models.X]` entry whose
/// resolved catalog row carries capability data, tagged with the winning
/// layer.
struct PriorCell {
    nickname: String,
    selector: String,
    source: Source,
    capabilities: Vec<(String, bool)>,
}

/// Run the doctor aggregator against `config_path` and render the report.
/// Read-only and infallible in posture: a load or probe failure degrades to
/// a finding, never a hard error. Returns the process exit code.
pub async fn run(config_path: &Path, json: bool) -> i32 {
    let ctx = gather_context(config_path).await;
    let report = build_report(&ctx);

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("error: failed to serialize doctor report: {e}");
                return 1;
            }
        }
    } else {
        for line in render_human(&report) {
            println!("{line}");
        }
    }

    overall_exit(&report.findings)
}

/// The network doctor gather: the no-network context PLUS one upstream
/// reachability pass. Building the whole context in exactly ONE place
/// ([`gather_context_no_network`]) is what keeps the two entry points from
/// drifting -- a new context field is added once and both paths carry it; the
/// only difference between the paths is this `probe_results` assignment.
async fn gather_context(config_path: &Path) -> DoctorContext {
    let ctx = gather_context_no_network(config_path).await;
    let probe_results = gather_probe_results(&ctx.config).await;
    DoctorContext {
        probe_results,
        ..ctx
    }
}

/// Gather every read-only input the no-network sections draw from, WITHOUT
/// any upstream dial: `probe_results` is left empty and
/// [`gather_probe_results`] (the only caller of `CompositeStore`/`probe_all`)
/// is never reached. Everything else -- per-layer config/overlay load, auth
/// via `probe_local` (no network, no refresh), secret presence checks, the
/// orphan scan, and the would-trim panel -- is retained.
pub(crate) async fn gather_context_no_network(config_path: &Path) -> DoctorContext {
    let raw_config = std::fs::read_to_string(config_path).ok();
    // Read-only, per-layer load: the config and the catalog overlay load
    // independently so the capability panel can degrade one without the
    // other. The version section keeps its coupled "config could not be
    // loaded" semantics -- a config parse error wins, else an overlay error
    // -- so a present-but-broken config still never reports all-Pass. On a
    // config failure the other sections run against defaults.
    let config_layer = crate::server::parse_config_only(config_path);
    let overlay_layer = crate::server::load_overlay_default();

    let (config, config_parse_error) = match config_layer {
        Ok(config) => (config, None),
        Err(e) => (Config::default(), Some(redact_config_load_error(&e))),
    };
    let config_load_error = config_parse_error.clone().or_else(|| {
        overlay_layer
            .as_ref()
            .err()
            .map(|e| redact_config_load_error(e))
    });

    let capability = build_capability_inputs(&config, config_parse_error, overlay_layer.ok());

    let (probes, seats, auth_store_error) = gather_auth().await;
    let secret_checks = gather_secret_checks(&config, &probes);
    let orphan_secrets = gather_orphan_secrets(&config);
    let would_trim = compute_would_trim_panel(&config, Local::now());

    DoctorContext {
        config,
        raw_config,
        config_load_error,
        probes,
        seats,
        auth_store_error,
        secret_checks,
        orphan_secrets,
        probe_results: Vec::new(),
        would_trim,
        now_unix: unix_now(),
        binary_version: env!("CARGO_PKG_VERSION"),
        capability,
    }
}

/// Build the capability section's per-layer inputs. A config parse error
/// (already redacted) yields the "panel unavailable" state -- NOT an
/// empty-from-default-config view. Otherwise the override rows and legacy
/// keys come from the parsed config, and the prior cells from the overlay
/// (or unavailable when the overlay could not be read).
fn build_capability_inputs(
    config: &Config,
    config_parse_error: Option<String>,
    overlay: Option<CatalogOverlay>,
) -> CapabilityInputs {
    if let Some(err) = config_parse_error {
        return CapabilityInputs {
            config: None,
            panel_unavailable: Some(err),
        };
    }

    let override_rows = OverrideRegistry::build(config).snapshot();
    let legacy_keys = present_legacy_capability_keys(config);
    let priors = match overlay {
        Some(overlay) => PriorLayer::Present(derive_prior_cells(config, &overlay)),
        None => PriorLayer::Unavailable,
    };

    CapabilityInputs {
        config: Some(CapabilityConfig {
            override_rows,
            legacy_keys,
            priors,
        }),
        panel_unavailable: None,
    }
}

/// Derive the catalog/overlay capability prior cells: one per `[models.X]`
/// entry whose resolved catalog row is `Present`, is NOT stale, AND carries
/// capability data. A `Missing` / `Disabled` cell (absent or explicitly
/// disabled), a stale cell (its `verified_at` older than the catalog
/// staleness horizon, or unparseable), or a row with no capability keys all
/// yield NO prior -- the conservative "unknown" baseline, never a fabricated
/// or falsely-unsupported row. Staleness uses the live clock, matching this
/// one-shot tool's fresh-process reads.
fn derive_prior_cells(config: &Config, overlay: &CatalogOverlay) -> Vec<PriorCell> {
    derive_effective_view(config, overlay)
        .models
        .into_iter()
        .filter_map(|cell| {
            let EffectiveRow::Present {
                row,
                source,
                verified_at,
            } = cell.row
            else {
                return None;
            };
            if is_stale_today(&verified_at) || row.capabilities.is_empty() {
                return None;
            }
            let capabilities = row.capabilities.into_iter().collect();
            Some(PriorCell {
                nickname: cell.nickname,
                selector: format!("{}/{}", cell.provider_kind, cell.upstream),
                source,
                capabilities,
            })
        })
        .collect()
}

/// Probe every configured provider read-only through the shared `probe_all`
/// orchestration, under the same bounded deadline the standalone `provider
/// probe` uses. Fail-closed: if the composite credential store cannot open,
/// every provider collapses to an `Unreachable` outcome (a WARN/FAIL finding
/// with a reason) rather than being dropped to a silent pass. That `Err` arm is
/// defensive and currently unreachable -- `CompositeStore::open_default`
/// degrades a store failure to an in-memory store (with a warning) and always
/// returns `Ok` -- but it is kept so a future non-degrading open path still
/// fails closed here.
async fn gather_probe_results(config: &Config) -> Vec<(String, ProbeOutcome)> {
    match CompositeStore::open_default().await {
        Ok(store) => probe_all(config, &store, PROBE_DEADLINE).await,
        Err(_) => config
            .providers
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    ProbeOutcome::Unreachable("credential store unavailable".into()),
                )
            })
            .collect(),
    }
}

/// Probe every routectl-owned OAuth id and list stored seats through the
/// default store. Read-only: `probe_local` performs no network or refresh
/// and `list` no writes. A store that fails to OPEN (schema mismatch,
/// corrupted file) returns a sanitized, path-free error string (see
/// [`sanitize_store_open_error`]) rather than being masked as an absent
/// store; probes then read `StoreUnavailable`.
async fn gather_auth() -> (
    Vec<(&'static str, LocalProbe)>,
    Vec<(String, TokenRecord)>,
    Option<String>,
) {
    let ids = routectl_auth::oauth::known_provider_ids();
    match OAuthStore::open_default().await {
        Ok(store) => {
            let mut probes = Vec::with_capacity(ids.len());
            for id in ids {
                probes.push((*id, store.probe_local(id).await));
            }
            (probes, store.list().await, None)
        }
        Err(e) => (
            ids.iter()
                .map(|id| (*id, LocalProbe::StoreUnavailable))
                .collect(),
            Vec::new(),
            Some(sanitize_store_open_error(&e)),
        ),
    }
}

/// Map an OAuth-store OPEN failure to a path-free message. Multiple
/// [`OAuthError`] variants embed the FULL credentials-store path in their
/// Display -- `SchemaMismatch`/`CorruptedFile` carry a `path` field, and every
/// `Io` open failure interpolates the path mid-message (`open <path>: ...`,
/// `credentials file <path> has permissions ...`). The doctor auth section
/// prints this verbatim, disclosing a filesystem path. Keep only the failure
/// CLASS and the store basename (`credentials.json`); never forward a variant's
/// raw Display. The enum is `#[non_exhaustive]`, so the catch-all is itself a
/// path-free class message rather than a passthrough -- fail-safe against a
/// future path-bearing variant.
fn sanitize_store_open_error(err: &OAuthError) -> String {
    const STORE_BASENAME: &str = "credentials.json";
    match err {
        OAuthError::SchemaMismatch {
            found, expected, ..
        } => format!(
            "credentials store schema is v{found}; this binary expects v{expected}; \
             upgrade routectl or delete {STORE_BASENAME} and re-run `routectl login`"
        ),
        OAuthError::CorruptedFile { .. } => {
            format!("oauth credentials file ({STORE_BASENAME}) is corrupted")
        }
        OAuthError::Io(_) => {
            format!("oauth credentials file ({STORE_BASENAME}) could not be read")
        }
        _ => format!("oauth credentials store ({STORE_BASENAME}) could not be opened"),
    }
}

fn build_report(ctx: &DoctorContext) -> DoctorReport {
    build_report_over(ctx, SECTIONS)
}

/// The no-network report: identical to [`build_report`] but iterates
/// [`NO_NETWORK_SECTIONS`], so it never renders the `probe` section and never
/// depends on a `gather_probe_results` pass. Same schema version and sort as
/// the network report.
// Consumed by the offline status surface, not the CLI `doctor` command.
pub(crate) fn build_report_no_network(ctx: &DoctorContext) -> DoctorReport {
    build_report_over(ctx, NO_NETWORK_SECTIONS)
}

/// Shared report builder: run each section producer over `ctx`, flatten and
/// deterministically sort the findings, and attach the panels. The section
/// slice is the only thing that varies between the network and no-network
/// reports, so the sort and assembly cannot drift between them.
fn build_report_over(ctx: &DoctorContext, sections: &[(&str, SectionFn)]) -> DoctorReport {
    let mut findings = Vec::new();
    for (_, producer) in sections {
        findings.extend(producer(ctx));
    }
    findings.sort_by(|a, b| {
        a.section
            .cmp(b.section)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| status_ord(a.status).cmp(&status_ord(b.status)))
    });

    DoctorReport {
        schema_version: SCHEMA_VERSION,
        findings,
        panels: DoctorPanels {
            would_trim: ctx.would_trim,
        },
    }
}

/// Probe section: one finding per configured provider, mapped from its
/// read-only reachability outcome through the shared `probe_finding` seam so
/// the doctor battery and `provider probe` never diverge on status, detail,
/// or remediation.
fn section_probe(ctx: &DoctorContext) -> Vec<Finding> {
    ctx.probe_results
        .iter()
        .map(|(name, outcome)| probe_finding(name, outcome, login_id_for(&ctx.config, name)))
        .collect()
}

fn section_inventory(ctx: &DoctorContext) -> Vec<Finding> {
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

fn section_version(ctx: &DoctorContext) -> Vec<Finding> {
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

/// Read-only presence classification of one provider secret reference.
/// Carries only the scheme label and a discriminant -- never a secret
/// value, an env var name, or a full `file://` / `literal:` ref string.
struct SecretCheck {
    provider: String,
    scheme: &'static str,
    presence: SecretPresence,
    oauth_id: Option<String>,
}

/// Outcome of a read-only presence check. Discriminants only.
#[derive(Clone, Copy)]
enum SecretPresence {
    Present,
    Missing,
    Expired,
    Unreadable,
    StoreUnavailable,
    UnknownOauthProvider,
    Invalid,
}

/// The leak-safe scheme prefix of a secret URI. A bare, unprefixed value
/// IS secret material, so it collapses to `"unknown"` rather than echoing
/// any of its bytes.
fn scheme_label(uri: &str) -> &'static str {
    if uri.starts_with("oauth://") {
        "oauth://"
    } else if uri.starts_with("env://") {
        "env://"
    } else if uri.starts_with("file://") {
        "file://"
    } else if uri.starts_with("literal:") {
        "literal:"
    } else {
        "unknown"
    }
}

/// Classify one secret ref without resolving its value or refreshing any
/// credential. `oauth://` reads the pre-gathered local probes (no network,
/// no token refresh); `env://` / `file://` / `literal:` do a non-mutating
/// existence / parse check.
fn classify_secret_ref(
    uri: &str,
    probes: &[(&'static str, LocalProbe)],
) -> (SecretPresence, Option<String>) {
    match SecretRef::parse(uri) {
        Ok(SecretRef::OAuth { provider, .. }) => {
            let presence = match probes.iter().find(|p| p.0 == provider.as_str()) {
                Some((_, LocalProbe::Present)) => SecretPresence::Present,
                Some((_, LocalProbe::Expired)) => SecretPresence::Expired,
                Some((_, LocalProbe::Missing)) => SecretPresence::Missing,
                Some((_, LocalProbe::StoreUnavailable)) => SecretPresence::StoreUnavailable,
                Some(_) | None => SecretPresence::UnknownOauthProvider,
            };
            (presence, Some(provider))
        }
        Ok(SecretRef::Env(var)) => {
            let present = std::env::var(&var).is_ok_and(|v| !v.is_empty());
            let presence = if present {
                SecretPresence::Present
            } else {
                SecretPresence::Missing
            };
            (presence, None)
        }
        Ok(SecretRef::File(path)) => (classify_file(&path), None),
        // A future `SecretRef` variant (the enum is `#[non_exhaustive]`)
        // whose presence this build cannot confirm: treat conservatively as
        // unresolved rather than assume it is usable. A `literal:` ref no
        // longer parses (rejected at the resolver), so it lands in `Err`
        // below and reports as Invalid.
        Ok(_) => (SecretPresence::Missing, None),
        Err(_) => (SecretPresence::Invalid, None),
    }
}

/// Read-only existence + readability check of a `file://` secret path.
fn classify_file(path: &Path) -> SecretPresence {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            if std::fs::File::open(path).is_ok() {
                SecretPresence::Present
            } else {
                SecretPresence::Unreadable
            }
        }
        Ok(_) => SecretPresence::Unreadable,
        Err(_) => SecretPresence::Missing,
    }
}

fn gather_secret_checks(
    config: &Config,
    probes: &[(&'static str, LocalProbe)],
) -> Vec<SecretCheck> {
    let mut out = Vec::new();
    for (name, entry) in &config.providers {
        for uri in entry.secret_uris() {
            let scheme = scheme_label(uri);
            let (presence, oauth_id) = classify_secret_ref(uri, probes);
            out.push(SecretCheck {
                provider: name.clone(),
                scheme,
                presence,
                oauth_id,
            });
        }
    }
    out
}

/// Canonical paths of every `file://` secret the config references.
fn referenced_secret_files(config: &Config) -> BTreeSet<PathBuf> {
    let mut set = BTreeSet::new();
    for entry in config.providers.values() {
        for uri in entry.secret_uris() {
            if let Ok(SecretRef::File(path)) = SecretRef::parse(uri) {
                let canon = std::fs::canonicalize(&path).unwrap_or(path);
                set.insert(canon);
            }
        }
    }
    set
}

/// Read-only diff of the managed secret directory against the `file://`
/// refs the config references. Never opens a `ManagedSecretStore` (which
/// would create the directory) and never removes a file -- it only reads
/// the directory listing. Returns the basenames of unreferenced files.
fn gather_orphan_secrets(config: &Config) -> Vec<String> {
    let Ok(dir) = default_secret_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let referenced = referenced_secret_files(config);
    let mut orphans = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if referenced.contains(&canon) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            orphans.push(name.to_string());
        }
    }
    orphans.sort();
    orphans
}

/// Config-check section: the STATIC validator suite (reused via
/// `validation_report`) rendered as findings, plus a read-only
/// secret-presence scan. Resolves no secret value and refreshes no
/// credential; every message names the scheme, never the value or ref.
fn section_config(ctx: &DoctorContext) -> Vec<Finding> {
    let mut findings = Vec::new();
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

fn secret_finding(check: &SecretCheck) -> Finding {
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
fn section_secret_orphans(ctx: &DoctorContext) -> Vec<Finding> {
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

fn section_auth(ctx: &DoctorContext) -> Vec<Finding> {
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

/// Capability section: a pure mapping of the gathered [`CapabilityInputs`]
/// to findings. NON-CONTRACTUAL, human-facing content -- no typed panel
/// struct. Every finding here is `Pass` or `Warn`; the section NEVER emits a
/// `Fail`, so it can never flip the doctor exit code. Per-layer degradation
/// (config unavailable, overlay unavailable, absent catalog cell) is
/// rendered honestly, never a whole-doctor fallback and never raw loader
/// text.
fn section_capability(ctx: &DoctorContext) -> Vec<Finding> {
    let Some(config) = &ctx.capability.config else {
        return vec![capability_unavailable(
            ctx.capability.panel_unavailable.as_deref(),
        )];
    };

    let mut findings = Vec::new();
    findings.extend(config.override_rows.iter().map(override_finding));
    findings.push(prior_finding(&config.priors));
    findings.push(learned_line());
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

/// One informational `Pass` finding per operator override cell: the target
/// spec, the capability key, the verdict, and the source label from
/// provenance. Never flips the exit code.
fn override_finding(row: &OverrideRow) -> Finding {
    Finding {
        section: "capability",
        name: row.target_spec.clone(),
        status: Status::Pass,
        detail: format!(
            "{} {} (source: {})",
            row.capability_key,
            verdict_label(row.verdict),
            provenance_label(row.provenance),
        ),
        remediation: None,
    }
}

const fn verdict_label(verdict: OverrideVerdict) -> &'static str {
    match verdict {
        OverrideVerdict::RouteAway => "route-away",
        OverrideVerdict::ForceSupported => "force-supported",
    }
}

const fn provenance_label(provenance: OverrideProvenance) -> &'static str {
    match provenance {
        OverrideProvenance::Override => "override",
        OverrideProvenance::ProviderStatic => "provider-static",
        OverrideProvenance::ModelStatic => "model-static",
    }
}

const fn source_label(source: Source) -> &'static str {
    match source {
        Source::Baked => "baked",
        Source::Import => "import",
        Source::User => "user",
    }
}

/// The catalog/overlay prior layer as findings. Present-with-cells renders
/// one `Pass` per cell; present-but-empty an honest "no priors" note;
/// unavailable a `Warn` that leaves the override rows intact.
fn prior_finding(priors: &PriorLayer) -> Finding {
    match priors {
        PriorLayer::Unavailable => Finding {
            section: "capability",
            name: "catalog priors".to_string(),
            status: Status::Warn,
            detail: "catalog capability priors unavailable: the catalog overlay could not be read"
                .to_string(),
            remediation: Some(
                "resolve the catalog overlay error, then re-run `routectl doctor`".to_string(),
            ),
        },
        PriorLayer::Present(cells) if cells.is_empty() => Finding {
            section: "capability",
            name: "catalog priors".to_string(),
            status: Status::Pass,
            detail: "no catalog capability priors present".to_string(),
            remediation: None,
        },
        PriorLayer::Present(cells) => {
            let detail = cells
                .iter()
                .map(prior_cell_detail)
                .collect::<Vec<_>>()
                .join("; ");
            Finding {
                section: "capability",
                name: "catalog priors".to_string(),
                status: Status::Pass,
                detail,
                remediation: None,
            }
        }
    }
}

/// One prior cell rendered as `nickname (selector) [source]: cap=bool, ...`.
fn prior_cell_detail(cell: &PriorCell) -> String {
    let caps = cell
        .capabilities
        .iter()
        .map(|(key, supported)| format!("{key}={supported}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} ({}) [{}]: {caps}",
        cell.nickname,
        cell.selector,
        source_label(cell.source),
    )
}

/// The single fixed learned-layer line. Capability learning is runtime-only
/// (in the live serve registry); a one-shot doctor cannot read it. No
/// fabricated counts -- just the fixed statement of where learning lives.
fn learned_line() -> Finding {
    Finding {
        section: "capability",
        name: "learned".to_string(),
        status: Status::Pass,
        detail: "capability learning is runtime-only, visible in serve logs; \
                 a future status surface will expose the live registry"
            .to_string(),
        remediation: None,
    }
}

/// The legacy-key migrate nudge: ONE `Warn` finding when a legacy
/// capability-list key is present, naming the present keys and the `config
/// migrate` pointer with the guarded phrasing. Absent (returns `None`) when
/// no legacy list is set. `Warn`, so it never flips the exit code. The
/// phrasing is EXACT and MUST NOT imply the lists are safe to delete.
fn legacy_nudge(legacy_keys: &[&'static str]) -> Option<Finding> {
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

fn render_human(report: &DoctorReport) -> Vec<String> {
    let mut out = vec!["routectl doctor".to_string(), String::new()];
    for (key, _) in SECTIONS {
        let section_findings: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.section == *key)
            .collect();
        render_section(key, &section_findings, &mut out);
        out.push(String::new());
    }
    if let Some(panel) = &report.panels.would_trim {
        for line in render_would_trim_panel(panel).lines() {
            out.push(line.to_string());
        }
        out.push(String::new());
    }
    out.push(render_summary(&report.findings));
    out
}

fn render_section(key: &str, findings: &[&Finding], out: &mut Vec<String>) {
    out.push(format!("[{}]", section_title(key)));
    if findings.is_empty() {
        return;
    }
    for f in findings {
        out.push(format!(
            "  {} {}: {}",
            status_label(f.status),
            f.name,
            f.detail
        ));
        if let Some(rem) = &f.remediation {
            out.push(format!("       fix: {rem}"));
        }
    }
}

fn render_summary(findings: &[Finding]) -> String {
    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;
    for f in findings {
        match f.status {
            Status::Pass => pass += 1,
            Status::Warn => warn += 1,
            Status::Fail => fail += 1,
        }
    }
    format!("summary: PASS {pass}  WARN {warn}  FAIL {fail}")
}

fn section_title(key: &str) -> &'static str {
    match key {
        "inventory" => "Provider activation",
        "version" => "Config schema version",
        "config" => "Config validation",
        "auth" => "OAuth credentials",
        "secrets" => "Managed secrets",
        "probe" => "Provider reachability",
        "capability" => "Capability",
        _ => "Other",
    }
}

const fn status_label(status: Status) -> &'static str {
    match status {
        Status::Pass => "PASS",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
    }
}

const fn status_ord(status: Status) -> u8 {
    match status {
        Status::Fail => 0,
        Status::Warn => 1,
        Status::Pass => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use routectl_router::{AliasValue, ModelEntry, ProviderEntry};

    /// RAII guard for env-var mutation in the IO tests; restores the prior
    /// value on drop so a panic cannot leak into a sibling test.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        // SAFETY: process-env mutation is unsynchronized, so every test that
        // constructs an EnvGuard MUST be #[serial_test::serial]; the two
        // async tests here that set XDG_CONFIG_HOME both carry that
        // attribute, and no non-serial sibling reads the var.
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn token_record(expires_at: u64) -> TokenRecord {
        token_record_with_refresh(expires_at, "rtok")
    }

    /// A seat whose access token is expired AND carries no refresh token,
    /// so it is genuinely unusable without a fresh login.
    fn token_record_no_refresh(expires_at: u64) -> TokenRecord {
        token_record_with_refresh(expires_at, "")
    }

    fn token_record_with_refresh(expires_at: u64, refresh: &str) -> TokenRecord {
        let json = serde_json::json!({
            "access_token": "tok",
            "refresh_token": refresh,
            "token_type": "Bearer",
            "expires_at_unix": expires_at,
            "scopes": ["user:inference"],
            "account": { "email": "a@example.com", "account_id": "acct-x" },
            "obtained_at_unix": 0,
        });
        serde_json::from_value(json).expect("valid TokenRecord json")
    }

    fn ctx(
        config: Config,
        raw_config: Option<&str>,
        probes: Vec<(&'static str, LocalProbe)>,
        seats: Vec<(String, TokenRecord)>,
    ) -> DoctorContext {
        let capability = build_capability_inputs(&config, None, Some(CatalogOverlay::default()));
        DoctorContext {
            config,
            raw_config: raw_config.map(str::to_string),
            config_load_error: None,
            probes,
            seats,
            auth_store_error: None,
            secret_checks: Vec::new(),
            orphan_secrets: Vec::new(),
            probe_results: Vec::new(),
            would_trim: None,
            now_unix: 1_000,
            binary_version: "test",
            capability,
        }
    }

    /// A config whose alias table reaches an `oauth://anthropic`-backed
    /// provider, so the anthropic inventory entry is `referenced_by_aliases`.
    fn config_referencing_anthropic() -> Config {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".to_string(),
            ProviderEntry::anthropic_api("oauth://anthropic"),
        );
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
        cfg.aliases.insert(
            "default".to_string(),
            AliasValue::Single("sonnet".to_string()),
        );
        cfg
    }

    fn find<'a>(findings: &'a [Finding], section: &str, name: &str) -> &'a Finding {
        findings
            .iter()
            .find(|f| f.section == section && f.name == name)
            .unwrap_or_else(|| panic!("missing finding {section}/{name}"))
    }

    #[test]
    fn every_warn_or_fail_finding_carries_remediation() {
        let cfg = config_referencing_anthropic();
        let context = ctx(
            cfg,
            Some("version = 1\n"),
            vec![("anthropic", LocalProbe::Missing)],
            Vec::new(),
        );
        let report = build_report(&context);
        for f in &report.findings {
            if matches!(f.status, Status::Warn | Status::Fail) {
                assert!(
                    f.remediation.is_some(),
                    "{}/{} is {:?} but has no remediation",
                    f.section,
                    f.name,
                    f.status
                );
            }
        }
    }

    #[test]
    fn activated_provider_maps_to_pass_without_remediation() {
        let cfg = config_referencing_anthropic();
        let context = ctx(
            cfg,
            Some("version = 3\n"),
            vec![("anthropic", LocalProbe::Present)],
            Vec::new(),
        );
        let report = build_report(&context);
        let f = find(&report.findings, "inventory", "anthropic");
        assert_eq!(f.status, Status::Pass);
        assert!(f.remediation.is_none());
    }

    #[test]
    fn referenced_unresolved_provider_warns_with_login_remediation() {
        let cfg = config_referencing_anthropic();
        let context = ctx(
            cfg,
            Some("version = 3\n"),
            vec![("anthropic", LocalProbe::Missing)],
            Vec::new(),
        );
        let report = build_report(&context);
        let f = find(&report.findings, "inventory", "anthropic");
        assert_eq!(f.status, Status::Warn);
        assert!(
            f.remediation
                .as_deref()
                .unwrap()
                .contains("routectl login anthropic"),
            "expected login remediation, got {:?}",
            f.remediation
        );
    }

    #[test]
    fn unreferenced_unresolved_provider_is_pass_without_remediation() {
        let context = ctx(
            Config::default(),
            Some("version = 3\n"),
            vec![("anthropic", LocalProbe::Missing)],
            Vec::new(),
        );
        let report = build_report(&context);
        let f = find(&report.findings, "inventory", "anthropic");
        assert_eq!(f.status, Status::Pass);
        assert!(f.remediation.is_none());
    }

    #[test]
    fn version_too_old_fails_with_migrate_remediation() {
        let context = ctx(
            Config::default(),
            Some("version = 1\n"),
            Vec::new(),
            Vec::new(),
        );
        let findings = section_version(&context);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].status, Status::Fail);
        assert!(
            findings[0]
                .remediation
                .as_deref()
                .unwrap()
                .contains("config migrate"),
            "expected migrate remediation, got {:?}",
            findings[0].remediation
        );
    }

    #[test]
    fn version_current_passes() {
        let raw = format!("version = {CURRENT_CONFIG_VERSION}\n");
        let context = ctx(Config::default(), Some(&raw), Vec::new(), Vec::new());
        let findings = section_version(&context);
        assert_eq!(findings[0].status, Status::Pass);
        assert!(findings[0].remediation.is_none());
    }

    #[test]
    fn present_but_broken_config_reports_fail_not_pass() {
        // preflight reads only the `version` key and returns Ok, but the
        // typed load failed for another reason: the section must Fail, not
        // pass on the raw version -- a present-but-broken config is not
        // healthy.
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.config_load_error = Some("config parse error: unknown field `bogus`".to_string());
        let findings = section_version(&context);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].status, Status::Fail);
        assert!(
            findings[0].detail.contains("bogus"),
            "expected the real load error in the detail, got {:?}",
            findings[0].detail
        );
        assert!(findings[0].remediation.is_some());
    }

    #[test]
    fn corrupted_store_reports_error_not_no_seats() {
        // A store that fails to open must surface its actionable error, not
        // be mislabeled as "nobody logged in".
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.auth_store_error = Some(
            "oauth credentials file at /x/credentials.json is corrupted: bad json".to_string(),
        );
        let findings = section_auth(&context);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].status, Status::Fail);
        assert!(
            findings[0].detail.contains("corrupted"),
            "expected the store error in the detail, got {:?}",
            findings[0].detail
        );
        assert!(
            !findings[0].detail.contains("no oauth providers"),
            "must not fall back to the generic no-seats message"
        );
        assert!(findings[0].remediation.is_some());
    }

    #[test]
    fn no_seats_warns_without_inheriting_exit_two() {
        let context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let findings = section_auth(&context);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].status, Status::Warn);
        // The no-seat state is a Warn -> overall_exit is 0, NOT whoami's 2.
        assert_eq!(overall_exit(&findings), 0);
    }

    #[test]
    fn expired_seat_warns_valid_seat_passes() {
        let seats = vec![
            ("anthropic".to_string(), token_record(2_000)),
            ("codex#work".to_string(), token_record_no_refresh(500)),
        ];
        let context = ctx(Config::default(), Some("version = 3\n"), Vec::new(), seats);
        let findings = section_auth(&context);
        assert_eq!(find(&findings, "auth", "anthropic").status, Status::Pass);
        let expired = find(&findings, "auth", "codex#work");
        assert_eq!(expired.status, Status::Warn);
        assert!(
            expired
                .remediation
                .as_deref()
                .unwrap()
                .contains("routectl login codex")
        );
    }

    #[tokio::test]
    async fn one_credential_state_agrees_across_report_surfaces() {
        // A credential with an expired access token but a stored refresh
        // token is locally usable -- it can be renewed without a fresh
        // login. Every report surface derives that one state from the shared
        // `is_locally_usable` predicate (config / inventory via the real
        // `probe_local`, auth directly), so they cannot contradict one
        // another for the same credential.
        use routectl_auth::oauth::types::CredentialsFile;

        let now = routectl_auth::oauth::types::unix_now();
        let rec = token_record(now.saturating_sub(100));

        // Seed a real store on disk and take the live probe, so the
        // config/inventory surfaces are exercised through the actual
        // derivation rather than a hand-rolled stand-in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut file = CredentialsFile::empty();
        file.upsert("anthropic", rec.clone());
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let store = OAuthStore::open(&path).await.unwrap();
        let probe = store.probe_local("anthropic").await;
        assert_eq!(
            probe,
            LocalProbe::Present,
            "an expired-but-refreshable seat probes as Present"
        );

        let cfg = config_referencing_anthropic();
        let secret_checks = gather_secret_checks(&cfg, &[("anthropic", probe)]);
        let mut context = ctx(
            cfg,
            Some("version = 3\n"),
            vec![("anthropic", probe)],
            vec![("anthropic".to_string(), rec)],
        );
        context.secret_checks = secret_checks;
        context.now_unix = now;

        let report = build_report(&context);

        // Inventory: usable -> Activated.
        assert_eq!(
            find(&report.findings, "inventory", "anthropic").status,
            Status::Pass,
            "inventory must show the credential as usable"
        );
        // Config: the oauth:// reference resolves.
        assert_eq!(
            find(&report.findings, "config", "anthropic").status,
            Status::Pass,
            "config must show the credential reference as resolving"
        );
        // Auth: logged in -- never "access token expired", which would
        // contradict the present state the other two surfaces show.
        let auth = find(&report.findings, "auth", "anthropic");
        assert_eq!(auth.status, Status::Pass, "auth must agree it is usable");
        assert!(
            !auth.detail.contains("expired"),
            "auth must not contradict the present state: {}",
            auth.detail
        );
    }

    #[test]
    fn capability_section_renders_content_not_placeholder() {
        let context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let report = build_report(&context);
        assert!(
            report.findings.iter().any(|f| f.section == "capability"),
            "capability section must now contribute findings"
        );
        let text = render_human(&report).join("\n");
        assert!(
            text.contains("[Capability]"),
            "capability header missing: {text}"
        );
        assert!(
            !text.contains("not yet available"),
            "placeholder must be gone: {text}"
        );
        // The runtime-only learned line and the honest empty-priors note are
        // both present on a default config.
        assert!(
            text.contains("runtime-only"),
            "learned line missing: {text}"
        );
        assert!(
            text.contains("no catalog capability priors present"),
            "honest empty-priors note missing: {text}"
        );
    }

    /// A config with a legacy provider list plus a new override, so the
    /// registry snapshot carries both a provider-static and an override row.
    fn config_with_overrides() -> Config {
        toml::from_str(
            "version = 3\n\
             [providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             unsupported_features = [\"web_search\"]\n\
             [capability.overrides.p]\n\
             force_supported = [\"structured_output\"]\n",
        )
        .expect("override config parses")
    }

    #[test]
    fn override_rows_render_with_source_labels_and_never_fail() {
        let context = ctx(
            config_with_overrides(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let findings = section_capability(&context);
        let overrides: Vec<&Finding> = findings.iter().filter(|f| f.name == "p").collect();
        assert_eq!(overrides.len(), 2, "one finding per override cell");
        assert!(overrides.iter().all(|f| f.status == Status::Pass));

        let route_away = overrides
            .iter()
            .find(|f| f.detail.contains("web_search"))
            .expect("provider-static route-away row");
        assert!(route_away.detail.contains("route-away"), "{route_away:?}");
        assert!(
            route_away.detail.contains("source: provider-static"),
            "{route_away:?}"
        );

        let forced = overrides
            .iter()
            .find(|f| f.detail.contains("structured_output"))
            .expect("override force-supported row");
        assert!(forced.detail.contains("force-supported"), "{forced:?}");
        assert!(forced.detail.contains("source: override"), "{forced:?}");

        // Capability findings never flip the exit code.
        assert_eq!(overall_exit(&findings), 0);
    }

    #[test]
    fn prior_cells_render_when_overlay_provides_them() {
        let config: Config = toml::from_str(
            "version = 3\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"env://ANTHROPIC_API_KEY\"\n\
             [models.opus]\n\
             provider = \"anthropic\"\n\
             upstream = \"claude-opus-4-8\"\n",
        )
        .expect("config parses");
        // An overlay cell that supplies capability data for the model's
        // (provider_kind, upstream) selector. `verified_at` is stamped TODAY
        // (via the live clock) so the cell is never stale-suppressed
        // regardless of when the suite runs.
        let today = Local::now().format("%Y-%m-%d").to_string();
        let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "cells": {
                "anthropic-api:claude-opus-4-8": {
                    "source": "user",
                    "verified_at": today,
                    "capabilities": { "web_search": true }
                }
            }
        }))
        .expect("valid overlay");

        let inputs = build_capability_inputs(&config, None, Some(overlay));
        let priors = &inputs.config.expect("config present").priors;
        let PriorLayer::Present(cells) = priors else {
            panic!("expected present priors");
        };
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].nickname, "opus");
        assert!(cells[0].selector.contains("claude-opus-4-8"));
        // The overlay cell wins the merge -> source is User.
        assert_eq!(cells[0].source, Source::User);
        // The overlay-supplied capability is present and true (baked keys the
        // overlay does not mention merge through unchanged).
        assert!(
            cells[0]
                .capabilities
                .contains(&("web_search".to_string(), true)),
            "web_search prior missing: {:?}",
            cells[0].capabilities
        );
    }

    #[test]
    fn stale_catalog_cell_yields_no_prior() {
        // A Present overlay cell whose verified_at is far in the past is
        // stale -> suppressed as unknown, never rendered as an authoritative
        // prior (spec 6c). An unparseable stamp is treated as stale too.
        let config: Config = toml::from_str(
            "version = 3\n\
             [providers.anthropic]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"env://ANTHROPIC_API_KEY\"\n\
             [models.opus]\n\
             provider = \"anthropic\"\n\
             upstream = \"claude-opus-4-8\"\n",
        )
        .expect("config parses");
        let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "cells": {
                "anthropic-api:claude-opus-4-8": {
                    "source": "user",
                    "verified_at": "2000-01-01",
                    "capabilities": { "web_search": true }
                }
            }
        }))
        .expect("valid overlay");
        let cells = derive_prior_cells(&config, &overlay);
        assert!(cells.is_empty(), "stale cell must yield no prior");
    }

    #[test]
    fn absent_catalog_cell_yields_no_prior_and_no_crash() {
        // A model whose provider is unknown resolves to an empty
        // provider_kind that matches no baked cell -> Missing -> no prior.
        let config: Config = toml::from_str(
            "version = 3\n\
             [models.ghost]\n\
             provider = \"nope\"\n\
             upstream = \"whatever\"\n",
        )
        .expect("config parses");
        let cells = derive_prior_cells(&config, &CatalogOverlay::default());
        assert!(cells.is_empty(), "absent cell must yield no prior");
    }

    #[test]
    fn learned_line_is_runtime_only_with_no_counts() {
        let f = learned_line();
        assert_eq!(f.status, Status::Pass);
        assert!(f.detail.contains("runtime-only"));
        assert!(f.detail.contains("serve logs"));
        // No fabricated counts.
        assert!(
            !f.detail.chars().any(|c| c.is_ascii_digit()),
            "learned line must carry no counts: {}",
            f.detail
        );
    }

    #[test]
    fn config_load_error_routes_through_redactor_to_unavailable() {
        // A TOML parse error whose diagnostic inlines a `literal:` secret in
        // the source-line preview. build_capability_inputs receives the
        // ALREADY-redacted string (redact_config_load_error), mirroring the
        // gather path; assert the secret never reaches the finding.
        let raw = "config parse error in `/home/x/config.toml`: TOML parse error at line 2, column 1\n  |\n2 | api_key_ref = \"literal:sk-super-secret\"\n  | ^\ninvalid type: string, expected integer\n";
        let redacted = redact_config_load_error(raw);
        let inputs = build_capability_inputs(&Config::default(), Some(redacted), None);
        let context = DoctorContext {
            capability: inputs,
            ..ctx(Config::default(), Some("x"), Vec::new(), Vec::new())
        };
        let findings = section_capability(&context);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].name, "panel");
        assert_eq!(findings[0].status, Status::Warn);
        assert!(findings[0].detail.contains("unavailable"));
        assert!(
            !findings[0].detail.contains("sk-super-secret"),
            "secret leaked into unavailable finding: {}",
            findings[0].detail
        );
        // Never flips the exit code.
        assert_eq!(overall_exit(&findings), 0);
    }

    #[test]
    fn overlay_unavailable_keeps_override_rows_marks_priors_unavailable() {
        // overlay = None simulates an unreadable overlay: override rows still
        // render, priors are marked unavailable.
        let inputs = build_capability_inputs(&config_with_overrides(), None, None);
        let context = DoctorContext {
            capability: inputs,
            ..ctx(Config::default(), Some("x"), Vec::new(), Vec::new())
        };
        let findings = section_capability(&context);
        assert!(
            findings.iter().any(|f| f.name == "p"),
            "override rows must still render on overlay failure"
        );
        let priors = findings
            .iter()
            .find(|f| f.name == "catalog priors")
            .expect("priors finding");
        assert_eq!(priors.status, Status::Warn);
        assert!(priors.detail.contains("unavailable"));
        assert_eq!(overall_exit(&findings), 0);
    }

    #[test]
    fn legacy_nudge_warns_once_with_guarded_phrasing() {
        let config: Config = toml::from_str(
            "version = 3\n\
             [providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             unsupported_features = [\"web_search\"]\n\
             [bedrock]\n\
             allowed_betas = [\"some-beta\"]\n",
        )
        .expect("config parses");
        let keys = present_legacy_capability_keys(&config);
        let nudge = legacy_nudge(&keys).expect("nudge present");
        assert_eq!(nudge.status, Status::Warn);
        assert!(nudge.detail.contains("unsupported_features"), "{nudge:?}");
        assert!(nudge.detail.contains("allowed_betas"), "{nudge:?}");
        // Exact guarded phrasing.
        assert!(
            nudge
                .detail
                .contains("the override layer replaces these via config migrate"),
            "{nudge:?}"
        );
        assert!(
            nudge.detail.contains(
                "the learner discovers use-time rejections automatically; these lists remain \
                 operator-owned until the next schema version"
            ),
            "{nudge:?}"
        );
        // Must not imply the lists are safe to delete.
        assert!(!nudge.detail.contains("delete"), "{nudge:?}");
        assert!(!nudge.detail.contains("remove"), "{nudge:?}");
        assert!(
            nudge
                .remediation
                .as_deref()
                .unwrap()
                .contains("config migrate")
        );
    }

    #[test]
    fn legacy_nudge_absent_without_legacy_lists() {
        assert!(legacy_nudge(&[]).is_none());
        // A config with a new override but no legacy list emits no nudge.
        let config: Config = toml::from_str(
            "version = 3\n\
             [providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n",
        )
        .expect("config parses");
        let context = ctx(config, Some("version = 3\n"), Vec::new(), Vec::new());
        let findings = section_capability(&context);
        assert!(
            findings.iter().all(|f| f.name != "legacy keys"),
            "no nudge without a legacy list"
        );
    }

    #[test]
    fn schema_version_is_two() {
        assert_eq!(SCHEMA_VERSION, 2);
        let context = ctx(
            config_with_overrides(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let report = build_report(&context);
        assert_eq!(report.schema_version, 2);
        // JSON mode carries the same capability content as the human render.
        let value = serde_json::to_value(&report).expect("serialize");
        let blob = value.to_string();
        assert!(blob.contains("route-away"), "json missing override verdict");
        assert!(
            blob.contains("provider-static"),
            "json missing source label"
        );
        assert!(blob.contains("runtime-only"), "json missing learned line");
    }

    #[test]
    fn human_render_carries_status_battery_and_remediation() {
        let cfg = config_referencing_anthropic();
        let context = ctx(
            cfg,
            Some("version = 1\n"),
            vec![("anthropic", LocalProbe::Missing)],
            Vec::new(),
        );
        let report = build_report(&context);
        let text = render_human(&report).join("\n");
        assert!(text.contains("FAIL"), "expected a FAIL line: {text}");
        assert!(text.contains("WARN"), "expected a WARN line: {text}");
        assert!(text.contains("fix:"), "expected a remediation line: {text}");
        assert!(
            text.contains("summary: PASS"),
            "expected a summary line: {text}"
        );
    }

    #[test]
    fn findings_sorted_deterministically_and_exit_matches() {
        let cfg = config_referencing_anthropic();
        let a = build_report(&ctx(
            cfg.clone(),
            Some("version = 3\n"),
            vec![
                ("anthropic", LocalProbe::Present),
                ("codex", LocalProbe::Missing),
            ],
            Vec::new(),
        ));
        let b = build_report(&ctx(
            cfg,
            Some("version = 3\n"),
            // Reversed probe order must not change the sorted output.
            vec![
                ("codex", LocalProbe::Missing),
                ("anthropic", LocalProbe::Present),
            ],
            Vec::new(),
        ));
        assert_eq!(a.findings, b.findings, "sort must be order-independent");
        assert_eq!(overall_exit(&a.findings), overall_exit(&b.findings));
        // No Fail here (v3 config, present anthropic) -> exit 0.
        assert_eq!(overall_exit(&a.findings), 0);
    }

    #[test]
    fn json_report_carries_schema_version_findings_and_panels() {
        let context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let report = build_report(&context);
        let value: serde_json::Value =
            serde_json::to_value(&report).expect("serialize doctor report");
        let obj = value.as_object().expect("top-level object");
        assert!(obj.contains_key("schema_version"));
        assert!(obj["findings"].is_array());
        assert!(obj.contains_key("panels"));
    }

    fn a_panel() -> WouldTrimPanel {
        WouldTrimPanel {
            candidate_requests: 5,
            would_trim_tokens: 42_000,
            verdict_met: 2,
            verdict_unmet: 1,
            verdict_cold: 1,
            verdict_unpriced: 1,
        }
    }

    /// The probe section is one finding per configured provider, each mapped
    /// through the shared `probe_finding` seam; every Fail/Warn carries a
    /// remediation.
    #[test]
    fn probe_section_is_one_finding_per_provider_via_shared_mapping() {
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.probe_results = vec![
            ("reach".to_string(), ProbeOutcome::Reachable),
            (
                "authf".to_string(),
                ProbeOutcome::AuthFailed("not logged in".into()),
            ),
            ("nofree".to_string(), ProbeOutcome::UnsupportedFreeProbe),
        ];
        let findings = section_probe(&context);
        assert_eq!(findings.len(), 3, "one finding per provider");
        assert!(findings.iter().all(|f| f.section == "probe"));
        assert_eq!(find(&findings, "probe", "reach").status, Status::Pass);
        assert_eq!(find(&findings, "probe", "authf").status, Status::Fail);
        for f in &findings {
            if matches!(f.status, Status::Warn | Status::Fail) {
                assert!(
                    f.remediation.is_some(),
                    "{} probe finding must carry a remediation",
                    f.name
                );
            }
        }
    }

    /// A forwarded provider probes as Skipped -> an informational Pass line,
    /// not a WARN, and carries no remediation.
    #[test]
    fn forwarded_probe_is_informational_pass_not_warn() {
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.probe_results =
            vec![("fwd".to_string(), ProbeOutcome::Skipped("forwarded".into()))];
        let findings = section_probe(&context);
        let f = find(&findings, "probe", "fwd");
        assert_eq!(
            f.status,
            Status::Pass,
            "forwarded is informational, not WARN"
        );
        assert!(f.remediation.is_none());
    }

    /// A provider kind with no free reachability probe surfaces as WARN with a
    /// "cannot verify" reason -- never a silent PASS.
    #[test]
    fn no_free_endpoint_probe_warns_not_silent_pass() {
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.probe_results = vec![("nofree".to_string(), ProbeOutcome::UnsupportedFreeProbe)];
        let findings = section_probe(&context);
        let f = find(&findings, "probe", "nofree");
        assert_eq!(
            f.status,
            Status::Warn,
            "unsupported probe must WARN, not silently pass"
        );
        assert!(f.remediation.is_some());
    }

    /// Exit code is order-independent with the probe section present: a probe
    /// AuthFailed keeps the run nonzero regardless of provider ordering.
    #[test]
    fn probe_section_keeps_exit_deterministic_across_ordering() {
        let forward = {
            let mut c = ctx(
                Config::default(),
                Some("version = 3\n"),
                Vec::new(),
                Vec::new(),
            );
            c.probe_results = vec![
                ("alpha".to_string(), ProbeOutcome::Reachable),
                ("zeta".to_string(), ProbeOutcome::AuthFailed("x".into())),
            ];
            build_report(&c)
        };
        let reversed = {
            let mut c = ctx(
                Config::default(),
                Some("version = 3\n"),
                Vec::new(),
                Vec::new(),
            );
            c.probe_results = vec![
                ("zeta".to_string(), ProbeOutcome::AuthFailed("x".into())),
                ("alpha".to_string(), ProbeOutcome::Reachable),
            ];
            build_report(&c)
        };
        assert_eq!(
            forward.findings, reversed.findings,
            "probe ordering must not change the sorted findings"
        );
        assert_eq!(
            overall_exit(&forward.findings),
            overall_exit(&reversed.findings)
        );
        assert_ne!(
            overall_exit(&forward.findings),
            0,
            "a probe AuthFailed must keep the run nonzero"
        );
    }

    /// A computed would-trim panel is wired into `panels.would_trim` and
    /// rendered in the human battery.
    #[test]
    fn would_trim_panel_populates_report_and_human_render() {
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.would_trim = Some(a_panel());
        let report = build_report(&context);
        assert_eq!(report.panels.would_trim, Some(a_panel()));
        let text = render_human(&report).join("\n");
        assert!(
            text.contains("would-trim"),
            "human battery must render the panel: {text}"
        );
        assert!(text.contains("5 reqs"), "panel counts must appear: {text}");
    }

    /// A no-data window yields `panels.would_trim = None` and the human battery
    /// omits the would-trim block entirely.
    #[test]
    fn no_data_would_trim_is_none_and_omitted_from_human_render() {
        let context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let report = build_report(&context);
        assert_eq!(report.panels.would_trim, None);
        let text = render_human(&report).join("\n");
        assert!(
            !text.contains("would-trim"),
            "no panel -> no would-trim block: {text}"
        );
    }

    /// The would-trim panel appears under `panels.would_trim` in `--json`.
    #[test]
    fn would_trim_panel_serializes_under_panels_in_json() {
        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.would_trim = Some(a_panel());
        let report = build_report(&context);
        let value: serde_json::Value =
            serde_json::to_value(&report).expect("serialize doctor report");
        let panel = &value["panels"]["would_trim"];
        assert_eq!(panel["candidate_requests"], serde_json::json!(5));
        assert_eq!(panel["would_trim_tokens"], serde_json::json!(42_000));
    }

    fn snapshot_dir(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(cur) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&cur) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.insert(path, bytes);
                }
            }
        }
        out
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn full_run_leaves_config_dir_byte_identical() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(&config_path, b"version = 3\n").unwrap();

        // Seed a credentials file the store will read, at 0600 (the loader
        // refuses a world-readable credentials file). A doctor run must
        // leave it byte-identical.
        let creds_path = cfg_dir.join("credentials.json");
        std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let before = snapshot_dir(tmp.path());
        let code = run(&config_path, false).await;
        let after = snapshot_dir(tmp.path());

        assert_eq!(
            before, after,
            "a doctor run must not mutate or create any file"
        );
        // v3 config, empty credentials store: no Fail -> exit 0.
        assert_eq!(code, 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn too_old_config_is_not_stamped_and_reports_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        let original = b"version = 1\n";
        std::fs::write(&config_path, original).unwrap();

        let context = gather_context(&config_path).await;
        let report = build_report(&context);

        let after = std::fs::read(&config_path).unwrap();
        assert_eq!(
            after.as_slice(),
            original,
            "version preflight must not stamp the file"
        );
        let f = find(&report.findings, "version", "config schema");
        assert_eq!(f.status, Status::Fail);
        assert!(f.remediation.as_deref().unwrap().contains("config migrate"));
        assert_ne!(overall_exit(&report.findings), 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn present_but_unparseable_config_yields_fail_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        // Parseable `version` key, but an unknown field the typed load
        // (deny_unknown_fields) rejects -- the raw preflight passes yet the
        // config is broken.
        std::fs::write(&config_path, b"version = 3\nbogus_key = true\n").unwrap();

        let context = gather_context(&config_path).await;
        let report = build_report(&context);

        let f = find(&report.findings, "version", "config schema");
        assert_eq!(
            f.status,
            Status::Fail,
            "a present-but-broken config must not report all-Pass"
        );
        assert_ne!(overall_exit(&report.findings), 0);
    }

    /// A config referencing an anthropic provider through an alias is clean:
    /// the config section reports a single Pass for the validator suite.
    #[test]
    fn clean_config_passes_validation() {
        let context = ctx(
            config_referencing_anthropic(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        let findings = section_config(&context);
        let f = find(&findings, "config", "validation");
        assert_eq!(f.status, Status::Pass);
        assert!(f.remediation.is_none());
    }

    /// A model pointing at a provider absent from `[providers]` is a semantic
    /// validation error -> a Fail in the config section with remediation.
    #[test]
    fn semantic_validation_error_reports_fail() {
        let mut cfg = Config::default();
        cfg.models.insert(
            "sonnet".to_string(),
            ModelEntry::new("ghost", "claude-sonnet-4-5"),
        );
        let context = ctx(cfg, Some("version = 3\n"), Vec::new(), Vec::new());
        let findings = section_config(&context);
        let f = find(&findings, "config", "validation");
        assert_eq!(f.status, Status::Fail);
        assert!(f.remediation.is_some());
        assert!(
            f.detail.contains("ghost"),
            "expected the offending provider in the detail, got {:?}",
            f.detail
        );
    }

    /// An `oauth://` ref resolves via the pre-gathered local probe: Present
    /// -> Pass; Missing -> Warn with a `routectl login` remediation. The
    /// classification reads the probe only -- it never resolves the token.
    #[test]
    fn oauth_secret_present_passes_missing_warns_with_login() {
        let cfg = config_referencing_anthropic();

        let present = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Present)]);
        let f = secret_finding(&present[0]);
        assert_eq!(f.status, Status::Pass);
        assert!(f.remediation.is_none());

        let missing = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Missing)]);
        let f = secret_finding(&missing[0]);
        assert_eq!(f.status, Status::Warn);
        assert!(
            f.remediation
                .as_deref()
                .unwrap()
                .contains("routectl login anthropic"),
            "expected a login remediation, got {:?}",
            f.remediation
        );
    }

    /// An `oauth://` ref whose local probe reports an expired, non-revivable
    /// credential is a Warn carrying a `routectl login` remediation.
    #[test]
    fn oauth_secret_expired_warns_with_login() {
        let cfg = config_referencing_anthropic();
        let checks = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Expired)]);
        let f = secret_finding(&checks[0]);
        assert_eq!(f.status, Status::Warn);
        assert!(
            f.remediation
                .as_deref()
                .unwrap()
                .contains("routectl login anthropic"),
            "expected a login remediation, got {:?}",
            f.remediation
        );
    }

    /// A `file://` ref that exists but is not a readable regular file (here a
    /// directory) is a Warn that names the scheme, never the path.
    #[test]
    fn unreadable_file_ref_warns_without_leaking_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.providers.insert(
            "compat".to_string(),
            ProviderEntry::openai_compat(
                "https://example.test/v1",
                format!("file://{}", tmp.path().display()),
            ),
        );
        let checks = gather_secret_checks(&cfg, &[]);
        let f = secret_finding(&checks[0]);
        assert_eq!(f.status, Status::Warn);
        assert!(f.remediation.is_some());
        assert!(
            f.detail.contains("file://") && !f.detail.contains(&tmp.path().display().to_string()),
            "detail must name the scheme, never the path: {:?}",
            f.detail
        );
    }

    /// An `env://` ref that resolves is a Pass; an unset one is a Warn that
    /// names the scheme -- never the variable name or value.
    #[test]
    #[serial_test::serial]
    fn env_ref_resolves_passes_unset_warns_without_leaking_name() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "compat".to_string(),
            ProviderEntry::openai_compat(
                "https://example.test/v1",
                "env://ROUTECTL_DOCTOR_TEST_KEY",
            ),
        );

        {
            let _var = EnvGuard::set("ROUTECTL_DOCTOR_TEST_KEY", "sk-secret-value");
            let checks = gather_secret_checks(&cfg, &[]);
            let f = secret_finding(&checks[0]);
            assert_eq!(f.status, Status::Pass);
        }

        // The guard above restored the variable to its prior (unset) state on
        // drop, so the ref no longer resolves here.
        let checks = gather_secret_checks(&cfg, &[]);
        let f = secret_finding(&checks[0]);
        assert_eq!(f.status, Status::Warn);
        assert!(f.remediation.is_some());
        assert!(
            f.detail.contains("env://") && !f.detail.contains("ROUTECTL_DOCTOR_TEST_KEY"),
            "detail must name the scheme, never the variable: {:?}",
            f.detail
        );
    }

    /// A missing `file://` secret path is a Warn that names the scheme, not
    /// the path (a full `file://` ref string must never reach a finding).
    #[test]
    fn missing_file_ref_warns_without_leaking_path() {
        let secret_path = "/nonexistent/routectl-doctor-secret-marker";
        let mut cfg = Config::default();
        cfg.providers.insert(
            "compat".to_string(),
            ProviderEntry::openai_compat(
                "https://example.test/v1",
                format!("file://{secret_path}"),
            ),
        );
        let checks = gather_secret_checks(&cfg, &[]);
        let f = secret_finding(&checks[0]);
        assert_eq!(f.status, Status::Warn);
        assert!(f.remediation.is_some());
        assert!(
            f.detail.contains("file://") && !f.detail.contains("routectl-doctor-secret-marker"),
            "detail must name the scheme, never the path: {:?}",
            f.detail
        );
    }

    /// No secret value or full ref string reaches any config-section message,
    /// across the redacting (`literal:`) and pointer (`file://`) schemes.
    #[test]
    fn config_findings_never_leak_secret_material() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "compat".to_string(),
            ProviderEntry::openai_compat("https://example.test/v1", "literal:hunter2-do-not-leak"),
        );
        cfg.providers.insert(
            "compat-file".to_string(),
            ProviderEntry::openai_compat(
                "https://example.test/v1",
                "file:///nonexistent/leaky-secret-path",
            ),
        );
        let mut context = ctx(cfg.clone(), Some("version = 3\n"), Vec::new(), Vec::new());
        context.secret_checks = gather_secret_checks(&cfg, &[]);
        let findings = section_config(&context);
        let blob = findings
            .iter()
            .map(|f| format!("{} {}", f.detail, f.remediation.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!blob.contains("hunter2"), "leaked literal value: {blob}");
        assert!(
            !blob.contains("leaky-secret-path"),
            "leaked file path: {blob}"
        );
    }

    /// A managed secret file no provider references surfaces as a Warn and is
    /// NOT deleted -- the scan is a read-only directory diff.
    #[test]
    #[serial_test::serial]
    fn orphan_managed_secret_warns_and_is_not_deleted() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let secret_dir = tmp.path().join("routectl").join("secrets");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let orphan = secret_dir.join("stranded-key");
        std::fs::write(&orphan, b"secret-bytes").unwrap();
        std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600)).unwrap();

        let orphans = gather_orphan_secrets(&Config::default());
        assert_eq!(orphans, vec!["stranded-key".to_string()]);

        let mut context = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        context.orphan_secrets = orphans;
        let findings = section_secret_orphans(&context);
        let f = find(&findings, "secrets", "stranded-key");
        assert_eq!(f.status, Status::Warn);
        assert!(f.remediation.is_some());
        assert!(
            orphan.exists(),
            "the orphan scan must never delete a stored secret"
        );
    }

    /// A file referenced by a `file://` provider ref is not an orphan.
    #[test]
    #[serial_test::serial]
    fn referenced_managed_secret_is_not_an_orphan() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let secret_dir = tmp.path().join("routectl").join("secrets");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let used = secret_dir.join("used-key");
        std::fs::write(&used, b"secret-bytes").unwrap();
        std::fs::set_permissions(&used, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut cfg = Config::default();
        cfg.providers.insert(
            "compat".to_string(),
            ProviderEntry::openai_compat(
                "https://example.test/v1",
                format!("file://{}", used.display()),
            ),
        );
        assert!(gather_orphan_secrets(&cfg).is_empty());
    }

    /// End-to-end: a doctor run over a config with an `oauth://` provider and
    /// a populated managed secret directory leaves the config dir, the
    /// credentials record, and the secret dir byte-identical -- the oauth
    /// presence check uses `probe_local` (no refresh) and the orphan scan is
    /// read-only.
    #[tokio::test]
    #[serial_test::serial]
    async fn full_run_leaves_secret_dir_byte_identical() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            b"version = 3\n[providers.anthropic]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n",
        )
        .unwrap();

        let creds_path = cfg_dir.join("credentials.json");
        std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let secret_dir = cfg_dir.join("secrets");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let orphan = secret_dir.join("stranded-key");
        std::fs::write(&orphan, b"secret-bytes").unwrap();
        std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600)).unwrap();

        let before = snapshot_dir(tmp.path());
        let context = gather_context(&config_path).await;
        let report = build_report(&context);
        let after = snapshot_dir(tmp.path());

        assert_eq!(
            before, after,
            "a doctor run must not mutate, create, or delete any file"
        );
        let orphan_finding = find(&report.findings, "secrets", "stranded-key");
        assert_eq!(orphan_finding.status, Status::Warn);
        assert!(orphan.exists(), "the orphan secret must survive the run");
    }

    #[test]
    fn store_open_error_never_discloses_the_full_credentials_path() {
        // Every path-bearing variant embeds the FULL store path in its Display
        // (SchemaMismatch/CorruptedFile via `path`, Io via the interpolated
        // message); the sanitized message keeps only the failure class +
        // basename.
        let full = "/home/someone/.config/routectl/credentials.json";
        let schema = sanitize_store_open_error(&OAuthError::SchemaMismatch {
            found: 1,
            expected: 2,
            path: full.to_string(),
        });
        assert!(!schema.contains(full), "path leaked: {schema}");
        assert!(!schema.contains("/home/someone"), "dir leaked: {schema}");
        assert!(schema.contains("credentials.json"), "{schema}");

        let corrupt = sanitize_store_open_error(&OAuthError::CorruptedFile {
            path: full.to_string(),
            detail: "trailing comma at line 4".to_string(),
        });
        assert!(!corrupt.contains(full), "path leaked: {corrupt}");
        assert!(!corrupt.contains("/home/someone"), "dir leaked: {corrupt}");
        assert!(corrupt.contains("credentials.json"), "{corrupt}");

        // The Io variant interpolates the path mid-message, so it too must be
        // reduced to a class-only string.
        let io = sanitize_store_open_error(&OAuthError::Io(format!(
            "credentials file {full} has permissions 644; use chmod 600"
        )));
        assert!(!io.contains(full), "path leaked: {io}");
        assert!(!io.contains("/home/someone"), "dir leaked: {io}");
        assert!(io.contains("credentials.json"), "{io}");
    }

    #[test]
    fn store_open_error_wildcard_is_a_path_free_class_message() {
        // A variant the sanitizer does not special-case must NOT forward its
        // raw Display (the enum is #[non_exhaustive] and a future variant could
        // embed a path); the fallback is a fixed class-only message.
        let msg = sanitize_store_open_error(&OAuthError::Internal(
            "/home/someone/.config/routectl/credentials.json is wedged".to_string(),
        ));
        assert!(!msg.contains("/home/someone"), "path leaked: {msg}");
        assert!(msg.contains("credentials.json"), "{msg}");
    }

    #[test]
    fn config_load_error_redacts_a_secret_bearing_parse_error() {
        // The loader wraps the toml diagnostic as `config parse error in
        // <path>: TOML parse error ...`; a mistyped secret in the numeric
        // `port` field lands verbatim in the `invalid type:` clause.
        let raw = "config parse error in `/home/someone/.config/routectl/config.toml`: \
                   TOML parse error at line 5, column 8\n  |\n5 | port = \"sk-live-LEAKED\"\n  \
                   |        ^^^^^^^^^^^^^^^^\ninvalid type: string \"sk-live-LEAKED\", expected u16";
        let redacted = redact_config_load_error(raw);
        assert!(!redacted.contains("sk-live-LEAKED"), "{redacted}");
        // Dropping everything before the toml header also drops the wrapping
        // config path.
        assert!(!redacted.contains("/home/someone"), "{redacted}");
        assert!(redacted.contains("line 5, column 8"), "{redacted}");
        assert!(redacted.contains("port"), "{redacted}");
    }

    #[test]
    fn config_load_error_sanitizes_path_bearing_non_parse_errors() {
        // The loader formats an unreadable config and a catalog-overlay failure
        // with the FULL filesystem path; both must collapse to a path-free
        // class message.
        let unreadable = "cannot read config `/home/someone/.config/routectl/config.toml`: \
                          Permission denied (os error 13)";
        let redacted = redact_config_load_error(unreadable);
        assert!(!redacted.contains("/home/someone"), "{redacted}");
        assert!(!redacted.contains("config.toml"), "{redacted}");

        let overlay = "catalog overlay load error: failed to read \
                       /home/someone/.config/routectl/catalog_overlay.json: broken";
        let redacted = redact_config_load_error(overlay);
        assert!(!redacted.contains("/home/someone"), "{redacted}");
        assert!(!redacted.contains("catalog_overlay.json"), "{redacted}");
    }

    #[test]
    fn config_load_error_keeps_a_path_free_error_verbatim() {
        // A version/legacy-key rejection carries no path or value, so it stays
        // actionable rather than being reduced to a class message.
        let raw = "config schema v9, this binary expects v3";
        assert_eq!(redact_config_load_error(raw), raw);
    }

    #[test]
    fn rendered_report_leaks_neither_a_config_secret_nor_a_store_path() {
        // End-to-end through BOTH renderers: a version finding built from a
        // redacted parse error and an auth finding built from a sanitized
        // store-open error must not surface the secret or the full path in the
        // human render OR the JSON serialization.
        const SECRET: &str = "sk-live-REPORT-LEAK";
        const FULL_PATH: &str = "/home/someone/.config/routectl/credentials.json";

        let raw_load_error = format!(
            "config parse error in `/home/someone/.config/routectl/config.toml`: \
             TOML parse error at line 5, column 8\n  |\n5 | port = \"{SECRET}\"\n  \
             |        ^^^^^^\ninvalid type: string \"{SECRET}\", expected u16"
        );
        let raw_store_error =
            OAuthError::Io(format!("open {FULL_PATH}: Permission denied (os error 13)"));

        let context = DoctorContext {
            config: Config::default(),
            raw_config: Some("version = 3\n".to_string()),
            config_load_error: Some(redact_config_load_error(&raw_load_error)),
            probes: Vec::new(),
            seats: Vec::new(),
            auth_store_error: Some(sanitize_store_open_error(&raw_store_error)),
            secret_checks: Vec::new(),
            orphan_secrets: Vec::new(),
            probe_results: Vec::new(),
            would_trim: None,
            now_unix: 1_000,
            binary_version: "test",
            capability: build_capability_inputs(
                &Config::default(),
                Some(redact_config_load_error(&raw_load_error)),
                None,
            ),
        };
        let report = build_report(&context);

        // Both the version and auth findings must be present and failing.
        assert_eq!(
            find(&report.findings, "version", "config schema").status,
            Status::Fail
        );
        assert_eq!(
            find(&report.findings, "auth", "oauth credentials").status,
            Status::Fail
        );

        let human = render_human(&report).join("\n");
        let json = serde_json::to_string(&report).expect("report serializes");
        for surface in [&human, &json] {
            assert!(!surface.contains(SECRET), "config secret leaked: {surface}");
            assert!(!surface.contains(FULL_PATH), "store path leaked: {surface}");
            assert!(
                !surface.contains("/home/someone"),
                "a directory leaked: {surface}"
            );
        }
    }

    /// The no-network section list is exactly [`SECTIONS`] minus the `probe`
    /// entry, order preserved: `probe` is present in the full list and absent
    /// from the no-network one.
    #[test]
    fn no_network_sections_are_sections_minus_probe() {
        let full: Vec<&str> = SECTIONS.iter().map(|(k, _)| *k).collect();
        let no_net: Vec<&str> = NO_NETWORK_SECTIONS.iter().map(|(k, _)| *k).collect();
        assert!(full.contains(&"probe"), "SECTIONS must contain probe");
        assert!(
            !no_net.contains(&"probe"),
            "no-network sections must omit probe"
        );
        let expected: Vec<&str> = full.iter().copied().filter(|k| *k != "probe").collect();
        assert_eq!(
            no_net, expected,
            "no-network sections must equal SECTIONS minus probe, order preserved"
        );
    }

    /// The no-network gather leaves `probe_results` empty (no
    /// `gather_probe_results` -> no `CompositeStore`/`probe_all` dial), and a
    /// report built from it carries no `probe` section rows.
    #[tokio::test]
    #[serial_test::serial]
    async fn gather_context_no_network_yields_no_probe_results() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            b"version = 3\n[providers.anthropic]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n",
        )
        .unwrap();
        let creds_path = cfg_dir.join("credentials.json");
        std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let context = gather_context_no_network(&config_path).await;
        assert!(
            context.probe_results.is_empty(),
            "the no-network gather must not populate probe results"
        );

        let report = build_report_no_network(&context);
        assert!(
            report.findings.iter().all(|f| f.section != "probe"),
            "a no-network report must carry no probe findings"
        );
    }

    /// `build_report_no_network` keeps the schema version, emits no `probe`
    /// rows, and its findings are exactly the network report's non-probe
    /// findings for the same context.
    #[test]
    fn build_report_no_network_matches_network_minus_probe() {
        let mut context = ctx(
            config_referencing_anthropic(),
            Some("version = 3\n"),
            vec![("anthropic", LocalProbe::Present)],
            Vec::new(),
        );
        context.probe_results = vec![("anthropic".to_string(), ProbeOutcome::Reachable)];

        let network = build_report(&context);
        let no_net = build_report_no_network(&context);

        assert_eq!(no_net.schema_version, 2);
        assert!(
            no_net.findings.iter().all(|f| f.section != "probe"),
            "no-network report must have no probe rows"
        );
        assert!(
            network.findings.iter().any(|f| f.section == "probe"),
            "the network report must have probe rows for the same context"
        );

        let network_non_probe: Vec<&Finding> = network
            .findings
            .iter()
            .filter(|f| f.section != "probe")
            .collect();
        let no_net_all: Vec<&Finding> = no_net.findings.iter().collect();
        assert_eq!(
            no_net_all, network_non_probe,
            "non-probe sections must be byte-identical across the two builders"
        );
    }

    /// The shared gather body cannot drift: the network and no-network entry
    /// points, run against the same fixture, produce identical non-probe
    /// findings. A field added in one path but not the other would break this.
    #[tokio::test]
    #[serial_test::serial]
    async fn network_and_no_network_gather_agree_outside_probe() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(&config_path, b"version = 3\n").unwrap();
        let creds_path = cfg_dir.join("credentials.json");
        std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let network = gather_context(&config_path).await;
        let no_network = gather_context_no_network(&config_path).await;

        // Both contexts fed through the no-network builder must agree: the
        // shared gather body produced the same non-probe inputs on both paths.
        let from_network = build_report_no_network(&network);
        let from_no_network = build_report_no_network(&no_network);
        assert_eq!(
            from_network.findings, from_no_network.findings,
            "the shared gather body must yield identical non-probe findings on both entry points"
        );
    }
}
