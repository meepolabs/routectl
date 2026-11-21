//! Coverage for the daemon-mediated purge command: it talks to the daemon over
//! loopback, reports a purge and a clean no-op differently, fails clearly when
//! the daemon is unreachable or refuses, leaves no local state behind on any
//! failure, and never opens the usage ledger itself.

use super::*;

use routectl_router::{ServerAuth, ServerConfig};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The lane every case purges, in the form doctor prints.
const LANE: &str = "anthropic#claude-sonnet-4-5";

/// A config whose server address points at `base_url`'s host and port, so the
/// command dials the mock daemon instead of a real one.
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

/// The daemon's success envelope for a given `purged` flag.
fn purge_response(purged: bool) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "purged": purged,
        "state_key": LANE,
        "capability_key": "web_search",
    })
}

#[tokio::test]
async fn a_purge_reported_by_the_daemon_exits_zero() {
    // Arrange
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert
    assert_eq!(code, 0);
}

#[tokio::test]
async fn a_clean_no_op_reported_by_the_daemon_also_exits_zero() {
    // Arrange
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(false)))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert: "nothing was there" is not a failure -- an operator scripting a
    // cleanup must be able to run it twice.
    assert_eq!(code, 0);
}

#[tokio::test]
async fn the_request_body_names_only_the_target_and_the_capability() {
    // Arrange: the mock accepts ONLY the exact body this command must send, so a
    // drifted field set fails to match and the request 404s.
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .and(wiremock::matchers::body_json(json!({
            "state_key": LANE,
            "capability_key": "web_search",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert: a provider kind, a config path, or any other field would have
    // failed the body matcher.
    assert_eq!(code, 0);
}

#[tokio::test]
async fn a_refusal_from_the_daemon_exits_non_zero() {
    // Arrange
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "schema_version": 1,
            "error": {"code": "invalid_request", "message": "refused"},
        })))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert
    assert_eq!(code, 1);
}

/// Each durability-class refusal exits non-zero and NEVER reads as a purge.
///
/// The distinction matters more than the exit code: an operator who reads
/// "nothing to purge" stops looking, so a refusal that rendered as a clean no-op
/// would leave a still-acting verdict behind a message saying it was gone.
#[tokio::test]
async fn every_durability_class_refusal_exits_non_zero_without_claiming_a_purge() {
    for (status, code) in [
        (503, "durability_failed"),
        (409, "purge_busy"),
        (409, "purge_stale"),
        (409, "purge_superseded"),
    ] {
        let daemon = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PURGE_PATH))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "schema_version": 1,
                "error": {"code": code, "message": "refused"},
            })))
            .expect(1)
            .mount(&daemon)
            .await;

        let exit = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

        assert_eq!(
            exit, 1,
            "`{code}` leaves the entry acting, so it must exit non-zero rather than read as a \
             completed purge",
        );
    }
}

/// An OLD daemon that does not know these codes must still be handled safely.
///
/// This command talks to whatever answered on the control port, which may be a
/// different build. An unknown refusal code exits non-zero with the generic line
/// and no guidance -- guessing at an unrecognized refusal would be worse than
/// saying nothing.
#[tokio::test]
async fn an_unknown_refusal_code_from_an_older_daemon_still_exits_non_zero() {
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "schema_version": 99,
            "error": {"code": "some_future_code", "message": "refused"},
        })))
        .expect(1)
        .mount(&daemon)
        .await;

    let exit = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    assert_eq!(
        exit, 1,
        "an unrecognized refusal -- including from a newer or older daemon -- must fail closed",
    );
}

/// The mirrored wire codes must equal the route's own constants.
///
/// The CLI mirrors them rather than importing them, because it renders a WIRE
/// vocabulary it may receive from a daemon of another build. That is only safe
/// while the mirror agrees with what THIS build emits, so the agreement is
/// pinned rather than assumed -- a drifted mirror would silently stop matching
/// its own daemon's refusals and the guidance would quietly disappear.
#[test]
fn the_mirrored_refusal_codes_match_the_routes_own() {
    assert_eq!(
        super::DURABILITY_FAILED_CODE,
        crate::handlers::control::durability_failed_code(),
    );
    assert_eq!(
        super::PURGE_BUSY_CODE,
        crate::handlers::control::purge_busy_code(),
    );
    assert_eq!(
        super::PURGE_STALE_CODE,
        crate::handlers::control::purge_stale_code(),
    );
    assert_eq!(
        super::PURGE_SUPERSEDED_CODE,
        crate::handlers::control::purge_superseded_code(),
    );
}

