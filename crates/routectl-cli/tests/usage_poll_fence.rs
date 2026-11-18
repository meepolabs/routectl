//! The usage poll cannot reach a vendor from an integration-test build.
//!
//! Integration tests link the library without `cfg(test)`, and many boot a
//! daemon through `serve_on_listener`, which spawns the background usage poll
//! whenever a credential store opens. A fixture that stored a token for a
//! managed login and put a model on it would otherwise have that token sent
//! to the vendor once the seat came due. The poll's HTTP client is fenced in
//! every test build instead: this test fetches through the very client the
//! daemon builds and requires that both vendor hosts were refused at name
//! resolution, so the fence (not some later failure) is what stopped them.

use routectl_cli::server::probe_usage_poll_fence;

#[tokio::test]
async fn the_daemon_poll_client_resolves_no_vendor_host_in_a_test_build() {
    // Act
    let probe = probe_usage_poll_fence().await;

    // Assert
    assert_eq!(
        probe.outcomes,
        vec!["transport", "transport"],
        "neither fetch may reach a vendor: {probe:?}"
    );
    for host in ["api.anthropic.com", "chatgpt.com"] {
        assert!(
            probe.refused_hosts.iter().any(|refused| refused == host),
            "{host} must be refused by the fence, not fail elsewhere: {probe:?}"
        );
    }
}
