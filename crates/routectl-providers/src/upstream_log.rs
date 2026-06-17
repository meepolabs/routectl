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

/// Emit the upstream-failure WARN for one provider. Picks the
/// "auth failed" message on 401/403 and the generic "upstream error"
/// message otherwise. The caller MUST pass a `body_excerpt` already
/// run through `sanitize_for_log` -- the upstream body may carry
/// attacker-controlled bytes (CRLF, control chars) that would forge
/// log lines otherwise.
///
/// `auth_kind` is `Option<&dyn Debug>` because each provider carries
/// its own `AuthKind` enum (and openai-compat has none). `Some(k)`
/// logs the field as the plain unwrapped value (`auth_kind = ?k`,
/// yielding `ApiKey`, not `Some(ApiKey)`); `None` omits the field
/// entirely. `context` is a stable discriminator field (provider
/// family, sub-path) that distinguishes one call site's WARN from
/// another without baking it into the message literal.
pub(crate) fn warn_upstream_failure(
    provider_id: &str,
    status: u16,
    auth_kind: Option<&dyn std::fmt::Debug>,
    body_excerpt: &str,
    context: &str,
) {
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
mod tests {
    use super::warn_upstream_failure;
    use tracing_test::traced_test;

    #[derive(Debug)]
    enum FakeAuthKind {
        ApiKey,
    }

    #[traced_test]
    #[test]
    fn auth_status_with_auth_kind_logs_plain_enum_not_option() {
        // Arrange + Act: a 401 with an auth_kind present.
        warn_upstream_failure(
            "anthropic:p",
            401,
            Some(&FakeAuthKind::ApiKey),
            "denied",
            "anthropic",
        );

        // Assert: the auth-failed message fires and the field carries the
        // plain unwrapped enum (`auth_kind=ApiKey`), NOT the Debug of the
        // Option (`Some(ApiKey)`).
        assert!(logs_contain("upstream auth failed"));
        assert!(logs_contain("auth_kind=ApiKey"));
        assert!(!logs_contain("Some(ApiKey)"));
    }

    #[traced_test]
    #[test]
    fn auth_status_without_key_omits_field() {
        // Arrange + Act: a 403 from a provider that carries no AuthKind.
        warn_upstream_failure("openai-compat:p", 403, None, "denied", "openai-compat");

        // Assert: the auth-failed message fires but no auth_kind field is
        // emitted at all. (Match the `auth_kind=` field token, not a bare
        // substring -- tracing-test's buffer also holds the span/test name.)
        assert!(logs_contain("upstream auth failed"));
        assert!(!logs_contain("auth_kind="));
    }

    #[traced_test]
    #[test]
    fn non_auth_status_emits_generic_error_message() {
        // Arrange + Act: a 500 carries no auth_kind.
        warn_upstream_failure("openai-compat:p", 500, None, "boom", "openai-compat");

        // Assert: the generic error message fires, NOT the auth one.
        assert!(logs_contain("upstream error"));
        assert!(!logs_contain("upstream auth failed"));
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
