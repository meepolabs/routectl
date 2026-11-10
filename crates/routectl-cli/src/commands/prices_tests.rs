//! The refresh command: client policy (headers, redirects, cookies,
//! timeouts), the read bounds on both sources, fail-closed writes, and what
//! reaches logs and output.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const SOURCE_SLICE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../routectl-router/src/prices/testdata/models_dev_slice.json"
);

fn date() -> TableDate {
    TableDate::parse("2026-11-09").expect("date")
}

fn valid_body() -> Vec<u8> {
    br#"{"acme":{"models":{"m1":{"cost":{"input":1,"output":2}}}}}"#.to_vec()
}

fn url(text: &str) -> Url {
    Url::parse(text).expect("test url parses")
}

async fn serve_once(server: &MockServer, route: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(response)
        .mount(server)
        .await;
}

/// The source ids of the real models.dev slice: every provider key and every
/// model key under it.
fn slice_ids() -> Vec<String> {
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(SOURCE_SLICE).expect("slice")).expect("json");
    let mut ids = Vec::new();
    for (provider, body) in doc.as_object().expect("object") {
        ids.push(provider.clone());
        if let Some(models) = body["models"].as_object() {
            ids.extend(models.keys().cloned());
        }
    }
    ids
}

#[tokio::test]
async fn the_request_carries_no_credential_cookie_or_encoding_header() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/api.json",
        ResponseTemplate::new(200).set_body_bytes(valid_body()),
    )
    .await;
    let addr = server.address();
    let dir = tempfile::tempdir().expect("tempdir");
    let from = format!("http://user:secret@{addr}/api.json");

    refresh_into(dir.path(), Some(&from), date())
        .await
        .expect("refresh succeeds");

    let requests = server.received_requests().await.expect("recording on");
    assert_eq!(requests.len(), 1);
    let headers = &requests[0].headers;
    assert!(
        headers.contains_key("host"),
        "control: the recorded request exposes its headers"
    );
    for name in ["authorization", "x-api-key", "cookie", "accept-encoding"] {
        assert!(!headers.contains_key(name), "{name} was sent");
    }
}

#[tokio::test]
async fn a_cookie_set_by_the_source_is_never_sent_back() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/api.json",
        ResponseTemplate::new(200)
            .insert_header("set-cookie", "session=abc; Path=/")
            .set_body_bytes(valid_body()),
    )
    .await;
    let client = build_client(TIMEOUTS).expect("client");
    let target = url(&format!("{}/api.json", server.uri()));

    fetch(&client, &target, MAX_INPUT_BYTES)
        .await
        .expect("first");
    fetch(&client, &target, MAX_INPUT_BYTES)
        .await
        .expect("second");

    let requests = server.received_requests().await.expect("recording on");
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].headers.contains_key("cookie"));
}

#[tokio::test]
async fn a_redirect_is_refused_and_names_only_the_location_origin() {
    let rows: [(&str, &str, &str); 2] = [
        (
            "absolute location",
            "http://u:p@127.0.0.1:9/served-path-text?served-query-text=v#frag",
            "http://127.0.0.1:9",
        ),
        (
            "relative location",
            "/served-path-text?served-query-text=v",
            "",
        ),
    ];
    for (name, location, origin) in rows {
        let server = MockServer::start().await;
        serve_once(
            &server,
            "/api.json",
            ResponseTemplate::new(302).insert_header("location", location),
        )
        .await;
        let target = url(&format!("{}/api.json", server.uri()));
        let client = build_client(TIMEOUTS).expect("client");
        let origin = if origin.is_empty() {
            server.uri()
        } else {
            origin.to_owned()
        };

        let err = fetch(&client, &target, MAX_INPUT_BYTES)
            .await
            .expect_err("a 3xx is an error");

        let text = err.to_string();
        assert!(
            text.contains(&format!("HTTP 302 redirect to {origin};")),
            "{name}: {text}"
        );
        for leaked in ["served-path-text", "served-query-text", "u:p", "frag"] {
            assert!(!text.contains(leaked), "{name}: {leaked} in {text}");
        }
    }
}

#[tokio::test]
async fn fetch_never_sends_userinfo_from_the_url_it_is_given() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/api.json",
        ResponseTemplate::new(200).set_body_bytes(valid_body()),
    )
    .await;
    let client = build_client(TIMEOUTS).expect("client");
    let target = url(&format!("http://u:p@{}/api.json", server.address()));
    assert_eq!(
        (target.username(), target.password()),
        ("u", Some("p")),
        "control: the url handed to fetch carries userinfo"
    );

    fetch(&client, &target, MAX_INPUT_BYTES)
        .await
        .expect("fetch succeeds");

    let requests = server.received_requests().await.expect("recording on");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].headers.contains_key("host"),
        "control: the recorded request exposes its headers"
    );
    assert!(
        !requests[0].headers.contains_key("authorization"),
        "userinfo became an authorization header"
    );
}

