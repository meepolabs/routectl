//! A one-shot child process that must exit on its own within a deadline.
//!
//! Included by `#[path]` rather than through `common/mod.rs`, so a binary
//! that spawns the real CLI does not also compile the shared harness.
//!
//! Output is drained on reader threads while the child runs: a child that
//! fills a pipe buffer would otherwise block on its write and read as hung.
//! On the deadline the captured bytes are read from the shared buffers
//! rather than by joining the readers, since a descendant of the killed
//! child can keep a pipe open.

#![allow(dead_code)]

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Give-up bound for a one-shot command. A command that is still running
/// at this point fails its test.
pub const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// Pause between polls of a child's state.
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run `command` with piped output and return it once the child exits.
///
/// Panics with the captured stdout and stderr when the child is still
/// running after [`EXIT_DEADLINE`]; the child is killed and reaped first.
pub fn run_to_exit(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("spawn {}: {err}", describe(command)));
    let stdout = Captured::drain(child.stdout.take());
    let stderr = Captured::drain(child.stderr.take());

    if let Some(status) = wait_until(&mut child, Instant::now() + EXIT_DEADLINE) {
        return Output {
            status,
            stdout: stdout.finish(),
            stderr: stderr.finish(),
        };
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "{} did not exit within {EXIT_DEADLINE:?}\nstdout: {}\nstderr: {}",
        describe(command),
        String::from_utf8_lossy(&stdout.so_far()),
        String::from_utf8_lossy(&stderr.so_far()),
    );
}

/// The child's exit status if it exits before `until`, else `None`.
fn wait_until(child: &mut Child, until: Instant) -> Option<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return Some(status);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One output pipe, read to its end on its own thread.
struct Captured {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
}

impl Captured {
    fn drain(pipe: Option<impl Read + Send + 'static>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let reader = std::thread::spawn(move || {
            let Some(mut pipe) = pipe else { return };
            let mut chunk = [0u8; 8192];
            while let Ok(read @ 1..) = pipe.read(&mut chunk) {
                sink.lock()
                    .expect("capture lock")
                    .extend_from_slice(&chunk[..read]);
            }
        });
        Self { bytes, reader }
    }

    /// Everything the pipe carried, once the child has exited.
    fn finish(self) -> Vec<u8> {
        let Self { bytes, reader } = self;
        reader.join().expect("pipe reader");
        std::mem::take(&mut *bytes.lock().expect("capture lock"))
    }

    fn so_far(&self) -> Vec<u8> {
        self.bytes.lock().expect("capture lock").clone()
    }
}

fn describe(command: &Command) -> String {
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy())
        .collect();
    format!(
        "`{} {}`",
        command.get_program().to_string_lossy(),
        args.join(" ")
    )
}
