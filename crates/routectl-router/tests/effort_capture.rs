//! Welds the effort rejection fixtures to their capture record
//! (`tests/fixtures/effort_capture.json`): every record entry names a
//! committed fixture whose status and verbatim message match the record, whose
//! note names the record's model, whose lane belongs to its directory, and
//! whose sent effort level reads from the record's `request_shape` at its
//! dialect's wire path and no other.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const CAPTURE: &str = include_str!("fixtures/effort_capture.json");
const CAPTURE_DATE: &str = "2026-10-22";

/// The request body shape a capture was sent in, which fixes where its
/// effort level and thinking object sit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    Messages,
    Invoke,
    Converse,
    Responses,
}

const DIALECTS: [Dialect; 4] = [
    Dialect::Messages,
    Dialect::Invoke,
    Dialect::Converse,
    Dialect::Responses,
];

impl Dialect {
    const fn effort_path(self) -> &'static [&'static str] {
        match self {
            Self::Messages | Self::Invoke => &["output_config", "effort"],
            Self::Converse => &["additionalModelRequestFields", "output_config", "effort"],
            Self::Responses => &["reasoning", "effort"],
        }
    }

    /// Responses requests carry no thinking object.
    const fn thinking_path(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Messages | Self::Invoke => Some(&["thinking"]),
            Self::Converse => Some(&["additionalModelRequestFields", "thinking"]),
            Self::Responses => None,
        }
    }

    /// The top-level keys a `request_shape` of this dialect may hold.
    fn shape_roots(self) -> Vec<&'static str> {
        let mut roots = vec![self.effort_path()[0]];
        if let Some(path) = self.thinking_path() {
            roots.push(path[0]);
        }
        roots.dedup();
        roots
    }
}

#[derive(Debug, PartialEq, Eq)]
enum EffortError {
    Absent,
    NotAString,
    AtForeignPath(String),
}

#[derive(Debug, PartialEq, Eq)]
enum RecordError {
    UnknownDirectory(String),
    LaneOutsideDirectory { dir: String, lane: String },
    ForeignRoot(String),
}

/// Every record entry and the effort level its request sent.
const SENT_EFFORT: [(&str, &str); 14] = [
    ("anthropic-api/effort-invalid-value-openrouter", "bogus"),
    ("anthropic-api/effort-invalid-value-opus-4-8", "bogus"),
    (
        "anthropic-api/effort-parameter-unsupported-haiku-4-5",
        "low",
    ),
    ("anthropic-api/effort-xhigh-unsupported-opus-4-5", "xhigh"),
    ("anthropic-api/effort-xhigh-unsupported-sonnet-4-6", "xhigh"),
    ("bedrock/effort-extra-input-invoke-opus-4-5", "xhigh"),
    ("bedrock/effort-invalid-value-invoke-opus-4-5-beta", "xhigh"),
    ("bedrock/effort-unknown-variant-converse-opus-4-8", "bogus"),
    ("bedrock/effort-unknown-variant-invoke-opus-4-8", "bogus"),
    (
        "bedrock/effort-xhigh-unsupported-converse-sonnet-4-6",
        "xhigh",
    ),
    (
        "bedrock/effort-xhigh-unsupported-invoke-sonnet-4-6",
        "xhigh",
    ),
    (
        "openai-responses/reasoning-effort-invalid-value-codex-gpt-5-6",
        "bogus",
    ),
    (
        "openai-responses/reasoning-effort-invalid-value-mantle",
        "bogus",
    ),
    (
        "openai-responses/reasoning-effort-minimal-unsupported-codex",
        "minimal",
    ),
];

fn rejections_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rejections")
}

fn capture_cases() -> BTreeMap<String, Value> {
    let capture: Value = serde_json::from_str(CAPTURE).expect("capture record is JSON");
    capture["cases"]
        .as_object()
        .expect("capture record has a cases object")
        .iter()
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .collect()
}

