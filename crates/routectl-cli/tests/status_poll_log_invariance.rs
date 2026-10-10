//! Polling the status surface must not grow the daemon's log, and must not
//! re-read on-disk state, however often it is polled.
//!
//! Driven against the REAL binary in a child process, because the property is
//! about what the assembled daemon writes to its own log sink: the production
//! subscriber, the request-span levels, the boot warm, and the `/status` and
//! `/status/doctor` builders all have to be wired exactly as shipped for the
//! count to mean anything.
//!
//! The fixture seeds the two inputs that used to make every poll log: a catalog
//! overlay carrying a soft cell defect (a below-sentinel write multiplier) and
//! usage-ledger rows the replay cannot map (an unknown vocabulary version). The
//! boot warm sees both once. A poll must see neither again: it renders the
//! daemon's resident state, so it neither reloads the overlay file nor replays
//! the ledger, and the field-verdict snapshot line is change-gated.
//!
//! Log reads are synchronised by a barrier rather than a sleep: a request to a
//! non-polling path closes an INFO span carrying a caller-chosen request id.
//! The child writes its log lines in order on one pipe, and every polled
//! response has returned before the barrier request is sent, so every line a
//! poll produced precedes the barrier line.
//!
//! Isolation: the child runs with a CLEARED environment whose `HOME` and
//! `XDG_CONFIG_HOME` point into a fresh tempdir, so the config, the overlay and
//! the usage ledger all resolve there and never under the developer's real
//! config directory. The child binds a fresh loopback port.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use routectl_router::{CATALOG_VERSION, CURRENT_CONFIG_VERSION, CURRENT_VOCAB_VERSION};
use serde_json::Value;

/// The real `routectl` binary under test, resolved by cargo for this crate.
const BIN: &str = env!("CARGO_BIN_EXE_routectl");

/// Give-up bound for the daemon to begin serving.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// Give-up bound for one barrier line to reach the parent.
const BARRIER_DEADLINE: Duration = Duration::from_secs(10);

/// Give-up bound for the file watcher to deliver the invalid overlay and the
/// daemon to record the rejected reload.
const RELOAD_DEADLINE: Duration = Duration::from_secs(15);

/// Per-request socket timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Ledger rows stamped with a vocabulary version this build does not know.
/// Each one is skipped by every replay that reads it.
const UNMAPPABLE_ROWS: usize = 4;

/// Overlay revision the fixture stamps on the overlay, the tombstone and the
/// rows, so the boot warm finds a matching boundary and replays the slice.
const OVERLAY_REVISION: i64 = 3;

/// The overlay cell carrying the soft defect.
const DEFECT_SELECTOR: &str = "openai-compat:grok-*";

/// Directive the child logs under: the default level plus the per-row
/// rebuild-skip lines, so a replay is visible in the log.
const LOG_DIRECTIVE: &str = "info,routectl_router::capability_rebuild=debug";

const FIELD_VERDICT_MESSAGE: &str = "envelope field verdict snapshot";
const REBUILD_SKIP_EVENT: &str = "rebuild_skip";
const OVERLAY_SOFT_DEFECT_MESSAGE: &str = "below-sentinel write multiplier";
const WARM_MESSAGE: &str = "warmed learned-capability registry";
const RELOAD_REJECTED_MESSAGE: &str = "config reload failed";

/// Line counts by class at one barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counts {
    /// WARN/ERROR lines other than a rejected reload's own.
    warn_or_error: usize,
    reload_rejected: usize,
    field_verdict: usize,
    rebuild_skip: usize,
    overlay_soft_defect: usize,
    warm: usize,
}

/// One child log line, decoded just enough to classify it.
struct LogLine {
    level: Option<String>,
    text: String,
}

