//! Daemon-level facts the config panel's source strip needs, behind a
//! read-only facade.
//!
//! Two facts are fixed for the process (the bound listener address and the
//! binary version) and one moves (the instant the live config was last
//! loaded, re-stamped by the reload coordinator on every successful router
//! swap). The moving fact is why this is shared state rather than a value
//! copied into [`super::StatusState`] at construction: a panel built after a
//! hot-reload must report the NEW load instant, not the boot one. Beside that
//! stamp sits the last REJECTED reload (its redacted class and instant), which
//! the doctor panel reads while no later load has superseded it.
//!
//! Same enforcement shape as [`super::router_view`]: the `Arc<DaemonMeta>`
//! is PRIVATE to [`DaemonMetaHandle`], so a panel module holding a handle can
//! call only [`DaemonMetaHandle::snapshot`] -- it can never name the inner
//! field and so can never reach the stamp writer.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

use crate::server::reload_failure::{ReloadFailure, ReloadFailureSnapshot};

/// Epoch-ms sentinel for "no config load has been stamped yet". Never
/// reaches the wire: [`DaemonMetaHandle::snapshot`] maps it to `None` rather
/// than reporting an epoch-1970 load.
const UNSTAMPED: i64 = 0;

/// Process-level daemon facts. The writer half: constructed once at server
/// bootstrap and stamped by the reload coordinator.
pub struct DaemonMeta {
    listen_addr: String,
    config_loaded_at_ms: AtomicI64,
    /// The last rejected reload's class and epoch-ms instant.
    last_reload_failure: Mutex<Option<(ReloadFailure, i64)>>,
}

impl DaemonMeta {
    /// Build the meta for a bound listener, with the config-load instant
    /// unstamped. `stamp_config_loaded` records the first load.
    pub const fn new(listen_addr: String) -> Self {
        Self {
            listen_addr,
            config_loaded_at_ms: AtomicI64::new(UNSTAMPED),
            last_reload_failure: Mutex::new(None),
        }
    }

    /// Record that the live config was loaded (or reloaded) now. Called at
    /// bootstrap and after every successful reload-driven router swap, so the
    /// reported age always tracks the config actually in effect.
    ///
    /// A load supersedes any earlier rejected reload, so it clears that record.
    pub fn stamp_config_loaded(&self) {
        let mut failure = self.last_reload_failure.lock();
        self.config_loaded_at_ms
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
        *failure = None;
    }

    /// Record that a config / overlay reload was rejected now, replacing any
    /// earlier record. Called by the reload coordinator; the previous config
    /// stays in effect, so the load stamp is left alone.
    pub fn record_reload_failure(&self, class: ReloadFailure) {
        let at_ms = chrono::Utc::now().timestamp_millis();
        *self.last_reload_failure.lock() = Some((class, at_ms));
    }
}

/// Read handle over one [`DaemonMeta`]. The inner `Arc` is private to this
/// module, so the only thing a panel can do with it is take a snapshot.
pub struct DaemonMetaHandle {
    inner: Arc<DaemonMeta>,
}

impl DaemonMetaHandle {
    pub const fn new(inner: Arc<DaemonMeta>) -> Self {
        Self { inner }
    }

    /// Snapshot the daemon facts against the caller's pinned clock reading,
    /// so a panel's source strip and its `as_of` share one instant. A load
    /// instant in the future (a clock step between stamp and read) clamps to
    /// zero rather than reporting a negative age.
    pub fn snapshot(&self, now_ms: i64) -> DaemonMetaSnapshot {
        let stamped = self.inner.config_loaded_at_ms.load(Ordering::Relaxed);
        DaemonMetaSnapshot {
            listen_addr: self.inner.listen_addr.clone(),
            version: env!("CARGO_PKG_VERSION"),
            config_loaded_age_ms: (stamped != UNSTAMPED).then(|| (now_ms - stamped).max(0)),
        }
    }

    /// The last rejected reload, when no config load has been stamped since
    /// it: a later successful reload means the daemon serves a config newer
    /// than the one it refused. The record is cleared by that stamp rather
    /// than compared against it, so a failure and a load landing in the same
    /// millisecond still resolve by their order.
    pub fn reload_failure(&self, now_ms: i64) -> Option<ReloadFailureSnapshot> {
        let (class, at_ms) = (*self.inner.last_reload_failure.lock())?;
        Some(ReloadFailureSnapshot {
            class,
            age_ms: (now_ms - at_ms).max(0),
        })
    }
}

/// One consistent read of the daemon facts, owned by the caller.
pub struct DaemonMetaSnapshot {
    /// The address the daemon's listener is bound to.
    pub listen_addr: String,
    /// The running binary's version.
    pub version: &'static str,
    /// How long ago the live config was loaded, or `None` before any load has
    /// been stamped -- never a `0`/epoch sentinel.
    pub config_loaded_age_ms: Option<i64>,
}

