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

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::oauth::pkce::{constant_time_eq, Pkce};
use crate::oauth::providers::{self, AuthParams, OAuthFlow};
use crate::oauth::store::OAuthStore;
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
}

/// Run the login flow for `provider_id` against `store`. On success,
/// persists tokens and returns the authenticated provider id (which
/// matches the input verbatim; passed back for symmetry with logout).
pub async fn run(
    provider_id: &str,
    store: &OAuthStore,
    options: LoginOptions,
) -> OAuthResult<String> {
    let flow = providers::lookup(provider_id)?;

    if options.print_url {
        run_print_url(flow, store).await
    } else {
        run_browser(flow, store, options.callback_port).await
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
) -> OAuthResult<String> {
    let pkce = Pkce::generate();

    let bind = bind_callback_listener(requested_port).await?;
    // Bind on 127.0.0.1 (no DNS, no IPv6 races) but advertise the
    // redirect_uri using `localhost` because claude.ai's allowed
    // redirect URIs for the public client are registered against
    // `localhost`, not `127.0.0.1`. Browsers resolve `localhost`
    // back to 127.0.0.1, so the callback still lands on our listener.
    let redirect_uri = format!("http://localhost:{}{}", bind.port, flow.callback_path());

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
    store
        .write_record(flow.provider_id(), record.clone())
        .await?;

    print_login_success(flow, &record);
    Ok(flow.provider_id().to_string())
}

struct BoundCallback {
    listener: tokio::net::TcpListener,
    port: u16,
}

/// Step 1: bind a TCP listener on the requested port (or 0 for an
/// ephemeral kernel-assigned port).
async fn bind_callback_listener(requested_port: Option<u16>) -> OAuthResult<BoundCallback> {
    let bind_port = requested_port.unwrap_or(0);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, bind_port))
        .await
        .map_err(|e| OAuthError::Io(format!("bind 127.0.0.1:{bind_port}: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OAuthError::Io(format!("local_addr: {e}")))?
        .port();
    Ok(BoundCallback { listener, port })
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
    });

    let app = Router::new()
        .route(flow.callback_path(), get(callback_handler))
        .with_state(app_state);

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
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
async fn run_print_url(flow: &'static dyn OAuthFlow, store: &OAuthStore) -> OAuthResult<String> {
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
    store
        .write_record(flow.provider_id(), record.clone())
        .await?;

    print_login_success(flow, &record);
    Ok(flow.provider_id().to_string())
}

/// Read one line of input from stdin, capping the read at `max_bytes`
/// so a misbehaving terminal cannot funnel a multi-GB blob into RAM.
/// Excess bytes return `OAuthError::Io`. Strips a single trailing
/// `\r?\n` so callers can `.trim()` like a normal line read.
fn read_line_capped(max_bytes: usize) -> OAuthResult<String> {
    use std::io::Read;
    let stdin = std::io::stdin();
    // Read::take wraps the stdin lock in a length-limited reader; we
    // request `max_bytes + 1` so we can detect overrun (a read of
    // exactly `max_bytes + 1` means the input did not fit).
    let mut handle = stdin.lock().take((max_bytes + 1) as u64);
    let mut buf = Vec::new();
    handle
        .read_to_end(&mut buf)
        .map_err(|e| OAuthError::Io(format!("read stdin: {e}")))?;
    if buf.len() > max_bytes {
        return Err(OAuthError::Io(format!(
            "input exceeds {max_bytes} bytes; refusing to load"
        )));
    }
    let mut line = String::from_utf8(buf)
        .map_err(|e| OAuthError::Io(format!("stdin input is not valid UTF-8: {e}")))?;
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
    State(state): State<Arc<CallbackState>>,
    Query(q): Query<CallbackQuery>,
) -> Html<&'static str> {
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
    Html(html)
}

fn decode_callback(
    provider_id: &str,
    expected_state: &str,
    q: CallbackQuery,
) -> OAuthResult<String> {
    if let Some(err) = q.error {
        // The redirect can be triggered by any local process that finds
        // the callback port. Sanitize the operator-visible parts so a
        // malicious caller cannot inject control characters or megabyte
        // payloads into the operator's terminal log.
        let detail = q.error_description.unwrap_or_default();
        let combined = format!("{err}: {detail}");
        return Err(OAuthError::TokenEndpoint(sanitize_error_blurb(&combined)));
    }
    let state = q
        .state
        .ok_or_else(|| OAuthError::Internal("callback missing `state` query param".into()))?;
    if !constant_time_eq(&state, expected_state) {
        return Err(OAuthError::StateMismatch(provider_id.to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // (codex in a prior change), the StateMismatch must report THAT provider's
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
        let q = CallbackQuery {
            code: None,
            state: None,
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
            state: None,
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
            state: None,
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
}
