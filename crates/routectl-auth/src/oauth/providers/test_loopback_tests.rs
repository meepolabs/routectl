//! The test-only flow resolves through `lookup` but stays out of the
//! operator-facing id set, every registry line naming it is gated to tests
//! and `test-utils`, and its token legs fail without a network call.

use super::{PROVIDER_ID, TestLoopback};
use crate::oauth::OAuthError;
use crate::oauth::providers::{OAuthFlow, known_provider_ids, lookup};

const GATE: &str = "#[cfg(any(test, feature = \"test-utils\"))]";

#[test]
fn lookup_resolves_the_test_id_while_known_ids_omit_it() {
    // Act
    let flow = lookup(PROVIDER_ID);

    // Assert
    assert_eq!(
        flow.map(|f| f.provider_id()).ok(),
        Some(PROVIDER_ID),
        "lookup resolves the test-only flow"
    );
    assert!(
        !known_provider_ids().contains(&PROVIDER_ID),
        "the test-only id is never an operator-facing provider"
    );
}

#[test]
fn every_registry_line_naming_the_test_flow_is_gated() {
    // Arrange
    let code: Vec<&str> = include_str!("mod.rs")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect();

    // Act
    let hooks: Vec<(&str, &str)> = code
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("test_loopback"))
        .map(|(i, line)| (*line, if i == 0 { "" } else { code[i - 1].trim() }))
        .collect();

    // Assert
    assert_eq!(
        hooks.len(),
        2,
        "module declaration and lookup arm: {hooks:?}"
    );
    for (line, prev) in hooks {
        assert_eq!(prev, GATE, "`{}` is not directly gated", line.trim());
    }
}

#[tokio::test]
async fn token_legs_fail_without_a_network_call() {
    // Arrange: a client whose every request would fail in transport, so a
    // leg that dialed would surface `Network`, not `TokenEndpoint`.
    let http = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:1").expect("proxy url"))
        .build()
        .expect("build client");

    // Act
    let exchange = TestLoopback
        .exchange_code(&http, "code", "verifier", "state", "http://localhost/cb")
        .await;
    let refresh = TestLoopback.refresh_token(&http, "refresh").await;

    // Assert
    for (leg, result) in [("exchange", exchange), ("refresh", refresh)] {
        assert!(
            matches!(result, Err(OAuthError::TokenEndpoint(_))),
            "{leg}: {result:?}"
        );
    }
}
