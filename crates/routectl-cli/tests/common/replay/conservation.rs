//! Wire conservation: a fixture's captured `ingress_request.json`
//! against its captured `outgoing_request.json`.
//!
//! Both files come from the SAME real request, so this harness re-runs no
//! routectl code, boots no daemon, and rebuilds no enrichment. It is a
//! pure data comparison, adjudicated by the lane class and the exception
//! table in [`super::lane`]: every divergence between the two bodies is
//! either an explained routectl transform or wire loss.
//!
//! # Orientation, and why it is load-bearing
//!
//! ```text
//! diff_all(actual = the OUTGOING body, expected = the NORMALIZED INGRESS body, ..)
//! ```
//!
//! That is the one orientation [`super::lane`]'s predicates are written
//! against. Swapping the two arguments inverts Added and Removed and
//! silently un-matches every matcher, which reads as "nothing is
//! explained" rather than as an error.
//!
//! # Normalize, THEN diff
//!
//! Array pairing in `diff_all` is positional. The dominant explained
//! transform on the anthropic lane removes `role:"system"` turns from the
//! MIDDLE of `messages[]`, so every later element shifts index: diffed
//! raw, that ONE transform reports a divergence at nearly every surviving
//! index, and the only exception broad enough to absorb it would cover
//! essentially the whole message array. So the lane's NORMALIZER entries
//! rewrite the ingress side first
//! ([`normalize_ingress_for_lane`]), and only the realigned pair is
//! diffed. Matcher entries stay post-hoc predicates over the returned
//! divergence set.
//!
//! # Verdicts
//!
//! - FIDELITY lane: any divergence no exception explains is a FAILURE.
//! - TRANSLATION lane: report-only against a committed baseline of
//!   divergence PATHS. The signal is CHANGE, not emptiness -- a path
//!   ABSENT from the baseline fails, a path present in it is counted and
//!   reported. Writing a real translation whitelist here would be
//!   authoring the byte-fidelity milestone's spec as a table.
//! - An exception matching ZERO divergences on a POPULATED
//!   NON-GATEABLE lane is a FAILURE. An unexercised matcher is an
//!   untested claim, and a too-broad matcher is how a whitelist becomes
//!   a mute button. Scoped to the non-gateable (live-box) corpus because
//!   that is the corpus the exception table was measured against; see
//!   `unexercised_exception_failures` for why a gateable slice is no
//!   evidence about it.
//! - A gated lane with zero asserted fixtures, or ANY skip on a gated
//!   lane, is a FAILURE.
//! - DEGRADED (loud, but not a failure): nothing was asserted, or the
//!   corpus carried entries that would not load.
//!
//! # Reading the gated-lane list without weakening it
//!
//! [`resolve_gated_lanes_at`] maps exactly one error variant --
//! [`GatedLaneError::NoLanesListed`] -- onto [`GatedLanes::None`], and
//! propagates every other. That is NOT fail-open, because the two states
//! it distinguishes are different facts: `NoLanesListed` means the list
//! WAS read and parsed and deliberately names nothing yet, while an Io or
//! a malformed-line error means the list could not be read at all and its
//! contents are unknown. An empty gated set stays UNREPRESENTABLE from a
//! parse failure, which is what `unwrap_or_default` would have made it.
//!
//! # The translation baseline, and why an empty one is safe
//!
//! The baseline file is plain text, one `<ingress> <egress> <path>` triple
//! per line, `#` comments and blank lines ignored -- the same shape as the
//! gated-lane list, for the same reason (the whole content is a set of
//! tokens). It is keyed per LANE, not per path alone, so a path excused on
//! one translation lane is not excused on another.
//!
//! An EMPTY baseline is legal here, in deliberate contrast to the
//! gated-lane list's fail-closed emptiness, because the two files' empty
//! states point in opposite directions: an empty gated set would make
//! every gated comparison silently report-only, whereas an empty baseline
//! makes every translation divergence a failure. Emptiness is the strict
//! end of this file's range, so it needs no refusal. No translation-lane
//! fixture exists yet, so the committed file is comment-only.
//!
//! # Output is BOUNDED, deliberately
//!
//! Captured bodies are real prompt traffic. Failure text reports the
//! fixture name, the divergence PATH, and the divergence KIND -- never a
//! value, never a subtree. A raw [`Divergence`] is never printed: its
//! `Display` renders both sides in full, so one membership divergence on
//! `messages` would dump a whole prompt. Value shape, where it helps, comes
//! from [`bounded_body_diff`], which caps both the number of divergences
//! shown and what each may echo.
//!
//! # Stated limit of what this harness proves
//!
//! `resign_cch_in_place`
//! (`crates/routectl-providers/src/claude_signing.rs`) rewrites five
//! lowercase hex characters of one `cch=` token inside the `system`
//! billing block AFTER the outgoing-body trace is emitted. It is present
//! in 133 of the 250 live-box outgoing bodies, is length-preserving, and
//! is a silent no-op when the token is absent. So the captured outgoing
//! body differs from the true transmitted bytes by exactly those five
//! characters, and this harness -- which compares captured ingress to
//! captured outgoing -- cannot see them. Conservation here therefore
//! proves structural conservation up to that token; a byte-identical
//! deletion gate inherits the same limit.
//!
//! The transform gets NO exception entry: it produces no
//! ingress-vs-outgoing divergence at all, so an entry for it would match
//! zero divergences and the zero-match rule would correctly fail it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::gated_lanes::{GatedLaneError, read_gated_lanes_at};
use super::harness::bounded_body_diff;
use super::json_diff::{Divergence, DivergenceKind, diff_all};
use super::lane::{
    INGRESS_IDS, LaneClass, LaneKey, all_exceptions, class_for_dialects,
    egress_lane_from_fixture_kind, exceptions_for_lane, ingress_dialect,
    normalize_ingress_for_lane, unexplained,
};
use super::loader::Fixture;

/// Paths excluded from the conservation comparison: NONE.
///
/// The replay drivers ignore `stream`, `anthropic_beta`, and
/// `stream_options` because they compare a bare `normalize_request` result
/// against a body the egress kept editing afterwards. Conservation compares
/// two CAPTURED bodies, so every such post-normalize edit is present on the
/// outgoing side exactly as it went out and is part of what conservation
/// adjudicates. Measured: the anthropic lane reduces to zero unexplained
/// divergences with no ignore list at all, so an entry here would only be
/// able to hide something.
const CONSERVATION_IGNORE_PATHS: &[&str] = &[];

/// Label used for a lane whose ingress dialect the capture did not record.
/// Not a dialect token: it can never collide with one from [`INGRESS_IDS`].
pub const UNPINNED_INGRESS_LABEL: &str = "<unpinned>";

/// Most divergence paths named in one fixture's failure text. The paths
/// identify the class; the tail repeats it.
const MAX_PATHS_REPORTED: usize = 5;

