//! Assembly-level integration tests for the MITM front-proxy: boots the
//! REAL `serve_on_listener` (the same entry point routectl's own `serve`
//! subcommand uses) with a `[mitm]` config enabled, then drives the
//! CONNECT proxy protocol against its loopback port exactly as
//! Claude Code (via `HTTPS_PROXY`) would.
//!
//! Distinct from the in-module tests in `src/proxy/listener.rs` (which
//! drive `run_listener` directly against a hand-built `MitmCtx`) and
//! from `src/proxy/mitm.rs` / `src/proxy/split.rs`'s own unit tests
//! (which drive the TLS-termination and split/classify layers in
//! isolation): this file's job is to prove the ASSEMBLY -- that
//! `serve_on_listener` wires the proxy listener's `reinject_base` to
//! the REAL main listener's dynamically-bound port, that the hard
//! non-loopback refusal actually fires from the booted entry point, and
//! that a `[mitm]`-absent config changes nothing observable.
//!
//! The byte-for-byte header/auth-preservation assertions on the
//! re-inject leg (`x-routectl-mitm-proxied: 1`, untouched
//! `Authorization`) are already pinned by
//! `proxy::split::tests::const_inference_path_reinjects_over_loopback_and_sets_marker_header`
//! in `src/proxy/split.rs`; re-asserting them here through a full real
//! provider-dispatch chain would need a complete alias/provider config
//! whose own outbound auth-substitution semantics are unrelated to what
//! this file is verifying (that the assembly wiring itself is correct).
//! Test (b) below instead uses `/v1/models` -- a real inference-path
//! route the main listener serves entirely in-process, with no
//! provider dispatch -- so the assertion that the response actually
//! came from the live main listener (rather than the fake Anthropic
//! origin) is a clean, direct proof of the wiring.
//!
//! Test (c) ("a control-plane path forwards verbatim to the fake
//! Anthropic origin with the client token intact") is NOT exercised in
//! THIS file: `routectl_router::validate_mitm_config` (wired into
//! `build_router_from_config`, and correctly so -- it is a real
//! production coherence check) rejects any `[mitm].upstream_origin`
//! that is not an absolute `https://` URL, and `wiremock` (the crate's
//! only test-double HTTP server) has no TLS mode, so there is no
//! in-process fake origin this file's booted server could legally point
//! `upstream_origin` at without either dialing the real internet or
//! weakening `build_client`'s TLS verification (out of scope -- that is
//! the MITM proxy's forward-leg transport, not this file's to change). That same
//! scenario IS covered end-to-end -- CONNECT parse, TLS handoff, and the
//! control-plane forward, all through this module's own real code paths
//! -- by
//! `proxy::listener::tests::connect_to_a_control_plane_path_forwards_to_upstream_origin_with_token_intact`
//! in `src/proxy/listener.rs`, which builds `MitmCtx` directly (bypassing
//! `Config`/`validate_mitm_config`, exactly like `split.rs`'s and
//! `mitm.rs`'s own existing test suites already do) so it can use a
//! plain-`http` wiremock server as the upstream double.

use std::path::Path;
use std::sync::Arc;

use routectl_router::{Config, MitmConfig, ServerAuth};
use rustls_pki_types::pem::PemObject;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

mod common;

use common::readiness::{READY_DEADLINE, await_health, sleep_until_cadence_or_deadline};

const MITM_HOST: &str = "api.anthropic.com";
const CONNECT_ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

/// Binds an ephemeral loopback port, reads it back, then drops the
/// listener -- reserving a free port NUMBER for `[mitm] listen_port`,
/// which must be a concrete `u16` in the config before the code under
/// test binds it (and, in the `[mitm]`-absent test, the number a probe
/// then asserts stayed free).
///
/// Keeping the socket bound instead is not an option here, because what
/// this call site needs is a bindable port rather than an occupied one:
/// `serve_on_listener` opens the proxy front itself from
/// `config.mitm.listen_port`, and the only listener it accepts as an
/// argument is the main HTTP one, so a pre-bound proxy socket cannot be
/// handed in.
///
/// The residual exposure is a concurrent bind claiming the number
/// between the drop here and that bind, which degrades the proxy start
/// (logged by `serve_on_listener`, deliberately non-fatal there) --
/// accepted for a single-process run, because the alternative needs the
/// OS-assigned port of a socket only the booted server holds, reported
/// back out through a surface that does not exist yet.
///
/// What is NOT accepted is that exposure surfacing as a confusing
/// failure: [`spawn_test_server`] gates on the proxy actually answering
/// CONNECT on this number before the test dials it, so a lost race fails
/// at the gate naming the race rather than as a bare "connection
/// refused" from a much later dial.
async fn reserve_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn mitm_config(
    cert_dir: &Path,
    listen_port: u16,
    upstream_origin: &str,
    mitm_host: &str,
) -> MitmConfig {
    MitmConfig {
        upstream_origin: upstream_origin.to_string(),
        listen_port,
        cert_dir: cert_dir.to_path_buf(),
        mitm_host: mitm_host.to_string(),
        tested_cc_version: None,
    }
}

