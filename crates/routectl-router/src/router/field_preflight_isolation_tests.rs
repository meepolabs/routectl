// Process isolation for tests whose threads may fail to terminate: the test
// re-runs itself as a child invocation of this test binary, and the parent
// enforces the deadline, kills and reaps an overrunning child, and relays the
// child's own output on failure.
//
// An `include!`d FRAGMENT of `field_preflight_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason.

/// Names the test a child invocation was started to run. Set only in the
/// child's environment, so the parent's own tests never see it.
const ISOLATED_CHILD_ENV: &str = "ROUTECTL_ISOLATED_CHILD_TEST";

/// Printed by a child on entry, so the parent can tell a child that ran the
/// test from one whose filter selected nothing (libtest exits 0 on an empty
/// selection).
const ISOLATED_CHILD_ENTERED: &str = "isolated-child-entered";

/// Added to a child's own worst-case run time to form the parent's kill bound:
/// covers exec, test-harness startup and a descheduled child on a loaded host.
const CHILD_PROCESS_SLACK: Duration = Duration::from_secs(10);

/// How often the parent re-checks whether its child has exited.
const CHILD_EXIT_POLL: Duration = Duration::from_millis(5);

/// One child invocation: which program to start and which single test in it
/// to run.
struct IsolatedChild {
    program: std::path::PathBuf,
    test: &'static str,
}

impl IsolatedChild {
    /// Run `test` -- a test function defined in this module -- in a child
    /// invocation of the currently running test binary.
    fn of_this_binary(test: &'static str) -> Self {
        let program = std::env::current_exe()
            .unwrap_or_else(|why| panic!("cannot locate the running test binary: {why}"));
        Self { program, test }
    }

    /// The libtest path of `self.test`: this module's path without the crate
    /// name, which libtest does not include.
    fn libtest_path(&self) -> String {
        let module = module_path!()
            .split_once("::")
            .map_or("", |(_crate, rest)| rest);
        format!("{module}::{}", self.test)
    }
}

/// A child that exited on its own within its bound. Its pipes are drained.
#[derive(Debug)]
struct ChildRun {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
enum ChildFailure {
    Start {
        program: std::path::PathBuf,
        why: std::io::Error,
    },
    Wait(std::io::Error),
    TimedOut {
        pid: u32,
        bound: Duration,
        waited: Duration,
        stdout: String,
        stderr: String,
    },
}

impl std::fmt::Display for ChildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start { program, why } => write!(
                f,
                "isolated child could not be started from {}: {why}",
                program.display()
            ),
            Self::Wait(why) => write!(f, "isolated child could not be waited on: {why}"),
            Self::TimedOut {
                pid,
                bound,
                waited,
                stdout,
                stderr,
            } => write!(
                f,
                "isolated child {pid} overran its {bound:?} bound and was killed and \
                 reaped after {waited:?}\n--- child stdout ---\n{stdout}\n--- child \
                 stderr ---\n{stderr}"
            ),
        }
    }
}

/// Whether this process is a child invocation started to run `test`.
fn entered_as_isolated_child(test: &str) -> bool {
    let entered = std::env::var_os(ISOLATED_CHILD_ENV).is_some_and(|named| named == test);
    if entered {
        println!("{ISOLATED_CHILD_ENTERED} {test}");
    }
    entered
}

/// Drain one of the child's output pipes to EOF on its own thread, so a child
/// writing more than a pipe buffer never blocks on a parent that is polling.
fn drain_pipe(
    pipe: Option<impl std::io::Read + Send + 'static>,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let Some(mut pipe) = pipe else {
            return String::new();
        };
        let mut bytes = Vec::new();
        let unreadable = std::io::Read::read_to_end(&mut pipe, &mut bytes).err();
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if let Some(why) = unreadable {
            text.push_str(&format!("\n<child output unreadable: {why}>"));
        }
        text
    })
}

fn joined_output(drain: std::thread::JoinHandle<String>) -> String {
    drain
        .join()
        .unwrap_or_else(|_| "<child output drain panicked>".to_owned())
}

