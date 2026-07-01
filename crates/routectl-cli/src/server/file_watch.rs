//! Filesystem-watch wiring for routectl's hot-reload path.
//!
//! Watches the parent directories of `config.toml` and
//! `credentials.json` (passed in as `WatchTarget`s), debounces a burst
//! of fs events into a single delivery via `notify-debouncer-full`,
//! routes events back to the reload coordinator by exact-basename
//! match against each target, and emits one `ReloadRequest` per
//! target whose final-name event survives the debounce.
//!
//! Why parent directories: both writers go through the
//! tempfile + fsync + rename-into-place pattern (see
//! `crates/routectl-auth/src/oauth/file_io.rs`). Watching the target
//! file inode directly misses the swap because `rename` replaces the
//! inode under the watched path on Linux. Watching the parent dir +
//! filtering on `path.file_name() == target.file_name()` reliably
//! sees the final post-rename event.
//!
//! Self-write filter: NONE. Reload is idempotent (load -> parse ->
//! write-lock-overwrite + Arc swap). At worst a self-triggered fs
//! event causes one redundant parse + Arc::store, which converges
//! atomically (a follow-up event arriving mid-handle is processed
//! after the first finishes). Counter / hash filters add race
//! surface for negligible savings and are intentionally omitted.

use std::path::PathBuf;
use std::time::Duration;

use notify::EventKind;
use notify_debouncer_full::DebouncedEvent;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// What the reload coordinator should re-read on the next event.
/// Variants mirror `WatchTarget`; the coordinator dispatches by
/// variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadRequest {
    /// Re-read + re-parse `config.toml`, validate, rebuild the
    /// `Router`, and atomically `ArcSwap` it into `AppState`.
    Config,
    /// Re-read `credentials.json` into the in-memory `OAuthStore`
    /// cache. No Router rebuild required.
    Credentials,
}

/// A single file the watcher should track. The parent directory of
/// `path` is registered with the underlying notify backend; events
/// are routed back by matching `event_path.file_name()` against
/// `path.file_name()`.
#[derive(Debug, Clone)]
pub enum WatchTarget {
    /// `config.toml` -- emits `ReloadRequest::Config` on change.
    Config(PathBuf),
    /// `credentials.json` -- emits `ReloadRequest::Credentials` on
    /// change.
    Credentials(PathBuf),
}

impl WatchTarget {
    const fn path(&self) -> &PathBuf {
        match self {
            Self::Config(p) | Self::Credentials(p) => p,
        }
    }

    const fn to_request(&self) -> ReloadRequest {
        match self {
            Self::Config(_) => ReloadRequest::Config,
            Self::Credentials(_) => ReloadRequest::Credentials,
        }
    }
}

/// Debounce window for `notify-debouncer-full`. 250 ms is wide enough
/// to coalesce a tempfile + rename burst (the production write
/// pattern) into one delivery and narrow enough that an interactive
/// `routectl login` is reflected in the daemon within human time.
const DEBOUNCE_MS: u64 = 250;