/// Serializes adjudication against every OTHER reader of the counters.
///
/// The per-exception match counters in [`super::lane`] are process-global
/// statics that only ever increase, so a zero-match gate has to read the
/// DELTA across its own walk. That delta is only attributable while no
/// other walk is running: two concurrent adjudications would each see the
/// other's hits and a genuinely unexercised entry could be credited with a
/// nonzero delta, passing a gate that should have failed.
///
/// This is deliberately the SAME lock the unit tests that touch
/// `Exception::matches` directly take. A private lock here would serialize
/// walks against walks while still racing those tests, which showed up as a
/// delta of 3 where 1 was asserted -- green alone, red in a full-suite run.
use super::lane::COUNTER_DELTA_LOCK as ADJUDICATION_LOCK;

// ---------------------------------------------------------------------
// Gated lanes
// ---------------------------------------------------------------------

/// The gated-lane set, resolved. Two states, and neither is "the list
/// could not be read" -- that stays an `Err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatedLanes {
    /// The list was read and parsed and names no lane yet.
    None,
    /// The lane tokens the list names, in `kind_str()` vocabulary.
    Listed(Vec<String>),
}

impl GatedLanes {
    /// Whether `egress_token` is gated.
    pub fn is_gated(&self, egress_token: &str) -> bool {
        match self {
            Self::None => false,
            Self::Listed(lanes) => lanes.iter().any(|l| l == egress_token),
        }
    }

    /// The gated tokens, empty for [`GatedLanes::None`].
    pub fn tokens(&self) -> &[String] {
        match self {
            Self::None => &[],
            Self::Listed(lanes) => lanes,
        }
    }
}

/// Read the gated-lane list at `path`, mapping ONLY
/// [`GatedLaneError::NoLanesListed`] onto [`GatedLanes::None`].
///
/// See the module docs: the deliberately-empty list and an unreadable list
/// are different facts, and only the first one means "no lane is gated".
/// Every other variant propagates, so an empty gated set cannot be
/// manufactured out of a parse failure.
pub fn resolve_gated_lanes_at(path: &Path) -> Result<GatedLanes, GatedLaneError> {
    match read_gated_lanes_at(path) {
        Ok(lanes) => Ok(GatedLanes::Listed(lanes)),
        Err(GatedLaneError::NoLanesListed { .. }) => Ok(GatedLanes::None),
        Err(other) => Err(other),
    }
}

/// [`resolve_gated_lanes_at`] against the committed list.
pub fn resolve_gated_lanes() -> Result<GatedLanes, GatedLaneError> {
    resolve_gated_lanes_at(&super::gated_lanes::gated_lanes_path())
}

// ---------------------------------------------------------------------
// Translation baseline
// ---------------------------------------------------------------------

/// File name of the committed translation-lane divergence baseline, a
/// sibling of the two fixture roots and of the gated-lane list.
pub const TRANSLATION_BASELINE_FILE: &str = "translation_baseline.txt";

/// One baselined divergence path, keyed by the lane it was measured on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEntry {
    /// `IngressAdapter::id()` spelling.
    pub ingress: String,
    /// [`super::lane::EgressLane::token`] spelling.
    pub egress: String,
    /// The divergence path, in `diff_all`'s syntax.
    pub path: String,
}

/// Why the translation baseline could not be read.
#[derive(Debug)]
pub enum BaselineError {
    /// The file is absent or unreadable. Distinct from an EMPTY baseline,
    /// which is legal: a missing file means the baseline is unknown, and
    /// an unknown baseline cannot adjudicate anything.
    Io {
        /// The file that could not be read.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A line that is neither blank, a comment, nor an
    /// `<ingress> <egress> <path>` triple.
    Malformed {
        /// The file the line is in.
        path: String,
        /// 1-based line number.
        line_no: usize,
        /// The offending line, trimmed.
        line: String,
    },
}

impl std::fmt::Display for BaselineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "io error reading translation baseline {path}: {source}")
            }
            Self::Malformed {
                path,
                line_no,
                line,
            } => write!(
                f,
                "malformed baseline line {line_no} of {path}: `{line}`; \
                 expected `<ingress> <egress> <divergence-path>`"
            ),
        }
    }
}

impl std::error::Error for BaselineError {}

/// Path to the committed translation baseline.
pub fn translation_baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(TRANSLATION_BASELINE_FILE)
}

/// Read and parse the baseline at `path`. An empty result is legal -- see
/// the module docs for why this file's empty state is its STRICT end.
pub fn read_translation_baseline_at(path: &Path) -> Result<Vec<BaselineEntry>, BaselineError> {
    let text = std::fs::read_to_string(path).map_err(|source| BaselineError::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse_translation_baseline(&text, &path.display().to_string())
}

/// [`read_translation_baseline_at`] against the committed file.
pub fn read_translation_baseline() -> Result<Vec<BaselineEntry>, BaselineError> {
    read_translation_baseline_at(&translation_baseline_path())
}

/// Parse the baseline body: one `<ingress> <egress> <path>` triple per
/// line, `#` comments and blank lines ignored.
pub fn parse_translation_baseline(
    text: &str,
    path: &str,
) -> Result<Vec<BaselineEntry>, BaselineError> {
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [ingress, egress, divergence_path] = fields[..] else {
            return Err(BaselineError::Malformed {
                path: path.to_string(),
                line_no: idx + 1,
                line: line.to_string(),
            });
        };
        out.push(BaselineEntry {
            ingress: ingress.to_string(),
            egress: egress.to_string(),
            path: divergence_path.to_string(),
        });
    }
    Ok(out)
}

/// Whether the baseline excuses `path` on this lane.
fn baseline_covers(baseline: &[BaselineEntry], ingress: &str, egress: &str, path: &str) -> bool {
    baseline
        .iter()
        .any(|e| e.ingress == ingress && e.egress == egress && e.path == path)
}

// ---------------------------------------------------------------------
// Run inputs and outputs
// ---------------------------------------------------------------------

/// One corpus root's contribution to a run.
pub struct CorpusSlice<'a> {
    /// Short label for failure text (the root's role, not its path).
    pub label: &'a str,
    /// Fixtures that loaded.
    pub fixtures: &'a [Fixture],
    /// Entries under this root that would not load.
    pub unloadable: usize,
    /// Whether a lane named in the gated list may be GATED on these
    /// fixtures. False for the live-box root, whose bodies are real
    /// prompts and are report-only whatever the list says.
    pub gateable: bool,
}

/// Three-valued run verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every asserted fixture conserved, every exception exercised.
    Pass,
    /// At least one unexplained divergence, unexercised exception,
    /// unresolvable lane, or gated-lane coverage hole.
    Fail,
    /// Nothing failed, but the run proved less than it should have.
    Degraded,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Degraded => "DEGRADED",
        })
    }
}