/// Boots the real `serve_on_listener` on an ephemeral loopback port in a
/// background task and returns the bound base URL once the server is
/// actually answering -- mirrors `tests/server.rs`'s
/// `helpers::spawn_test_server`, duplicated here (rather than shared)
/// because that helper lives inside `server.rs`'s own `mod helpers`,
/// private to that integration-test binary.
///
/// Readiness is signalled, never slept for: the listener is bound before
/// the serve task starts, so a bare TCP connect succeeds from the OS
/// backlog well before the router (or the MITM front) can answer. Both
/// fronts are polled against a deadline instead -- a fixed wait is a
/// latent flake the moment the suite runs loaded and in parallel.
async fn spawn_test_server(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let proxy_front = config
        .mitm
        .as_ref()
        .map(|mitm| (mitm.listen_port, mitm.mitm_host.clone()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });

    await_health(&base_url).await;
    if let Some((port, mitm_host)) = proxy_front {
        await_proxy_front(port, &mitm_host).await;
    }

    base_url
}

/// Poll the MITM front's CONNECT protocol until it answers `200
/// Connection Established` for `mitm_host` (which the MITM branch
/// answers without dialing anything) or the readiness deadline elapses.
///
/// A completed CONNECT handshake is the readiness signal specifically
/// because it discriminates: the port merely being connectable would
/// also be true of a sibling test that claimed the number
/// `reserve_free_port` handed back (see its doc on that residual
/// exposure). So a lost port race fails HERE, naming the race, instead
/// of surfacing as a misleading "connection refused" (proxy startup
/// failed, logged and swallowed as non-fatal) at a later CONNECT dial.
///
/// Each attempt is bounded by the remaining deadline for the same reason
/// as [`await_health`]: a peer that accepts but never answers must fail
/// this gate, not outlive it.
async fn await_proxy_front(port: u16, mitm_host: &str) {
    let deadline = Instant::now() + READY_DEADLINE;
    while Instant::now() < deadline {
        if tokio::time::timeout_at(deadline, probe_connect(port, mitm_host))
            .await
            .unwrap_or(false)
        {
            return;
        }
        sleep_until_cadence_or_deadline(deadline).await;
    }
    panic!(
        "the MITM proxy front never answered CONNECT on 127.0.0.1:{port}: it either failed to \
         start (a sibling claimed the reserved port number before the proxy bound it) or another \
         process holds that port"
    );
}

