//! The field-repair learner and the generic capability learner never claim the
//! same rejection: a closed-table row's path, refused in any repair kind's own
//! dialect, resolves no generic capability. Field repair runs first and
//! `continue`s, so an overlap would be decided by arm order rather than by a
//! deliberate precedence -- a row that collides fails here when it is added.

use super::*;
use routectl_core::UpstreamBody;
use routectl_core::failure_class::classify;
use serde_json::json;

use crate::capability_matcher::resolve_requested_capability;

/// The pydantic reason the anthropic-api and bedrock dialects refuse a field
/// with.
const EXTRA_INPUTS: &str = "Extra inputs are not permitted";

/// An anthropic-api `invalid_request_error` refusing `path`.
fn anthropic_rejection(path: &str) -> Error {
    Error::upstream(
        "disjoint-mock",
        400,
        json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": format!("{path}: {EXTRA_INPUTS}")},
        })
        .to_string(),
    )
}

/// A bedrock-runtime `ValidationException` refusing `path`.
fn bedrock_rejection(path: &str) -> Error {
    Error::upstream_with_body(
        "disjoint-mock",
        400,
        UpstreamBody::new(json!({ "message": format!("{path}: {EXTRA_INPUTS}") }).to_string()),
        None,
        Some("ValidationException".to_string()),
        None,
    )
}

/// An openai-compat unsupported-parameter refusal of `path`, with or without
/// the `error.code` that lifts the class out of `BadRequest`.
fn openai_compat_rejection(path: &str, code: Option<&str>) -> Error {
    let mut error = json!({
        "message": format!("Unsupported parameter: '{path}' is not supported with this model."),
        "type": "invalid_request_error",
        "param": path,
    });
    if let Some(code) = code {
        error["code"] = json!(code);
    }
    Error::upstream_with_body(
        "disjoint-mock",
        400,
        UpstreamBody::new(json!({ "error": error }).to_string()),
        None,
        Some("invalid_request_error".to_string()),
        code.map(str::to_string),
    )
}

/// Every repair kind's refusals of `path`, labelled for a failure message.
fn dialect_rejections(path: &str) -> Vec<(&'static str, &'static str, Error)> {
    vec![
        (
            ANTHROPIC_API_KIND,
            "location prefix",
            anthropic_rejection(path),
        ),
        (BEDROCK_KIND, "location prefix", bedrock_rejection(path)),
        (
            BEDROCK_KIND,
            "converse field bag",
            bedrock_rejection(&format!("additionalModelRequestFields.{path}")),
        ),
        (
            OPENAI_COMPAT_KIND,
            "coded",
            openai_compat_rejection(path, Some(OPENAI_UNSUPPORTED_CODE)),
        ),
        (
            OPENAI_COMPAT_KIND,
            "template only",
            openai_compat_rejection(path, None),
        ),
    ]
}

/// Mutation check: add a closed-table row whose path a generic resolver
/// translates (e.g. `output_config.format`, or a bedrock `advisor` row) -> red
/// on that row's kind.
#[test]
fn no_closed_table_row_resolves_a_generic_capability_in_any_repair_kind() {
    for row in closed_table() {
        for (kind, shape, err) in dialect_rejections(row.path) {
            let classified = classify(&err, Some(kind));

            assert!(
                rejected_field(&classified.class, &err, kind).is_some(),
                "control: {kind} ({shape}) names `{}` to the field parser under {:?}",
                row.path,
                classified.class,
            );
            assert_eq!(
                resolve_requested_capability(kind, &err, &classified),
                None,
                "{kind} ({shape}): `{}` is a field-repair row and must resolve no generic \
                 capability",
                row.path,
            );
        }
    }
}
