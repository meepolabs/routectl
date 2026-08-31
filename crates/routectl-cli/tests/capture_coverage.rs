//! Derived coverage view over the committed driver corpus.
//!
//! This is a REPORT, not a gate: it prints what the committed driver
//! corpus under `tests/fixtures/driver/` covers and does not, and
//! asserts nothing about the LEVEL of that coverage. Absence of a cell
//! is a deployment property (no AWS credentials, no Gemini key, a
//! client this feature does not drive) rather than a defect, so the one
//! thing this view must never do is fail because a lane nobody funded
//! stayed empty.
//!
//! Everything below is DERIVED at run time from three things already on
//! disk and never persisted as a fourth:
//! - the committed driver corpus's own directory listing
//!   ([`discover_driver_fixtures`]), never `manifest.jsonl` (that file is
//!   append-only across reruns of a directory that gets REPLACED, so a
//!   manifest-derived view resurrects deleted cells)
//! - `scripts/drivers/cases/*.json`, which name the wire patterns a case
//!   file actually claims (the columns of the grid)
//! - `scripts/drivers/config/*.toml`, whose file stems name the egress
//!   lanes this checkout can actually drive (the shippable subset)
//!
//! A "cell" here is `(egress lane token, wire pattern)`. That is also
//! the CANONICAL identity the regression check diffs across git
//! revisions: a case directory rename that keeps the same lane and wire
//! pattern is not a regression, because the cell identity never
//! mentions the directory name.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use common::replay::{
    EgressLane, Fixture, LoadedCorpus, Reachability, discover_driver_fixtures, driver_root,
    egress_lane_from_fixture_kind, front_proxy_reachability, workspace_root,
};

/// Relative-to-workspace-root path of the case files that declare the
/// wire-pattern vocabulary's grid columns. Read for their `wire_pattern`
/// field only; the closed vocabulary itself lives in
/// `scripts/drivers/lib/validate_case.py` and is welded elsewhere
/// (`wire_pattern_weld.rs`) -- this view never re-derives it, it just
/// reads which patterns a real case file on disk actually claims.
const DRIVER_CASES_DIR: &str = "scripts/drivers/cases";

/// Relative-to-workspace-root path of the per-lane driver configs. A
/// lane with no config here structurally cannot be driven by this
/// checkout, independent of any credential -- the absence is read off
/// the directory listing, never hand-listed.
const DRIVER_CONFIG_DIR: &str = "scripts/drivers/config";

/// The filename this view must never open. Present here only so its
/// name appears exactly once, next to the comment explaining why.
const FORBIDDEN_MANIFEST_FILE: &str = "manifest.jsonl";

/// Env var naming the git revision this run treats as "the prior run".
/// Unset (the common case: a local checkout or a shallow CI clone with
/// no ancestor commit reachable) means the regression leg prints its
/// "not evaluated" line rather than silently passing.
const BASELINE_REV_ENV: &str = "ROUTECTL_COVERAGE_BASE_REV";

/// Path (relative to the workspace root) the regression leg diffs
/// across revisions.
const DRIVER_ROOT_REL: &str = "crates/routectl-cli/tests/fixtures/driver";

// ---------------------------------------------------------------------
// Wild-evidence citations. Verbatim backlog ids -- this view is a
// consumer of them, never their author. `bl-codex-driver-lane` is
// deliberately absent: this feature drains it, so an uncovered
// openai-responses cell here names the capture gap, not that filing.
// ---------------------------------------------------------------------

const CITE_BEDROCK_CREDENTIALS: &str = "bl-bedrock-capture-cells-need-aws-credentials";
const CITE_GEMINI_NO_KEY: &str =
    "bl-gemini-capture-cell-has-no-api-key-and-cloudcode-is-a-second-dialect";
const CITE_OPENCODE_DRIVER_LANE: &str = "bl-opencode-driver-lane";
const CITE_PI_DRIVER_LANE: &str = "bl-pi-driver-lane";
const CITE_HERMES_AGENT_DRIVER_LANE: &str = "bl-hermes-agent-driver-lane";
const CITE_LARGE_CONTEXT_OFF_ANTHROPIC: &str =
    "bl-large-context-on-any-non-anthropic-lane-uncaptured";
const CITE_CREDENTIAL_REF_SCHEME_GAP: &str =
    "bl-file-and-oauth-credential-refs-unexercised-by-any-driven-capture";