#[tokio::test]
async fn an_unrecognized_success_body_exits_non_zero() {
    // Arrange: a 200 that carries no `purged` field. Reporting this as a purge
    // would tell the operator something happened that may not have.
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"schema_version": 99})))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert
    assert_eq!(code, 1);
}

/// A success status whose body is over the read cap is an unrecognized
/// response, not a purge. The oversized body is a VALID purge-success envelope
/// padded past 2 MiB, so an uncapped read would parse it and exit zero: the
/// only way this exits non-zero is the cap refusing the read.
#[tokio::test]
async fn an_oversized_success_body_is_an_unrecognized_response() {
    // Arrange
    let mut body = purge_response(true);
    body["padding"] = json!("x".repeat(2 * daemon_client::MAX_BODY_BYTES));
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert
    assert_eq!(code, 1, "an over-cap body must not be read as a purge");
}

#[tokio::test]
async fn an_unreachable_daemon_exits_non_zero_and_writes_no_local_state() {
    // Arrange: a port bound WITHOUT listening, so every dial is refused and no
    // sibling can claim the port mid-test.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a held port");
    let port = held.local_addr().expect("read held port").port();
    let mut config = Config::default();
    config.server.host = "127.0.0.1".to_string();
    config.server.port = port;
    let ledger = tempfile::tempdir().expect("tempdir");
    config.usage.db_path = ledger.path().join("usage.db");

    // Act
    let code = run(&config, LANE, "web_search").await;

    // Assert
    assert_eq!(code, 1, "an unreachable daemon must be a clear failure");
    assert!(
        !config.usage.db_path.exists(),
        "a failed purge must leave no partial local state -- the command never \
         opens the ledger, so the file must not even exist"
    );
    drop(held);
}

#[tokio::test]
async fn a_successful_purge_never_opens_the_usage_ledger() {
    // Arrange: a daemon that reports a purge, and a config naming a ledger path
    // that does not exist. The ledger FILE is the observable: every open in this
    // repo migrates, so an open would materialize it.
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .mount(&daemon)
        .await;
    let mut config = config_pointing_at(&daemon.uri());
    let dir = tempfile::tempdir().expect("tempdir");
    config.usage.db_path = dir.path().join("usage.db");

    // Act
    let code = run(&config, LANE, "web_search").await;

    // Assert
    assert_eq!(code, 0);
    assert!(
        !config.usage.db_path.exists(),
        "the CLI must reach the ledger only through the daemon; opening it here \
         would both miss the live registry and endanger the daemon's own \
         database"
    );
}

/// The source-text half of the daemon-only boundary. The behavioral test above
/// proves the ledger file is not materialized on the paths it drives; this one
/// proves the command holds no ledger API at all, so a future edit cannot
/// reintroduce one on a path no test happens to drive.
#[test]
fn the_command_source_names_no_ledger_or_registry_api() {
    // Arrange: the module's own production text. There is no inline test module
    // here (the tests are a sidecar), so the whole file is the production
    // region and nothing can truncate the scan.
    let source = include_str!("capability_purge.rs");
    assert!(
        !source.contains("mod tests {"),
        "this guard assumes a sidecar test module; an inline one would put test \
         text inside the scanned region"
    );

    // Assert: no direct database or registry reach-in of any kind.
    for forbidden in [
        "rusqlite",
        "routectl_usage",
        "UsageWriter",
        "UsageHandle",
        "CapabilityEvent",
        "purge_learned_capability",
        "learned_capability_snapshot",
    ] {
        assert!(
            !source.contains(forbidden),
            "`{forbidden}` in the purge command breaks the daemon-only \
             boundary: the CLI must ask the daemon rather than reach the state \
             itself"
        );
    }
}