/// Spawn the file-watch task. The returned `JoinHandle` resolves when
/// the watcher exits (shutdown signal or unrecoverable backend
/// failure). On any unrecoverable failure during setup, returns an
/// error and emits no `ReloadRequest`s.
///
/// `targets` may contain a mix of `WatchTarget::Config(_)` and
/// `WatchTarget::Credentials(_)`. Empty `targets` is supported and
/// produces a no-op watcher (the spawned task immediately observes
/// shutdown). This is the test path.
pub fn spawn_watcher(
    targets: Vec<WatchTarget>,
    tx: mpsc::Sender<ReloadRequest>,
    mut shutdown: watch::Receiver<()>,
) -> std::io::Result<JoinHandle<()>> {
    if targets.is_empty() {
        // No-op watcher: nothing to watch, but we still spawn so the
        // call site has a uniform JoinHandle to await.
        return Ok(tokio::spawn(async move {
            let _ = shutdown.changed().await;
        }));
    }

    // Channel: the debouncer's blocking handler pushes batches in;
    // the async task below drains them and routes by basename. We
    // use a tokio unbounded channel because the closure is invoked
    // from the debouncer's worker thread (not a tokio runtime
    // worker), and `UnboundedSender::send` is non-blocking and
    // safely callable from any thread.
    let (raw_tx, mut raw_rx) =
        mpsc::unbounded_channel::<Result<Vec<DebouncedEvent>, Vec<notify::Error>>>();

    let mut debouncer = notify_debouncer_full::new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |res| {
            // Best-effort: if the receiver was dropped (shutdown
            // race), silently ignore. The async drain task is the
            // sole consumer.
            let _ = raw_tx.send(res);
        },
    )
    .map_err(|e| std::io::Error::other(format!("debouncer init: {e}")))?;

    // Watch each target's parent dir. Deduplicate parents so a
    // shared parent (the production case: both files under
    // `~/.config/routectl/`) registers exactly one notify watch.
    let mut watched_parents: std::collections::BTreeSet<PathBuf> =
        std::collections::BTreeSet::new();
    for target in &targets {
        let raw_path = target.path();
        // Canonicalize to resolve symlinks so the debouncer watches the
        // real inode's parent directory. If the file does not exist yet
        // (e.g. credentials.json before the first `routectl login`),
        // canonicalize fails; fall back to the textual path and emit a
        // one-line WARN so operators know the watcher may miss symlinked
        // targets until the file appears.
        let resolved = if let Ok(p) = std::fs::canonicalize(raw_path) {
            p
        } else {
            tracing::warn!(
                target = %raw_path.display(),
                "watch target canonicalize failed (file may not exist yet); \
                 using textual path to derive parent directory",
            );
            raw_path.clone()
        };
        let parent = if let Some(p) = resolved.parent() {
            p.to_path_buf()
        } else {
            tracing::warn!(
                target = %raw_path.display(),
                "watch target has no parent directory; skipping",
            );
            continue;
        };
        if !watched_parents.insert(parent.clone()) {
            continue;
        }
        if let Err(e) = debouncer.watch(&parent, notify::RecursiveMode::NonRecursive) {
            tracing::warn!(
                parent = %parent.display(),
                error = %e,
                "failed to install fs watch on parent directory; \
                 hot-reload via fs events will not fire for this target \
                 (SIGHUP remains as an escape hatch)",
            );
        }
    }

    let handle = tokio::spawn(async move {
        // Move the debouncer into the spawned task so its lifetime
        // ends when the task exits (shutdown). Dropping the
        // debouncer joins the inotify thread cleanly.
        let _debouncer = debouncer;
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    return;
                }
                received = raw_rx.recv() => {
                    let Some(batch) = received else {
                        // All senders dropped (debouncer
                        // teardown). Exit cleanly.
                        return;
                    };
                    process_batch(batch, &targets, &tx).await;
                    // Drain any other batches that arrived before
                    // we awoke so the per-batch dedupe can
                    // coalesce a tempfile + rename pair into a
                    // single delivery.
                    while let Ok(extra) = raw_rx.try_recv() {
                        process_batch(extra, &targets, &tx).await;
                    }
                }
            }
        }
    });

    Ok(handle)
}

/// Forward one batch result (Ok = events to process; Err = backend
/// errors) to the appropriate handler.
async fn process_batch(
    batch: Result<Vec<DebouncedEvent>, Vec<notify::Error>>,
    targets: &[WatchTarget],
    tx: &mpsc::Sender<ReloadRequest>,
) {
    match batch {
        Ok(events) => handle_event_batch(events, targets, tx).await,
        Err(errors) => {
            for e in errors {
                tracing::error!(
                    error = %e,
                    "fs watcher backend error; reload via fs events may be degraded \
                     (SIGHUP remains as an escape hatch)",
                );
            }
        }
    }
}

