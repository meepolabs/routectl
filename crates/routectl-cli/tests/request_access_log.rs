//! Access-line level of the per-request span, driven through the
//! production sink: read-only polling paths close their span at DEBUG,
//! inference paths at INFO, and a rejected poll's WARN still carries its
//! `request_id`.
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
use routectl_cli::server::auth::{TokenSet, auth_layer};
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

/// `/status` behind the listener auth layer, under the request-id
/// middleware as production stacks them (request id outermost).
fn authed_status_app() -> Router {
    Router::new()
        .route("/status", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(TokenSet::new(vec!["listener-token".to_owned()])),
            auth_layer,
        ))
        .layer(axum::middleware::from_fn(request_id::middleware))
}

fn production_subscriber(
    directive: &str,
    buffer: &Buffer,
) -> Box<dyn tracing::Subscriber + Send + Sync> {
    let sink = buffer.clone();
    log_sink::subscriber(
        EnvFilter::new(directive),
        move || sink.clone(),
        false,
        Clock::Off,
    )
}

/// Send one request through the request-id middleware under the
/// production subscriber at `directive` and return what the sink wrote.
fn access_log(directive: &str, method: &str, path: &str) -> String {
    let buffer = Buffer::default();
    let subscriber = production_subscriber(directive, &buffer);
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

#[test]
fn rejected_status_poll_warn_carries_the_echoed_request_id_at_info() {
    // Arrange
    let buffer = Buffer::default();
    let subscriber = production_subscriber("info", &buffer);
    let request = Request::builder()
        .method("GET")
        .uri("/status")
        .body(Body::empty())
        .expect("request");

    // Act
    let response = tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(authed_status_app().oneshot(request))
            .expect("infallible router")
    });

    // Assert
    assert_eq!(response.status(), 401);
    let echoed = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id echoed")
        .to_owned();
    let out = buffer.text();
    let warns: Vec<&str> = out
        .lines()
        .filter(|line| line.contains(" WARN ") && line.contains("listener auth rejected"))
        .collect();
    assert_eq!(warns.len(), 1, "{out:?}");
    assert!(
        warns[0].contains(&format!("request_id={echoed}")),
        "{warns:?}"
    );
}