#[tokio::test]
async fn a_followable_redirect_is_not_followed() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/api.json",
        ResponseTemplate::new(302).insert_header("location", "/target"),
    )
    .await;
    serve_once(
        &server,
        "/target",
        ResponseTemplate::new(200).set_body_bytes(valid_body()),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let from = format!("{}/api.json", server.uri());

    let err = refresh_into(dir.path(), Some(&from), date())
        .await
        .expect_err("redirect refused");

    assert!(matches!(err, PricesError::Redirect { status: 302, .. }));
    let hits = server.received_requests().await.expect("recording on");
    assert_eq!(hits.len(), 1, "the redirect target was requested");
    assert!(!dir.path().join(PRICES_FILE).exists());
}

#[tokio::test]
async fn the_total_timeout_ends_a_slow_response() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/api.json",
        ResponseTemplate::new(200)
            .set_delay(Duration::from_secs(5))
            .set_body_bytes(valid_body()),
    )
    .await;
    let client = build_client(Timeouts {
        connect: TIMEOUTS.connect,
        total: Duration::from_millis(200),
    })
    .expect("client");

    let err = fetch(
        &client,
        &url(&format!("{}/api.json", server.uri())),
        MAX_INPUT_BYTES,
    )
    .await
    .expect_err("times out");

    assert!(
        matches!(
            err,
            PricesError::Request {
                cause: "timed out",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn refresh_timeouts_are_five_seconds_to_connect_and_fifteen_in_total() {
    assert_eq!(TIMEOUTS.connect, Duration::from_secs(5));
    assert_eq!(TIMEOUTS.total, Duration::from_secs(15));
}

#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/at",
        ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 10]),
    )
    .await;
    serve_once(
        &server,
        "/over",
        ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 11]),
    )
    .await;
    let client = build_client(TIMEOUTS).expect("client");

    let at = fetch(&client, &url(&format!("{}/at", server.uri())), 10).await;
    let over = fetch(&client, &url(&format!("{}/over", server.uri())), 10).await;

    assert_eq!(at.expect("at the cap").len(), 10);
    assert!(matches!(
        over,
        Err(PricesError::TooLarge {
            limit: 10,
            check: SizeCheck::Declared
        })
    ));
}

/// A one-shot HTTP/1.1 server answering with a chunked body (no
/// `Content-Length`), so only the streaming cap can see its size.
async fn chunked_server(chunks: Vec<Vec<u8>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buf).await.expect("read");
            if n == 0 {
                return;
            }
            request.extend_from_slice(&buf[..n]);
        }
        let mut out = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for chunk in chunks {
            out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            out.extend_from_slice(&chunk);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        let _ = socket.write_all(&out).await;
        let _ = socket.shutdown().await;
    });
    format!("http://{addr}/api.json")
}

#[tokio::test]
async fn a_streamed_body_is_capped_as_it_is_read() {
    let rows: [(&str, Vec<Vec<u8>>, bool); 2] = [
        ("at the cap", vec![vec![b'x'; 6], vec![b'x'; 4]], true),
        ("one byte over", vec![vec![b'x'; 6], vec![b'x'; 5]], false),
    ];
    for (name, chunks, fits) in rows {
        let target = url(&chunked_server(chunks).await);
        let client = build_client(TIMEOUTS).expect("client");

        let result = fetch(&client, &target, 10).await;

        if fits {
            assert_eq!(result.expect(name).len(), 10, "{name}");
        } else {
            assert!(
                matches!(
                    result,
                    Err(PricesError::TooLarge {
                        check: SizeCheck::Read,
                        ..
                    })
                ),
                "{name}: {result:?}"
            );
        }
    }
}

/// A reader serving `len` bytes that counts how many it handed out.
struct CountingReader {
    remaining: usize,
    served: Arc<AtomicUsize>,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.remaining);
        buf[..n].fill(b'x');
        self.remaining -= n;
        self.served.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

#[test]
fn a_bounded_read_stops_one_byte_past_the_limit() {
    let served = Arc::new(AtomicUsize::new(0));
    let reader = CountingReader {
        remaining: 1 << 20,
        served: Arc::clone(&served),
    };

    let result = read_bounded(reader, 100);

    assert!(matches!(result, Err(BoundedRead::TooLarge)));
    assert_eq!(served.load(Ordering::SeqCst), 101);
}

#[test]
fn a_bounded_read_accepts_exactly_the_limit() {
    let reader = CountingReader {
        remaining: 100,
        served: Arc::new(AtomicUsize::new(0)),
    };

    assert_eq!(read_bounded(reader, 100).expect("fits").len(), 100);
}

