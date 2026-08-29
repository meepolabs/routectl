//! The gated-lane list: which egress lanes of the DRIVER corpus a
//! consumer is allowed to gate on.
//!
//! Deliberately a plain text file of lane ids, one per line, with `#`
//! comments and blank lines tolerated -- not TOML, not JSON, no schema.
//! The list is a set of tokens in the `kind_str()` vocabulary of
//! `ProviderEntry`; a structured format would only add a parse surface
//! and a version field to a file whose entire content is that set.
//!
//! Reading is FAIL-CLOSED in both directions. A list that cannot be
//! read, cannot be parsed, or names no lane at all is an error rather
//! than an empty gated set: an empty set would silently downgrade every
//! gated comparison to report-only, which looks identical to a passing
//! gate. The reciprocal check -- a lane named here with zero fixtures
//! behind it -- belongs to the consumer that walks the corpus.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Error returned while resolving the gated-lane list. Every variant
/// carries the file path so a test can name it.
#[derive(Debug, Error)]
pub enum GatedLaneError {
    #[error("io error reading gated-lane list {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "malformed lane id `{line}` on line {line_no} of {path}: \
         expected a lowercase `kind_str()` token ([a-z0-9-])"
    )]
    MalformedLaneId {
        path: String,
        line_no: usize,
        line: String,
    },
    #[error("duplicate lane id `{lane}` on line {line_no} of {path}")]
    DuplicateLaneId {
        path: String,
        line_no: usize,
        lane: String,
    },
    #[error(
        "gated-lane list {path} names no lane; refusing to report an empty gated set \
         (an empty set makes every gated comparison silently report-only)"
    )]
    NoLanesListed { path: String },
}

/// File name of the gated-lane list, a sibling of the two fixture roots.
pub const GATED_LANES_FILE: &str = "gated_lanes.txt";

/// Path to the committed gated-lane list.
pub fn gated_lanes_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(GATED_LANES_FILE)
}

/// Read and parse the committed gated-lane list.
pub fn read_gated_lanes() -> Result<Vec<String>, GatedLaneError> {
    read_gated_lanes_at(&gated_lanes_path())
}

/// Read and parse a gated-lane list at an explicit path.
pub fn read_gated_lanes_at(path: &Path) -> Result<Vec<String>, GatedLaneError> {
    let text = fs::read_to_string(path).map_err(|e| GatedLaneError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_gated_lanes(&text, &path.display().to_string())
}

/// Parse the list body: one lane id per line, `#` comments and blank
/// lines ignored, surrounding whitespace trimmed.
pub fn parse_gated_lanes(text: &str, path: &str) -> Result<Vec<String>, GatedLaneError> {
    let mut lanes: Vec<String> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !is_lane_id(line) {
            return Err(GatedLaneError::MalformedLaneId {
                path: path.to_string(),
                line_no,
                line: line.to_string(),
            });
        }
        if lanes.iter().any(|l| l == line) {
            return Err(GatedLaneError::DuplicateLaneId {
                path: path.to_string(),
                line_no,
                lane: line.to_string(),
            });
        }
        lanes.push(line.to_string());
    }
    if lanes.is_empty() {
        return Err(GatedLaneError::NoLanesListed {
            path: path.to_string(),
        });
    }
    Ok(lanes)
}

/// Whether `lane` (a fixture's `meta.lane`) is in the gated set.
pub fn is_lane_gated(gated: &[String], lane: &str) -> bool {
    gated.iter().any(|l| l == lane)
}

