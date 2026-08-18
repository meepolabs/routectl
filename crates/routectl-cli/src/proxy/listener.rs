//! CONNECT front-listener for the MITM front-proxy: the assembly point
//! that binds a loopback TCP port, speaks the HTTP `CONNECT` tunneling
//! protocol a client configures via `HTTPS_PROXY`, and dispatches each
//! accepted connection to one of two fates -- decrypt-and-classify (the
//! configured `mitm_host`) via [`handle_mitm_connection`], or an opaque
//! byte-for-byte blind tunnel (any other host) via
//! `tokio::io::copy_bidirectional`.
//!
//! The blind tunnel exists because `HTTPS_PROXY` is process-global: a
//! client like Claude Code that is pointed at this proxy for
//! `api.anthropic.com` traffic ALSO routes its telemetry, Sentry, and
//! any other outbound HTTPS through the same proxy. Without a
//! passthrough for every other host, that unrelated traffic would break
//! outright. The blind tunnel never terminates TLS, never touches a
//! cert, and never decodes a byte of what it relays -- it is pure TCP
//! plumbing, deliberately outside the MITM decrypt/classify path.
//!
//! This module owns binding the socket and constructing the shared
//! [`MitmCtx`] exactly once at proxy startup; it does not decide
//! whether the MITM feature is enabled at all (that gate lives in
//! `server::serve_on_listener`, keyed on `Config::mitm.is_some()`) and
//! it does not decide what happens after handoff to [`handle_mitm_connection`]
//! (that is `proxy::mitm` and `proxy::split`'s job).

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio_rustls::TlsAcceptor;

use super::ca;
use super::cc_version::CcVersionWarnGuard;
use super::forward::{
    DEFAULT_MAX_CONCURRENT_STREAMS, ForwardState, STREAM_IDLE_WINDOW, build_client,
};
use super::metrics::{Leg, PathClass, ProxyMetrics, ResultClass, WarnOnce};
use super::mitm::{MitmCtx, handle_mitm_connection};

/// Upper bound on the size of a CONNECT request's request-line + header
/// block this listener will buffer while looking for the terminating
/// blank line. A legitimate `CONNECT host:port HTTP/1.1` line plus a
/// handful of headers a real client (or `curl`) sends is well under 1
/// KiB; this is a generous leak/DoS backstop, not a tuning knob.
const MAX_CONNECT_HEADER_BYTES: usize = 8 * 1024;

/// How long [`handle_connection`] waits for a complete CONNECT header
/// block (the terminating CRLFCRLF) before giving up and dropping the
/// connection. Without this, a client that opens the TCP socket and
/// then withholds or drips bytes forever (slowloris-style) would hold
/// its accepted-connection slot -- and the [`Semaphore`] permit
/// acquired before the parse (see [`handle_connection`]'s doc) --
/// indefinitely. Ten seconds is generous for any real CONNECT client
/// (the request-line + headers is a single small write) while bounding
/// how long a stalled connection can occupy proxy capacity.
const CONNECT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on concurrently accepted connections held by [`handle_connection`],
/// from the moment a connection is accepted (BEFORE the CONNECT header
/// block is even parsed) through the end of whichever leg it dispatches
/// to -- a blind tunnel's `copy_bidirectional` relay loop or a MITM
/// connection's full decrypt/classify/forward lifetime. Acquiring the
/// permit this early (rather than only around the blind-tunnel/MITM
/// handoff) is what bounds a flood of pre-parse-stalled connections: each
/// one holds a permit for at most [`CONNECT_READ_TIMEOUT`] before
/// [`read_connect_target`] times out and the connection (and its permit)
/// is dropped.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// How often the accept loop flushes a [`ProxyMetrics`] snapshot to
/// `tracing` while the proxy is running. The counters are write-only
/// without a periodic emission: a front-proxy runs for hours, so a
/// shutdown-only flush would surface nothing during a live session.
/// This fires on the accept loop's own timer, once per interval -- never
/// on the per-request or per-connection path -- and the counters are
/// flushed once more at graceful shutdown so a session shorter than one
/// interval still surfaces its totals.
const METRICS_SNAPSHOT_INTERVAL: Duration = Duration::from_mins(1);

/// The `host:port` a client's CONNECT request asked to tunnel to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectTarget {
    host: String,
    port: u16,
}

/// Outcome of successfully reading a complete CONNECT request off the
/// wire: either a well-formed target, or a request that parsed as
/// "headers arrived" but not as a valid CONNECT line -- distinguished
/// from an early-EOF close ([`read_connect_target`] returns `Ok(None)`
/// for that) so a malformed-but-complete request gets an explicit 400
/// rather than being silently dropped like a client that never finished
/// sending.
#[derive(Debug)]
enum ConnectRequest {
    Target(ConnectTarget),
    Malformed,
}

