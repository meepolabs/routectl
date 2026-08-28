//! Fixture file format: `meta.json`, ingress / outgoing request
//! bodies + headers (always required), and optional upstream / egress
//! response bodies + headers whose presence is read from the directory
//! listing -- `meta.json` carries no file-presence flags. The
//! filesystem is the only record of which optional files exist, so
//! there is no second copy of that fact to disagree with it.
//!
//! Every file read here is also refused if it ends with routectl's own
//! trace truncation marker (see [`truncation_marker`]) -- a clipped body
//! is a prefix of the wire body and would diff as drift.

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
    #[error(
        "unsupported fixture schema_version {found} in {path}: this loader reads major {supported}"
    )]
    UnsupportedSchemaVersion {
        path: String,
        found: u32,
        supported: u32,
    },
    #[error(
        "{path} ends with routectl's own trace truncation marker `{marker}`: the capture was \
         clipped by the trace body cap, so this file is a prefix of the wire body, not the \
         wire body -- recapture with a larger ROUTECTL_TRACE_BODY_BYTES"
    )]
    TruncatedBody { path: String, marker: String },
}

/// Fixture-format major version this loader reads. The integer IS the
/// major: a format change that an existing fixture cannot satisfy bumps
/// it, and `read_meta` refuses any other value outright rather than
/// half-loading a shape it does not understand.
///
/// # The compatibility rule: no minor, ever -- recapture IS the migration
///
/// 1. ADDING AN OPTIONAL KEY is the only evolution permitted at a fixed
///    major. This loader ignores keys it does not know ([`FixtureMeta`]
///    carries no `deny_unknown_fields`) and every additive field carries
///    `#[serde(default)]`, so a checkout that writes a new key stays
///    readable by a checkout that has never heard of it. A consumer that
///    NEEDS a key refuses the individual fixture lacking it -- the
///    posture already documented on `lane` / `case_id` / `client` /
///    `config_sha` -- rather than the loader refusing the corpus.
/// 2. ANYTHING ELSE bumps this integer, and the bumping change
///    RECAPTURES the committed driver corpus in the same commit. Driver
///    fixtures are synthetic and reproducible by construction, so a
///    single tree never holds a mixed-major committed corpus. That is
///    why `read_meta` gates on `!=` and NOT on `>`: a fixture at a LOWER
///    major is a directory shape this loader cannot read either, and
///    half-loading it is the exact failure the gate exists to prevent.
///    Do not weaken the comparison.
/// 3. THERE IS NO MINOR, of any spelling. A second integer replicated
///    across `scripts/capture_fixtures.sh`, this constant and
///    `docs/REPLAY-FIXTURES.md` with nothing enforcing it reads as a
///    guarantee and is not one. Recapture under clause 2 is the whole
///    migration story.
///
/// The live-box corpus is EXEMPT from clause 2 and cannot be
/// recaptured, which is why [`default_schema_version`] returns the
/// literal 1 instead of tracking this constant.
///
/// Both directions of the gate are pinned by tests:
/// `loader_accepts_a_fixture_whose_meta_carries_an_unknown_key`
/// (clause 1's tolerance),
/// `loader_rejects_a_fixture_at_an_unknown_schema_major` and
/// `loader_rejects_a_fixture_below_the_current_schema_major`
/// (clause 2's `!=`).
pub const FIXTURE_SCHEMA_VERSION: u32 = 1;

/// Implicit major for a fixture captured before `schema_version` was
/// written. Captures predating the key are major 1 by definition -- the
/// key was introduced alongside purely ADDITIVE fields, so a
/// pre-versioning directory is a valid major-1 fixture with those fields
/// absent.
const fn default_schema_version() -> u32 {
    // The literal 1, deliberately NOT `FIXTURE_SCHEMA_VERSION`. This is a
    // historical fact about captures that predate the key; the constant is
    // what THIS loader reads. They are equal only coincidentally, while the
    // current major is still 1. Tracking the constant would make every
    // pre-versioning fixture claim whatever major the loader happens to
    // read, so a future bump would silently mis-read that corpus instead of
    // letting `read_meta`'s gate refuse it.
    1
}

/// Client identity that produced the captured ingress request. Pinned
/// because a client's wire shape varies by version AND by how it reaches
/// routectl: Claude Code sends `role:"system"` turns in `messages[]`
/// through a MITM front proxy but inlines the same content as
/// system-reminder text with zero system turns in base-url mode, so an
/// unpinned mode makes a cross-mode comparison read as drift.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixtureClient {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub connection_mode: String,
}

