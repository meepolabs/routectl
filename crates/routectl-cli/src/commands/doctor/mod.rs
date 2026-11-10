//! Doctor command entry + report assembly.
//!
//! `routectl doctor` -- a read-only health report. A doctor run mutates
//! NOTHING: config, credentials, and usage DB are all byte-identical
//! afterward. It loads config through the unvalidated,
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
mod served;

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "lane_matrix_tests.rs"]
mod lane_matrix_tests;

#[cfg(test)]
#[path = "seed_matrix_tests.rs"]
mod seed_matrix_tests;

#[cfg(test)]
#[path = "recheck_backstop_matrix_tests.rs"]
mod recheck_backstop_matrix_tests;

#[cfg(test)]
#[path = "precedence_matrix_tests.rs"]
mod precedence_matrix_tests;

#[cfg(test)]
#[path = "legacy_folder_tests.rs"]
mod legacy_folder_tests;

#[cfg(test)]
#[path = "value_matrix_tests.rs"]
mod value_matrix_tests;

#[cfg(test)]
#[path = "tuning_section_tests.rs"]
mod tuning_section_tests;

#[cfg(test)]
#[path = "price_fill_section_tests.rs"]
mod price_fill_section_tests;

#[cfg(test)]
#[path = "leftover_catalog_files_tests.rs"]
mod leftover_catalog_files_tests;

use std::path::Path;
use std::time::Instant;

use routectl_auth::LocalProbe;

use crate::config_folder::LegacyConfigDir;
use crate::server::reload_failure::ReloadFailureSnapshot;
use routectl_auth::oauth::types::TokenRecord;
use routectl_core::ProbeOutcome;
use routectl_router::prices::PriceSources;
use routectl_router::{
    BetaSeedScope, Config, DoctorPanels, DoctorReport, FillCeiling, Finding, LearnedRegistryEntry,
    MatrixReplaySummary, MatrixWarm, PriceTableDates, PricingConfig, SeedClearMarker, Status,
    TuningRow, WouldTrimPanel, overall_exit,
};

use self::gather::{SecretCheck, gather_context};
use self::render::render_human;
use self::sections::{
    section_auth, section_capability, section_config, section_inventory, section_knobs,
    section_pricing, section_probe, section_seat_orphans, section_seat_pools,
    section_secret_orphans, section_tuning, section_version,
};

