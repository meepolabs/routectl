//! End-to-end acceptance for the daemon-mediated learned-capability purge,
//! driven through a REAL spawned `routectl serve` over loopback rather than
//! through a tower service.
//!
//! What this covers that the unit tests cannot: the route is actually reachable
//! on the deployable server's own socket with its real layer stack, the peer
//! extension the loopback refusal reads is actually populated (a bare
//! `axum::serve` would not populate it, and every unit test injects it by
//! hand), and the listener auth layer really does gate the route when tokens
//! are configured.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, ServerAuth, ServerConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;

mod common;

/// The purge route's path on the daemon.
const PURGE_PATH: &str = "/control/capability/purge";

/// A minimal servable config: one anthropic-api provider on a dead local port,
/// one model, one alias. Nothing here dials the upstream -- the purge route
/// never touches a provider.
fn servable_config(auth: Option<ServerAuth>) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("test-key")),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "sonnet".to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "default".to_string(),
        AliasValue::Single("sonnet".to_string()),
    );
    Arc::new(Config {
        server: ServerConfig {
            auth,
            ..ServerConfig::default()
        },
        providers,
        models,
        aliases,
        ..Config::default()
    })
}

/// Spawn a real server on an OS-assigned loopback port and return its base URL
/// once `/health` answers.
async fn spawn(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base_url).await;
    base_url
}

/// POST a purge body at the daemon, optionally with a credential.
async fn purge(base_url: &str, body: Value, token: Option<&str>) -> (reqwest::StatusCode, Value) {
    let client = reqwest::Client::new();
    let mut request = client.post(format!("{base_url}{PURGE_PATH}")).json(&body);
    if let Some(token) = token {
        request = request.header("x-api-key", token);
    }
    let response = request.send().await.expect("daemon answers");
    let status = response.status();
    let json = response.json().await.unwrap_or(Value::Null);
    (status, json)
}

/// The route is reachable on the real server and answers the clean no-op for a
/// key no traffic has taught. A fresh daemon's registry is empty, so this is
/// exactly the no-op case -- and it is what proves the route is WIRED, not just
/// registered: an unwired path would 404 rather than answer this envelope.
#[tokio::test]
async fn the_purge_route_answers_a_clean_no_op_on_a_fresh_daemon() {
    // Arrange
    let base_url = spawn(servable_config(None)).await;

    // Act
    let (status, body) = purge(
        &base_url,
        json!({"state_key": "sonnet", "capability_key": "web_search"}),
        None,
    )
    .await;

    // Assert
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["purged"],
        json!(false),
        "a fresh daemon has learned nothing, so the answer is the clean no-op"
    );
    assert_eq!(body["state_key"], json!("sonnet"));
    assert_eq!(body["capability_key"], json!("web_search"));
}

/// An out-of-vocabulary body is refused by the real server with the one fixed
/// code, so the refusal shape survives the whole layer stack rather than only
/// the handler.
#[tokio::test]
async fn the_real_server_refuses_an_out_of_vocabulary_purge_body() {
    // Arrange
    let base_url = spawn(servable_config(None)).await;

    // Act
    let (status, body) = purge(&base_url, json!({"state_key": "sonnet"}), None).await;

    // Assert
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], json!("invalid_request"));
}

/// With tokens configured the route sits behind the SAME listener auth as
/// `/v1/*` -- no bespoke scheme, and no exemption. An unauthenticated call is
/// challenged; the same call with the credential is served.
///
/// The paired positive is what makes the negative evidence: a 401 on both calls
/// would equally satisfy "auth is enforced" while describing a route that is
/// simply unreachable.
#[tokio::test]
async fn a_token_configured_daemon_gates_the_purge_route_on_the_listener_token() {
    // Arrange
    let token_uri = common::file_ref("listener-token");
    let base_url = spawn(servable_config(Some(ServerAuth {
        tokens: vec![token_uri],
    })))
    .await;
    let body = json!({"state_key": "sonnet", "capability_key": "web_search"});

    // Act + Assert: no credential is challenged.
    let (unauthenticated, _body) = purge(&base_url, body.clone(), None).await;
    assert_eq!(
        unauthenticated,
        reqwest::StatusCode::UNAUTHORIZED,
        "a mutating control route must not be reachable without the listener \
         credential the rest of the surface requires"
    );

    // Act + Assert: the credential is accepted.
    let (authenticated, answer) = purge(&base_url, body, Some("listener-token")).await;
    assert_eq!(
        authenticated,
        reqwest::StatusCode::OK,
        "control: the route must be reachable WITH the credential, or the \
         challenge above proves nothing about auth"
    );
    assert_eq!(answer["purged"], json!(false));
}

