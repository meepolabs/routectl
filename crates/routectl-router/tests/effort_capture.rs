//! Welds the effort rejection fixtures to their capture record
//! (`tests/fixtures/effort_capture.json`): every record entry names a
//! committed fixture whose status and verbatim message match the record, whose
//! note names the record's model, and whose sent effort level is readable from
//! the record's `request_shape` at exactly one wire path.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const CAPTURE: &str = include_str!("fixtures/effort_capture.json");
const CAPTURE_DATE: &str = "2026-10-22";

/// Where each dialect's request body carries the effort level.
const EFFORT_PATHS: [&[&str]; 3] = [
    &["output_config", "effort"],
    &["additionalModelRequestFields", "output_config", "effort"],
    &["reasoning", "effort"],
];

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

fn read_fixture(key: &str) -> Value {
    let path = rejections_dir().join(format!("{key}.json"));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: fixture not readable: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: not JSON: {e}", path.display()))
}

/// The upstream message inside a fixture body, read where its dialect puts it.
fn envelope_message(dialect: &str, body: &Value) -> Option<String> {
    let message = match dialect {
        "bedrock" => &body["message"],
        _ => &body["error"]["message"],
    };
    message.as_str().map(str::to_string)
}

fn value_at<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(root, |node, key| node.get(key))
}

/// The effort level a capture's `request_shape` sent, or why it cannot be
/// read: the level must sit at exactly one of the dialect wire paths.
fn sent_effort(request_shape: &Value) -> Result<&str, String> {
    let hits: Vec<&Value> = EFFORT_PATHS
        .iter()
        .filter_map(|path| value_at(request_shape, path))
        .collect();
    match hits.as_slice() {
        [level] => level
            .as_str()
            .ok_or_else(|| format!("effort is not a string: {level}")),
        [] => Err("no effort path in request_shape".into()),
        _ => Err(format!("{} effort paths in request_shape", hits.len())),
    }
}

fn check_entry(key: &str, entry: &Value) -> Result<(), String> {
    let (dialect, _) = key.split_once('/').ok_or("key is not <dialect>/<case>")?;
    let fixture = read_fixture(key);
    if fixture["status"] != entry["status"] {
        return Err(format!(
            "status {} in the fixture, {} in the record",
            fixture["status"], entry["status"]
        ));
    }
    let body: Value = serde_json::from_str(fixture["body"].as_str().ok_or("fixture body")?)
        .map_err(|e| format!("fixture body is not JSON: {e}"))?;
    let message = envelope_message(dialect, &body).ok_or("fixture body has no message")?;
    if Some(message.as_str()) != entry["message"].as_str() {
        return Err(format!(
            "message differs:\n  fixture {message:?}\n  record  {}",
            entry["message"]
        ));
    }
    let model = entry["model"].as_str().ok_or("record model")?;
    let opening = format!("Tier 1, captured on {dialect} ({CAPTURE_DATE}, {model}, ");
    let note = fixture["note"].as_str().ok_or("fixture note")?;
    if !note.starts_with(&opening) {
        return Err(format!("note does not open with {opening:?}"));
    }
    Ok(())
}

#[test]
fn the_capture_record_covers_exactly_the_pinned_cases() {
    // Arrange
    let expected: Vec<&str> = SENT_EFFORT.iter().map(|(key, _)| *key).collect();

    // Act
    let recorded: Vec<String> = capture_cases().into_keys().collect();

    // Assert
    assert_eq!(recorded, expected);
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

    // Act
    let sent: Vec<(String, Result<String, String>)> = cases
        .iter()
        .map(|(key, entry)| {
            let level = sent_effort(&entry["request_shape"]).map(str::to_string);
            (key.clone(), level)
        })
        .collect();

    // Assert
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
    let with_thinking: Vec<&String> = cases
        .iter()
        .filter(|(_, entry)| entry["request_shape"].get("thinking").is_some())
        .map(|(key, _)| key)
        .collect();

    // Assert
    assert_eq!(cases.len(), SENT_EFFORT.len(), "premise: every entry read");
    assert!(
        with_thinking.is_empty(),
        "a capture sent thinking; its tests must read the thinking shape: {with_thinking:?}"
    );
}

#[test]
fn sent_effort_requires_exactly_one_wire_path() {
    // Arrange
    let one = serde_json::json!({"reasoning": {"effort": "high"}});
    let none = serde_json::json!({"thinking": {"type": "adaptive"}});
    let two = serde_json::json!({
        "output_config": {"effort": "high"},
        "reasoning": {"effort": "low"},
    });
    let not_a_string = serde_json::json!({"output_config": {"effort": 3}});

    // Act + Assert
    assert_eq!(sent_effort(&one), Ok("high"));
    assert!(sent_effort(&none).is_err());
    assert!(sent_effort(&two).is_err());
    assert!(sent_effort(&not_a_string).is_err());
}
