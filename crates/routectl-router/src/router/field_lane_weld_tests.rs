//! Welds each Bedrock Mantle sub-lane's admitted rows to the refusal captured
//! on that sub-lane: for every (egress, path) an allowlist names, the capture
//! record (`tests/fixtures/mantle_capture.json`) holds an entry whose committed
//! fixture the existing grammar on that egress resolves to that row, through
//! the same class gate a live rejection passes, and whose one repaired retry
//! succeeded with the refused field dropped for a drop row, or sent at a
//! different value for every other transform.
//! The census runs the other way too: every record entry names a row its
//! egress admits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use routectl_core::Error;
use routectl_core::failure_class::classify;
use routectl_core::wire_check::EgressKind;
use serde_json::{Value, json};

use super::super::rejection_parser::rejected_wire_path;
use super::super::wire_rejection::effort_and_thinking;
use super::effort::{
    ANTHROPIC_API_MANTLE_ROWS, OPENAI_COMPAT_MANTLE_ROWS, OPENAI_RESPONSES_MANTLE_ROWS,
};
use super::{FieldRepairRow, FieldRepairTable, FieldTransform, rejected_field};

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
    /// A committed fixture that cannot be read or parsed, or whose body has
    /// no `error.message`; `reason` says which.
    UnreadableFixture { key: String, reason: String },
    /// A fixture whose status or message is not the record's.
    FixtureDisagrees { key: String },
    /// A record entry whose model is not the one its request sent, or whose
    /// fixture note does not open with that model and the capture date.
    ModelDisagrees { key: String },
    /// A fixture the grammar on the entry's egress resolves to another row,
    /// or to none.
    ResolvesElsewhere { key: String, got: Option<String> },
    /// A record entry whose repaired retry did not succeed.
    NoRepairedSuccess { key: String },
    /// A record entry whose refused request did not send the field its
    /// refusal names, so the record contradicts its own capture.
    RefusedFieldNotSent { key: String },
    /// A record entry whose repaired body does not drop the refused field
    /// (a drop row) or send it at a different value (every other transform).
    RepairNotApplied { key: String },
}

impl WeldError {
    /// This failure with any free-text reason blanked, so planted rows match
    /// on the cause and key alone.
    fn keyed(self) -> Self {
        match self {
            Self::UnreadableFixture { key, .. } => Self::UnreadableFixture {
                key,
                reason: String::new(),
            },
            other => other,
        }
    }
}

/// What reading a fixture found: `Ok(None)` when no file is committed under
/// the key, `Err` when one is but cannot be read or parsed.
pub(super) type FixtureRead = Result<Option<Value>, String>;

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

/// The `<key>.json` fixture under `dir`.
fn read_fixture(dir: &Path, key: &str) -> FixtureRead {
    let text = match std::fs::read_to_string(dir.join(format!("{key}.json"))) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| err.to_string())
}

/// The committed `rejections/<key>.json`.
pub(super) fn corpus_fixture(key: &str) -> FixtureRead {
    read_fixture(&rejections_dir(), key)
}

/// The upstream error a fixture's body becomes on the egress: its status and
/// body, with the `error.type` and `error.code` tokens the egress lifts from
/// a nested envelope, so the classifier reads what it reads in production.
/// `Err` says which part of the fixture is missing or malformed.
pub(super) fn fixture_error(fixture: &Value) -> Result<(Error, String), String> {
    let status = fixture["status"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .ok_or("no HTTP status")?;
    let body = fixture["body"]
        .as_str()
        .ok_or("no body string")?
        .to_string();
    let envelope: Value =
        serde_json::from_str(&body).map_err(|err| format!("body is not JSON: {err}"))?;
    let token = |field: &str| envelope["error"][field].as_str().map(str::to_owned);
    let message = token("message").ok_or("no error.message")?;
    let err = Error::upstream_full("mantle", status, body, None, token("type"), token("code"));
    Ok((err, message))
}

/// The row the grammar on `egress` names for the refusal `err`, drawn by a
/// request that sent `request_shape`: the live resolver, behind the class
/// gate.
fn resolved_row(
    err: &Error,
    kind: &str,
    egress: EgressKind,
    request_shape: &Value,
) -> Option<&'static FieldRepairRow> {
    let class = classify(err, Some(kind)).class;
    let (sent, _) = effort_and_thinking(egress, request_shape);
    rejected_field(
        FieldRepairTable::grounded_only(),
        &class,
        err,
        kind,
        Some(egress),
        sent,
    )
    .map(|named| named.row)
}