impl LogLine {
    fn parse(raw: &str) -> Self {
        let text = strip_ansi(raw);
        // Format: `<timestamp> <LEVEL> <spans/target>: <message> <fields>`.
        let level = text
            .split_whitespace()
            .take(3)
            .find(|token| ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"].contains(token))
            .map(str::to_string);
        Self { level, text }
    }

    fn is(&self, level: &str) -> bool {
        self.level.as_deref() == Some(level)
    }
}

/// Remove ANSI CSI sequences (`ESC [ ... final-byte`) in case styling is on.
fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn count(lines: &[LogLine]) -> Counts {
    Counts {
        warn_or_error: lines
            .iter()
            .filter(|l| l.is("WARN") || l.is("ERROR"))
            .filter(|l| !l.text.contains(RELOAD_REJECTED_MESSAGE))
            .count(),
        reload_rejected: lines
            .iter()
            .filter(|l| l.is("WARN") && l.text.contains(RELOAD_REJECTED_MESSAGE))
            .count(),
        field_verdict: lines
            .iter()
            .filter(|l| l.text.contains(FIELD_VERDICT_MESSAGE))
            .count(),
        rebuild_skip: lines
            .iter()
            .filter(|l| l.is("DEBUG") && l.text.contains(REBUILD_SKIP_EVENT))
            .count(),
        overlay_soft_defect: lines
            .iter()
            .filter(|l| l.is("WARN") && l.text.contains(OVERLAY_SOFT_DEFECT_MESSAGE))
            .count(),
        warm: lines
            .iter()
            .filter(|l| l.is("INFO") && l.text.contains(WARM_MESSAGE))
            .count(),
    }
}

/// The child's stderr, collected line by line on a reader thread so the pipe
/// never fills however much the child logs.
#[derive(Clone, Default)]
struct LogTail(Arc<Mutex<Vec<LogLine>>>);

impl LogTail {
    fn follow(stderr: impl Read + Send + 'static) -> Self {
        let tail = Self::default();
        let sink = tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                sink.0
                    .lock()
                    .expect("log tail lock")
                    .push(LogLine::parse(&line));
            }
        });
        tail
    }

    /// Index one past the first line at or after `from` matching `pred`.
    fn wait_for(&self, from: usize, deadline: Duration, pred: impl Fn(&LogLine) -> bool) -> usize {
        let until = Instant::now() + deadline;
        loop {
            {
                let lines = self.0.lock().expect("log tail lock");
                if let Some(at) = lines.iter().skip(from).position(&pred) {
                    return from + at + 1;
                }
            }
            assert!(
                Instant::now() < until,
                "no matching log line within {deadline:?}; log so far:\n{}",
                self.dump(),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn counts_until(&self, end: usize) -> Counts {
        count(&self.0.lock().expect("log tail lock")[..end])
    }

    fn texts(&self, range: std::ops::Range<usize>) -> Vec<String> {
        self.0.lock().expect("log tail lock")[range]
            .iter()
            .map(|l| l.text.clone())
            .collect()
    }

    fn dump(&self) -> String {
        let lines = self.0.lock().expect("log tail lock");
        lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A child `routectl serve`, killed and reaped when dropped so a failing
/// assertion cannot leak a live daemon.
struct Daemon {
    child: Child,
    port: u16,
    log: LogTail,
    barriers: usize,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn spawn(home: &Path, config: &Path, port: u16) -> Self {
        let mut child = daemon_command(home, config)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {BIN} serve on port {port}: {e}"));
        let stderr = child.stderr.take().expect("stderr is piped");
        let mut daemon = Self {
            child,
            port,
            log: LogTail::follow(stderr),
            barriers: 0,
        };
        daemon.await_serving();
        daemon
    }

    fn await_serving(&mut self) {
        let until = Instant::now() + READY_DEADLINE;
        while Instant::now() < until {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                panic!(
                    "the daemon exited during startup with {status}; log:\n{}",
                    self.log.dump()
                );
            }
            if matches!(try_get(self.port, "/health", None), Some((200, _))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the daemon never began serving on port {}", self.port);
    }

    /// Wait until every line logged before this call has reached the parent,
    /// and return the line count at that point.
    fn barrier(&mut self) -> usize {
        self.barriers += 1;
        let marker = format!("poll-invariance-barrier-{}", self.barriers);
        let _ = get(self.port, "/v1/models", Some(&marker));
        let needle = format!("request_id={marker}");
        self.log.wait_for(0, BARRIER_DEADLINE, |l| {
            l.is("INFO") && l.text.contains(&needle) && l.text.contains("close")
        })
    }

    /// Line counts by class over everything logged before this call.
    fn checkpoint(&mut self) -> Counts {
        let end = self.barrier();
        self.log.counts_until(end)
    }

    fn poll_both(&self, times: usize) {
        for _ in 0..times {
            let (status, body) = get(self.port, "/status", None);
            assert_eq!(status, 200, "/status answered {status}: {body}");
            self.doctor();
        }
    }

    /// The `/status/doctor` payload.
    fn doctor(&self) -> Value {
        let (status, body) = get(self.port, "/status/doctor", None);
        assert_eq!(status, 200, "/status/doctor answered {status}: {body}");
        let panel: Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("/status/doctor is not JSON ({e}): {body}"));
        assert!(
            panel["unavailable"].is_null(),
            "the doctor panel must be available: {panel}"
        );
        panel["data"]["report"].clone()
    }
}

/// The child command: a cleared environment carrying only the scoped home and
/// the logging knobs.
fn daemon_command(home: &Path, config: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .env_clear()
        .arg("--config")
        .arg(config)
        .arg("serve")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ROUTECTL_LOG", LOG_DIRECTIVE)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

/// One HTTP/1.0 GET over a plain socket: `(status, body)`, or `None` when the
/// daemon is not answering yet.
fn try_get(port: u16, path: &str, request_id: Option<&str>) -> Option<(u16, String)> {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    socket.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    let id_header = request_id.map_or_else(String::new, |id| format!("x-request-id: {id}\r\n"));
    let request = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n{id_header}\r\n");
    socket.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).ok()?;
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response
        .split_once("\r\n\r\n")
        .map_or_else(String::new, |(_, body)| body.to_string());
    Some((status, body))
}

fn get(port: u16, path: &str, request_id: Option<&str>) -> (u16, String) {
    try_get(port, path, request_id)
        .unwrap_or_else(|| panic!("GET {path} on port {port} got no HTTP response"))
}

/// A loopback port the OS handed out as free a moment ago.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

/// The routectl config directory the child resolves from its scoped home.
fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("routectl")
}

fn write_config(dir: &Path, port: u16) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = {CURRENT_CONFIG_VERSION}\n\
             [server]\n\
             host = \"127.0.0.1\"\n\
             port = {port}\n\
             [usage]\n\
             enabled = true\n"
        ),
    )
    .expect("write config");
    path
}

