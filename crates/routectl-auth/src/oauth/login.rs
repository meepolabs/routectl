//! Login flow driver. Runs an OAuth 2.0 PKCE flow end-to-end:
//!
//! 1. Generate PKCE verifier/challenge + CSRF state.
//! 2. Bind a local TCP listener on `127.0.0.1:<ephemeral_port>`.
//! 3. Spawn an `axum` sub-app on that listener with one route at the
//!    provider's `callback_path`. Validates `state`, captures `code`.
//! 4. Build the auth URL and launch the browser via `webbrowser`
//!    (or print the URL and read the redirect from stdin under
//!    `--print-url`).
//! 5. Await the callback handler with a 120s timeout.
//! 6. POST to the token endpoint; persist the resulting `TokenRecord`.
//!
//! The login flow runs entirely on the operator's machine -- there is
//! no server-side state in routectl. PKCE verifier and state token
//! live only as stack/heap variables in this driver and are dropped
//! when `run` returns.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::oauth::pkce::{Pkce, constant_time_eq};
use crate::oauth::providers::{self, AuthParams, OAuthFlow};
use crate::oauth::rate_limit::{Decision, RateLimitTracker};
use crate::oauth::store::OAuthStore;
use crate::oauth::types::seat_key;
use crate::oauth::{OAuthError, OAuthResult};

/// Knobs the operator can override on the CLI.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct LoginOptions {
    /// Print the auth URL to stdout and read the redirect from stdin
    /// instead of launching a browser. For SSH / headless sessions.
    pub print_url: bool,
    /// Override the local callback port. Default: kernel-assigned
    /// ephemeral port via bind on `127.0.0.1:0`.
    pub callback_port: Option<u16>,
    /// Seat label. `None` writes the default (unlabeled) seat -- the
    /// bare provider key, byte-for-byte today's behavior. `Some(label)`
    /// writes a labeled seat (`provider#label`) without touching the
    /// default seat, so an operator can register a pool of same-provider
    /// credentials.
    pub label: Option<String>,
}

impl LoginOptions {
    /// Build a default options bag and tweak via builder calls.
    /// Required because `#[non_exhaustive]` blocks struct-literal
    /// construction from outside the crate.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_print_url(mut self, print_url: bool) -> Self {
        self.print_url = print_url;
        self
    }

    pub fn with_callback_port(mut self, port: Option<u16>) -> Self {
        self.callback_port = port;
        self
    }

    pub fn with_label(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }
}

/// Run the login flow for `provider_id` against `store`. On success,
/// persists tokens and returns the authenticated provider id (which
/// matches the input verbatim; passed back for symmetry with logout).
///
/// `options.label` selects the seat the resulting record is written
/// under: `None` writes the default (unlabeled) seat -- the bare
/// provider key -- exactly as before; `Some(label)` writes the labeled
/// seat `provider#label` without disturbing the default seat. The
/// freshly-exchanged `TokenRecord` carries its own per-record identity
/// (the codex flow mints a new `session_id` on every fresh exchange),
/// so each seat gets a stable identity of its own.
pub async fn run(
    provider_id: &str,
    store: &OAuthStore,
    options: LoginOptions,
) -> OAuthResult<String> {
    let flow = providers::lookup(provider_id)?;
    let seat = seat_key(flow.provider_id(), options.label.as_deref());

    if options.print_url {
        run_print_url(flow, store, &seat).await
    } else {
        run_browser(flow, store, options.callback_port, &seat).await
    }
}

