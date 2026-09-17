//! Coverage for the daemon-mediated purge command: it talks to the daemon over
//! loopback, reports a purge and a clean no-op differently, fails clearly when
//! the daemon is unreachable or refuses, leaves no local state behind on any
//! failure, and never opens the usage ledger itself.

use super::*;

use routectl_router::{ServerAuth, ServerConfig};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
        "state_key": "sonnet",
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
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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
            "state_key": "sonnet",
            "capability_key": "web_search",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(purge_response(true)))
        .expect(1)
        .mount(&daemon)
        .await;

    // Act
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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

        let exit = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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

    let exit = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

    // Assert
    assert_eq!(code, 1);
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
    let code = run(&config, "sonnet", "web_search").await;

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
    let code = run(&config, "sonnet", "web_search").await;

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
    let code = run(&config, "sonnet", "web_search").await;

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
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

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

// --- Fix round: destination derivation, proxy bypass, redirect refusal ---

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
    let code = run(&config, "sonnet", "web_search").await;

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
    let code = run(&config, "sonnet", "web_search").await;

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
        let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

        // Assert
        assert_eq!(
            code, 1,
            "status {status} must be reported as a failure, never as a purge, \
             even when its body would parse as one"
        );
    }
}

/// Single-source pin for the two client settings whose absence is otherwise
/// only observable through a process-global environment variable.
///
/// The behavioral proof of the proxy bypass lives in its own test BINARY
/// (`tests/capability_purge_no_proxy.rs`) because the environment is
/// per-process; this guard is the cheap in-suite companion, and it fails on
/// exactly one line so removing either setting names itself.
#[test]
fn the_control_client_pins_no_proxy_and_no_redirects() {
    // Arrange: this module's production text. The tests are a sidecar, so the
    // whole file is the production region and nothing can truncate the scan.
    let source = include_str!("capability_purge.rs");

    // Assert
    assert!(
        source.contains(".no_proxy()"),
        "the control client must bypass any configured proxy: reqwest reads \
         HTTP_PROXY / ALL_PROXY from the environment by default, and a loopback \
         control call must not hand its listener token to an outbound hop"
    );
    assert!(
        source.contains("redirect::Policy::none()"),
        "the control client must not follow redirects: `x-api-key` is not on \
         reqwest's cross-host strip list, so a followed 3xx would carry the \
         credential and the body to whatever host Location names"
    );
}

// --- Second fix round: the destination is loopback or it is nothing ---

/// A LITERAL loopback bind is preserved exactly as written, and an IPv6 literal
/// is bracketed. Preserved rather than normalized because the daemon bound that
/// specific address: rewriting `127.0.0.5` to `127.0.0.1` would dial an address
/// nothing is listening on.
#[test]
fn a_literal_loopback_bind_is_preserved_verbatim() {
    for (bind, expected) in [
        (
            "127.0.0.1",
            "http://127.0.0.1:8791/control/capability/purge",
        ),
        (
            "127.0.0.5",
            "http://127.0.0.5:8791/control/capability/purge",
        ),
        (
            "127.255.255.254",
            "http://127.255.255.254:8791/control/capability/purge",
        ),
        ("::1", "http://[::1]:8791/control/capability/purge"),
        ("[::1]", "http://[::1]:8791/control/capability/purge"),
    ] {
        // Act
        let url = control_url(bind, 8791);

        // Assert
        assert_eq!(
            url.as_deref(),
            Some(expected),
            "literal loopback bind `{bind}` must be addressed exactly as bound"
        );
    }
}

/// A WILDCARD bind derives the loopback address of the matching family. A
/// wildcard is reachable on every interface INCLUDING loopback, so this is a
/// derivation the operator's own configuration justifies -- `0.0.0.0` itself is
/// not an address to dial.
#[test]
fn a_wildcard_bind_derives_the_matching_family_loopback() {
    for (bind, expected) in [
        ("0.0.0.0", "http://127.0.0.1:8791/control/capability/purge"),
        ("::", "http://[::1]:8791/control/capability/purge"),
        ("[::]", "http://[::1]:8791/control/capability/purge"),
    ] {
        // Act
        let url = control_url(bind, 8791);

        // Assert
        assert_eq!(
            url.as_deref(),
            Some(expected),
            "wildcard bind `{bind}` must derive its family's loopback address"
        );
    }
}