/// An overlay whose one cell carries a finite write multiplier below the
/// sentinel: accepted by the loader, reported as a soft defect.
fn write_overlay(dir: &Path) -> PathBuf {
    let path = dir.join("catalog_overlay.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"schema_version":1,"revision":{OVERLAY_REVISION},"cells":{{"{DEFECT_SELECTOR}":{{"source":"user","verified_at":"2026-01-01","wm":1.0}}}}}}"#
        ),
    )
    .expect("write overlay");
    path
}

/// A ledger whose newest tombstone matches this boot's catalog version and the
/// overlay's revision, followed by rows in an unknown vocabulary version.
fn seed_ledger(dir: &Path) -> PathBuf {
    let path = dir.join("usage.db");
    let db = routectl_usage::open(&path).expect("create + migrate usage db");
    let catalog = i64::from(CATALOG_VERSION);
    let ts = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits i64");
    let insert = |event: &routectl_usage::CapabilityEvent| {
        routectl_usage::insert_capability_event(db.conn(), event).expect("insert capability event");
    };
    insert(&routectl_usage::CapabilityEvent::tombstone(
        ts,
        catalog,
        OVERLAY_REVISION,
    ));
    for _ in 0..UNMAPPABLE_ROWS {
        insert(&routectl_usage::CapabilityEvent {
            ts,
            lane_key: "local#m-a".to_string(),
            capability: "web_search".to_string(),
            verdict: "broken".to_string(),
            phase: "f1".to_string(),
            source: "live".to_string(),
            tier: "self-identifying".to_string(),
            evidence_class: None,
            upstream_token: None,
            catalog_version: catalog,
            overlay_revision: OVERLAY_REVISION,
            provider_kind: Some("openai-compat".to_string()),
            vocab_version: Some(CURRENT_VOCAB_VERSION + 1),
        });
    }
    drop(db);
    path
}