/// Parses the first line of a buffered CONNECT request. Deliberately
/// tolerant of an IPv6 literal authority (`[::1]:443`): splitting on the
/// LAST `:` via `rsplit_once` lands on the port separator regardless of
/// how many colons the host portion itself contains.
fn parse_connect_request(raw: &[u8]) -> ConnectRequest {
    let Ok(text) = std::str::from_utf8(raw) else {
        return ConnectRequest::Malformed;
    };
    let Some(request_line) = text.split("\r\n").next() else {
        return ConnectRequest::Malformed;
    };
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        return ConnectRequest::Malformed;
    };
    if !method.eq_ignore_ascii_case("CONNECT") {
        return ConnectRequest::Malformed;
    }
    let Some(authority) = parts.next() else {
        return ConnectRequest::Malformed;
    };
    let Some((host, port_str)) = authority.rsplit_once(':') else {
        return ConnectRequest::Malformed;
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return ConnectRequest::Malformed;
    };
    if host.is_empty() {
        return ConnectRequest::Malformed;
    }
    ConnectRequest::Target(ConnectTarget {
        host: host.to_string(),
        port,
    })
}

/// Reads byte-by-byte off `stream` until the CRLFCRLF that terminates a
/// CONNECT request's header block, then parses the buffered bytes.
///
/// Byte-at-a-time is deliberate, not an oversight: this parse runs
/// exactly once per accepted TCP connection (never per request), so its
/// per-byte syscall cost is negligible, and it guarantees the read never
/// consumes a single byte past the header terminator. Over-reading here
/// would strand client bytes that belong to the tunnel body (the TLS
/// ClientHello for a MITM-host CONNECT, or the first payload byte for a
/// blind tunnel) inside this function's own buffer, and neither
/// [`handle_mitm_connection`] nor the blind-tunnel copy loop below
/// accepts a pre-buffered prefix -- both read directly from the raw
/// [`TcpStream`] handed to them.
///
/// Returns `Ok(None)` on a clean EOF before the terminator arrives (the
/// client closed before finishing its request); `Err` on a genuine I/O
/// error or exceeding [`MAX_CONNECT_HEADER_BYTES`].
async fn read_connect_target(stream: &mut TcpStream) -> io::Result<Option<ConnectRequest>> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.push(byte[0]);
        // The size cap is checked BEFORE the terminator check on every
        // iteration (not after, and not only on non-break paths): a
        // request whose CRLFCRLF lands exactly on the byte that would
        // have tripped the cap must still be rejected, not admitted
        // because the terminator happened to be found first.
        if buf.len() > MAX_CONNECT_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CONNECT request header block exceeded the size limit",
            ));
        }
        if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
            break;
        }
    }
    Ok(Some(parse_connect_request(&buf)))
}

async fn respond_connect_established(stream: &mut TcpStream) -> io::Result<()> {
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
}

async fn respond_bad_request(stream: &mut TcpStream) -> io::Result<()> {
    stream
        .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
        .await
}

async fn respond_bad_gateway(stream: &mut TcpStream) -> io::Result<()> {
    stream
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
        .await
}

/// Relays raw bytes between `client` and `upstream` (both already live,
/// established connections), with no TLS termination and no
/// interpretation of the stream contents whatsoever -- see the module
/// doc for why this exists. Always brackets the tunnel's lifetime with
/// `metrics.stream_opened()`/`stream_closed()` and records exactly one
/// `incr_request(Leg::BlindTunnel, ..)` outcome per tunnel (a relay-level
/// I/O error as [`ResultClass::ServerError`]; a clean close on either
/// side as [`ResultClass::Success`]). `path_class` is always
/// [`PathClass::Unknown`]: a blind tunnel has no HTTP path to classify.
/// The dial itself (and its [`ResultClass::Unreachable`] accounting)
/// happens in [`handle_connection`], BEFORE the client is ever told
/// `200 Connection Established` -- see that function's doc for why.
async fn run_blind_tunnel(
    mut client: TcpStream,
    mut upstream: TcpStream,
    host: String,
    port: u16,
    metrics: Arc<ProxyMetrics>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    metrics.stream_opened();
    let copy_result = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    metrics.stream_closed();

    let result_class = match copy_result {
        Ok(_) => ResultClass::Success,
        Err(error) => {
            tracing::debug!(
                target: "routectl_cli::proxy::listener",
                host = %host,
                port,
                %error,
                "blind tunnel ended with an I/O error"
            );
            ResultClass::ServerError
        }
    };
    metrics.incr_request(Leg::BlindTunnel, result_class, PathClass::Unknown);
}

