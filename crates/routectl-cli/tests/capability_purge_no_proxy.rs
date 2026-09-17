//! Process-isolated proof that the capability-purge control call bypasses any
//! configured HTTP proxy.
//!
//! # Why this is its own test binary, with no shared harness
//!
//! The only way to observe reqwest's default proxy behavior is to actually set
//! `HTTP_PROXY` / `ALL_PROXY`, and the environment is per-PROCESS, not per-test.
//! Inside the crate's lib test binary those variables are visible to every
//! sibling running concurrently: this check was originally written there with
//! `#[serial_test::serial]`, and it still redirected an unrelated
//! proxy-forwarding test's reqwest calls to the mock -- reproducing 4 times in 6
//! runs at `--test-threads=512`. `#[serial]` serializes only against OTHER
//! serial-marked tests, which is not isolation from a suite that makes HTTP
//! calls.
//!
//! So this binary deliberately contains ONE test and its own minimal helpers.
//! It does NOT pull in the shared `mod common` harness: that module compiles ~190
//! of its own tests into every binary that declares it, and every one of them
//! would then run inside this process with the proxy variables exported -- which
//! is the very contamination this file exists to escape. A local `file://`
//! secret-ref helper is a few lines; importing the harness would undo the
//! isolation.

use std::io::Write;

use routectl_router::{Config, ServerAuth, ServerConfig};
use serde_json::json;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The control route's path on the daemon.
const PURGE_PATH: &str = "/control/capability/purge";

/// A `file://` secret ref resolving to `value`, plus the guard that owns the
/// backing file.
///
/// The `NamedTempFile` is RETURNED rather than leaked: the caller keeps it in
/// scope for as long as the ref must resolve, and its `Drop` removes the file.
/// `mem::forget` would have left a stray 0600 file carrying a token-shaped value
/// in the temp directory after every run of this binary -- which is exactly the
/// kind of residue a test that exists to protect a credential should not create.
///
/// Local rather than the shared test helper for the isolation reason in the
/// module docs. `literal:` refs are rejected at parse and resolve, so a
/// resolvable listener token has to come from a real 0600 file.
fn file_ref(value: &str) -> (String, tempfile::NamedTempFile) {
    let mut file = tempfile::NamedTempFile::new().expect("create token file");
    file.write_all(value.as_bytes()).expect("write token");
    file.flush().expect("flush token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    let uri = format!("file://{}", file.path().display());
    (uri, file)
}

/// A config whose server address points at `base_url`'s host and port.
fn config_pointing_at(base_url: &str) -> Config {
    let authority = base_url
        .strip_prefix("http://")
        .expect("mock server base url is http");
    let (host, port) = authority
        .rsplit_once(':')
        .expect("mock server base url carries a port");
    Config {
        server: ServerConfig {
            host: host.to_string(),
            port: port.parse().expect("mock port parses"),
            ..ServerConfig::default()
        },
        ..Config::default()
    }
}

/// The daemon's success envelope.
fn purge_response() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "purged": true,
        "state_key": "sonnet",
        "capability_key": "web_search",
    })
}

/// A configured HTTP / ALL proxy must receive NOTHING from a control call.
///
/// A control call is a loopback conversation with a process on this machine.
/// Routing it through an operator's outbound web proxy would hand the listener
/// token, and the names of the operator's own targets, to an unrelated hop --
/// silently, on any box where those variables happen to be exported, because
/// reqwest reads them by default.
///
/// `NO_PROXY` / `no_proxy` are explicitly UNSET for the duration. An inherited
/// `NO_PROXY` covering localhost would make reqwest skip the proxy for its own
/// reasons, so the test would pass on a machine that has one whether or not the
/// code carries `.no_proxy()` -- a vacuous pass that depends on the developer's
/// shell. Clearing them makes the proxy variables authoritative, which is what
/// puts the code under test on the hook.
///
/// Both halves are asserted: the proxy saw nothing AND the daemon saw the call.
/// Without the second, "the proxy is empty" would be equally true of a command
/// that sent nothing at all.
#[tokio::test]
async fn a_configured_proxy_receives_nothing_from_the_control_call() {
    // Arrange: a proxy that would answer anything, and the real daemon target.
    let proxy = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response()))
        .mount(&proxy)
        .await;
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response()))
        .mount(&daemon)
        .await;
    let mut config = config_pointing_at(&daemon.uri());
    // A credential is configured deliberately: it is the thing whose exposure
    // this check exists to prevent, so the request under test carries one.
    // `_token_file` holds the backing file alive for the whole test and removes
    // it on drop.
    let (token_ref, _token_file) = file_ref("listener-token");
    config.server.auth = Some(ServerAuth {
        tokens: vec![token_ref],
    });

    // Act: every proxy spelling reqwest honors SET, and every bypass spelling
    // UNSET, for this process only.
    let _guards = [
        routectl_testkit::ScopedEnv::set("HTTP_PROXY", proxy.uri()),
        routectl_testkit::ScopedEnv::set("http_proxy", proxy.uri()),
        routectl_testkit::ScopedEnv::set("ALL_PROXY", proxy.uri()),
        routectl_testkit::ScopedEnv::set("all_proxy", proxy.uri()),
        routectl_testkit::ScopedEnv::unset("NO_PROXY"),
        routectl_testkit::ScopedEnv::unset("no_proxy"),
    ];
    // Premise, asserted rather than assumed: with the bypass variables cleared
    // and the proxy variables set, a stock client WOULD proxy this request. If a
    // future reqwest stopped reading them, the assertions below would go vacuous
    // silently, so the premise is proven inside the test.
    let stock = reqwest::Client::builder()
        .build()
        .expect("build a stock client");
    let via_proxy = stock
        .post(format!("http://{}:{}{PURGE_PATH}", "127.0.0.1", 1))
        .json(&json!({"state_key": "premise", "capability_key": "premise"}))
        .send()
        .await;
    assert!(
        via_proxy.is_ok(),
        "premise: a stock client must reach the PROXY even for an address \
         nothing listens on -- if it errored, the environment is not being read \
         and this test can no longer observe the bypass"
    );
    let premise_hits = proxy.received_requests().await.expect("proxy records");
    assert_eq!(
        premise_hits.len(),
        1,
        "premise: the stock client's request must have landed on the proxy"
    );
    proxy.reset().await;

    // Act: the code under test.
    let code = routectl_cli::commands::capability_purge::run(&config, "sonnet", "web_search").await;

    // Assert
    assert_eq!(code, 0, "the call must succeed straight to the daemon");
    let proxied = proxy.received_requests().await.expect("proxy records");
    assert!(
        proxied.is_empty(),
        "a configured proxy must see no part of a loopback control call -- it \
         would otherwise receive the listener token and the target names"
    );
    let direct = daemon.received_requests().await.expect("daemon records");
    assert_eq!(
        direct.len(),
        1,
        "control: the daemon must have received the call directly, or the \
         emptiness above says only that nothing was sent at all"
    );
}
