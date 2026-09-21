//! The daemon's graceful shutdown, driven against the REAL binary in a CHILD
//! process.
//!
//! WHY A SEPARATE PROCESS. The property under test needs a real SIGTERM, and a
//! signal is a PROCESS-WIDE fact: sent from inside the unit-test binary it
//! reaches that binary, where SIGTERM's default disposition terminates every
//! test running in it -- reported as an exit code with no failing test name.
//! Working around that from inside (registering a handler, retaining a stream)
//! means the test must alter the disposition every other test in the process
//! inherits. Signalling a child instead removes the hazard rather than managing
//! it: this parent never registers a SIGTERM handler and never touches its own
//! disposition, so nothing here can affect a sibling test.
//!
//! WHAT IT PINS. The daemon holds a producer handle to its usage writer through
//! the Router's paid-probe accounting adapter, so a shutdown that drains the
//! writer before releasing the Router waits out the writer's whole abandon
//! deadline. The bound below is far under that deadline and far over a healthy
//! shutdown, so it fails loudly on the ordering regression and on nothing else.
//!
//! Isolation: `HOME` and `XDG_CONFIG_HOME` are scoped to a fresh tempdir ON THE
//! CHILD ONLY, so the config, the usage ledger, and the catalog state all
//! resolve inside it and never touch the developer's real `~/.config/routectl`.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use routectl_router::CURRENT_CONFIG_VERSION as CURRENT;

/// The real `routectl` binary under test, resolved by cargo for this crate.
const BIN: &str = env!("CARGO_BIN_EXE_routectl");

/// The ceiling the shutdown must beat.
///
/// Chosen against the two numbers that bracket it: the usage writer's own
/// drain-abandon deadline is 5s, and a healthy shutdown with nothing to wait
/// for completes in milliseconds. A regression that drains before releasing the
/// Router pays that full 5s, so at 4s this discriminates -- it is not a
/// generous no-hang backstop but the assertion itself.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(4);

/// How long the parent waits for the child to exit before giving up, killing
/// it, and failing by name.
///
/// Deliberately above [`SHUTDOWN_BUDGET`]: a child that exceeds the budget but
/// still exits produces the precise "took too long" diagnosis, while this only
/// catches a child that never reacts at all.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// How long the parent waits for the daemon to begin serving.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// A child `routectl serve` that is killed and reaped if the test leaves early.
///
/// Cleanup lives in `Drop` rather than at each exit path because a panicking
/// assertion is the case that matters: without it a failing run leaks a live
/// daemon holding a port for the rest of the suite. Both steps are required --
/// `kill` alone leaves a zombie, so the wait is what actually reaps it.
struct ServeChild {
    child: Option<Child>,
}

/// Build the hermetic child command without inheriting config overrides.
fn serve_command(home: &Path, config: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg("--config")
        .arg(config)
        .arg("serve")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("ROUTECTL_CONFIG")
        // Quiet: the daemon's own logs are not the subject, and an unread
        // inherited pipe could fill and block it.
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

impl ServeChild {
    fn spawn(home: &Path, config: &Path, port: u16) -> Self {
        let child = serve_command(home, config)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {BIN} serve on port {port}: {e}"));
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child is live").id()
    }

    /// Wait for exit under `deadline`, returning the status.
    ///
    /// Polls `try_wait` rather than blocking on `wait`: a blocking wait has no
    /// timeout, so a child that never exits would hang the suite instead of
    /// failing this test.
    fn wait_until(&mut self, deadline: Duration) -> Option<std::process::ExitStatus> {
        let child = self.child.as_mut().expect("child is live");
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            match child.try_wait().expect("poll child") {
                Some(status) => return Some(status),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        None
    }

    /// Whatever the child wrote to stderr, for a failure message.
    fn stderr(&mut self) -> String {
        let mut buf = String::new();
        if let Some(child) = self.child.as_mut()
            && let Some(mut err) = child.stderr.take()
        {
            let _ = err.read_to_string(&mut buf);
        }
        buf
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Already-exited is the normal path and reports an error here;
            // ignore it and reap either way.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A hermetic config directory with a serve config on `port`.
fn hermetic_home(port: u16) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_dir = dir.path().join(".config").join("routectl");
    std::fs::create_dir_all(&config_dir).expect("mkdir config dir");
    let config_path = config_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {CURRENT}\n\
             [server]\n\
             host = \"127.0.0.1\"\n\
             port = {port}\n\
             [usage]\n\
             enabled = true\n"
        ),
    )
    .expect("write config");
    (dir, config_path)
}

/// A loopback port that was free a moment ago.
///
/// Bound and released to learn the number: the child binds it itself, so this
/// is inherently a window rather than a reservation -- which is why a failure
/// to serve is reported as such rather than silently retried.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("bound address").port()
}