/// One CONNECT attempt against the MITM front. Every failure shape (no
/// connection, a short or unexpected response) is a not-ready-yet, since
/// the caller retries until its deadline. The caller also bounds this
/// call, so nothing here needs its own timeout.
async fn probe_connect(port: u16, mitm_host: &str) -> bool {
    let Ok(mut tcp) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    if tcp
        .write_all(format!("CONNECT {mitm_host}:443 HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .is_err()
    {
        return false;
    }
    let mut response = [0u8; CONNECT_ESTABLISHED.len()];
    tcp.read_exact(&mut response).await.is_ok() && response.as_slice() == CONNECT_ESTABLISHED
}

/// Speaks one CONNECT handshake against the booted MITM proxy port and
/// returns the still-open plaintext `TcpStream` positioned right after
/// the `200 Connection Established` response -- ready for the caller to
/// either wrap in TLS (a `mitm_host` target) or write raw bytes into (a
/// blind-tunnel target).
async fn connect_tunnel(
    mitm_port: u16,
    target_host: &str,
    target_port: u16,
) -> tokio::net::TcpStream {
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", mitm_port))
        .await
        .expect("connect to the MITM proxy's CONNECT front");
    tcp.write_all(format!("CONNECT {target_host}:{target_port} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = [0u8; CONNECT_ESTABLISHED.len()];
    tcp.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, CONNECT_ESTABLISHED);
    tcp
}

/// Completes the TLS handshake over an established CONNECT tunnel,
/// trusting only the MITM proxy's own generated CA (mirroring how an
/// operator points Claude Code's `NODE_EXTRA_CA_CERTS` at it).
async fn tls_handshake(
    tcp: tokio::net::TcpStream,
    mitm_host: &str,
    ca_pem: &str,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let mut root_store = rustls::RootCertStore::empty();
    let ca_der = rustls_pki_types::CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap();
    root_store.add(ca_der).unwrap();
    let mut client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let server_name = rustls::pki_types::ServerName::try_from(mitm_host.to_string()).unwrap();
    connector.connect(server_name, tcp).await.unwrap()
}

// ---------------------------------------------------------------------------
// (a) blind tunnel
// ---------------------------------------------------------------------------

/// A CONNECT to a host OTHER than `mitm_host` must relay raw bytes
/// end-to-end through the real booted server, with no TLS involved --
/// this is the traffic shape Claude Code's telemetry/Sentry calls take
/// when `HTTPS_PROXY` points every outbound HTTPS call at this same
/// proxy port. `upstream_origin` is a placeholder here (never dialed:
/// a blind-tunnel target is reached directly via the raw CONNECT
/// authority, not via `[mitm].upstream_origin`) -- `validate_mitm_config`
/// still requires it to be a syntactically valid absolute `https://` URL.
#[tokio::test]
async fn blind_tunnel_relays_bytes_through_the_booted_server() {
    let dir = tempfile::tempdir().unwrap();
    let mitm_port = reserve_free_port().await;

    let config = Config {
        mitm: Some(mitm_config(
            dir.path(),
            mitm_port,
            "https://api.anthropic.com",
            MITM_HOST,
        )),
        ..Config::default()
    };
    let _base_url = spawn_test_server(Arc::new(config)).await;

    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = echo_listener.accept().await.unwrap();
        let mut buf = [0u8; 5];
        socket.read_exact(&mut buf).await.unwrap();
        socket.write_all(&buf).await.unwrap();
    });

    let mut tunnel = connect_tunnel(mitm_port, "127.0.0.1", echo_port).await;
    tunnel.write_all(b"hello").await.unwrap();
    let mut echoed = [0u8; 5];
    tunnel.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"hello");
}

// ---------------------------------------------------------------------------
// (b) re-inject assembly: CONNECT -> TLS -> the REAL live main listener
// ---------------------------------------------------------------------------

/// A CONNECT to `mitm_host` followed by a request to a const inference
/// path (`/v1/models`) must reach the REAL booted main listener -- proof
/// that `serve_on_listener` wired `reinject_base` to the main listener's
/// actual (dynamically OS-assigned) bound port, not a stale or
/// hardcoded one. `/v1/models` is served entirely in-process (no
/// provider dispatch), so a coherent 200 with the expected JSON shape
/// can only have come from the live main listener. `upstream_origin` is
/// again a never-dialed placeholder: a const inference path is always
/// re-injected, never forwarded to `upstream_origin`.
#[tokio::test]
async fn connect_to_mitm_host_reinjects_v1_models_to_the_live_main_listener() {
    let dir = tempfile::tempdir().unwrap();
    let mitm_port = reserve_free_port().await;

    let config = Config {
        mitm: Some(mitm_config(
            dir.path(),
            mitm_port,
            "https://api.anthropic.com",
            MITM_HOST,
        )),
        ..Config::default()
    };
    let _base_url = spawn_test_server(Arc::new(config)).await;

    let ca_pem =
        std::fs::read_to_string(routectl_cli::proxy::ca::ca_cert_path(dir.path())).unwrap();
    let tunnel = connect_tunnel(mitm_port, MITM_HOST, 443).await;
    let mut tls = tls_handshake(tunnel, MITM_HOST, &ca_pem).await;

    tls.write_all(
        b"GET /v1/models HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut response = Vec::new();
    // Bounded like the `src/proxy/listener.rs` test module's reads: if the
    // re-inject leg never reaches the live main listener, nothing closes
    // this stream and an unbounded read to EOF would stall a parallel
    // shard instead of failing it.
    tokio::time::timeout(READY_DEADLINE, tls.read_to_end(&mut response))
        .await
        .expect(
            "the re-inject leg must answer and close the `Connection: close` request: a stream \
             that never closes means the request never reached the live main listener",
        )
        .unwrap();
    let response = String::from_utf8_lossy(&response);

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected a 200 from the live main listener's /v1/models, got: {response}"
    );
    assert!(
        response.contains("\"object\":\"model\"") || response.contains("[]"),
        "expected the real /v1/models JSON shape, got: {response}"
    );
}

