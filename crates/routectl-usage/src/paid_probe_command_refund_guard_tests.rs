// The no-refund invariant, enforced across every first-party production source
// that can reach the control table.
//
// The reservation's own absence guard covers the module that implements it. This
// one covers everything else: a future module that learns to write the `meta`
// table could give a committed unit back without ever touching
// `paid_probe.rs`, and that is exactly the change no reviewer would flag as a
// refund. So the rule is enforced where a violation would actually be written.
//
// TWO CRATES, for one reason: `routectl-usage` owns the table, and
// `routectl-cli` holds a PUBLISHED WRITABLE connection to it (`open_rw`), so it
// is the other first-party place a refund could be written. Arbitrary external
// consumers of that published connection are beyond mechanical reach from here
// -- nothing in this repo can scan a crate it does not contain. The doc on
// `UsageDb` / `open_rw` is what carries the rule to them; this guard is the
// backstop for the code we do own.

/// Every production file in `routectl-usage/src`, DECLARED.
///
/// The completeness check compares the filesystem against this list, so adding a
/// module without classifying it is RED. That is the whole point of declaring
/// it: a check that derived both sides from the filesystem would be a tautology
/// (it was, in an earlier draft, and a planted new module passed it), while a
/// declared inventory forces a human decision exactly once per new module.
///
/// `paid_probe.rs` appears here AND in the exempt list below: it is production
/// code that the undo-scan deliberately skips.
const USAGE_PRODUCTION_FILES: &[&str] = &[
    "capability_ack.rs",
    "capability_batch.rs",
    "capability_event.rs",
    "cost.rs",
    "db.rs",
    "handle.rs",
    "learn_event.rs",
    "lib.rs",
    "migrate.rs",
    "paid_probe.rs",
    "paid_probe_command.rs",
    "paid_probe_lifecycle.rs",
    "paid_probe_read.rs",
    "query/aggregate.rs",
    "query/cache_decision.rs",
    "query/calibration.rs",
    "query/capability.rs",
    "query/deadline.rs",
    "query/grouped.rs",
    "query/mod.rs",
    "query/reduction.rs",
    "query/session_ref.rs",
    "query/would_trim.rs",
    "record.rs",
    "retention.rs",
    "schema.rs",
    "writer.rs",
];

/// Files in `routectl-usage/src` deliberately EXEMPT from the undo scan.
///
/// `paid_probe.rs` is the reservation implementation: it is the one place
/// allowed to name the durable key and write the count, and its own absence
/// guard covers it. Every other production file must be scanned, which the
/// completeness assertion below enforces -- a new module is RED until it is
/// either scanned or listed here with a reason.
const USAGE_EXEMPT_FILES: &[&str] = &["paid_probe.rs"];

/// The absolute path to a first-party crate's `src` directory.
///
/// Derived from this crate's manifest rather than from the current directory,
/// which is not the crate root under every runner.
fn crate_src(crate_name: &str) -> std::path::PathBuf {
    let usage_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    if crate_name == "routectl-usage" {
        return usage_src;
    }
    usage_src
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the crates directory is two levels above this crate's src")
        .join(crate_name)
        .join("src")
}

/// Whether `path` is a production Rust source: a `.rs` file that is not a test
/// sidecar or test-support module.
///
/// The `_tests.rs` / `_test_support.rs` suffixes are this repo's convention, and
/// the convention is load-bearing here: a test that plants a forbidden token in a
/// FIXTURE is not a refund path, so scanning test files would fail on correct
/// code and teach the next reader to delete the guard.
fn is_production_source(path: &std::path::Path) -> bool {
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs")
        && !name.ends_with("_tests.rs")
        && !name.ends_with("_test_support.rs")
        && name != "tests.rs"
        && !name.contains("test_support")
}

/// Source files that are RELEASE-ABSENT: declared behind `cfg(test)` or the
/// non-default `test-utils` feature, so no release build compiles them.
///
/// Distinguished from the exempt list below, and the difference is the whole
/// point. An EXEMPT file ships and is trusted for a stated reason. A file here
/// does not ship at all, so a refund path in it cannot exist in any artifact a
/// deployment runs -- the classification is enforced by the build, not by this
/// guard's trust.
///
/// The classification is VERIFIED rather than declared: `assert_release_absent`
/// below reads each file's declaration site in `lib.rs` and fails if the gate is
/// missing. So removing the `cfg` to ship one of these turns it into an
/// UNDECLARED production module (red on the completeness check) AND fails the
/// gate assertion -- it cannot quietly become a shipped refund path.
const USAGE_RELEASE_ABSENT_FILES: &[&str] = &["paid_probe_test_support.rs"];