#[test]
fn a_file_whose_metadata_exceeds_the_limit_is_refused_before_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("big.json");
    std::fs::write(&file, vec![b'x'; 11]).expect("write");

    let result = read_file(&file, 10);

    assert!(matches!(
        result,
        Err(PricesError::TooLarge {
            check: SizeCheck::Declared,
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn a_file_whose_metadata_understates_its_content_stops_at_the_limit() {
    let result = read_file(Path::new("/dev/zero"), 10);

    assert!(matches!(
        result,
        Err(PricesError::TooLarge {
            check: SizeCheck::Read,
            ..
        })
    ));
}

#[tokio::test]
async fn a_bad_body_leaves_the_previous_table_byte_identical() {
    let all_invalid = br#"{"acme":{"models":{"m1":{"cost":{"input":-1,"output":20000}}}}}"#;
    let rows: [(&str, &[u8], &str); 4] = [
        ("malformed json, file", b"{\"acme\": {", "file"),
        ("all prices invalid, file", all_invalid, "file"),
        ("malformed json, url", b"{\"acme\": {", "url"),
        ("all prices invalid, url", all_invalid, "url"),
    ];
    for (name, body, via) in rows {
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = b"previous table bytes\n";
        let target = dir.path().join(PRICES_FILE);
        std::fs::write(&target, previous).expect("seed");
        let server = MockServer::start().await;
        serve_once(
            &server,
            "/api.json",
            ResponseTemplate::new(200).set_body_bytes(body),
        )
        .await;
        let source_file = dir.path().join("source.json");
        std::fs::write(&source_file, body).expect("source");
        let from = match via {
            "file" => source_file.display().to_string(),
            _ => format!("{}/api.json", server.uri()),
        };

        let result = refresh_into(dir.path(), Some(&from), date()).await;

        assert!(
            matches!(result, Err(PricesError::Table(_))),
            "{name}: {result:?}"
        );
        assert_eq!(std::fs::read(&target).expect("read"), previous, "{name}");
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["prices.json", "source.json"], "{name}");
    }
}

#[tokio::test]
async fn a_valid_body_replaces_the_previous_table_with_the_canonical_form() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join(PRICES_FILE);
    std::fs::write(&target, b"previous table bytes\n").expect("seed");
    let source = dir.path().join("source.json");
    std::fs::write(&source, valid_body()).expect("source");

    let summary = refresh_into(dir.path(), Some(&source.display().to_string()), date())
        .await
        .expect("refresh");

    let written = std::fs::read(&target).expect("read");
    let reparsed = routectl_router::prices::parse_and_validate(&written).expect("canonical");
    assert_eq!(reparsed.generated_at(), date());
    assert_eq!((summary.providers, summary.models), (1, 1));
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["prices.json", "source.json"],
        "the refresh created nothing else in the config folder"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_refresh_into_a_missing_config_folder_creates_it_and_the_table_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().expect("tempdir");
    let config_dir = root.path().join("absent").join("routectl");
    let source = root.path().join("source.json");
    std::fs::write(&source, valid_body()).expect("source");

    refresh_into(&config_dir, Some(&source.display().to_string()), date())
        .await
        .expect("refresh");

    assert!(config_dir.join(PRICES_FILE).is_file());
    let mode_of = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    let folder = mode_of(&config_dir);
    assert_eq!(folder, 0o700, "config folder mode {folder:o}");
    let file = mode_of(&config_dir.join(PRICES_FILE));
    assert_eq!(file, 0o600, "table file mode {file:o}");
}

#[test]
fn a_write_into_an_unusable_directory_fails_without_a_temp_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"file").expect("blocker");

    let result = write_atomic(&blocker.join(PRICES_FILE), b"{}");

    assert!(matches!(result, Err(PricesError::Write { .. })));
    assert_eq!(std::fs::read_dir(dir.path()).expect("list").count(), 1);
}

/// A failed directory fsync after the rename is reported as a WARN with the
/// error kind; a successful one logs nothing.
#[test]
fn a_failed_directory_fsync_warns_with_its_error_kind() {
    let rows = [
        (
            "fsync failed",
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            Some("PermissionDenied"),
        ),
        ("fsync succeeded", Ok(()), None),
    ];
    for (name, result, expected) in rows {
        let events = routectl_testkit::capture_events(|| warn_on_parent_sync_failure(result));

        let warned: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .collect();
        assert_eq!(
            warned.first().and_then(|e| e.field("io_error")),
            expected,
            "{name}: {events:?}"
        );
        assert_eq!(warned.len(), usize::from(expected.is_some()), "{name}");
    }
}