/// Driver client identities this feature does not drive at all. Not a
/// lane on the grid (no cell would ever carry their name), so they are
/// reported alongside the grid rather than folded into it.
const EXCLUDED_DRIVER_CLIENTS: &[(&str, &str)] = &[
    ("opencode", CITE_OPENCODE_DRIVER_LANE),
    ("pi", CITE_PI_DRIVER_LANE),
    ("hermes-agent", CITE_HERMES_AGENT_DRIVER_LANE),
];

/// One coverage grid cell: an egress lane crossed with a wire pattern.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Cell {
    lane: String,
    pattern: String,
}

impl std::fmt::Display for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.lane, self.pattern)
    }
}

fn repo_root() -> PathBuf {
    workspace_root().expect("workspace root must resolve for the coverage view to run at all")
}

/// Wire patterns actually claimed by a case file on disk, read from the
/// case JSON's own `wire_pattern` field -- never the Python vocabulary
/// constant, so this stays a read of committed content, not a second
/// derivation of it.
fn declared_wire_patterns(root: &Path) -> BTreeSet<String> {
    let cases_dir = root.join(DRIVER_CASES_DIR);
    let entries = std::fs::read_dir(&cases_dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", cases_dir.display()));
    let mut patterns = BTreeSet::new();
    for entry in entries {
        let entry = entry.expect("directory entry read failed");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let value: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()));
        if let Some(pattern) = value.get("wire_pattern").and_then(Value::as_str) {
            patterns.insert(pattern.to_string());
        }
    }
    patterns
}

/// Egress-lane tokens this checkout can actually drive, read from the
/// per-lane config file stems (`anthropic-api.toml`,
/// `anthropic-api.front-proxy.toml`, ... -> `anthropic-api`). A lane
/// with no file here is structurally undriveable regardless of
/// credentials.
fn configured_lane_tokens(root: &Path) -> BTreeSet<String> {
    let config_dir = root.join(DRIVER_CONFIG_DIR);
    let entries = std::fs::read_dir(&config_dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", config_dir.display()));
    let mut tokens = BTreeSet::new();
    for entry in entries {
        let entry = entry.expect("directory entry read failed");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let lane_token = stem.split('.').next().unwrap_or(stem);
            tokens.insert(lane_token.to_string());
        }
    }
    tokens
}

/// The full grid: every [`EgressLane`] crossed with every declared wire
/// pattern. `M`/`T` in the printed denominators are sizes of subsets of
/// this set, never a literal.
fn all_cells(root: &Path) -> BTreeSet<Cell> {
    let patterns = declared_wire_patterns(root);
    let mut cells = BTreeSet::new();
    for lane in EgressLane::ALL {
        for pattern in &patterns {
            cells.insert(Cell {
                lane: lane.token().to_string(),
                pattern: pattern.clone(),
            });
        }
    }
    cells
}

/// Why a lane cannot ship this feature, when it cannot. `None` means the
/// lane is shippable (a driver config exists for it).
fn unshippable_reason<'a>(lane_token: &str, configured: &BTreeSet<String>) -> Option<&'a str> {
    if configured.contains(lane_token) {
        return None;
    }
    match lane_token {
        "bedrock-invoke" | "bedrock-converse" => Some(CITE_BEDROCK_CREDENTIALS),
        "gemini" => Some(CITE_GEMINI_NO_KEY),
        _ => Some("(no driver config for this lane)"),
    }
}

/// Cite why an otherwise-shippable, currently-uncovered cell is empty.
/// `None` means the gap has no known filing -- printed plainly rather
/// than attributed to a filing that does not cover it (in particular,
/// never `bl-codex-driver-lane`: this feature drains that one).
fn uncovered_shippable_reason(cell: &Cell) -> Option<&'static str> {
    if cell.pattern == "large-context" && cell.lane != "anthropic-api" {
        return Some(CITE_LARGE_CONTEXT_OFF_ANTHROPIC);
    }
    None
}

/// Cells the committed driver corpus actually covers, read from its
/// directory listing via [`discover_driver_fixtures`] -- which walks
/// `child_dirs()`, real directory entries, and never opens
/// `manifest.jsonl`. See [`manifest_jsonl_is_never_consulted`] for the
/// mutation-provable half of that claim.
fn covered_cells_from_corpus(corpus: &LoadedCorpus) -> BTreeSet<Cell> {
    let mut cells = BTreeSet::new();
    for fixture in &corpus.fixtures {
        if let Some(cell) = cell_for_fixture(fixture) {
            cells.insert(cell);
        }
    }
    cells
}