/// Handles one accepted TCP connection end-to-end: acquires a connection
/// slot, reads the CONNECT request (bounded by [`CONNECT_READ_TIMEOUT`]),
/// then either hands the raw stream to [`handle_mitm_connection`] (target
/// host matches `mitm_host`, case-insensitively -- DNS hostnames are not
/// case-sensitive, and a client is free to send
/// `CONNECT API.Anthropic.COM:443`) or dials the blind-tunnel target
/// BEFORE answering the CONNECT at all. Dialing first (rather than
/// answering `200` immediately and dialing inside the spawned relay task)
/// matters: a client that already received `200 Connection Established`
/// reasonably believes the tunnel is live and will start writing tunnel
/// bytes into it, so an unreachable target must be reported as a
/// CONNECT-level failure (`502`), never as a false "established" followed
/// by a silent later disconnect. The MITM branch has no equivalent dial
/// (there is nothing to reach except the local TLS terminator), so it
/// answers `200` immediately. Never panics; every failure path logs and
/// returns, dropping the connection.
///
/// The [`MAX_CONCURRENT_CONNECTIONS`] permit is acquired FIRST, before
/// the CONNECT header is even read, and is held all the way through
/// whichever leg the connection dispatches to (moved into the spawned
/// blind-tunnel or MITM task rather than dropped at handoff) -- see that
/// constant's doc for why the permit's scope starts this early.
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    ctx: Arc<MitmCtx>,
    mitm_host: Arc<str>,
    connection_semaphore: Arc<Semaphore>,
) {
    let permit = match connection_semaphore.acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            // The semaphore is never explicitly closed anywhere in this
            // module; unreachable in practice, kept fail-safe rather than
            // panicking the accept loop's spawned task.
            tracing::error!(
                target: "routectl_cli::proxy::listener",
                peer = %peer,
                "MITM proxy connection concurrency semaphore unexpectedly closed"
            );
            return;
        }
    };

    let target = match tokio::time::timeout(CONNECT_READ_TIMEOUT, read_connect_target(&mut stream))
        .await
    {
        Ok(Ok(Some(ConnectRequest::Target(target)))) => target,
        Ok(Ok(Some(ConnectRequest::Malformed))) => {
            tracing::warn!(
                target: "routectl_cli::proxy::listener",
                peer = %peer,
                "rejecting malformed CONNECT request"
            );
            let _ = respond_bad_request(&mut stream).await;
            return;
        }
        Ok(Ok(None)) => {
            tracing::debug!(
                target: "routectl_cli::proxy::listener",
                peer = %peer,
                "connection closed before a complete CONNECT request arrived"
            );
            return;
        }
        Ok(Err(error)) => {
            tracing::warn!(
                target: "routectl_cli::proxy::listener",
                peer = %peer,
                %error,
                "failed to read CONNECT request"
            );
            let _ = respond_bad_request(&mut stream).await;
            return;
        }
        Err(_elapsed) => {
            tracing::warn!(
                target: "routectl_cli::proxy::listener",
                peer = %peer,
                timeout_secs = CONNECT_READ_TIMEOUT.as_secs(),
                "dropping connection: no complete CONNECT request arrived within the read timeout"
            );
            return;
        }
    };

    if target.host.eq_ignore_ascii_case(&mitm_host) {
        if let Err(error) = respond_connect_established(&mut stream).await {
            tracing::debug!(
                target: "routectl_cli::proxy::listener",
                peer = %peer,
                %error,
                "failed to write CONNECT 200 response"
            );
            return;
        }
        tokio::spawn(handle_mitm_connection(stream, acceptor, ctx, permit));
        return;
    }

    let upstream = match TcpStream::connect((target.host.as_str(), target.port)).await {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::warn!(
                target: "routectl_cli::proxy::listener",
                host = %target.host,
                port = target.port,
                %error,
                "blind-tunnel target unreachable"
            );
            ctx.metrics.incr_request(
                Leg::BlindTunnel,
                ResultClass::Unreachable,
                PathClass::Unknown,
            );
            let _ = respond_bad_gateway(&mut stream).await;
            return;
        }
    };

    if let Err(error) = respond_connect_established(&mut stream).await {
        tracing::debug!(
            target: "routectl_cli::proxy::listener",
            peer = %peer,
            %error,
            "failed to write CONNECT 200 response"
        );
        return;
    }

    let metrics = Arc::clone(&ctx.metrics);
    tokio::spawn(run_blind_tunnel(
        stream,
        upstream,
        target.host,
        target.port,
        metrics,
        permit,
    ));
}

/// The accept loop: binds nothing itself (the listener is already bound
/// by [`build_and_bind`]) and runs until `shutdown` fires, spawning one
/// [`handle_connection`] task per accepted connection. A transient
/// accept error is logged and does not stop the loop -- only the
/// shutdown signal does.
async fn run_listener(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    ctx: Arc<MitmCtx>,
    mitm_host: Arc<str>,
    mut shutdown: watch::Receiver<()>,
) {
    let connection_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    // Skip the immediate first tick `interval` would otherwise fire at
    // t=0 (an all-zero snapshot at startup carries no signal).
    let mut snapshot_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + METRICS_SNAPSHOT_INTERVAL,
        METRICS_SNAPSHOT_INTERVAL,
    );
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                ctx.metrics.log_snapshot();
                tracing::info!(
                    target: "routectl_cli::proxy::listener",
                    "MITM proxy listener shutting down"
                );
                return;
            }
            _ = snapshot_tick.tick() => {
                ctx.metrics.log_snapshot();
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let acceptor = acceptor.clone();
                        let ctx = Arc::clone(&ctx);
                        let mitm_host = Arc::clone(&mitm_host);
                        let connection_semaphore = Arc::clone(&connection_semaphore);
                        tokio::spawn(handle_connection(
                            stream,
                            peer,
                            acceptor,
                            ctx,
                            mitm_host,
                            connection_semaphore,
                        ));
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "routectl_cli::proxy::listener",
                            %error,
                            "MITM proxy listener accept failed"
                        );
                    }
                }
            }
        }
    }
}