fn assert_fetch_line_is_stripped(name: &str, lines: &[String]) {
    let fetch_line = lines
        .iter()
        .find(|l| l.contains("prices refresh: fetching"))
        .unwrap_or_else(|| panic!("{name}: no fetch line in {lines:?}"));
    assert!(fetch_line.contains("/x"), "{name}: {fetch_line}");
    for line in lines {
        for leaked in ["u:p", "k=v", "frag"] {
            assert!(!line.contains(leaked), "{name}: {leaked} in {line}");
        }
    }
}

#[tokio::test]
async fn the_logged_url_drops_userinfo_query_and_fragment() {
    let server = MockServer::start().await;
    serve_once(
        &server,
        "/x",
        ResponseTemplate::new(200).set_body_bytes(valid_body()),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let from = format!("http://u:p@{}/x?k=v#frag", server.address());

    let (result, lines) =
        routectl_testkit::capture_lines(refresh_into(dir.path(), Some(&from), date())).await;

    result.expect("control: the stub refresh succeeds");
    assert_fetch_line_is_stripped("stub", &lines);
}

#[tokio::test]
async fn the_logged_url_for_a_raw_https_url_drops_userinfo_and_query() {
    // The proxy is a closed loopback port, so the named host is never
    // resolved and the fetch fails after the log line is written.
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:9").expect("proxy"))
        .build()
        .expect("client");
    let target = url("https://u:p@h/x?k=v");

    let (result, lines) =
        routectl_testkit::capture_lines(fetch(&client, &target, MAX_INPUT_BYTES)).await;

    assert!(result.is_err(), "the closed proxy refuses the fetch");
    assert_fetch_line_is_stripped("raw https", &lines);
}

#[tokio::test]
async fn output_and_logs_carry_counts_and_the_date_never_an_id() {
    let dir = tempfile::tempdir().expect("tempdir");

    let (summary, lines) =
        routectl_testkit::capture_lines(refresh_into(dir.path(), Some(SOURCE_SLICE), date())).await;

    let summary = summary.expect("slice refreshes");
    let printed = summary.to_string();
    assert!(summary.models > 0, "control: the slice has rows");
    assert_eq!(
        printed,
        format!(
            "prices refreshed: {} models from {} providers, table dated 2026-11-09",
            summary.models, summary.providers
        )
    );
    let ids = slice_ids();
    assert!(
        ids.iter().any(|id| id == "opencode-go"),
        "control: ids collected"
    );
    for id in &ids {
        assert!(!printed.contains(id.as_str()), "{id} printed");
        for line in &lines {
            assert!(!line.contains(id.as_str()), "{id} logged: {line}");
        }
    }
}

/// A source row whose id the table rules skip is counted on the summary
/// line, never named; the table is still written. The control is the same
/// source without that row, whose line carries no skip clause.
#[tokio::test]
async fn skipped_source_rows_are_counted_on_the_summary_line() {
    let skipped_id = "glm\u{7F}5";
    let rows = [
        (
            "with an unusable id",
            true,
            "; 1 source rows skipped for an unusable id",
        ),
        ("control", false, ""),
    ];
    for (name, with_bad_row, suffix) in rows {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut models = serde_json::json!({ "kept": { "cost": { "input": 1.0 } } });
        if with_bad_row {
            models[skipped_id] = serde_json::json!({ "cost": { "input": 2.0 } });
        }
        let source = dir.path().join("api.json");
        std::fs::write(
            &source,
            serde_json::to_vec(&serde_json::json!({ "acme": { "models": models } })).unwrap(),
        )
        .unwrap();

        let summary = refresh_into(dir.path(), source.to_str(), date())
            .await
            .expect(name);

        assert_eq!(
            summary.to_string(),
            format!("prices refreshed: 1 models from 1 providers, table dated 2026-11-09{suffix}"),
            "{name}"
        );
        assert!(
            dir.path().join(PRICES_FILE).exists(),
            "{name}: the table is written"
        );
    }
}

#[test]
fn source_selection_reads_urls_by_scheme_and_everything_else_as_a_file() {
    let rows: [(&str, Option<&str>, Option<Source>); 6] = [
        ("default", None, Some(Source::Url(url(DEFAULT_SOURCE_URL)))),
        (
            "https url",
            Some("HTTPS://example.com/a.json"),
            Some(Source::Url(url("https://example.com/a.json"))),
        ),
        (
            "userinfo dropped",
            Some("http://u:p@example.com/a.json"),
            Some(Source::Url(url("http://example.com/a.json"))),
        ),
        (
            "relative file",
            Some("api.json"),
            Some(Source::File(PathBuf::from("api.json"))),
        ),
        ("http url without host", Some("http://"), None),
        (
            "name shorter than a scheme",
            Some("ht"),
            Some(Source::File(PathBuf::from("ht"))),
        ),
    ];
    for (name, from, expected) in rows {
        assert_eq!(Source::parse(from).ok(), expected, "{name}");
    }
}
