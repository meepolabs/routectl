//! Pinned expectation over the client fingerprint every committed
//! `anthropic-api` driver fixture carries, and its reviewed relation to the
//! values routectl itself mints.
//!
//! WHY A PIN AND NOT A DRIFT GATE. routectl's minted fingerprint sits
//! behind the client the corpus captured, and it will keep sitting behind
//! it: the client releases on its own cadence and no commit can fix that.
//! A test that reddened on any difference would be red most weeks for a
//! reason nobody in that commit can act on, so it would be muted. What IS
//! actionable is a NEW observation nobody reviewed. So the observed sets
//! below are pinned EXACTLY: a capture carrying a value not listed here
//! reds in the commit that lands it, which is the one commit whose author
//! has the client in front of them. Rolling routectl's constants forward is
//! a separate decision, deliberately not this test's to force -- but it is
//! not a free one either: a bumped constant changes what a row's reviewed
//! [`Relation`] claims, so the same commit must re-take that verdict.
//!
//! NOTHING HERE READS THE LOCALLY INSTALLED CLIENT. The two inputs are the
//! committed corpus and routectl's own compiled constants, both of which
//! every checkout shares. A build that consulted whatever client happens to
//! be installed would pass or fail per machine.
//!
//! WHAT IS DERIVED VS REVIEWED. The observed sets are DERIVED from the
//! corpus on every run, through one map ([`derived_dimensions`]) that the
//! register's coverage is checked against -- so a dimension cannot be
//! observed without a verdict or given a verdict without being observed.
//! The RELATION each dimension holds is REVIEWED, and each is re-CHECKED
//! against the live minted values, so a verdict that stopped being true is
//! a red build rather than stale prose.
//!
//! TWO CEILINGS, STATED RATHER THAN CLOSED.
//!
//! 1. Every fixture in this corpus carries an `x-claude-code-session-id`,
//!    which is the single header the default cloak classification keys on.
//!    So the whole corpus exercises the GENUINE-CLIENT arm; the non-client
//!    arm -- the system relocation, the metadata user-id mint, the tool
//!    sort, the beta floor -- has NO fixture here. This test's coverage
//!    claim stops at the genuine-client arm, and the session-id assertion
//!    below is what keeps that ceiling honest: widening the corpus to a
//!    fixture without the header reds it.
//! 2. A caller using routectl as a library sends no HTTP headers at all, so
//!    it contributes no fingerprint observation by construction. Nothing
//!    here says anything about that population.
//!
//! Every parse fails LOUDLY, and each refusal has a synthetic control
//! proving it fires: a missing corpus directory, an empty corpus, a thinned
//! corpus, an unloadable case, a malformed headers file, an absent /
//! duplicated / whitespace-only / redacted required header, an unparseable
//! User-Agent, and an empty beta list. An empty observed set satisfies
//! every set comparison below by observing nothing, which is the exact
//! failure this module refuses.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use common::replay::harness::driver_root;
use common::replay::loader::{Fixture, INGRESS_HEADERS, discover_fixtures};

/// The lane directory under the driver corpus root whose fixtures carry a
/// Claude Code client fingerprint. Other lanes capture other clients and
/// have no fingerprint contract with this module.
const LANE: &str = "anthropic-api";

/// Placeholder the capture scrubber writes over a header value it must not
/// commit. A required dimension carrying it has no observation to pin, so
/// it is an error rather than a value.
const REDACTED: &str = "[REDACTED]";

/// Floor on the fixture count. A guard on the WALK, never the contract: a
/// corpus that read as empty would satisfy every per-fixture assertion by
/// asserting over nothing.
const MIN_FIXTURES: usize = 7;

// -- OBSERVED SETS, exact-pinned ------------------------------------------
//
// Derived from the committed corpus on 2026-09-12 and pinned as EXACTLY
// these values. Extend a list here in the same commit that lands a capture
// carrying a new value, having read what changed.

/// Stable Claude CLI versions the corpus self-reports.
const OBSERVED_CLI_VERSIONS: &[&str] = &["2.1.246"];

