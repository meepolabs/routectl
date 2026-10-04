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
    let output = std::process::Command::new(&program)
        .args([
            &format!("{module}::{TEST}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(UMASK_CHILD_ENV, TEST)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|err| panic!("run child {}: {err}", program.display()));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.lines().any(|line| line == UMASK_CHILD_VERIFIED),
        "the umask child must exit 0 and print its verified marker; status {}\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status
    );
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
