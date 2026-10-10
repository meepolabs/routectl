//! The Bedrock error reader parses the upstream body once, within the core
//! parse cap, and the error carries the parse of the text it carries.
//!
//! The parse counter is thread-local, so each count is taken around the
//! synchronous half of the reader, with no subscriber on the thread: the
//! DEBUG body line formats its own redacted copy only when DEBUG is enabled.

use super::*;
use routectl_core::MAX_ERROR_BODY_BYTES;
use routectl_core::test_utils::capped_parse_count;

const ARN_SENTINEL: &str = "arn:aws:iam::999988887777:role/SentinelRole";

fn parses_during<T>(run: impl FnOnce() -> T) -> (T, usize) {
    tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
        let before = capped_parse_count();
        let out = run();
        (out, capped_parse_count() - before)
    })
}

/// Everything `read_error_body` and its caller do after the body bytes are
/// read, from the text on.
fn read(status: u16, text: String) -> Error {
    let body = UpstreamBody::new(text);
    let (upstream_type, upstream_code) =
        classify_and_log_error_body("bedrock-invoke", "prov", status, &body, false, None, None);
    build_client_error(
        "prov",
        status,
        None,
        body,
        false,
        upstream_type,
        upstream_code,
    )
}

fn validation_envelope(padding: usize) -> String {
    serde_json::json!({
        "__type": "com.amazon.coral.validate#ValidationException",
        "message": format!("extraneous key [bogus_field] is not permitted{}", " ".repeat(padding)),
    })
    .to_string()
}

#[test]
fn in_cap_validation_envelope_is_parsed_once_and_carried() {
    let body = validation_envelope(0);

    let (err, parses) = parses_during(|| read(400, body.clone()));

    assert_eq!(parses, 1);
    let carried = err
        .parsed_body()
        .expect("in-cap envelope carries its parse");
    assert_eq!(
        carried.value().get("message").and_then(Value::as_str),
        Some("extraneous key [bogus_field] is not permitted")
    );
    let Error::Upstream {
        body: carried_text,
        upstream_type,
        ..
    } = &err
    else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(carried_text, &body);
    assert_eq!(upstream_type.as_deref(), Some("ValidationException"));
}

#[test]
fn over_cap_validation_envelope_is_never_parsed() {
    let body = validation_envelope(MAX_ERROR_BODY_BYTES);
    assert!(body.len() > MAX_ERROR_BODY_BYTES);

    let (err, parses) = parses_during(|| read(400, body));

    assert_eq!(parses, 0);
    assert!(err.parsed_body().is_none());
    let Error::Upstream {
        body: carried_text,
        upstream_type,
        ..
    } = &err
    else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(upstream_type, &None);
    assert!(
        !carried_text.contains("bogus_field"),
        "an over-cap body must not be carried raw: {} bytes",
        carried_text.len()
    );
}

#[test]
fn access_denied_403_carries_no_parse_of_the_raw_principal_body() {
    let raw = serde_json::json!({
        "__type": "com.amazonaws.bedrock#AccessDeniedException",
        "message": format!(
            "User: {ARN_SENTINEL} is not authorized to perform: bedrock:InvokeModel on resource: {ARN_SENTINEL}"
        ),
    })
    .to_string();
    assert!(
        UpstreamBody::new(raw.clone())
            .parsed()
            .is_some_and(|p| p.value().to_string().contains(ARN_SENTINEL)),
        "the raw body's parse must hold the sentinel for this check to bite"
    );

    let (err, parses) = parses_during(|| read(403, raw));

    assert_eq!(parses, 1);
    let Error::Upstream {
        body: carried_text,
        upstream_type,
        ..
    } = &err
    else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(
        carried_text,
        "bedrock access denied: missing IAM action bedrock:InvokeModel"
    );
    assert_eq!(upstream_type.as_deref(), Some("AccessDeniedException"));
    let carried_parse = err.parsed_body().map(|p| p.value().to_string());
    assert!(
        !carried_parse
            .as_deref()
            .is_some_and(|v| v.contains(ARN_SENTINEL)),
        "the carried parse leaked the principal ARN: {carried_parse:?}"
    );
}

/// The BOM-prefixed row pins the source of the message: the carried parse
/// strips a leading BOM under the shared body rules, while a fresh
/// `serde_json` parse of the stored text rejects it.
#[test]
fn beta_repair_reads_the_carried_parse_without_parsing_again() {
    let message = "Unexpected value(s) `zz-probe-2099-01-01` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
    let envelope = serde_json::json!({
        "__type": "com.amazon.coral.validate#ValidationException",
        "message": message,
    })
    .to_string();
    let client = vec!["zz-probe-2099-01-01".to_string()];
    for (name, text) in [
        ("plain", envelope.clone()),
        ("bom-prefixed", format!("\u{feff}{envelope}")),
    ] {
        let err = parses_during(|| read(400, text)).0;

        let (flags, parses) = parses_during(|| {
            beta_repair::repairable_rejected_betas("prov", "bedrock-invoke", &err, &client, &[])
        });

        assert_eq!(parses, 0, "{name}");
        assert_eq!(flags.as_ref(), Some(&client), "{name}");
    }
}