/// Browser-launched flow: bind a listener, spawn axum, open browser,
/// await the callback. Split into three named helpers so each step is
/// scannable and the failure points are at the visible top of the
/// function.
async fn run_browser(
    flow: &'static dyn OAuthFlow,
    store: &OAuthStore,
    requested_port: Option<u16>,
    seat: &str,
) -> OAuthResult<String> {
    let pkce = Pkce::generate();

    let bind = bind_callback_listener(requested_port, flow.callback_port_candidates()).await?;
    // Bind on 127.0.0.1 (no DNS, no IPv6 races) but advertise the
    // redirect_uri using the provider's registered callback host. Most
    // public clients (claude.ai, codex) register their allowed redirect
    // URIs against `localhost`, which browsers resolve back to 127.0.0.1
    // so the callback still lands on our listener. xAI registered its
    // redirect against the literal `127.0.0.1` instead, so the host is
    // provider-specific (see `OAuthFlow::callback_host`).
    let redirect_uri = format!(
        "http://{}:{}{}",
        flow.callback_host(),
        bind.port,
        flow.callback_path()
    );

    let (code_rx, server_handle) =
        spawn_callback_server(flow, bind.listener, pkce.state().to_string());

    let auth_url = flow.auth_url(&AuthParams {
        challenge: pkce.challenge(),
        state: pkce.state(),
        redirect_uri: &redirect_uri,
    });
    launch_browser_or_print_url(flow, &auth_url);

    let cb = match tokio::time::timeout(Duration::from_secs(120), code_rx).await {
        Ok(Ok(cb)) => cb,
        Ok(Err(_)) => return Err(OAuthError::Internal("callback channel closed".into())),
        Err(_) => {
            server_handle.abort();
            return Err(OAuthError::LoginTimeout(flow.provider_id().into()));
        }
    };

    // Stop the server once we have the code, no matter how the rest of
    // the function exits. axum's `with_graceful_shutdown` is wired to
    // the oneshot the handler fires; this `timeout` just reaps the
    // task. axum::serve does NOT install ctrl-c handlers; the shutdown
    // oneshot is the only stop signal.
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;

    let code = cb.code?;
    let record = flow
        .exchange_code(
            store.http(),
            &code,
            pkce.verifier(),
            pkce.state(),
            &redirect_uri,
        )
        .await?;
    store.write_record(seat, record.clone()).await?;

    print_login_success(flow, &record);
    Ok(flow.provider_id().to_string())
}

struct BoundCallback {
    listener: tokio::net::TcpListener,
    port: u16,
}

/// Step 1: bind a TCP listener for the callback server.
///
/// Port-selection precedence (highest first):
/// 1. `requested_port` -- the operator's explicit `--callback-port`
///    override. Binds exactly that port; no fallback (the operator
///    asked for a specific one, so a clash is their signal to fix it).
/// 2. `flow_candidates` -- the ordered list from the provider's
///    `OAuthFlow::callback_port_candidates()`. Tried in order; the first
///    available port is bound.
async fn bind_callback_listener(
    requested_port: Option<u16>,
    flow_candidates: Vec<u16>,
) -> OAuthResult<BoundCallback> {
    let candidates = bind_port_candidates(requested_port, flow_candidates);
    let mut last_err = None;
    for bind_port in candidates {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, bind_port)).await {
            Ok(listener) => {
                let port = listener
                    .local_addr()
                    .map_err(|e| OAuthError::Io(format!("local_addr: {e}")))?
                    .port();
                return Ok(BoundCallback { listener, port });
            }
            Err(e) => {
                last_err = Some(OAuthError::Io(format!("bind 127.0.0.1:{bind_port}: {e}")));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| OAuthError::Io("no candidate callback port to bind".into())))
}

/// Ordered list of ports `bind_callback_listener` will try. Pure so the
/// precedence rules are unit-testable without opening sockets. See
/// `bind_callback_listener` for the precedence rationale.
fn bind_port_candidates(requested_port: Option<u16>, flow_candidates: Vec<u16>) -> Vec<u16> {
    // Explicit operator override wins outright; the flow's candidate list
    // is ignored.
    if let Some(p) = requested_port {
        return vec![p];
    }
    flow_candidates
}

