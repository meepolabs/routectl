//! Welds each Bedrock Mantle sub-lane's admitted rows to the refusal captured
//! on that sub-lane: for every (egress, path) an allowlist names, the capture
//! record (`tests/fixtures/mantle_capture.json`) holds an entry whose committed
//! fixture the existing grammar on that egress resolves to that row, through
//! the same class gate a live rejection passes, and whose one repaired retry
//! succeeded. The census runs the other way too: every record entry names a
//! row its egress admits.

use std::collections::BTreeSet;
use std::path::PathBuf;

use routectl_core::Error;
use routectl_core::failure_class::classify;
use routectl_core::wire_check::EgressKind;
use serde_json::{Value, json};

use super::super::wire_rejection::effort_and_thinking;
use super::effort::{
    ANTHROPIC_API_MANTLE_ROWS, OPENAI_COMPAT_MANTLE_ROWS, OPENAI_RESPONSES_MANTLE_ROWS,
};
use super::{FieldRepairTable, rejected_field};

const CAPTURE: &str = include_str!("../../tests/fixtures/mantle_capture.json");

/// Each Mantle egress and the allowlist it carries.
const ALLOWLISTS: [(EgressKind, &[&str]); 3] = [
    (EgressKind::AnthropicApiMantle, ANTHROPIC_API_MANTLE_ROWS),
    (EgressKind::OpenaiCompatMantle, OPENAI_COMPAT_MANTLE_ROWS),
    (
        EgressKind::OpenaiResponsesMantle,
        OPENAI_RESPONSES_MANTLE_ROWS,
    ),
];

/// Each Mantle egress a weld checks, with the paths its allowlist names.
type Allowlists<'a> = &'a [(EgressKind, &'a [&'a str])];

/// Why an allowlist entry or a record entry fails the weld.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WeldError {
    /// An allowlist path no record entry on its egress grounds.
    PathMissingFromRecord { egress: String, path: String },
    /// A record entry on an egress or row no allowlist admits.
    RecordNotAdmitted { key: String },
    /// A record entry whose egress is not a Mantle sub-lane of the dialect
    /// its key's directory names.
    ForeignEgress { key: String },
    /// A record entry naming no committed fixture.
    MissingFixture { key: String },
    /// A fixture whose status or message is not the record's.
    FixtureDisagrees { key: String },
    /// A fixture the grammar on the entry's egress resolves to another row,
    /// or to none.
    ResolvesElsewhere { key: String, got: Option<String> },
    /// A record entry whose repaired retry did not succeed.
    NoRepairedSuccess { key: String },
}

/// The provider kind whose grammar a Mantle egress's refusals are read with.
fn sibling_kind(egress: EgressKind) -> Option<&'static str> {
    match egress {
        EgressKind::AnthropicApiMantle => Some("anthropic-api"),
        EgressKind::OpenaiCompatMantle => Some("openai-compat"),
        EgressKind::OpenaiResponsesMantle => Some("openai-responses"),
        _ => None,
    }
}

fn egress_of_label(label: &str) -> Option<EgressKind> {
    EgressKind::ALL
        .into_iter()
        .find(|kind| kind.label() == label)
}

fn rejections_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rejections")
}

