//! Doctor command entry + report assembly.
//!
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

mod gather;
mod matrix;
mod render;
mod sections;

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;

use std::path::Path;
use std::time::Instant;

use routectl_auth::LocalProbe;
use routectl_auth::oauth::types::TokenRecord;
use routectl_core::ProbeOutcome;
use routectl_router::{
    CatalogImportState, Config, DoctorPanels, DoctorReport, Finding, LearnedRegistryEntry, Status,
    WouldTrimPanel, overall_exit,
};

use self::gather::{SecretCheck, gather_context};
use self::render::render_human;
use self::sections::{
    section_auth, section_capability, section_config, section_freshness, section_inventory,
    section_knobs, section_pricing, section_probe, section_seat_orphans, section_seat_pools,
    section_secret_orphans, section_version,
};

pub(crate) use self::gather::{gather_context_no_network, sanitize_store_open_error};

/// UNSTABLE report schema version. Bumped on ANY structural or semantic
/// change a consumer would care about -- including an ADDITIVE one, since the
/// report JSON is explicitly human-facing and non-contractual. Bump when a
/// section's finding shape, a panel field, or the meaning of an existing
/// field changes.
///
/// v1 -> v2: the reserved `capability` section became a real producer
/// (override rows, catalog priors, the runtime-only learned line, and the
/// legacy-key migrate nudge).
///
/// v2 -> v3: the status doctor panel's per-target reachability is derived
/// from the last settled dispatch outcome (three states: reachable / unknown
/// / degraded) instead of the coarse circuit phase.
///
/// v3 -> v4: the capability section's override / prior / learned findings are
/// superseded by the structured `capability_matrix` panel (lanes by
/// capability keys, one resolved display cell each), and the freshness
/// section joins the report.
///
/// v4 -> v5: the `seats` section joins the report, one finding per stored
/// OAuth seat no provider entry references.
///
/// v5 -> v6: the additive `pools` section joins the report, one purely
/// informational Pass finding per `oauth://` reference describing how many
/// stored seats it resolves to and which seat-selection strategy applies.
///
/// v6 -> v7: the `pools` section's finding SET and statuses change. Config
/// schema version 4 made `[pools.<name>]` the multi-seat shape, so a finding
/// is now keyed on the POOL (its members and their seats, the strategy, the
/// growth marker) with the per-ref rows reduced to the entries no pool claims.
/// The section can also now Warn (a member with no stored credential) and Fail
/// (a pool with none), so it is no longer exit-code-neutral.
///
/// v7 -> v8: the `config` section renders the validator suite's WARNING half
/// as Warn findings alongside the error half, and its single Pass finding now
/// requires BOTH halves empty.
///
/// v8 -> v9: the additive `pricing` section joins the report, one finding per
/// configured model naming where its cost rates come from (an operator
/// `[registry]` row, the baked catalog, a managed subscription, or nothing).
///
/// v9 -> v10: the additive `knobs` section joins the report, one finding per
/// configured model naming where its outbound `max_output_tokens` ceiling comes
/// from (the operator's own `[models.X]` value, the catalog fill, or the
/// egress's hardcoded baseline).
const SCHEMA_VERSION: u32 = 10;

/// A section-producer: pure mapping of the read-only [`DoctorContext`] to a
/// section's findings.
type SectionFn = fn(&DoctorContext) -> Vec<Finding>;

/// The ordered aggregator sequence. THE extension point: a later section
/// (probe, config-check, orphan-scan, ...) plugs in by adding one row here
/// plus its render title in `section_title`. Order here is the render
/// order; the flat findings list is sorted independently for deterministic
/// output and exit codes.
const SECTIONS: &[(&str, SectionFn)] = &[
    ("inventory", section_inventory),
    ("version", section_version),
    ("config", section_config),
    ("auth", section_auth),
    ("pools", section_seat_pools),
    ("seats", section_seat_orphans),
    ("secrets", section_secret_orphans),
    ("probe", section_probe),
    ("capability", section_capability),
    ("freshness", section_freshness),
    ("pricing", section_pricing),
    ("knobs", section_knobs),
];