/// `User-Agent` surface tokens the corpus self-reports -- the trailing word
/// in the `(external, <surface>)` parenthetical. Its own dimension because
/// pinning the version says NOTHING about the rest of the User-Agent, and a
/// reader must not mistake a version pin for a full-UA pin.
const OBSERVED_UA_SURFACES: &[&str] = &["sdk-cli"];

/// `x-stainless-package-version` values the corpus self-reports.
const OBSERVED_STAINLESS_PACKAGE_VERSIONS: &[&str] = &["0.112.1"];

/// `x-stainless-runtime-version` values the corpus self-reports.
const OBSERVED_STAINLESS_RUNTIME_VERSIONS: &[&str] = &["v26.3.0"];

/// The UNION of every `anthropic-beta` flag the corpus sends. A union, not
/// a per-fixture set: the corpus carries two distinct beta sets (one
/// fixture additionally requests a cache-diagnosis flag) and both are
/// legitimate client behavior, so the pin is over what the client is
/// capable of sending.
const OBSERVED_CLIENT_BETAS: &[&str] = &[
    "cache-diagnosis-2026-04-07",
    "claude-code-20250219",
    "context-management-2025-06-27",
    "interleaved-thinking-2025-05-14",
    "prompt-caching-scope-2026-01-05",
    "thinking-token-count-2026-05-13",
];

/// Distinct `anthropic-beta` sets the corpus carries. Pinned as a count so
/// a capture that collapses or splits the populations is a review moment.
const OBSERVED_DISTINCT_BETA_SETS: usize = 2;

/// Reviewed rows the register must hold. Pinned so DELETING a row is a red
/// build rather than a quietly narrower claim.
const EXPECTED_REGISTER_ROWS: usize = 5;

/// How an observed dimension relates to the value routectl mints. The
/// vocabulary is closed: a new relation is a design conversation, not a
/// string to invent at a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// routectl mints exactly one value and every observation equals it.
    Matches,
    /// The two value sets are wholly disjoint: nothing observed is minted
    /// and nothing minted is observed. Legal, and the runtime drift warning
    /// is what surfaces it on live traffic.
    Diverges,
    /// The sets OVERLAP without either containing the other: each side
    /// carries values the other lacks, and they share some. Two
    /// intentionally different populations, so an equality assertion
    /// between them would be wrong -- this row pins the overlap and both
    /// differences instead. Total disjointness is NOT this relation; it is
    /// [`Relation::Diverges`], and conflating them would let a set that
    /// stopped sharing anything keep a verdict claiming it does.
    PartialOverlapByDesign,
}

/// One reviewed row: a dimension, the values observed, routectl's own
/// value, and the verdict a reader took on the pair.
struct RegisterRow {
    dimension: Dimension,
    observed: Vec<String>,
    minted: Vec<String>,
    relation: Relation,
    /// Why the relation is what it is, in terms a reader can re-check
    /// against the two value sets beside it.
    reason: &'static str,
}

/// The fingerprint dimensions this module derives and reviews. A closed
/// enum rather than strings so the derivation and the register cannot
/// disagree about a dimension's identity through a typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dimension {
    CliVersion,
    UaSurface,
    StainlessPackageVersion,
    StainlessRuntimeVersion,
    ClientBetas,
}

impl Dimension {
    /// Every dimension, so both halves iterate one list.
    const ALL: &'static [Self] = &[
        Self::CliVersion,
        Self::UaSurface,
        Self::StainlessPackageVersion,
        Self::StainlessRuntimeVersion,
        Self::ClientBetas,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::CliVersion => "claude-cli version (user-agent)",
            Self::UaSurface => "user-agent surface token",
            Self::StainlessPackageVersion => "x-stainless-package-version",
            Self::StainlessRuntimeVersion => "x-stainless-runtime-version",
            Self::ClientBetas => "anthropic-beta (client union vs routectl floor)",
        }
    }
}

fn sorted(values: &[&str]) -> Vec<String> {
    into_sorted(values.iter().map(|s| (*s).to_string()))
}

fn into_sorted(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = values.collect();
    out.sort();
    out.dedup();
    out
}

