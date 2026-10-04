use std::path::Path;
use std::time::Duration;

use super::{secret_file_ref, write_owner_only};
use crate::temp_reaper::{create_secret_dir, reap_in};

fn path_of(uri: &str) -> &Path {
    Path::new(uri.strip_prefix("file://").expect("file:// ref"))
}

/// A pid that belonged to a process that has exited and been reaped.
#[cfg(unix)]
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn ref_resolves_to_the_exact_value() {
    let uri = secret_file_ref("  sk-test value\n");

    assert_eq!(
        std::fs::read_to_string(path_of(&uri)).unwrap(),
        "  sk-test value\n"
    );
}

#[test]
fn refs_get_distinct_files_in_one_per_process_dir_under_temp() {
    let a = secret_file_ref("a");
    let b = secret_file_ref("b");

    let (a, b) = (path_of(&a), path_of(&b));
    assert_ne!(a, b);
    assert_eq!(a.parent(), b.parent(), "one dir per process");
    assert_eq!(a.parent().unwrap().parent().unwrap(), std::env::temp_dir());
}

#[cfg(unix)]
#[test]
fn ref_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let uri = secret_file_ref("k");

    let mode = std::fs::metadata(path_of(&uri))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "mode {mode:o} must deny group and other");
}

/// Names the one test a child invocation of this binary was started to run.
#[cfg(unix)]
const UMASK_CHILD_ENV: &str = "ROUTECTL_TESTKIT_UMASK_CHILD";

/// Printed by the child only after its assertion passed, so a child that
/// exits 0 without asserting (or a filter that selects nothing) fails the
/// parent.
#[cfg(unix)]
const UMASK_CHILD_VERIFIED: &str = "umask-child-verified: secret dir is 0700";

/// The secret dir is created owner-only regardless of umask: under a
/// permissive `0002` a plain `create_dir` would yield `0775`. The umask is
/// process-global and libtest runs tests on concurrent threads, so the umask
/// is set only in a child invocation of this binary that runs this test
/// alone; the parent never touches it.
#[cfg(unix)]
#[test]
fn secret_dir_is_owner_only_under_a_permissive_umask() {
    const TEST: &str = "secret_dir_is_owner_only_under_a_permissive_umask";
    if std::env::var_os(UMASK_CHILD_ENV).is_some_and(|named| named == TEST) {
        assert_secret_dir_is_owner_only_under_umask_0002();
        // libtest has already printed `test <name> ... ` without a newline.
        println!("\n{UMASK_CHILD_VERIFIED}");
        return;
    }

    let module = module_path!()
        .split_once("::")
        .map_or("", |(_crate, rest)| rest);
    let program = std::env::current_exe().expect("the running test binary");
    let mut child = std::process::Command::new(&program);
    child
        .args([
            &format!("{module}::{TEST}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(UMASK_CHILD_ENV, TEST);
    let run = run_child_within(child, UMASK_CHILD_DEADLINE)
        .unwrap_or_else(|err| panic!("run child {}: {err}", program.display()));

    let (stdout, stderr) = (&run.stdout, &run.stderr);
    let Some(status) = run.status else {
        panic!(
            "the umask child overran its {UMASK_CHILD_DEADLINE:?} deadline and was killed \
             and reaped\n--- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}"
        );
    };
    assert!(
        status.success() && stdout.lines().any(|line| line == UMASK_CHILD_VERIFIED),
        "the umask child must exit 0 and print its verified marker; status {status}\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}"
    );
}

/// How long the parent waits for the umask child before killing it. Generous
/// against a loaded host: the child runs one filesystem check.
#[cfg(unix)]
const UMASK_CHILD_DEADLINE: Duration = Duration::from_mins(1);

/// How often the parent re-checks whether its child has exited.
#[cfg(unix)]
const CHILD_EXIT_POLL: Duration = Duration::from_millis(5);

/// A finished child run. `status` is `None` when the child overran its
/// deadline and was killed.
#[cfg(unix)]
struct ChildRun {
    status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
}

/// Spawn `child`, wait up to `deadline` from the spawn for it to exit, and
/// kill it if it has not. Returns only after the child has been reaped and
/// both output drains have been joined.
#[cfg(unix)]
fn run_child_within(
    mut child: std::process::Command,
    deadline: Duration,
) -> std::io::Result<ChildRun> {
    use std::process::Stdio;
    use std::time::Instant;

    let mut process = child
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();
    let stdout = drain_pipe(process.stdout.take());
    let stderr = drain_pipe(process.stderr.take());

    let exited = loop {
        match process.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Ok(None) if started.elapsed() >= deadline => break Ok(None),
            Ok(None) => std::thread::sleep(CHILD_EXIT_POLL),
            Err(err) => break Err(err),
        }
    };
    let reaped = if matches!(exited, Ok(Some(_))) {
        Ok(())
    } else {
        // A kill error only means the child has already exited; the wait
        // below reaps it either way.
        let _ = process.kill();
        process.wait().map(drop)
    };
    let (stdout, stderr) = (joined_output(stdout), joined_output(stderr));
    let status = exited?;
    reaped?;
    Ok(ChildRun {
        status,
        stdout,
        stderr,
    })
}

/// Drain one of the child's output pipes to EOF on its own thread, so a child
/// writing more than a pipe buffer never blocks on a parent that is polling.
#[cfg(unix)]
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

#[cfg(unix)]
fn joined_output(drain: std::thread::JoinHandle<String>) -> String {
    drain
        .join()
        .unwrap_or_else(|_| "<child output drain panicked>".to_owned())
}

#[cfg(unix)]
fn assert_secret_dir_is_owner_only_under_umask_0002() {
    use nix::sys::stat::{Mode, umask};
    use std::os::unix::fs::PermissionsExt;

    let previous = umask(Mode::from_bits_truncate(0o002));
    let created = create_secret_dir();
    umask(previous);

    let mode = std::fs::metadata(&created).unwrap().permissions().mode();
    std::fs::remove_dir(&created).unwrap();
    assert_eq!(mode & 0o777, 0o700, "secret dir mode {mode:o} must be 0700");
}

/// The dir a real ref lives in is reaped once renamed onto a dead pid, and
/// survives even a zero-grace pass under a live pid -- another process's or
/// this one's.
#[cfg(unix)]
#[test]
fn produced_secret_dir_is_reaped_only_once_its_owner_is_dead() {
    let uri = secret_file_ref("k");
    let produced = path_of(&uri).parent().unwrap().file_name().unwrap();
    let own_name = produced.to_str().unwrap();
    let own_pid = std::process::id().to_string();
    let mut live_child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let on_pid = |pid: u32| {
        root.path()
            .join(own_name.replacen(&own_pid, &pid.to_string(), 1))
    };
    let (dead, live, own) = (
        on_pid(dead_pid()),
        on_pid(live_child.id()),
        on_pid(std::process::id()),
    );
    for dir in [&dead, &live, &own] {
        std::fs::create_dir(dir).unwrap();
        write_owner_only(&dir.join("secret-0"), "k");
    }

    reap_in(root.path(), Duration::ZERO);
    live_child.kill().unwrap();
    live_child.wait().unwrap();

    assert!(
        !dead.exists(),
        "control: a dead process's secret dir is reaped"
    );
    for dir in [&live, &own] {
        assert_eq!(
            std::fs::read_to_string(dir.join("secret-0")).unwrap(),
            "k",
            "a live process's secret file must survive: {}",
            dir.display()
        );
    }
}
