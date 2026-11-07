//! The shared OAuth transport client never follows a redirect, so a 3xx
//! from a token endpoint cannot replay a credential-bearing request to the
//! redirect target.

use routectl_testkit::redirect_pin::CrossHostRedirect;

use super::OAuthStore;

#[tokio::test]
async fn shared_client_does_not_follow_a_cross_host_redirect() {
    // Arrange
    let redirect = CrossHostRedirect::start().await;
    let http = OAuthStore::build_http_client().expect("build shared client");

    // Act
    let resp = http
        .get(redirect.origin_uri())
        .send()
        .await
        .expect("origin answers");

    // Assert
    assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
    redirect.assert_target_untouched().await;
}

#[tokio::test]
async fn default_client_follows_the_same_redirect() {
    // Control: the fixture does reach the target when redirects are followed.
    let redirect = CrossHostRedirect::start().await;

    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build default client")
        .get(redirect.origin_uri())
        .send()
        .await
        .expect("redirect is followed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let target_hits = redirect
        .target
        .received_requests()
        .await
        .expect("request recording is enabled");
    assert_eq!(target_hits.len(), 1);
}