/// Build the reviewed register from the pinned observations and the live
/// minted values. The MINTED side is read from the code, never re-typed, so
/// a constant rolled forward changes this side automatically and the
/// relation check below re-adjudicates -- which is why a bump must re-take
/// the affected verdict in the same commit.
fn register() -> Vec<RegisterRow> {
    let identity_headers =
        routectl_core::identity::anthropic::default_claude_code_identity_headers();
    let minted_header = |name: &str| -> Vec<String> {
        identity_headers
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, v)| (*v).to_string())
            .collect()
    };

    vec![
        RegisterRow {
            dimension: Dimension::CliVersion,
            observed: sorted(OBSERVED_CLI_VERSIONS),
            minted: vec![
                routectl_core::identity::anthropic::compiled_claude_cli_version().to_string(),
            ],
            relation: Relation::Diverges,
            reason: "the corpus client is ahead of the compiled pin; the ingress drift \
                     warning reports the live gap and rolling the pin is an operator call",
        },
        RegisterRow {
            dimension: Dimension::UaSurface,
            observed: sorted(OBSERVED_UA_SURFACES),
            minted: vec![routectl_core::identity::anthropic::MINTED_UA_SURFACE.to_string()],
            relation: Relation::Diverges,
            reason: "the corpus client reports the SDK-driven surface while routectl mints \
                     the plain CLI surface -- a fingerprint difference independent of the \
                     version, so pinning the version alone would leave it unreviewed",
        },
        RegisterRow {
            dimension: Dimension::StainlessPackageVersion,
            observed: sorted(OBSERVED_STAINLESS_PACKAGE_VERSIONS),
            minted: minted_header("x-stainless-package-version"),
            relation: Relation::Diverges,
            reason: "the SDK version travels with the client release, so it drifts on the \
                     same cadence as the CLI version above",
        },
        RegisterRow {
            dimension: Dimension::StainlessRuntimeVersion,
            observed: sorted(OBSERVED_STAINLESS_RUNTIME_VERSIONS),
            minted: minted_header("x-stainless-runtime-version"),
            relation: Relation::Diverges,
            reason: "the client's bundled runtime major moved; routectl mints a fixed \
                     literal and does not read the host runtime",
        },
        RegisterRow {
            dimension: Dimension::ClientBetas,
            observed: sorted(OBSERVED_CLIENT_BETAS),
            minted: routectl_core::identity::anthropic::default_claude_code_anthropic_betas()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            relation: Relation::PartialOverlapByDesign,
            reason: "the client opts into model-gated flags the floor omits on purpose \
                     (forcing them rejects models that lack the capability), and the floor \
                     carries capability flags this client's requests do not need, while both \
                     carry the base flags -- overlapping populations, never an equality",
        },
    ]
}

// -- corpus derivation ----------------------------------------------------

/// What one fixture self-reported.
#[derive(Debug)]
struct Observation {
    fixture: String,
    cli_version: String,
    ua_surface: String,
    stainless_package_version: String,
    stainless_runtime_version: String,
    betas: BTreeSet<String>,
    has_session_id: bool,
}

fn lane_root() -> PathBuf {
    driver_root().join(LANE)
}

/// Case-insensitive single-valued header lookup. Two entries under one name
/// is ambiguity, not a value to pick from: which one the client meant is
/// unknowable, so it is an error.
fn required_header(
    headers: &[(String, String)],
    fixture: &str,
    name: &str,
) -> Result<String, String> {
    let mut found = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string());
    let value = found
        .next()
        .ok_or_else(|| format!("fixture `{fixture}` carries no `{name}` header"))?;
    if found.next().is_some() {
        return Err(format!(
            "fixture `{fixture}` carries `{name}` more than once; which value the client \
             sent is ambiguous"
        ));
    }
    if value.is_empty() {
        return Err(format!("fixture `{fixture}` carries an empty `{name}`"));
    }
    if value.contains(REDACTED) {
        return Err(format!(
            "fixture `{fixture}` carries a redacted `{name}`; there is no observation to pin"
        ));
    }
    Ok(value)
}