pub(crate) use self::gather::gather_context_no_network;
pub(crate) use self::served::{
    GatherSources, ParkedSeat, ResidentLearned, SeatFix, ServedInputs, ServedPrices, UnusableSeat,
    unusable_seat_fix,
};

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
///
/// v10 -> v11: a `pricing` row for a SUBSCRIPTION model gains a second clause
/// naming its API-equivalence basis -- which layer supplies the rates the usage
/// report values that seat's traffic at, or which dimensions are unpriced and
/// therefore keep the equivalent absent. The billed-by-seat statement is
/// unchanged; the row's detail text is not.
///
/// v11 -> v12: the capability matrix panel's `Unavailable` code now names a
/// too-old ledger schema as `version_too_old` instead of folding it into the
/// same `expected` code a cold ledger renders -- the two states have
/// different remedies and were previously indistinguishable in the report.
///
/// v12 -> v13: the capability matrix panel's rows are learned lanes
/// (`provider_entry#upstream`) rather than model nicknames, each carrying
/// the nicknames that map to it and its provider kind; every cell gains its
/// winning layer, the routing action, and the resident learned entry's
/// first-seen / last-seen / expiry timestamps; the panel gains the replay
/// tally (`replay`); and a post-boundary slice read that fails renders
/// `Unavailable` (`open_failed` / `query_failed`) instead of an empty matrix.
///
/// v13 -> v14: the shipped beta seed renders as its own matrix layer. Its
/// flags appear as `beta:<flag>` columns on lanes of the kind it applies to,
/// a seed cell carries source and layer `seed` with the new `withhold`
/// action (or verdict `cleared` and action `allow` once a seed-clear marker
/// lifts it), and a learned negative on a `beta:` key acts as `withhold`
/// rather than `strip` / `route_away`.
///
/// v14 -> v15: the capability matrix panel names its learned layer's origin
/// in `source` (`ledger_replay` for a read-only replay run for the report,
/// `resident` for the daemon's in-memory registry) and gains `warm`, the
/// resident registry's boot warm outcome and tally. A `ledger_replay` panel
/// carries `replay` and no `warm`; a `resident` panel carries `warm` and no
/// `replay`. Two additive `config` findings land with it: `reload`, the
/// daemon's last rejected config / overlay reload (closed class and age), and
/// one `catalog overlay` finding per soft-defect overlay cell.
///
/// v15 -> v16: the capability matrix renders every observed capability key
/// as its own column, and the panel drops its overflow count field.
///
/// v16 -> v17: the served report's `auth` section gains one `parked seat
/// <state_key>` Warn per credential-parked dispatch seat, naming the reason
/// and the park deadline. The CLI report, which cannot see daemon state,
/// carries none.
///
/// v17 -> v18: additive. Every capability matrix cell, and each
/// `nickname_actions` entry, gains `overrode_layer`, the next tier below the
/// deciding one that had an opinion. The cells resolve through the same
/// precedence dispatch applies, so `layer` and `source` gain the values `pin`
/// (an operator wire floor pins the flag, verdict `forced_supported`) and
/// `representability` (a lane that cannot carry the flag, verdict
/// `unrepresentable`, action `withhold`). The served `/status/doctor` panel
/// gains `lossy_repairs_by_lane`: one `{lane, lossy}` row per learned lane a
/// lossy field repair landed on (at most 256), plus an `other` row for lanes
/// past that cap and targets with no learned lane.
///
/// v18 -> v19: additive, served panel only. The `/status/doctor` panel gains
/// `budget_exhaustions_by_lane`: one `{lane, exhausted}` row per learned lane
/// where a rejection's proposed repair found the request's repair allowance
/// spent (at most 256, plus `other`), and `repairs_per_request`: requests by
/// the number of repairs each drew, one bucket per count from zero to the
/// per-request allowance. The report itself is unchanged.
///
/// v19 -> v20: additive. The `config` section gains a `legacy folder` Warn
/// for each folder routectl used before every OS shared one config folder
/// (`%APPDATA%\routectl`, then `~/Library/Application Support/routectl`)
/// that still holds files (named, at most eight, plus a count of the rest) or
/// could not be inspected.
///
/// v20 -> v21: the `knobs` section loses its config state (the output ceiling
/// is no longer a `[models.X]` setting): every row is the catalog fill or the
/// egress baseline, and the catalog row's detail names the catalog command
/// that corrects it.
///
/// v21 -> v22: additive. Every capability matrix cell gains `value`, the
/// value the resident learned verdict applies (`shape:<v>` / `ceiling:<n>`),
/// `null` when no learned entry backs the cell or its verdict writes none.
///
/// v22 -> v23: additive, served panel only. The `/status/doctor` panel gains
/// `ledger_health` (the live `dropped_full`, `dropped_closed` and
/// `write_errors` request-row loss counters) and `observation`: today's
/// summed per-request observation counts, its ledger gap rows, and whether
/// its rows are complete (`null` when the usage ledger cannot be read). The
/// report itself is unchanged.
///
/// v23 -> v24: additive. The `tuning` section joins the report, one Pass
/// finding per `[providers.X]` entry (pool members included) and one per
/// `[models.X]` entry with its own `stream_first_byte_timeout_ms` (one per
/// seat for a pooled model), named `provider <id>` / `model <nickname>` /
/// `model <nickname>#<member>`. Its detail lists the resolved
/// `circuit_failures`, `circuit_cooldown_ms`, `rpm_limit`,
/// `request_timeout_ms` and `stream_first_byte_timeout_ms` as
/// `key=value(source)`, the same cells `config show --effective` prints.
///
/// v24 -> v25: additive, served report only. The `auth` section gains one
/// Warn `unusable seat <state_key>` per dispatch seat whose credential the
/// running daemon holds as unusable, naming the reason
/// (`credential_unusable`), the cause (`not_logged_in`, `unreadable`,
/// `expired` or `revoked`) and since when, with the fix as its remediation:
/// `routectl login <provider> --label <label>` for a labeled OAuth seat (a
/// label outside `[A-Za-z0-9_.-]` single-quoted), `routectl login <provider>`
/// for an unlabeled one, and a correction of the ref for an OAuth provider
/// `routectl login` does not accept. The CLI cannot see daemon
/// memory and reports none. The `pools` remediations name the local
/// `credential_unusable` refusal.
///
/// v25 -> v26: every `knobs` row reads its ceiling from the model's own
/// `[models.X] max_output_tokens`. A row names its source as `model-config`
/// (the configured value) or `default` (the egress baseline); the catalog
/// source, its `routectl catalog set` pointer, and the section-unavailable
/// line for an unreadable catalog overlay are gone.
///
/// v26 -> v27: catalog capability data no longer feeds the capability
/// matrix. No cell carries layer or source `prior` or verdict `assumed`; a
/// cell the catalog alone used to fill is `unknown`, and the `stale` flag
/// marks only a verified learned cell past the staleness hint. The panel's
/// state line names `seed/override cells` where it named
/// `prior/seed/override cells`.
///
/// v27 -> v28: every `pricing` row resolves through the price tables and
/// names a source token per price (`model-config`, `registry`, `refreshed`,
/// `shipped`, `derived` or `unknown`) plus the date of each table read; the
/// `baked catalog` source and the section-unavailable line for an unreadable
/// catalog overlay are gone. A subscription row keeps its API-equivalence
/// clause. A `knobs` row names `model-config`, `learned` or `default` and the
/// value that source resolves.
///
/// v28 -> v29: the `freshness` section leaves the report (no baked catalog,
/// overlay verified or last successful import rows), and so do the `config`
/// section's `catalog overlay` soft-defect findings: doctor no longer reads
/// the catalog overlay or the import record. The `config` section gains one
/// Pass `leftover catalog files` finding naming which of
/// `catalog_overlay.json`, `catalog_overlay.json.lock`, `catalog_state.json`
/// and `catalog_import_state.json` are still in the config folder, absent
/// when none is.
///
/// v29 -> v30: observation block gains the five walk fields. Additive, served
/// panel only: the `/status/doctor` `observation` block gains
/// `request_caused_failures`, `excluded_endpoint`, `excluded_model`,
/// `excluded_target` and `excluded_probe_run`. The `capability` section gains
/// one Warn `learning` finding when `[capability] enabled = false`, saying
/// nothing is learned or applied; it never Fails, so the exit code is unmoved.
pub(crate) const SCHEMA_VERSION: u32 = 30;

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
    ("pricing", section_pricing),
    ("knobs", section_knobs),
    ("tuning", section_tuning),
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
    ("pricing", section_pricing),
    ("knobs", section_knobs),
    ("tuning", section_tuning),
];