/// Each fixture directory's carrier lanes and the body shape each sends.
const LANES: [(&str, &str, Dialect); 6] = [
    ("anthropic-api", "anthropic-oauth", Dialect::Messages),
    ("anthropic-api", "openrouter-anthropic", Dialect::Messages),
    ("bedrock", "bedrock-invoke", Dialect::Invoke),
    ("bedrock", "bedrock-converse", Dialect::Converse),
    ("openai-responses", "codex", Dialect::Responses),
    ("openai-responses", "mantle-responses", Dialect::Responses),
];

fn read_fixture(key: &str) -> Result<Value, String> {
    let path = rejections_dir().join(format!("{key}.json"));
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("{}: fixture not readable: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: not JSON: {e}", path.display()))
}

/// The upstream message inside a fixture body, read where its dialect puts it.
fn envelope_message(dir: &str, body: &Value) -> Option<String> {
    let message = match dir {
        "bedrock" => &body["message"],
        _ => &body["error"]["message"],
    };
    message.as_str().map(str::to_string)
}

fn value_at<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(root, |node, key| node.get(key))
}

/// The body shape a record's request was sent in: its key's directory and
/// its lane, which must be one of that directory's lanes.
fn record_dialect(key: &str, entry: &Value) -> Result<Dialect, RecordError> {
    let dir = key.split_once('/').map_or(key, |(dir, _)| dir);
    if !LANES.iter().any(|(d, _, _)| *d == dir) {
        return Err(RecordError::UnknownDirectory(dir.to_string()));
    }
    let lane = entry["lane"].as_str().unwrap_or_default();
    LANES
        .iter()
        .find(|(d, l, _)| *d == dir && *l == lane)
        .map(|(_, _, dialect)| *dialect)
        .ok_or_else(|| RecordError::LaneOutsideDirectory {
            dir: dir.to_string(),
            lane: lane.to_string(),
        })
}

/// A `request_shape` holds only its dialect's effort and thinking roots.
fn check_shape_roots(dialect: Dialect, request_shape: &Value) -> Result<(), RecordError> {
    let roots = dialect.shape_roots();
    match request_shape
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .find(|key| !roots.contains(&key.as_str()))
    {
        Some(foreign) => Err(RecordError::ForeignRoot(foreign.clone())),
        None => Ok(()),
    }
}

/// The effort level a request body sent at its dialect's wire path. A value
/// at any other dialect's effort path is an error, not a fallback.
fn sent_effort(dialect: Dialect, body: &Value) -> Result<&str, EffortError> {
    let own = dialect.effort_path();
    if let Some(foreign) = DIALECTS
        .iter()
        .map(|other| other.effort_path())
        .filter(|path| *path != own)
        .find(|path| value_at(body, path).is_some())
    {
        return Err(EffortError::AtForeignPath(foreign.join(".")));
    }
    let level = value_at(body, own).ok_or(EffortError::Absent)?;
    level.as_str().ok_or(EffortError::NotAString)
}

/// The thinking object a request body sent at its dialect's wire path.
fn thinking_at(dialect: Dialect, body: &Value) -> Option<&Value> {
    value_at(body, dialect.thinking_path()?)
}

fn check_entry(key: &str, entry: &Value) -> Result<(), String> {
    let (dir, _) = key.split_once('/').ok_or("key is not <dir>/<case>")?;
    let dialect = record_dialect(key, entry).map_err(|e| format!("{e:?}"))?;
    check_shape_roots(dialect, &entry["request_shape"]).map_err(|e| format!("{e:?}"))?;
    let fixture = read_fixture(key)?;
    if fixture["status"] != entry["status"] {
        return Err(format!(
            "status {} in the fixture, {} in the record",
            fixture["status"], entry["status"]
        ));
    }
    let body: Value = serde_json::from_str(fixture["body"].as_str().ok_or("fixture body")?)
        .map_err(|e| format!("fixture body is not JSON: {e}"))?;
    let message = envelope_message(dir, &body).ok_or("fixture body has no message")?;
    if Some(message.as_str()) != entry["message"].as_str() {
        return Err(format!(
            "message differs:\n  fixture {message:?}\n  record  {}",
            entry["message"]
        ));
    }
    let model = entry["model"].as_str().ok_or("record model")?;
    let opening = format!("Tier 1, captured on {dir} ({CAPTURE_DATE}, {model}, ");
    let note = fixture["note"].as_str().ok_or("fixture note")?;
    if !note.starts_with(&opening) {
        return Err(format!("note does not open with {opening:?}"));
    }
    Ok(())
}

