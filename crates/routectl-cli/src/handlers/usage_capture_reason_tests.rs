//! The terminal error's typed credential reason lands in the row's `extra`
//! JSON as `upstream_reason`; a reasonless error leaves the key absent. A
//! capability every lane refuses lands there too, as its capability key and
//! sanitized lane list.

use super::*;
use routectl_core::{ChatRequest, UpstreamReason};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

fn capture() -> (UsageCapture, UsageWriter) {
    let db_path = crate::test_usage_dir::usage_dir().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path, CHANNEL_CAPACITY, 0, true);
    let req = ChatRequest {
        model: "m".to_string(),
        ..Default::default()
    };
    let draft = build_usage_draft("openai", &req, "req-reason".to_string());
    (
        UsageCapture::new(
            draft,
            handle,
            "ingress-1".to_string(),
            routectl_core::RequestObservation::new(),
        ),
        writer,
    )
}

#[test]
fn a_terminal_error_stamps_its_typed_reason_and_a_reasonless_one_stamps_nothing() {
    let rows: [(&str, Error, Option<&str>); 5] = [
        (
            "upstream expired",
            Error::upstream("p", 401, "expired")
                .with_upstream_reason(Some(UpstreamReason::CredentialExpired)),
            Some("credential-expired"),
        ),
        (
            "upstream invalid",
            Error::upstream("p", 401, "invalid")
                .with_upstream_reason(Some(UpstreamReason::CredentialInvalid)),
            Some("credential-invalid"),
        ),
        (
            "all seats parked",
            Error::Credential {
                detail: "seat credential parked: credential-expired".into(),
                reason: UpstreamReason::CredentialExpired,
                parked_until_ms: Some(1_000),
            },
            Some("credential-expired"),
        ),
        ("reasonless 401", Error::upstream("p", 401, "denied"), None),
        ("auth without reason", Error::Auth("refresh".into()), None),
    ];
    for (case, err, want) in rows {
        let (mut cap, _writer) = capture();

        cap.observe_error(&err);

        let got = cap
            .record
            .extra
            .as_ref()
            .and_then(|extra| extra.get("upstream_reason"))
            .and_then(Value::as_str);
        assert_eq!(got, want, "{case}");
        if want.is_none() {
            assert_eq!(cap.record.extra, None, "{case}: extra stays untouched");
        }
    }
}

#[test]
fn the_reason_stamp_keeps_the_existing_extra_keys() {
    let (mut cap, _writer) = capture();
    cap.mark_stream_stage(StreamStage::PreContentDispatch);

    cap.observe_error(
        &Error::upstream("p", 401, "expired")
            .with_upstream_reason(Some(UpstreamReason::CredentialExpired)),
    );

    assert_eq!(
        cap.record.extra,
        Some(serde_json::json!({
            "stream_stage": "pre_content_dispatch",
            "upstream_reason": "credential-expired",
        })),
    );
}

/// A capability every lane refuses is its own error class, distinct from
/// the override-emptied `not_implemented`, and the persisted pre-dispatch
/// row keeps the `gate_blocked` outcome with the sanitized lane list in
/// `extra`.
#[test]
fn a_capability_refusal_persists_gate_blocked_with_its_class_and_lanes() {
    let db_path = crate::test_usage_dir::usage_dir().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let req = ChatRequest {
        model: "m".to_string(),
        ..Default::default()
    };
    let draft = build_usage_draft("anthropic", &req, "cap-refusal-row".to_string());
    let mut cap = UsageCapture::new(
        draft,
        handle.clone(),
        "ingress-1".to_string(),
        routectl_core::RequestObservation::new(),
    );
    let err = Error::capability_unsupported_everywhere(
        "advisor",
        ["bedrock-a#us-east-1", "bedrock-b#eu\u{1b}[31m"],
    );
    let meta = routectl_router::DispatchMeta::for_alias("m");

    cap.observe_error(&err);
    cap.finalize(outcome_for_dispatch_err(&meta));
    drop(cap);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while handle.counters().persisted() < 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    drop(handle);
    writer.shutdown();

    assert_ne!(
        error_class_of(&err),
        error_class_of(&Error::NotImplemented("a".into(), "d".into()))
    );
    let conn = rusqlite::Connection::open(&db_path).expect("read open");
    let (outcome, error_class, attempts, extra): (String, String, i64, String) = conn
        .query_row(
            "SELECT outcome, error_class, attempt_count, extra FROM requests \
             WHERE request_id='cap-refusal-row'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("row persisted");
    assert_eq!(outcome, "gate_blocked");
    assert_eq!(error_class, "capability_unsupported");
    assert_eq!(attempts, 0);
    let extra: Value = serde_json::from_str(&extra).expect("extra is JSON");
    assert_eq!(
        extra,
        serde_json::json!({
            "unsupported_capability": "advisor",
            "unsupported_lanes": ["bedrock-a#us-east-1", "bedrock-b#eu?[31m"],
        })
    );
}