/// Replace the overlay with text that fails to parse, in one rename so the
/// watcher sees a single complete write.
fn corrupt_overlay(path: &Path) {
    let staged = path.with_extension("json.staged");
    std::fs::write(&staged, b"{ not json").expect("write invalid overlay");
    std::fs::rename(&staged, path).expect("swap in invalid overlay");
}

fn findings(report: &Value) -> &[Value] {
    report["findings"].as_array().map_or(&[], Vec::as_slice)
}

fn finding<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    findings(report).iter().find(|f| f["name"] == name)
}

fn assert_overlay_defect_finding(report: &Value) {
    let defect = finding(report, "catalog overlay")
        .unwrap_or_else(|| panic!("the served overlay's soft defect must be a finding: {report}"));
    assert!(
        defect["detail"]
            .as_str()
            .is_some_and(|d| d.contains(DEFECT_SELECTOR)),
        "the soft-defect finding must name its cell: {defect}"
    );
}

fn assert_no_overlay_load_error(report: &Value) {
    let schema = finding(report, "config schema")
        .unwrap_or_else(|| panic!("the report must carry the config schema finding: {report}"));
    assert_eq!(
        schema["status"], "Pass",
        "a poll must not load the on-disk overlay: {schema}"
    );
    assert!(
        findings(report).iter().all(|f| !f["detail"]
            .as_str()
            .unwrap_or("")
            .contains("could not be loaded")),
        "a poll must not report an overlay load error: {report}"
    );
}

#[test]
fn polling_status_neither_grows_the_log_nor_rereads_disk_state() {
    // Arrange: hermetic home, seeded overlay and ledger, a serving daemon.
    let port = free_port();
    let home = tempfile::tempdir().expect("tempdir");
    let dir = config_dir(home.path());
    std::fs::create_dir_all(&dir).expect("mkdir config dir");
    let config = write_config(&dir, port);
    let overlay = write_overlay(&dir);
    let ledger = seed_ledger(&dir);
    for path in [&config, &overlay, &ledger] {
        assert!(
            path.starts_with(home.path()),
            "{} must live in the test's tempdir",
            path.display()
        );
    }
    let mut daemon = Daemon::spawn(home.path(), &config, port);
    daemon.log.wait_for(0, BARRIER_DEADLINE, |l| {
        l.text
            .contains(&format!("listening on http://127.0.0.1:{port}"))
    });

    let settled = assert_polls_leave_the_log_flat(&mut daemon);
    assert_a_rejected_reload_keeps_the_served_overlay(&mut daemon, &overlay, settled);
}