/// A lane id is a `kind_str()` token: lowercase ASCII alphanumerics and
/// `-`, nothing else. Rejecting anything wider is what makes a stray
/// heading, a TOML table header, or a commented-out entry missing its
/// `#` an error instead of a phantom lane no fixture can ever match.
fn is_lane_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    /// The real `kind_str()` tokens -- the only values that ever appear
    /// in this file, so the accept case is drawn from them rather than
    /// from a toy string.
    const REAL_LANES: &[&str] = &[
        "anthropic-api",
        "openai-compat",
        "openai-responses",
        "bedrock",
        "gemini",
    ];

    #[test]
    fn parses_one_lane_id_per_line_ignoring_blanks_and_comments() {
        let text = "# gated lanes\n\nanthropic-api\n\n  openai-compat  \n# trailing note\n";

        let lanes = parse_gated_lanes(text, "gated_lanes.txt").unwrap();

        assert_eq!(lanes, vec!["anthropic-api", "openai-compat"]);
    }

    #[test]
    fn reports_a_listed_lane_as_gated_and_an_unlisted_lane_as_not_gated() {
        let text = "anthropic-api\nopenai-compat\n";
        let lanes = parse_gated_lanes(text, "gated_lanes.txt").unwrap();

        assert!(is_lane_gated(&lanes, "anthropic-api"));
        assert!(is_lane_gated(&lanes, "openai-compat"));
        assert!(!is_lane_gated(&lanes, "bedrock"));
        assert!(!is_lane_gated(&lanes, ""));
    }

    /// Positive control for the charset rule below: every real lane
    /// token must parse, so the rule cannot be tightened into rejecting
    /// the values the file exists to hold.
    #[test]
    fn accepts_every_real_lane_token() {
        let text = REAL_LANES.join("\n");

        let lanes = parse_gated_lanes(&text, "gated_lanes.txt").unwrap();

        assert_eq!(lanes, REAL_LANES);
    }

    #[test]
    fn rejects_a_malformed_lane_id_naming_the_line() {
        let text = "anthropic-api\n[lanes]\n";

        let err = parse_gated_lanes(text, "gated_lanes.txt").unwrap_err();

        match &err {
            GatedLaneError::MalformedLaneId { line_no, line, .. } => {
                assert_eq!(*line_no, 2);
                assert_eq!(line, "[lanes]");
            }
            other => panic!("expected MalformedLaneId, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_duplicate_lane_id() {
        let text = "anthropic-api\nanthropic-api\n";

        let err = parse_gated_lanes(text, "gated_lanes.txt").unwrap_err();

        assert!(
            matches!(err, GatedLaneError::DuplicateLaneId { line_no: 2, .. }),
            "expected DuplicateLaneId on line 2, got {err:?}",
        );
    }

    /// Fail-closed: a file whose lane set is empty must NOT read as
    /// "nothing is gated" -- that is indistinguishable from a green gate.
    #[test]
    fn fails_closed_when_the_list_names_no_lane() {
        for text in ["", "\n\n", "# populated by the driver task\n"] {
            let err = parse_gated_lanes(text, "gated_lanes.txt").unwrap_err();
            assert!(
                matches!(err, GatedLaneError::NoLanesListed { .. }),
                "expected NoLanesListed for {text:?}, got {err:?}",
            );
        }
    }

    #[test]
    fn fails_closed_when_the_list_file_is_absent() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join(GATED_LANES_FILE);

        let err = read_gated_lanes_at(&missing).unwrap_err();

        match &err {
            GatedLaneError::Io { path, .. } => assert!(
                path.contains(GATED_LANES_FILE),
                "error did not name the list file: {path}",
            ),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn reads_a_lane_list_from_disk() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join(GATED_LANES_FILE);
        fs::write(&path, "# header\nbedrock\n").unwrap();

        let lanes = read_gated_lanes_at(&path).unwrap();

        assert_eq!(lanes, vec!["bedrock"]);
    }

    /// The committed list lives where the reader looks for it.
    #[test]
    fn committed_lane_list_path_is_a_sibling_of_the_fixture_roots() {
        let path = gated_lanes_path();

        assert!(path.exists(), "{} not present", path.display());
        assert_eq!(
            path.parent().unwrap(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        );
    }

    /// Pins the COMMITTED file's parse outcome as an EXACT set, not a
    /// membership check: a `contains` assertion would let a lane be added
    /// to the list silently, and the whole risk of this file is an entry
    /// no committed fixture backs. Asserting the full vector makes every
    /// future lane addition turn this test red, which is the review
    /// moment each entry deserves.
    #[test]
    fn the_committed_lane_list_names_exactly_the_lanes_committed_fixtures_back() {
        let lanes = read_gated_lanes().expect("the committed lane list must parse");

        assert_eq!(lanes, vec!["anthropic-api".to_string()]);
    }
}