/// The subset of the operator's `[mitm]` config block [`build_and_bind`]
/// needs, plus `reinject_port` (the MAIN routectl listener's bound
/// port, resolved at runtime by the caller -- never a config value,
/// since the main listener's port is only known once it is actually
/// bound). Deliberately plain data rather than `routectl_router::MitmConfig`
/// itself: the `proxy` module imports nothing from outside its own
/// tree except the config crate's plain types it is handed, keeping the
/// coupling to exactly the fields this module uses.
pub struct ProxyListenerConfig {
    pub listen_port: u16,
    pub cert_dir: PathBuf,
    pub mitm_host: String,
    pub upstream_origin: String,
    pub reinject_port: u16,
    /// The operator's `[mitm].tested_cc_version`, threaded straight into
    /// the shared `MitmCtx` for the CC-version warn-and-proceed check
    /// (see `proxy::cc_version`).
    pub tested_cc_version: Option<String>,
    /// The SAME `Arc<MitmSeamNonce>` the server bootstrap put on `AppState`
    /// -- threaded through so the reinject leg stamps the exact value the
    /// ingress admission/capture gates compare against. Generated ONCE per
    /// process, never here.
    pub seam_nonce: Arc<crate::ingress::MitmSeamNonce>,
}

/// Failure to start the MITM proxy listener. Every variant is a
/// startup-time, operator-actionable condition; the caller
/// (`server::serve_on_listener`) logs this loudly and continues serving
/// its own HTTP listener without the MITM front rather than crashing
/// the whole process -- see the module doc on why a degraded MITM
/// proxy is not a fatal condition for the main server.
#[derive(Debug, thiserror::Error)]
pub enum ProxyStartError {
    #[error("failed to prepare the MITM CA/leaf certificate: {0}")]
    Cert(#[from] ca::CaError),
    #[error("[mitm] upstream_origin {origin:?} is not a valid URL: {reason}")]
    UpstreamOrigin { origin: String, reason: String },
    #[error("failed to build the MITM forward-leg HTTP client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("failed to bind the MITM proxy listener on 127.0.0.1:{port}: {source}")]
    Bind { port: u16, source: std::io::Error },
}

/// Builds the TLS acceptor, the shared [`MitmCtx`], and binds the
/// loopback listener socket -- everything [`spawn`] needs, constructed
/// exactly once at proxy startup. Never itself spawns a task; the
/// caller decides when (and on which shutdown channel) to hand the
/// result to [`spawn`], which lets the caller log a successful bind
/// (with the OS-assigned port, if `listen_port` was 0) before the
/// accept loop starts.
pub async fn build_and_bind(
    config: ProxyListenerConfig,
) -> Result<(TcpListener, TlsAcceptor, Arc<MitmCtx>), ProxyStartError> {
    let acceptor = ca::load_or_create(&config.cert_dir, &config.mitm_host)?;

    let client = build_client()?;
    let forward_state =
        ForwardState::new(client, DEFAULT_MAX_CONCURRENT_STREAMS, STREAM_IDLE_WINDOW);
    let metrics = Arc::new(ProxyMetrics::new());
    let warn_once = Arc::new(WarnOnce::new());
    let upstream_origin =
        Url::parse(&config.upstream_origin).map_err(|source| ProxyStartError::UpstreamOrigin {
            origin: config.upstream_origin.clone(),
            reason: source.to_string(),
        })?;
    // Always `http://` (never `https://`): this is a loopback re-inject
    // into routectl's own listener, not a call to the real upstream.
    let reinject_base = Url::parse(&format!("http://127.0.0.1:{}", config.reinject_port))
        .expect("a loopback origin with a valid port always parses as a URL");

    let ctx = Arc::new(MitmCtx {
        forward_state,
        metrics,
        warn_once,
        upstream_origin,
        reinject_base,
        tested_cc_version: config.tested_cc_version,
        cc_version_warn_guard: CcVersionWarnGuard::new(),
        seam_nonce: config.seam_nonce,
    });

    let addr = format!("127.0.0.1:{}", config.listen_port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|source| ProxyStartError::Bind {
            port: config.listen_port,
            source,
        })?;

    Ok((listener, acceptor, ctx))
}

/// Spawns the accept loop built from [`build_and_bind`]'s output,
/// observing `shutdown` (the same channel the reload pipeline watches
/// in `server::serve_on_listener`) so both stop on the same signal.
/// Returns the `JoinHandle` for the caller to retain until shutdown.
pub fn spawn(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    ctx: Arc<MitmCtx>,
    mitm_host: String,
    shutdown: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_listener(
        listener,
        acceptor,
        ctx,
        Arc::from(mitm_host),
        shutdown,
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rustls_pki_types::pem::PemObject;
    use tokio::net::TcpSocket;
    use tokio::sync::watch;
    use wiremock::MockServer;

    use super::*;

    /// How long a test waits for the side under test to close its half of
    /// a connection before treating the read as a failure. Any regression
    /// whose failure shape is "the stream never closes" would otherwise
    /// STALL a parallel cargo test shard on an unbounded read to EOF, and
    /// a stalled test is a strictly worse diagnosis than a red assertion
    /// (observed: mutating one of these paths hung past ten minutes rather
    /// than failing).
    const CLOSE_DEADLINE: Duration = Duration::from_secs(5);

    /// Read `stream` to EOF under [`CLOSE_DEADLINE`] and decode it lossily.
    /// `unclosed_means` states what a stream that never closes would prove
    /// about the code under test, so a blown deadline reads as the specific
    /// regression rather than as a bare timeout.
    async fn read_to_close<S: tokio::io::AsyncRead + Unpin>(
        stream: &mut S,
        unclosed_means: &str,
    ) -> String {
        let mut response = Vec::new();
        tokio::time::timeout(CLOSE_DEADLINE, stream.read_to_end(&mut response))
            .await
            .expect(unclosed_means)
            .unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    #[test]
    fn parse_connect_request_parses_host_and_port() {
        let raw = b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\n\r\n";
        match parse_connect_request(raw) {
            ConnectRequest::Target(target) => {
                assert_eq!(target.host, "api.anthropic.com");
                assert_eq!(target.port, 443);
            }
            other => panic!("expected a parsed target, got {other:?}"),
        }
    }

    #[test]
    fn parse_connect_request_handles_ipv6_literal_authority() {
        let raw = b"CONNECT [::1]:8443 HTTP/1.1\r\n\r\n";
        match parse_connect_request(raw) {
            ConnectRequest::Target(target) => {
                assert_eq!(target.host, "[::1]");
                assert_eq!(target.port, 8443);
            }
            other => panic!("expected a parsed target, got {other:?}"),
        }
    }

    #[test]
    fn parse_connect_request_rejects_non_connect_method() {
        let raw = b"GET / HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_connect_request(raw),
            ConnectRequest::Malformed
        ));
    }

    #[test]
    fn parse_connect_request_rejects_missing_port() {
        let raw = b"CONNECT api.anthropic.com HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_connect_request(raw),
            ConnectRequest::Malformed
        ));
    }

    #[test]
    fn parse_connect_request_rejects_empty_host() {
        let raw = b"CONNECT :443 HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_connect_request(raw),
            ConnectRequest::Malformed
        ));
    }

    #[tokio::test]
    async fn read_connect_target_returns_none_on_early_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_connect_target(&mut stream).await
        });

        let client = TcpStream::connect(addr).await.unwrap();
        // Half a request line, then drop the connection before the
        // terminating CRLFCRLF ever arrives.
        let mut client = client;
        client.write_all(b"CONNECT api.anthropic").await.unwrap();
        drop(client);

        let result = server.await.unwrap();
        assert!(
            matches!(result, Ok(None)),
            "an early EOF must return Ok(None), not an error or a parsed target"
        );
    }

    #[tokio::test]
    async fn read_connect_target_rejects_a_header_block_past_the_size_cap() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_connect_target(&mut stream).await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        // Oversized filler header value, never terminated by CRLFCRLF
        // within the cap -- must be rejected before the terminator is
        // ever found, not admitted because a byte happens to look like
        // the end of the block.
        let oversized = "a".repeat(MAX_CONNECT_HEADER_BYTES + 64);
        client
            .write_all(
                format!("CONNECT api.anthropic.com:443 HTTP/1.1\r\nX-Filler: {oversized}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();

        let result = server.await.unwrap();
        assert!(
            result.is_err(),
            "a header block past MAX_CONNECT_HEADER_BYTES must be rejected"
        );
    }

    #[tokio::test]
    async fn read_connect_target_never_consumes_bytes_past_the_terminator() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let outcome = read_connect_target(&mut stream).await.unwrap();
            // Whatever the client sent AFTER the CRLFCRLF terminator
            // must still be sitting unread on the stream -- proving the
            // byte-at-a-time reader stopped exactly at the boundary
            // rather than over-reading into the tunnel body.
            let mut leftover = [0u8; 5];
            stream.read_exact(&mut leftover).await.unwrap();
            (outcome, leftover)
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"CONNECT api.anthropic.com:443 HTTP/1.1\r\n\r\nTUNNL")
            .await
            .unwrap();

        let (outcome, leftover) = server.await.unwrap();
        assert!(matches!(outcome, Some(ConnectRequest::Target(_))));
        assert_eq!(&leftover, b"TUNNL");
    }

    /// Slowloris guard: a client that connects and then withholds the
    /// terminating CRLFCRLF forever must not stall `read_connect_target`
    /// indefinitely -- wrapped in [`CONNECT_READ_TIMEOUT`] (as
    /// [`handle_connection`] does), the read must time out rather than
    /// hang. `start_paused` lets the test observe the 10s timeout without
    /// actually sleeping that long.
    #[tokio::test(start_paused = true)]
    async fn read_connect_target_times_out_when_the_terminator_never_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::time::timeout(CONNECT_READ_TIMEOUT, read_connect_target(&mut stream)).await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"CONNECT api.anthropic").await.unwrap();
        // Deliberately never send the CRLFCRLF terminator or close the
        // socket -- the client just sits there, as a stalled/malicious
        // peer would.

        let result = server.await.unwrap();
        assert!(
            result.is_err(),
            "a CONNECT read that never sees the terminator must time out, not hang forever"
        );
    }

    fn test_ctx(reinject_base: Url, upstream_origin: Url) -> Arc<MitmCtx> {
        Arc::new(MitmCtx {
            forward_state: ForwardState::new(build_client().unwrap(), 8, Duration::from_secs(30)),
            metrics: Arc::new(ProxyMetrics::new()),
            warn_once: Arc::new(WarnOnce::new()),
            upstream_origin,
            reinject_base,
            tested_cc_version: None,
            cc_version_warn_guard: CcVersionWarnGuard::new(),
            seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
        })
    }

    /// Drives [`run_listener`] directly (no cert/TLS involved) against a
    /// blind-tunnel target: a CONNECT to a host that is NOT `mitm_host`
    /// must relay raw bytes end-to-end via `copy_bidirectional`, proving
    /// the listener's non-MITM branch never touches TLS.
    #[tokio::test]
    async fn connect_to_non_mitm_host_blind_tunnels_bytes_without_tls() {
        let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = echo_listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            socket.read_exact(&mut buf).await.unwrap();
            socket.write_all(&buf).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), "api.anthropic.com").unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from("api.anthropic.com"),
            shutdown_rx,
        ));

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", echo_addr.port()).as_bytes(),
            )
            .await
            .unwrap();
        let mut response_buf = [0u8; 39];
        client.read_exact(&mut response_buf).await.unwrap();
        assert_eq!(
            &response_buf,
            b"HTTP/1.1 200 Connection Established\r\n\r\n"
        );

        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
    }

    /// A CONNECT to an unreachable blind-tunnel target must get a `502`
    /// CONNECT-level failure with no relayed bytes and no established
    /// response, never a `200` -- proving the listener dials the target
    /// BEFORE telling the client the tunnel is established (see
    /// [`handle_connection`]'s doc for why answering `200` before a
    /// successful dial would be a false positive).
    #[tokio::test]
    async fn connect_to_an_unreachable_blind_tunnel_target_gets_502_not_200() {
        // Unreachability comes from PRESENCE, not absence: the socket is
        // bound (so the port is never handed back out as another test's
        // ephemeral pick, even one asking for SO_REUSEADDR) but never
        // listens, so every dial is refused. Reserving a port by binding
        // and DROPPING a listener leaves it free for a sibling to claim in
        // the window before the dial, at which point the target is
        // reachable and this test sees the very false 200 it exists to
        // forbid.
        let dead = TcpSocket::new_v4().unwrap();
        dead.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let dead_port = dead.local_addr().unwrap().port();

        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), "api.anthropic.com").unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let metrics = Arc::clone(&ctx.metrics);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from("api.anthropic.com"),
            shutdown_rx,
        ));

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{dead_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let response = read_to_close(
            &mut client,
            "the CONNECT exchange must close rather than establish a tunnel: a regression that \
             answers 200 and establishes it leaves neither side closing",
        )
        .await;
        assert!(
            response.starts_with("HTTP/1.1 502"),
            "an unreachable target must get a 502, not a false 200: {response}"
        );
        // Read to EOF above, so the whole exchange is these bytes: nothing
        // was relayed and no `200 Connection Established` followed, which a
        // `starts_with` on the status line alone would not catch.
        assert_eq!(
            response, "HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n",
            "the 502 must be the bare CONNECT-level failure: {response}"
        );
        // The 502 has to be the unreachable-dial one specifically. A 502
        // booked as anything else, or a second request booked at all (a
        // tunnel that ran anyway), would mean the listener answered
        // established despite the failed dial.
        assert_eq!(
            metrics.request_count(
                Leg::BlindTunnel,
                ResultClass::Unreachable,
                PathClass::Unknown
            ),
            1
        );
        assert_eq!(metrics.requests_total(), 1);

        // The bind must outlive the dial -- it IS the unreachability.
        drop(dead);
    }

    /// A CONNECT to `mitm_host` must hand the raw stream to
    /// [`handle_mitm_connection`]: a TLS client trusting the generated
    /// CA reaches the split layer over the tunnel, proving the
    /// listener's MITM branch (as opposed to the blind-tunnel branch
    /// above) wires correctly.
    #[tokio::test]
    async fn connect_to_mitm_host_hands_off_to_the_mitm_layer() {
        const HOST: &str = "api.anthropic.com";
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), HOST).unwrap();
        let ca_pem = std::fs::read_to_string(ca::ca_cert_path(dir.path())).unwrap();

        let reinject_server = MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&reinject_server)
            .await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from(HOST),
            shutdown_rx,
        ));

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(format!("CONNECT {HOST}:443 HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response_buf = [0u8; 39];
        client.read_exact(&mut response_buf).await.unwrap();
        assert_eq!(
            &response_buf,
            b"HTTP/1.1 200 Connection Established\r\n\r\n"
        );

        let mut root_store = rustls::RootCertStore::empty();
        let ca_der = rustls_pki_types::CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap();
        root_store.add(ca_der).unwrap();
        let mut client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let server_name = rustls::pki_types::ServerName::try_from(HOST)
            .unwrap()
            .to_owned();
        let mut tls = connector.connect(server_name, client).await.unwrap();

        tls.write_all(
            b"GET /v1/models HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let response = read_to_close(
            &mut tls,
            "the MITM handoff must answer and close the `Connection: close` request: a stream \
             that never closes means the request was never dispatched through the MITM branch",
        )
        .await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected the reinject mock's 200 through the MITM handoff, got: {response}"
        );
    }

    /// DNS hostnames are case-insensitive: a client sending a
    /// mixed-case CONNECT authority for the configured `mitm_host` must
    /// still be handed off to the MITM layer, not silently blind-
    /// tunneled because of an exact-case string mismatch.
    #[tokio::test]
    async fn connect_to_mitm_host_matches_case_insensitively() {
        const HOST: &str = "api.anthropic.com";
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), HOST).unwrap();

        let reinject_server = MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&reinject_server)
            .await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from(HOST),
            shutdown_rx,
        ));

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(b"CONNECT API.ANTHROPIC.COM:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response_buf = [0u8; 39];
        // Bounded: a regression that sent this through the blind-tunnel
        // branch instead would attempt a real DNS dial for
        // `API.ANTHROPIC.COM` with no timeout, hanging rather than
        // failing fast.
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut response_buf))
            .await
            .expect("must not hang trying to dial a real host")
            .unwrap();
        assert_eq!(
            &response_buf,
            b"HTTP/1.1 200 Connection Established\r\n\r\n"
        );

        assert!(
            upstream_server
                .received_requests()
                .await
                .unwrap()
                .is_empty(),
            "a mixed-case mitm_host CONNECT must never fall through to the blind tunnel"
        );
    }

    /// Full assembly proof of acceptance criterion (c) -- "a control-plane
    /// path forwards verbatim to the fake Anthropic origin with the
    /// client token intact" -- exercised through this module's own real
    /// CONNECT parse + TLS handoff + split-dispatch code paths.
    /// `upstream_origin` is a plain-`http` wiremock server here (built
    /// directly into `MitmCtx`, bypassing `Config`/`validate_mitm_config`
    /// exactly like `split.rs`'s and `mitm.rs`'s own test suites already
    /// do) because `validate_mitm_config` requires an `https://` origin
    /// and `wiremock` has no TLS mode -- see the module doc on
    /// `tests/proxy_integration.rs` for why that scenario cannot run
    /// through the full `serve_on_listener` boot path in-process.
    #[tokio::test]
    async fn connect_to_a_control_plane_path_forwards_to_upstream_origin_with_token_intact() {
        const HOST: &str = "api.anthropic.com";
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), HOST).unwrap();
        let ca_pem = std::fs::read_to_string(ca::ca_cert_path(dir.path())).unwrap();

        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/hello"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(b"ok".to_vec(), "text/plain"),
            )
            .mount(&upstream_server)
            .await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from(HOST),
            shutdown_rx,
        ));

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(format!("CONNECT {HOST}:443 HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response_buf = [0u8; 39];
        client.read_exact(&mut response_buf).await.unwrap();
        assert_eq!(
            &response_buf,
            b"HTTP/1.1 200 Connection Established\r\n\r\n"
        );

        let mut root_store = rustls::RootCertStore::empty();
        let ca_der = rustls_pki_types::CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap();
        root_store.add(ca_der).unwrap();
        let mut client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let server_name = rustls::pki_types::ServerName::try_from(HOST)
            .unwrap()
            .to_owned();
        let mut tls = connector.connect(server_name, client).await.unwrap();

        tls.write_all(
            b"GET /api/hello HTTP/1.1\r\nHost: api.anthropic.com\r\n\
              Authorization: Bearer client-secret-token\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let response = read_to_close(
            &mut tls,
            "the control-plane forward must answer and close the `Connection: close` request: a \
             stream that never closes means the forward leg never completed against the upstream \
             origin",
        )
        .await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected the fake Anthropic origin's 200, got: {response}"
        );

        let received = upstream_server.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            1,
            "the fake Anthropic origin must see exactly one request"
        );
        assert_eq!(
            received[0].headers.get("authorization").unwrap(),
            "Bearer client-secret-token",
            "the client Authorization must reach the real Anthropic origin untouched"
        );
        assert!(
            reinject_server
                .received_requests()
                .await
                .unwrap()
                .is_empty(),
            "a control-plane path must never reach the loopback reinject target"
        );
    }

    #[tokio::test]
    async fn run_listener_stops_promptly_on_shutdown_signal() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), "api.anthropic.com").unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let handle = tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from("api.anthropic.com"),
            shutdown_rx,
        ));

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener must stop within 2s of the shutdown signal")
            .expect("listener task must not panic");
    }

    #[tokio::test]
    async fn run_listener_flushes_a_metrics_snapshot_at_graceful_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), "api.anthropic.com").unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );
        // Seed a counter so the flushed snapshot carries a non-zero value
        // and the assertion proves the shared instance's totals reach it.
        ctx.metrics
            .incr_request(Leg::Inference, ResultClass::Success, PathClass::Inference);

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        // Flip shutdown before driving the loop so `run_listener` takes
        // its shutdown branch on the first poll and returns ON THIS
        // THREAD -- `with_capture` only sees events emitted on the calling
        // thread, never on a spawned task.
        shutdown_tx.send(()).unwrap();

        let ((), events) = routectl_testkit::with_capture(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from("api.anthropic.com"),
            shutdown_rx,
        ))
        .await;

        let snapshot = events
            .iter()
            .find(|event| {
                event.target == "routectl_cli::proxy::metrics"
                    && event.message == "proxy metrics snapshot"
            })
            .expect("graceful shutdown must flush a proxy metrics snapshot");
        assert_eq!(
            snapshot.field("rc_proxy_requests_total"),
            Some("1"),
            "the flushed snapshot must carry the shared instance's accumulated total"
        );
    }

    /// Guards the OTHER `log_snapshot()` seam in the accept loop: the
    /// periodic [`METRICS_SNAPSHOT_INTERVAL`] tick, which the
    /// shutdown-flush test above cannot observe (it returns on the
    /// shutdown branch before the timer ever elapses). `start_paused`
    /// auto-advances the clock to each pending deadline, so a full
    /// interval passes without a single millisecond of wall time.
    #[tokio::test(start_paused = true)]
    async fn run_listener_flushes_a_metrics_snapshot_on_the_periodic_tick() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), "api.anthropic.com").unwrap();
        // Plain loopback origins rather than wiremock servers: this test
        // sends no traffic at all, and a mock server's own background
        // tasks would keep the runtime busy enough to interfere with the
        // paused clock's auto-advance.
        let ctx = test_ctx(
            Url::parse("http://127.0.0.1:9").unwrap(),
            Url::parse("http://127.0.0.1:9").unwrap(),
        );
        // Seed a counter so the emitted snapshot carries a non-zero value
        // and the assertion proves the shared instance's totals reach it.
        ctx.metrics
            .incr_request(Leg::Inference, ResultClass::Success, PathClass::Inference);

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        // The sender stays alive for the whole test: dropping it makes
        // `shutdown.changed()` resolve (with an error the select arm
        // matches), which would fire the SHUTDOWN flush and mask a
        // missing periodic emission.
        let (_shutdown_tx, shutdown_rx) = watch::channel(());

        // Driven inline (not on a spawned task) because `with_capture`
        // only sees events emitted on the calling thread. `run_listener`
        // never returns on its own here, so the timeout is how the future
        // ends -- it must outlast exactly one snapshot interval.
        let (outcome, events) = routectl_testkit::with_capture(tokio::time::timeout(
            METRICS_SNAPSHOT_INTERVAL + Duration::from_secs(1),
            run_listener(
                proxy_listener,
                acceptor,
                ctx,
                Arc::from("api.anthropic.com"),
                shutdown_rx,
            ),
        ))
        .await;
        assert!(
            outcome.is_err(),
            "the loop must still be running: it may only exit on the shutdown signal"
        );

        let snapshots: Vec<_> = events
            .iter()
            .filter(|event| {
                event.target == "routectl_cli::proxy::metrics"
                    && event.message == "proxy metrics snapshot"
            })
            .collect();
        assert_eq!(
            snapshots.len(),
            1,
            "exactly one periodic snapshot must be emitted per elapsed interval, got {}",
            snapshots.len()
        );
        assert_eq!(
            snapshots[0].field("rc_proxy_requests_total"),
            Some("1"),
            "the periodic snapshot must carry the shared instance's accumulated total"
        );
    }

    /// Assembly-level proof of the slowloris guard: a client that
    /// connects to the real accept loop and then withholds the CONNECT
    /// terminator forever must have its connection closed once
    /// [`CONNECT_READ_TIMEOUT`] elapses, cleanly (no panic in the
    /// spawned `handle_connection` task) rather than held open forever.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_connect_client_is_dropped_cleanly_after_the_read_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = ca::load_or_create(dir.path(), "api.anthropic.com").unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        tokio::spawn(run_listener(
            proxy_listener,
            acceptor,
            ctx,
            Arc::from("api.anthropic.com"),
            shutdown_rx,
        ));

        let mut stalled = TcpStream::connect(proxy_addr).await.unwrap();
        stalled.write_all(b"CONNECT api.anthropic").await.unwrap();
        // Never send the CRLFCRLF terminator.

        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(30), stalled.read(&mut buf))
            .await
            .expect(
                "the stalled connection must be closed once the read timeout elapses, not held \
                 open forever",
            )
            .unwrap();
        assert_eq!(
            n, 0,
            "expected a clean EOF (the listener dropped the connection), not data"
        );
    }

    #[tokio::test]
    async fn build_and_bind_rejects_an_unparseable_upstream_origin() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProxyListenerConfig {
            listen_port: 0,
            cert_dir: dir.path().to_path_buf(),
            mitm_host: "api.anthropic.com".to_string(),
            upstream_origin: "not a url".to_string(),
            reinject_port: 9100,
            tested_cc_version: None,
            seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
        };

        let result = build_and_bind(config).await;
        match result {
            Err(error) => assert!(matches!(error, ProxyStartError::UpstreamOrigin { .. })),
            Ok(_) => panic!("expected an unparseable upstream_origin to be rejected"),
        }
    }

    #[tokio::test]
    async fn build_and_bind_binds_an_ephemeral_loopback_port() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProxyListenerConfig {
            listen_port: 0,
            cert_dir: dir.path().to_path_buf(),
            mitm_host: "api.anthropic.com".to_string(),
            upstream_origin: "https://api.anthropic.com".to_string(),
            reinject_port: 9100,
            tested_cc_version: None,
            seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
        };

        let (listener, _acceptor, _ctx) = build_and_bind(config).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0, "the OS must have assigned a real port");
    }
}