/// What the repaired retry did to the field the refusal `err` locates in
/// `sent`.
#[derive(Debug, PartialEq, Eq)]
enum RepairCheck {
    /// Dropped for a drop row; sent at a different value for every other
    /// transform.
    Applied,
    /// The refused request did not send the field its refusal names.
    NotSent,
    /// The repaired body does not repair the field as `row` does, or the
    /// refusal names no wire path to compare.
    NotApplied,
}

/// Whether `repaired` changed the field the refusal `err` locates in `sent`
/// the way `row` repairs it: dropped for a drop row, sent at a different
/// value for every other transform.
fn repair_applied(
    row: &FieldRepairRow,
    err: &Error,
    kind: &str,
    sent: &Value,
    repaired: &Value,
) -> RepairCheck {
    let Some(rejected) = super::super::class_observe::upstream_facts(err)
        .status
        .and_then(|status| rejected_wire_path(kind, status, err.parsed_body()))
    else {
        return RepairCheck::NotApplied;
    };
    let at = |body: &'_ Value| -> Option<Value> {
        rejected
            .locator
            .segments()
            .try_fold(body, |value, segment| value.get(segment))
            .cloned()
    };
    let (before, after) = (at(sent), at(repaired));
    if before.is_none() {
        return RepairCheck::NotSent;
    }
    let applied = match row.transform {
        FieldTransform::Drop => after.is_none(),
        FieldTransform::Rewrite { .. }
        | FieldTransform::Ceiling
        | FieldTransform::RankedRewrite { .. } => after.is_some() && after != before,
    };
    if applied {
        RepairCheck::Applied
    } else {
        RepairCheck::NotApplied
    }
}

/// Whether the entry's model is the one its request sent, and its fixture's
/// note opens naming that model and the capture date.
fn names_its_capture(dir: &str, entry: &Value, fixture: &Value) -> bool {
    let (Some(model), Some(date)) = (
        entry["model"].as_str(),
        entry["date"].as_str().and_then(|date| date.get(..10)),
    ) else {
        return false;
    };
    let opening = format!("Tier 1, captured on {dir} ({date}, {model}, ");
    entry["request_shape"]["model"].as_str() == Some(model)
        && fixture["note"]
            .as_str()
            .is_some_and(|note| note.starts_with(&opening))
}

/// Check one record entry against its fixture, its grammar and its retry.
fn check_entry(
    key: &str,
    entry: &Value,
    fixture: impl Fn(&str) -> FixtureRead,
) -> Result<(), WeldError> {
    let owned = || key.to_string();
    let dir = key.split_once('/').map_or(key, |(dir, _)| dir);
    let egress = entry["egress"]
        .as_str()
        .and_then(egress_of_label)
        .filter(|egress| sibling_kind(*egress) == Some(dir))
        .ok_or_else(|| WeldError::ForeignEgress { key: owned() })?;
    let unreadable = |reason| WeldError::UnreadableFixture {
        key: owned(),
        reason,
    };
    let fixture = fixture(key)
        .map_err(unreadable)?
        .ok_or_else(|| WeldError::MissingFixture { key: owned() })?;
    let (err, message) = fixture_error(&fixture).map_err(unreadable)?;
    if fixture["status"] != entry["status"] || entry["message"].as_str() != Some(&message) {
        return Err(WeldError::FixtureDisagrees { key: owned() });
    }
    if !names_its_capture(dir, entry, &fixture) {
        return Err(WeldError::ModelDisagrees { key: owned() });
    }
    let resolved = resolved_row(&err, dir, egress, &entry["request_shape"]);
    let row = resolved
        .filter(|row| entry["row"].as_str() == Some(row.path))
        .ok_or_else(|| WeldError::ResolvesElsewhere {
            key: owned(),
            got: resolved.map(|row| row.path.to_string()),
        })?;
    let repaired = entry["repaired_status"].as_u64().unwrap_or_default();
    if !(200..300).contains(&repaired) || !entry["repaired_request_shape"].is_object() {
        return Err(WeldError::NoRepairedSuccess { key: owned() });
    }
    match repair_applied(
        row,
        &err,
        dir,
        &entry["request_shape"],
        &entry["repaired_request_shape"],
    ) {
        RepairCheck::Applied => Ok(()),
        RepairCheck::NotSent => Err(WeldError::RefusedFieldNotSent { key: owned() }),
        RepairCheck::NotApplied => Err(WeldError::RepairNotApplied { key: owned() }),
    }
}

