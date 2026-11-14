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

/// Everything the lane's error path does after the body bytes are read,
/// from the text on.
fn read(status: u16, text: String) -> Error {
    let read =
        UpstreamErrorRead::from_parts(status, &reqwest::header::HeaderMap::new(), text, false);
    map_bedrock_upstream_error("bedrock-invoke", "prov", read)
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
fn access_denied_403_carries_its_envelope_not_the_raw_principal_body() {
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
        r#"{"error":{"message":"bedrock access denied (AccessDeniedException): missing IAM action bedrock:InvokeModel"}}"#
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

/// A carryable 400 envelope whose message ends in `Bearer {token}`, with
/// 200 empty `*_token` keys the credential-key redaction grows, padded so
/// the lane's carried cut keeps exactly `kept` chars of the token.
fn envelope_cut_inside_bearer(token: &str, kept: usize) -> String {
    let envelope = |pad: usize| {
        let mut map = serde_json::Map::new();
        map.insert("__type".into(), "ValidationException".into());
        for i in 0..200 {
            map.insert(format!("k{i:03}_token"), "".into());
        }
        let message = format!("{} Bearer {token}", "x".repeat(pad));
        map.insert("message".into(), message.into());
        Value::Object(map)
    };
    let redacted = routectl_core::redact_prompts_in(&envelope(0)).to_string();
    let token_at = redacted
        .find(token)
        .expect("token in the redacted envelope");
    let pad = MAX_ERROR_BODY_BYTES - "... [truncated]".len() - kept - token_at;
    envelope(pad).to_string()
}

#[test]
fn request_fault_display_of_an_envelope_cut_inside_a_bearer_token_redacts_it() {
    let rows = [
        (
            "no digit, 17 chars kept",
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ",
            17,
        ),
        ("digit, 4 chars kept", "a1b2c3d4e5f6g7h8i9j0", 4),
    ];
    for (row, token, kept) in rows {
        let raw = envelope_cut_inside_bearer(token, kept);
        assert!(
            raw.len() < MAX_ERROR_BODY_BYTES,
            "{row}: raw body under the cut"
        );

        let err = read(400, raw);

        let Error::Upstream { body: carried, .. } = &err else {
            panic!("{row}: expected Error::Upstream, got {err:?}");
        };
        assert!(
            carried.ends_with(&format!(" Bearer {}... [truncated]", &token[..kept])),
            "{row}: the carried cut keeps {kept} token chars"
        );
        let display = err.to_string();
        assert!(
            display.ends_with(" Bearer [redacted]... [truncated]")
                && display.matches("... [truncated]").count() == 1,
            "{row}: {:?}",
            &display[display.len() - 60..]
        );
        assert!(!display.contains(&token[..3]), "{row}: token run survives");
    }
}
