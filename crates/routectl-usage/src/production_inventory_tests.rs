// The declared inventory of this crate's production modules.
//
// The completeness check compares the filesystem against the declared list, so
// adding a module without listing it is RED. A check that derived both sides
// from the filesystem would be a tautology; a declared inventory forces a human
// decision exactly once per new module.

use std::collections::BTreeSet;
use std::path::Path;

/// Every production file in `routectl-usage/src`, DECLARED.
const USAGE_PRODUCTION_FILES: &[&str] = &[
    "capability_ack.rs",
    "capability_batch.rs",
    "capability_event.rs",
    "cost.rs",
    "db.rs",
    "downgrade.rs",
    "handle.rs",
    "lib.rs",
    "migrate.rs",
    "query/aggregate.rs",
    "query/cache_decision.rs",
    "query/calibration.rs",
    "query/capability.rs",
    "query/deadline.rs",
    "query/grouped.rs",
    "query/mod.rs",
    "query/reduction.rs",
    "query/session_ref.rs",
    "query/stream_extra.rs",
    "query/would_trim.rs",
    "record.rs",
    "retention.rs",
    "schema.rs",
    "writer.rs",
];

/// Whether `path` is a production Rust source: a `.rs` file that is not a test
/// sidecar or test-support module, per this repo's file-suffix convention.
fn is_production_source(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs")
        && !name.ends_with("_tests.rs")
        && name != "tests.rs"
        && !name.contains("test_support")
}

/// Every production source under `dir`, as a path relative to `dir`.
fn production_sources(dir: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
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
                out.insert(format!("{prefix}{name}"));
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, "", &mut out);
    out
}

/// How the filesystem and the declared inventory disagree.
#[derive(Debug, Default, PartialEq, Eq)]
struct InventoryDrift {
    /// On disk but not declared: a module nobody classified.
    undeclared: Vec<String>,
    /// Declared but not on disk: a stale entry that would mask the first.
    missing: Vec<String>,
}

fn inventory_drift(discovered: &BTreeSet<String>, declared: &[&str]) -> InventoryDrift {
    let declared: BTreeSet<String> = declared.iter().map(ToString::to_string).collect();
    InventoryDrift {
        undeclared: discovered.difference(&declared).cloned().collect(),
        missing: declared.difference(discovered).cloned().collect(),
    }
}

/// Lay out the declared inventory as empty files under `root`.
fn materialize_declared_tree(root: &Path) {
    for file in USAGE_PRODUCTION_FILES {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().expect("a declared file has a parent"))
            .expect("create fixture directory");
        std::fs::write(&path, "").expect("write fixture file");
    }
}

#[test]
fn every_production_module_is_declared() {
    // Arrange
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    // Act
    let discovered = production_sources(&src);
    let drift = inventory_drift(&discovered, USAGE_PRODUCTION_FILES);

    // Assert: the walk found this crate rather than an empty or wrong directory,
    // and no module escapes classification in either direction.
    assert!(
        discovered.len() > 15,
        "the walk found only {} production sources -- wrong directory?",
        discovered.len(),
    );
    assert_eq!(
        drift,
        InventoryDrift::default(),
        "undeclared modules must be added to the inventory; missing ones removed from it",
    );
}

#[test]
fn a_planted_undeclared_module_is_reported() {
    // Arrange: the declared tree, plus one new production module and one test
    // sidecar beside it.
    let root = tempfile::TempDir::new().expect("tempdir");
    materialize_declared_tree(root.path());
    std::fs::write(root.path().join("query/planted.rs"), "").expect("plant module");
    std::fs::write(root.path().join("planted_tests.rs"), "").expect("plant sidecar");

    // Act
    let drift = inventory_drift(&production_sources(root.path()), USAGE_PRODUCTION_FILES);

    // Assert: the module is caught and the sidecar is not.
    assert_eq!(
        drift,
        InventoryDrift {
            undeclared: vec!["query/planted.rs".to_string()],
            missing: Vec::new(),
        },
    );
}

#[test]
fn a_declared_module_missing_from_disk_is_reported() {
    // Arrange
    let root = tempfile::TempDir::new().expect("tempdir");
    materialize_declared_tree(root.path());
    std::fs::remove_file(root.path().join("cost.rs")).expect("remove declared file");

    // Act
    let drift = inventory_drift(&production_sources(root.path()), USAGE_PRODUCTION_FILES);

    // Assert
    assert_eq!(
        drift,
        InventoryDrift {
            undeclared: Vec::new(),
            missing: vec!["cost.rs".to_string()],
        },
    );
}
