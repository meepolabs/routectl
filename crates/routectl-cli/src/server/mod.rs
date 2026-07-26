//! Server module hub: the shared `AppState` every axum handler reads,
//! the loopback bind-safety guard, and the per-concern submodules (the
//! `serve` loop / router build / bounded drain, config load, reload
//! coordinator, auth, file watch, status gate), re-exported here at the
//! `server::` paths callers use. Each submodule owns its own
//! `#[path]`-included unit-test sidecar; hub-local tests (bind safety)
//! live in the sibling `tests.rs`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_core::{Error, Result};
use routectl_router::{ActivationState, Router};
use routectl_usage::UsageHandle;

pub mod auth;
pub mod capability_rebuild;
mod config_load;
pub mod file_watch;
pub mod k_rebuild;
pub mod ledger_reader;
mod reload;
pub mod request_id;
mod router_build;
pub mod secrets;
mod serve;
pub mod status_gate;

pub use config_load::{LoadedConfig, load_effective_config, load_effective_config_unvalidated};
pub(crate) use config_load::{load_overlay_default, parse_config_only};
pub(crate) use router_build::build_router_from_config_with_overlay;
pub use secrets::CompositeStore;
pub use serve::{serve, serve_on_listener, serve_on_listener_with_overlay};

#[cfg(test)]
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};
#[cfg(test)]
pub(crate) use router_build::build_router_from_config;

/// Shared state every axum handler reads from. The `Router` lives
/// behind an `ArcSwap` so the file-watch / SIGHUP coordinator can
/// hot-swap a freshly-built routing surface without per-request
/// locking; handlers `load_full()` once at entry and use that
/// snapshot for the lifetime of the request.
///
/// The two pre-v0.7 fields `strict_translation` and `max_body_bytes`
/// were folded back into the `Router.config` because both are
/// duplicates of `config.server.*` knobs and only `max_body_bytes`
/// has any read site outside the handler path (in `build_axum_router`,
/// where it is now read directly from the live config at the same
/// wiring step).
///
/// `usage` is the `Clone` producer handle for the usage-accounting
/// writer. It lives DIRECTLY on `AppState`, NOT inside the
/// `Arc<ArcSwap<Router>>`, so a Router hot-swap never rebuilds or
/// disturbs the writer (which owns a DB handle opened once at boot).
/// The owning `UsageWriter` (the shutdown handle) is kept in the
/// `serve_on_listener` scope, not here.
pub struct AppState {
    pub router: Arc<ArcSwap<Router>>,
    pub usage: UsageHandle,
    /// Auto-activation inventory: which routectl-owned OAuth providers
    /// currently carry a usable local credential. A SIBLING of `router`,
    /// NOT a field inside the `Arc<ArcSwap<Router>>` -- keeping it
    /// physically outside the Router makes "inventory never reaches
    /// dispatch" true by construction (the dispatch path reads only the
    /// Router). The reload coordinator swaps this alongside the router on a
    /// config or credentials change; `apply_activation` is the sole writer.
    pub activation: Arc<ArcSwap<ActivationState>>,
    /// The per-process value that makes the `x-routectl-mitm-proxied` seam
    /// header unspoofable in practice (see
    /// `crate::ingress::MitmSeamNonce`). Generated ONCE at server bootstrap
    /// and shared with the MITM proxy listener's `MitmCtx` (the sole
    /// legitimate stamper) via the SAME `Arc` -- never regenerated
    /// per-request, never logged, never in config.
    pub mitm_seam_nonce: Arc<crate::ingress::MitmSeamNonce>,
}

impl AppState {
    /// Test-only constructor: wraps `router` with a usage handle backed
    /// by a writer pointed at an isolated in-tempdir DB, so handler unit
    /// tests that build `AppState` directly never touch the real
    /// `~/.config/routectl/usage.db`. The owning `UsageWriter` is
    /// detached (dropped) -- the handle stays usable (it accepts-and-drops
    /// once the channel closes), which is all a non-usage handler test
    /// needs. Returns the `TempDir` guard; keep it alive for the test.
    #[cfg(test)]
    pub fn for_test(router: Arc<ArcSwap<Router>>) -> (Arc<Self>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("usage tempdir");
        let (usage, _writer) =
            UsageWriter::start(dir.path().join("usage.db"), CHANNEL_CAPACITY, 0, false);
        let state = Arc::new(Self {
            router,
            usage,
            activation: Arc::new(ArcSwap::from_pointee(ActivationState::default())),
            mitm_seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
        });
        (state, dir)
    }
}

/// Validate that `host` is loopback or that `unsafe_public` has been set.
/// Returns an error (without binding) if the check fails.
pub fn check_bind_safety(host: &str, unsafe_public: bool) -> Result<()> {
    if unsafe_public {
        if !is_loopback(host) {
            tracing::warn!(
                "WARNING: routectl bound to {host}, exposing your local LLM credentials \
                 to anyone reachable on this address"
            );
        }
        return Ok(());
    }
    if !is_loopback(host) {
        return Err(Error::Config(format!(
            "refusing to bind to non-loopback address `{host}`; \
             pass --unsafe-public to override"
        )));
    }
    Ok(())
}

pub(crate) fn is_loopback(host: &str) -> bool {
    use std::net::IpAddr;
    use std::str::FromStr;
    match IpAddr::from_str(host) {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
        Err(_) => host == "localhost",
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "capability_lifecycle_tests.rs"]
mod capability_lifecycle_tests;