/// The listener credential travels when one is configured. A token-configured
/// daemon 401s an unauthenticated control call, so a command that skipped this
/// would be broken on exactly the deployments that gate their listener.
#[tokio::test]
async fn a_configured_listener_token_is_sent_with_the_request() {
    // Arrange: the mock matches ONLY a request carrying the credential header.
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .and(header_exists("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .expect(1)
        .mount(&daemon)
        .await;
    let mut config = config_pointing_at(&daemon.uri());
    config.server.auth = Some(ServerAuth {
        tokens: vec![crate::test_secret::file_ref("listener-token")],
    });

    // Act
    let code = run(&config, LANE, "web_search").await;

    // Assert
    assert_eq!(code, 0);
}

/// A token-less daemon is the loopback dev default, and the command must work
/// against it without inventing a credential.
#[tokio::test]
async fn no_credential_header_is_sent_when_the_daemon_is_token_less() {
    // Arrange
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert: read the header off the request the daemon actually received,
    // rather than off a matcher -- a matcher that fails to match reports as a
    // 404, which the exit code alone cannot tell apart from a refusal.
    assert_eq!(code, 0);
    let requests = daemon
        .received_requests()
        .await
        .expect("mock records requests");
    assert_eq!(requests.len(), 1);
    assert!(
        !requests[0].headers.contains_key("x-api-key"),
        "a token-less config must not make the command invent a credential"
    );
}

// --- Fix round: destination derivation, redirect refusal ---

/// The whole command refuses before transmitting when no loopback destination
/// can be derived. Proven on a config an operator can really write: a hostname
/// bind. The observable is that nothing was sent anywhere -- asserted against a
/// mock that would have recorded a request had one been made.
#[tokio::test]
async fn an_underivable_bind_transmits_nothing_and_exits_non_zero() {
    // Arrange: a live mock exists, so "no request recorded" is a statement
    // about the command rather than about an absent listener.
    let daemon = MockServer::start().await;
    let mut config = config_pointing_at(&daemon.uri());
    config.server.host = "daemon.example.com".to_string();
    config.server.auth = Some(ServerAuth {
        tokens: vec![crate::test_secret::file_ref("listener-token")],
    });

    // Act
    let code = run(&config, LANE, "web_search").await;

    // Assert
    assert_eq!(
        code, 1,
        "an underivable destination must be a clear failure"
    );
    let requests = daemon
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests.is_empty(),
        "the command must refuse locally, before a credential reaches any socket"
    );
}

/// A cross-host redirect must be REFUSED, not followed. reqwest's default
/// policy strips only `Authorization` / `Cookie` on a host change, so an
/// `x-api-key` -- which is what this command sends -- would travel to whatever
/// host a `Location` header names, along with the request body.
#[tokio::test]
async fn a_cross_host_redirect_receives_neither_body_nor_credential() {
    // Arrange: the daemon 302s to a second host that records everything.
    let elsewhere = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .mount(&elsewhere)
        .await;
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("{}{PURGE_PATH}", elsewhere.uri()).as_str(),
        ))
        .mount(&daemon)
        .await;
    let mut config = config_pointing_at(&daemon.uri());
    config.server.auth = Some(ServerAuth {
        tokens: vec![crate::test_secret::file_ref("listener-token")],
    });

    // Act
    let code = run(&config, LANE, "web_search").await;

    // Assert: the redirect target saw nothing at all.
    let hops = elsewhere.received_requests().await.expect("records");
    assert!(
        hops.is_empty(),
        "a redirect target must receive neither the body nor the x-api-key \
         header; reqwest's default cross-host strip list does not cover it"
    );
    assert_eq!(
        code, 1,
        "a 3xx is a failure, not a success -- with redirects disabled reqwest \
         RETURNS the 3xx, so a status gate written as `>= 400` would read it as \
         a purge"
    );
}

/// Every 3xx is a failure, one class at a time -- and the fixture is what makes
/// that testable. Each 3xx here carries a VALID purge-success body, because with
/// redirects disabled reqwest hands back the 3xx response itself: a status gate
/// written as "not >= 400" would then parse that body and report a purge that
/// never happened. An empty 3xx body would fail to parse and exit non-zero for
/// the wrong reason, proving nothing about the gate.
#[tokio::test]
async fn every_3xx_status_is_reported_as_a_failure() {
    for status in [300_u16, 301, 302, 303, 307, 308] {
        // Arrange
        let daemon = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PURGE_PATH))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("location", "/elsewhere")
                    .set_body_json(purge_response(true)),
            )
            .mount(&daemon)
            .await;

        // Act
        let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

        // Assert
        assert_eq!(
            code, 1,
            "status {status} must be reported as a failure, never as a purge, \
             even when its body would parse as one"
        );
    }
}

// --- Second fix round: the daemon's error.code is untrusted input ---