/// Poll `process` until it exits or `deadline` passes. `None` means still
/// running at the deadline.
fn wait_until(
    process: &mut std::process::Child,
    deadline: Instant,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = process.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(CHILD_EXIT_POLL);
    }
}

/// Start `child`, wait up to `bound` from the spawn for it to exit, and kill
/// and reap it if it has not. Every path that started a child returns only
/// after the child has been reaped and both output drains have been joined.
fn run_isolated_child(child: &IsolatedChild, bound: Duration) -> Result<ChildRun, ChildFailure> {
    let mut process = std::process::Command::new(&child.program)
        .args([
            &child.libtest_path(),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ISOLATED_CHILD_ENV, child.test)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|why| ChildFailure::Start {
            program: child.program.clone(),
            why,
        })?;
    let started = Instant::now();
    let stdout = drain_pipe(process.stdout.take());
    let stderr = drain_pipe(process.stderr.take());

    let exited = wait_until(&mut process, started + bound);
    let overran_or_unwaitable = !matches!(exited, Ok(Some(_)));
    if overran_or_unwaitable {
        // A kill error only means the child has already exited; the wait
        // below reaps it either way.
        let _ = process.kill();
    }
    let reaped = if overran_or_unwaitable {
        process.wait().map(Some)
    } else {
        Ok(None)
    };
    let waited = started.elapsed();
    let stdout = joined_output(stdout);
    let stderr = joined_output(stderr);
    match (exited, reaped) {
        (Ok(Some(status)), _) => Ok(ChildRun {
            status,
            stdout,
            stderr,
        }),
        (Err(why), _) | (_, Err(why)) => Err(ChildFailure::Wait(why)),
        (Ok(None), Ok(_)) => Err(ChildFailure::TimedOut {
            pid: process.id(),
            bound,
            waited,
            stdout,
            stderr,
        }),
    }
}

/// Whether a child that exited on its own actually ran `test` and passed it.
/// A failure carries the child's full output, so its own assertion message
/// reaches the parent's failure.
fn child_verdict(run: &ChildRun, test: &str) -> Result<(), String> {
    let entered = run
        .stdout
        .contains(&format!("{ISOLATED_CHILD_ENTERED} {test}"));
    if entered && run.status.success() {
        return Ok(());
    }
    let why = if entered {
        "failed"
    } else {
        "never entered the test (the child's filter selected nothing)"
    };
    Err(format!(
        "isolated child for {test} {why}: {}\n--- child stdout ---\n{}\n--- child \
         stderr ---\n{}",
        run.status, run.stdout, run.stderr
    ))
}

