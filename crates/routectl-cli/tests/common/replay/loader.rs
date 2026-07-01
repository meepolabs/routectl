//! Fixture file format: `meta.json`, ingress / outgoing request
//! bodies + headers (always required), and optional upstream / egress
//! response bodies + headers gated by `meta.has_*_response`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Error returned by the fixture loader. Carries the file path that
/// failed so callers can name it in test output.
#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("json parse error in {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("missing required file: {0}")]
    MissingFile(String),
    #[error("invalid header file format in {path}: expected array of [name, value] pairs")]
    InvalidHeaderFormat { path: String },
    #[error("unexpected file present (meta declared it absent): {path}")]
    UnexpectedFilePresent { path: String },
}

/// Mirror of the `meta.json` schema documented in
/// `docs/REPLAY-FIXTURES.md`. `model` is the post-alias provider model
/// id from the trace; the test drivers use it to skip fixtures whose
/// model needs router-side enrichment that the bare ingress -> egress
/// path does not yet replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureMeta {
    pub provider_kind: String,
    pub stream: bool,
    pub has_upstream_response: bool,
    pub has_egress_response: bool,
    #[serde(default)]
    pub expected_unknown_block_count: Option<u32>,
    /// Post-alias provider model id; populated by the capture rig.
    /// Optional so older fixtures (and the loader's unit tests) load
    /// without a forced rewrite.
    #[serde(default)]
    pub model: Option<String>,
    /// Workspace package version stamped by `capture_fixtures.sh`.
    /// Optional so older captures (and the loader's unit tests) load
    /// without a forced rewrite.
    #[serde(default)]
    pub routectl_version: Option<String>,
}

/// One loaded fixture. Body files are parsed as JSON for the request
/// halves and kept as raw bytes for the response halves so SSE streams
/// survive the round-trip.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub ingress_request: Value,
    pub ingress_request_headers: Vec<(String, String)>,
    pub outgoing_request: Value,
    pub outgoing_request_headers: Vec<(String, String)>,
    /// Empty when `meta.has_upstream_response` is false.
    pub upstream_response_bytes: Vec<u8>,
    /// Empty when `meta.has_upstream_response` is false.
    pub upstream_response_headers: Vec<(String, String)>,
    /// Empty when `meta.has_egress_response` is false.
    pub egress_response_bytes: Vec<u8>,
    /// Empty when `meta.has_egress_response` is false.
    pub egress_response_headers: Vec<(String, String)>,
    pub meta: FixtureMeta,
}

/// File names expected inside a fixture directory.
pub const META_JSON: &str = "meta.json";
pub const INGRESS_BODY: &str = "ingress_request.json";
pub const INGRESS_HEADERS: &str = "ingress_request.headers.json";
pub const OUTGOING_BODY: &str = "outgoing_request.json";
pub const OUTGOING_HEADERS: &str = "outgoing_request.headers.json";
pub const UPSTREAM_BODY: &str = "upstream_response.json";
pub const UPSTREAM_HEADERS: &str = "upstream_response.headers.json";
pub const EGRESS_BODY: &str = "egress_response.json";
pub const EGRESS_HEADERS: &str = "egress_response.headers.json";

/// Load one fixture directory. The directory's last path component
/// becomes `Fixture.name`. Validation rules are documented in
/// `docs/REPLAY-FIXTURES.md`.
pub fn load_fixture(dir: &Path) -> Result<Fixture, ReplayError> {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = read_meta(dir)?;

    let ingress_request = read_required_json(dir, INGRESS_BODY)?;
    let ingress_request_headers = read_required_headers(dir, INGRESS_HEADERS)?;
    let outgoing_request = read_required_json(dir, OUTGOING_BODY)?;
    let outgoing_request_headers = read_required_headers(dir, OUTGOING_HEADERS)?;
    let (upstream_response_bytes, upstream_response_headers) = read_optional_response(
        dir,
        meta.has_upstream_response,
        UPSTREAM_BODY,
        UPSTREAM_HEADERS,
    )?;
    let (egress_response_bytes, egress_response_headers) =
        read_optional_response(dir, meta.has_egress_response, EGRESS_BODY, EGRESS_HEADERS)?;

    Ok(Fixture {
        name,
        ingress_request,
        ingress_request_headers,
        outgoing_request,
        outgoing_request_headers,
        upstream_response_bytes,
        upstream_response_headers,
        egress_response_bytes,
        egress_response_headers,
        meta,
    })
}