fn observe(fixture: &Fixture) -> Result<Observation, String> {
    let name = fixture.name.clone();
    let headers = &fixture.ingress_request_headers;

    let user_agent = required_header(headers, &name, "user-agent")?;
    let cli_version = routectl_core::identity::anthropic::parse_claude_cli_version(&user_agent)
        .ok_or_else(|| {
            format!(
                "fixture `{name}` carries a `user-agent` no stable Claude CLI version parses \
                 out of (value not echoed)"
            )
        })?
        .to_string();
    let ua_surface = routectl_core::identity::anthropic::parse_claude_cli_ua_surface(&user_agent)
        .ok_or_else(|| {
            format!(
                "fixture `{name}` carries a `user-agent` with no readable surface token \
                 (value not echoed)"
            )
        })?
        .to_string();

    let beta_value = required_header(headers, &name, "anthropic-beta")?;
    let betas: BTreeSet<String> = beta_value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if betas.is_empty() {
        return Err(format!(
            "fixture `{name}` carries an `anthropic-beta` header with no flags in it"
        ));
    }

    Ok(Observation {
        fixture: name.clone(),
        cli_version,
        ua_surface,
        stainless_package_version: required_header(headers, &name, "x-stainless-package-version")?,
        stainless_runtime_version: required_header(headers, &name, "x-stainless-runtime-version")?,
        betas,
        has_session_id: headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("x-claude-code-session-id")),
    })
}

/// Walk ONE lane directory and observe every fixture in it, or report why
/// the corpus cannot be read. Takes the path so the refusal controls can
/// drive it over a planted corpus instead of the committed one -- a refusal
/// nothing exercises is a refusal nobody knows works.
///
/// Absence and emptiness are both errors: the corpus IS the evidence this
/// module pins, so nothing to walk means nothing to assert.
fn observations_at(lane: &Path) -> Result<Vec<Observation>, String> {
    if !lane.is_dir() {
        return Err(format!(
            "the fixture lane directory is missing at {}; there is no corpus to pin",
            lane.display()
        ));
    }
    let loaded =
        discover_fixtures(lane).map_err(|e| format!("walking {} failed: {e}", lane.display()))?;
    if loaded.entries_walked() == 0 {
        return Err(format!(
            "the corpus at {} holds no fixture directories; an empty corpus satisfies \
             every assertion by observing nothing",
            lane.display()
        ));
    }
    if loaded.skipped > 0 {
        return Err(format!(
            "{} fixture(s) under {} failed to load; a fingerprint pin over a thinned corpus \
             would silently stop covering them ({INGRESS_HEADERS} malformed, or a body/meta \
             parse failure -- see the loader's stderr lines)",
            loaded.skipped,
            lane.display(),
        ));
    }
    loaded.fixtures.iter().map(observe).collect()
}

fn observed_or_panic() -> Vec<Observation> {
    match observations_at(&lane_root()) {
        Ok(observed) if observed.len() >= MIN_FIXTURES => observed,
        Ok(observed) => panic!(
            "only {} fixture(s) observed, below the {MIN_FIXTURES} this pin was derived over; \
             a thinned corpus reports as coverage it no longer has",
            observed.len()
        ),
        Err(e) => panic!("{e}"),
    }
}

/// The single derived map: every dimension's observed values, read off the
/// corpus. Both the per-dimension pins and the register's coverage check
/// read THIS, so a dimension cannot be derived in one place and reviewed in
/// another under a different name.
fn derived_dimensions(observed: &[Observation]) -> BTreeMap<Dimension, Vec<String>> {
    let mut derived = BTreeMap::new();
    derived.insert(
        Dimension::CliVersion,
        into_sorted(observed.iter().map(|o| o.cli_version.clone())),
    );
    derived.insert(
        Dimension::UaSurface,
        into_sorted(observed.iter().map(|o| o.ua_surface.clone())),
    );
    derived.insert(
        Dimension::StainlessPackageVersion,
        into_sorted(observed.iter().map(|o| o.stainless_package_version.clone())),
    );
    derived.insert(
        Dimension::StainlessRuntimeVersion,
        into_sorted(observed.iter().map(|o| o.stainless_runtime_version.clone())),
    );
    derived.insert(
        Dimension::ClientBetas,
        into_sorted(observed.iter().flat_map(|o| o.betas.clone())),
    );
    derived
}