fn record_sent_effort(key: &str, entry: &Value) -> Result<String, String> {
    let dialect = record_dialect(key, entry).map_err(|e| format!("{e:?}"))?;
    sent_effort(dialect, &entry["request_shape"])
        .map(str::to_string)
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn every_effort_fixture_matches_its_capture_record() {
    // Arrange
    let cases = capture_cases();

    // Act
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(key, entry)| check_entry(key, entry).err().map(|e| format!("{key}: {e}")))
        .collect();

    // Assert
    assert_eq!(cases.len(), SENT_EFFORT.len(), "premise: every entry read");
    assert!(
        failures.is_empty(),
        "fixtures out of step with the capture record:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_capture_names_the_effort_level_it_sent() {
    // Arrange
    let cases = capture_cases();
    let pinned: Vec<&str> = SENT_EFFORT.iter().map(|(key, _)| *key).collect();
    let recorded: Vec<&str> = cases.keys().map(String::as_str).collect();

    // Act
    let sent: Vec<(String, Result<String, String>)> = cases
        .iter()
        .map(|(key, entry)| (key.clone(), record_sent_effort(key, entry)))
        .collect();

    // Assert
    assert_eq!(
        recorded, pinned,
        "premise: the record covers exactly the pinned cases"
    );
    let expected: Vec<(String, Result<String, String>)> = SENT_EFFORT
        .iter()
        .map(|(key, level)| ((*key).to_string(), Ok((*level).to_string())))
        .collect();
    assert_eq!(sent, expected);
}

#[test]
fn no_capture_sent_a_thinking_object() {
    // Arrange
    let cases = capture_cases();

    // Act
    let with_thinking: Vec<String> = cases
        .iter()
        .filter_map(|(key, entry)| match record_dialect(key, entry) {
            Ok(dialect) => thinking_at(dialect, &entry["request_shape"])
                .map(|_| format!("{key}: sent thinking")),
            Err(e) => Some(format!("{key}: {e:?}")),
        })
        .collect();

    // Assert
    assert_eq!(cases.len(), SENT_EFFORT.len(), "premise: every entry read");
    assert!(
        with_thinking.is_empty(),
        "a capture sent thinking; its tests must read the thinking shape: {with_thinking:?}"
    );
}

#[test]
fn sent_effort_reads_only_its_dialects_wire_path() {
    // Arrange
    let rows: [(&str, Dialect, Value, Result<&str, EffortError>); 9] = [
        (
            "messages at its own path",
            Dialect::Messages,
            json!({"output_config": {"effort": "high"}}),
            Ok("high"),
        ),
        (
            "converse at its own path",
            Dialect::Converse,
            json!({"additionalModelRequestFields": {"output_config": {"effort": "low"}}}),
            Ok("low"),
        ),
        (
            "responses at its own path",
            Dialect::Responses,
            json!({"reasoning": {"effort": "high"}}),
            Ok("high"),
        ),
        (
            "converse with effort at the messages path",
            Dialect::Converse,
            json!({"output_config": {"effort": "high"}}),
            Err(EffortError::AtForeignPath("output_config.effort".into())),
        ),
        (
            "converse with effort at both bedrock paths",
            Dialect::Converse,
            json!({
                "output_config": {"effort": "high"},
                "additionalModelRequestFields": {"output_config": {"effort": "high"}},
            }),
            Err(EffortError::AtForeignPath("output_config.effort".into())),
        ),
        (
            "invoke with effort at both bedrock paths",
            Dialect::Invoke,
            json!({
                "output_config": {"effort": "high"},
                "additionalModelRequestFields": {"output_config": {"effort": "high"}},
            }),
            Err(EffortError::AtForeignPath(
                "additionalModelRequestFields.output_config.effort".into(),
            )),
        ),
        (
            "messages with a second level at the responses path",
            Dialect::Messages,
            json!({"output_config": {"effort": "high"}, "reasoning": {"effort": "low"}}),
            Err(EffortError::AtForeignPath("reasoning.effort".into())),
        ),
        (
            "no effort path",
            Dialect::Messages,
            json!({"thinking": {"type": "adaptive"}}),
            Err(EffortError::Absent),
        ),
        (
            "effort not a string",
            Dialect::Invoke,
            json!({"output_config": {"effort": 3}}),
            Err(EffortError::NotAString),
        ),
    ];

    for (name, dialect, body, expected) in rows {
        // Act
        let sent = sent_effort(dialect, &body);

        // Assert
        assert_eq!(sent, expected, "{name}");
    }
}

#[test]
fn thinking_at_reads_its_dialects_wire_path() {
    // Arrange
    let thinking = json!({"type": "adaptive"});
    let top_level = json!({"thinking": {"type": "adaptive"}});
    let converse = json!({"additionalModelRequestFields": {"thinking": {"type": "adaptive"}}});
    let rows: [(&str, Dialect, &Value, Option<&Value>); 6] = [
        (
            "messages top level",
            Dialect::Messages,
            &top_level,
            Some(&thinking),
        ),
        (
            "invoke top level",
            Dialect::Invoke,
            &top_level,
            Some(&thinking),
        ),
        (
            "converse wrapped",
            Dialect::Converse,
            &converse,
            Some(&thinking),
        ),
        ("converse top level", Dialect::Converse, &top_level, None),
        ("invoke wrapped", Dialect::Invoke, &converse, None),
        ("responses top level", Dialect::Responses, &top_level, None),
    ];

    for (name, dialect, body, expected) in rows {
        // Act
        let found = thinking_at(dialect, body);

        // Assert
        assert_eq!(found, expected, "{name}");
    }
}

#[test]
fn record_dialect_takes_only_its_directorys_lanes() {
    // Arrange
    let rows: [(&str, &str, Result<Dialect, RecordError>); 5] = [
        ("bedrock/case", "bedrock-converse", Ok(Dialect::Converse)),
        (
            "anthropic-api/case",
            "openrouter-anthropic",
            Ok(Dialect::Messages),
        ),
        (
            "bedrock/case",
            "codex",
            Err(RecordError::LaneOutsideDirectory {
                dir: "bedrock".into(),
                lane: "codex".into(),
            }),
        ),
        (
            "openai-responses/case",
            "anthropic-oauth",
            Err(RecordError::LaneOutsideDirectory {
                dir: "openai-responses".into(),
                lane: "anthropic-oauth".into(),
            }),
        ),
        (
            "vertex/case",
            "bedrock-invoke",
            Err(RecordError::UnknownDirectory("vertex".into())),
        ),
    ];

    for (key, lane, expected) in rows {
        // Act
        let dialect = record_dialect(key, &json!({ "lane": lane }));

        // Assert
        assert_eq!(dialect, expected, "{key} via {lane}");
    }
}

#[test]
fn request_shape_holds_only_its_dialects_roots() {
    // Arrange
    let rows: [(&str, Dialect, Value, Result<(), RecordError>); 5] = [
        (
            "messages effort and thinking",
            Dialect::Messages,
            json!({"output_config": {}, "thinking": {}}),
            Ok(()),
        ),
        (
            "converse wrapper",
            Dialect::Converse,
            json!({"additionalModelRequestFields": {}}),
            Ok(()),
        ),
        (
            "converse with a top-level output_config",
            Dialect::Converse,
            json!({"additionalModelRequestFields": {}, "output_config": {}}),
            Err(RecordError::ForeignRoot("output_config".into())),
        ),
        (
            "responses with thinking",
            Dialect::Responses,
            json!({"reasoning": {}, "thinking": {}}),
            Err(RecordError::ForeignRoot("thinking".into())),
        ),
        (
            "invoke with an unrelated field",
            Dialect::Invoke,
            json!({"output_config": {}, "max_tokens": 1}),
            Err(RecordError::ForeignRoot("max_tokens".into())),
        ),
    ];

    for (name, dialect, shape, expected) in rows {
        // Act
        let checked = check_shape_roots(dialect, &shape);

        // Assert
        assert_eq!(checked, expected, "{name}");
    }
}
