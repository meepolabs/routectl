//! Planting helpers for SYNTHETIC fixture corpora in a tempdir.
//!
//! Every consumer of the loader that needs a corpus to walk builds one
//! through here, so "what a minimally valid fixture on disk looks like"
//! has ONE owner. A second writer of the same five files drifts from the
//! loader's required-file list the moment that list moves, and the drift
//! shows up as a test that plants something no capture could produce.
//!
//! Nothing here ever touches a real fixture root: a planted corpus is a
//! caller-owned tempdir, because the live-box corpus is per-contributor
//! and unrecapturable.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::loader::{
    FIXTURE_SCHEMA_VERSION, INGRESS_BODY, INGRESS_HEADERS, META_JSON, OUTGOING_BODY,
    OUTGOING_HEADERS,
};

/// The full current-schema `meta.json` a rig-written fixture carries.
/// Callers mutate the returned value to plant a variant.
pub fn current_meta() -> Value {
    json!({
        "schema_version": FIXTURE_SCHEMA_VERSION,
        "provider_kind": "anthropic",
        "lane": "anthropic-api",
        "ingress_kind": "anthropic",
        "case_id": "smoke",
        "config_sha": "abc123",
        "wire_pattern": "baseline",
        "client": {
            "name": "claude-code",
            "version": "2.1.167",
            "binary_version": "2.1.167 (Claude Code)",
            "connection_mode": "base-url",
        },
        "stream": false,
        "model": "claude-sonnet-4-5",
        "routectl_version": "0.8.0",
    })
}

/// Write the four ALWAYS-required files plus the given `meta.json`.
/// Optional response files are the caller's business.
pub fn write_required_files(dir: &Path, meta: &Value) {
    fs::write(
        dir.join(META_JSON),
        serde_json::to_vec_pretty(meta).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join(INGRESS_BODY),
        serde_json::to_vec(&json!({"model": "x"})).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join(INGRESS_HEADERS),
        serde_json::to_vec(&json!([["content-type", "application/json"]])).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join(OUTGOING_BODY),
        serde_json::to_vec(&json!({"model": "y"})).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join(OUTGOING_HEADERS),
        serde_json::to_vec(&json!([["content-type", "application/json"]])).unwrap(),
    )
    .unwrap();
}

/// Plant a loadable fixture directory at `dir`, creating parents.
pub fn plant_fixture(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    write_required_files(dir, &current_meta());
}

/// Plant a loadable driver-corpus case at `<root>/<lane>/<case_id>` and
/// return its path. `meta.lane` follows the planted lane component so
/// the fixture is self-consistent the way a rig-written one is.
pub fn plant_driver_case(root: &Path, lane: &str, case_id: &str) -> PathBuf {
    let dir = root.join(lane).join(case_id);
    fs::create_dir_all(&dir).unwrap();
    let mut meta = current_meta();
    meta["lane"] = json!(lane);
    meta["case_id"] = json!(case_id);
    write_required_files(&dir, &meta);
    dir
}

/// Plant a driver-corpus case that is PRESENT but unloadable: every
/// required file except `meta.json`, which is what the loader refuses on
/// first. The distinction this exists to draw is present-but-broken vs
/// absent -- an entry that walks and fails is not an empty corpus.
pub fn plant_unloadable_driver_case(root: &Path, lane: &str, case_id: &str) -> PathBuf {
    let dir = plant_driver_case(root, lane, case_id);
    fs::remove_file(dir.join(META_JSON)).unwrap();
    dir
}

/// Overwrite a planted fixture's outgoing body with its ingress body, so
/// conservation adjudicates it as conserved rather than divergent.
pub fn make_conserved(dir: &Path) {
    let ingress = fs::read(dir.join(INGRESS_BODY)).unwrap();
    fs::write(dir.join(OUTGOING_BODY), ingress).unwrap();
}
