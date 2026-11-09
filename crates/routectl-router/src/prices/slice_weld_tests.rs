//! The models.dev test slice is a verbatim copy of providers in the vendored
//! snapshot: same provider set, same bytes once parsed.

use serde_json::Value;

const SLICE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/prices/testdata/models_dev_slice.json"
);
const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/catalog_data/models_dev.json");

const SLICE_PROVIDERS: [&str; 7] = [
    "anthropic",
    "lmstudio",
    "minimax",
    "minimax-coding-plan",
    "ollama-cloud",
    "opencode",
    "opencode-go",
];

fn read_json(path: &str) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn the_slice_is_exactly_seven_snapshot_providers_copied_verbatim() {
    let slice = read_json(SLICE);
    let snapshot = read_json(SNAPSHOT);
    let slice = slice.as_object().expect("slice is an object");
    let snapshot = snapshot.as_object().expect("snapshot is an object");

    let keys: Vec<&str> = slice.keys().map(String::as_str).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, SLICE_PROVIDERS);
    for provider in SLICE_PROVIDERS {
        let in_snapshot = snapshot
            .get(provider)
            .unwrap_or_else(|| panic!("{provider} missing from the snapshot"));
        assert!(
            slice[provider] == *in_snapshot,
            "{provider} differs from the snapshot"
        );
    }
}