fn cell_for_fixture(fixture: &Fixture) -> Option<Cell> {
    let lane = egress_lane_from_fixture_kind(&fixture.meta.provider_kind).ok()?;
    if fixture.meta.wire_pattern.is_empty() {
        return None;
    }
    Some(Cell {
        lane: lane.token().to_string(),
        pattern: fixture.meta.wire_pattern.clone(),
    })
}

/// One `MIXED-VERSION` line per client name whose captured cells carry
/// more than one distinct `meta.client.version`. A client with exactly
/// one observed version never appears here -- see the paired positive
/// and negative control tests below.
fn mixed_version_labels(corpus: &LoadedCorpus) -> Vec<String> {
    let mut by_client: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for fixture in &corpus.fixtures {
        let name = fixture.meta.client.name.trim();
        let version = fixture.meta.client.version.trim();
        if name.is_empty() || version.is_empty() {
            continue;
        }
        by_client
            .entry(name.to_string())
            .or_default()
            .insert(version.to_string());
    }
    by_client
        .into_iter()
        .filter(|(_, versions)| versions.len() > 1)
        .map(|(name, versions)| {
            let versions: Vec<String> = versions.into_iter().collect();
            format!(
                "MIXED-VERSION: client={name} versions=[{}]",
                versions.join(", ")
            )
        })
        .collect()
}