/// Boot, then 3 polls of each endpoint, then 30 more: every class stays at its
/// boot count. Returns the counts after the last poll.
fn assert_polls_leave_the_log_flat(daemon: &mut Daemon) -> Counts {
    // Act
    let boot = daemon.checkpoint();
    daemon.poll_both(3);
    let after_3 = daemon.checkpoint();
    daemon.poll_both(30);
    let after_33 = daemon.checkpoint();
    let report = daemon.doctor();

    // Assert: the boot logged each seeded condition once, and polling added
    // nothing that scales with the poll count.
    assert_eq!(
        boot.rebuild_skip,
        UNMAPPABLE_ROWS,
        "the boot warm skips each unmappable row once; log:\n{}",
        daemon.log.dump()
    );
    assert_eq!(boot.overlay_soft_defect, 1, "log:\n{}", daemon.log.dump());
    assert_eq!(boot.warm, 1, "log:\n{}", daemon.log.dump());
    assert_eq!(
        after_3.warn_or_error, boot.warn_or_error,
        "{after_3:?} vs {boot:?}"
    );
    assert_eq!(
        after_33.warn_or_error, after_3.warn_or_error,
        "{after_33:?} vs {after_3:?}"
    );
    assert_eq!(
        after_33.rebuild_skip, UNMAPPABLE_ROWS,
        "a poll must not replay the ledger"
    );
    assert_eq!(
        after_33.overlay_soft_defect, 1,
        "a poll must not reload the overlay"
    );
    assert_eq!(after_33.warm, 1);
    // The first poll emits the snapshot (the classifier's positive control);
    // unchanged state keeps every later poll silent.
    assert_eq!(after_3.field_verdict, 1, "{after_3:?}");
    assert_eq!(after_33.field_verdict, 1, "{after_33:?}");

    let matrix = &report["panels"]["capability_matrix"];
    assert_eq!(matrix["source"], "resident", "{matrix}");
    assert_eq!(matrix["warm"]["outcome"], "replayed", "{matrix}");
    assert_eq!(
        matrix["warm"]["summary"]["skipped_vocab"], UNMAPPABLE_ROWS,
        "the boot warm must have read the seeded rows: {matrix}"
    );
    assert_overlay_defect_finding(&report);
    assert_no_overlay_load_error(&report);
    after_33
}

/// Make the on-disk overlay unloadable: the daemon refuses the reload, the
/// doctor reports the refusal while still rendering the served overlay, and
/// later polls neither re-read the broken file nor log per poll.
fn assert_a_rejected_reload_keeps_the_served_overlay(
    daemon: &mut Daemon,
    overlay: &Path,
    after_33: Counts,
) {
    // Act
    corrupt_overlay(overlay);
    let until = Instant::now() + RELOAD_DEADLINE;
    let rejected = loop {
        let report = daemon.doctor();
        if finding(&report, "reload").is_some() {
            break report;
        }
        assert!(
            Instant::now() < until,
            "no rejected-reload finding within {RELOAD_DEADLINE:?}; last report: {report}\nlog:\n{}",
            daemon.log.dump()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let after_reload_end = daemon.barrier();
    let after_reload = daemon.log.counts_until(after_reload_end);
    daemon.poll_both(30);
    let after_more_end = daemon.barrier();
    let after_more = daemon.log.counts_until(after_more_end);
    let report = daemon.doctor();

    // Assert: the rejection is reported from the daemon's record while the
    // served overlay keeps rendering, and nothing re-reads the broken file.
    let reload = finding(&rejected, "reload").expect("reload finding");
    assert!(
        reload["detail"]
            .as_str()
            .is_some_and(|d| d.contains("overlay_load_failed")),
        "the reload finding must name the overlay class: {reload}"
    );
    for report in [&rejected, &report] {
        assert_overlay_defect_finding(report);
        assert_no_overlay_load_error(report);
        assert_eq!(report["panels"]["capability_matrix"]["source"], "resident");
    }
    assert_eq!(after_33.reload_rejected, 0);
    assert!(
        after_reload.reload_rejected >= 1,
        "the rejected reload logs its WARN; log:\n{}",
        daemon.log.dump()
    );
    assert_eq!(
        after_reload.warn_or_error,
        after_33.warn_or_error,
        "the rejected reload logs nothing at WARN/ERROR beyond its own line: {:?}",
        daemon.log.texts(0..after_reload_end)
    );
    assert_eq!(
        after_more.warn_or_error,
        after_reload.warn_or_error,
        "polls after a rejected reload must add no WARN/ERROR: {:?}",
        daemon.log.texts(after_reload_end..after_more_end)
    );
    assert_eq!(after_more.rebuild_skip, UNMAPPABLE_ROWS);
    assert_eq!(after_more.overlay_soft_defect, 1);
    assert_eq!(after_more.field_verdict, after_33.field_verdict);
}