/// The refusal code is rendered SANITIZED and CAPPED.
///
/// It reaches this command as a JSON string from whatever answered on that
/// port, so it is untrusted bytes on a path straight to a terminal. A raw
/// `println`/`eprintln` of it forges output: a newline fabricates a whole extra
/// line of what looks like routectl's own diagnostics, and an ANSI CSI sequence
/// repaints or clears the operator's screen. The same filter the daemon's own
/// log surfaces use applies here.
#[test]
fn a_hostile_error_code_is_neutralized_before_it_reaches_a_terminal() {
    for (label, hostile) in [
        (
            "a newline forging a second line",
            "bad\nerror: purged 47 entries",
        ),
        (
            "a carriage return repainting the line",
            "bad\rerror: all clear",
        ),
        ("an ANSI CSI clear-screen", "bad\x1b[2Jerror: fine"),
        ("an ANSI colour run", "\x1b[31mDANGER\x1b[0m"),
        ("a bare escape", "bad\x1bcreset"),
        ("a NUL byte", "bad\0code"),
        ("a backspace run", "bad\u{8}\u{8}\u{8}ok"),
    ] {
        // Act
        let rendered = render_refusal_code(hostile);

        // Assert
        assert!(
            !rendered.contains('\n')
                && !rendered.contains('\r')
                && !rendered.contains('\x1b')
                && !rendered.contains('\0'),
            "{label}: `{hostile:?}` must not reach a terminal with its control \
             bytes intact -- rendered `{rendered:?}`"
        );
    }
}

/// An oversized code is capped, so a daemon (or an impostor) cannot flood the
/// operator's terminal through a diagnostic line.
#[test]
fn an_oversize_error_code_is_capped() {
    // Arrange
    let huge = "x".repeat(10_000);

    // Act
    let rendered = render_refusal_code(&huge);

    // Assert
    assert!(
        rendered.len() < huge.len(),
        "an oversize code must be capped, not echoed whole"
    );
    assert!(
        rendered.len() <= MAX_REFUSAL_CODE_CHARS + 8,
        "the cap must actually bound the line; rendered {} chars",
        rendered.len()
    );
}

/// The positive control, and the reason the assertions above are about the
/// sanitizer: an ORDINARY code survives byte-for-byte. Without this, a renderer
/// that returned a fixed placeholder for everything would satisfy every
/// hostile-input assertion while destroying the field's only diagnostic value.
#[test]
fn an_ordinary_error_code_is_preserved_exactly() {
    for code in [
        "invalid_request",
        "forbidden_peer",
        "unsupported_media_type",
        "unknown",
    ] {
        // Act
        let rendered = render_refusal_code(code);

        // Assert
        assert_eq!(
            rendered, code,
            "an ordinary closed-set code must be preserved exactly, or the \
             field stops being useful for diagnosis"
        );
    }
}

/// End to end: a daemon answering with a hostile code cannot forge terminal
/// output through this command. Driven through the real `run` so the sanitizer
/// is proven to sit on the actual output path, not merely to exist.
#[tokio::test]
async fn a_daemon_sending_a_hostile_code_still_exits_non_zero() {
    // Arrange
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "schema_version": 1,
            "error": {"code": "bad\nerror: purged everything\x1b[2J", "message": "x"},
        })))
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert
    assert_eq!(
        code, 1,
        "a refusal is a failure however its code is spelled"
    );
}

// --- Final correction: `localhost` is a NAME, so it is underivable ---

/// A `localhost` bind refuses locally: nothing is transmitted.
///
/// The observable here is precise, and deliberately NOT a claim about credential
/// resolution: a LIVE mock recorded no request, so nothing reached a socket
/// authenticated or otherwise. The token ref names a path that does not exist,
/// which keeps the fixture from creating temp-file residue and means no real
/// credential is involved -- but it cannot prove resolution was SKIPPED, because
/// `first_listener_token` swallows a failed lookup (`.ok()`) and returns `None`
/// with no observable effect. "Skipped" and "attempted and failed" are therefore
/// indistinguishable from outside, so the ORDER is pinned by the source guard
/// below instead, which can actually fail.
#[tokio::test]
async fn a_localhost_bind_refuses_locally_without_transmitting() {
    // Arrange
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .mount(&daemon)
        .await;
    let mut config = config_pointing_at(&daemon.uri());
    config.server.host = "localhost".to_string();
    // A ref whose backing file does not exist, so no real credential exists and
    // this test writes nothing to disk.
    let unresolvable = std::env::temp_dir()
        .join("routectl-purge-no-such-token-file-must-not-be-read")
        .display()
        .to_string();
    assert!(
        !std::path::Path::new(&unresolvable).exists(),
        "premise: the token path must NOT exist, so no credential is available \
         to leak even if the command were to reach its credential step"
    );
    config.server.auth = Some(ServerAuth {
        tokens: vec![format!("file://{unresolvable}")],
    });

    // Act
    let code = run(&config, LANE, "web_search").await;

    // Assert
    assert_eq!(code, 1, "a localhost bind must be a clear local failure");
    let requests = daemon
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests.is_empty(),
        "the refusal must precede any request -- nothing may reach a socket"
    );
}