/// Read the covered-cell set out of `<rev>:<DRIVER_ROOT_REL>` via
/// `git ls-tree` + `git show`, never `git checkout` -- the working tree
/// is untouched. Fails closed (`Err`) on anything short of a clean
/// resolve, so the caller's fallback is always the loud
/// "not evaluated" line, never a silent empty diff.
fn baseline_covered_cells(root: &Path, rev: &str) -> Result<BTreeSet<Cell>, String> {
    let verify = Command::new("git")
        .args(["-C", &root.display().to_string(), "rev-parse", "--verify"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .map_err(|e| e.to_string())?;
    if !verify.status.success() {
        return Err(String::from_utf8_lossy(&verify.stderr).into_owned());
    }

    let listing = Command::new("git")
        .args([
            "-C",
            &root.display().to_string(),
            "ls-tree",
            "-r",
            "--name-only",
        ])
        .arg(rev)
        .args(["--", DRIVER_ROOT_REL])
        .output()
        .map_err(|e| e.to_string())?;
    if !listing.status.success() {
        return Err(String::from_utf8_lossy(&listing.stderr).into_owned());
    }
    let listing = String::from_utf8_lossy(&listing.stdout);

    let mut cells = BTreeSet::new();
    for path in listing.lines().filter(|line| line.ends_with("/meta.json")) {
        let show = Command::new("git")
            .args(["-C", &root.display().to_string(), "show"])
            .arg(format!("{rev}:{path}"))
            .output()
            .map_err(|e| e.to_string())?;
        if !show.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&show.stdout);
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let provider_kind = value
            .get("provider_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let pattern = value
            .get("wire_pattern")
            .and_then(Value::as_str)
            .unwrap_or("");
        if provider_kind.is_empty() || pattern.is_empty() {
            continue;
        }
        if let Ok(lane) = egress_lane_from_fixture_kind(provider_kind) {
            cells.insert(Cell {
                lane: lane.token().to_string(),
                pattern: pattern.to_string(),
            });
        }
    }
    Ok(cells)
}

/// The regression leg's one printed line. Never a silent pass: with no
/// usable baseline it says so explicitly.
fn regression_line(root: &Path, current: &BTreeSet<Cell>) -> String {
    let rev = match std::env::var(BASELINE_REV_ENV) {
        Ok(rev) if !rev.trim().is_empty() => rev,
        _ => return "REGRESSION: not evaluated (no base revision)".to_string(),
    };
    match baseline_covered_cells(root, &rev) {
        Err(_) => "REGRESSION: not evaluated (no base revision)".to_string(),
        Ok(baseline) => {
            let lost: BTreeSet<&Cell> = baseline.difference(current).collect();
            if lost.is_empty() {
                format!("REGRESSION: none (base {rev})")
            } else {
                let names: Vec<String> = lost.iter().map(|c| c.to_string()).collect();
                format!("REGRESSION: lost coverage for {}", names.join(", "))
            }
        }
    }
}

/// Print the front-proxy reachability line, sourced from f3's
/// [`front_proxy_reachability`] -- imported, never re-derived. The one
/// hard assertion this view makes outside the mixed-version and
/// manifest checks: the settled pin (anthropic-only) has not moved
/// under this view without it noticing.
fn print_front_proxy_reachability() {
    for ingress in ["anthropic", "openai", "openai-responses"] {
        match front_proxy_reachability(ingress) {
            Ok(Reachability::Reachable) => {
                println!("front-proxy reachability: {ingress}=reachable");
            }
            Ok(unreachable) => {
                println!(
                    "front-proxy reachability: {ingress}=unreachable ({})",
                    unreachable.reason().unwrap_or("no reason given")
                );
            }
            Err(e) => panic!("front-proxy pin unresolvable: {e}"),
        }
    }
    assert!(
        matches!(
            front_proxy_reachability("anthropic"),
            Ok(Reachability::Reachable)
        ),
        "the settled MITM pin is anthropic-only; this view's front-proxy \
         framing assumes that and must notice if it ever moves"
    );
    assert!(
        !matches!(
            front_proxy_reachability("openai"),
            Ok(Reachability::Reachable)
        ),
        "openai must stay unreachable through the front proxy while the \
         pin is anthropic-only (paired control for the assertion above)"
    );
}

#[test]
fn coverage_view_prints_derived_matrix_over_committed_corpus() {
    let root = repo_root();
    let driver = driver_root();

    let corpus = match discover_driver_fixtures(&driver) {
        Ok(corpus) => corpus,
        Err(e) => {
            println!("capture_coverage: NOT RUN (driver corpus unreadable: {e})");
            return;
        }
    };

    let configured = configured_lane_tokens(&root);
    let grid = all_cells(&root);
    let covered = covered_cells_from_corpus(&corpus);

    let shippable: BTreeSet<&Cell> = grid
        .iter()
        .filter(|cell| unshippable_reason(&cell.lane, &configured).is_none())
        .collect();
    let covered_shippable = covered.iter().filter(|c| shippable.contains(c)).count();
    let covered_total = covered.iter().filter(|c| grid.contains(*c)).count();

    println!("{covered_shippable} of {} shippable cells", shippable.len());
    println!("{covered_total} of {} total cells", grid.len());

    for cell in &grid {
        if covered.contains(cell) {
            continue;
        }
        match unshippable_reason(&cell.lane, &configured) {
            Some(reason) => println!("EXCLUDED {cell}: {reason}"),
            None => match uncovered_shippable_reason(cell) {
                Some(citation) => println!("UNCOVERED {cell}: {citation}"),
                None => println!("UNCOVERED {cell}: (no filing; capture pending)"),
            },
        }
    }

    for (client, citation) in EXCLUDED_DRIVER_CLIENTS {
        println!("EXCLUDED driver client {client}: {citation}");
    }
    println!(
        "NOTE credential-ref schemes (file, oauth) unexercised by any driven capture: {CITE_CREDENTIAL_REF_SCHEME_GAP}"
    );

    for label in mixed_version_labels(&corpus) {
        println!("{label}");
    }

    print_front_proxy_reachability();

    println!("{}", regression_line(&root, &covered));
}

// ---------------------------------------------------------------------
// Unit tests: mutation-provable controls for the pieces the printed
// view above cannot assert about itself (a live corpus's actual
// contents are not something this suite may assert a level on).
// ---------------------------------------------------------------------

#[cfg(test)]
mod unit_tests {
    use super::*;
    use common::replay::plant_driver_case;
    use std::fs;
    use tempfile::tempdir;

    fn client_json(name: &str, version: &str) -> Value {
        serde_json::json!({
            "name": name,
            "version": version,
            "binary_version": "",
            "connection_mode": "base-url",
        })
    }

    fn plant_with_client(root: &Path, lane: &str, case_id: &str, name: &str, version: &str) {
        let dir = plant_driver_case(root, lane, case_id);
        let meta_path = dir.join("meta.json");
        let mut meta: Value =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta["client"] = client_json(name, version);
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    }

    #[test]
    fn mixed_version_label_fires_when_one_client_shows_two_versions() {
        let dir = tempdir().unwrap();
        plant_with_client(
            dir.path(),
            "anthropic-api",
            "case-a",
            "claude-cli",
            "2.1.246",
        );
        plant_with_client(
            dir.path(),
            "anthropic-api",
            "case-b",
            "claude-cli",
            "2.1.250",
        );
        let corpus = discover_driver_fixtures(dir.path()).unwrap();

        let labels = mixed_version_labels(&corpus);

        assert_eq!(labels.len(), 1);
        assert!(labels[0].contains("client=claude-cli"));
        assert!(labels[0].contains("2.1.246"));
        assert!(labels[0].contains("2.1.250"));
    }

    /// Paired negative control: a single client at a single version
    /// must never be labelled, so the fixture above proves this test
    /// would have caught it had the version actually varied.
    #[test]
    fn mixed_version_label_absent_when_one_client_shows_one_version() {
        let dir = tempdir().unwrap();
        plant_with_client(
            dir.path(),
            "anthropic-api",
            "case-a",
            "claude-cli",
            "2.1.246",
        );
        plant_with_client(
            dir.path(),
            "anthropic-api",
            "case-b",
            "claude-cli",
            "2.1.246",
        );
        let corpus = discover_driver_fixtures(dir.path()).unwrap();

        let labels = mixed_version_labels(&corpus);

        assert!(labels.is_empty());
    }

    /// Proves the coverage cell set is read exclusively off the
    /// directory listing: a decoy `manifest.jsonl` naming a cell that
    /// has no real fixture directory must never surface as covered.
    /// Mutation-verified: pointing `covered_cells_from_corpus` at a
    /// manifest-derived list instead of `discover_driver_fixtures`
    /// makes this test fail, which is the point of carrying it.
    #[test]
    fn manifest_jsonl_is_never_consulted_for_coverage() {
        let dir = tempdir().unwrap();
        plant_driver_case(dir.path(), "anthropic-api", "real-case");
        fs::write(
            dir.path().join(FORBIDDEN_MANIFEST_FILE),
            "{\"lane\": \"gemini\", \"wire_pattern\": \"baseline\", \"case_id\": \"decoy\"}\n",
        )
        .unwrap();

        let corpus = discover_driver_fixtures(dir.path()).unwrap();
        let covered = covered_cells_from_corpus(&corpus);

        assert!(covered.contains(&Cell {
            lane: "anthropic-api".to_string(),
            pattern: "baseline".to_string(),
        }));
        assert!(!covered.iter().any(|c| c.lane == "gemini"));
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .expect("git invocation failed");
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            root.display()
        );
    }

    /// Plant a driver case whose `meta.provider_kind` actually matches the
    /// requested lane token, the way a real capture rig would. Plain
    /// [`plant_driver_case`] only stamps `meta.lane`, leaving
    /// `provider_kind` at its `"anthropic"` default -- fine for lane
    /// `anthropic-api`, wrong for any other lane, since
    /// [`egress_lane_from_fixture_kind`] (what this view actually reads)
    /// resolves through `provider_kind`, not `lane`.
    fn plant_lane_case(root: &Path, lane_token: &str, case_id: &str) -> PathBuf {
        let dir = plant_driver_case(root, lane_token, case_id);
        let provider_kind = match lane_token {
            "anthropic-api" => "anthropic",
            other => other,
        };
        let meta_path = dir.join("meta.json");
        let mut meta: Value =
            serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta["provider_kind"] = Value::String(provider_kind.to_string());
        fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
        dir
    }

    fn init_git_repo_with_driver_corpus(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "test"]);
        let driver = root.join(DRIVER_ROOT_REL);
        plant_lane_case(&driver, "anthropic-api", "case-a");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "baseline corpus"]);
    }

    /// Regression IS a set difference across two real git revisions: a
    /// cell present at the baseline commit and absent at HEAD is
    /// flagged; a cell whose case directory was renamed but keeps its
    /// lane and wire pattern is NOT (both directions of D9's rename
    /// rule, pinned in one test).
    /// SAFETY (test-only): both tests below set/unset [`BASELINE_REV_ENV`]
    /// for the duration of a single `regression_line` call and restore it
    /// immediately after. `#[serial]` (see each test) keeps them from
    /// interleaving with each other across threads; no other test in
    /// this binary touches the same var.
    fn with_baseline_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let previous = std::env::var(BASELINE_REV_ENV).ok();
        match value {
            Some(v) => unsafe { std::env::set_var(BASELINE_REV_ENV, v) },
            None => unsafe { std::env::remove_var(BASELINE_REV_ENV) },
        }
        let result = f();
        match previous {
            Some(v) => unsafe { std::env::set_var(BASELINE_REV_ENV, v) },
            None => unsafe { std::env::remove_var(BASELINE_REV_ENV) },
        }
        result
    }

    #[test]
    #[serial_test::serial(coverage_baseline_env)]
    fn regression_check_flags_lost_cells_but_not_a_rename() {
        let repo = tempdir().unwrap();
        init_git_repo_with_driver_corpus(repo.path());

        let driver = repo.path().join(DRIVER_ROOT_REL);
        // Rename the case directory but keep the same (lane, pattern).
        fs::rename(
            driver.join("anthropic-api").join("case-a"),
            driver.join("anthropic-api").join("case-a-renamed"),
        )
        .unwrap();
        // Add a lane, commit, and take THAT commit as the baseline "prior
        // run" -- the one that actually had openai-compat covered, so its
        // later removal below has something real to lose.
        plant_lane_case(&driver, "openai-compat", "case-b");
        git(repo.path(), &["add", "-A"]);
        git(
            repo.path(),
            &["commit", "-q", "-m", "rename plus a new lane"],
        );
        let base_rev = String::from_utf8(
            Command::new("git")
                .args([
                    "-C",
                    &repo.path().display().to_string(),
                    "rev-parse",
                    "HEAD",
                ])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::remove_dir_all(driver.join("openai-compat")).unwrap();
        git(repo.path(), &["add", "-A"]);
        git(
            repo.path(),
            &["commit", "-q", "-m", "drop the new lane again"],
        );

        let corpus = discover_driver_fixtures(&driver).unwrap();
        let current = covered_cells_from_corpus(&corpus);
        let line = with_baseline_env(Some(&base_rev), || regression_line(repo.path(), &current));

        assert!(
            line.contains("openai-compat"),
            "dropping openai-compat must be flagged: {line}"
        );
        assert!(
            !line.contains("case-a") && !line.contains("case-a-renamed"),
            "a rename that keeps the same (lane, pattern) must not be \
             flagged as lost: {line}"
        );
    }

    #[test]
    #[serial_test::serial(coverage_baseline_env)]
    fn regression_check_reports_not_evaluated_without_a_baseline() {
        let root = repo_root();
        let line = with_baseline_env(None, || regression_line(&root, &BTreeSet::new()));

        assert_eq!(line, "REGRESSION: not evaluated (no base revision)");
    }

    #[test]
    fn unshippable_reason_cites_the_backlog_filing_for_credentialed_lanes() {
        let configured: BTreeSet<String> = ["anthropic-api", "openai-compat", "openai-responses"]
            .into_iter()
            .map(String::from)
            .collect();

        assert_eq!(
            unshippable_reason("bedrock-invoke", &configured),
            Some(CITE_BEDROCK_CREDENTIALS)
        );
        assert_eq!(
            unshippable_reason("bedrock-converse", &configured),
            Some(CITE_BEDROCK_CREDENTIALS)
        );
        assert_eq!(
            unshippable_reason("gemini", &configured),
            Some(CITE_GEMINI_NO_KEY)
        );
        assert_eq!(unshippable_reason("anthropic-api", &configured), None);
    }

    #[test]
    fn no_citation_the_view_can_emit_names_the_drained_codex_filing() {
        // Sweeps every citation BOTH reason functions can produce, over a grid
        // holding each lane token paired with each pattern that has a branch.
        // Asserting the drained id against one function's return is vacuous --
        // that function has no branch able to produce it -- so the guard has to
        // cover the whole emitted set, where a future citation table entry
        // would actually land.
        let configured: BTreeSet<String> = ["anthropic-api", "openai-compat", "openai-responses"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let lanes = [
            "anthropic-api",
            "openai-compat",
            "openai-responses",
            "bedrock-invoke",
            "bedrock-converse",
            "gemini",
        ];
        let patterns = ["baseline", "large-context", "mcp-tools"];

        let mut emitted: Vec<&str> = Vec::new();
        for lane in lanes {
            if let Some(reason) = unshippable_reason(lane, &configured) {
                emitted.push(reason);
            }
            for pattern in patterns {
                let cell = Cell {
                    lane: lane.to_string(),
                    pattern: pattern.to_string(),
                };
                if let Some(reason) = uncovered_shippable_reason(&cell) {
                    emitted.push(reason);
                }
            }
        }

        // Positive control: the sweep really does collect citations, so the
        // absence below is evidence rather than an empty pass.
        assert!(
            emitted.contains(&CITE_BEDROCK_CREDENTIALS),
            "the sweep collected no bedrock citation, so it proves nothing"
        );
        assert!(
            emitted.contains(&CITE_LARGE_CONTEXT_OFF_ANTHROPIC),
            "the sweep collected no large-context citation, so it proves nothing"
        );

        for reason in emitted {
            assert_ne!(
                reason, "bl-codex-driver-lane",
                "this feature drains that filing; an uncovered codex cell is \
                 the capture task's gap, not a filing's"
            );
        }
    }
}