/// The committed `rejections/<key>.json`, or `None` when there is none.
fn corpus_fixture(key: &str) -> Option<Value> {
    let text = std::fs::read_to_string(rejections_dir().join(format!("{key}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// The upstream error a fixture's body becomes on the egress: its status and
/// body, with the `error.type` and `error.code` tokens the egress lifts from
/// a nested envelope, so the classifier reads what it reads in production.
fn fixture_error(fixture: &Value) -> Option<(Error, String)> {
    let status = u16::try_from(fixture["status"].as_u64()?).ok()?;
    let body = fixture["body"].as_str()?.to_string();
    let envelope: Value = serde_json::from_str(&body).ok()?;
    let token = |field: &str| envelope["error"][field].as_str().map(str::to_owned);
    let message = token("message")?;
    let err = Error::upstream_full("mantle", status, body, None, token("type"), token("code"));
    Some((err, message))
}

/// The row the grammar on `egress` names for `fixture`, drawn by a request
/// that sent `request_shape`: the live resolver, behind the class gate.
fn resolved_row(
    fixture: &Value,
    kind: &str,
    egress: EgressKind,
    request_shape: &Value,
) -> Option<&'static str> {
    let (err, _) = fixture_error(fixture)?;
    let class = classify(&err, Some(kind)).class;
    let (sent, _) = effort_and_thinking(egress, request_shape);
    rejected_field(
        FieldRepairTable::grounded_only(),
        &class,
        &err,
        kind,
        Some(egress),
        sent,
    )
    .map(|named| named.row.path)
}

/// Check one record entry against its fixture, its grammar and its retry.
fn check_entry(
    key: &str,
    entry: &Value,
    fixture: impl Fn(&str) -> Option<Value>,
) -> Result<(), WeldError> {
    let owned = || key.to_string();
    let dir = key.split_once('/').map_or(key, |(dir, _)| dir);
    let egress = entry["egress"]
        .as_str()
        .and_then(egress_of_label)
        .filter(|egress| sibling_kind(*egress) == Some(dir))
        .ok_or_else(|| WeldError::ForeignEgress { key: owned() })?;
    let fixture = fixture(key).ok_or_else(|| WeldError::MissingFixture { key: owned() })?;
    let agrees = fixture_error(&fixture).is_some_and(|(_, message)| {
        fixture["status"] == entry["status"] && entry["message"].as_str() == Some(&message)
    });
    if !agrees {
        return Err(WeldError::FixtureDisagrees { key: owned() });
    }
    let got = resolved_row(&fixture, dir, egress, &entry["request_shape"]);
    if got != entry["row"].as_str() {
        return Err(WeldError::ResolvesElsewhere {
            key: owned(),
            got: got.map(str::to_string),
        });
    }
    let repaired = entry["repaired_status"].as_u64().unwrap_or_default();
    if !(200..300).contains(&repaired) || !entry["repaired_request_shape"].is_object() {
        return Err(WeldError::NoRepairedSuccess { key: owned() });
    }
    Ok(())
}

/// Every weld failure of `allowlists` against `record`, and how many record
/// entries were checked.
fn weld(
    allowlists: Allowlists,
    record: &Value,
    fixture: impl Fn(&str) -> Option<Value> + Copy,
) -> (Vec<WeldError>, usize) {
    let cases = record["cases"].as_object().cloned().unwrap_or_default();
    let recorded: BTreeSet<(String, String)> = cases
        .values()
        .map(|entry| {
            (
                entry["egress"].as_str().unwrap_or_default().to_string(),
                entry["row"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let admitted: BTreeSet<(String, String)> = allowlists
        .iter()
        .flat_map(|(egress, paths)| {
            paths
                .iter()
                .map(|path| (egress.label().to_string(), (*path).to_string()))
        })
        .collect();
    let mut failures: Vec<WeldError> = admitted
        .difference(&recorded)
        .map(|(egress, path)| WeldError::PathMissingFromRecord {
            egress: egress.clone(),
            path: path.clone(),
        })
        .collect();
    for (key, entry) in &cases {
        let pair = (
            entry["egress"].as_str().unwrap_or_default().to_string(),
            entry["row"].as_str().unwrap_or_default().to_string(),
        );
        if !admitted.contains(&pair) {
            failures.push(WeldError::RecordNotAdmitted { key: key.clone() });
            continue;
        }
        if let Err(failure) = check_entry(key, entry, fixture) {
            failures.push(failure);
        }
    }
    (failures, cases.len())
}

/// Every admitted row on every Mantle sub-lane is welded to its capture, and
/// the record holds nothing beyond the allowlists.
///
/// Mutation check: add `"temperature"` to `OPENAI_COMPAT_MANTLE_ROWS` -> red
/// (`PathMissingFromRecord`); drop `"top_p"` from it -> red
/// (`RecordNotAdmitted`); drop the Responses Mantle arm of
/// `effort_and_thinking` -> red (`ResolvesElsewhere`, the ranked refusal
/// names no row when the sent level cannot be read).
#[test]
fn every_mantle_allowlist_row_is_welded_to_its_capture() {
    // Arrange
    let record: Value = serde_json::from_str(CAPTURE).expect("the record is JSON");
    let admitted: usize = ALLOWLISTS.iter().map(|(_, paths)| paths.len()).sum();

    // Act
    let (failures, checked) = weld(&ALLOWLISTS, &record, corpus_fixture);

    // Assert
    assert!(admitted > 0, "premise: some row is admitted on Mantle");
    assert_eq!(checked, admitted, "one record entry per admitted row");
    assert_eq!(failures, Vec::new());
}

/// A record shaped like the committed one, holding `entries`.
fn planted_record(entries: &[(&str, Value)]) -> Value {
    let cases: serde_json::Map<String, Value> = entries
        .iter()
        .map(|(key, entry)| ((*key).to_string(), entry.clone()))
        .collect();
    json!({ "cases": cases })
}

/// The committed record's entry for `key`.
fn committed_entry(key: &str) -> Value {
    let record: Value = serde_json::from_str(CAPTURE).expect("the record is JSON");
    record["cases"][key].clone()
}

const TOP_P: &str = "openai-compat/unsupported-parameter-top-p-mantle";
const PRESENCE: &str = "openai-compat/unsupported-parameter-presence-penalty-mantle";

/// Each way an admission can come loose from its evidence is caught and
/// named, against allowlists of its own, so the weld goes red whatever the
/// production allowlists hold.
#[test]
fn a_planted_admission_without_its_evidence_is_named_by_its_cause() {
    let compat_top_p: [(EgressKind, &[&str]); 1] = [(EgressKind::OpenaiCompatMantle, &["top_p"])];
    let missing_fixture = {
        let mut entry = committed_entry(TOP_P);
        entry["row"] = json!("top_p");
        planted_record(&[("openai-compat/no-such-capture-mantle", entry)])
    };
    let resolves_elsewhere = planted_record(&[(PRESENCE, {
        let mut entry = committed_entry(PRESENCE);
        entry["row"] = json!("top_p");
        entry
    })]);
    let failed_retry = planted_record(&[(TOP_P, {
        let mut entry = committed_entry(TOP_P);
        entry["repaired_status"] = json!(400);
        entry
    })]);
    let foreign_egress = planted_record(&[(TOP_P, {
        let mut entry = committed_entry(TOP_P);
        entry["egress"] = json!("openai-compat");
        entry
    })]);
    let rows: [(&str, Allowlists, Value, WeldError); 6] = [
        (
            "an allowlist path the record does not hold",
            &[(EgressKind::OpenaiCompatMantle, &["top_p", "temperature"])],
            planted_record(&[(TOP_P, committed_entry(TOP_P))]),
            WeldError::PathMissingFromRecord {
                egress: "openai-compat-mantle".into(),
                path: "temperature".into(),
            },
        ),
        (
            "a record entry no allowlist admits",
            &[(EgressKind::OpenaiCompatMantle, &[])],
            planted_record(&[(TOP_P, committed_entry(TOP_P))]),
            WeldError::RecordNotAdmitted { key: TOP_P.into() },
        ),
        (
            "a record entry with no fixture",
            &compat_top_p,
            missing_fixture,
            WeldError::MissingFixture {
                key: "openai-compat/no-such-capture-mantle".into(),
            },
        ),
        (
            "a fixture that resolves to another row",
            &compat_top_p,
            resolves_elsewhere,
            WeldError::ResolvesElsewhere {
                key: PRESENCE.into(),
                got: Some("presence_penalty".into()),
            },
        ),
        (
            "a record whose repaired retry did not succeed",
            &compat_top_p,
            failed_retry,
            WeldError::NoRepairedSuccess { key: TOP_P.into() },
        ),
        (
            "a record entry on a direct egress",
            &[(EgressKind::OpenaiCompat, &["top_p"])],
            foreign_egress,
            WeldError::ForeignEgress { key: TOP_P.into() },
        ),
    ];

    for (name, allowlists, record, expected) in rows {
        // Act
        let (failures, _) = weld(allowlists, &record, corpus_fixture);

        // Assert
        assert_eq!(failures, vec![expected], "{name}");
    }
}

/// The weld reads a fixture's message and status against its record entry,
/// and resolves the fixture on the entry's own egress: the same compat
/// refusal names its row on compat Mantle once admitted there, and on no
/// other Mantle egress, so a mismatched egress cannot pass.
#[test]
fn a_fixture_resolves_only_on_the_egress_that_admits_its_row() {
    // Arrange
    let entry = committed_entry(TOP_P);
    let fixture = corpus_fixture(TOP_P).expect("the fixture is committed");
    let mut altered = fixture.clone();
    altered["status"] = json!(422);

    // Act
    let on_compat_mantle = resolved_row(
        &fixture,
        "openai-compat",
        EgressKind::OpenaiCompatMantle,
        &entry["request_shape"],
    );
    let on_responses_mantle = resolved_row(
        &fixture,
        "openai-compat",
        EgressKind::OpenaiResponsesMantle,
        &entry["request_shape"],
    );
    let disagreeing = check_entry(TOP_P, &entry, |_| Some(altered.clone()));

    // Assert
    assert_eq!(on_compat_mantle, Some("top_p"));
    assert_eq!(on_responses_mantle, None);
    assert_eq!(
        disagreeing,
        Err(WeldError::FixtureDisagrees { key: TOP_P.into() })
    );
}