/// A SPECIFIC non-loopback bind is REFUSED, not rewritten.
///
/// Rewriting it to `127.0.0.1` was wrong in both directions. It is a guess: a
/// daemon bound only to `10.20.30.40` is NOT listening on loopback, so the
/// rewrite dials a port that may belong to an entirely different process on
/// this machine -- and the credentialed request goes to whatever answers. And it
/// silently contradicts the operator's own configuration instead of saying the
/// control surface is unreachable as configured. A refusal is the honest answer.
#[test]
fn a_specific_non_loopback_bind_is_refused_rather_than_rewritten() {
    for bind in [
        "10.20.30.40",
        "192.168.1.5",
        "203.0.113.7",
        "2001:db8::1",
        "[2001:db8::1]",
        "::ffff:203.0.113.7",
        "169.254.1.1",
    ] {
        // Act
        let url = control_url(bind, 8791);

        // Assert
        assert!(
            url.is_none(),
            "specific non-loopback bind `{bind}` must be refused, not rewritten \
             to loopback: the daemon is not listening there, so the rewrite \
             would send a credential to whatever else holds that port"
        );
    }
}

/// An IPv4-MAPPED loopback bind is a loopback bind. The shared predicate
/// unwraps the mapping, so `::ffff:127.0.0.1` is preserved rather than refused.
#[test]
fn an_ipv4_mapped_loopback_bind_is_preserved() {
    // Act
    let url = control_url("::ffff:127.0.0.1", 8791);

    // Assert
    assert_eq!(
        url.as_deref(),
        Some("http://[::ffff:127.0.0.1]:8791/control/capability/purge"),
        "an IPv4-mapped loopback literal names loopback and must be preserved"
    );
}

/// Everything unclassifiable is refused. A hostname cannot be shown to name
/// loopback without asking a resolver, and a resolver answer is exactly what
/// this command must not depend on.
#[test]
fn every_unclassifiable_bind_is_refused() {
    for bind in [
        "",
        "   ",
        "not a host",
        "example.com",
        "daemon.internal",
        "http://127.0.0.1",
        "127.0.0.1:8791",
        // A NAME, and treated as one: see the dedicated test below.
        "localhost",
        "LOCALHOST",
        "LocalHost",
        "[::1",
        "::1]",
        "localhost.evil.example",
    ] {
        // Act
        let url = control_url(bind, 8791);

        // Assert
        assert!(
            url.is_none(),
            "bind `{bind}` cannot be shown to name loopback, so it must be \
             refused rather than guessed"
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
    let code = run(&config_pointing_at(&daemon.uri()), "sonnet", "web_search").await;

    // Assert
    assert_eq!(
        code, 1,
        "a refusal is a failure however its code is spelled"
    );
}

// --- Final correction: `localhost` is a NAME, so it is underivable ---

/// `localhost` is refused like any other hostname.
///
/// It reads as obviously loopback, which is exactly why it was tempting to
/// special-case -- and why the special case was wrong. Accepting it forced this
/// command to pick an address FAMILY on the operator's behalf: `localhost`
/// resolves to `127.0.0.1`, `::1`, or both depending on the resolver and the
/// hosts file, and the daemon bound whichever one it was given. Choosing `::1`
/// when the daemon bound `127.0.0.1` sends a credentialed request to a port that
/// may belong to a different process; choosing `127.0.0.1` when it bound `::1`
/// does the same. There is no correct guess available here, so the honest answer
/// is to require the operator to say which one -- the literal they already had
/// to give the daemon.
#[test]
fn the_localhost_name_is_underivable_like_any_other_hostname() {
    for bind in ["localhost", "LOCALHOST", "LocalHost", "localhost."] {
        // Act
        let url = control_url(bind, 8791);

        // Assert
        assert!(
            url.is_none(),
            "`{bind}` is a NAME: deriving from it would mean picking an address \
             family the operator never chose, and resolving it would let a \
             resolver decide where a credential goes"
        );
    }
}

/// The paired positive: the EXPLICIT loopback literals of both families still
/// work. Without this, the refusal above would be equally satisfied by a command
/// that refused every bind, which would make the control surface unusable.
#[test]
fn both_explicit_loopback_literal_families_still_derive() {
    // Act + Assert: IPv4.
    assert_eq!(
        control_url("127.0.0.1", 8791).as_deref(),
        Some("http://127.0.0.1:8791/control/capability/purge"),
        "an explicit IPv4 loopback literal must still work"
    );
    // Act + Assert: IPv6, bracketed.
    assert_eq!(
        control_url("::1", 8791).as_deref(),
        Some("http://[::1]:8791/control/capability/purge"),
        "an explicit IPv6 loopback literal must still work, bracketed"
    );
    // And the wildcard derivations are untouched by this change.
    assert_eq!(
        control_url("0.0.0.0", 8791).as_deref(),
        Some("http://127.0.0.1:8791/control/capability/purge"),
    );
    assert_eq!(
        control_url("::", 8791).as_deref(),
        Some("http://[::1]:8791/control/capability/purge"),
    );
}

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
    let code = run(&config, "sonnet", "web_search").await;

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