/// Whether `/health` answers with a success status.
///
/// A hand-rolled request over a plain socket rather than an HTTP client: the
/// crate's `reqwest` carries no blocking feature, and enabling one across the
/// whole graph to read nine bytes of status line would be a heavier dependency
/// change than this test warrants. Only the status line is parsed, which is all
/// readiness turns on.
fn health_is_serving(port: u16) -> bool {
    use std::io::Write;

    let Ok(mut socket) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    if socket
        .write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    if socket.read_to_string(&mut response).is_err() {
        return false;
    }
    response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")
}

/// Poll `/health` until the daemon serves it, or fail once `READY_DEADLINE`
/// elapses.
///
/// A readiness PROBE, not a sleep: it answers only once the whole boot sequence
/// has finished (writer started, accounting adapter installed, Router
/// published), which is the state the shutdown measurement needs. A child that
/// died during boot is detected here rather than mistaken for a slow start.
fn await_serving(child: &mut ServeChild, port: u16) {
    let until = Instant::now() + READY_DEADLINE;
    while Instant::now() < until {
        if let Some(status) = child
            .child
            .as_mut()
            .expect("child is live")
            .try_wait()
            .expect("poll child")
        {
            panic!(
                "the daemon exited during startup with {status}; stderr:\n{}",
                child.stderr()
            );
        }
        if health_is_serving(port) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the daemon never began serving on port {port}");
}

/// The child must use the fixture even when the parent carries a config override.
#[test]
fn the_child_command_uses_only_the_explicit_fixture_config() {
    let home = Path::new("fixture-home");
    let config = Path::new("fixture-config.toml");
    let command = serve_command(home, config);
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(
        args,
        [
            std::ffi::OsStr::new("--config"),
            config.as_os_str(),
            std::ffi::OsStr::new("serve"),
        ],
    );
    assert!(
        command
            .get_envs()
            .any(|(name, value)| name == "ROUTECTL_CONFIG" && value.is_none()),
        "the child command must remove any ambient ROUTECTL_CONFIG override",
    );
}

/// A real daemon must complete graceful shutdown far inside the usage writer's
/// drain-abandon deadline.
///
/// The regression this exists for: the Router carries the paid-probe accounting
/// adapter, which holds a producer clone of the writer's channel. Releasing the
/// Router only after draining leaves the drain waiting on a handle nothing will
/// use again, turning every shutdown into a multi-second stall. Measured end to
/// end on the shipped binary, from the signal to the process exit.
#[cfg(unix)]
#[test]
fn a_booted_daemon_shuts_down_well_inside_the_writer_abandon_deadline() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    // Arrange: a hermetic daemon, serving.
    let port = free_port();
    let (home, config_path) = hermetic_home(port);
    let mut child = ServeChild::spawn(home.path(), &config_path, port);
    await_serving(&mut child, port);

    // Act: exactly one SIGTERM, to the CHILD's pid. The parent's own signal
    // disposition is never touched, so no sibling test is affected.
    let child_pid = Pid::from_raw(child.id() as i32);
    let signalled_at = Instant::now();
    kill(child_pid, Signal::SIGTERM).expect("SIGTERM the child");
    let status = child.wait_until(EXIT_DEADLINE);
    let elapsed = signalled_at.elapsed();

    // Assert. A child that never exited is killed and reaped by `Drop` on the
    // way out of this panic, so the failure cannot leak a live daemon.
    let status = status.unwrap_or_else(|| {
        panic!("the daemon did not exit within {EXIT_DEADLINE:?} of one SIGTERM")
    });
    assert!(
        status.success(),
        "graceful shutdown must exit cleanly, got {status}; stderr:\n{}",
        child.stderr(),
    );
    assert!(
        elapsed < SHUTDOWN_BUDGET,
        "shutdown took {elapsed:?}: the Router's accounting adapter must be released \
         before the usage writer is drained, or the drain waits out its abandon deadline",
    );
}