// Test-only items live below ALL production code: the forbidden-token scan in
// `super` truncates each scanned file at its first `#[cfg(test)]`, so a
// mid-file test gate would silently exclude the read facade from the scan.
#[cfg(test)]
impl DaemonMeta {
    /// Test-only stand-in: a loopback-shaped bind with the load instant
    /// already stamped, so a panel test sees the realistic shape.
    pub fn for_test() -> Arc<Self> {
        let meta = Arc::new(Self::new("127.0.0.1:0".to_string()));
        meta.stamp_config_loaded();
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_none_age_before_any_stamp() {
        let handle = DaemonMetaHandle::new(Arc::new(DaemonMeta::new("127.0.0.1:9000".to_string())));

        let snapshot = handle.snapshot(chrono::Utc::now().timestamp_millis());

        assert_eq!(snapshot.listen_addr, "127.0.0.1:9000");
        assert_eq!(snapshot.version, env!("CARGO_PKG_VERSION"));
        assert!(snapshot.config_loaded_age_ms.is_none());
    }

    #[test]
    fn snapshot_reports_age_since_the_latest_stamp() {
        let meta = Arc::new(DaemonMeta::new("127.0.0.1:9000".to_string()));
        let handle = DaemonMetaHandle::new(meta.clone());
        meta.stamp_config_loaded();
        let stamped_at = meta.config_loaded_at_ms.load(Ordering::Relaxed);

        let age = handle
            .snapshot(stamped_at + 5_000)
            .config_loaded_age_ms
            .expect("a stamped load reports an age");

        assert_eq!(age, 5_000);
    }

    /// A clock step backwards between the stamp and the read must not yield a
    /// negative age -- the honest floor is zero.
    #[test]
    fn snapshot_clamps_a_future_stamp_to_zero_age() {
        let meta = Arc::new(DaemonMeta::new("127.0.0.1:9000".to_string()));
        let handle = DaemonMetaHandle::new(meta.clone());
        meta.stamp_config_loaded();
        let stamped_at = meta.config_loaded_at_ms.load(Ordering::Relaxed);

        let age = handle.snapshot(stamped_at - 10_000).config_loaded_age_ms;

        assert_eq!(age, Some(0));
    }

    /// A reload re-stamps the SAME shared meta, so a handle taken before the
    /// reload reports the new load -- the reason this is shared state rather
    /// than a value copied at construction.
    #[test]
    fn a_later_stamp_is_visible_through_an_existing_handle() {
        let meta = Arc::new(DaemonMeta::new("127.0.0.1:9000".to_string()));
        let handle = DaemonMetaHandle::new(meta.clone());
        meta.stamp_config_loaded();
        let first = meta.config_loaded_at_ms.load(Ordering::Relaxed);

        meta.config_loaded_at_ms
            .store(first + 60_000, Ordering::Relaxed);

        assert_eq!(
            handle.snapshot(first + 60_000).config_loaded_age_ms,
            Some(0),
            "the handle must observe the re-stamped load instant"
        );
    }

    #[test]
    fn a_successful_reload_after_a_failure_hides_the_failure() {
        let meta = Arc::new(DaemonMeta::new("127.0.0.1:9000".to_string()));
        let handle = DaemonMetaHandle::new(meta.clone());
        meta.stamp_config_loaded();
        meta.record_reload_failure(ReloadFailure::OverlayLoadFailed);

        meta.stamp_config_loaded();

        assert_eq!(
            handle.reload_failure(chrono::Utc::now().timestamp_millis()),
            None
        );
    }

    #[test]
    fn a_failure_after_a_successful_reload_is_reported_with_its_age() {
        let meta = Arc::new(DaemonMeta::new("127.0.0.1:9000".to_string()));
        let handle = DaemonMetaHandle::new(meta.clone());
        meta.stamp_config_loaded();
        meta.record_reload_failure(ReloadFailure::BoundaryNotDurable);
        let recorded_at = meta
            .last_reload_failure
            .lock()
            .expect("the failure was recorded")
            .1;

        let snapshot = handle.reload_failure(recorded_at + 7_000);

        assert_eq!(
            snapshot,
            Some(ReloadFailureSnapshot {
                class: ReloadFailure::BoundaryNotDurable,
                age_ms: 7_000,
            })
        );
    }

    #[test]
    fn no_failure_is_reported_before_any_reload_is_rejected() {
        let handle = DaemonMetaHandle::new(DaemonMeta::for_test());

        assert_eq!(
            handle.reload_failure(chrono::Utc::now().timestamp_millis()),
            None
        );
    }
}
