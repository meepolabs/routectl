//! Coverage for the daemon client: the destination is a loopback literal or
//! nothing, the client pins its no-proxy and no-redirect policy, and a body is
//! read only up to the cap.

use super::*;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The path the moved destination rows were written against. Any path renders
/// the same way; the rows keep one so their expected URLs stay readable.
const PURGE_PATH: &str = "/control/capability/purge";

/// Single-source pin for the two client settings whose absence is otherwise
/// only observable through a process-global environment variable.
///
/// The behavioral proof of the proxy bypass lives in its own test BINARY
/// (`tests/capability_purge_no_proxy.rs`, which drives this client through the
/// purge command) because the environment is per-process; this guard is the
/// cheap in-suite companion, and it fails on exactly one line so removing
/// either setting names itself.
#[test]
fn the_control_client_pins_no_proxy_and_no_redirects() {
    // Arrange: this module's production text. The tests are a sidecar, so the
    // whole file is the production region and nothing can truncate the scan.
    let source = include_str!("daemon_client.rs");

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

// --- The destination is loopback or it is nothing ---

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
        let url = control_url(bind, 8791, PURGE_PATH);

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
        let url = control_url(bind, 8791, PURGE_PATH);

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
        let url = control_url(bind, 8791, PURGE_PATH);

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
    let url = control_url("::ffff:127.0.0.1", 8791, PURGE_PATH);

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
        let url = control_url(bind, 8791, PURGE_PATH);

        // Assert
        assert!(
            url.is_none(),
            "bind `{bind}` cannot be shown to name loopback, so it must be \
             refused rather than guessed"
        );
    }
}

// --- `localhost` is a NAME, so it is underivable ---

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
        let url = control_url(bind, 8791, PURGE_PATH);

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
        control_url("127.0.0.1", 8791, PURGE_PATH).as_deref(),
        Some("http://127.0.0.1:8791/control/capability/purge"),
        "an explicit IPv4 loopback literal must still work"
    );
    // Act + Assert: IPv6, bracketed.
    assert_eq!(
        control_url("::1", 8791, PURGE_PATH).as_deref(),
        Some("http://[::1]:8791/control/capability/purge"),
        "an explicit IPv6 loopback literal must still work, bracketed"
    );
    // And the wildcard derivations are untouched by this change.
    assert_eq!(
        control_url("0.0.0.0", 8791, PURGE_PATH).as_deref(),
        Some("http://127.0.0.1:8791/control/capability/purge"),
    );
    assert_eq!(
        control_url("::", 8791, PURGE_PATH).as_deref(),
        Some("http://[::1]:8791/control/capability/purge"),
    );
}

/// The path is the caller's: the same derivation serves any daemon route, and
/// the refusals do not depend on which route was asked for.
#[test]
fn the_url_carries_the_callers_path_and_keeps_the_refusals() {
    let port = 9100;
    for (bind, expected) in [
        ("127.0.0.5", Some("http://127.0.0.5:9100/status/attention")),
        ("::", Some("http://[::1]:9100/status/attention")),
        ("localhost", None),
        ("daemon.example.com", None),
        ("10.0.0.1", None),
    ] {
        // Act
        let url = control_url(bind, port, "/status/attention");

        // Assert
        assert_eq!(url.as_deref(), expected, "bind `{bind}`");
    }
}

// --- The capped body reader ---

/// A response from a mock serving `len` bytes with status 200.
async fn response_with_body_len(daemon: &MockServer, len: usize) -> reqwest::Response {
    Mock::given(method("GET"))
        .and(path("/body"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; len]))
        .mount(daemon)
        .await;
    build_client(REQUEST_TIMEOUT)
        .expect("client builds")
        .get(format!("{}/body", daemon.uri()))
        .send()
        .await
        .expect("mock answers")
}

#[tokio::test]
async fn a_body_of_exactly_the_cap_is_returned_whole() {
    // Arrange
    let daemon = MockServer::start().await;
    let response = response_with_body_len(&daemon, MAX_BODY_BYTES).await;

    // Act
    let body = read_capped_body(response).await;

    // Assert
    let body = body.expect("a body at the cap is within it");
    assert_eq!(body.len(), MAX_BODY_BYTES);
}

#[tokio::test]
async fn a_body_one_byte_over_the_cap_is_refused() {
    // Arrange
    let daemon = MockServer::start().await;
    let response = response_with_body_len(&daemon, MAX_BODY_BYTES + 1).await;

    // Act
    let body = read_capped_body(response).await;

    // Assert
    assert!(
        matches!(body, Err(BodyError::TooLarge { limit }) if limit == MAX_BODY_BYTES),
        "one byte over the cap must be refused, got {body:?}"
    );
}

#[test]
fn the_cap_is_one_mebibyte() {
    assert_eq!(MAX_BODY_BYTES, 1_048_576);
}
