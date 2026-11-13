//! Shared WARN emitter for upstream HTTP failures across every egress
//! provider.
//!
//! Each provider's error path needs the same two-branch WARN: a
//! dedicated "auth failed" line on 401/403 (carrying the provider's
//! `auth_kind` for triage) and a generic "upstream error" line for
//! every other 4xx/5xx. Centralizing it keeps the message wording and
//! the auth-vs-error split identical across providers.
//!
//! The tracing event message is a `&'static str` literal -- the
//! provider family and any sub-path (e.g. count-tokens) ride in the
//! `provider` / `context` FIELDS, never interpolated into the message,
//! so log subscribers can filter on a stable message string.

use routectl_core::upstream_text_for_log;

/// Emit the upstream-failure WARN for one provider. Picks the
/// "auth failed" message on 401/403 and the generic "upstream error"
/// message otherwise. Pass the raw upstream text as `body_excerpt`, never
/// a capped or filtered excerpt: it is rendered through
/// [`upstream_text_for_log`], which scrubs credentials from the complete
/// text before control-filtering and capping it.
///
/// `auth_kind` is `Option<&dyn Debug>` because each provider carries
/// its own `AuthKind` enum (and openai-compat has none). `Some(k)`
/// logs the field as the plain unwrapped value (`auth_kind = ?k`,
/// yielding `ApiKey`, not `Some(ApiKey)`); `None` omits the field
/// entirely. `context` is a stable discriminator field (provider
/// family, sub-path) that distinguishes one call site's WARN from
/// another without baking it into the message literal.
pub fn warn_upstream_failure(
    provider_id: &str,
    status: u16,
    auth_kind: Option<&dyn std::fmt::Debug>,
    body_excerpt: &str,
    context: &str,
) {
    let body_excerpt = upstream_text_for_log(body_excerpt);
    let is_auth = status == 401 || status == 403;
    match (is_auth, auth_kind) {
        (true, Some(k)) => tracing::warn!(
            provider = %provider_id,
            status,
            auth_kind = ?k,
            context = %context,
            body_excerpt = %body_excerpt,
            "upstream auth failed",
        ),
        (true, None) => tracing::warn!(
            provider = %provider_id,
            status,
            context = %context,
            body_excerpt = %body_excerpt,
            "upstream auth failed",
        ),
        (false, _) => tracing::warn!(
            provider = %provider_id,
            status,
            context = %context,
            body_excerpt = %body_excerpt,
            "upstream error",
        ),
    }
}

#[cfg(test)]
pub mod tests {
    use super::warn_upstream_failure;
    use routectl_testkit::CapturedEvent;
    use tracing_test::traced_test;

    /// Clean upstream text every planted row carries; it must survive the
    /// scrub verbatim, so a sink that drops the excerpt cannot pass.
    pub const CLEAN_SENTINEL: &str = "upstream-sentinel-kept";

    /// One planted upstream message per row: `(name, message, planted)`.
    /// `planted` is the fragment that must not reach the log line. The
    /// last rows hold a credential the old cap-then-filter order hides
    /// from the scrub: one straddling the 256-character cap, one whose
    /// separator is a non-ASCII no-break space.
    pub fn planted_messages() -> Vec<(&'static str, String, &'static str)> {
        let key_body = "a".repeat(30);
        let pad = "x".repeat(254 - CLEAN_SENTINEL.len() - 2 - "sk-ant-api03-".len());
        vec![
            (
                "principal arn",
                format!("{CLEAN_SENTINEL} denied for arn:aws:iam::123456789012:user/alice"),
                "123456789012",
            ),
            (
                "vendor key",
                format!("{CLEAN_SENTINEL} invalid x-api-key sk-ant-api03-{key_body}"),
                "sk-ant-api03-",
            ),
            (
                "url userinfo",
                format!("{CLEAN_SENTINEL} fetch https://bob:hunter2pass@example.com/x failed"),
                "hunter2pass",
            ),
            (
                "key straddling the cap",
                format!("{CLEAN_SENTINEL} {pad} sk-ant-api03-{key_body}"),
                "sk-ant-api03-",
            ),
            (
                "account id after a no-break space",
                format!("{CLEAN_SENTINEL} account\u{a0}123456789012 is suspended"),
                "123456789012",
            ),
        ]
    }