/// Filter a debounced batch to events that should trigger a reload,
/// route by exact-basename match against the watch targets, and emit
/// one `ReloadRequest` per matched target.
async fn handle_event_batch(
    events: Vec<DebouncedEvent>,
    targets: &[WatchTarget],
    tx: &mpsc::Sender<ReloadRequest>,
) {
    // Per-batch deduplication so a single tempfile + rename burst
    // produces exactly one `ReloadRequest` per target even if the
    // backend coalesced multiple kinds (Create + ModifyName::To)
    // into one batch.
    let mut emit_config = false;
    let mut emit_credentials = false;

    for ev in &events {
        if !is_reload_kind(&ev.kind) {
            continue;
        }
        for path in &ev.paths {
            for target in targets {
                if !basenames_match(path, target.path()) {
                    continue;
                }
                if matches!(ev.kind, EventKind::Remove(_)) {
                    if is_atomic_rewrite_remove(path, &events) {
                        // Atomic-rewrite editors (vim, Edit/Write
                        // tools, safe-save) issue Remove + Create for
                        // the same basename in rapid succession. The
                        // sibling Create/Modify is the load-bearing
                        // signal -- it triggers the reload -- so a
                        // WARN here would mislead an operator into
                        // thinking config or credentials were lost.
                        // Keep a DEBUG breadcrumb for triage.
                        tracing::debug!(
                            path = %path.display(),
                            "watched file removed but a sibling create/modify in the same \
                             debounced batch will trigger reload (atomic rewrite)",
                        );
                    } else {
                        tracing::warn!(
                            path = %path.display(),
                            "watched file was removed; keeping last known good in-memory state \
                             (will reload on next Create event)",
                        );
                    }
                    continue;
                }
                match target.to_request() {
                    ReloadRequest::Config => emit_config = true,
                    ReloadRequest::Credentials => emit_credentials = true,
                }
            }
        }
    }

    if emit_config {
        send_reload(tx, ReloadRequest::Config).await;
    }
    if emit_credentials {
        send_reload(tx, ReloadRequest::Credentials).await;
    }
}

/// True if a Remove event for `remove_path` is part of an atomic
/// rewrite -- i.e. the same debounced batch contains a sibling
/// Create or Modify event with the same basename. In that case the
/// Remove is a transient artifact of a rename-into-place dance, and
/// the Create/Modify is what actually triggers the reload; surfacing
/// the Remove as a WARN would falsely imply config / credentials
/// were lost.
///
/// Basename comparison delegates to `basenames_match` so the two
/// spellings of "match by final path component" stay in lockstep
/// (one update site if semantics ever tighten -- case folding,
/// NFC/NFD, etc.).
fn is_atomic_rewrite_remove(remove_path: &std::path::Path, batch: &[DebouncedEvent]) -> bool {
    use notify::event::ModifyKind;
    batch.iter().any(|ev| {
        matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
        ) && ev.paths.iter().any(|p| basenames_match(p, remove_path))
    })
}

/// Forward a reload request to the coordinator. A send error means the
/// coordinator's receiver was dropped (post-shutdown); we log at warn so
/// an operator notices a stuck pipeline rather than silently losing the
/// reload. `Sender::send` is async-suspending under back-pressure, so a
/// "channel full" case never surfaces here.
async fn send_reload(tx: &mpsc::Sender<ReloadRequest>, req: ReloadRequest) {
    if let Err(e) = tx.send(req.clone()).await {
        tracing::warn!(
            request = ?req,
            error = %e,
            "reload coordinator channel closed (receiver dropped); dropping reload request",
        );
    }
}

/// True if this event kind should map to a reload. Filters out
/// noisy kinds (`EventKind::Access`, `EventKind::Other`, and
/// `EventKind::Modify(Metadata(_))`) so a `cat credentials.json`
/// or a `chmod`/`touch`/`utimes` from another shell does not fire
/// a spurious reload.
const fn is_reload_kind(kind: &EventKind) -> bool {
    use notify::event::ModifyKind;
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
            | EventKind::Remove(_)
    )
}