// -- the pins -------------------------------------------------------------

fn assert_exact_set(dimension: &str, observed: &[String], pinned: &[&str]) {
    assert!(
        !observed.is_empty(),
        "{dimension}: the corpus yielded no values; an empty observed set is a failed \
         derivation, not a satisfied pin"
    );
    assert_eq!(
        observed,
        sorted(pinned),
        "{dimension}: the corpus no longer carries exactly the reviewed values. A new value \
         here means a capture landed with a fingerprint nobody has read yet -- read what \
         changed, then extend the pinned list in this commit"
    );
}

#[test]
fn the_corpus_self_reports_exactly_the_pinned_cli_versions() {
    let derived = derived_dimensions(&observed_or_panic());

    assert_exact_set(
        Dimension::CliVersion.label(),
        &derived[&Dimension::CliVersion],
        OBSERVED_CLI_VERSIONS,
    );
}

/// The version and the surface are separate pins on purpose: a client that
/// changed only its surface token would otherwise land unreviewed under a
/// green "version pinned" test.
#[test]
fn the_corpus_self_reports_exactly_the_pinned_user_agent_surface() {
    let derived = derived_dimensions(&observed_or_panic());

    assert_exact_set(
        Dimension::UaSurface.label(),
        &derived[&Dimension::UaSurface],
        OBSERVED_UA_SURFACES,
    );
}

#[test]
fn the_corpus_self_reports_exactly_the_pinned_stainless_versions() {
    let derived = derived_dimensions(&observed_or_panic());

    assert_exact_set(
        Dimension::StainlessPackageVersion.label(),
        &derived[&Dimension::StainlessPackageVersion],
        OBSERVED_STAINLESS_PACKAGE_VERSIONS,
    );
    assert_exact_set(
        Dimension::StainlessRuntimeVersion.label(),
        &derived[&Dimension::StainlessRuntimeVersion],
        OBSERVED_STAINLESS_RUNTIME_VERSIONS,
    );
}

#[test]
fn the_corpus_sends_exactly_the_pinned_client_beta_union() {
    let observed = observed_or_panic();
    let derived = derived_dimensions(&observed);

    assert_exact_set(
        Dimension::ClientBetas.label(),
        &derived[&Dimension::ClientBetas],
        OBSERVED_CLIENT_BETAS,
    );

    let distinct: BTreeSet<Vec<String>> = observed
        .iter()
        .map(|o| o.betas.iter().cloned().collect())
        .collect();
    assert_eq!(
        distinct.len(),
        OBSERVED_DISTINCT_BETA_SETS,
        "the corpus carries a different number of distinct beta populations than reviewed; \
         sets observed: {distinct:?}"
    );
}

/// The coverage ceiling, asserted rather than asserted-about: every fixture
/// carries the session-id header the default classification keys on, so the
/// whole corpus exercises the genuine-client arm and the non-client cloak
/// arm has no fixture. A capture without the header widens what this
/// corpus covers, which is a claim to re-take, not a detail to absorb.
#[test]
fn every_fixture_carries_the_session_id_that_marks_the_genuine_client_arm() {
    let observed = observed_or_panic();

    let without: Vec<&str> = observed
        .iter()
        .filter(|o| !o.has_session_id)
        .map(|o| o.fixture.as_str())
        .collect();
    assert!(
        without.is_empty(),
        "these fixtures carry no session id, so the corpus no longer covers only the \
         genuine-client arm: {without:?}. The non-client arm's transforms have no fixture \
         here -- update this module's stated ceiling before widening it"
    );
}