    /// One planted upstream error BODY per row with no string
    /// `error.message`, so a lane must excerpt the body itself:
    /// `(name, body, planted)`, each in a plain-text, a flat-JSON and an
    /// `{"error":...}` envelope shape.
    /// Each credential sits where an excerpt cut at
    /// [`routectl_core::MAX_LOG_BODY_EXCERPT`] characters before the scrub
    /// would break its rule: a key whose body the cut leaves 10 characters
    /// of (an ARN ahead of it redacts short, pulling the fragment back under
    /// the field cap), and a JWT longer than the cut.
    pub fn planted_bodies_without_message() -> Vec<(String, String, &'static str)> {
        const KEY_AT: usize = routectl_core::MAX_LOG_BODY_EXCERPT - 23;
        let arn = format!("arn:aws:iam::123456789012:role/{}", "r".repeat(300));
        let key = "sk-ant-api03-SECRETBODY0123456789abcdefghij";
        let segment = "abcDEF1234".repeat(30);
        let jwt = format!("eyJ{segment}.{segment}.{}", &segment[..40]);
        let shapes = [
            ("plain text", "", ""),
            (
                "flat json",
                r#"{"__type":"ValidationException","message":""#,
                r#""}"#,
            ),
            (
                "envelope with a non-string message",
                r#"{"error":{"type":"server_error","message":7},"detail":""#,
                r#""}"#,
            ),
        ];
        let mut rows = Vec::new();
        for (shape, open, close) in shapes {
            let head = format!("{open}{CLEAN_SENTINEL} {arn} ");
            let pad = "x".repeat(KEY_AT - head.len() - 1);
            let straddling = format!("{head}{pad} {key} tail{close}");
            assert_eq!(straddling.find(key), Some(KEY_AT), "{shape}: key offset");
            rows.push((
                format!("{shape}/key straddling the excerpt cut"),
                straddling,
                "sk-ant-api03-",
            ));
            let long_jwt = format!("{open}{CLEAN_SENTINEL} rejected token {jwt} tail{close}");
            assert!(
                long_jwt.find(&jwt).expect("jwt planted") + jwt.len()
                    > routectl_core::MAX_LOG_BODY_EXCERPT,
                "{shape}: the jwt must run past the excerpt cut"
            );
            rows.push((
                format!("{shape}/jwt longer than the excerpt cut"),
                long_jwt,
                "eyJ",
            ));
        }
        rows
    }

    /// One planted upstream discriminator (an event, item, block or reason
    /// name) per row: `(name, value, expected rendering, planted)`. The
    /// credential rows must render redacted; the clean rows must render
    /// verbatim, so a sink that drops or blanks the field cannot pass.
    pub fn planted_tags() -> [(&'static str, String, String, Option<&'static str>); 4] {
        const ARN: &str = "arn:aws:iam::123456789012:role/ops-role";
        const KEY: &str = "sk-proj-PlantedDiscriminatorKey0123456789";
        [
            ("arn", ARN.to_owned(), "[redacted]".to_owned(), Some(ARN)),
            (
                "clean beside a key",
                format!("sentinel_tag {KEY}"),
                "sentinel_tag [redacted]".to_owned(),
                Some(KEY),
            ),
            (
                "vendor key",
                KEY.to_owned(),
                "[redacted]".to_owned(),
                Some(KEY),
            ),
            (
                "clean sentinel",
                "sentinel_future_tag".to_owned(),
                "sentinel_future_tag".to_owned(),
                None,
            ),
        ]
    }

    /// The `field` of the single `level` event whose message contains
    /// `message` renders `expected`, and no captured field carries `planted`.
    pub fn assert_tag_scrubbed(
        row: &str,
        events: &[CapturedEvent],
        (level, message): (tracing::Level, &str),
        field: &str,
        expected: &str,
        planted: Option<&str>,
    ) {
        let matched: Vec<_> = events
            .iter()
            .filter(|e| e.level == level && e.message.contains(message))
            .collect();
        assert_eq!(matched.len(), 1, "{row}: {message:?} in {events:?}");
        assert_eq!(matched[0].field(field), Some(expected), "{row}: {field}");
        if let Some(planted) = planted {
            assert!(
                events
                    .iter()
                    .all(|e| e.fields.iter().all(|(_, v)| !v.contains(planted))),
                "{row}: planted value reached a log field: {events:?}"
            );
        }
    }

    /// The excerpt carries the clean sentinel and a redaction, and none of
    /// the planted fragment.
    pub fn assert_excerpt_scrubbed(row: &str, excerpt: Option<&str>, planted: &str) {
        let excerpt = excerpt.unwrap_or_else(|| panic!("{row}: no body_excerpt field"));
        assert!(
            excerpt.contains(CLEAN_SENTINEL),
            "{row}: clean sentinel missing from {excerpt:?}"
        );
        assert!(
            excerpt.contains("[redacted]"),
            "{row}: no redaction in {excerpt:?}"
        );
        assert!(
            !excerpt.contains(planted),
            "{row}: planted {planted:?} reached {excerpt:?}"
        );
    }

