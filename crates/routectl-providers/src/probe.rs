//! Shared single-shot reachability probe for the HTTP-based providers.
//!
//! `routectl doctor` calls `Provider::probe` to answer "can I reach this
//! provider with the credential it holds?" without billing a model call.
//! The three OpenAI/Anthropic-shape egresses share the same mechanics --
//! one GET against a free models-list endpoint, a bounded total timeout,
//! no retry, no fallback -- so the logic lives here once.
//!
//! Reliability contract (BINDING): every transport failure (DNS, connect,
//! TLS, timeout) collapses to `Unreachable`; a 401/403 collapses to
//! `AuthFailed`; a 2xx is `Reachable`. Any other status the endpoint
//! answered (3xx, 404, 429, 5xx) completed a round trip but does not prove
//! the endpoint is a healthy provider, so it maps to `IndeterminateHttp`
//! carrying the status -- a warning, never a silent pass. The probe never
//! panics and never hangs -- the per-request timeout caps the whole
//! exchange -- and never follows a redirect, so one probe is exactly one
//! request.
//!
//! Reason strings are fixed literals: a probe outcome must never carry a
//! token, an api-key, or a credential-bearing URL into an operator-facing
//! message.

use std::time::Duration;

use reqwest::header::HeaderMap;
use routectl_core::ProbeOutcome;

/// Total per-probe timeout (connect + read). Overrides the shared
/// client's 5-minute streaming read timeout for this one exchange so a
/// black-holed endpoint fails the probe fast instead of stalling
/// `doctor`. No retry: a single attempt within this bound.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Issue exactly one GET against `url` with the given auth `headers`,
/// bounded by `timeout`, and classify the result into a `ProbeOutcome`.
///
/// Uses a dedicated client with redirect-following DISABLED so one probe
/// is exactly one request: a `Location` header on the response is never
/// followed (it would multiply the request count and let a hostile
/// endpoint steer the probe to another host -- SSRF). Never retries and
/// never falls back. A transport error (no HTTP response reached us --
/// DNS, connect, TLS, or the timeout tripping) maps to `Unreachable`; an
/// HTTP response (including a 3xx) is handed to [`classify_probe_status`].
pub async fn http_get_probe(
    user_agent: Option<&str>,
    url: &str,
    headers: HeaderMap,
    timeout: Duration,
) -> ProbeOutcome {
    let client = match crate::http_client::build_no_redirect(user_agent) {
        Ok(c) => c,
        Err(_) => return ProbeOutcome::Unreachable("probe client could not be built".into()),
    };
    let request = match client.get(url).headers(headers).timeout(timeout).build() {
        Ok(r) => r,
        Err(_) => return ProbeOutcome::Unreachable("probe request could not be built".into()),
    };
    match client.execute(request).await {
        Ok(resp) => classify_probe_status(resp.status().as_u16()),
        Err(_) => ProbeOutcome::Unreachable("provider endpoint unreachable".into()),
    }
}

/// Map an HTTP status from a reachability probe to a `ProbeOutcome`.
///
/// - 2xx -> `Reachable` (the endpoint answered the free probe).
/// - 401 / 403 -> `AuthFailed` (reached the host, credential rejected).
/// - anything else (3xx, 404, 429, 5xx) -> `IndeterminateHttp`: the
///   request completed a round trip, so the network path and TLS work,
///   but the status does not prove a healthy provider -- a mistyped base
///   URL (404), a redirect the probe does not follow (3xx), rate limiting
///   (429), or an unhealthy upstream (5xx) all land here as a warning
///   rather than a false pass.
pub fn classify_probe_status(status: u16) -> ProbeOutcome {
    if (200..300).contains(&status) {
        ProbeOutcome::Reachable
    } else if status == 401 || status == 403 {
        ProbeOutcome::AuthFailed("provider rejected the probe credential".into())
    } else {
        ProbeOutcome::IndeterminateHttp { status }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn classify_maps_2xx_to_reachable() {
        assert_eq!(classify_probe_status(200), ProbeOutcome::Reachable);
        assert_eq!(classify_probe_status(204), ProbeOutcome::Reachable);
    }

    #[test]
    fn classify_maps_401_and_403_to_auth_failed() {
        assert!(matches!(
            classify_probe_status(401),
            ProbeOutcome::AuthFailed(_)
        ));
        assert!(matches!(
            classify_probe_status(403),
            ProbeOutcome::AuthFailed(_)
        ));
    }

    #[test]
    fn classify_maps_indeterminate_statuses_to_indeterminate_http() {
        // A round trip completed, but none of these statuses prove a
        // healthy provider, so each maps to IndeterminateHttp carrying the
        // status rather than a false Reachable.
        for status in [302u16, 404, 429, 500, 503] {
            assert_eq!(
                classify_probe_status(status),
                ProbeOutcome::IndeterminateHttp { status },
                "status {status} must map to IndeterminateHttp"
            );
        }
    }

    /// A non-responsive endpoint (delay far exceeding the timeout) must
    /// return a typed `Unreachable` within the bound and must NOT hang.
    /// Proven WITHOUT a fixed sleep: the mock delay is longer than the
    /// probe timeout, and the assertion is on the outcome, not on wall
    /// time. A short timeout keeps the test fast.
    #[tokio::test]
    async fn probe_times_out_to_unreachable_without_hanging() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;

        let outcome = http_get_probe(
            None,
            &format!("{}/models", server.uri()),
            HeaderMap::new(),
            Duration::from_millis(150),
        )
        .await;

        assert!(
            matches!(outcome, ProbeOutcome::Unreachable(_)),
            "a delayed endpoint past the timeout must map to Unreachable, got {outcome:?}"
        );
    }

    /// A refused connection maps to `Unreachable`, never a panic. Uses a
    /// closed loopback port (nothing binds `127.0.0.1:1`) so the connect
    /// deterministically fails with ECONNREFUSED. Stands in for DNS /
    /// connect / TLS transport failures, which all surface as a reqwest
    /// transport error the probe folds into `Unreachable`.
    #[tokio::test]
    async fn probe_connection_refused_maps_to_unreachable() {
        let outcome = http_get_probe(
            None,
            "http://127.0.0.1:1/models",
            HeaderMap::new(),
            PROBE_TIMEOUT,
        )
        .await;

        assert!(
            matches!(outcome, ProbeOutcome::Unreachable(_)),
            "a refused connection must map to Unreachable, got {outcome:?}"
        );
    }

    /// The probe must NOT follow a redirect: one probe is exactly one
    /// request. The `/models` mock returns a 302 whose `Location` points
    /// at `/redirected`; the redirect target is mounted with `.expect(0)`
    /// so wiremock fails on drop if the probe ever hops to it. The 302 is
    /// classified in isolation (non-2xx, non-auth -> `IndeterminateHttp`)
    /// rather than being resolved to the target's status.
    #[tokio::test]
    async fn probe_does_not_follow_redirects() {
        let server = MockServer::start().await;
        let target = format!("{}/redirected", server.uri());
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/redirected"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // the probe must NOT follow the Location header.
            .mount(&server)
            .await;

        let outcome = http_get_probe(
            None,
            &format!("{}/models", server.uri()),
            HeaderMap::new(),
            PROBE_TIMEOUT,
        )
        .await;

        assert_eq!(
            outcome,
            ProbeOutcome::IndeterminateHttp { status: 302 },
            "a 302 must be classified in isolation, not followed"
        );
    }
}
