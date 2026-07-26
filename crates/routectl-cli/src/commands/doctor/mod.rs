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
    Config, DoctorPanels, DoctorReport, Finding, LearnedRegistryEntry, OverrideRow, Source, Status,
    WouldTrimPanel, overall_exit,
};

use self::gather::{SecretCheck, gather_context};
use self::render::render_human;
use self::sections::{
    section_auth, section_capability, section_config, section_inventory, section_probe,
    section_secret_orphans, section_version,
};

pub(crate) use self::gather::gather_context_no_network;

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
const SCHEMA_VERSION: u32 = 3;

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
    /// The learned-capability matrix source: a read-only one-shot ledger
    /// replay for this run's revision, availability classified as a
    /// first-class tri-state. Populated in the single gather pass and
    /// consumed by the capability matrix panel renderer.
    #[cfg_attr(not(test), allow(dead_code))]
    capability_matrix: CapabilityMatrixSource,
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

/// The read-only ledger-replay source the capability matrix panel renders
/// from, with availability as a first-class tri-state. The matrix is
/// honest-`Empty` ONLY when the ledger was readable, its tombstone matched
/// this run's revision, and the post-boundary slice held zero rows. Every
/// other outcome -- unreadable ledger, version-too-new, absent or foreign
/// tombstone, a config that would not parse -- is `Unavailable` with a
/// path-free class token, NEVER a silent empty: boot's fail-closed-to-empty
/// is correct for serving but would mislead a diagnostic into reporting
/// "nothing learned" when the truth is "could not read".
#[cfg_attr(not(test), allow(dead_code))]
enum CapabilityMatrixSource {
    /// The ledger replayed at least one learned entry. `now` / `now_ms` are
    /// the single pinned clock anchors the mapped instants were taken
    /// against, so every derived cell age shares one skew-free basis.
    Available {
        entries: Vec<LearnedRegistryEntry>,
        now: Instant,
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