/// Per-lane counts. One of these renders one terminal line.
#[derive(Debug, Clone, Default)]
pub struct LaneSummary {
    /// Raw `meta.ingress_kind`, or [`UNPINNED_INGRESS_LABEL`].
    pub ingress: String,
    /// The egress lane token.
    pub egress: String,
    /// `None` when the ingress dialect is unpinned, so no class exists.
    pub class: Option<LaneClass>,
    /// Whether this lane is gated for the fixtures counted here.
    pub gated: bool,
    /// Whether the fixtures counted here came from a GATEABLE root, i.e.
    /// the synthetic driver corpus rather than the live-box captures.
    /// Distinct from `gated`, which additionally requires the lane to be
    /// named in the gated list: a driver-corpus lane nobody gates yet is
    /// still gateable, and it is gateability -- not gating -- that says
    /// whether these fixtures are evidence about the exception table.
    pub gateable: bool,
    /// Fixtures attributed to this lane, asserted or not.
    pub fixtures: usize,
    /// Fixtures actually compared.
    pub asserted: usize,
    /// Fixtures skipped without a comparison.
    pub skipped: usize,
    /// Fixtures whose ingress body a normalizer rewrote.
    pub normalized: usize,
    /// Divergences an exception explained.
    pub explained: usize,
    /// Divergences no exception explained.
    pub unexplained: usize,
    /// Unexplained divergences the translation baseline excused.
    pub report_only: usize,
    /// Fixtures that failed.
    pub failed: usize,
}

impl LaneSummary {
    const fn class_label(&self) -> &'static str {
        match self.class {
            Some(LaneClass::Fidelity) => "FIDELITY",
            Some(LaneClass::Translation) => "TRANSLATION",
            None => "UNKNOWN",
        }
    }
}

/// One exception's hits during a single run, measured as a DELTA around
/// the walk rather than read off the global counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionHits {
    /// The entry's stable id.
    pub id: &'static str,
    /// The lane it is claimed for.
    pub egress: &'static str,
    /// Hits contributed by THIS run.
    pub hits: usize,
}

/// Everything one run produced.
#[derive(Debug, Clone, Default)]
pub struct ConservationRun {
    /// Per-lane counts, ordered by `(ingress, egress)`.
    pub lanes: Vec<LaneSummary>,
    /// Per-exception hits contributed by this run.
    pub exception_hits: Vec<ExceptionHits>,
    /// Bounded failure messages. Non-empty means [`Verdict::Fail`].
    pub failures: Vec<String>,
    /// Loud-but-passing notes. Non-empty with no failures means
    /// [`Verdict::Degraded`].
    pub degradations: Vec<String>,
    /// Corpus entries that would not load, across all slices.
    pub unloadable: usize,
}

impl ConservationRun {
    /// The run's verdict.
    pub const fn verdict(&self) -> Verdict {
        if !self.failures.is_empty() {
            Verdict::Fail
        } else if !self.degradations.is_empty() {
            Verdict::Degraded
        } else {
            Verdict::Pass
        }
    }

    /// Fixtures compared, across every lane.
    pub fn asserted(&self) -> usize {
        self.lanes.iter().map(|l| l.asserted).sum()
    }

    /// Terminal output: one line per lane, one per exception, then the
    /// verdict. Every line is bounded -- no body value appears here.
    pub fn report_lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .lanes
            .iter()
            .map(|lane| {
                format!(
                    "conservation: lane {}->{} class={} gated={} fixtures={} asserted={} \
                     explained={} unexplained={} report_only={} normalized={} skipped={} failed={}",
                    lane.ingress,
                    lane.egress,
                    lane.class_label(),
                    if lane.gated { "yes" } else { "no" },
                    lane.fixtures,
                    lane.asserted,
                    lane.explained,
                    lane.unexplained,
                    lane.report_only,
                    lane.normalized,
                    lane.skipped,
                    lane.failed,
                )
            })
            .collect();
        for hit in &self.exception_hits {
            out.push(format!(
                "conservation: exception {} (lane {}) matched {}",
                hit.id, hit.egress, hit.hits,
            ));
        }
        out.push(format!(
            "conservation: unloadable entries {}",
            self.unloadable
        ));
        for note in &self.degradations {
            out.push(format!("conservation: DEGRADED {note}"));
        }
        for failure in &self.failures {
            out.push(format!("conservation: FAILURE {failure}"));
        }
        out.push(format!("conservation: {}", self.verdict()));
        out
    }
}

// ---------------------------------------------------------------------
// Adjudication
// ---------------------------------------------------------------------

/// What one fixture's comparison produced.
enum FixtureOutcome {
    /// Compared. Carries the divergence accounting.
    Compared {
        explained: usize,
        unexplained: usize,
        report_only: usize,
        normalized: bool,
        failure: Option<String>,
    },
    /// Not compared, with a reason.
    Skipped(String),
}

/// The `'static` spelling of an ingress token, so a resolved token can key
/// a [`LaneKey`]. Fails closed on anything outside [`INGRESS_IDS`].
fn static_ingress_token(token: &str) -> Option<&'static str> {
    INGRESS_IDS.iter().copied().find(|known| *known == token)
}

/// Render the unexplained subset as KIND + PATH only, capped.
///
/// Never a value and never a `Divergence`'s own `Display`: paths and kinds
/// are the diagnostic and carry no payload, whereas a membership
/// divergence's value is the whole subtree on the side that has it.
fn bounded_paths(divergences: &[&Divergence]) -> String {
    let shown: Vec<String> = divergences
        .iter()
        .take(MAX_PATHS_REPORTED)
        .map(|d| {
            let kind = match d.kind {
                DivergenceKind::Added => "Added",
                DivergenceKind::Removed => "Removed",
                DivergenceKind::Changed => "Changed",
            };
            let path = if d.path.is_empty() { "<root>" } else { &d.path };
            format!("{kind} at {path}")
        })
        .collect();
    let mut out = format!("{} unexplained", divergences.len());
    if divergences.len() > shown.len() {
        out.push_str(&format!(" (first {} shown)", shown.len()));
    }
    out.push_str(": ");
    out.push_str(&shown.join("; "));
    out
}