#[test]
fn every_reviewed_row_still_holds_its_relation_against_the_minted_values() {
    let rows = register();
    assert_eq!(
        rows.len(),
        EXPECTED_REGISTER_ROWS,
        "the register's row count changed. Deleting a row narrows what this module claims \
         without narrowing what it appears to claim"
    );

    let mut seen = BTreeSet::new();
    for row in &rows {
        assert!(
            seen.insert(row.dimension),
            "dimension `{}` is registered twice",
            row.dimension.label()
        );
        assert!(
            !row.observed.is_empty(),
            "row `{}` pins no observed value",
            row.dimension.label()
        );
        assert!(
            !row.minted.is_empty(),
            "row `{}` reads no minted value; the relation has nothing to hold against",
            row.dimension.label()
        );
        assert!(
            !row.reason.trim().is_empty(),
            "row `{}` records a verdict with no re-checkable reason",
            row.dimension.label()
        );

        relation_holds(row.relation, &row.observed, &row.minted).unwrap_or_else(|why| {
            panic!(
                "row `{}` no longer holds its reviewed verdict: {why}. A minted constant \
                 rolled forward must re-take this row's relation in the same commit",
                row.dimension.label()
            )
        });
    }
}

/// Whether `relation` is still true of the two value sets, or why not. A
/// named decision rather than an inline match so the control below can
/// exercise every variant in both directions -- a verdict checker nothing
/// disagrees with is a verdict checker that accepts anything.
fn relation_holds(
    relation: Relation,
    observed: &[String],
    minted: &[String],
) -> Result<(), String> {
    let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();
    let minted: BTreeSet<&str> = minted.iter().map(String::as_str).collect();
    let shared: Vec<&&str> = observed.intersection(&minted).collect();
    let client_only: Vec<&&str> = observed.difference(&minted).collect();
    let minted_only: Vec<&&str> = minted.difference(&observed).collect();

    match relation {
        Relation::Matches => (observed == minted).then_some(()).ok_or_else(|| {
            format!("registered as Matches but the values differ (minted: {minted:?})")
        }),
        Relation::Diverges => shared.is_empty().then_some(()).ok_or_else(|| {
            format!("registered as Diverges but these values appear on both sides: {shared:?}")
        }),
        Relation::PartialOverlapByDesign => {
            if shared.is_empty() {
                return Err(format!(
                    "registered as PartialOverlapByDesign but the sets share nothing -- \
                     that is Diverges, and keeping this verdict would claim an overlap \
                     that is gone. client-only: {client_only:?}, minted-only: {minted_only:?}"
                ));
            }
            if client_only.is_empty() || minted_only.is_empty() {
                return Err(format!(
                    "registered as PartialOverlapByDesign, which claims BOTH sides carry \
                     values the other lacks. client-only: {client_only:?}, \
                     minted-only: {minted_only:?}"
                ));
            }
            Ok(())
        }
    }
}

/// Non-vacuity control for [`relation_holds`]: every verdict in the closed
/// vocabulary must ACCEPT the shape it names and REJECT every other shape.
/// Without this, a checker that returned `Ok` unconditionally would satisfy
/// the register test above over any values at all.
#[test]
fn each_verdict_accepts_only_the_shape_it_names() {
    let same = (vec!["a".to_string()], vec!["a".to_string()]);
    let disjoint = (vec!["a".to_string()], vec!["b".to_string()]);
    let overlapping = (
        vec!["a".to_string(), "shared".to_string()],
        vec!["b".to_string(), "shared".to_string()],
    );
    let subset = (
        vec!["shared".to_string()],
        vec!["shared".to_string(), "b".to_string()],
    );

    for (relation, accepted, rejected) in [
        (
            Relation::Matches,
            vec![&same],
            vec![&disjoint, &overlapping, &subset],
        ),
        (
            Relation::Diverges,
            vec![&disjoint],
            vec![&same, &overlapping, &subset],
        ),
        (
            Relation::PartialOverlapByDesign,
            vec![&overlapping],
            // Disjoint belongs to Diverges; a subset has no client-only
            // side; equal sets have neither difference.
            vec![&same, &disjoint, &subset],
        ),
    ] {
        for (observed, minted) in accepted {
            assert!(
                relation_holds(relation, observed, minted).is_ok(),
                "{relation:?} must accept observed={observed:?} minted={minted:?}"
            );
        }
        for (observed, minted) in rejected {
            assert!(
                relation_holds(relation, observed, minted).is_err(),
                "{relation:?} must reject observed={observed:?} minted={minted:?}"
            );
        }
    }
}