/// Result of walking a fixture corpus: the fixtures that loaded cleanly
/// plus the count that were skipped (parse error, missing required file,
/// stray optional file). A degraded corpus -- e.g. one truncated by a
/// log line-length limit at capture time -- thins `fixtures` while
/// `skipped` rises, so the run's coverage is visible rather than silent.
#[derive(Debug, Default)]
pub struct LoadedCorpus {
    pub fixtures: Vec<Fixture>,
    pub skipped: usize,
}

/// Walk `canon_root` for fixture subdirectories, sorting by directory
/// name for deterministic test ordering. Skips dotfiles and any
/// non-directory entry (e.g. `README.md`, `.gitkeep`). A subdirectory
/// that fails to load (malformed `meta.json` or body JSON, missing
/// required file, stray optional file) is logged to stderr (naming the
/// directory and the error), counted in `LoadedCorpus.skipped`, and
/// skipped rather than aborting the whole corpus, so one bad fixture
/// cannot blind the run to every other fixture's regression signal. A
/// final summary line reports loaded vs skipped so a thin corpus is
/// visible. Filesystem-level errors reading `canon_root` itself still
/// propagate as `Err`.
pub fn discover_fixtures(canon_root: &Path) -> Result<LoadedCorpus, ReplayError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let read = fs::read_dir(canon_root).map_err(|e| ReplayError::Io {
        path: canon_root.display().to_string(),
        source: e,
    })?;
    for entry in read {
        let entry = entry.map_err(|e| ReplayError::Io {
            path: canon_root.display().to_string(),
            source: e,
        })?;
        let path = entry.path();
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if file_name.starts_with('.') {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        dirs.push(path);
    }
    dirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    let mut fixtures = Vec::with_capacity(dirs.len());
    let mut skipped = 0usize;
    for dir in dirs {
        match load_fixture(&dir) {
            Ok(f) => fixtures.push(f),
            Err(e) => {
                skipped += 1;
                eprintln!(
                    "[replay] skipping unloadable fixture `{}`: {e}",
                    dir.display(),
                );
            }
        }
    }
    eprintln!(
        "[replay] corpus: loaded {}, skipped {} (parse errors / malformed fixtures)",
        fixtures.len(),
        skipped,
    );
    Ok(LoadedCorpus { fixtures, skipped })
}

fn read_meta(dir: &Path) -> Result<FixtureMeta, ReplayError> {
    let path = dir.join(META_JSON);
    let bytes = read_file_or_missing(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| ReplayError::Json {
        path: path.display().to_string(),
        source: e,
    })
}

fn read_required_json(dir: &Path, name: &str) -> Result<Value, ReplayError> {
    let path = dir.join(name);
    let bytes = read_file_or_missing(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| ReplayError::Json {
        path: path.display().to_string(),
        source: e,
    })
}

fn read_required_headers(dir: &Path, name: &str) -> Result<Vec<(String, String)>, ReplayError> {
    let path = dir.join(name);
    let value = read_required_json(dir, name)?;
    parse_headers_value(&value, &path)
}

/// Read body + headers for an optional response slot. When the meta
/// flag is `false`, both files MUST be absent and the result is empty
/// vectors. When `true`, both files MUST be present.
#[allow(clippy::type_complexity)]
fn read_optional_response(
    dir: &Path,
    expected: bool,
    body_name: &str,
    headers_name: &str,
) -> Result<(Vec<u8>, Vec<(String, String)>), ReplayError> {
    if !expected {
        let body_path = dir.join(body_name);
        if body_path.exists() {
            return Err(ReplayError::UnexpectedFilePresent {
                path: body_path.display().to_string(),
            });
        }
        let headers_path = dir.join(headers_name);
        if headers_path.exists() {
            return Err(ReplayError::UnexpectedFilePresent {
                path: headers_path.display().to_string(),
            });
        }
        return Ok((Vec::new(), Vec::new()));
    }
    let body_path = dir.join(body_name);
    let body = read_file_or_missing(&body_path)?;
    let headers_value = read_required_json(dir, headers_name)?;
    let headers_path = dir.join(headers_name);
    let headers = parse_headers_value(&headers_value, &headers_path)?;
    Ok((body, headers))
}