/// Compare one fixture's two captured bodies.
fn compare_fixture(
    fixture: &Fixture,
    lane_key: &LaneKey,
    class: LaneClass,
    baseline: &[BaselineEntry],
) -> FixtureOutcome {
    let normalized_ingress = normalize_ingress_for_lane(lane_key, &fixture.ingress_request);
    let normalized = normalized_ingress != fixture.ingress_request;
    let divergences = diff_all(
        &fixture.outgoing_request,
        &normalized_ingress,
        CONSERVATION_IGNORE_PATHS,
    );
    let residual = unexplained(lane_key, &divergences);
    let explained = divergences.len() - residual.len();

    let (report_only, failing): (Vec<&Divergence>, Vec<&Divergence>) = match class {
        // A fidelity lane has no baseline: every residual divergence is
        // wire loss until an exception explains it.
        LaneClass::Fidelity => (Vec::new(), residual.clone()),
        LaneClass::Translation => residual
            .iter()
            .partition(|d| baseline_covers(baseline, lane_key.ingress, lane_key.egress, &d.path)),
    };

    let failure = (!failing.is_empty()).then(|| {
        let mut msg = format!(
            "fixture `{}` on lane {}->{} ({}): {}",
            fixture.name,
            lane_key.ingress,
            lane_key.egress,
            match class {
                LaneClass::Fidelity => "no exception explains these",
                LaneClass::Translation => "these paths are absent from the translation baseline",
            },
            bounded_paths(&failing),
        );
        // Value SHAPE, through the bounded reporter: it caps how many
        // divergences print and what each may echo.
        if let Some(summary) = bounded_body_diff(
            &fixture.outgoing_request,
            &normalized_ingress,
            CONSERVATION_IGNORE_PATHS,
        ) {
            msg.push_str(&format!(" | bounded shape: {summary}"));
        }
        msg
    });

    FixtureOutcome::Compared {
        explained,
        unexplained: residual.len(),
        report_only: report_only.len(),
        normalized,
        failure,
    }
}