/// The read-only inputs every section producer draws from, gathered once
/// per run so producers stay pure and the run is a single filesystem pass.
pub(crate) struct DoctorContext {
    config: Config,
    raw_config: Option<String>,
    /// Set when the read-only typed load failed for a reason the raw-bytes
    /// version preflight does not catch (TOML syntax error, unknown field,
    /// legacy key). The version section surfaces it as a
    /// Fail so a present-but-broken config never reports all-Pass. Already
    /// redacted at gather time (see
    /// [`crate::commands::parse_error_redaction::redact_config_load_error`])
    /// -- a toml/serde parse error can inline a `literal:` credential, so the
    /// stored string is never the raw loader error.
    config_load_error: Option<String>,
    /// The schema version of a config a running daemon accepted. When set,
    /// the version section reports it instead of preflighting `raw_config`:
    /// the file on disk may hold a later edit the daemon rejected, and its
    /// version says nothing about the config being served.
    accepted_config_version: Option<u32>,
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
    /// The capability section's read-only input (see [`CapabilityInputs`]).
    capability: CapabilityInputs,
    /// The learned-capability matrix source: a read-only one-shot ledger
    /// replay for this run's revision, availability classified as a
    /// first-class tri-state. Populated in the single gather pass and
    /// consumed by the capability matrix panel builder.
    capability_matrix: CapabilityMatrixSource,
    /// Where `capability_matrix`'s learned layer came from, and for the
    /// daemon's resident registry, how it was warmed at boot.
    matrix_origin: MatrixOrigin,
    /// The shipped beta seed the matrix renders as its own layer: the same
    /// scope the router withholds by and the replay registry bounds its
    /// seed-clear markers with.
    beta_seed: BetaSeedScope,
    /// The pricing section's rows: one per configured model, resolved once at
    /// gather time against one pinned pair of price tables so the section
    /// producer stays a pure mapping.
    pricing: Vec<PricingRow>,
    /// The knobs section's rows: one per configured model, resolved once at
    /// gather time so the section producer stays a pure mapping. Read from
    /// the config and the learned layer the capability matrix shows.
    knobs: Vec<KnobRow>,
    /// The tuning section's rows: one per `[providers.X]` entry, then one per
    /// model with its own first-content timeout, each cell carrying the tier
    /// that supplied it. Read from the config alone.
    tuning: Vec<TuningRow>,
    /// The running daemon's last rejected config reload, as a closed
    /// class and an age. Only a served gather sets it; the CLI reads the files
    /// itself, so their errors render directly.
    reload_failure: Option<ReloadFailureSnapshot>,
    /// The running daemon's credential-parked seats. Only a served gather
    /// sets any; the CLI cannot see daemon state and reports no parks.
    parked_seats: Vec<ParkedSeat>,
    /// The running daemon's unusable seats. The mark lives only in daemon
    /// memory, so only a served gather sets any; the CLI reports none.
    unusable_seats: Vec<UnusableSeat>,
    /// The [`LEFTOVER_CATALOG_FILES`] present in the config folder, in that
    /// order. routectl no longer reads any of them.
    leftover_catalog_files: Vec<&'static str>,
    /// What each folder routectl used before the shared config folder still
    /// holds, one row per candidate that is not [`LegacyFolder::Absent`], in
    /// candidate order. routectl never reads them, so anything left there is
    /// residue the operator may still expect to be in use.
    legacy_folders: Vec<LegacyFolder>,
}