/// Mirror of the `meta.json` schema documented in
/// `docs/REPLAY-FIXTURES.md`. There is exactly ONE schema: every key the
/// capture rig writes and this struct reads is that schema, and the
/// struct carries no key the rig does not produce.
///
/// File presence is NOT part of it. Which optional response files exist
/// is read from the directory listing (see [`read_optional_response`]);
/// the rig's tmp-then-rename promotion already makes a fixture directory
/// all-or-nothing, so a second copy of that fact in `meta.json` could
/// only ever disagree with the filesystem.
///
/// BACKWARD COMPATIBILITY, decided deliberately: the loader TOLERATES a
/// fixture captured before this schema settled. The fields added here
/// (`lane`, `case_id`, `client`, `config_sha`, `wire_pattern`, and
/// `schema_version` itself) are additive, so `#[serde(default)]` lets the
/// existing
/// per-contributor corpus keep loading -- which is the point, since a
/// clean break would zero out the only wire evidence anyone has on disk
/// and there is no way to recapture a past session. Tolerance here is
/// not permission to run an unpinned fixture through a GATED comparison:
/// a consumer that needs a pinned lane, case, client or config refuses
/// the individual fixture that lacks it. `schema_version` is the one
/// hard gate, because a major bump means the directory shape itself
/// changed and no per-field default can rescue it.
///
/// This struct also deliberately carries no `deny_unknown_fields`: an
/// unknown key is IGNORED, which is clause 1 of the compatibility rule
/// stated on [`FIXTURE_SCHEMA_VERSION`]. That rule governs how this
/// schema may evolve; read it there rather than inferring it from the
/// per-field defaults below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureMeta {
    /// Fixture-format major. Absent means major 1 (pre-versioning).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Egress provider kind in the `PROVIDER_KIND` vocabulary of
    /// `routectl-providers` (`anthropic`, not `anthropic-api`). Kept
    /// distinct from `lane`, which normalizes the same concept into the
    /// config vocabulary.
    ///
    /// This is also the api-shape carrier: `lane` collapses
    /// `bedrock-invoke` and `bedrock-converse` onto one token, and this
    /// raw value is what still tells the two apart. There is no separate
    /// `api_shape` field for that reason.
    pub provider_kind: String,
    /// Egress lane token in the `kind_str()` vocabulary of
    /// `ProviderEntry` (`anthropic-api`, `openai-compat`,
    /// `openai-responses`, `bedrock`, `gemini`) -- the vocabulary the
    /// lane contract derives a lane's class from. Normalized at WRITE
    /// time by the capture rig; empty on a pre-versioning fixture.
    #[serde(default)]
    pub lane: String,
    /// Ingress dialect that parsed the inbound body, in the vocabulary
    /// of `IngressAdapter::id()` (`anthropic`, `openai`,
    /// `openai-responses`) -- so a consumer dispatches on this value
    /// with no mapping table. EMPTY when the capture could not extract
    /// the token, never a sentinel word outside that vocabulary.
    #[serde(default)]
    pub ingress_kind: String,
    /// Stable identifier for the SCENARIO this fixture captures, as
    /// opposed to the one-off request id. A rerun of the same case
    /// re-lands on the same identity, so it either matches or diffs.
    /// Empty on a request-id-keyed pre-versioning fixture.
    #[serde(default)]
    pub case_id: String,
    /// Hash of the config in force at capture time. A rerun under a
    /// drifted config would otherwise read as client drift. Empty when
    /// the capture ran against an ad-hoc local config.
    #[serde(default)]
    pub config_sha: String,
    /// Wire shape the driven case claims to cover, in the closed
    /// vocabulary of `WIRE_PATTERNS` in
    /// `scripts/drivers/lib/validate_case.py`. Derived from the case file
    /// by the driver runner, so it records which shape a fixture was
    /// captured FOR -- information that exists nowhere else on disk.
    /// Empty on a live-box capture, which cannot observe it, and on any
    /// fixture predating the key.
    #[serde(default)]
    pub wire_pattern: String,
    #[serde(default)]
    pub client: FixtureClient,
    pub stream: bool,
    /// Post-alias provider model id; populated by the capture rig.
    /// Optional so older fixtures (and the loader's unit tests) load
    /// without a forced rewrite.
    #[serde(default)]
    pub model: Option<String>,
    /// Workspace package version stamped by `capture_fixtures.sh`.
    /// Purely informational triage aid -- NOT a compatibility signal.
    /// `schema_version` is the only field the loader gates on.
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
    /// Empty when `upstream_response.json` is absent from the directory.
    pub upstream_response_bytes: Vec<u8>,
    /// Empty when `upstream_response.headers.json` is absent.
    pub upstream_response_headers: Vec<(String, String)>,
    /// Empty when `egress_response.json` is absent from the directory.
    pub egress_response_bytes: Vec<u8>,
    /// Empty when `egress_response.headers.json` is absent.
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
    let (upstream_response_bytes, upstream_response_headers) =
        read_optional_response(dir, UPSTREAM_BODY, UPSTREAM_HEADERS)?;
    let (egress_response_bytes, egress_response_headers) =
        read_optional_response(dir, EGRESS_BODY, EGRESS_HEADERS)?;

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
/// unsupported schema major). A degraded corpus -- e.g. one truncated by
/// a log line-length limit at capture time -- thins `fixtures` while
/// `skipped` rises, so the run's coverage is visible rather than silent.
///
/// `fixtures.len() + skipped` is the number of ENTRIES WALKED, which is
/// the only honest answer to "did this corpus hold anything". A caller
/// keying an emptiness note on `fixtures.len()` alone reports a corpus
/// of broken fixtures as an ABSENT corpus, which is the loudest fact on
/// disk reported as the quietest.
#[derive(Debug, Default)]
pub struct LoadedCorpus {
    pub fixtures: Vec<Fixture>,
    pub skipped: usize,
}

impl LoadedCorpus {
    /// Entries the walk considered: loaded plus unloadable.
    pub const fn entries_walked(&self) -> usize {
        self.fixtures.len() + self.skipped
    }
}

