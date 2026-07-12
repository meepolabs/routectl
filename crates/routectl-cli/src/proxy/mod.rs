//! MITM front-proxy module for first-party credential passthrough.
//!
//! Deliberately isolated from the rest of `routectl-cli`: nothing here
//! imports `crate::handlers`, `crate::server::AppState`, or
//! `crate::ingress`. That isolation is the point -- removing the whole
//! MITM feature should stay a two-file change (delete this directory,
//! drop the `pub mod proxy;` line in `lib.rs`).
//!
//! `ca` owns the local CA + leaf certificate lifecycle and hands back a
//! ready-to-use `tokio_rustls::TlsAcceptor`. `metrics` holds the
//! lock-free counters and warn-once helper the split/forward/listener
//! tasks emit into. `cc_version` holds the CC-version warn-and-proceed
//! check (tested-vs-observed Claude Code version, warn-once dedup, never
//! a hard refuse). `forward` is the dumb byte forwarder both split
//! legs reuse (loopback re-inject and catch-all upstream forward) --
//! it streams bytes and records what it is told, but does not itself
//! classify traffic. `split` decides, per decrypted request, which of
//! those two legs a request belongs to (`ANTHROPIC_INFERENCE_PATHS` is
//! the source of truth for that decision), and also runs the
//! `cc_version` check against the request's `User-Agent`. `mitm`
//! terminates TLS on one accepted connection and serves HTTP/1.1 over
//! it, handing each request to `split`. `listener` is the assembly
//! point: it binds the loopback CONNECT front, builds the
//! `TlsAcceptor` and shared `MitmCtx` once at proxy startup, and
//! dispatches each accepted connection to either
//! `mitm::handle_mitm_connection` (the configured `mitm_host`) or a
//! blind `copy_bidirectional` tunnel (every other CONNECT target).
//! `server::serve_on_listener` spawns `listener::spawn`'s task only when
//! `Config::mitm.is_some()`.

pub mod ca;
pub(crate) mod cc_version;
pub mod forward;
pub mod listener;
pub mod metrics;
pub mod mitm;
pub mod split;
