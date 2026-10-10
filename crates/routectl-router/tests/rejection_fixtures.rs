//! Every committed rejection fixture is well-formed: the `{note, status,
//! lifted_type?, body}` shape `scripts/capture-rejection.sh` writes, under a
//! dialect directory from its closed set, with a note that states its
//! evidence tier and a body that is the dialect's real error envelope.
//!
//! Parser tests read these files as ground truth, so a fixture whose body is
//! not the envelope the dialect actually returns would ground a parser on an
//! invented shape.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const DIALECTS: [&str; 4] = [
    "anthropic-api",
    "bedrock",
    "openai-compat",
    "openai-responses",
];

const FIXTURE_KEYS: [&str; 4] = ["note", "status", "lifted_type", "body"];

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rejections")
}

/// `(dialect, path)` for every file under the corpus root, sorted so a
/// failure names the same file on every run.
fn corpus_files() -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    for dir in fs::read_dir(corpus_root()).expect("rejection corpus root is readable") {
        let dir = dir.expect("corpus dir entry").path();
        assert!(
            dir.is_dir(),
            "{}: only dialect directories belong at the corpus root",
            dir.display()
        );
        let dialect = dir.file_name().unwrap().to_string_lossy().into_owned();
        for file in fs::read_dir(&dir).expect("dialect dir is readable") {
            files.push((dialect.clone(), file.expect("fixture dir entry").path()));
        }
    }
    files.sort();
    files
}

fn str_at<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn non_empty_str(obj: &Map<String, Value>, key: &str) -> bool {
    str_at(obj, key).is_some_and(|s| !s.trim().is_empty())
}

fn null_or_str(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key).is_none_or(|v| v.is_null() || v.is_string())
}

/// Anthropic Messages API: `{"type":"error","error":{"type","message"}}`.
fn is_anthropic_envelope(body: &Value) -> bool {
    let Some(top) = body.as_object() else {
        return false;
    };
    let Some(error) = top.get("error").and_then(Value::as_object) else {
        return false;
    };
    str_at(top, "type") == Some("error")
        && non_empty_str(error, "type")
        && non_empty_str(error, "message")
}

/// bedrock-runtime: the flat `{"message":...}` body, the exception type in a
/// header or an optional top-level `__type`, never a nested `error` object.
fn is_bedrock_envelope(body: &Value) -> bool {
    let Some(top) = body.as_object() else {
        return false;
    };
    non_empty_str(top, "message")
        && top.keys().all(|k| k == "message" || k == "__type")
        && null_or_str(top, "__type")
}

/// OpenAI Chat Completions and Responses share `{"error":{message, type,
/// param, code}}` with nothing beside `error`; only `message` is guaranteed,
/// the rest may be null.
fn is_openai_envelope(body: &Value) -> bool {
    let Some(top) = body.as_object() else {
        return false;
    };
    let Some(error) = top.get("error").and_then(Value::as_object) else {
        return false;
    };
    top.len() == 1
        && non_empty_str(error, "message")
        && ["type", "param", "code"]
            .iter()
            .all(|k| null_or_str(error, k) || error.get(*k).is_some_and(Value::is_number))
}

fn envelope_matches_dialect(dialect: &str, body: &Value) -> bool {
    match dialect {
        "anthropic-api" => is_anthropic_envelope(body),
        "bedrock" => is_bedrock_envelope(body),
        "openai-compat" | "openai-responses" => is_openai_envelope(body),
        _ => false,
    }
}

/// Tier 1 names the dialect it was captured on; tier 2 cites a public URL.
fn note_states_tier(dialect: &str, note: &str) -> bool {
    note.starts_with(&format!("Tier 1, captured on {dialect} "))
        || (note.starts_with("Tier 2, vendor-documented") && note.contains("https://"))
}