/// Immediate non-dot subdirectories of `parent`, sorted by name for
/// deterministic test ordering. Non-directory entries (`README.md`,
/// `.gitkeep`) are skipped. Filesystem-level errors reading `parent`
/// itself propagate.
fn child_dirs(parent: &Path) -> Result<Vec<PathBuf>, ReplayError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let read = fs::read_dir(parent).map_err(|e| ReplayError::Io {
        path: parent.display().to_string(),
        source: e,
    })?;
    for entry in read {
        let entry = entry.map_err(|e| ReplayError::Io {
            path: parent.display().to_string(),
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
    Ok(dirs)
}

/// Load every directory in `dirs` as a fixture. One that fails to load
/// (malformed `meta.json` or body JSON, missing required file,
/// unsupported schema major) is logged to stderr naming the directory
/// and the error, counted in `LoadedCorpus.skipped`, and skipped rather
/// than aborting the whole corpus -- so one bad fixture cannot blind the
/// run to every other fixture's regression signal. A final summary line
/// reports loaded vs skipped so a thin corpus is visible.
fn load_all(dirs: Vec<PathBuf>) -> LoadedCorpus {
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
    LoadedCorpus { fixtures, skipped }
}

/// Walk `canon_root` for fixture subdirectories ONE level deep: this is
/// the LIVE-BOX corpus shape, which is flat and request-id-keyed.
/// Loading, skipping, and reporting follow [`load_all`]; filesystem-level
/// errors reading `canon_root` itself propagate as `Err`.
///
/// The driver corpus is keyed `<lane>/<case_id>` and needs
/// [`discover_driver_fixtures`] instead. The depth belongs to the ROOT,
/// not to the caller, which is why it is a second function rather than a
/// parameter every live-box call site would have to pass correctly.
pub fn discover_fixtures(canon_root: &Path) -> Result<LoadedCorpus, ReplayError> {
    Ok(load_all(child_dirs(canon_root)?))
}

/// Walk the DRIVER corpus root, whose fixtures land two levels deep at
/// `<root>/<lane>/<case_id>`. Only the case directories are offered to
/// the loader; a lane directory is never itself a fixture.
///
/// A lane directory holding no case directory contributes nothing --
/// there is no fixture there to load and no fixture there to skip, and
/// counting the lane itself as either would report a walk of the corpus
/// shape as a walk of its contents. That is exactly the fault this
/// function replaced: the single-level walk handed each lane directory
/// to `load_fixture`, which refused it for a missing `meta.json`, so a
/// perfectly good corpus read as `loaded 0, skipped 1`.
pub fn discover_driver_fixtures(driver_root: &Path) -> Result<LoadedCorpus, ReplayError> {
    let mut cases: Vec<PathBuf> = Vec::new();
    for lane in child_dirs(driver_root)? {
        cases.extend(child_dirs(&lane)?);
    }
    Ok(load_all(cases))
}

fn read_meta(dir: &Path) -> Result<FixtureMeta, ReplayError> {
    let path = dir.join(META_JSON);
    let bytes = read_file_or_missing(&path)?;
    let meta: FixtureMeta = serde_json::from_slice(&bytes).map_err(|e| ReplayError::Json {
        path: path.display().to_string(),
        source: e,
    })?;
    // `!=`, not `>`: clause 2 of the rule on FIXTURE_SCHEMA_VERSION. A
    // lower major is a directory shape this loader cannot read either.
    if meta.schema_version != FIXTURE_SCHEMA_VERSION {
        return Err(ReplayError::UnsupportedSchemaVersion {
            path: path.display().to_string(),
            found: meta.schema_version,
            supported: FIXTURE_SCHEMA_VERSION,
        });
    }
    Ok(meta)
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

/// Read body + headers for an optional response slot, taking presence
/// from the directory itself: each of the two files is read when it
/// exists and yields an empty vector when it does not. The four
/// combinations (both, body only, headers only, neither) are all valid
/// -- a stream capture legitimately has response HEADERS with no
/// response BODY, since the body arrives as SSE frames rather than a
/// single logged JSON value.
#[allow(clippy::type_complexity)]
fn read_optional_response(
    dir: &Path,
    body_name: &str,
    headers_name: &str,
) -> Result<(Vec<u8>, Vec<(String, String)>), ReplayError> {
    let body_path = dir.join(body_name);
    let body = if body_path.exists() {
        read_file_or_missing(&body_path)?
    } else {
        Vec::new()
    };
    let headers_path = dir.join(headers_name);
    let headers = if headers_path.exists() {
        let value = read_required_json(dir, headers_name)?;
        parse_headers_value(&value, &headers_path)?
    } else {
        Vec::new()
    };
    Ok((body, headers))
}

/// Read a file's bytes; map `NotFound` to `MissingFile` (which carries
/// the path) and any other io error to `Io { path, source }`. A file
/// ending in routectl's own trace truncation marker is refused here, so
/// every fixture file passes the check exactly once and the refusal
/// names truncation rather than surfacing as a downstream JSON parse
/// error.
fn read_file_or_missing(path: &Path) -> Result<Vec<u8>, ReplayError> {
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ReplayError::MissingFile(path.display().to_string())
        } else {
            ReplayError::Io {
                path: path.display().to_string(),
                source: e,
            }
        }
    })?;
    if let Some(marker) = truncation_marker(&bytes) {
        return Err(ReplayError::TruncatedBody {
            path: path.display().to_string(),
            marker,
        });
    }
    Ok(bytes)
}

/// Leading and trailing literals of the marker `truncate_json_for_log`
/// (`routectl-core/src/log_safe.rs`) appends to a clipped body:
/// `... [truncated at <cap> bytes]`.
const TRUNCATION_MARKER_HEAD: &str = "... [truncated at ";
const TRUNCATION_MARKER_TAIL: &str = " bytes]";

/// Widest byte window the marker can occupy: both literals plus a
/// `usize` cap in decimal. The 20 is exactly `usize::MAX`'s decimal
/// width (`18446744073709551615`), so the window is TIGHT -- it has no
/// slack, and any change to either literal must be reflected here or a
/// real marker will fall outside the window and go undetected.
const TRUNCATION_MARKER_WINDOW: usize =
    TRUNCATION_MARKER_HEAD.len() + 20 + TRUNCATION_MARKER_TAIL.len();