/// Files earlier releases kept in the config folder for the model catalog.
/// Nothing reads them now; doctor names the ones still present.
const LEFTOVER_CATALOG_FILES: [&str; 4] = [
    "catalog_overlay.json",
    "catalog_overlay.json.lock",
    "catalog_state.json",
    "catalog_import_state.json",
];

/// The most entry names a [`LegacyFolder::Entries`] carries; the rest are
/// counted in `more`.
const LEGACY_FOLDER_NAME_CAP: usize = 8;

/// The state of the old config folder.
#[derive(Debug, PartialEq, Eq)]
enum LegacyFolder {
    /// No old folder, an empty folder, or the folder holds the config this
    /// run reads (an explicit `--config` there): nothing to warn about.
    Absent,
    /// The folder holds entries routectl does not read.
    Entries {
        /// A home-free name for the folder, safe to print.
        display: &'static str,
        /// Sorted basenames, at most [`LEGACY_FOLDER_NAME_CAP`].
        names: Vec<String>,
        /// How many entries past the cap were left out of `names`.
        more: usize,
        /// Whether the folder holds a `config.toml`, listed or past the cap.
        has_config: bool,
    },
    /// The folder could not be listed. Only the error kind is kept: an
    /// `io::Error` message can carry the absolute home path.
    InspectError {
        /// A home-free name for the folder, safe to print.
        display: &'static str,
        /// Why the listing failed.
        kind: std::io::ErrorKind,
    },
}

/// One configured model's output fill: the `max_tokens` its egress sends when
/// the caller omits one, and where it comes from.
///
/// Read off the router's effective view (the configured or default fill the
/// factory stamps) lowered by the learned ceiling that would apply to the
/// model, so the diagnostic names the fill the pre-flight would clamp to. A
/// learned value shows only with capability learning enabled, on a lane whose
/// kind field repair admits, and with no `force_supported` override masking
/// `max_tokens`. The pre-flight's per-request gates (writer health, verdict
/// quorum) are not reflected, so a given request may still go unclamped.
struct KnobRow {
    /// The `[models.<nickname>]` key.
    nickname: String,
    /// The referenced provider's kind token (empty when the provider is
    /// unknown to the config).
    provider_kind: String,
    /// The upstream model id the fill was resolved for.
    upstream: String,
    /// The fill and its source; `None` when the egress forwards an omitted
    /// limit as omitted. A pool-backed model reads the egress of its first
    /// resolvable member for the whole pool, even when members differ in
    /// egress (Bedrock Invoke and Converse share one kind).
    fill: Option<FillCeiling>,
    /// For a `learned` fill, the value it lowers.
    lowers: Option<u32>,
    /// The model's configured `max_output_tokens`, when set and nonzero.
    configured: Option<u32>,
}