/// The register's rows must be exactly the dimensions the derivation
/// produces, compared against the ONE derived map. A row dropped from
/// either side is how a register quietly stops covering a dimension while
/// still reading as complete.
#[test]
fn the_register_covers_every_derived_dimension() {
    let derived = derived_dimensions(&observed_or_panic());
    let rows = register();

    let registered: BTreeSet<Dimension> = rows.iter().map(|r| r.dimension).collect();
    let derived_keys: BTreeSet<Dimension> = derived.keys().copied().collect();
    let all: BTreeSet<Dimension> = Dimension::ALL.iter().copied().collect();

    assert_eq!(
        registered, derived_keys,
        "every derived dimension needs a reviewed relation, and a relation with nothing \
         deriving it pins prose"
    );
    assert_eq!(
        derived_keys, all,
        "the derivation stopped covering a declared dimension"
    );
}

/// The pinned observed sets must be the sets the live corpus yields -- the
/// weld between the two halves above. Without it, the register could hold a
/// reviewed verdict over values the corpus stopped carrying.
#[test]
fn the_registered_observations_are_the_ones_the_corpus_yields() {
    let derived = derived_dimensions(&observed_or_panic());
    let rows = register();

    for row in &rows {
        assert_eq!(
            row.observed,
            derived[&row.dimension],
            "row `{}` pins observations the corpus does not carry",
            row.dimension.label()
        );
    }
}

// -- refusal controls -----------------------------------------------------
//
// Each proves one loud failure actually fires. A refusal nothing exercises
// is a refusal nobody knows works, and these are the failures that keep an
// empty or broken corpus from passing every pin above by observing nothing.

/// Ingress headers of a fixture that observes cleanly on every dimension.
/// The POSITIVE CONTROL the refusal cases below are perturbations of: if
/// this did not observe, every refusal assertion would pass for the wrong
/// reason.
fn good_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("content-type", "application/json"),
        ("user-agent", "claude-cli/2.1.246 (external, sdk-cli)"),
        ("x-claude-code-session-id", "sess-1"),
        ("x-stainless-package-version", "0.112.1"),
        ("x-stainless-runtime-version", "v26.3.0"),
        (
            "anthropic-beta",
            "claude-code-20250219,interleaved-thinking-2025-05-14",
        ),
    ]
}

/// Plant a one-case lane under a fresh tempdir and observe it. Returns the
/// tempdir (keep it alive) alongside the outcome.
fn observe_planted(
    headers: &[(&str, &str)],
) -> (tempfile::TempDir, Result<Vec<Observation>, String>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("driver");
    common::replay::plant::plant_driver_case_with_ingress(
        &root,
        LANE,
        "synthetic",
        &common::replay::plant::current_meta(),
        headers,
        &serde_json::json!({"model": "m", "messages": []}),
    );
    let outcome = observations_at(&root.join(LANE));
    (tmp, outcome)
}

fn headers_without(name: &str) -> Vec<(&'static str, &'static str)> {
    good_headers()
        .into_iter()
        .filter(|(n, _)| !n.eq_ignore_ascii_case(name))
        .collect()
}

fn headers_with_value(
    name: &'static str,
    value: &'static str,
) -> Vec<(&'static str, &'static str)> {
    let mut headers = headers_without(name);
    headers.push((name, value));
    headers
}

#[test]
fn the_positive_control_fixture_observes_on_every_dimension() {
    let (_tmp, outcome) = observe_planted(&good_headers());

    let observed = outcome.expect("the control fixture must observe cleanly");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].cli_version, "2.1.246");
    assert_eq!(observed[0].ua_surface, "sdk-cli");
    assert_eq!(observed[0].stainless_package_version, "0.112.1");
    assert_eq!(observed[0].stainless_runtime_version, "v26.3.0");
    assert_eq!(observed[0].betas.len(), 2);
    assert!(observed[0].has_session_id);
}

#[test]
fn an_absent_required_header_is_refused_by_name() {
    for name in [
        "user-agent",
        "x-stainless-package-version",
        "x-stainless-runtime-version",
        "anthropic-beta",
    ] {
        let (_tmp, outcome) = observe_planted(&headers_without(name));

        let err = outcome.expect_err("an absent required header must be refused");
        assert!(
            err.contains(name),
            "the refusal must name the missing header; got {err}"
        );
    }
}