/// Whether `pid` still names a process, reaped or not: an unreaped zombie
/// keeps its `/proc` entry until its parent waits on it.
#[cfg(target_os = "linux")]
fn process_table_holds(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

const STALLING_CHILD_TEST: &str =
    "an_isolated_child_that_reports_and_then_stalls_is_killed_and_reaped_within_its_bound";

/// Printed by the stalling child's reader once it has reported, just before it
/// blocks for good.
const STALLING_CHILD_REPORTED: &str = "stalling-reader-reported";

/// How long the stalling child is allowed before the parent kills it.
const STALLING_CHILD_BOUND: Duration = Duration::from_secs(3);

/// How late past its bound the kill-and-reap may return and still pass.
const STALLING_CHILD_KILL_SLACK: Duration = Duration::from_secs(2);

#[test]
fn an_isolated_child_that_reports_and_then_stalls_is_killed_and_reaped_within_its_bound() {
    if entered_as_isolated_child(STALLING_CHILD_TEST) {
        let (_never_sent, blocked) = std::sync::mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            println!("{STALLING_CHILD_REPORTED}");
            let _ = blocked.recv();
        });
        let _ = reader.join();
        return;
    }
    let child = IsolatedChild::of_this_binary(STALLING_CHILD_TEST);
    let (sent, verdict) = std::sync::mpsc::channel();
    let started = Instant::now();

    // On its own thread under an outer timeout, so a parent that fails to
    // kill fails this test instead of hanging it.
    std::thread::spawn(move || {
        let _ = sent.send(run_isolated_child(&child, STALLING_CHILD_BOUND));
    });
    let outcome = verdict
        .recv_timeout(STALLING_CHILD_BOUND + STALLING_CHILD_KILL_SLACK)
        .unwrap_or_else(|_| {
            panic!(
                "the parent must kill and reap its child within \
                 {STALLING_CHILD_KILL_SLACK:?} of the {STALLING_CHILD_BOUND:?} bound; \
                 no verdict after {:?}",
                started.elapsed()
            )
        });

    let returned = started.elapsed();
    let Err(ChildFailure::TimedOut { pid, stdout, .. }) = outcome else {
        panic!("a child that never exits must be reported overrun, got {outcome:?}");
    };
    assert!(
        returned >= STALLING_CHILD_BOUND,
        "the child was killed before its bound: {returned:?} of {STALLING_CHILD_BOUND:?}",
    );
    assert!(
        returned <= STALLING_CHILD_BOUND + STALLING_CHILD_KILL_SLACK,
        "kill and reap of child {pid} must return within \
         {STALLING_CHILD_KILL_SLACK:?} of the {STALLING_CHILD_BOUND:?} bound, took \
         {returned:?}",
    );
    assert!(
        stdout.contains(STALLING_CHILD_REPORTED),
        "premise: the child's reader reported before stalling, and its output \
         reached the parent: {stdout}",
    );
    #[cfg(target_os = "linux")]
    assert!(
        !process_table_holds(pid),
        "the killed child {pid} must be reaped, not left in the process table",
    );
}

const PANICKING_CHILD_TEST: &str = "an_isolated_child_assertion_reaches_the_parent_verbatim";

const PANICKING_CHILD_MESSAGE: &str = "planted isolated-child assertion";

#[test]
fn an_isolated_child_assertion_reaches_the_parent_verbatim() {
    assert!(
        !entered_as_isolated_child(PANICKING_CHILD_TEST),
        "{PANICKING_CHILD_MESSAGE}"
    );
    let child = IsolatedChild::of_this_binary(PANICKING_CHILD_TEST);

    let run = run_isolated_child(&child, Duration::from_secs(30))
        .unwrap_or_else(|why| panic!("premise: the child exits on its own: {why}"));

    let why = child_verdict(&run, PANICKING_CHILD_TEST)
        .expect_err("a child whose test panicked must fail the parent");
    assert!(
        why.contains(PANICKING_CHILD_MESSAGE),
        "the parent's failure must carry the child's own panic message: {why}",
    );
}

#[test]
fn an_isolated_child_whose_filter_selects_nothing_is_named_as_never_entered() {
    // Arrange
    let child = IsolatedChild {
        program: std::env::current_exe().expect("the running test binary"),
        test: "no_test_by_this_name",
    };
    let run = run_isolated_child(&child, Duration::from_secs(30))
        .unwrap_or_else(|why| panic!("premise: the child exits on its own: {why}"));

    // Act
    let verdict = child_verdict(&run, child.test);

    // Assert
    assert!(
        run.status.success(),
        "premise: libtest exits 0 on an empty selection, so status alone would pass",
    );
    let why = verdict.expect_err("a child that ran no test must not pass");
    assert!(
        why.contains("never entered"),
        "the failure must name the empty selection: {why}",
    );
}

#[test]
fn an_isolated_child_that_cannot_start_is_named_with_its_program() {
    // Arrange
    let child = IsolatedChild {
        program: std::path::PathBuf::from("/nonexistent/isolated-child-binary"),
        test: STALLING_CHILD_TEST,
    };

    // Act
    let outcome = run_isolated_child(&child, Duration::from_secs(1));

    // Assert
    let Err(failure @ ChildFailure::Start { .. }) = outcome else {
        panic!("an unstartable child must be a start failure, got {outcome:?}");
    };
    let why = failure.to_string();
    assert!(
        why.contains("could not be started") && why.contains("/nonexistent/isolated-child-binary"),
        "the failure must name the start and the program: {why}",
    );
}