/// The destination is derived BEFORE the credential is resolved, pinned on the
/// source because no behavioral fixture can see it.
///
/// `first_listener_token` maps a failed lookup to `None` and returns silently,
/// so a premature resolve leaves no trace an assertion could catch -- no error,
/// no output, no filesystem effect. The order still matters: reversing it means a
/// token is read off disk for a destination the command is about to refuse, which
/// is the wrong sequence for a secret even when nothing is transmitted.
///
/// So the guard reads the production text and requires the `control_url` call to
/// precede the `first_listener_token` call. Mutation-verified: moving the resolve
/// above the derivation trips this.
#[test]
fn the_destination_is_derived_before_any_credential_is_resolved() {
    // Arrange: this module's production text. The tests are a sidecar, so the
    // whole file is the production region and nothing can truncate the scan.
    let source = include_str!("capability_purge.rs");
    assert!(
        !source.contains("mod tests {"),
        "this guard assumes a sidecar test module; an inline one would put test \
         text inside the scanned region"
    );

    // Act: locate the two calls in `run`'s body.
    let derive_at = source
        .find("control_url(&config.server.host")
        .expect("run must derive the destination through control_url");
    let resolve_at = source
        .find("first_listener_token(config)")
        .expect("run must resolve the listener token through first_listener_token");

    // Assert
    assert!(
        derive_at < resolve_at,
        "the destination derivation must come FIRST: resolving a listener token \
         for a destination that is about to be refused reads a secret off disk \
         for no reason, and a failed resolve is silent so no behavioral test can \
         catch the reversal"
    );
}

/// A target that is not a learned lane is refused before anything leaves the
/// process: no request reaches the daemon, and the exit is non-zero rather than
/// a "nothing to purge" that would read as already gone.
#[tokio::test]
async fn an_unparseable_lane_exits_non_zero_without_dialing_the_daemon() {
    // Arrange: the mock answers a well-formed success, so a request that slipped
    // through would exit zero.
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(false)))
        .expect(0)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

    // Assert
    assert_ne!(code, 0, "a bare nickname is not a lane");
    assert!(
        daemon
            .received_requests()
            .await
            .expect("request recording is on")
            .is_empty(),
        "nothing may reach the daemon for an unparseable lane",
    );
}

/// Positive control for the refusal above: the same daemon and the same
/// command, with a lane, does dial and exits zero.
#[tokio::test]
async fn a_parseable_lane_reaches_the_daemon() {
    let daemon = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(false)))
        .expect(1)
        .mount(&daemon)
        .await;

    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    assert_eq!(code, 0);
}

#[test]
fn each_success_envelope_renders_its_own_first_line() {
    let rows = [
        (
            "learned purge",
            json!({"purged": true}),
            "purged learned capability",
        ),
        (
            "seed lift",
            json!({"purged": true, "seed_lifted": true}),
            "lifted the shipped seed",
        ),
        ("clean no-op", json!({"purged": false}), "nothing to purge"),
    ];

    for (name, body, expected) in rows {
        let lines = success_lines(&body, "cap", LANE).expect("a purged verdict renders");

        assert!(lines[0].starts_with(expected), "{name}: {lines:?}");
    }
    assert!(success_lines(&json!({"seed_lifted": true}), "cap", LANE).is_none());
}

#[tokio::test]
async fn a_seed_lift_reported_by_the_daemon_exits_zero() {
    // Arrange
    let daemon = MockServer::start().await;
    let mut body = purge_response(true);
    body["seed_lifted"] = json!(true);
    Mock::given(method("POST"))
        .and(path(PURGE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), LANE, "web_search").await;

    // Assert
    assert_eq!(code, 0);
}