#[test]
fn a_duplicated_required_header_is_refused_as_ambiguous() {
    let mut headers = good_headers();
    headers.push(("x-stainless-package-version", "0.999.0"));

    let (_tmp, outcome) = observe_planted(&headers);

    let err = outcome.expect_err("a duplicated required header must be refused");
    assert!(err.contains("ambiguous"), "got {err}");
}

#[test]
fn a_whitespace_only_required_header_is_refused_as_empty() {
    let (_tmp, outcome) =
        observe_planted(&headers_with_value("x-stainless-runtime-version", "   "));

    let err = outcome.expect_err("a whitespace-only value carries no observation");
    assert!(err.contains("empty"), "got {err}");
}

#[test]
fn a_redacted_required_header_is_refused_rather_than_pinned() {
    let (_tmp, outcome) = observe_planted(&headers_with_value(
        "x-stainless-package-version",
        "[REDACTED]",
    ));

    let err = outcome.expect_err("a redacted value must never become a pinned observation");
    assert!(err.contains("redacted"), "got {err}");
}

#[test]
fn an_unparseable_user_agent_is_refused_without_echoing_the_value() {
    for ua in [
        "Mozilla/5.0 (X11; Linux)",
        "claude-cli/2.1.246.1e8 (external, cli)",
        "claude-cli/",
    ] {
        let (_tmp, outcome) = observe_planted(&headers_with_value("user-agent", ua));

        let err = outcome.expect_err("an unparseable User-Agent must be refused");
        assert!(
            !err.contains(ua),
            "the refusal must not echo the header value; got {err}"
        );
    }
}

#[test]
fn a_user_agent_with_no_surface_token_is_refused() {
    let (_tmp, outcome) = observe_planted(&headers_with_value("user-agent", "claude-cli/2.1.246"));

    let err = outcome.expect_err("a User-Agent with no parenthetical has no surface observation");
    assert!(err.contains("surface"), "got {err}");
}

#[test]
fn an_empty_beta_header_is_refused() {
    for value in [",", " , "] {
        let (_tmp, outcome) = observe_planted(&headers_with_value("anthropic-beta", value));

        let err = outcome.expect_err("a beta header with no flags carries no observation");
        assert!(err.contains("anthropic-beta"), "got {err}");
    }
}

#[test]
fn a_missing_corpus_directory_is_refused_not_read_as_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let err = observations_at(&tmp.path().join("no-such-lane"))
        .expect_err("a missing lane directory must be refused");

    assert!(err.contains("missing"), "got {err}");
}

#[test]
fn an_empty_corpus_directory_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lane = tmp.path().join(LANE);
    std::fs::create_dir_all(&lane).unwrap();

    let err = observations_at(&lane).expect_err("an empty corpus must be refused");

    assert!(err.contains("no fixture directories"), "got {err}");
}

#[test]
fn a_corpus_thinned_by_an_unloadable_case_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("driver");
    common::replay::plant::plant_driver_case_with_ingress(
        &root,
        LANE,
        "good",
        &common::replay::plant::current_meta(),
        &good_headers(),
        &serde_json::json!({"model": "m", "messages": []}),
    );
    common::replay::plant::plant_unloadable_driver_case(&root, LANE, "broken");

    let err = observations_at(&root.join(LANE))
        .expect_err("a corpus with an unloadable case must be refused, not silently thinned");

    assert!(err.contains("failed to load"), "got {err}");
}

#[test]
fn a_malformed_headers_file_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("driver");
    let case = common::replay::plant::plant_driver_case_with_ingress(
        &root,
        LANE,
        "malformed",
        &common::replay::plant::current_meta(),
        &good_headers(),
        &serde_json::json!({"model": "m", "messages": []}),
    );
    // An OBJECT where the loader requires an array of pairs.
    std::fs::write(
        case.join(INGRESS_HEADERS),
        b"{\"user-agent\":\"claude-cli/2.1.246\"}",
    )
    .unwrap();

    let err = observations_at(&root.join(LANE))
        .expect_err("a headers file that is not an array of pairs must be refused");

    assert!(err.contains("failed to load"), "got {err}");
}
