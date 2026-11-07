//! Test-only OAuth flow, `oauth://test-loopback`.
//!
//! Lets a dependent crate's tests seed a managed credential and dispatch it
//! to a loopback mock upstream without borrowing a real vendor family, whose
//! token the router contains to that vendor's host. Compiled only under
//! `cfg(any(test, feature = "test-utils"))`, and the feature is enabled from
//! dev-dependencies alone, so a release build has no such provider.
//!
//! It never mints or refreshes a token: both token-endpoint legs return an
//! error without touching the network. Seeded records are used as stored.

use async_trait::async_trait;
use url::Url;

use crate::oauth::providers::{AuthParams, OAuthFlow};
use crate::oauth::types::TokenRecord;
use crate::oauth::{OAuthError, OAuthResult};

pub(super) const PROVIDER_ID: &str = "test-loopback";

const AUTHORIZE_URL: &str = "http://127.0.0.1/test-loopback/authorize";

pub struct TestLoopback;

fn no_token_endpoint() -> OAuthError {
    OAuthError::TokenEndpoint("test-loopback has no token endpoint".into())
}

#[async_trait]
impl OAuthFlow for TestLoopback {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Test loopback"
    }

    fn manual_redirect_url(&self) -> &'static str {
        AUTHORIZE_URL
    }

    fn auth_url(&self, params: &AuthParams<'_>) -> Url {
        let mut url = Url::parse(AUTHORIZE_URL).expect("authorize URL is a constant");
        url.query_pairs_mut()
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("code_challenge", params.challenge)
            .append_pair("state", params.state);
        url
    }

    async fn exchange_code(
        &self,
        _http: &reqwest::Client,
        _code: &str,
        _verifier: &str,
        _state: &str,
        _redirect_uri: &str,
    ) -> OAuthResult<TokenRecord> {
        Err(no_token_endpoint())
    }

    async fn refresh_token(
        &self,
        _http: &reqwest::Client,
        _refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        Err(no_token_endpoint())
    }
}

#[cfg(test)]
#[path = "test_loopback_tests.rs"]
mod test_loopback_tests;