/// The exact truncation marker this file ends with, if any.
///
/// ANCHORED TO THE TAIL AND MATCHED IN FULL -- head literal, decimal
/// cap, tail literal -- deliberately, not the bare phrase `truncated
/// at`.
///
/// End-anchoring makes a false positive STRUCTURALLY impossible, not
/// merely unlikely: valid JSON always ends with `}` or `]`, so a
/// well-formed fixture cannot end with the marker, and a body that
/// merely discusses truncation always keeps the phrase inside a string
/// value. Only a clipped body has the marker as its final bytes.
///
/// The empirical case for full-marker over bare-phrase matching, from
/// the live-box corpus at 250 fixtures: the bare phrase matches 12
/// files, every one of them legitimate prompt content (a captured
/// system-reminder reading "...which was truncated at 27748 chars") and
/// every one of them valid JSON; the full marker matches 0. A
/// bare-phrase detector would therefore refuse healthy fixtures at a
/// 100% false-positive rate.
fn truncation_marker(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
    let start = end.saturating_sub(TRUNCATION_MARKER_WINDOW);
    let tail = String::from_utf8_lossy(&bytes[start..end]);
    let rest = tail.strip_suffix(TRUNCATION_MARKER_TAIL)?;
    let digits_len = rest.bytes().rev().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 {
        return None;
    }
    let (head, digits) = rest.split_at(rest.len() - digits_len);
    if !head.ends_with(TRUNCATION_MARKER_HEAD) {
        return None;
    }
    Some(format!(
        "{TRUNCATION_MARKER_HEAD}{digits}{TRUNCATION_MARKER_TAIL}"
    ))
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
    use super::super::plant::{
        current_meta, plant_driver_case, plant_fixture, write_required_files,
    };
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn write_headers_file(path: PathBuf) {
        fs::write(
            path,
            serde_json::to_vec(&json!([["content-type", "application/json"]])).unwrap(),
        )
        .unwrap();
    }

    /// Which of the two files of one optional response slot exist on
    /// disk. All four combinations are legal: a stream capture has
    /// headers with no body.
    #[derive(Clone, Copy)]
    struct Present {
        body: bool,
        headers: bool,
    }

    const BOTH: Present = Present {
        body: true,
        headers: true,
    };
    const NEITHER: Present = Present {
        body: false,
        headers: false,
    };
    const HEADERS_ONLY: Present = Present {
        body: false,
        headers: true,
    };
    const BODY_ONLY: Present = Present {
        body: true,
        headers: false,
    };

    fn write_fixture(dir: &Path, upstream: Present, egress: Present) {
        write_required_files(dir, &current_meta());
        if upstream.body {
            fs::write(dir.join(UPSTREAM_BODY), b"{\"id\":\"u\"}").unwrap();
        }
        if upstream.headers {
            write_headers_file(dir.join(UPSTREAM_HEADERS));
        }
        if egress.body {
            fs::write(dir.join(EGRESS_BODY), b"{\"id\":\"e\"}").unwrap();
        }
        if egress.headers {
            write_headers_file(dir.join(EGRESS_HEADERS));
        }
    }

    #[test]
    fn loader_round_trips_minimal_fixture() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, BOTH, BOTH);

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
    fn loader_reads_case_lane_client_and_config_sha_from_meta() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, BOTH, BOTH);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.lane, "anthropic-api");
        assert_eq!(f.meta.case_id, "smoke");
        assert_eq!(f.meta.config_sha, "abc123");
        assert_eq!(f.meta.client.name, "claude-code");
        assert_eq!(f.meta.client.version, "2.1.167");
        assert_eq!(f.meta.client.connection_mode, "base-url");
    }

    /// `ingress_kind` is what a downstream consumer dispatches the
    /// ingress adapter on, and the rig has written it since before the
    /// loader could read it -- so the value the rig emits must survive
    /// the load verbatim, in `IngressAdapter::id()` spelling.
    #[test]
    fn loader_round_trips_the_rig_written_ingress_kind() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let mut meta = current_meta();
        meta["ingress_kind"] = json!("openai-responses");
        write_required_files(&dir, &meta);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.ingress_kind, "openai-responses");
    }

    /// The wire pattern is the one claim in `meta.json` that no other
    /// file on disk carries, so a load that dropped it would leave the
    /// fixture unable to say which shape it was captured for.
    #[test]
    fn loader_round_trips_the_wire_pattern_a_driver_case_claims() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let mut meta = current_meta();
        meta["wire_pattern"] = json!("cache-breakpoints");
        write_required_files(&dir, &meta);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.wire_pattern, "cache-breakpoints");
    }

    /// Paired negative for the round-trip above: the key is ADDITIVE, so
    /// a fixture written before it existed must still load, with the
    /// claim empty rather than the load refused.
    #[test]
    fn loader_accepts_a_fixture_with_no_wire_pattern_key() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let mut meta = current_meta();
        assert!(
            meta.as_object_mut()
                .unwrap()
                .remove("wire_pattern")
                .is_some(),
            "the planted meta must CARRY wire_pattern for its removal to be \
             the condition under test",
        );
        write_required_files(&dir, &meta);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.wire_pattern, "");
        assert_eq!(f.meta.case_id, "smoke");
    }

    #[test]
    fn loader_accepts_upstream_and_egress_response_bodies_with_headers() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, BOTH, BOTH);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.upstream_response_bytes, b"{\"id\":\"u\"}");
        assert_eq!(f.upstream_response_headers.len(), 1);
        assert_eq!(f.egress_response_bytes, b"{\"id\":\"e\"}");
        assert_eq!(f.egress_response_headers.len(), 1);
    }

    /// The shape of a STREAM capture: response headers were logged, the
    /// body arrived as SSE frames and was never logged as one JSON value.
    /// This combination is what the deleted `has_*` flags rejected, and
    /// it accounted for the overwhelming majority of the corpus.
    #[test]
    fn loader_accepts_response_headers_without_a_response_body() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, HEADERS_ONLY, HEADERS_ONLY);

        let f = load_fixture(&dir).unwrap();

        assert!(f.upstream_response_bytes.is_empty());
        assert_eq!(f.upstream_response_headers.len(), 1);
        assert!(f.egress_response_bytes.is_empty());
        assert_eq!(f.egress_response_headers.len(), 1);
    }

    /// A capture taken without `ROUTECTL_TRACE_HEADERS`: bodies logged,
    /// no header lines to extract.
    #[test]
    fn loader_accepts_a_response_body_without_its_headers() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, BODY_ONLY, BODY_ONLY);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.upstream_response_bytes, b"{\"id\":\"u\"}");
        assert!(f.upstream_response_headers.is_empty());
        assert_eq!(f.egress_response_bytes, b"{\"id\":\"e\"}");
        assert!(f.egress_response_headers.is_empty());
    }

    #[test]
    fn loader_accepts_a_fixture_with_no_optional_response_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);

        let f = load_fixture(&dir).unwrap();

        assert!(f.upstream_response_bytes.is_empty());
        assert!(f.upstream_response_headers.is_empty());
        assert!(f.egress_response_bytes.is_empty());
        assert!(f.egress_response_headers.is_empty());
    }

    #[test]
    fn loader_accepts_a_fixture_at_the_current_schema_major() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, BOTH, BOTH);

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.schema_version, FIXTURE_SCHEMA_VERSION);
    }

    #[test]
    fn loader_rejects_a_fixture_at_an_unknown_schema_major() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let mut meta = current_meta();
        meta["schema_version"] = json!(FIXTURE_SCHEMA_VERSION + 1);
        write_required_files(&dir, &meta);

        let err = load_fixture(&dir).unwrap_err();

        match &err {
            ReplayError::UnsupportedSchemaVersion {
                found, supported, ..
            } => {
                assert_eq!(*found, FIXTURE_SCHEMA_VERSION + 1);
                assert_eq!(*supported, FIXTURE_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    /// Clause 2 of the compatibility rule, LOWER direction: the gate is
    /// `!=`, not `>`. A fixture below the current major is a directory
    /// shape this loader cannot read either, so half-loading it is the
    /// same failure an unknown-higher major would cause. Weakening the
    /// comparison to `>` -- the plausible "be liberal with old data"
    /// edit -- turns this red, which is why the lower direction is
    /// pinned at all.
    #[test]
    fn loader_rejects_a_fixture_below_the_current_schema_major() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let mut meta = current_meta();
        meta["schema_version"] = json!(0);
        write_required_files(&dir, &meta);

        let err = load_fixture(&dir).unwrap_err();

        match &err {
            ReplayError::UnsupportedSchemaVersion {
                found, supported, ..
            } => {
                assert_eq!(*found, 0);
                assert_eq!(*supported, FIXTURE_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
        }
    }

    /// Clause 1 of the compatibility rule: adding an optional key is the
    /// permitted evolution at a fixed major, which only holds because a
    /// reader IGNORES keys it does not know. A checkout that writes a new
    /// key must stay readable by a checkout built before it existed, so
    /// `FixtureMeta` must never gain `deny_unknown_fields`.
    #[test]
    fn loader_accepts_a_fixture_whose_meta_carries_an_unknown_key() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let mut meta = current_meta();
        meta["a_key_this_loader_has_never_heard_of"] = json!({"nested": [1, 2, 3]});
        write_required_files(&dir, &meta);

        // Non-vacuity: prove the key reached disk. Without this the test
        // would also pass if the fixture writer silently dropped it, which
        // is tolerance of nothing.
        let on_disk: Value =
            serde_json::from_slice(&fs::read(dir.join(META_JSON)).unwrap()).unwrap();
        assert!(
            on_disk
                .get("a_key_this_loader_has_never_heard_of")
                .is_some(),
            "the unknown key must be present in the written meta.json, or this \
             test pins nothing: {on_disk}"
        );

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.schema_version, FIXTURE_SCHEMA_VERSION);
        assert_eq!(f.meta.provider_kind, "anthropic");
    }

    /// `default_schema_version()` must stay the LITERAL 1, never
    /// `FIXTURE_SCHEMA_VERSION`. If it tracked the constant, bumping the
    /// major would make every pre-versioning fixture in the unrecapturable
    /// live-box corpus claim the NEW major and be half-loaded by a loader
    /// built for a different directory shape -- instead of being refused
    /// by `read_meta`'s gate, which exists to prevent exactly that.
    #[test]
    fn pre_versioning_default_major_is_a_literal_not_the_loader_constant() {
        assert_eq!(
            default_schema_version(),
            1,
            "a fixture with no `schema_version` key is major 1 as a historical \
             fact -- it predates the key. Returning FIXTURE_SCHEMA_VERSION here \
             makes the unrecapturable live-box corpus claim whatever major this \
             loader reads, so a future bump mis-reads it silently instead of \
             refusing it."
        );

        // The assert above is `1 == 1` while the constant is still 1, so it
        // documents intent without guarding it. This half is the guard: read
        // this function's own body out of the source and require the literal,
        // so reverting it to track FIXTURE_SCHEMA_VERSION fails TODAY rather
        // than on the day of the bump -- which is the day the unrecapturable
        // live-box corpus would be silently mis-read.
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/replay/loader.rs"),
        )
        .expect("this test file must be readable to guard its own constant");
        let body = src
            .split_once("const fn default_schema_version() -> u32 {")
            .expect("default_schema_version must exist with this exact signature")
            .1
            .split_once('}')
            .expect("its body must be brace-closed")
            .0;
        // Strip line comments first: the body's own comment EXPLAINS why it does
        // not use the constant, so a naive substring check fails on the
        // explanation rather than on the code.
        let code: String = body
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("FIXTURE_SCHEMA_VERSION"),
            "default_schema_version's body names FIXTURE_SCHEMA_VERSION: it must \
             return the literal 1. Tracking the constant is the defect this test \
             exists to prevent, and an equality assertion cannot catch it until \
             the constant has already moved. Code was: {code:?}"
        );
        assert!(
            code.lines().any(|l| l.trim() == "1"),
            "default_schema_version's body does not contain the bare literal 1; \
             if the shape changed, update this guard deliberately. Code: {code:?}"
        );
    }

    /// `scripts/capture_fixtures.sh` carries a REPLICA of
    /// `FIXTURE_SCHEMA_VERSION` -- it stamps the major into every
    /// `meta.json` from shell, without linking against this crate. The two
    /// were joined only by a comment, which is the drift class already
    /// closed once for the scrub gate's credential rules: bump the Rust
    /// constant alone and every subsequent capture is refused by every
    /// loader; bump the shell one alone and the corpus loads under a wrong
    /// major.
    ///
    /// This test parses the assignment out of the script rather than
    /// restating its value, so a hand-copied expectation cannot become a
    /// third replica.
    #[test]
    fn capture_rig_schema_version_matches_the_loader_constant() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/capture_fixtures.sh")
            .canonicalize()
            .expect(
                "capture_fixtures.sh must exist: it is the only writer of the \
                 fixture schema_version",
            );
        let source = fs::read_to_string(&script).expect("capture_fixtures.sh must be readable");

        // Take the shell assignment's right-hand side: `SCHEMA_VERSION=<n>`
        // up to the end of that line.
        let assignments: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("SCHEMA_VERSION="))
            .collect();

        assert_eq!(
            assignments.len(),
            1,
            "found {} `SCHEMA_VERSION=` assignments in {} -- the parse broke or \
             the rig grew a second writer of the major, and a silently-empty \
             match would make this weld vacuous",
            assignments.len(),
            script.display()
        );

        let shell_version: u32 = assignments[0].trim().parse().unwrap_or_else(|e| {
            panic!(
                "`SCHEMA_VERSION={}` in {} is not an integer major ({e}) -- keep it \
                 in step with FIXTURE_SCHEMA_VERSION",
                assignments[0],
                script.display()
            )
        });

        assert_eq!(
            shell_version,
            FIXTURE_SCHEMA_VERSION,
            "{} writes schema_version {} into every meta.json, but this loader \
             reads major {} and refuses any other value. Keep the two in step: \
             bumping one alone either refuses every new capture or loads the \
             corpus under a wrong major.",
            script.display(),
            shell_version,
            FIXTURE_SCHEMA_VERSION
        );
    }

    /// The tolerance decision documented on `FixtureMeta`: a capture
    /// taken before the schema settled still loads, with the added
    /// fields empty, because the corpus cannot be recaptured.
    #[test]
    fn loader_accepts_a_pre_versioning_fixture_with_the_new_fields_empty() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        let meta = json!({
            "provider_kind": "anthropic",
            "ingress_kind": "anthropic",
            "stream": true,
            "model": "claude-sonnet-4-5",
        });
        write_required_files(&dir, &meta);
        write_headers_file(dir.join(UPSTREAM_HEADERS));

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.meta.schema_version, FIXTURE_SCHEMA_VERSION);
        assert_eq!(f.meta.lane, "");
        assert_eq!(f.meta.case_id, "");
        assert_eq!(f.meta.config_sha, "");
        assert_eq!(f.meta.wire_pattern, "");
        assert_eq!(f.meta.client.name, "");
        assert_eq!(f.upstream_response_headers.len(), 1);
    }

    #[test]
    fn loader_errors_when_required_file_missing() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        fs::remove_file(dir.join(OUTGOING_BODY)).unwrap();

        let err = load_fixture(&dir).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(OUTGOING_BODY),
            "error did not name the missing file: {msg}"
        );
    }

    #[test]
    fn discover_fixtures_sorts_by_directory_name() {
        let tmp = tempdir().unwrap();
        for name in ["zeta_scenario", "alpha_scenario", "mu_scenario"] {
            let dir = tmp.path().join(name);
            fs::create_dir(&dir).unwrap();
            write_fixture(&dir, NEITHER, NEITHER);
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
            write_fixture(&dir, NEITHER, NEITHER);
        }
        let bad = tmp.path().join("bad_scenario");
        fs::create_dir(&bad).unwrap();
        write_fixture(&bad, NEITHER, NEITHER);
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
        write_fixture(&good, NEITHER, NEITHER);

        let bad = tmp.path().join("truncated_scenario");
        fs::create_dir(&bad).unwrap();
        write_fixture(&bad, NEITHER, NEITHER);
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

    /// A body the trace body cap clipped: a JSON prefix with
    /// `truncate_json_for_log`'s marker appended. Written byte-for-byte
    /// as that function emits it.
    fn cap_clipped_body(cap: usize) -> Vec<u8> {
        format!("{{\"model\":\"claude-sonnet-4-5\",\"messages\":[{{\"role\":\"user\",\"content\":\"hel... [truncated at {cap} bytes]")
            .into_bytes()
    }

    /// A body whose PROMPT TEXT talks about truncation. The load-bearing
    /// part is the phrase `which was truncated at 27748 chars`, the shape
    /// carried by 12 of the 250 live-box fixtures -- all valid, complete
    /// JSON. The surrounding sentence is SYNTHESIZED, not excerpted: a
    /// verbatim excerpt of a captured body would commit the very content
    /// this loader's two-root split exists to keep out of the repo.
    fn body_mentioning_truncation_in_prompt_text() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "claude-sonnet-4-5",
            "messages": [{
                "role": "user",
                "content": "The upstream summary (which was truncated at 27748 chars) \
                            is the one to use, not the full transcript.",
            }],
        }))
        .unwrap()
    }

    #[test]
    fn loader_refuses_a_body_carrying_the_trace_truncation_marker() {
        // Arrange
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        fs::write(dir.join(OUTGOING_BODY), cap_clipped_body(16384)).unwrap();

        // Act
        let err = load_fixture(&dir).unwrap_err();

        // Assert: refused AS TRUNCATION (not as a JSON parse error), and
        // the error names the offending file plus the marker it found.
        match &err {
            ReplayError::TruncatedBody { path, marker } => {
                assert!(
                    path.contains(OUTGOING_BODY),
                    "error did not name the file: {path}"
                );
                assert_eq!(marker, "... [truncated at 16384 bytes]");
            }
            other => panic!("expected TruncatedBody, got {other:?}"),
        }
    }

    /// POSITIVE CONTROL for the test above, and the reason the detector
    /// matches the full anchored marker rather than the bare phrase
    /// `truncated at`. On the live-box corpus at 250 fixtures the bare
    /// phrase matches 12 files -- all legitimate prompt content, all
    /// valid JSON -- while the full marker matches 0. A bare-phrase
    /// detector would refuse those 12 healthy fixtures.
    #[test]
    fn loader_accepts_a_body_whose_prompt_text_contains_the_phrase_truncated_at() {
        // Arrange
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        let body = body_mentioning_truncation_in_prompt_text();
        assert!(
            String::from_utf8_lossy(&body).contains("truncated at"),
            "fixture does not carry the phrase the detector must NOT match",
        );
        fs::write(dir.join(OUTGOING_BODY), &body).unwrap();

        // Act
        let f = load_fixture(&dir).unwrap();

        // Assert
        assert_eq!(f.name, "scenario");
        assert_eq!(f.outgoing_request["model"], json!("claude-sonnet-4-5"));
    }

    /// The marker is refused wherever it lands, including a response
    /// half whose bytes are never JSON-parsed -- so an SSE capture
    /// clipped by the cap cannot slip through unparsed.
    #[test]
    fn loader_refuses_a_truncated_optional_response_body() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        fs::write(dir.join(UPSTREAM_BODY), cap_clipped_body(1048576)).unwrap();

        let err = load_fixture(&dir).unwrap_err();

        match &err {
            ReplayError::TruncatedBody { path, marker } => {
                assert!(
                    path.contains(UPSTREAM_BODY),
                    "error did not name the file: {path}"
                );
                assert_eq!(marker, "... [truncated at 1048576 bytes]");
            }
            other => panic!("expected TruncatedBody, got {other:?}"),
        }
    }

    /// Refusal flows through the existing skip-and-count path, so one
    /// clipped fixture never blinds the run to the rest of the corpus.
    #[test]
    fn discover_fixtures_counts_a_truncated_fixture_as_skipped() {
        // Arrange
        let tmp = tempdir().unwrap();
        for name in ["good_a", "good_b"] {
            let dir = tmp.path().join(name);
            fs::create_dir(&dir).unwrap();
            write_fixture(&dir, NEITHER, NEITHER);
        }
        let clipped = tmp.path().join("clipped_scenario");
        fs::create_dir(&clipped).unwrap();
        write_fixture(&clipped, NEITHER, NEITHER);
        fs::write(clipped.join(INGRESS_BODY), cap_clipped_body(16384)).unwrap();

        // Act
        let loaded = discover_fixtures(tmp.path()).unwrap();

        // Assert
        let names: Vec<_> = loaded.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["good_a", "good_b"]);
        assert_eq!(loaded.skipped, 1);
    }

    /// The detector is anchored to the END of the file: the marker can
    /// only be appended there, and a body quoting it mid-prompt is a
    /// complete body.
    #[test]
    fn loader_accepts_a_body_quoting_the_marker_inside_prompt_content() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        fs::write(
            dir.join(OUTGOING_BODY),
            serde_json::to_vec(&json!({
                "model": "claude-sonnet-4-5",
                "messages": [{
                    "role": "user",
                    "content": "the log line ended with ... [truncated at 16384 bytes] \
                                so raise the cap",
                }],
            }))
            .unwrap(),
        )
        .unwrap();

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.outgoing_request["model"], json!("claude-sonnet-4-5"));
    }

    /// Trailing whitespace / a trailing newline must not hide the
    /// marker, since a shell redirect commonly appends one.
    #[test]
    fn loader_refuses_a_truncated_body_with_a_trailing_newline() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        let mut body = cap_clipped_body(16384);
        body.extend_from_slice(b"\n");
        fs::write(dir.join(OUTGOING_BODY), &body).unwrap();

        let err = load_fixture(&dir).unwrap_err();

        assert!(
            matches!(err, ReplayError::TruncatedBody { .. }),
            "expected TruncatedBody, got {err:?}",
        );
    }

    /// The cap must be DECIMAL DIGITS, not arbitrary text: a tail that
    /// is marker-shaped but carries no number is not routectl's marker.
    ///
    /// The body is written RAW, ending in the digitless tail. Routing
    /// this through `serde_json::to_vec` instead would put the tail
    /// inside a string value, so the file would end `bytes]"}` and the
    /// end-anchor alone would decide the outcome -- the digits branch
    /// would never be reached and deleting its guard would not fail this
    /// test. Asserted against `truncation_marker` directly so the
    /// verdict is the detector's, not a downstream parse error's.
    #[test]
    fn detector_ignores_a_marker_shaped_tail_whose_cap_is_not_digits() {
        // Arrange: three digitless caps, each ending the body exactly.
        for cap in ["", "some", "16k"] {
            let body = format!("{{\"model\":\"m\"}}... [truncated at {cap} bytes]");

            // Act
            let found = truncation_marker(body.as_bytes());

            // Assert
            assert!(
                found.is_none(),
                "cap `{cap}` is not decimal; expected no marker, got {found:?}",
            );
        }
    }

    /// Positive control for the test above: the same tail WITH a decimal
    /// cap is the marker, so the digits rule cannot be tightened into
    /// missing the real thing.
    #[test]
    fn detector_matches_a_marker_shaped_tail_whose_cap_is_digits() {
        for cap in ["0", "16384", "1048576"] {
            let body = format!("{{\"model\":\"m\"}}... [truncated at {cap} bytes]");

            let found = truncation_marker(body.as_bytes());

            assert_eq!(
                found.as_deref(),
                Some(&*format!("... [truncated at {cap} bytes]"))
            );
        }
    }

    /// A body that merely QUOTES a digitless marker-shaped tail inside a
    /// string value still loads -- the loader-level counterpart of the
    /// detector test above.
    #[test]
    fn loader_accepts_a_body_quoting_a_marker_shaped_tail_without_a_cap() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("scenario");
        fs::create_dir(&dir).unwrap();
        write_fixture(&dir, NEITHER, NEITHER);
        fs::write(
            dir.join(OUTGOING_BODY),
            serde_json::to_vec(&json!({
                "model": "m",
                "note": "... [truncated at some bytes]",
            }))
            .unwrap(),
        )
        .unwrap();

        let f = load_fixture(&dir).unwrap();

        assert_eq!(f.outgoing_request["model"], json!("m"));
    }

    // ---- the two roots' walk depths, asserted separately ----
    //
    // The driver corpus lands at `<root>/<lane>/<case_id>` and the
    // live-box corpus is flat and request-id-keyed. The pairs below pin
    // BOTH directions on BOTH walkers: a depth fix that "works" by
    // walking everything everywhere would let a live-box fixture be
    // found two deep, and a walker that silently skips every entry
    // satisfies any load assertion that only counts what came back.

    #[test]
    fn the_driver_walk_loads_a_case_two_levels_deep_and_never_the_lane_directory() {
        // Arrange: two lanes, three cases.
        let tmp = tempdir().unwrap();
        plant_driver_case(tmp.path(), "anthropic-api", "plain-turn-01");
        plant_driver_case(tmp.path(), "anthropic-api", "thinking-01");
        plant_driver_case(tmp.path(), "openai-responses", "plain-turn-01");

        // Act
        let loaded = discover_driver_fixtures(tmp.path()).unwrap();

        // Assert: the three CASE directories loaded and neither lane
        // directory was offered to the loader as a fixture. `skipped`
        // is the load-bearing half -- before the two-level walk, the
        // lane directory itself was the entry and it counted here.
        let names: Vec<_> = loaded.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["plain-turn-01", "thinking-01", "plain-turn-01"]);
        assert_eq!(
            loaded.skipped, 0,
            "a lane directory was walked as a fixture"
        );
        assert_eq!(loaded.entries_walked(), 3);
    }

    /// PAIRED CONTROL for the test above: a genuinely broken case two
    /// levels deep must still be REACHED and counted. Without it, a
    /// walker that skips every entry silently passes the clean-corpus
    /// assertion, since a corpus of nothing has no unexpected skips.
    #[test]
    fn the_driver_walk_counts_a_broken_case_two_levels_deep_as_skipped() {
        // Arrange
        let tmp = tempdir().unwrap();
        plant_driver_case(tmp.path(), "anthropic-api", "good-01");
        let bad = plant_driver_case(tmp.path(), "anthropic-api", "broken-01");
        fs::remove_file(bad.join(META_JSON)).unwrap();

        // Act
        let loaded = discover_driver_fixtures(tmp.path()).unwrap();

        // Assert
        let names: Vec<_> = loaded.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["good-01"]);
        assert_eq!(loaded.skipped, 1);
        assert_eq!(loaded.entries_walked(), 2);
    }

    /// A lane directory holding nothing is corpus SHAPE, not corpus
    /// content: it is neither a fixture nor a skipped fixture.
    #[test]
    fn an_empty_lane_directory_contributes_no_entry_to_the_driver_walk() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("anthropic-api")).unwrap();

        let loaded = discover_driver_fixtures(tmp.path()).unwrap();

        assert!(loaded.fixtures.is_empty());
        assert_eq!(loaded.skipped, 0);
        assert_eq!(loaded.entries_walked(), 0);
    }

    /// The live-box walk is unchanged and stays SINGLE-level: a fixture
    /// one deep loads, and the same fixture planted two deep does not.
    /// Its root is flat and request-id-keyed, and both replay drivers
    /// walk it through `discover_fixtures`.
    #[test]
    fn the_live_box_walk_loads_one_level_deep_and_not_two() {
        // Arrange: the same fixture content at both depths, in
        // separate roots so neither can mask the other.
        let flat = tempdir().unwrap();
        plant_fixture(&flat.path().join("req-0001"));
        let nested = tempdir().unwrap();
        plant_fixture(&nested.path().join("anthropic-api").join("req-0001"));

        // Act
        let one_deep = discover_fixtures(flat.path()).unwrap();
        let two_deep = discover_fixtures(nested.path()).unwrap();

        // Assert
        let names: Vec<_> = one_deep.fixtures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["req-0001"]);
        assert_eq!(one_deep.skipped, 0);

        // The intermediate directory is what the flat walk reaches, and
        // it is not a fixture: nothing loads, and the entry is skipped
        // rather than invisible.
        assert!(
            two_deep.fixtures.is_empty(),
            "the live-box walk descended a level it must not",
        );
        assert_eq!(two_deep.skipped, 1);
    }
}