/// The no-network subset of [`SECTIONS`]: [`SECTIONS`] MINUS the `probe`
/// entry. Every producer here is pure over [`DoctorContext`] and dials
/// nothing -- inventory, version, config, auth (`probe_local`, no refresh),
/// secrets, and capability. A report built from these needs no
/// `gather_probe_results` pass, so a status surface can produce a full
/// doctor report offline and derive reachability from an already-observed
/// circuit phase rather than an upstream dial.
// Consumed by the offline status surface, not the CLI `doctor` command; the
// CLI path stays on the full `SECTIONS` list.
const NO_NETWORK_SECTIONS: &[(&str, SectionFn)] = &[
    ("inventory", section_inventory),
    ("version", section_version),
    ("config", section_config),
    ("auth", section_auth),
    ("pools", section_seat_pools),
    ("seats", section_seat_orphans),
    ("secrets", section_secret_orphans),
    ("capability", section_capability),
    ("freshness", section_freshness),
    ("pricing", section_pricing),
    ("knobs", section_knobs),
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
    /// redacted at gather time (see
    /// [`crate::commands::parse_error_redaction::redact_config_load_error`])
    /// -- a toml/serde parse error can inline a `literal:` credential, so the
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
    /// Seat keys (`<provider>` / `<provider>#<label>`) of stored OAuth
    /// credentials no provider entry's `oauth://` ref reaches. A read-only
    /// stored-seats-vs-refs diff; no credential is refreshed or removed. Only
    /// the provider id and the operator's own seat label -- never token
    /// material or a storage path.
    orphan_seats: Vec<String>,
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
    /// The learned-capability matrix source: a read-only one-shot ledger
    /// replay for this run's revision, availability classified as a
    /// first-class tri-state. Populated in the single gather pass and
    /// consumed by the capability matrix panel builder.
    capability_matrix: CapabilityMatrixSource,
    /// The freshness section's read-only inputs: baked catalog stamp, the
    /// freshest overlay verification, and the last SUCCESSFUL import. Purely
    /// additive to the context; gathered once like every other input.
    freshness: FreshnessInputs,
    /// The pricing section's rows: one per configured model, resolved once at
    /// gather time so the section producer stays a pure mapping. `None` when
    /// the catalog overlay could not be loaded -- the overlay is the rate
    /// CORRECTION channel, so without it no resolved figure can be trusted and
    /// the section renders an honest unavailable line instead.
    pricing: Option<Vec<PricingRow>>,
    /// The knobs section's rows: one per configured model, resolved once at
    /// gather time so the section producer stays a pure mapping. `None` when
    /// the catalog overlay could not be loaded -- the overlay both corrects and
    /// disables catalog ceilings, so without it no source attribution can be
    /// trusted and the section renders an honest unavailable line instead.
    knobs: Option<Vec<KnobRow>>,
}

/// Where one configured model's outbound `max_output_tokens` ceiling comes
/// from, and what it resolves to.
///
/// Resolved through the SAME `EffectiveRow::output_ceiling_tokens` accessor the
/// factory's fill reads, so the diagnostic can never name a ceiling the served
/// router would not apply.
struct KnobRow {
    /// The `[models.<nickname>]` key.
    nickname: String,
    /// The referenced provider's kind token (empty when the provider is
    /// unknown to the config).
    provider_kind: String,
    /// The upstream model id the ceiling was resolved for.
    upstream: String,
    /// The resolved ceiling and the layer that supplied it.
    source: OutputCeilingSource,
}

/// The three states a [`KnobRow`] can report, in the precedence order the
/// dispatch path resolves them: config wins, else the catalog fill, else the
/// Anthropic-shape egress's own hardcoded baseline.
enum OutputCeilingSource {
    /// The operator wrote `[models.X] max_output_tokens`; the catalog never
    /// touches it.
    Config(u32),
    /// The config left the ceiling unset and the model's resolved catalog cell
    /// confirmed one, so the factory filled it.
    Catalog(u32),
    /// Neither layer supplies a ceiling: the Anthropic-shape egresses fall
    /// through to their hardcoded baseline and every other egress forwards
    /// caller omission untouched.
    Default,
}

/// Where one configured model's cost rates come from, and what they are.
///
/// Resolved through the SAME `routectl_router::effective_pricing` boundary the
/// usage report prices rows against, so the diagnostic can never claim a
/// source the bill does not use.
struct PricingRow {
    /// The `[models.<nickname>]` key.
    nickname: String,
    /// The `provider` the model references.
    provider: String,
    /// The referenced provider's kind token (empty when the provider is
    /// unknown to the config).
    provider_kind: String,
    /// The upstream model id the rates were resolved for.
    upstream: String,
    /// The resolved rates and the layer that supplied them.
    source: PricingRowSource,
}

/// The four states a [`PricingRow`] can report, in the same vocabulary the
/// usage report's cost column uses.
enum PricingRowSource {
    /// A managed-OAuth (`oauth://`) provider: billed by subscription, so no
    /// per-token rate applies and none is looked up.
    Subscription,
    /// An operator `[registry]` pricing row, taken verbatim.
    Registry {
        input_per_mtok: Option<f64>,
        output_per_mtok: Option<f64>,
    },
    /// The two-layer catalog's own baked rates, converted to per-million.
    Catalog {
        input_per_mtok: Option<f64>,
        output_per_mtok: Option<f64>,
    },
    /// Neither layer prices this model: its usage reports as unpriced rather
    /// than as a fabricated zero.
    Unpriced,
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
    /// redactor (see
    /// [`crate::commands::parse_error_redaction::redact_config_load_error`]).
    panel_unavailable: Option<String>,
}