/// Every weld failure of `allowlists` against `record`, and how many record
/// entries were checked.
fn weld(
    allowlists: Allowlists,
    record: &Value,
    fixture: impl Fn(&str) -> FixtureRead + Copy,
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
const CEILING: &str = "anthropic-api/max-tokens-above-model-limit-mantle";
const RANKED: &str = "openai-responses/reasoning-effort-minimal-unsupported-mantle";

/// `key`'s committed entry with `edit` applied.
fn edited_entry(key: &str, edit: impl FnOnce(&mut Value)) -> Value {
    let mut entry = committed_entry(key);
    edit(&mut entry);
    entry
}

/// The committed fixture for `key` with `edit` applied.
fn edited_fixture(key: &str, edit: fn(&mut Value)) -> FixtureRead {
    corpus_fixture(key).map(|fixture| {
        fixture.map(|mut fixture| {
            edit(&mut fixture);
            fixture
        })
    })
}

/// One planted weld: what it plants, the allowlists it is checked against,
/// the record, where its fixtures are read, and the one failure it draws.
type Planted<'a> = (
    &'static str,
    Allowlists<'a>,
    Value,
    &'a dyn Fn(&str) -> FixtureRead,
    WeldError,
);

/// Each way an admission can come loose from its evidence is caught and
/// named, against allowlists of its own, so the weld goes red whatever the
/// production allowlists hold.
#[test]
fn a_planted_admission_without_its_evidence_is_named_by_its_cause() {
    // Arrange
    let compat_top_p: [(EgressKind, &[&str]); 1] = [(EgressKind::OpenaiCompatMantle, &["top_p"])];
    let anthropic_ceiling: [(EgressKind, &[&str]); 1] =
        [(EgressKind::AnthropicApiMantle, &["max_tokens"])];
    let responses_minimal: [(EgressKind, &[&str]); 1] = [(
        EgressKind::OpenaiResponsesMantle,
        &["output_config.effort.minimal"],
    )];
    let top_p_only = || planted_record(&[(TOP_P, committed_entry(TOP_P))]);
    let unparsable = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(unparsable.path().join("openai-compat")).expect("dialect dir");
    std::fs::write(
        unparsable.path().join(format!("{TOP_P}.json")),
        "not a fixture",
    )
    .expect("fixture written");
    let a_directory = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(a_directory.path().join(format!("{TOP_P}.json")))
        .expect("a directory where the fixture file goes");
    let corpus = &corpus_fixture;
    let from_unparsable = &|key: &str| read_fixture(unparsable.path(), key);
    let from_a_directory = &|key: &str| read_fixture(a_directory.path(), key);
    let without_message = &|key: &str| {
        edited_fixture(key, |fixture| {
            fixture["body"] = json!(r#"{"error":{"code":"unsupported_parameter"}}"#);
        })
    };
    let altered_status = &|key: &str| edited_fixture(key, |fixture| fixture["status"] = json!(422));
    let rows: [Planted; 19] = [
        (
            "an allowlist path the record does not hold",
            &[(EgressKind::OpenaiCompatMantle, &["top_p", "temperature"])],
            top_p_only(),
            corpus,
            WeldError::PathMissingFromRecord {
                egress: "openai-compat-mantle".into(),
                path: "temperature".into(),
            },
        ),
        (
            "a record entry no allowlist admits",
            &[(EgressKind::OpenaiCompatMantle, &[])],
            top_p_only(),
            corpus,
            WeldError::RecordNotAdmitted { key: TOP_P.into() },
        ),
        (
            "a record entry with no fixture",
            &compat_top_p,
            planted_record(&[(
                "openai-compat/no-such-capture-mantle",
                committed_entry(TOP_P),
            )]),
            corpus,
            WeldError::MissingFixture {
                key: "openai-compat/no-such-capture-mantle".into(),
            },
        ),
        (
            "a fixture that does not parse",
            &compat_top_p,
            top_p_only(),
            from_unparsable,
            WeldError::UnreadableFixture {
                key: TOP_P.into(),
                reason: String::new(),
            },
        ),
        (
            "a fixture path that is a directory, not a file",
            &compat_top_p,
            top_p_only(),
            from_a_directory,
            WeldError::UnreadableFixture {
                key: TOP_P.into(),
                reason: String::new(),
            },
        ),
        (
            "a fixture whose body has no error.message",
            &compat_top_p,
            top_p_only(),
            without_message,
            WeldError::UnreadableFixture {
                key: TOP_P.into(),
                reason: String::new(),
            },
        ),
        (
            "a fixture whose status is not the record's",
            &compat_top_p,
            top_p_only(),
            altered_status,
            WeldError::FixtureDisagrees { key: TOP_P.into() },
        ),
        (
            "a record model that is not the one its request sent",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| {
                    entry["request_shape"]["model"] = json!("openai.gpt-5.4");
                }),
            )]),
            corpus,
            WeldError::ModelDisagrees { key: TOP_P.into() },
        ),
        (
            "a record model the fixture note does not name",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| {
                    entry["model"] = json!("openai.gpt-5.4");
                    entry["request_shape"]["model"] = json!("openai.gpt-5.4");
                }),
            )]),
            corpus,
            WeldError::ModelDisagrees { key: TOP_P.into() },
        ),
        (
            "a record date the fixture note does not name",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| {
                    entry["date"] = json!("2026-10-24T03:26:42+00:00");
                }),
            )]),
            corpus,
            WeldError::ModelDisagrees { key: TOP_P.into() },
        ),
        (
            "a fixture that resolves to another row",
            &compat_top_p,
            planted_record(&[(
                PRESENCE,
                edited_entry(PRESENCE, |entry| entry["row"] = json!("top_p")),
            )]),
            corpus,
            WeldError::ResolvesElsewhere {
                key: PRESENCE.into(),
                got: Some("presence_penalty".into()),
            },
        ),
        (
            "a record whose repaired retry did not succeed",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| entry["repaired_status"] = json!(400)),
            )]),
            corpus,
            WeldError::NoRepairedSuccess { key: TOP_P.into() },
        ),
        (
            "a drop row whose repaired shape still carries the field, 200",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| {
                    entry["repaired_request_shape"] = entry["request_shape"].clone();
                }),
            )]),
            corpus,
            WeldError::RepairNotApplied { key: TOP_P.into() },
        ),
        (
            "a drop row whose repaired shape rewrites the field, 200",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| {
                    entry["repaired_request_shape"]["top_p"] = json!(0.7);
                }),
            )]),
            corpus,
            WeldError::RepairNotApplied { key: TOP_P.into() },
        ),
        (
            "a ceiling row whose repaired shape omits the limit, 200",
            &anthropic_ceiling,
            planted_record(&[(
                CEILING,
                edited_entry(CEILING, |entry| {
                    entry["repaired_request_shape"]
                        .as_object_mut()
                        .expect("a repaired shape")
                        .remove("max_tokens");
                }),
            )]),
            corpus,
            WeldError::RepairNotApplied {
                key: CEILING.into(),
            },
        ),
        (
            "a record whose refused request did not send the refused field",
            &compat_top_p,
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| {
                    entry["request_shape"]
                        .as_object_mut()
                        .expect("a request shape")
                        .remove("top_p");
                }),
            )]),
            corpus,
            WeldError::RefusedFieldNotSent { key: TOP_P.into() },
        ),
        (
            "a ceiling row whose repaired shape sends the refused limit, 200",
            &anthropic_ceiling,
            planted_record(&[(
                CEILING,
                edited_entry(CEILING, |entry| {
                    entry["repaired_request_shape"] = entry["request_shape"].clone();
                }),
            )]),
            corpus,
            WeldError::RepairNotApplied {
                key: CEILING.into(),
            },
        ),
        (
            "a ranked row whose repaired shape sends the refused level, 200",
            &responses_minimal,
            planted_record(&[(
                RANKED,
                edited_entry(RANKED, |entry| {
                    entry["repaired_request_shape"] = entry["request_shape"].clone();
                }),
            )]),
            corpus,
            WeldError::RepairNotApplied { key: RANKED.into() },
        ),
        (
            "a record entry on a direct egress",
            &[(EgressKind::OpenaiCompat, &["top_p"])],
            planted_record(&[(
                TOP_P,
                edited_entry(TOP_P, |entry| entry["egress"] = json!("openai-compat")),
            )]),
            corpus,
            WeldError::ForeignEgress { key: TOP_P.into() },
        ),
    ];

    for (name, allowlists, record, fixture, expected) in rows {
        // Act
        let (failures, _) = weld(allowlists, &record, fixture);

        // Assert
        for failure in &failures {
            if let WeldError::UnreadableFixture { reason, .. } = failure {
                assert!(!reason.is_empty(), "{name}: an unreadable fixture says why");
            }
        }
        let keyed: Vec<WeldError> = failures.into_iter().map(WeldError::keyed).collect();
        assert_eq!(keyed, vec![expected], "{name}");
    }
}