/// Step 2: spawn the one-shot callback server on `listener`. Returns the
/// receiver side of the callback channel and a JoinHandle the caller
/// can abort on timeout.
fn spawn_callback_server(
    flow: &'static dyn OAuthFlow,
    listener: tokio::net::TcpListener,
    expected_state: String,
) -> (
    oneshot::Receiver<CallbackResult>,
    tokio::task::JoinHandle<()>,
) {
    let (code_tx, code_rx) = oneshot::channel::<CallbackResult>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let app_state = Arc::new(CallbackState {
        provider_id: flow.provider_id(),
        expected_state,
        tx: tokio::sync::Mutex::new(Some(code_tx)),
        shutdown: tokio::sync::Mutex::new(Some(shutdown_tx)),
        rate_limit: std::sync::Mutex::new(RateLimitTracker::default()),
    });

    let app = Router::new()
        .route(flow.callback_path(), get(callback_handler))
        .with_state(app_state);

    let server = tokio::spawn(async move {
        // `into_make_service_with_connect_info::<SocketAddr>()` is what
        // makes the per-request `ConnectInfo<SocketAddr>` extractor
        // populated; without it, the rate-limit branch of the handler
        // panics at extraction time.
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    (code_rx, server)
}

/// Step 3: tell the operator where to authorize, and try to launch the
/// browser. A failure to auto-launch is not fatal -- the URL was
/// already printed, so the operator can paste it manually.
fn launch_browser_or_print_url(flow: &'static dyn OAuthFlow, auth_url: &url::Url) {
    println!("Opening browser for {} login...", flow.display_name());
    println!("If the browser does not open, paste this URL manually:");
    println!("  {auth_url}");
    if let Err(e) = webbrowser::open(auth_url.as_str()) {
        eprintln!("(could not auto-open browser: {e})");
    }
}

/// Headless flow: print the URL, read redirect from stdin. The
/// upstream's `manual_redirect_url` (Anthropic claude.ai uses
/// `https://platform.claude.com/oauth/code/callback`) shows the
/// operator a `code#state` pair after they authorize.
async fn run_print_url(
    flow: &'static dyn OAuthFlow,
    store: &OAuthStore,
    seat: &str,
) -> OAuthResult<String> {
    let pkce = Pkce::generate();
    let redirect_uri = flow.manual_redirect_url().to_string();
    let auth_url = flow.auth_url(&AuthParams {
        challenge: pkce.challenge(),
        state: pkce.state(),
        redirect_uri: &redirect_uri,
    });

    println!("Open this URL in any browser to log in:");
    println!("\n  {auth_url}\n");
    println!(
        "After authorizing, paste the value shown on the success page \
         (looks like `<code>#<state>`):"
    );

    let line = read_line_capped(16 * 1024)?;
    let trimmed = line.trim();
    let (code, state) = trimmed.split_once('#').ok_or_else(|| {
        OAuthError::Internal("input must be `<code>#<state>` (no '#' separator found)".into())
    })?;

    if !constant_time_eq(state, pkce.state()) {
        return Err(OAuthError::StateMismatch(flow.provider_id().into()));
    }

    let record = flow
        .exchange_code(
            store.http(),
            code,
            pkce.verifier(),
            pkce.state(),
            &redirect_uri,
        )
        .await?;
    store.write_record(seat, record.clone()).await?;

    print_login_success(flow, &record);
    Ok(flow.provider_id().to_string())
}

/// Read one line of input from stdin, capping the read at `max_bytes`
/// so a misbehaving terminal cannot funnel a multi-GB blob into RAM.
/// Excess bytes return `OAuthError::Io`. Strips a single trailing
/// `\r?\n` so callers can `.trim()` like a normal line read.
///
/// Uses `BufRead::read_line` rather than `Read::take(N).read_to_end(...)`.
/// The old approach kept reading until EOF or the byte cap: on an
/// interactive TTY in canonical (line-buffered) mode that blocked past
/// the first newline because `read_to_end` only returns on EOF or a
/// full-cap buffer. `BufRead::read_line` returns as soon as it sees a
/// newline, which is what the operator expects after pressing Enter.
fn read_line_capped(max_bytes: usize) -> OAuthResult<String> {
    let stdin = std::io::stdin();
    let handle = stdin.lock();
    read_line_from_bufread(handle, max_bytes)
}

/// Core of `read_line_capped`, factored out so unit tests can drive it
/// with a `Cursor<&[u8]>` without redirecting stdin.
fn read_line_from_bufread(reader: impl std::io::BufRead, max_bytes: usize) -> OAuthResult<String> {
    use std::io::BufRead as _;

    let mut line = String::new();
    // Bound the underlying reader at `max_bytes + 1` BEFORE `read_line`
    // buffers anything: a newline-free blob is otherwise read in full
    // (up to EOF) before the cap check, defeating the cap. The `+ 1`
    // lets an exactly-`max_bytes` line through while still surfacing the
    // first over-cap byte as `n > max_bytes`.
    let mut limited = reader.take(max_bytes as u64 + 1);
    // `read_line` appends until the first newline or EOF. On a canonical
    // TTY it returns immediately after the user presses Enter. `n` is the
    // number of raw bytes placed into `line` (including the newline if
    // one was present).
    let n = limited
        .read_line(&mut line)
        .map_err(|e| OAuthError::Io(format!("read stdin: {e}")))?;
    // n == 0 means EOF with no bytes (Ctrl-D on an empty line). Treat as
    // an empty line; the caller's split_once('#') produces a clear error.
    if n > max_bytes {
        return Err(OAuthError::Io(format!(
            "input exceeds {max_bytes} bytes; refusing to load"
        )));
    }
    // Strip a single trailing \r?\n.
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

fn print_login_success(flow: &'static dyn OAuthFlow, rec: &crate::oauth::TokenRecord) {
    let who = rec
        .account
        .email
        .as_deref()
        .or(rec.account.account_id.as_deref())
        .unwrap_or("(no account info)");
    println!(
        "Logged in to {} as {who} (provider id: {}). Token expires in {} seconds.",
        flow.display_name(),
        flow.provider_id(),
        rec.expires_at_unix.saturating_sub(rec.obtained_at_unix)
    );
}

// ---------------- callback handler ----------------

struct CallbackState {
    provider_id: &'static str,
    expected_state: String,
    tx: tokio::sync::Mutex<Option<oneshot::Sender<CallbackResult>>>,
    shutdown: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
    // Per-source-port rejection tracker. `std::sync::Mutex` (not the
    // tokio one) because the operations are short, synchronous, and
    // never held across an `.await`.
    rate_limit: std::sync::Mutex<RateLimitTracker>,
}

#[derive(Debug)]
struct CallbackResult {
    code: OAuthResult<String>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<CallbackState>>,
    Query(q): Query<CallbackQuery>,
) -> (StatusCode, Html<&'static str>) {
    // SECURITY: validate the CSRF `state` query param BEFORE consuming
    // the one-shot or signaling shutdown. The redirect URI binds on
    // 127.0.0.1 and is reachable by any co-resident local process; only
    // the genuine browser callback knows the state token issued in the
    // auth URL. Treating a state-missing or state-mismatched hit (incl.
    // `?error=...` from a co-resident process) as terminal would let
    // any local actor abort an in-flight login by sending an arbitrary
    // GET to /callback. On a rejected hit, return 400 and leave the
    // listener up so the legitimate browser callback still resolves.
    //
    // This gate applies to BOTH branches (`error=...` and `code=...`):
    // both must echo `state` to be acknowledged.
    if !state_matches(&q, &state.expected_state) {
        // Sustained-rejection rate limit. State-valid hits never enter
        // this branch, so legitimate browser traffic bypasses the
        // tracker entirely; only co-resident noise contributes. The
        // rate limit escalates from 400 to 429 on the same response
        // shape (no `state.tx`/`state.shutdown` touch), so an abusive
        // port still cannot abort the wait.
        let decision = state
            .rate_limit
            .lock()
            .expect("rate limit mutex poisoned")
            .record_rejection(addr.port(), Instant::now());
        let status = match decision {
            Decision::Admitted => StatusCode::BAD_REQUEST,
            Decision::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        };
        return (status, Html(REJECTED_HTML));
    }

    let result = decode_callback(state.provider_id, &state.expected_state, q);
    let html = match &result {
        Ok(_) => SUCCESS_HTML,
        Err(_) => FAILURE_HTML,
    };
    // Send the result + trigger graceful shutdown. Both are one-shots
    // taken once; if the callback fires twice (browser refresh, prefetch),
    // the second hit just renders the HTML without sending again.
    if let Some(tx) = state.tx.lock().await.take() {
        let _ = tx.send(CallbackResult { code: result });
    }
    if let Some(sd) = state.shutdown.lock().await.take() {
        let _ = sd.send(());
    }
    (StatusCode::OK, Html(html))
}

/// Constant-time check that the callback's `state` query param matches
/// the CSRF token issued in the auth URL. Missing state counts as a
/// mismatch (we never issue an empty state token, and an unauthenticated
/// hit must not be treated as a legitimate callback).
fn state_matches(q: &CallbackQuery, expected: &str) -> bool {
    match q.state.as_deref() {
        Some(s) => constant_time_eq(s, expected),
        None => false,
    }
}

fn decode_callback(
    provider_id: &str,
    expected_state: &str,
    q: CallbackQuery,
) -> OAuthResult<String> {
    // Defense in depth: validate state again here even though the
    // callback_handler gate already checked it. Direct callers (unit
    // tests, future inline use) must see the same contract.
    let state = q
        .state
        .ok_or_else(|| OAuthError::Internal("callback missing `state` query param".into()))?;
    if !constant_time_eq(&state, expected_state) {
        return Err(OAuthError::StateMismatch(provider_id.to_string()));
    }
    if let Some(err) = q.error {
        // The redirect can be triggered by any local process that finds
        // the callback port. Sanitize the operator-visible parts so a
        // malicious caller cannot inject control characters or megabyte
        // payloads into the operator's terminal log.
        let detail = q.error_description.unwrap_or_default();
        let combined = format!("{err}: {detail}");
        return Err(OAuthError::TokenEndpoint(sanitize_error_blurb(&combined)));
    }
    q.code
        .ok_or_else(|| OAuthError::Internal("callback missing `code` query param".into()))
}

/// Cap and strip control chars from operator-visible error blurbs. Caps
/// at 200 chars total; allows space (`0x20`) but rejects any other
/// control character.
fn sanitize_error_blurb(s: &str) -> String {
    const MAX_CHARS: usize = 200;
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(MAX_CHARS)
        .collect();
    cleaned
}

const SUCCESS_HTML: &str = r#"<!doctype html>
<html><head><title>routectl: login complete</title></head>
<body style="font-family:system-ui,sans-serif;max-width:520px;margin:4em auto;padding:2em;
             border:1px solid #ddd;border-radius:8px;">
<h2>Login complete.</h2>
<p>You can close this tab and return to your terminal.</p>
</body></html>
"#;

const FAILURE_HTML: &str = r#"<!doctype html>
<html><head><title>routectl: login failed</title></head>
<body style="font-family:system-ui,sans-serif;max-width:520px;margin:4em auto;padding:2em;
             border:1px solid #ddd;border-radius:8px;">
<h2>Login failed.</h2>
<p>Check the routectl terminal for details, then re-run <code>routectl login</code>.</p>
</body></html>
"#;

/// Body returned when a request reaches the callback URL without the
/// expected CSRF `state` query param. Kept short and deliberately
/// uninformative -- a co-resident process probing the port should not
/// learn what the listener is waiting for.
const REJECTED_HTML: &str = r#"<!doctype html>
<html><head><title>routectl</title></head>
<body><p>Invalid callback.</p></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    // ----- read_line_from_bufread tests -----

    #[test]
    fn read_line_from_bufread_returns_first_line_only() {
        // Multi-line input: only the first line (up to and including the
        // first newline) is consumed and returned without the trailing
        // newline. The second line stays unread in the buffer.
        use std::io::Cursor;
        let input = b"first-line\nsecond-line\n";
        let result = read_line_from_bufread(Cursor::new(input), 16 * 1024).unwrap();
        assert_eq!(result, "first-line");
    }

    #[test]
    fn read_line_from_bufread_strips_crlf() {
        use std::io::Cursor;
        let input = b"data\r\n";
        let result = read_line_from_bufread(Cursor::new(input), 1024).unwrap();
        assert_eq!(result, "data");
    }

    #[test]
    fn read_line_from_bufread_cap_exceeded_is_error() {
        // A line (including its newline) longer than max_bytes must error.
        // "toolong\n" is 8 bytes; cap is 4.
        use std::io::Cursor;
        let input = b"toolong\n";
        let err = read_line_from_bufread(Cursor::new(input), 4).unwrap_err();
        assert!(
            err.to_string().contains("exceeds"),
            "expected cap error, got: {err}"
        );
    }

    #[test]
    fn read_line_from_bufread_line_exactly_at_cap_is_ok() {
        // n == max_bytes is NOT an overrun (check is `>`, not `>=`).
        // "ab\n" is 3 bytes; cap is 3. Should succeed.
        use std::io::Cursor;
        let input = b"ab\n";
        let result = read_line_from_bufread(Cursor::new(input), 3).unwrap();
        assert_eq!(result, "ab");
    }

    #[test]
    fn read_line_from_bufread_eof_without_newline_returns_content() {
        // EOF without a trailing newline: the content is returned as-is.
        use std::io::Cursor;
        let input = b"no-newline";
        let result = read_line_from_bufread(Cursor::new(input), 1024).unwrap();
        assert_eq!(result, "no-newline");
    }

    #[test]
    fn read_line_from_bufread_empty_input_returns_empty_string() {
        // Ctrl-D / EOF with no bytes: n == 0, returns empty string.
        use std::io::Cursor;
        let result = read_line_from_bufread(Cursor::new(b""), 1024).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn read_line_from_bufread_newline_free_over_cap_is_error() {
        // The cap must fire on a newline-FREE blob longer than max_bytes,
        // not just a long line that happens to contain a newline. Without
        // the `take(max_bytes + 1)` bound, `read_line` would buffer the
        // whole blob (to EOF) before the cap check; with it, the read
        // stops at max_bytes + 1 and the `n > max_bytes` check fires.
        use std::io::Cursor;
        let input = vec![b'x'; 4096]; // no newline anywhere
        let err = read_line_from_bufread(Cursor::new(input), 64).unwrap_err();
        assert!(
            err.to_string().contains("exceeds"),
            "expected cap error on newline-free over-cap input, got: {err}"
        );
    }

    #[test]
    fn read_line_from_bufread_unbounded_newline_free_input_is_bounded() {
        // The load-bearing reason for the `take(max_bytes + 1)` bound: an
        // unbounded, newline-free reader must NOT be buffered in full. A
        // `BufReader<Repeat>` yields an endless stream of 'x' with no
        // newline; the pre-fix `read_line` would loop until OOM. The
        // bound makes the cap fire after reading only max_bytes + 1 bytes,
        // so this test returns promptly with the cap error instead of
        // hanging or exhausting memory.
        use std::io::BufReader;
        let infinite = BufReader::new(std::io::repeat(b'x'));
        let err = read_line_from_bufread(infinite, 64).unwrap_err();
        assert!(
            err.to_string().contains("exceeds"),
            "unbounded input must hit the cap, got: {err}"
        );
    }

    #[test]
    fn read_line_from_bufread_at_cap_newline_free_returns_content() {
        // A newline-free blob exactly at the cap is NOT an overrun and
        // round-trips unchanged (the `+ 1` headroom lets the full
        // max_bytes through).
        use std::io::Cursor;
        let input = vec![b'a'; 64];
        let result = read_line_from_bufread(Cursor::new(input), 64).unwrap();
        assert_eq!(result, "a".repeat(64));
    }

    #[test]
    fn read_line_from_bufread_normal_line_round_trips_stripped() {
        // A normal line with a trailing \n comes back stripped.
        use std::io::Cursor;
        let input = b"hunter2\n";
        let result = read_line_from_bufread(Cursor::new(input), 1024).unwrap();
        assert_eq!(result, "hunter2");
    }

    // ----- bind_port_candidates tests -----

    #[test]
    fn bind_port_candidates_default_is_ephemeral_only() {
        // The Anthropic path: no explicit override, flow returns [0].
        // The candidate list must be exactly [0] so binding stays
        // byte-for-byte the old ephemeral-port behavior.
        assert_eq!(bind_port_candidates(None, vec![0]), vec![0]);
    }

    #[test]
    fn bind_port_candidates_preferred_then_fallback() {
        // Codex flow returns [1455, 1457]: try 1455, then 1457.
        assert_eq!(
            bind_port_candidates(None, vec![1455, 1457]),
            vec![1455, 1457]
        );
    }

    #[test]
    fn bind_port_candidates_operator_override_wins_with_no_fallback() {
        // An explicit --callback-port beats the flow's candidate list and
        // does not fall back.
        assert_eq!(
            bind_port_candidates(Some(9000), vec![1455, 1457]),
            vec![9000]
        );
        assert_eq!(bind_port_candidates(Some(9000), vec![0]), vec![9000]);
    }

    #[test]
    fn decode_callback_extracts_code_when_state_matches() {
        let q = CallbackQuery {
            code: Some("CODE".into()),
            state: Some("S".into()),
            error: None,
            error_description: None,
        };
        let code = decode_callback("anthropic", "S", q).unwrap();
        assert_eq!(code, "CODE");
    }

    #[test]
    fn decode_callback_rejects_state_mismatch() {
        let q = CallbackQuery {
            code: Some("CODE".into()),
            state: Some("WRONG".into()),
            error: None,
            error_description: None,
        };
        let err = decode_callback("anthropic", "S", q).unwrap_err();
        match err {
            OAuthError::StateMismatch(provider) => {
                assert_eq!(provider, "anthropic", "provider should propagate");
            }
            other => panic!("expected StateMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_callback_state_mismatch_uses_provider_id() {
        // If a future provider with a different id calls decode_callback
        // (codex), the StateMismatch must report THAT provider's
        // id, not a hardcoded "anthropic".
        let q = CallbackQuery {
            code: Some("CODE".into()),
            state: Some("WRONG".into()),
            error: None,
            error_description: None,
        };
        let err = decode_callback("codex", "S", q).unwrap_err();
        match err {
            OAuthError::StateMismatch(p) => assert_eq!(p, "codex"),
            other => panic!("expected StateMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_callback_surfaces_oauth_error_param() {
        // After the state-first reordering, surfacing an `error=...`
        // response only happens once state is validated -- the legitimate
        // browser callback echoes both. A noise hit without state is
        // rejected upstream by `state_matches`; this test exercises the
        // post-validation branch.
        let q = CallbackQuery {
            code: None,
            state: Some("S".into()),
            error: Some("access_denied".into()),
            error_description: Some("user clicked cancel".into()),
        };
        let err = decode_callback("anthropic", "S", q).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("access_denied"), "msg: {msg}");
        assert!(msg.contains("user clicked cancel"), "msg: {msg}");
    }

    #[test]
    fn decode_callback_missing_state_is_internal_error() {
        // Browser prefetch hits the redirect URL with neither `state`
        // nor `code` -- it should not panic and should not collapse to
        // a state mismatch.
        let q = CallbackQuery {
            code: Some("CODE".into()),
            state: None,
            error: None,
            error_description: None,
        };
        let err = decode_callback("anthropic", "S", q).unwrap_err();
        match err {
            OAuthError::Internal(msg) => assert!(msg.contains("state")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn decode_callback_missing_code_after_state_match_is_internal_error() {
        // The state matches but no `code` arrived -- treat as a
        // protocol error rather than silently returning empty.
        let q = CallbackQuery {
            code: None,
            state: Some("S".into()),
            error: None,
            error_description: None,
        };
        let err = decode_callback("anthropic", "S", q).unwrap_err();
        match err {
            OAuthError::Internal(msg) => assert!(msg.contains("code")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn decode_callback_sanitizes_control_chars_in_error() {
        let q = CallbackQuery {
            code: None,
            state: Some("S".into()),
            error: Some("bad\x00\x01stuff".into()),
            error_description: Some("line1\nline2\twith tab".into()),
        };
        let err = decode_callback("anthropic", "S", q).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains('\x00'), "null byte leaked: {msg:?}");
        assert!(!msg.contains('\n'), "newline leaked: {msg:?}");
        assert!(!msg.contains('\t'), "tab leaked: {msg:?}");
        assert!(msg.contains("badstuff"));
    }

    #[test]
    fn decode_callback_caps_error_blurb_length() {
        let huge = "x".repeat(10_000);
        let q = CallbackQuery {
            code: None,
            state: Some("S".into()),
            error: Some("huge".into()),
            error_description: Some(huge),
        };
        let err = decode_callback("anthropic", "S", q).unwrap_err();
        // The error string itself includes prefix wrapping; the
        // sanitized inner blurb must fit under MAX_CHARS (200) plus the
        // "token endpoint returned an error: " prefix from Display.
        assert!(
            err.to_string().len() < 400,
            "blurb not capped, got len {}",
            err.to_string().len()
        );
    }

    #[test]
    fn state_matches_requires_present_and_equal() {
        let with = |s: Option<&str>| CallbackQuery {
            code: None,
            state: s.map(str::to_string),
            error: None,
            error_description: None,
        };
        assert!(state_matches(&with(Some("expected")), "expected"));
        assert!(!state_matches(&with(Some("other")), "expected"));
        assert!(!state_matches(&with(Some("")), "expected"));
        assert!(!state_matches(&with(None), "expected"));
    }

    /// Live-server regression for the loopback callback CSRF gate.
    ///
    /// Spawns the same callback sub-app run() uses against a real
    /// 127.0.0.1 listener on a kernel-assigned port. Sends three
    /// unauthenticated GETs (no state, wrong state, no params at all)
    /// and asserts each returns 400 AND leaves the wait-future pending
    /// -- the listener must not let a co-resident process abort the
    /// login. Then sends a state-valid hit and asserts the wait
    /// resolves with the expected code.
    #[tokio::test]
    async fn callback_handler_rejects_unauthenticated_hits() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();

        let flow = providers::lookup("anthropic").expect("known provider");
        let expected_state = "expected-csrf-state-token";
        let (mut code_rx, server_handle) =
            spawn_callback_server(flow, listener, expected_state.to_string());

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}{}", flow.callback_path());

        // Hit 1: ?error=access_denied with NO state. The pre-fix
        // handler took both one-shots on this path and shut down; the
        // fix must reject with 400 and leave the wait pending.
        let resp = client
            .get(format!("{base}?error=access_denied"))
            .send()
            .await
            .expect("noise hit completes");
        assert_eq!(
            resp.status().as_u16(),
            400,
            "noise hit must not be acknowledged"
        );
        assert_wait_pending(&mut code_rx, "after error+no-state hit");

        // Hit 2: code+state, but state does not match.
        let resp = client
            .get(format!("{base}?code=ATTACKER&state=wrong"))
            .send()
            .await
            .expect("wrong-state hit completes");
        assert_eq!(resp.status().as_u16(), 400);
        assert_wait_pending(&mut code_rx, "after wrong-state hit");

        // Hit 3: no params at all (e.g. browser prefetch).
        let resp = client.get(&base).send().await.expect("empty hit completes");
        assert_eq!(resp.status().as_u16(), 400);
        assert_wait_pending(&mut code_rx, "after empty-query hit");

        // Hit 4: legitimate browser callback. NOW the wait must
        // resolve with the code we sent.
        let resp = client
            .get(format!("{base}?code=GOODCODE&state={expected_state}"))
            .send()
            .await
            .expect("legit hit completes");
        assert_eq!(resp.status().as_u16(), 200);

        let cb = tokio::time::timeout(Duration::from_secs(2), code_rx)
            .await
            .expect("wait must resolve once state matches")
            .expect("callback channel must not drop");
        assert_eq!(
            cb.code.expect("legit hit yields a code"),
            "GOODCODE",
            "wait resolved with the wrong code"
        );

        // Reap the server task; the legit hit fired the shutdown
        // one-shot so axum::serve will return.
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    /// Helper: assert the callback wait-future is still pending, i.e.
    /// no one has called `tx.send` on the one-shot. `try_recv` returns
    /// `Empty` when the sender is alive and has not sent; any other
    /// outcome (including `Closed`) means the wait was terminated.
    fn assert_wait_pending(rx: &mut oneshot::Receiver<CallbackResult>, ctx: &str) {
        match rx.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("callback channel closed by noise hit ({ctx})");
            }
            Ok(cb) => panic!("noise hit terminated the wait ({ctx}): {cb:?}"),
        }
    }
}

// Handler-level tests for the per-source-port rate limit. Split into
// a sibling file so login.rs stays under the 800-line cap; the
// `#[path]` mod declaration keeps it a child of `login`, so private
// items remain accessible from the test code.
#[cfg(test)]
#[path = "login_rate_limit_tests.rs"]
mod rate_limit_tests;
