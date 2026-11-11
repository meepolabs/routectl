//! The status WARN of a Bedrock error read: every arm's `body_excerpt`
//! renders the upstream body scrubbed, so a role ARN, an account id or a
//! credential the body names never reaches the line, and the clean text
//! beside it does.

use super::*;
use routectl_testkit::{CapturedEvent, capture_events};

const CLEAN: &str = "SENTINEL-CLEAN-TEXT";

fn status_warn(status: u16, body: &str) -> CapturedEvent {
    let events = capture_events(|| {
        log_bedrock_upstream_error(
            "prov",
            status,
            &UpstreamBody::new(body),
            false,
            &AccessDeniedDetail::default(),
        );
    });
    let warns: Vec<CapturedEvent> = events
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .collect();
    assert_eq!(warns.len(), 1, "one status WARN for {status}: {warns:?}");
    warns.into_iter().next().expect("one WARN")
}

fn has_account_id_run(text: &str) -> bool {
    text.as_bytes()
        .windows(12)
        .any(|w| w.iter().all(u8::is_ascii_digit))
}

#[test]
fn every_non_403_arm_scrubs_a_principal_from_the_excerpt() {
    let body = format!(
        r#"{{"message":"Role arn:aws:iam::123456789012:role/ops-role in account 123456789012 cannot invoke this model. {CLEAN}"}}"#
    );
    let rows: [(&str, u16, &str); 5] = [
        ("401 auth", 401, "bedrock upstream auth rejected"),
        ("400 validation", 400, "bedrock validation error"),
        ("404 other 4xx", 404, "bedrock upstream error"),
        ("500 server", 500, "bedrock upstream 5xx"),
        ("503 server", 503, "bedrock upstream 5xx"),
    ];
    for (name, status, message) in rows {
        let warn = status_warn(status, &body);

        assert_eq!(warn.message, message, "{name}");
        let excerpt = warn.field("body_excerpt").expect("body_excerpt field");
        assert!(
            !excerpt.to_ascii_lowercase().contains("arn:aws"),
            "{name}: ARN in {excerpt}"
        );
        assert!(!has_account_id_run(excerpt), "{name}: account in {excerpt}");
        assert!(excerpt.contains("[redacted]"), "{name}: {excerpt}");
        assert!(
            excerpt.contains(CLEAN),
            "{name}: clean text lost: {excerpt}"
        );
    }
}

/// The scrub runs over the complete body: a credential straddling the
/// 256-character field cap is redacted, not cut into a fragment the scrub
/// no longer recognizes.
#[test]
fn a_credential_across_the_field_cap_is_redacted_not_cut() {
    let key = format!("AKIA{}", "Q".repeat(16));
    let pad = "p".repeat(240 - r#"{"message":""#.len());
    let body = format!(r#"{{"message":"{pad}{key}"}}"#);
    assert!(
        body.find(&key)
            .is_some_and(|at| at < 256 && at + key.len() > 256)
    );

    let warn = status_warn(400, &body);

    let excerpt = warn.field("body_excerpt").expect("body_excerpt field");
    assert!(!excerpt.contains("AKIA"), "{excerpt}");
    assert!(excerpt.contains("[redacted]"), "{excerpt}");
}

/// The 403 excerpt is the access-denied rendering, scrubbed too: an
/// action token that passes the IAM shape check but is credential-shaped
/// does not reach the excerpt, while the rendering around it does.
#[test]
fn the_403_excerpt_is_the_access_denied_rendering_scrubbed() {
    let key = format!("AKIA{}", "Q".repeat(16));
    let body = format!(
        r#"{{"message":"User: arn:aws:iam::123456789012:role/ops-role is not authorized to perform: svc:{key} on resource: x"}}"#
    );

    let warn = status_warn(403, &body);

    assert_eq!(warn.message, "bedrock IAM access denied");
    let excerpt = warn.field("body_excerpt").expect("body_excerpt field");
    assert_eq!(
        excerpt,
        "bedrock access denied: missing IAM action svc:[redacted]"
    );
    assert_eq!(warn.field("principal_present"), Some("true"));
}
