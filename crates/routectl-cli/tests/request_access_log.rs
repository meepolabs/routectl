//! Access-line level of the per-request span, driven through the
//! production sink: read-only polling paths close their span at DEBUG,
//! inference paths at INFO, and every rejection WARN carries its
//! `request_id` exactly once, whether or not the request span is enabled.
//!
//! Its own integration binary so the request-span callsites are first
//! registered under the subscribers these cases install.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, header};
use axum::routing::{get, post};
use routectl_cli::log_sink::{self, Clock};
use routectl_cli::server::auth::{TokenSet, auth_layer};
use routectl_cli::server::request_id;
use routectl_cli::server::status_gate::{
    STATUS_MAX_INFLIGHT, StatusHostAllowlist, apply_overload_layers, host_guard,
};
use tokio::sync::Semaphore;
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

/// A polling and an inference route behind the listener auth layer, under the
/// request-id middleware as production stacks them (request id outermost).
fn authed_app() -> Router {
    Router::new()
        .route("/status", get(|| async { "ok" }))
        .route("/v1/messages", post(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(TokenSet::new(vec!["listener-token".to_owned()])),
            auth_layer,
        ))
        .layer(axum::middleware::from_fn(request_id::middleware))
}

/// `/status` behind the production host guard, under the request-id
/// middleware as production stacks them.
fn host_guarded_status_app() -> Router {
    let bound: SocketAddr = "127.0.0.1:8787".parse().expect("socket addr");
    Router::new()
        .route("/status", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            StatusHostAllowlist::new(bound),
            host_guard,
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

/// Drive one request through `app` on a current-thread runtime under the
/// production subscriber at INFO; return the response and the sink text.
fn drive_at_info(app: Router, request: Request<Body>) -> (Response<Body>, String) {
    let buffer = Buffer::default();
    let subscriber = production_subscriber("info", &buffer);
    let response = tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(app.oneshot(request))
            .expect("infallible router")
    });
    (response, buffer.text())
}

fn echoed_request_id(response: &Response<Body>) -> String {
    response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id echoed")
        .to_owned()
}

/// The single WARN line containing `message`, asserting it names `echoed`
/// as its `request_id` exactly once.
fn assert_one_warn_with_request_id_once(out: &str, message: &str, echoed: &str) {
    let warns: Vec<&str> = out
        .lines()
        .filter(|line| line.contains(" WARN ") && line.contains(message))
        .collect();
    assert_eq!(warns.len(), 1, "{out:?}");
    assert!(
        warns[0].contains(&format!("request_id={echoed}")),
        "{warns:?}"
    );
    assert_eq!(warns[0].matches("request_id=").count(), 1, "{warns:?}");
}

/// A rejected poll's span is DEBUG and so off at INFO; a rejected inference
/// request's span is INFO and already prints the id. Either way the WARN
/// names it exactly once.
#[test]
fn listener_auth_rejection_names_the_request_id_once_at_info() {
    for (method, path) in [("GET", "/status"), ("POST", "/v1/messages")] {
        // Arrange
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .expect("request");

        // Act
        let (response, out) = drive_at_info(authed_app(), request);

        // Assert
        assert_eq!(response.status(), 401, "{path}");
        let echoed = echoed_request_id(&response);
        assert_one_warn_with_request_id_once(&out, "listener auth rejected", &echoed);
    }
}

/// The host-guard refusal is sampled; this binary drives no other refusal, so
/// this one is the process's first and is always logged.
#[test]
fn host_guard_rejection_names_the_request_id_once_at_info() {
    // Arrange
    let request = Request::builder()
        .method("GET")
        .uri("/status")
        .header(header::HOST, "rebind.example:8787")
        .body(Body::empty())
        .expect("request");

    // Act
    let (response, out) = drive_at_info(host_guarded_status_app(), request);

    // Assert
    assert_eq!(response.status(), 403);
    let echoed = echoed_request_id(&response);
    assert_one_warn_with_request_id_once(&out, "disallowed authority claim", &echoed);
}

#[derive(Clone)]
struct Hold {
    arrived: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

async fn hold(State(hold): State<Hold>) -> &'static str {
    hold.arrived.fetch_add(1, Ordering::SeqCst);
    let _permit = hold.release.acquire().await.expect("release semaphore");
    "ok"
}

/// The overload shed is sampled; this binary drives no other shed, so this
/// one is the process's first and is always logged. Parked handlers hold
/// every permit on the single runtime thread, so the extra request finds none.
#[test]
fn overload_shed_names_the_request_id_once_at_info() {
    // Arrange
    let held = Hold {
        arrived: Arc::new(AtomicUsize::new(0)),
        release: Arc::new(Semaphore::new(0)),
    };
    let app = apply_overload_layers(
        Router::new()
            .route("/status", get(hold))
            .with_state(held.clone()),
    )
    .layer(axum::middleware::from_fn(request_id::middleware));
    let poll = || {
        Request::builder()
            .method("GET")
            .uri("/status")
            .body(Body::empty())
            .expect("request")
    };
    let buffer = Buffer::default();
    let subscriber = production_subscriber("info", &buffer);

    // Act
    let response = tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                let parked: Vec<_> = (0..STATUS_MAX_INFLIGHT)
                    .map(|_| tokio::spawn(app.clone().oneshot(poll())))
                    .collect();
                while held.arrived.load(Ordering::SeqCst) < STATUS_MAX_INFLIGHT {
                    tokio::task::yield_now().await;
                }
                let shed = app
                    .clone()
                    .oneshot(poll())
                    .await
                    .expect("infallible router");
                held.release.add_permits(STATUS_MAX_INFLIGHT);
                for handle in parked {
                    let served = handle.await.expect("join").expect("infallible router");
                    assert_eq!(served.status(), 200);
                }
                shed
            })
    });

    // Assert
    assert_eq!(response.status(), 503);
    let echoed = echoed_request_id(&response);
    assert_one_warn_with_request_id_once(&buffer.text(), "shed a request", &echoed);
}
