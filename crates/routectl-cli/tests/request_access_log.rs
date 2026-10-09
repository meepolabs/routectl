//! Access-line level of the per-request span, driven through the
//! production sink: read-only polling paths close their span at DEBUG,
//! inference paths at INFO.
//!
//! Its own integration binary so the request-span callsites are first
//! registered under the subscribers these cases install.

use std::io;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::{get, post};
use routectl_cli::log_sink::{self, Clock};
use routectl_cli::server::request_id;
use tower::ServiceExt;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("buffer lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("buffer lock").clone()).expect("sink wrote UTF-8")
    }
}

fn app() -> Router {
    Router::new()
        .route("/status", get(|| async { "ok" }))
        .route("/v1/messages", post(|| async { "ok" }))
        .layer(axum::middleware::from_fn(request_id::middleware))
}

/// Send one request through the request-id middleware under the
/// production subscriber at `directive` and return what the sink wrote.
fn access_log(directive: &str, method: &str, path: &str) -> String {
    let buffer = Buffer::default();
    let sink = buffer.clone();
    let subscriber = log_sink::subscriber(
        EnvFilter::new(directive),
        move || sink.clone(),
        false,
        Clock::Off,
    );
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("request");
    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let response = runtime
            .block_on(app().oneshot(request))
            .expect("infallible router");
        assert!(
            response.status().is_success(),
            "status {}",
            response.status()
        );
    });
    buffer.text()
}

fn span_close_lines<'a>(out: &'a str, path: &str) -> Vec<&'a str> {
    let field = format!("path={path} ");
    out.lines()
        .filter(|line| line.contains("request{") && line.contains(&field) && line.contains("close"))
        .collect()
}

#[test]
fn status_poll_writes_no_access_line_at_info() {
    // Arrange / Act
    let out = access_log("info", "GET", "/status");

    // Assert
    assert!(span_close_lines(&out, "/status").is_empty(), "{out:?}");
}

#[test]
fn inference_request_writes_one_access_line_at_info() {
    // Arrange / Act
    let out = access_log("info", "POST", "/v1/messages");

    // Assert
    let lines = span_close_lines(&out, "/v1/messages");
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(lines[0].contains(" INFO "), "{lines:?}");
}

#[test]
fn status_poll_access_line_appears_at_debug() {
    // Arrange / Act
    let out = access_log("debug", "GET", "/status");

    // Assert
    let lines = span_close_lines(&out, "/status");
    assert_eq!(lines.len(), 1, "{out:?}");
    assert!(lines[0].contains("DEBUG "), "{lines:?}");
}