/// The real server refuses a browser-shaped simple cross-origin request before
/// mutating anything. Driven over a real socket rather than a tower service, so
/// the content-type gate is proven to sit inside the deployed layer stack.
#[tokio::test]
async fn the_real_server_refuses_a_browser_simple_cross_origin_request() {
    // Arrange
    let base_url = spawn(servable_config(None)).await;
    let client = reqwest::Client::new();

    // Act: `text/plain` plus a foreign Origin -- what a page on another origin
    // can put on the wire with no preflight.
    let response = client
        .post(format!("{base_url}{PURGE_PATH}"))
        .header("content-type", "text/plain;charset=UTF-8")
        .header("origin", "https://evil.example")
        .body(r#"{"state_key":"sonnet","capability_key":"web_search"}"#)
        .send()
        .await
        .expect("daemon answers");

    // Assert
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a simple cross-origin request must be refused by the deployed stack, \
         not merely by the handler in isolation"
    );
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert_eq!(body["error"]["code"], json!("unsupported_media_type"));

    // Control: the same call WITH a JSON content-type is served, so the refusal
    // above is attributable to the content-type and not to the route being
    // unreachable.
    let served = client
        .post(format!("{base_url}{PURGE_PATH}"))
        .header("origin", "https://evil.example")
        .json(&json!({"state_key": "sonnet", "capability_key": "web_search"}))
        .send()
        .await
        .expect("daemon answers");
    assert_eq!(served.status(), reqwest::StatusCode::OK);
}

/// The real server rejects a rebound `Host` before mutating -- the property the
/// content-type guard cannot provide.
///
/// An attacker who controls a hostname can point it at 127.0.0.1, after which a
/// page on `http://rebind.evil` is SAME-ORIGIN with the daemon: no preflight is
/// needed, so it may send `application/json` freely, and its peer really is
/// loopback. Only the `Host` header still names the attacker. Driven over a real
/// socket with the JSON content-type the rebound page would use, so the refusal
/// is attributable to Host validation and nothing else.
#[tokio::test]
async fn the_real_server_rejects_a_rebound_host_before_mutating() {
    // Arrange
    let base_url = spawn(servable_config(None)).await;
    let client = reqwest::Client::new();
    let body = json!({"state_key": "sonnet", "capability_key": "web_search"});

    // Act: a correct JSON request whose Host carries an attacker's name.
    let response = client
        .post(format!("{base_url}{PURGE_PATH}"))
        .header("host", "rebind.evil")
        .header("origin", "http://rebind.evil")
        .json(&body)
        .send()
        .await
        .expect("daemon answers");

    // Assert
    assert_eq!(
        response.status(),
        reqwest::StatusCode::FORBIDDEN,
        "a rebound Host must be refused by the deployed stack; the JSON \
         content-type here is exactly what a same-origin rebound page can send"
    );
    let refused: Value = response.json().await.unwrap_or(Value::Null);
    assert_eq!(refused["error"]["code"], json!("forbidden_host"));

    // Control: the identical request with the daemon's own authority is served,
    // so the refusal above is about the Host and not about the route.
    let served = client
        .post(format!("{base_url}{PURGE_PATH}"))
        .json(&body)
        .send()
        .await
        .expect("daemon answers");
    assert_eq!(served.status(), reqwest::StatusCode::OK);
    let ok: Value = served.json().await.unwrap_or(Value::Null);
    assert_eq!(ok["purged"], json!(false));
}