fn check_fixture(dialect: &str, text: &str) -> Result<(), String> {
    let fixture: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let obj = fixture.as_object().ok_or("fixture is not an object")?;
    if let Some(extra) = obj.keys().find(|k| !FIXTURE_KEYS.contains(&k.as_str())) {
        return Err(format!("unexpected key `{extra}`"));
    }
    let note = str_at(obj, "note").ok_or("`note` missing or not a string")?;
    if !note_states_tier(dialect, note) {
        return Err("note does not open with its evidence tier".into());
    }
    let status = obj
        .get("status")
        .and_then(Value::as_u64)
        .ok_or("`status` missing or not an integer")?;
    if !(400..600).contains(&status) {
        return Err(format!("status {status} is not an error status"));
    }
    if obj.contains_key("lifted_type") && !non_empty_str(obj, "lifted_type") {
        return Err("`lifted_type` present but not a non-empty string".into());
    }
    let body_text = str_at(obj, "body").ok_or("`body` missing or not a string")?;
    let body: Value =
        serde_json::from_str(body_text).map_err(|e| format!("body is not JSON: {e}"))?;
    if !envelope_matches_dialect(dialect, &body) {
        return Err(format!("body is not the {dialect} error envelope"));
    }
    Ok(())
}

#[test]
fn every_rejection_fixture_is_a_well_formed_dialect_envelope() {
    // Arrange
    let files = corpus_files();

    // Act
    let failures: Vec<String> = files
        .iter()
        .filter_map(|(dialect, path)| {
            let verdict = if !DIALECTS.contains(&dialect.as_str()) {
                Err(format!("`{dialect}` is not a capture dialect"))
            } else if path.extension().is_none_or(|e| e != "json") {
                Err("not a .json file".to_string())
            } else {
                check_fixture(
                    dialect,
                    &fs::read_to_string(path).expect("fixture is UTF-8"),
                )
            };
            verdict.err().map(|e| format!("{}: {e}", path.display()))
        })
        .collect();

    // Assert
    assert!(
        !files.is_empty(),
        "the rejection corpus is empty; nothing was checked"
    );
    assert!(
        failures.is_empty(),
        "malformed rejection fixtures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn a_body_in_another_dialects_envelope_is_rejected() {
    // Arrange: each dialect's real shape, filed under every other dialect.
    let anthropic = r#"{"type":"error","error":{"type":"invalid_request_error","message":"m"}}"#;
    let bedrock = r#"{"message":"m"}"#;
    let openai =
        r#"{"error":{"message":"m","type":"invalid_request_error","param":"p","code":"c"}}"#;
    let fixture = |body: &str| {
        serde_json::json!({"note": "Tier 2, vendor-documented, https://example.com", "status": 400, "body": body})
            .to_string()
    };
    let misfiled = [
        ("anthropic-api", bedrock),
        ("anthropic-api", openai),
        ("bedrock", anthropic),
        ("bedrock", openai),
        ("openai-compat", anthropic),
        ("openai-compat", bedrock),
        ("openai-responses", anthropic),
        ("openai-responses", bedrock),
    ];

    // Act + Assert
    for (dialect, body) in misfiled {
        assert!(
            check_fixture(dialect, &fixture(body)).is_err(),
            "{dialect} accepted {body}"
        );
    }
    for (dialect, body) in [
        ("anthropic-api", anthropic),
        ("bedrock", bedrock),
        ("openai-compat", openai),
    ] {
        assert_eq!(
            check_fixture(dialect, &fixture(body)),
            Ok(()),
            "{dialect} refused its own shape"
        );
    }
}

#[test]
fn a_note_without_its_evidence_tier_is_rejected() {
    // Arrange
    let fixture = |note: &str| {
        serde_json::json!({"note": note, "status": 400, "body": r#"{"message":"m"}"#}).to_string()
    };

    // Act + Assert
    assert!(check_fixture("bedrock", &fixture("Captured on bedrock.")).is_err());
    assert!(
        check_fixture(
            "bedrock",
            &fixture("Tier 1, captured on anthropic-api (x).")
        )
        .is_err()
    );
    assert!(check_fixture("bedrock", &fixture("Tier 2, vendor-documented, no source.")).is_err());
    assert_eq!(
        check_fixture("bedrock", &fixture("Tier 1, captured on bedrock (x).")),
        Ok(())
    );
}