/// One configured model's effective prices, read off the router's effective
/// view, which resolves them through `routectl_router::prices::effective_pricing`
/// the way the usage report prices rows, so the diagnostic can never claim a
/// source the bill does not use.
struct PricingRow {
    /// The `[models.<nickname>]` key.
    nickname: String,
    /// The `provider` the model references.
    provider: String,
    /// The referenced provider's kind token (empty when the provider is
    /// unknown to the config).
    provider_kind: String,
    /// The upstream model id the prices were resolved for.
    upstream: String,
    /// Whether the provider bills by managed subscription rather than per
    /// token. Classified before any price decides anything.
    subscription: bool,
    /// The effective prices; a field left `None` is unknown.
    pricing: PricingConfig,
    /// The source of each field of [`Self::pricing`].
    sources: PriceSources,
    /// Whether a `[registry]` row matched, which wins whole even when it
    /// prices nothing.
    registry_matched: bool,
    /// The dates of the price tables the resolution read.
    tables: PriceTableDates,
}

/// The capability section's input: whether the config parsed. The section
/// renders a single redacted "panel unavailable" line when it did not,
/// rather than a misleading empty-from-default-config view.
struct CapabilityInputs {
    /// The already-redacted config load error, set only when the config could
    /// not be parsed. Redacted at gather time through the shared parse-error
    /// redactor (see
    /// [`crate::commands::parse_error_redaction::redact_config_load_error`]).
    panel_unavailable: Option<String>,
}

/// Where the capability matrix's learned layer came from.
enum MatrixOrigin {
    /// A read-only ledger replay run for this report.
    LedgerReplay,
    /// The daemon's resident registry, with its boot warm.
    Resident(MatrixWarm),
}

/// The learned source the capability matrix panel renders from, with
/// availability as a first-class tri-state.
///
/// For a ledger replay, the matrix is honest-`Empty` ONLY when the ledger was
/// readable, its tombstone matched this run's revision, its post-boundary
/// slice was actually read, and the replay left nothing resident. Every other
/// outcome -- unreadable ledger, version-too-new, absent or foreign tombstone,
/// a slice read that failed after the boundary classified, a config that would
/// not parse -- is `Unavailable` with a path-free class token, NEVER a silent
/// empty: boot's fail-closed-to-empty is correct for serving but would mislead
/// a diagnostic into reporting "nothing learned" when the truth is "could not
/// read". For the resident registry the same holds of the boot warm: an empty
/// registry whose warm could not read the ledger is `Unavailable` with the
/// warm's outcome token.
enum CapabilityMatrixSource {
    /// At least one learned entry is resident. `now` / `now_ms` are
    /// the single pinned clock anchors the mapped instants were taken
    /// against, so every derived cell age and timestamp shares one
    /// skew-free basis.
    Available {
        entries: Vec<LearnedRegistryEntry>,
        now: Instant,
        /// The wall-clock anchor paired with `now`: a cell's epoch-ms
        /// timestamps are `now_ms` offset by the instant's distance from
        /// `now`.
        now_ms: i64,
        /// The replay tally; `None` for the resident registry.
        replay: Option<MatrixReplaySummary>,
        /// Every resident seed-clear marker.
        seed_clears: Vec<SeedClearMarker>,
    },
    /// The source was read and nothing is resident: an honest, non-degraded
    /// empty. A replay's tally still says how many rows were read and why each
    /// was skipped; a seed-clear marker leaves no resident entry, so an empty
    /// source may still carry some.
    Empty {
        /// The replay tally; `None` for the resident registry.
        replay: Option<MatrixReplaySummary>,
        seed_clears: Vec<SeedClearMarker>,
    },
    /// The source could not be read at this run's revision; the token is a
    /// path-free class (`config_unavailable` / `no_data` / `no_tombstone` /
    /// `revision_mismatch` / `tombstone_read` / `open_failed` /
    /// `query_failed` / an open-error class such as `version_too_new`, or a
    /// failed boot warm's `unreadable` / `restate_failed`).
    Unavailable(&'static str),
}

/// Run the doctor aggregator against `config_path` and render the report.
/// Read-only and infallible in posture: a load or probe failure degrades to
/// a finding, never a hard error. Returns the process exit code.
pub async fn run(config_path: &Path, legacy_dirs: &[LegacyConfigDir], json: bool) -> i32 {
    let ctx = gather_context(config_path, legacy_dirs).await;
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