/// Match by final path component. Both writers swap the inode under
/// the watched path via `rename`, so absolute-path equality is too
/// strict (the source path of the rename event may carry the
/// tempfile name); basename equality is the right granularity.
fn basenames_match(event_path: &std::path::Path, target_path: &std::path::Path) -> bool {
    match (event_path.file_name(), target_path.file_name()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration as StdDuration;
    use tempfile::tempdir;
    use tracing::field::{Field, Visit};

    /// One captured tracing event. `target` and `fields` are unused by
    /// the assertions but are kept on the record so a failing test's
    /// `{captured:?}` Debug output stays useful.
    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct CapturedEvent {
        level: tracing::Level,
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    }

    #[derive(Default)]
    struct FieldCollector {
        message: String,
        fields: Vec<(String, String)>,
    }

    impl Visit for FieldCollector {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else {
                self.fields.push((field.name().into(), value.into()));
            }
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let s = format!("{value:?}");
            if field.name() == "message" {
                self.message = s.trim_matches('"').to_string();
            } else {
                self.fields.push((field.name().into(), s));
            }
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.fields.push((field.name().into(), value.to_string()));
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.fields.push((field.name().into(), value.to_string()));
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields.push((field.name().into(), value.to_string()));
        }
    }

    #[derive(Default)]
    struct CaptureSubscriber {
        captured: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let meta = event.metadata();
            let mut visitor = FieldCollector::default();
            event.record(&mut visitor);
            let captured = CapturedEvent {
                level: *meta.level(),
                target: meta.target().to_string(),
                message: visitor.message,
                fields: visitor.fields,
            };
            if let Ok(mut guard) = self.captured.lock() {
                guard.push(captured);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Run `f` with an in-process tracing subscriber installed as the
    /// thread-local default. The subscriber records every event into a
    /// `Vec<CapturedEvent>` returned alongside `f`'s output. Scoped via
    /// `tracing::subscriber::with_default` so concurrent unit tests do
    /// not leak captured state across threads.
    fn with_capturing_subscriber<F, R>(f: F) -> (R, Vec<CapturedEvent>)
    where
        F: FnOnce() -> R,
    {
        let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            captured: captured.clone(),
        };
        let out = tracing::subscriber::with_default(subscriber, f);
        let events = captured.lock().expect("capture lock poisoned").clone();
        (out, events)
    }

    /// Settle delay between arming the watcher and mutating its target.
    /// `notify-debouncer-full` registers the inotify watch synchronously
    /// but the inotify thread needs a moment to enter its read loop;
    /// without this, the first mutation can race ahead and the test
    /// observes no event. 100 ms is well below the debounce window
    /// (`DEBOUNCE_MS = 250`) so it does not artificially extend the
    /// runtime of negative-path tests.
    const SETTLE_MS: u64 = 100;

    /// Wait for at most `max_wait` for a single ReloadRequest to
    /// arrive on `rx`. Returns `None` on timeout.
    async fn wait_for_one(
        rx: &mut mpsc::Receiver<ReloadRequest>,
        max_wait: StdDuration,
    ) -> Option<ReloadRequest> {
        tokio::time::timeout(max_wait, rx.recv())
            .await
            .ok()
            .flatten()
    }

    /// Drain everything sitting on `rx` for up to `quiet_window`. Used
    /// to assert "no further events arrived" after a positive case.
    async fn drain_quiet_window(
        rx: &mut mpsc::Receiver<ReloadRequest>,
        quiet_window: StdDuration,
    ) -> Vec<ReloadRequest> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + quiet_window;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ev)) => out.push(ev),
                _ => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn writing_watched_file_emits_reload_request() {
        // Arrange
        let dir = tempdir().unwrap();
        let target = dir.path().join("config.toml");
        std::fs::write(&target, b"# initial\n").unwrap();

        let (tx, mut rx) = mpsc::channel::<ReloadRequest>(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let _handle =
            spawn_watcher(vec![WatchTarget::Config(target.clone())], tx, shutdown_rx).unwrap();

        // Settle the initial watcher arming before mutating.
        tokio::time::sleep(StdDuration::from_millis(SETTLE_MS)).await;

        // Act
        std::fs::write(&target, b"# modified\n").unwrap();

        // Assert: at least one Config request arrives within the
        // debounce + polling slack window. Allow up to 3s on
        // platforms where the inotify queue is slow to wake.
        let got = wait_for_one(&mut rx, StdDuration::from_secs(3)).await;
        assert_eq!(got, Some(ReloadRequest::Config));
    }

    #[tokio::test]
    async fn writing_sibling_file_does_not_emit_reload_request() {
        // Arrange: watch config.toml under tempdir, then write a
        // SIBLING file (different basename) under the same parent.
        // Basename routing must drop the event.
        let dir = tempdir().unwrap();
        let target = dir.path().join("config.toml");
        let sibling = dir.path().join("notes.txt");
        std::fs::write(&target, b"# initial\n").unwrap();

        let (tx, mut rx) = mpsc::channel::<ReloadRequest>(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let _handle =
            spawn_watcher(vec![WatchTarget::Config(target.clone())], tx, shutdown_rx).unwrap();

        tokio::time::sleep(StdDuration::from_millis(SETTLE_MS)).await;

        // Act: write only to the sibling.
        std::fs::write(&sibling, b"hello\n").unwrap();

        // Assert: no ReloadRequest arrives within a full debounce +
        // polling window.
        let observed = drain_quiet_window(&mut rx, StdDuration::from_millis(800)).await;
        assert!(
            observed.is_empty(),
            "expected no reload requests for sibling-file write, got {observed:?}",
        );
    }

    #[tokio::test]
    async fn rapid_consecutive_writes_coalesce_to_one_reload() {
        // Arrange
        let dir = tempdir().unwrap();
        let target = dir.path().join("config.toml");
        std::fs::write(&target, b"# initial\n").unwrap();

        let (tx, mut rx) = mpsc::channel::<ReloadRequest>(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let _handle =
            spawn_watcher(vec![WatchTarget::Config(target.clone())], tx, shutdown_rx).unwrap();

        tokio::time::sleep(StdDuration::from_millis(SETTLE_MS)).await;

        // Act: three writes well within the 250ms debounce window.
        std::fs::write(&target, b"# v1\n").unwrap();
        std::fs::write(&target, b"# v2\n").unwrap();
        std::fs::write(&target, b"# v3\n").unwrap();

        // Assert: exactly one reload request (debounce coalesces
        // the burst). Allow a generous 3s for the first event;
        // then assert nothing else within an additional quiet
        // window.
        let first = wait_for_one(&mut rx, StdDuration::from_secs(3)).await;
        assert_eq!(first, Some(ReloadRequest::Config));
        let extras = drain_quiet_window(&mut rx, StdDuration::from_millis(500)).await;
        assert!(
            extras.is_empty(),
            "expected debounce to coalesce to one event; saw extras: {extras:?}",
        );
    }

    #[tokio::test]
    async fn rename_into_place_emits_one_reload() {
        // Arrange: mirror oauth/file_io.rs's write pattern: write a
        // tempfile in the same parent dir, persist (rename) onto
        // the watched basename. The watcher must see the final
        // post-rename state and emit exactly one ReloadRequest
        // even though the temp path is also under the watched
        // parent.
        let dir = tempdir().unwrap();
        let target = dir.path().join("credentials.json");
        std::fs::write(&target, b"{}").unwrap();

        let (tx, mut rx) = mpsc::channel::<ReloadRequest>(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let _handle = spawn_watcher(
            vec![WatchTarget::Credentials(target.clone())],
            tx,
            shutdown_rx,
        )
        .unwrap();

        tokio::time::sleep(StdDuration::from_millis(SETTLE_MS)).await;

        // Act: tempfile + persist (atomic rename). NamedTempFile
        // in the same parent dir mirrors `write_to_temp` /
        // `atomic_rename` in oauth/file_io.rs.
        let tmp = tempfile::Builder::new()
            .prefix(".credentials.tmp.")
            .suffix(".json")
            .tempfile_in(dir.path())
            .unwrap();
        std::fs::write(tmp.path(), br#"{"schema_version":1,"providers":{}}"#).unwrap();
        tmp.persist(&target).unwrap();

        // Assert: exactly one ReloadRequest::Credentials arrives.
        let first = wait_for_one(&mut rx, StdDuration::from_secs(3)).await;
        assert_eq!(first, Some(ReloadRequest::Credentials));
        let extras = drain_quiet_window(&mut rx, StdDuration::from_millis(500)).await;
        assert!(
            extras.is_empty(),
            "rename-into-place should emit exactly one reload; saw extras: {extras:?}",
        );
    }

    /// Build a `DebouncedEvent` for the suppression-decision tests.
    /// The `time` field is irrelevant to the decision; we use a
    /// single shared `Instant` so the events are minimally distinct
    /// from production batches.
    fn make_event(kind: EventKind, path: &std::path::Path) -> DebouncedEvent {
        DebouncedEvent::new(
            notify::Event::new(kind).add_path(path.to_path_buf()),
            std::time::Instant::now(),
        )
    }

    #[test]
    fn paired_remove_and_create_same_basename_is_atomic_rewrite() {
        // Arrange: simulate an atomic-rewrite editor (vim, Edit/Write
        // tool, safe-save) that issues Remove + Create for the same
        // basename within a single debounce window. Production paths
        // are absolute under a parent dir; tempfile sources may carry
        // a different prefix but the final path equals target.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let target = dir.join("config.toml");
        let batch = vec![
            make_event(EventKind::Remove(notify::event::RemoveKind::File), &target),
            make_event(EventKind::Create(notify::event::CreateKind::File), &target),
        ];

        // Assert: suppression decision says "this is an atomic
        // rewrite, do not WARN".
        assert!(
            is_atomic_rewrite_remove(&target, &batch),
            "Remove paired with Create on same basename should be classified as atomic rewrite",
        );
    }

    #[test]
    fn paired_remove_and_modify_same_basename_is_atomic_rewrite() {
        // Some editors emit Remove + Modify (rather than Create) for
        // the rename-into-place pattern. Suppression rule must cover
        // both Create and Modify.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let target = dir.join("credentials.json");
        let batch = vec![
            make_event(EventKind::Remove(notify::event::RemoveKind::File), &target),
            make_event(
                EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::To,
                )),
                &target,
            ),
        ];

        assert!(
            is_atomic_rewrite_remove(&target, &batch),
            "Remove paired with Modify on same basename should be classified as atomic rewrite",
        );
    }

    #[test]
    fn lone_remove_is_not_atomic_rewrite() {
        // Arrange: genuine deletion -- only a Remove event in the
        // batch, no sibling Create/Modify with the same basename.
        // This case must surface as a WARN so an operator who
        // accidentally deleted credentials.json sees the alert.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let target = dir.join("config.toml");
        let batch = vec![make_event(
            EventKind::Remove(notify::event::RemoveKind::File),
            &target,
        )];

        assert!(
            !is_atomic_rewrite_remove(&target, &batch),
            "lone Remove with no sibling Create/Modify must NOT be suppressed (genuine deletion)",
        );
    }

    #[test]
    fn remove_with_different_basename_create_is_not_atomic_rewrite() {
        // Arrange: a Remove of config.toml co-existing with a Create
        // of an unrelated sibling under the same parent. Basename
        // disambiguates -- the Create does not pair with the Remove.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let removed = dir.join("config.toml");
        let other_create = dir.join("notes.txt");
        let batch = vec![
            make_event(EventKind::Remove(notify::event::RemoveKind::File), &removed),
            make_event(
                EventKind::Create(notify::event::CreateKind::File),
                &other_create,
            ),
        ];

        assert!(
            !is_atomic_rewrite_remove(&removed, &batch),
            "Create on a different basename must not pair with Remove on config.toml",
        );
    }

    /// Drive `handle_event_batch` directly with a fabricated batch and
    /// return the captured tracing events. Pinned to a current_thread
    /// runtime so the thread-local subscriber installed by
    /// `with_capturing_subscriber` applies for every poll of the future.
    fn drive_handle_event_batch(
        batch: Vec<DebouncedEvent>,
        targets: Vec<WatchTarget>,
    ) -> Vec<CapturedEvent> {
        let (tx, _rx) = mpsc::channel::<ReloadRequest>(8);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current_thread runtime");
        let ((), events) = with_capturing_subscriber(|| {
            rt.block_on(handle_event_batch(batch, &targets, &tx));
        });
        events
    }

    #[test]
    fn paired_remove_and_create_emits_debug_breadcrumb_not_warn() {
        // Arrange: an atomic-rewrite batch (Remove + Create on the same
        // basename within one debounce window). The WARN-suppression
        // fix lives at handle_event_batch's call site; this test pins
        // the actual user-visible dispatch (no WARN, one DEBUG) so a
        // future regression that inverts the suppression condition
        // would fail here -- not just on the helper's unit tests.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let target = dir.join("config.toml");
        let batch = vec![
            make_event(EventKind::Remove(notify::event::RemoveKind::File), &target),
            make_event(EventKind::Create(notify::event::CreateKind::File), &target),
        ];
        let targets = vec![WatchTarget::Config(target)];

        // Act
        let events = drive_handle_event_batch(batch, targets);

        // Assert: no WARN with the lone-Remove wording fires.
        let warns: Vec<&CapturedEvent> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .collect();
        assert!(
            warns.is_empty(),
            "expected no WARN-level events under atomic-rewrite suppression, got {warns:?}",
        );

        // Assert: exactly one DEBUG breadcrumb fires for the suppressed
        // Remove. Match on the unique "atomic rewrite" tail of the
        // breadcrumb wording so a wording tweak elsewhere does not
        // nullify the assertion silently.
        let debug_breadcrumbs: Vec<&CapturedEvent> = events
            .iter()
            .filter(|e| e.level == tracing::Level::DEBUG && e.message.contains("atomic rewrite"))
            .collect();
        assert_eq!(
            debug_breadcrumbs.len(),
            1,
            "expected exactly one DEBUG 'atomic rewrite' breadcrumb, got {debug_breadcrumbs:?}",
        );
    }

    #[test]
    fn lone_remove_emits_warn_with_existing_wording() {
        // Arrange: a genuine deletion -- only a Remove event in the
        // batch, no sibling Create/Modify. The original WARN must
        // still fire so an operator who accidentally deleted
        // credentials.json sees the alert.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let target = dir.join("config.toml");
        let batch = vec![make_event(
            EventKind::Remove(notify::event::RemoveKind::File),
            &target,
        )];
        let targets = vec![WatchTarget::Config(target)];

        // Act
        let events = drive_handle_event_batch(batch, targets);

        // Assert: exactly one WARN fires with the exact wording shipped
        // by the post-fix code at handle_event_batch's WARN branch.
        // String-equality on the full message catches both wording
        // drift and accidental level downgrades (e.g. WARN -> INFO).
        let expected_warn = "watched file was removed; keeping last known good in-memory state \
                             (will reload on next Create event)";
        let warns: Vec<&CapturedEvent> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "expected exactly one WARN for lone Remove, got {warns:?}",
        );
        assert_eq!(
            warns[0].message, expected_warn,
            "WARN wording for lone Remove changed; if intentional, update the test \
             alongside the production string. Got: {:?}",
            warns[0].message,
        );
    }

    // --- Finding #4: metadata events must not trigger reloads ---------

    #[test]
    fn is_reload_kind_passes_data_writes_and_creates() {
        // Arrange + Assert: the kinds that SHOULD trigger a reload pass
        // the filter. This pins the positive side of the tightened gate.
        let allowed: &[EventKind] = &[
            EventKind::Create(notify::event::CreateKind::File),
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To,
            )),
            EventKind::Remove(notify::event::RemoveKind::File),
        ];
        for kind in allowed {
            assert!(
                is_reload_kind(kind),
                "is_reload_kind should return true for {kind:?} but returned false",
            );
        }
    }

    #[test]
    fn is_reload_kind_excludes_metadata_events() {
        // Arrange + Assert: chmod / touch / utimes / access events must
        // NOT pass the filter. Before the fix, Modify(Metadata(_)) fell
        // through the broad `Modify(_)` arm.
        let blocked: &[EventKind] = &[
            EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::Permissions,
            )),
            EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::WriteTime,
            )),
            EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::AccessTime,
            )),
            EventKind::Access(notify::event::AccessKind::Read),
            EventKind::Access(notify::event::AccessKind::Open(
                notify::event::AccessMode::Read,
            )),
        ];
        for kind in blocked {
            assert!(
                !is_reload_kind(kind),
                "is_reload_kind should return false for {kind:?} but returned true",
            );
        }
    }

    #[test]
    fn metadata_event_does_not_trigger_reload() {
        // Arrange: a single Metadata(Permissions) event -- the exact
        // kind emitted by `chmod 600 config.toml`. Before the fix this
        // caused a spurious reload; now it must produce zero
        // ReloadRequest sends.
        let dir = std::path::PathBuf::from("/tmp/routectl-test");
        let target = dir.join("config.toml");
        let batch = vec![make_event(
            EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::Permissions,
            )),
            &target,
        )];
        let targets = vec![WatchTarget::Config(target)];

        // Act
        let events = drive_handle_event_batch(batch, targets);

        // Assert: no WARN and no INFO (the reload branch emits no
        // events for silently-dropped kinds). Any event at all here
        // means the filter did not engage.
        let reload_events: Vec<&CapturedEvent> = events
            .iter()
            .filter(|e| {
                // The reload path emits a DEBUG breadcrumb with the
                // target path when it dispatches. Match on any message
                // referencing config.toml to catch false positives.
                e.message.contains("config.toml") || e.message.to_lowercase().contains("reload")
            })
            .collect();
        assert!(
            reload_events.is_empty(),
            "metadata-only event must not emit any reload-related log entries; \
             got {reload_events:?}",
        );
    }

    // --- Finding #3: symlinked config paths must fire reloads ----------

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_config_path_fires_reload() {
        // Arrange: real file under one tempdir; symlink under another
        // dir points to the real file. `spawn_watcher` receives the
        // symlink path. Canonicalize inside `spawn_watcher` must resolve
        // to the real file's parent dir so the inotify watch fires when
        // the real file is written.
        let real_dir = tempdir().unwrap();
        let real_file = real_dir.path().join("config.toml");
        std::fs::write(&real_file, b"# initial\n").unwrap();

        let link_dir = tempdir().unwrap();
        let link_path = link_dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real_file, &link_path).unwrap();

        let (tx, mut rx) = mpsc::channel::<ReloadRequest>(8);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let _handle = spawn_watcher(
            vec![WatchTarget::Config(link_path.clone())],
            tx,
            shutdown_rx,
        )
        .unwrap();

        tokio::time::sleep(StdDuration::from_millis(SETTLE_MS)).await;

        // Act: write to the real file (NOT through the symlink). The
        // inotify watch on the real dir should fire.
        std::fs::write(&real_file, b"# changed via real path\n").unwrap();

        // Assert: a Config reload request arrives within the debounce
        // + polling slack window.
        let got = wait_for_one(&mut rx, StdDuration::from_secs(3)).await;
        assert_eq!(
            got,
            Some(ReloadRequest::Config),
            "writing to the real file behind a symlink must fire a Config reload",
        );
    }
}
