//! MITM front-proxy module for first-party credential passthrough.
//!
//! Deliberately isolated from the rest of `routectl-cli`: nothing here
//! imports `crate::handlers`, `crate::server::AppState`, or
//! `crate::ingress`. That isolation is one-directional -- removing the
//! whole MITM feature means deleting this directory, dropping the
//! `pub mod proxy;` line in `lib.rs`, and updating the three outside
//! call sites that import from it: `commands/rc.rs` (`proxy::ca` + `proxy::pin`),
//! `server/mod.rs` (`proxy::listener`), and `handlers/models.rs`
//! (`proxy::forward` + `proxy::metrics`, for the `/v1/models` forwarded
//! lane).
//!
//! `ca` owns the local CA + leaf certificate lifecycle and hands back a
//! ready-to-use `tokio_rustls::TlsAcceptor`. `metrics` holds the
//! lock-free counters and warn-once helper the split/forward/listener
//! tasks emit into. `forward` is the dumb byte forwarder both split
//! legs reuse (loopback re-inject and catch-all upstream forward) --
//! it streams bytes and records what it is told, but does not itself
//! classify traffic. `split` decides, per decrypted request, which of
//! those two legs a request belongs to (`ANTHROPIC_INFERENCE_PATHS` is
//! the source of truth for that decision). `mitm`
//! terminates TLS on one accepted connection and serves HTTP/1.1 over
//! it, handing each request to `split`. `pin` holds the one host and origin
//! the proxy terminates and forwards to, and builds the listener inputs
//! from them. `listener` is the assembly
//! point: it binds the loopback CONNECT front, builds the
//! `TlsAcceptor` and shared `MitmCtx` once at proxy startup, and
//! dispatches each accepted connection to either
//! `mitm::handle_mitm_connection` (the pinned host) or a
//! blind `copy_bidirectional` tunnel (every other CONNECT target).
//! `server::serve_on_listener` spawns `listener::spawn`'s task only when
//! `Config::mitm.is_some()`.

pub mod ca;
pub mod forward;
pub mod listener;
pub mod metrics;
pub mod mitm;
pub mod pin;
pub mod split;