/// Assert every release-absent file really is gated at its declaration.
///
/// Reads `lib.rs` rather than the file itself: a `cfg` INSIDE a module does not
/// stop the module from being compiled, and the gate that matters is the one on
/// the `mod` item. Comment-stripped, so prose about the gate cannot satisfy it.
fn assert_release_absent(src: &std::path::Path) {
    let lib = std::fs::read_to_string(src.join("lib.rs")).expect("lib.rs must read");
    let code: String = lib
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for file in USAGE_RELEASE_ABSENT_FILES {
        let module = file.trim_end_matches(".rs");
        let decl = format!("mod {module};");
        let at = code.find(&decl).unwrap_or_else(|| {
            panic!("{file} is classified release-absent but `{decl}` is not in lib.rs")
        });
        let gate = r#"#[cfg(any(test, feature = "test-utils"))]"#;
        let above = &code[..at];
        assert!(
            above
                .rfind(gate)
                .is_some_and(|gate_at| !above[gate_at..].contains("\n\n")),
            "{file} is classified release-absent, so its `mod` declaration in lib.rs must \
             carry `{gate}` immediately above it -- without that gate the file SHIPS, and \
             it holds a path that can lower a committed paid-probe count",
        );
    }
}

/// Every production source under `dir`, as `(path relative to dir, text)`.
///
/// Walked from the FILESYSTEM, so a module cannot be omitted from the scan by
/// forgetting to list it -- which is the completeness property an
/// `include_str!` list could not give: that list stays green while the tree
/// grows around it.
fn production_sources(dir: &std::path::Path) -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, String)>) {
        let entries = std::fs::read_dir(dir).expect("source directory must read");
        for entry in entries {
            let path = entry.expect("directory entry must read").path();
            let name = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_string();
            if path.is_dir() {
                walk(&path, &format!("{prefix}{name}/"), out);
            } else if is_production_source(&path) {
                let text = std::fs::read_to_string(&path).expect("source file must read");
                out.push((format!("{prefix}{name}"), text));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out.sort();
    out
}

/// The region of `source` to scan: everything above its inline `mod tests {`
/// opener, comments stripped.
///
/// Comments go because the rule is about CODE: the forbidden tokens are
/// legitimately discussed in prose (this fragment names them), and a scan that
/// read prose would fail on correct code.
///
/// AMBIGUITY SCANS WHOLE, deliberately. With several openers the cut cannot be
/// placed correctly, and for an ABSENCE rule the two errors are not symmetric:
/// scanning too little reports CLEAN over code nobody checked, while scanning too
/// much can only over-report. So the fail-closed reading is to scan the entire
/// file. (One CLI file has ten openers because its whole subject is the needle --
/// it is a helper for cutting at `mod tests {` and carries the literal in its own
/// fixtures.) Measured before relying on it: zero forbidden tokens occur anywhere
/// under either crate's production sources, inline test modules included, so this
/// costs nothing today and a future fixture that tripped it would get a message
/// telling it to move to a test sidecar.
fn scanned_region(source: &str) -> String {
    let production = if source.matches("mod tests {").count() == 1 {
        source
            .split_once("mod tests {")
            .map_or(source, |(above, _)| above)
    } else {
        source
    };
    production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tokens that would let a module undo a committed unit.
///
/// `DELETE FROM meta` covers removing the row. `paid_probe_reservation:` -- WITH
/// the separator, which is the durable key's own shape -- covers reaching the row
/// at all, the prerequisite for rewriting its value. The separator is what makes
/// the token a KEY rather than an identifier: the admission function is
/// legitimately called `admit_paid_probe_reservation`, and a bare-identifier ban
/// fired on it, which is how a correct-code failure teaches the next reader to
/// delete the guard.
///
/// Deliberately NOT a general `UPDATE meta` ban: the migration ladder legitimately
/// upserts its own schema-version rows, and a rule that fires on correct code gets
/// loosened until it means nothing.
const FORBIDDEN_UNDO_TOKENS: &[&str] = &[
    "DELETE FROM meta",
    "delete from meta",
    "paid_probe_reservation:",
];

/// Assert no production source in `sources` can undo a committed unit.
fn assert_no_undo_path(crate_name: &str, sources: &[(String, String)]) {
    for (path, text) in sources {
        let region = scanned_region(text);
        for forbidden in FORBIDDEN_UNDO_TOKENS {
            assert!(
                !region.contains(forbidden),
                "{crate_name}/src/{path} names `{forbidden}`: a committed paid-probe unit is \
                 never given back, so only the reservation implementation may name its key or \
                 remove its row",
            );
        }
    }
}

/// The scan's corpus is COMPLETE: the filesystem and the declared inventory
/// agree exactly.
///
/// This is what an `include_str!` list could not do, and what a
/// filesystem-only check could not do either. A static list reports CLEAN
/// forever while modules are added around it; a check that derives both sides
/// from the filesystem is a tautology. Comparing the two makes a new module RED
/// until somebody classifies it.
#[test]
fn the_scanned_corpus_covers_every_production_module() {
    // Arrange
    let src = crate_src("routectl-usage");
    let discovered: std::collections::BTreeSet<String> = production_sources(&src)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let declared: std::collections::BTreeSet<String> = USAGE_PRODUCTION_FILES
        .iter()
        .map(std::string::ToString::to_string)
        .collect();

    // Assert the premise: the walk really found this crate, rather than an empty
    // or wrong directory.
    assert!(
        discovered.len() > 15,
        "the walk found only {} production sources -- wrong directory?",
        discovered.len(),
    );

    // Assert: no module escapes classification, in either direction. A file on
    // disk but not declared is the dangerous case (unscanned code); a declared
    // file no longer on disk is a stale entry that would mask the first.
    let undeclared: Vec<&String> = discovered.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "these production modules are not classified, so the no-refund scan does not \
         provably cover them -- add each to the declared inventory (and to the exempt list \
         only with a stated reason): {undeclared:?}",
    );
    let missing: Vec<&String> = declared.difference(&discovered).collect();
    assert!(
        missing.is_empty(),
        "these declared modules no longer exist -- remove them, or a stale entry hides a \
         real gap: {missing:?}",
    );

    // Assert: every exemption names a file that EXISTS and is itself declared,
    // so a stale exemption cannot quietly widen the hole after a rename.
    for file in USAGE_EXEMPT_FILES {
        assert!(
            discovered.contains(*file),
            "exempt file {file} no longer exists -- remove or update the exemption",
        );
    }

    // And every RELEASE-ABSENT classification is verified against the build, not
    // trusted: the file must exist on disk (a stale entry would mask a real gap
    // after a rename, exactly as for an exemption) and its `mod` declaration must
    // carry the feature gate. A file whose gate is removed SHIPS, and this one
    // holds a path that can lower a committed spend count.
    for file in USAGE_RELEASE_ABSENT_FILES {
        let path = src.join(file);
        assert!(
            path.exists(),
            "release-absent file {file} no longer exists -- remove or update the entry",
        );
        assert!(
            !discovered.contains(*file),
            "{file} is classified release-absent yet the production walk FOUND it -- the \
             two classifications are disjoint by construction, so this means the \
             suffix-based production filter changed",
        );
    }
    assert_release_absent(&src);
}

/// No production module in the owning crate, outside the reservation
/// implementation, can undo a committed unit.
#[test]
fn no_usage_module_outside_the_reservation_can_undo_a_committed_unit() {
    // Arrange
    let src = crate_src("routectl-usage");
    let sources: Vec<(String, String)> = production_sources(&src)
        .into_iter()
        .filter(|(path, _)| !USAGE_EXEMPT_FILES.contains(&path.as_str()))
        .collect();

    // Assert the premise: the corpus is real, and it PROVABLY contains the SQL
    // shape being forbidden -- a zero match is then an absence, not a scan that
    // cannot see control-table code.
    assert!(!sources.is_empty(), "the scanned corpus must not be empty");
    assert!(
        sources
            .iter()
            .any(|(_, text)| scanned_region(text).contains("INSERT INTO meta")),
        "the corpus must include real control-table SQL, or the scan cannot see this class",
    );

    // Assert
    assert_no_undo_path("routectl-usage", &sources);
}

/// The same rule over the CLI crate, which holds a published writable connection
/// to the same database and is therefore the other first-party place a refund
/// could be written.
///
/// Enforcement stops at the first-party boundary: an arbitrary external consumer
/// of `open_rw` cannot be scanned from inside this repo, which is why the
/// connection's own docs carry the rule.
#[test]
fn no_cli_module_can_undo_a_committed_unit() {
    // Arrange
    let src = crate_src("routectl-cli");
    let sources = production_sources(&src);

    // Assert the premise: the walk found the CLI crate and it really is a
    // consumer of the usage database, so this scan is not vacuous.
    assert!(
        sources.len() > 50,
        "the walk found only {} CLI production sources -- wrong directory?",
        sources.len(),
    );
    assert!(
        sources
            .iter()
            .any(|(_, text)| text.contains("routectl_usage")),
        "the CLI corpus must show it consumes the usage crate",
    );

    // Assert
    assert_no_undo_path("routectl-cli", &sources);
}

/// The guard's own reach, proven rather than assumed: the reservation module IS
/// excluded, and it is excluded because it legitimately names these tokens.
///
/// Without this, an exclusion list that had grown to cover the whole crate would
/// still report CLEAN.
#[test]
fn the_reservation_implementation_is_the_only_excluded_module() {
    // Arrange
    let excluded = include_str!("paid_probe.rs");

    // Assert: the excluded module really does carry what the others may not, so
    // the exclusion is load-bearing and not a leftover.
    assert!(
        excluded.contains("paid_probe_reservation:"),
        "the excluded module must be the one that legitimately names the key",
    );
    assert_eq!(
        USAGE_EXEMPT_FILES.len(),
        1,
        "exactly one file may be exempt; a second needs its own stated reason",
    );
}