/// Adjudicate every fixture in `slices`.
///
/// Holds [`ADJUDICATION_LOCK`] for the whole walk so the per-exception
/// counter deltas it reports are attributable to this run.
pub fn adjudicate(
    slices: &[CorpusSlice<'_>],
    gated: &GatedLanes,
    baseline: &[BaselineEntry],
) -> ConservationRun {
    let _guard = ADJUDICATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let before: Vec<usize> = all_exceptions().iter().map(|e| e.matched_count()).collect();

    let mut run = ConservationRun::default();
    // (ingress label, egress token, gateable) -> summary. `gateable` is
    // part of the key so a lane present under both roots does not report
    // the live-box fixtures as gated.
    let mut lanes: BTreeMap<(String, String, bool), LaneSummary> = BTreeMap::new();

    for slice in slices {
        run.unloadable += slice.unloadable;
        for fixture in slice.fixtures {
            let Ok(egress) = egress_lane_from_fixture_kind(&fixture.meta.provider_kind) else {
                run.failures.push(format!(
                    "fixture `{}` ({}): meta.provider_kind `{}` resolves to no egress lane",
                    fixture.name, slice.label, fixture.meta.provider_kind,
                ));
                continue;
            };
            let egress_token = egress.token();
            let gated_here = slice.gateable && gated.is_gated(egress_token);

            let ingress_raw = fixture.meta.ingress_kind.as_str();
            let ingress_label = if ingress_raw.is_empty() {
                UNPINNED_INGRESS_LABEL.to_string()
            } else {
                ingress_raw.to_string()
            };
            let entry = lanes
                .entry((
                    ingress_label.clone(),
                    egress_token.to_string(),
                    slice.gateable,
                ))
                .or_insert_with(|| LaneSummary {
                    ingress: ingress_label,
                    egress: egress_token.to_string(),
                    gated: gated_here,
                    gateable: slice.gateable,
                    ..LaneSummary::default()
                });
            entry.fixtures += 1;

            // The meta contract's EMPTY ingress_kind means the capture
            // could not observe the dialect. The loader's stance on an
            // unpinned field is to refuse the individual fixture, so this
            // is a skip; a NON-empty value outside the vocabulary is a
            // fixture-authoring bug or vocabulary drift and fails closed.
            if ingress_raw.is_empty() {
                entry.skipped += 1;
                continue;
            }
            let (Some(ingress_static), Ok(ingress_dialect_resolved)) = (
                static_ingress_token(ingress_raw),
                ingress_dialect(ingress_raw),
            ) else {
                run.failures.push(format!(
                    "fixture `{}` ({}): meta.ingress_kind `{ingress_raw}` resolves to no \
                     ingress dialect",
                    fixture.name, slice.label,
                ));
                continue;
            };
            let class = class_for_dialects(ingress_dialect_resolved, egress.dialect());
            entry.class = Some(class);
            let lane_key = LaneKey {
                ingress: ingress_static,
                egress: egress_token,
            };

            match compare_fixture(fixture, &lane_key, class, baseline) {
                FixtureOutcome::Skipped(reason) => {
                    entry.skipped += 1;
                    eprintln!(
                        "conservation: skipping fixture `{}`: {reason}",
                        fixture.name,
                    );
                }
                FixtureOutcome::Compared {
                    explained,
                    unexplained: residual,
                    report_only,
                    normalized,
                    failure,
                } => {
                    entry.asserted += 1;
                    entry.explained += explained;
                    entry.unexplained += residual;
                    entry.report_only += report_only;
                    entry.normalized += usize::from(normalized);
                    if let Some(msg) = failure {
                        entry.failed += 1;
                        run.failures.push(msg);
                    }
                }
            }
        }
    }

    run.exception_hits = all_exceptions()
        .iter()
        .zip(before)
        .map(|(entry, was)| ExceptionHits {
            id: entry.id,
            egress: entry.lane.egress,
            hits: entry.matched_count().saturating_sub(was),
        })
        .collect();
    run.lanes = lanes.into_values().collect();

    run.failures.extend(unexercised_exception_failures(
        &run.lanes,
        &run.exception_hits,
    ));
    run.failures
        .extend(gated_lane_failures(&run.lanes, gated, slices));
    run.degradations.extend(degradations(&run));
    run
}

/// Exceptions that matched nothing on a NON-GATEABLE lane that HAS
/// asserted fixtures.
///
/// Reads the per-run DELTAS, never the global counters: those are
/// process-global statics shared by every walk in the binary, so a global
/// read credits an entry with hits some other walk contributed and passes
/// a gate that should have failed.
///
/// GATEABLE lanes are excluded, and the exclusion is the rule's scope
/// rather than a softening of it. The exception table's hit counts were
/// measured across the live-box corpus -- hundreds of real requests, in
/// which every entry fires. The gateable (driver) corpus is synthetic and
/// grows one deliberately-chosen case at a time, and a case cannot
/// exercise every entry by construction: a plain base-url turn sends zero
/// system turns, leaves thinking off, and names a model whose alias
/// resolves without a suffix, so three of the four anthropic entries are
/// unreachable from it. Counting that as an untested claim would indict
/// the table over a slice that is no evidence about it, and the only way
/// to satisfy such a rule is to force the first driver case to trip all
/// four -- which makes the case a special-case of the transform list and
/// still breaks on the second case. The driver corpus's own coverage
/// question is a different question, and `gated_lane_failures` answers it.
fn unexercised_exception_failures(lanes: &[LaneSummary], hits: &[ExceptionHits]) -> Vec<String> {
    let mut out = Vec::new();
    for lane in lanes.iter().filter(|l| l.asserted > 0 && !l.gateable) {
        for hit in hits.iter().filter(|h| h.egress == lane.egress) {
            if hit.hits == 0 {
                out.push(format!(
                    "exception `{}` matched zero divergences on populated lane {}->{} \
                     ({} fixture(s) asserted); an unexercised entry is an untested claim",
                    hit.id, lane.ingress, lane.egress, lane.asserted,
                ));
            }
        }
    }
    out
}

/// A gated lane with no asserted fixture, or with any skip, is a failure.
fn gated_lane_failures(
    lanes: &[LaneSummary],
    gated: &GatedLanes,
    slices: &[CorpusSlice<'_>],
) -> Vec<String> {
    if !slices.iter().any(|s| s.gateable) {
        // Nothing gateable was walked at all, so the gated list says
        // nothing about this run. The zero-coverage question belongs to
        // the run that DOES walk the driver root.
        return Vec::new();
    }
    let mut out = Vec::new();
    for token in gated.tokens() {
        let gated_lanes: Vec<&LaneSummary> = lanes
            .iter()
            .filter(|l| l.gated && &l.egress == token)
            .collect();
        let asserted: usize = gated_lanes.iter().map(|l| l.asserted).sum();
        let skipped: usize = gated_lanes.iter().map(|l| l.skipped).sum();
        if asserted == 0 {
            out.push(format!(
                "gated lane `{token}` has zero asserted fixtures; a gate at zero coverage \
                 is decorative green",
            ));
        }
        if skipped > 0 {
            out.push(format!(
                "gated lane `{token}` skipped {skipped} fixture(s); a gated lane admits no skip",
            ));
        }
    }
    out
}

/// Loud-but-passing notes: the run proved less than it should have.
fn degradations(run: &ConservationRun) -> Vec<String> {
    let mut out = Vec::new();
    if run.unloadable > 0 {
        out.push(format!(
            "{} corpus entry/entries would not load, so they were never compared",
            run.unloadable,
        ));
    }
    if run.asserted() == 0 {
        out.push("no fixture was asserted; the run proves nothing".to_string());
    }
    for lane in run.lanes.iter().filter(|l| l.asserted == 0 && !l.gated) {
        out.push(format!(
            "non-gated lane {}->{} has {} fixture(s) and none asserted",
            lane.ingress, lane.egress, lane.fixtures,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::loader::{FIXTURE_SCHEMA_VERSION, FixtureClient, FixtureMeta};
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    // ---------- synthetic fixtures (never captured content) ----------

    /// A fixture pair built from SYNTHESIZED bodies. Realism comes from
    /// the SHAPE -- key names, turn structure, value types -- never from
    /// copied capture content: a captured body carries the operator's real
    /// prompts and belongs in no source file.
    fn fixture(
        name: &str,
        provider_kind: &str,
        ingress_kind: &str,
        ingress: Value,
        outgoing: Value,
    ) -> Fixture {
        Fixture {
            name: name.to_string(),
            ingress_request: ingress,
            ingress_request_headers: Vec::new(),
            outgoing_request: outgoing,
            outgoing_request_headers: Vec::new(),
            upstream_response_bytes: Vec::new(),
            upstream_response_headers: Vec::new(),
            egress_response_bytes: Vec::new(),
            egress_response_headers: Vec::new(),
            meta: FixtureMeta {
                schema_version: FIXTURE_SCHEMA_VERSION,
                provider_kind: provider_kind.to_string(),
                lane: String::new(),
                ingress_kind: ingress_kind.to_string(),
                case_id: name.to_string(),
                config_sha: String::new(),
                wire_pattern: String::new(),
                client: FixtureClient::default(),
                stream: false,
                model: None,
                routectl_version: None,
            },
        }
    }

    /// A body carrying all three matcher-visible transforms plus a lifted
    /// system turn, i.e. the shape the anthropic lane actually produces.
    /// Every exception on the lane is exercised by this one pair.
    fn all_four_transforms(name: &str) -> Fixture {
        let ingress = json!({
            "model": "claude-opus-4-8[1m]",
            "max_tokens": 1024,
            "thinking": {"type": "disabled"},
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "system", "content": "lifted out of the array"},
                {"role": "assistant", "content": "second turn"},
            ],
        });
        let outgoing = json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "temperature": 1.0,
            "messages": [
                {"role": "user", "content": "first turn"},
                {"role": "assistant", "content": "second turn"},
            ],
        });
        fixture(name, "anthropic", "anthropic", ingress, outgoing)
    }

    fn slice(fixtures: &[Fixture]) -> CorpusSlice<'_> {
        CorpusSlice {
            label: "synthetic",
            fixtures,
            unloadable: 0,
            gateable: false,
        }
    }

    fn run(fixtures: &[Fixture]) -> ConservationRun {
        adjudicate(&[slice(fixtures)], &GatedLanes::None, &[])
    }

    fn hits(run: &ConservationRun, id: &str) -> usize {
        run.exception_hits
            .iter()
            .find(|h| h.id == id)
            .unwrap_or_else(|| panic!("no exception `{id}`"))
            .hits
    }

    // ---------- fidelity lane, both directions ----------

    #[test]
    fn a_fidelity_lane_passes_when_every_divergence_is_explained_and_fails_when_one_is_not() {
        // POSITIVE CONTROL FIRST: the explained pair really does pass, so
        // the failure below is caused by the added divergence and not by
        // the fixture shape being unadjudicable.
        let clean = vec![all_four_transforms("explained")];

        let passing = run(&clean);

        assert_eq!(passing.verdict(), Verdict::Pass, "{:?}", passing.failures);
        assert_eq!(passing.lanes[0].unexplained, 0);
        assert_eq!(passing.lanes[0].asserted, 1);

        // Same fixture with ONE unexplainable change: the wire dropped
        // half the token budget, which no exception claims.
        let mut lossy = all_four_transforms("unexplained");
        lossy.outgoing_request["max_tokens"] = json!(512);
        let corpus = vec![all_four_transforms("explained"), lossy];

        let failing = run(&corpus);

        assert_eq!(failing.verdict(), Verdict::Fail);
        assert_eq!(failing.lanes[0].unexplained, 1);
        assert_eq!(failing.lanes[0].failed, 1);
        assert!(
            failing.failures.iter().any(|f| f.contains("max_tokens")),
            "the failure must name the divergence path: {:?}",
            failing.failures,
        );
    }

    /// The failure text names paths and kinds and echoes no prompt-bearing
    /// value. A membership divergence on `messages` renders the whole
    /// subtree through `Divergence`'s own `Display`, which is exactly what
    /// this harness must never print.
    #[test]
    fn failure_text_reports_paths_and_kinds_without_echoing_content() {
        let secret = "synthetic-prose-that-must-not-be-echoed";
        let mut lossy = all_four_transforms("content-bearing");
        lossy.ingress_request["messages"][0]["content"] = json!(secret);
        let corpus = vec![lossy];

        let failing = run(&corpus);

        assert_eq!(failing.verdict(), Verdict::Fail, "the pair must diverge");
        let text = failing.failures.join(" ");
        assert!(
            text.contains("messages[0].content"),
            "the path is the diagnostic and must survive: {text}",
        );
        assert!(
            !text.contains(secret),
            "value leaked into failure text: {text}"
        );
    }

    // ---------- normalize before diff ----------

    #[test]
    fn the_normalizer_runs_before_the_diff_so_a_middle_removal_reconciles() {
        let f = all_four_transforms("normalized");

        // POSITIVE CONTROL: diffed RAW, the middle removal shifts every
        // later element and the pair does not reconcile.
        let raw = diff_all(&f.outgoing_request, &f.ingress_request, &[]);
        let raw_messages = raw
            .iter()
            .filter(|d| d.path.starts_with("messages"))
            .count();
        assert!(
            raw_messages > 0,
            "raw diff must show the positional misalignment the normalizer removes",
        );

        let normalized = run(&[f]);

        assert_eq!(normalized.lanes[0].normalized, 1);
        assert_eq!(
            normalized.lanes[0].unexplained, 0,
            "normalizing the ingress side first must realign messages[]: {:?}",
            normalized.failures,
        );
        assert!(hits(&normalized, "system-turn-lift") > 0);
    }

    // ---------- the unexercised-exception control ----------

    #[test]
    fn an_exception_matching_nothing_on_a_populated_lane_fails_and_an_exercised_one_does_not() {
        let lane = LaneSummary {
            ingress: "anthropic".to_string(),
            egress: "anthropic-api".to_string(),
            asserted: 7,
            ..LaneSummary::default()
        };
        let exercised = vec![ExceptionHits {
            id: "exercised-entry",
            egress: "anthropic-api",
            hits: 3,
        }];

        // POSITIVE CONTROL: an entry with hits is not flagged, so the
        // failure below is about the zero and not about the lane.
        assert!(unexercised_exception_failures(std::slice::from_ref(&lane), &exercised).is_empty());

        let unmatchable = vec![ExceptionHits {
            id: "deliberately-unmatchable",
            egress: "anthropic-api",
            hits: 0,
        }];

        let failures = unexercised_exception_failures(&[lane], &unmatchable);

        assert_eq!(failures.len(), 1, "got: {failures:?}");
        assert!(failures[0].contains("deliberately-unmatchable"));
    }

    #[test]
    fn an_exception_matching_nothing_is_not_flagged_on_an_unpopulated_lane() {
        // Zero coverage is the DEGRADED signal, not the unexercised one:
        // an entry cannot be called untested by a walk that tested nothing.
        let empty_lane = LaneSummary {
            ingress: "anthropic".to_string(),
            egress: "anthropic-api".to_string(),
            asserted: 0,
            fixtures: 2,
            ..LaneSummary::default()
        };
        let unmatchable = vec![ExceptionHits {
            id: "deliberately-unmatchable",
            egress: "anthropic-api",
            hits: 0,
        }];

        assert!(unexercised_exception_failures(&[empty_lane], &unmatchable).is_empty());
    }

    /// The rule's SCOPE, both directions in one test.
    ///
    /// A gateable lane is the synthetic driver corpus: one deliberately
    /// chosen case, which cannot reach every entry in the table. A
    /// non-gateable lane is the corpus the table's hit counts were
    /// measured against, where a zero is a real regression. A rule that
    /// simply never fired would satisfy the first assertion alone, so the
    /// second one is what makes the first mean anything.
    #[test]
    fn a_gateable_lane_cannot_indict_an_exception_but_a_live_box_lane_still_does() {
        let unmatchable = vec![ExceptionHits {
            id: "deliberately-unmatchable",
            egress: "anthropic-api",
            hits: 0,
        }];
        let one_asserted = LaneSummary {
            ingress: "anthropic".to_string(),
            egress: "anthropic-api".to_string(),
            fixtures: 1,
            asserted: 1,
            ..LaneSummary::default()
        };

        let driver_lane = LaneSummary {
            gateable: true,
            ..one_asserted.clone()
        };

        assert!(
            unexercised_exception_failures(&[driver_lane], &unmatchable).is_empty(),
            "a one-case synthetic slice is no evidence about the exception table",
        );

        // PAIRED CONTROL: the same lane, the same single asserted
        // fixture, the same zero-hit entry -- non-gateable this time, and
        // it MUST still fail.
        let live_box_lane = LaneSummary {
            gateable: false,
            ..one_asserted
        };

        let failures = unexercised_exception_failures(&[live_box_lane], &unmatchable);

        assert_eq!(failures.len(), 1, "got: {failures:?}");
        assert!(failures[0].contains("deliberately-unmatchable"));
    }

    /// End to end: a populated anthropic lane whose fixture exercises only
    /// the temperature clamp leaves the other three entries at zero, and
    /// the run FAILS naming them.
    #[test]
    fn a_corpus_that_exercises_only_one_exception_fails_on_the_rest() {
        let partial = fixture(
            "temperature-only",
            "anthropic",
            "anthropic",
            json!({"model": "m", "messages": [{"role": "user", "content": "one"}]}),
            json!({
                "model": "m",
                "temperature": 1.0,
                "messages": [{"role": "user", "content": "one"}],
            }),
        );

        let partial_run = run(&[partial]);

        assert_eq!(partial_run.verdict(), Verdict::Fail);
        assert_eq!(hits(&partial_run, "thinking-temperature-clamp"), 1);
        for unexercised in [
            "system-turn-lift",
            "model-alias-suffix-resolved",
            "disabled-thinking-dropped",
        ] {
            assert!(
                partial_run
                    .failures
                    .iter()
                    .any(|f| f.contains(unexercised) && f.contains("zero divergences")),
                "`{unexercised}` matched nothing and must be flagged: {:?}",
                partial_run.failures,
            );
        }

        // POSITIVE CONTROL: the pair that exercises all four passes, so
        // the failure above is the zero-match rule firing and not this
        // corpus being unadjudicable.
        let full = run(&[all_four_transforms("all-four")]);
        assert_eq!(full.verdict(), Verdict::Pass, "{:?}", full.failures);
    }

    // ---------- counter scoping ----------

    #[test]
    fn two_sequential_walks_report_their_own_hits_rather_than_a_running_total() {
        let corpus = vec![all_four_transforms("first")];

        let first = run(&corpus);
        let second = run(&corpus);

        assert!(hits(&first, "thinking-temperature-clamp") > 0);
        assert_eq!(
            hits(&second, "thinking-temperature-clamp"),
            hits(&first, "thinking-temperature-clamp"),
            "a second identical walk must report the same DELTA, not a doubled global",
        );
        assert_eq!(
            hits(&second, "model-alias-suffix-resolved"),
            hits(&first, "model-alias-suffix-resolved"),
        );
    }

    // ---------- fail-closed lane resolution ----------

    #[test]
    fn an_unresolvable_lane_value_fails_naming_it() {
        // POSITIVE CONTROL: the real vocabularies resolve, so the two
        // rejections below are boundaries and not a broken resolver.
        assert_eq!(
            run(&[all_four_transforms("resolvable")]).verdict(),
            Verdict::Pass,
        );

        let bad_egress = fixture(
            "bad-egress",
            "anthropic-messages",
            "anthropic",
            json!({"model": "m"}),
            json!({"model": "m"}),
        );
        let egress_run = run(&[bad_egress]);

        assert_eq!(egress_run.verdict(), Verdict::Fail);
        assert!(
            egress_run
                .failures
                .iter()
                .any(|f| f.contains("anthropic-messages") && f.contains("provider_kind")),
            "got: {:?}",
            egress_run.failures,
        );

        let bad_ingress = fixture(
            "bad-ingress",
            "anthropic",
            "anthropic-api",
            json!({"model": "m"}),
            json!({"model": "m"}),
        );
        let ingress_run = run(&[bad_ingress]);

        assert_eq!(ingress_run.verdict(), Verdict::Fail);
        assert!(
            ingress_run
                .failures
                .iter()
                .any(|f| f.contains("anthropic-api") && f.contains("ingress_kind")),
            "got: {:?}",
            ingress_run.failures,
        );
    }

    #[test]
    fn an_unpinned_ingress_kind_is_skipped_rather_than_failed() {
        // The meta contract's empty value means the capture could not
        // observe the dialect -- distinct from a value outside the
        // vocabulary, which the test above proves still fails.
        let unpinned = fixture(
            "unpinned",
            "anthropic",
            "",
            json!({"model": "m"}),
            json!({"model": "m"}),
        );

        let unpinned_run = run(&[unpinned]);

        assert_eq!(unpinned_run.lanes.len(), 1);
        assert_eq!(unpinned_run.lanes[0].skipped, 1);
        assert_eq!(unpinned_run.lanes[0].asserted, 0);
        assert_eq!(unpinned_run.lanes[0].ingress, UNPINNED_INGRESS_LABEL);
        assert!(
            unpinned_run.failures.is_empty(),
            "{:?}",
            unpinned_run.failures
        );
        assert_eq!(
            unpinned_run.verdict(),
            Verdict::Degraded,
            "a lane that asserted nothing is degraded, never a pass",
        );
    }

    // ---------- gated lanes ----------

    fn gated_slice(fixtures: &[Fixture]) -> CorpusSlice<'_> {
        CorpusSlice {
            label: "driver",
            fixtures,
            unloadable: 0,
            gateable: true,
        }
    }

    #[test]
    fn a_gated_lane_with_zero_asserted_fixtures_fails_while_a_covered_one_passes() {
        let gated = GatedLanes::Listed(vec!["anthropic-api".to_string()]);
        let covered = vec![all_four_transforms("covered")];

        // POSITIVE CONTROL: with real coverage the same gated lane passes.
        let ok = adjudicate(&[gated_slice(&covered)], &gated, &[]);

        assert_eq!(ok.verdict(), Verdict::Pass, "{:?}", ok.failures);
        assert!(ok.lanes[0].gated, "the lane must be reported as gated");

        let empty: Vec<Fixture> = Vec::new();
        let uncovered = adjudicate(&[gated_slice(&empty)], &gated, &[]);

        assert_eq!(uncovered.verdict(), Verdict::Fail);
        assert!(
            uncovered
                .failures
                .iter()
                .any(|f| f.contains("anthropic-api") && f.contains("zero asserted")),
            "got: {:?}",
            uncovered.failures,
        );
    }

    #[test]
    fn any_skip_on_a_gated_lane_fails() {
        let gated = GatedLanes::Listed(vec!["anthropic-api".to_string()]);
        let corpus = vec![
            all_four_transforms("asserted"),
            fixture(
                "unpinned",
                "anthropic",
                "",
                json!({"model": "m"}),
                json!({"model": "m"}),
            ),
        ];

        let gated_run = adjudicate(&[gated_slice(&corpus)], &gated, &[]);

        assert_eq!(gated_run.verdict(), Verdict::Fail);
        assert!(
            gated_run
                .failures
                .iter()
                .any(|f| f.contains("anthropic-api") && f.contains("admits no skip")),
            "got: {:?}",
            gated_run.failures,
        );
    }

    /// The live-box root is report-only: naming its lane in the gated list
    /// must not turn real prompt captures into a commit gate.
    #[test]
    fn a_non_gateable_slice_is_never_gated_even_when_its_lane_is_listed() {
        let gated = GatedLanes::Listed(vec!["anthropic-api".to_string()]);
        let corpus = vec![all_four_transforms("live-box")];

        let report_only = adjudicate(&[slice(&corpus)], &gated, &[]);

        assert!(!report_only.lanes[0].gated);
        assert_eq!(
            report_only.verdict(),
            Verdict::Pass,
            "{:?}",
            report_only.failures
        );
    }

    // ---------- the gated-list reader ----------

    #[test]
    fn a_list_that_names_no_lane_reads_as_no_gated_lane_while_an_unreadable_one_fails() {
        let tmp = tempdir().unwrap();

        // POSITIVE CONTROL: a populated list resolves to its tokens, so
        // the None below is the deliberate-emptiness state and not a
        // reader that always answers None.
        let populated = tmp.path().join("populated.txt");
        fs::write(&populated, "anthropic-api\n").unwrap();
        assert_eq!(
            resolve_gated_lanes_at(&populated).unwrap(),
            GatedLanes::Listed(vec!["anthropic-api".to_string()]),
        );

        // Deliberately-empty: read, parsed, names nothing.
        let comment_only = tmp.path().join("comment_only.txt");
        fs::write(&comment_only, "# populated once the drivers exist\n").unwrap();
        assert_eq!(
            resolve_gated_lanes_at(&comment_only).unwrap(),
            GatedLanes::None,
        );

        // Absent: the list could not be read, so what it names is UNKNOWN
        // and an empty gated set must stay unrepresentable.
        let missing = tmp.path().join("absent.txt");
        assert!(
            matches!(
                resolve_gated_lanes_at(&missing),
                Err(GatedLaneError::Io { .. })
            ),
            "an unreadable list must not read as no-gated-lane",
        );

        // Malformed: same reasoning, different cause.
        let malformed = tmp.path().join("malformed.txt");
        fs::write(&malformed, "[lanes]\n").unwrap();
        assert!(matches!(
            resolve_gated_lanes_at(&malformed),
            Err(GatedLaneError::MalformedLaneId { .. })
        ));
    }

    #[test]
    fn the_committed_gated_list_currently_names_no_lane() {
        // Self-invalidating on purpose: the commit that populates the list
        // turns this red, which is the review moment that change deserves.
        assert_eq!(resolve_gated_lanes().unwrap(), GatedLanes::None);
    }

    // ---------- the translation baseline ----------

    /// A translation lane: anthropic ingress, gemini egress. No exception
    /// is registered for it, so every divergence is residual and the
    /// baseline alone decides.
    fn translation_fixture(name: &str, outgoing: Value) -> Fixture {
        fixture(
            name,
            "gemini",
            "anthropic",
            json!({"model": "m", "max_tokens": 16}),
            outgoing,
        )
    }

    #[test]
    fn a_baselined_translation_path_is_report_only_and_an_unbaselined_one_fails() {
        let corpus = vec![translation_fixture(
            "translated",
            json!({"model": "m", "generationConfig": {"maxOutputTokens": 16}}),
        )];
        let baseline = vec![
            BaselineEntry {
                ingress: "anthropic".to_string(),
                egress: "gemini".to_string(),
                path: "max_tokens".to_string(),
            },
            BaselineEntry {
                ingress: "anthropic".to_string(),
                egress: "gemini".to_string(),
                path: "generationConfig".to_string(),
            },
        ];

        // POSITIVE CONTROL: fully baselined, the lane is report-only.
        let baselined = adjudicate(&[slice(&corpus)], &GatedLanes::None, &baseline);

        assert_eq!(baselined.lanes[0].class, Some(LaneClass::Translation));
        assert_eq!(baselined.lanes[0].unexplained, 2);
        assert_eq!(baselined.lanes[0].report_only, 2);
        assert_eq!(
            baselined.verdict(),
            Verdict::Pass,
            "a baselined translation divergence is report-only: {:?}",
            baselined.failures,
        );

        // A path ABSENT from the baseline is the CHANGE signal.
        let drifted = adjudicate(&[slice(&corpus)], &GatedLanes::None, &baseline[..1]);

        assert_eq!(drifted.verdict(), Verdict::Fail);
        assert!(
            drifted
                .failures
                .iter()
                .any(|f| f.contains("generationConfig") && f.contains("baseline")),
            "got: {:?}",
            drifted.failures,
        );
    }

    #[test]
    fn a_baseline_entry_does_not_excuse_the_same_path_on_another_lane() {
        let corpus = vec![translation_fixture(
            "translated",
            json!({"model": "m", "generationConfig": {"maxOutputTokens": 16}}),
        )];
        let other_lane = vec![
            BaselineEntry {
                ingress: "openai".to_string(),
                egress: "gemini".to_string(),
                path: "max_tokens".to_string(),
            },
            BaselineEntry {
                ingress: "openai".to_string(),
                egress: "gemini".to_string(),
                path: "generationConfig".to_string(),
            },
        ];

        let failing = adjudicate(&[slice(&corpus)], &GatedLanes::None, &other_lane);

        assert_eq!(
            failing.verdict(),
            Verdict::Fail,
            "a transform measured on one lane proves nothing on another",
        );
    }

    #[test]
    fn the_baseline_parser_reads_lane_keyed_triples_and_rejects_anything_else() {
        let text = "# header\n\nanthropic gemini max_tokens\n  openai gemini generationConfig  \n";

        let entries = parse_translation_baseline(text, TRANSLATION_BASELINE_FILE).unwrap();

        assert_eq!(
            entries,
            vec![
                BaselineEntry {
                    ingress: "anthropic".to_string(),
                    egress: "gemini".to_string(),
                    path: "max_tokens".to_string(),
                },
                BaselineEntry {
                    ingress: "openai".to_string(),
                    egress: "gemini".to_string(),
                    path: "generationConfig".to_string(),
                },
            ],
        );

        for bad in ["max_tokens", "anthropic gemini", "a b c d"] {
            let err = parse_translation_baseline(bad, TRANSLATION_BASELINE_FILE)
                .expect_err("a non-triple line must be refused");
            assert!(matches!(err, BaselineError::Malformed { line_no: 1, .. }));
        }
    }

    /// An EMPTY baseline is legal and is this file's STRICT end -- every
    /// translation divergence then fails. The contrast with the
    /// gated-lane list, whose emptiness is fail-OPEN and therefore
    /// refused, is the whole reason the two readers differ.
    #[test]
    fn an_empty_baseline_is_legal_and_excuses_nothing() {
        let entries =
            parse_translation_baseline("# nothing yet\n", TRANSLATION_BASELINE_FILE).unwrap();
        assert!(entries.is_empty());

        let corpus = vec![translation_fixture(
            "translated",
            json!({"model": "m", "generationConfig": {"maxOutputTokens": 16}}),
        )];

        let failing = adjudicate(&[slice(&corpus)], &GatedLanes::None, &entries);

        assert_eq!(failing.verdict(), Verdict::Fail);
    }

    #[test]
    fn the_committed_baseline_reads_and_is_not_yet_populated() {
        let entries = read_translation_baseline().expect("the committed baseline must be readable");

        assert!(
            entries.is_empty(),
            "no translation-lane fixture exists yet; got {entries:?}",
        );
    }

    #[test]
    fn an_absent_baseline_file_is_an_error_rather_than_an_empty_baseline() {
        let tmp = tempdir().unwrap();

        let err = read_translation_baseline_at(&tmp.path().join(TRANSLATION_BASELINE_FILE))
            .expect_err("a missing baseline is unknown, not empty");

        assert!(matches!(err, BaselineError::Io { .. }));
    }

    // ---------- degraded ----------

    #[test]
    fn an_empty_non_gated_corpus_is_degraded_rather_than_passing() {
        let empty: Vec<Fixture> = Vec::new();

        let degraded = run(&empty);

        assert_eq!(degraded.verdict(), Verdict::Degraded);
        assert!(degraded.failures.is_empty());
        assert!(
            degraded
                .degradations
                .iter()
                .any(|d| d.contains("proves nothing")),
            "got: {:?}",
            degraded.degradations,
        );

        // POSITIVE CONTROL: one asserted fixture is enough to clear it.
        assert_eq!(run(&[all_four_transforms("one")]).verdict(), Verdict::Pass);
    }

    #[test]
    fn unloadable_corpus_entries_degrade_the_run() {
        let fixtures = vec![all_four_transforms("loaded")];

        let degraded = adjudicate(
            &[CorpusSlice {
                label: "synthetic",
                fixtures: &fixtures,
                unloadable: 3,
                gateable: false,
            }],
            &GatedLanes::None,
            &[],
        );

        assert_eq!(degraded.verdict(), Verdict::Degraded);
        assert_eq!(degraded.unloadable, 3);
        assert!(degraded.failures.is_empty(), "{:?}", degraded.failures);
    }

    // ---------- reporting ----------

    #[test]
    fn the_report_ends_with_a_three_valued_verdict_line_and_one_line_per_lane() {
        let lines = run(&[all_four_transforms("reported")]).report_lines();

        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("conservation: lane "))
                .count(),
            1,
        );
        let verdict_line = lines.last().expect("report is never empty");
        assert!(
            [
                "conservation: PASS",
                "conservation: FAIL",
                "conservation: DEGRADED"
            ]
            .contains(&verdict_line.as_str()),
            "got: {verdict_line}",
        );
    }
}
