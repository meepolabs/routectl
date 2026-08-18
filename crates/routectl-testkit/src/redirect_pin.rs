//! Two-server cross-host redirect pin, shared by every credentialed
//! egress lane's redirect regression test.
//!
//! Every credentialed lane builds a no-redirect client and maps a
//! surfaced `3xx` to a retryable upstream `502`. Both halves are lane-
//! local code, so a lane that hand-rolled its own `Client::builder()`
//! (or dropped its `300..400` arm) would pass every other gate. This
//! harness makes the regression observable: one mock server stands in
//! for the configured host and answers everything with a `302`, a
//! second stands in for a DIFFERENT host and answers everything `200`.
//! A followed redirect therefore looks like a success on the wire --
//! the only thing keeping the second server untouched is the lane's
//! own policy.
//!
//! It lives in this dev-dependency crate rather than a `tests/` support
//! module because the lanes' HTTP tests are split across both
//! compilation shapes (`#[cfg(test)]` in `src/` and `tests/`
//! integration binaries); a dev-dependency crate is the only home that
//! reaches both.

use routectl_core::Error;
use routectl_core::failure_class::{FailureClass, classify};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A pair of mock servers standing in for two distinct hosts.
pub struct CrossHostRedirect {
    /// The configured host. Answers every request with a `302` whose
    /// `Location` points at [`Self::target`].
    pub origin: MockServer,
    /// The redirect target, on a different host than [`Self::origin`].
    /// Answers every request `200`, so a followed redirect would
    /// succeed rather than fail for an unrelated reason.
    pub target: MockServer,
}

impl CrossHostRedirect {
    /// Start both servers and mount the catch-all `302` / `200` pair.
    pub async fn start() -> Self {
        let origin = MockServer::start().await;
        let target = MockServer::start().await;
        // Rewrite the target's host from `127.0.0.1` to `localhost`.
        // Both resolve to the loopback interface, but this pins the
        // cross-HOST case literally rather than merely cross-port, so
        // the assertion still holds if a future policy were to permit
        // same-host redirects.
        let location = format!(
            "{}/redirected",
            target.uri().replacen("127.0.0.1", "localhost", 1)
        );
        Mock::given(any())
            .respond_with(ResponseTemplate::new(302).insert_header("location", location.as_str()))
            .mount(&origin)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&target)
            .await;
        Self { origin, target }
    }

    /// Base URL to configure the lane under test with.
    pub fn origin_uri(&self) -> String {
        self.origin.uri()
    }

    /// Assert the lane refused the cross-host hop: the redirect target
    /// was never dialed, and the lane surfaced the shared
    /// `3xx` -> retryable-upstream-`502` mapping.
    ///
    /// `provider_kind` is the config `kind` string the lane classifies
    /// under (`anthropic-api`, `openai-compat`, `bedrock`, ...).
    pub async fn assert_not_followed(&self, err: &Error, provider_kind: &str) {
        // Ordered so a followed redirect reports the redirect itself
        // rather than whatever the lane made of the target's 200.
        self.assert_target_untouched().await;
        match err {
            Error::Upstream { status, .. } => assert_eq!(
                *status, 502,
                "a 3xx must surface as a mapped upstream server fault, not the bare redirect status"
            ),
            other => panic!("expected Error::Upstream from an unfollowed 302, got {other:?}"),
        }
        assert_eq!(
            classify(err, Some(provider_kind)).class,
            FailureClass::ServerError,
            "a redirect the client refuses to follow must classify (and retry / fail over) like a server fault"
        );
    }

    /// Assert the origin was dialed exactly once and the redirect target
    /// not at all.
    ///
    /// Split out from [`Self::assert_not_followed`] for the lanes whose
    /// endpoint is not overridable, which drive the lane's client
    /// directly and so have no lane-mapped error to classify.
    pub async fn assert_target_untouched(&self) {
        let origin_hits = self
            .origin
            .received_requests()
            .await
            .expect("wiremock request recording must be enabled on the origin");
        assert_eq!(
            origin_hits.len(),
            1,
            "the configured host must see exactly one request"
        );
        let target_hits = self
            .target
            .received_requests()
            .await
            .expect("wiremock request recording must be enabled on the target");
        assert!(
            target_hits.is_empty(),
            "a no-redirect client must not follow the 302 to a different host: the target saw {} request(s)",
            target_hits.len()
        );
    }
}