/// The config-derived capability view: everything the section and the
/// matrix panel draw from the parsed config.
struct CapabilityConfig {
    /// Present legacy capability-list key NAMES (never values) driving the
    /// migrate nudge.
    legacy_keys: Vec<&'static str>,
    /// The catalog/overlay capability prior cells, one per configured model
    /// whose resolved catalog row carries capability data. Empty when the
    /// overlay could not be read -- priors are then absent, while the
    /// learned and override matrix cells still render.
    priors: Vec<PriorCell>,
}

/// One catalog/overlay capability prior cell: a `[models.X]` entry whose
/// resolved catalog row carries capability data, with its verification
/// stamp so the matrix panel can flag a prior stale past the operator
/// staleness hint.
struct PriorCell {
    nickname: String,
    verified_at: String,
    capabilities: Vec<(String, bool)>,
}

/// The read-only ledger-replay source the capability matrix panel renders
/// from, with availability as a first-class tri-state. The matrix is
/// honest-`Empty` ONLY when the ledger was readable, its tombstone matched
/// this run's revision, and the post-boundary slice held zero rows. Every
/// other outcome -- unreadable ledger, version-too-new, absent or foreign
/// tombstone, a config that would not parse -- is `Unavailable` with a
/// path-free class token, NEVER a silent empty: boot's fail-closed-to-empty
/// is correct for serving but would mislead a diagnostic into reporting
/// "nothing learned" when the truth is "could not read".
enum CapabilityMatrixSource {
    /// The ledger replayed at least one learned entry. `now` / `now_ms` are
    /// the single pinned clock anchors the mapped instants were taken
    /// against, so every derived cell age shares one skew-free basis.
    Available {
        entries: Vec<LearnedRegistryEntry>,
        now: Instant,
        /// The pinned wall-clock anchor the ledger replay read once. The
        /// matrix panel derives cell ages from the monotonic `now`
        /// (skew-free relative deltas), so this absolute anchor is reserved
        /// for a future consumer that needs epoch-ms timestamps.
        #[cfg_attr(not(test), allow(dead_code))]
        now_ms: i64,
    },
    /// Readable ledger, matched tombstone, zero post-boundary rows: an honest,
    /// non-degraded empty.
    Empty,
    /// The source could not be read at this run's revision; the token is a
    /// path-free class (`config_unavailable` / `no_data` / `no_tombstone` /
    /// `revision_mismatch` / `tombstone_read` / an open-error class such as
    /// `version_too_new`).
    Unavailable(&'static str),
}

/// The freshness section's read-only inputs. All ages are computed against
/// `today_epoch_day` (pinned once at gather time) so the three rows share a
/// single clock, and every age is epoch-day-clamped at zero.
struct FreshnessInputs {
    /// The compiled-in baked catalog version.
    catalog_version: u32,
    /// The whole baked table's snapshot date (`YYYY-MM-DD`).
    snapshot_date: &'static str,
    /// The freshest `verified_at` (`YYYY-MM-DD`) among the effective view's
    /// OVERLAY-sourced cells (import / user, never baked). `None` when no
    /// configured model resolves to an overlay-verified cell -- the operator
    /// is running on baked defaults, rendered as an honest "no overlay
    /// verified stamp".
    overlay_verified_at: Option<String>,
    /// The operator's display-only staleness horizon in days
    /// (`[capability].staleness_hint_days`), the threshold both age rows warn
    /// past.
    staleness_hint_days: u64,
    /// Today's epoch-day, pinned once so all rows age against one clock.
    today_epoch_day: i64,
    /// The last SUCCESSFUL import's persisted state, or `None` when no import
    /// has been recorded (missing / unreadable sidecar).
    last_import: Option<CatalogImportState>,
    /// Reserved for the future durable import RESULT (timestamp, version,
    /// outcome token, error detail) the import channel will persist. No such
    /// state exists today, so this is always `None` and the section renders
    /// nothing for it; the row lands here when the persistence does.
    #[allow(dead_code)]
    import_result: Option<ImportResult>,
}

/// Reserved shape for the future durable import RESULT. Distinct from
/// [`CatalogImportState`], which records ONLY successful imports: this will
/// carry the outcome of the LAST attempt (success or failure) once the
/// import channel persists it. Never constructed today -- reserved so the
/// freshness section grows the row without a context reshuffle.
#[allow(dead_code)]
struct ImportResult {
    /// When the attempt ran.
    at_unix: u64,
    /// The catalog version the attempt targeted.
    catalog_version: u32,
    /// The forever-token outcome vocabulary
    /// (`ok`/`signature_invalid`/`schema_mismatch`/`io_error`).
    outcome: &'static str,
    /// Operator-facing error detail on a failed attempt.
    detail: Option<String>,
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
            capability_matrix: Some(matrix::build_capability_matrix_panel(ctx)),
        },
    }
}

const fn status_ord(status: Status) -> u8 {
    match status {
        Status::Fail => 0,
        Status::Warn => 1,
        Status::Pass => 2,
    }
}