    #[derive(Debug)]
    enum FakeAuthKind {
        ApiKey,
    }

    fn with_message<'a>(events: &'a [CapturedEvent], message: &str) -> Vec<&'a CapturedEvent> {
        events.iter().filter(|e| e.message == message).collect()
    }

    #[test]
    fn auth_status_with_auth_kind_logs_plain_enum_not_option() {
        // Arrange + Act: a 401 with an auth_kind present.
        let events = routectl_testkit::capture_events(|| {
            warn_upstream_failure(
                "anthropic:p",
                401,
                Some(&FakeAuthKind::ApiKey),
                "denied",
                "anthropic",
            );
        });

        // Assert: the auth-failed message fires and the field carries the
        // plain unwrapped enum (`ApiKey`), NOT the Debug of the Option
        // (`Some(ApiKey)`).
        let auth = with_message(&events, "upstream auth failed");
        assert_eq!(auth.len(), 1, "captured {events:?}");
        assert_eq!(auth[0].field("auth_kind"), Some("ApiKey"));
    }

    #[test]
    fn auth_status_without_key_omits_field() {
        // Arrange + Act: a 403 from a provider that carries no AuthKind,
        // beside a control that does carry one.
        let events = routectl_testkit::capture_events(|| {
            warn_upstream_failure("openai-compat:p", 403, None, "denied", "openai-compat");
            warn_upstream_failure(
                "anthropic:control",
                403,
                Some(&FakeAuthKind::ApiKey),
                "denied",
                "anthropic",
            );
        });

        // Assert: the control proves the capture records the field when
        // present; the keyless event carries no auth_kind field at all.
        let auth = with_message(&events, "upstream auth failed");
        assert_eq!(auth.len(), 2, "captured {events:?}");
        let control = auth
            .iter()
            .find(|e| e.field("provider") == Some("anthropic:control"))
            .expect("control event captured");
        assert_eq!(control.field("auth_kind"), Some("ApiKey"));
        let keyless = auth
            .iter()
            .find(|e| e.field("provider") == Some("openai-compat:p"))
            .expect("keyless event captured");
        assert_eq!(keyless.field("auth_kind"), None, "{keyless:?}");
    }

    #[test]
    fn non_auth_status_emits_generic_error_message() {
        // Arrange + Act: a 500 carries no auth_kind; a 401 control proves
        // the capture would see the auth message.
        let events = routectl_testkit::capture_events(|| {
            warn_upstream_failure("openai-compat:p", 500, None, "boom", "openai-compat");
            warn_upstream_failure(
                "openai-compat:control",
                401,
                None,
                "denied",
                "openai-compat",
            );
        });

        // Assert: the generic error message fires, NOT the auth one.
        let errors = with_message(&events, "upstream error");
        assert_eq!(errors.len(), 1, "captured {events:?}");
        assert_eq!(errors[0].field("provider"), Some("openai-compat:p"));
        let auth = with_message(&events, "upstream auth failed");
        assert_eq!(auth.len(), 1, "captured {events:?}");
        assert_eq!(auth[0].field("provider"), Some("openai-compat:control"));
    }

    #[test]
    fn every_arm_scrubs_raw_upstream_text_before_capping() {
        let arms: [(&str, u16, Option<&dyn std::fmt::Debug>, &str); 3] = [
            (
                "auth with kind",
                401,
                Some(&FakeAuthKind::ApiKey),
                "upstream auth failed",
            ),
            ("auth without kind", 403, None, "upstream auth failed"),
            ("error", 500, None, "upstream error"),
        ];
        for (arm, status, auth_kind, message) in arms {
            for (row, text, planted) in planted_messages() {
                let events = routectl_testkit::capture_events(|| {
                    warn_upstream_failure("anthropic:p", status, auth_kind, &text, "anthropic");
                });

                let matched = with_message(&events, message);
                assert_eq!(matched.len(), 1, "{arm}/{row}: captured {events:?}");
                assert_excerpt_scrubbed(
                    &format!("{arm}/{row}"),
                    matched[0].field("body_excerpt"),
                    planted,
                );
            }
        }
    }

    #[traced_test]
    #[test]
    fn context_field_distinguishes_call_sites() {
        // Arrange + Act: a sub-path discriminator rides in the field.
        warn_upstream_failure("anthropic:p", 429, None, "slow", "anthropic count_tokens");

        // Assert: the context discriminator reaches the event.
        assert!(logs_contain("anthropic count_tokens"));
    }
}