// ---------------------------------------------------------------------------
// hard-refuse: non-loopback bind + [mitm] enabled, independent of --unsafe-public
// ---------------------------------------------------------------------------

/// `serve_on_listener` takes an already-bound listener and never itself
/// consults `--unsafe-public` (that flag is consumed earlier, only by
/// `serve()`, before the bind happens) -- binding `0.0.0.0` directly
/// here reproduces exactly the address `serve()` would hand off after
/// `--unsafe-public` cleared `check_bind_safety`. The MITM hard-refuse
/// must still fire from this entry point regardless.
#[tokio::test]
async fn mitm_enabled_on_a_non_loopback_bind_is_hard_refused() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let mitm_port = reserve_free_port().await;

    let config = Config {
        mitm: Some(mitm_config(
            dir.path(),
            mitm_port,
            "https://api.anthropic.com",
            MITM_HOST,
        )),
        ..Config::default()
    };
    let config = common::isolate_usage_db(Arc::new(config));

    let error = routectl_cli::server::serve_on_listener(config, listener, None)
        .await
        .expect_err("a non-loopback bind with [mitm] enabled must be hard-refused");
    let message = error.to_string();
    assert!(
        message.to_ascii_lowercase().contains("mitm"),
        "refusal message must name the MITM proxy: {message}"
    );
    assert!(
        message.contains("unsafe-public"),
        "refusal message must say --unsafe-public cannot override it: {message}"
    );
}

/// The mirror case: with `[mitm]` absent, the SAME non-loopback bind
/// must not trip the MITM-specific refusal. A listener token is
/// configured so the pre-existing (unrelated) "public bind without
/// auth" cross-check also passes, isolating this assertion to the MITM
/// hard-refuse specifically.
///
/// `serve_on_listener` never returns on success (it serves until
/// shutdown), so liveness is the assertion -- but it is gated on the
/// server ANSWERING `/health` over the non-loopback bind, not on a fixed
/// wait. Both refusals happen before the serve loop starts, so a served
/// `/health` can only mean the bind decision was cleared; a fixed sleep
/// proves only that time passed and goes false-green under load. The
/// refusal is raced against readiness so a returning regression fails
/// immediately, naming its own error, instead of at the poll deadline.
#[tokio::test]
async fn mitm_absent_does_not_trip_the_hard_refuse_on_a_non_loopback_bind() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let config = Config {
        server: routectl_router::ServerConfig {
            auth: Some(ServerAuth {
                tokens: vec![common::file_ref("test-listener-token")],
            }),
            ..routectl_router::ServerConfig::default()
        },
        ..Config::default()
    };
    let config = common::isolate_usage_db(Arc::new(config));

    let mut handle = tokio::spawn(routectl_cli::server::serve_on_listener(
        config, listener, None,
    ));

    let base_url = format!("http://127.0.0.1:{port}");
    tokio::select! {
        served = &mut handle => panic!(
            "a non-loopback bind with [mitm] absent and listener auth configured must not error \
             out, but serve_on_listener returned: {served:?}"
        ),
        () = await_health(&base_url) => {}
    }

    handle.abort();
}

// ---------------------------------------------------------------------------
// default-off: [mitm] absent means zero proxy behavior
// ---------------------------------------------------------------------------

/// With `[mitm]` absent, the port a hypothetical MITM proxy would have
/// bound must stay completely free -- proof that `serve_on_listener`
/// spawns no proxy task, binds no extra socket, and changes no
/// behavior when the feature is off.
#[tokio::test]
async fn mitm_absent_binds_no_extra_port() {
    let reserved_port = reserve_free_port().await;

    let config = Config::default();
    let _base_url = spawn_test_server(Arc::new(config)).await;

    // If the (nonexistent) proxy task had bound `reserved_port`, this
    // bind would fail with `AddrInUse`.
    let probe = TcpListener::bind(("127.0.0.1", reserved_port)).await;
    assert!(
        probe.is_ok(),
        "the MITM proxy must not have bound any port when [mitm] is absent"
    );
}