/// Read a file's bytes; map `NotFound` to `MissingFile` (which carries
/// the path) and any other io error to `Io { path, source }`.
fn read_file_or_missing(path: &Path) -> Result<Vec<u8>, ReplayError> {
    fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ReplayError::MissingFile(path.display().to_string())
        } else {
            ReplayError::Io {
                path: path.display().to_string(),
                source: e,
            }
        }
    })
}

/// Decode a header file. Format: a JSON array of two-element
/// `[name, value]` arrays (mirrors `routectl_core::log_safe::headers_to_json`).
fn parse_headers_value(value: &Value, path: &Path) -> Result<Vec<(String, String)>, ReplayError> {
    let arr = value
        .as_array()
        .ok_or_else(|| ReplayError::InvalidHeaderFormat {
            path: path.display().to_string(),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for pair in arr {
        let pair_arr = pair
            .as_array()
            .ok_or_else(|| ReplayError::InvalidHeaderFormat {
                path: path.display().to_string(),
            })?;
        if pair_arr.len() != 2 {
            return Err(ReplayError::InvalidHeaderFormat {
                path: path.display().to_string(),
            });
        }
        let name = pair_arr[0]
            .as_str()
            .ok_or_else(|| ReplayError::InvalidHeaderFormat {
                path: path.display().to_string(),
            })?;
        let val = pair_arr[1]
            .as_str()
            .ok_or_else(|| ReplayError::InvalidHeaderFormat {
                path: path.display().to_string(),
            })?;
        out.push((name.to_string(), val.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn write_minimal_fixture(dir: &Path, has_upstream: bool, has_egress: bool) {
        let meta = json!({
            "provider_kind": "anthropic",
            "stream": false,
            "has_upstream_response": has_upstream,
            "has_egress_response": has_egress,
        });
        fs::write(
            dir.join(META_JSON),
            serde_json::to_vec_pretty(&meta).unwrap(),
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
        if has_upstream {
            fs::write(dir.join(UPSTREAM_BODY), b"{\"id\":\"u\"}").unwrap();
            fs::write(
                dir.join(UPSTREAM_HEADERS),
                serde_json::to_vec(&json!([["content-type", "application/json"]])).unwrap(),
            )
            .unwrap();
        }
        if has_egress {
            fs::write(dir.join(EGRESS_BODY), b"{\"id\":\"e\"}").unwrap();
            fs::write(
                dir.join(EGRESS_HEADERS),
                serde_json::to_vec(&json!([["content-type", "application/json"]])).unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn loader_round_trips_minimal_fixture() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_minimal_fixture(&dir, true, true);

        let f = load_fixture(&dir).unwrap();
        assert_eq!(f.name, "scenario");
        assert_eq!(f.meta.provider_kind, "anthropic");
        assert!(!f.meta.stream);
        assert_eq!(f.ingress_request, json!({"model": "x"}));
        assert_eq!(f.outgoing_request, json!({"model": "y"}));
        assert_eq!(f.ingress_request_headers.len(), 1);
        assert_eq!(f.upstream_response_bytes, b"{\"id\":\"u\"}");
        assert_eq!(f.upstream_response_headers.len(), 1);
        assert_eq!(f.egress_response_bytes, b"{\"id\":\"e\"}");
        assert_eq!(f.egress_response_headers.len(), 1);
    }

    #[test]
    fn loader_skips_optional_files_when_meta_is_false() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_minimal_fixture(&dir, false, false);

        let f = load_fixture(&dir).unwrap();
        assert!(f.upstream_response_bytes.is_empty());
        assert!(f.upstream_response_headers.is_empty());
        assert!(f.egress_response_bytes.is_empty());
        assert!(f.egress_response_headers.is_empty());
    }

    #[test]
    fn loader_rejects_stray_optional_response_when_meta_is_false() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_minimal_fixture(&dir, false, false);
        // Stray body file left behind by a partial sanitization.
        fs::write(dir.join(UPSTREAM_BODY), b"{\"id\":\"u\"}").unwrap();

        let err = load_fixture(&dir).unwrap_err();
        match &err {
            ReplayError::UnexpectedFilePresent { path } => {
                assert!(
                    path.contains(UPSTREAM_BODY),
                    "error did not name the stray file: {path}"
                );
            }
            other => panic!("expected UnexpectedFilePresent, got {other:?}"),
        }
    }

    #[test]
    fn loader_rejects_stray_optional_headers_when_meta_is_false() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_minimal_fixture(&dir, false, false);
        // Stray headers file left behind by a partial sanitization.
        fs::write(
            dir.join(EGRESS_HEADERS),
            serde_json::to_vec(&json!([["content-type", "application/json"]])).unwrap(),
        )
        .unwrap();

        let err = load_fixture(&dir).unwrap_err();
        match &err {
            ReplayError::UnexpectedFilePresent { path } => {
                assert!(
                    path.contains(EGRESS_HEADERS),
                    "error did not name the stray file: {path}"
                );
            }
            other => panic!("expected UnexpectedFilePresent, got {other:?}"),
        }
    }

    #[test]
    fn loader_errors_when_required_file_missing() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_minimal_fixture(&dir, false, false);
        fs::remove_file(dir.join(OUTGOING_BODY)).unwrap();

        let err = load_fixture(&dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(OUTGOING_BODY),
            "error did not name the missing file: {msg}"
        );
    }

    #[test]
    fn loader_errors_when_optional_response_promised_but_missing() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_minimal_fixture(&dir, true, false);
        fs::remove_file(dir.join(UPSTREAM_BODY)).unwrap();

        let err = load_fixture(&dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(UPSTREAM_BODY),
            "error did not name the missing file: {msg}"
        );
    }

    #[test]
    fn discover_fixtures_sorts_by_directory_name() {
        let tmp = tempdir().unwrap();
        for name in ["zeta_scenario", "alpha_scenario", "mu_scenario"] {
            let dir = tmp.path().join(name);
            fs::create_dir(&dir).unwrap();
            write_minimal_fixture(&dir, false, false);
        }
        // Add a dotfile and a README that must be skipped.
        fs::write(tmp.path().join(".gitkeep"), b"").unwrap();
        fs::write(tmp.path().join("README.md"), b"# notes").unwrap();

        let loaded = discover_fixtures(tmp.path()).unwrap();
        let names: Vec<_> = loaded.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha_scenario", "mu_scenario", "zeta_scenario"]
        );
        assert_eq!(loaded.skipped, 0);
    }

    #[test]
    fn discover_fixtures_skips_unloadable_fixture() {
        let tmp = tempdir().unwrap();
        // Two well-formed fixtures plus one whose required outgoing body
        // is missing -- the bad one must be skipped, not abort the walk.
        for name in ["good_a", "good_b"] {
            let dir = tmp.path().join(name);
            fs::create_dir(&dir).unwrap();
            write_minimal_fixture(&dir, false, false);
        }
        let bad = tmp.path().join("bad_scenario");
        fs::create_dir(&bad).unwrap();
        write_minimal_fixture(&bad, false, false);
        fs::remove_file(bad.join(OUTGOING_BODY)).unwrap();

        let loaded = discover_fixtures(tmp.path()).unwrap();
        let names: Vec<_> = loaded.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["good_a", "good_b"]);
        assert_eq!(loaded.skipped, 1);
    }

    #[test]
    fn discover_fixtures_skips_fixture_with_malformed_body_json() {
        // Arrange: one well-formed fixture and one whose required body
        // JSON is truncated mid-string with a bare trailing backslash --
        // mirrors the journald LineMax truncation that left most of the
        // real corpus unparseable (serde reports `invalid escape` /
        // unexpected EOF).
        let tmp = tempdir().unwrap();
        let good = tmp.path().join("good_scenario");
        fs::create_dir(&good).unwrap();
        write_minimal_fixture(&good, false, false);

        let bad = tmp.path().join("truncated_scenario");
        fs::create_dir(&bad).unwrap();
        write_minimal_fixture(&bad, false, false);
        // Overwrite the outgoing body with a truncated JSON string.
        fs::write(bad.join(OUTGOING_BODY), b"{\"model\": \"y\\").unwrap();

        // Act
        let loaded = discover_fixtures(tmp.path()).unwrap();

        // Assert: the good fixture loads, the malformed one is skipped
        // (not panicked / not erroring the whole load) and the skip is
        // reflected in the count.
        let names: Vec<_> = loaded.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["good_scenario"]);
        assert_eq!(loaded.fixtures.len(), 1);
        assert_eq!(loaded.skipped, 1);
    }
}
