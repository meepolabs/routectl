use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};

use super::{
    GRACE, create_exclusive_dir, mitm_dir_name, random_nonce, reap_in, secret_dir_name,
    usage_dir_name,
};

const NONCE_A: &str = "00000000000000aa";
const NONCE_B: &str = "00000000000000bb";

const OLD: Duration = Duration::from_hours(1);
const YOUNG: Duration = Duration::from_secs(5);

/// Create `root/name` holding one file, with its mtime set `age` in the past.
fn make_dir(root: &Path, name: &str, age: Duration) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("usage-0.db"), b"x").unwrap();
    std::fs::File::open(&dir)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
    dir
}

/// A pid that belonged to a process that has exited and been reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// A live process other than this one; kill it via [`stop`].
fn live_child() -> Child {
    Command::new("sleep").arg("60").spawn().unwrap()
}

fn stop(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

fn usage_name(pid: u32, nonce: &str) -> String {
    format!("routectl-usage-test-{pid}-{nonce}")
}

fn secret_name(pid: u32, nonce: &str) -> String {
    format!("routectl-secret-test-{pid}-{nonce}")
}

fn mitm_name(pid: u32, nonce: &str, tag: &str, n: u32) -> String {
    format!("routectl-mitm-e2e-{pid}-{nonce}-{tag}-{n}")
}

#[test]
fn removes_dead_pid_dirs_older_than_grace() {
    let root = tempfile::tempdir().unwrap();
    let pid = dead_pid();
    let usage = make_dir(root.path(), &usage_name(pid, NONCE_A), OLD);
    let secret = make_dir(root.path(), &secret_name(pid, NONCE_A), OLD);
    let mitm = make_dir(root.path(), &mitm_name(pid, NONCE_A, "tag", 3), OLD);

    reap_in(root.path(), GRACE);

    assert!(!usage.exists(), "dead-pid usage dir must be removed");
    assert!(!secret.exists(), "dead-pid secret dir must be removed");
    assert!(!mitm.exists(), "dead-pid mitm dir must be removed");
}

#[test]
fn keeps_dir_of_live_pid_and_of_own_pid() {
    let root = tempfile::tempdir().unwrap();
    let child = live_child();
    let live = make_dir(root.path(), &usage_name(child.id(), NONCE_A), OLD);
    let live_secret = make_dir(root.path(), &secret_name(child.id(), NONCE_A), OLD);
    let own_secret = make_dir(root.path(), &secret_name(std::process::id(), NONCE_B), OLD);
    let own_usage = make_dir(root.path(), &usage_name(std::process::id(), NONCE_B), OLD);
    let own_mitm = make_dir(
        root.path(),
        &mitm_name(std::process::id(), NONCE_B, "tag", 0),
        OLD,
    );

    reap_in(root.path(), GRACE);
    stop(child);

    assert!(live.exists(), "a live pid's dir must survive");
    assert!(
        live_secret.exists() && own_secret.exists(),
        "a live pid's secret dir must survive"
    );
    assert!(
        own_usage.exists() && own_mitm.exists(),
        "the reaper's own pid is live, so its dirs survive whatever the nonce"
    );
}

#[test]
fn keeps_dead_pid_dir_younger_than_grace() {
    let root = tempfile::tempdir().unwrap();
    let pid = dead_pid();
    let young = make_dir(root.path(), &usage_name(pid, NONCE_A), YOUNG);
    let old = make_dir(root.path(), &mitm_name(pid, NONCE_A, "old", 0), OLD);

    reap_in(root.path(), GRACE);

    assert!(young.exists(), "a dir inside the grace must survive");
    assert!(!old.exists(), "control: the same pid's old dir is reaped");
}

#[test]
fn keeps_legacy_nonce_less_names_of_a_dead_pid() {
    let root = tempfile::tempdir().unwrap();
    let pid = dead_pid();
    let legacy: Vec<PathBuf> = [
        format!("routectl-usage-test-{pid}"),
        format!("routectl-secret-test-{pid}"),
        format!("routectl-mitm-e2e-{pid}-tag-3"),
        format!("routectl-mitm-e2e-{pid}-old-0"),
    ]
    .iter()
    .map(|name| make_dir(root.path(), name, OLD))
    .collect();
    let control = make_dir(root.path(), &usage_name(pid, NONCE_A), OLD);

    reap_in(root.path(), GRACE);

    assert!(
        !control.exists(),
        "control: a nonce-named dead dir is reaped"
    );
    for dir in &legacy {
        assert!(dir.exists(), "legacy {} must be kept", dir.display());
    }
}

#[test]
fn keeps_names_that_do_not_match_strictly() {
    let root = tempfile::tempdir().unwrap();
    let pid = dead_pid();
    let kept: Vec<PathBuf> = [
        "routectl-usage-test-abc".to_string(),
        "routectl-usage-test-123x".to_string(),
        "routectl-usage-test-".to_string(),
        format!("routectl-usage-test-{pid}-extra"),
        format!("routectl-usage-test-{pid}x"),
        format!("routectl-secret-test-{pid}-extra"),
        format!("routectl-secret-test-{pid}x-{NONCE_A}"),
        format!("routectl-mitm-e2e-{pid}"),
        "routectl-mitm-e2e-abc-1".to_string(),
        "routectl-mitm-e2e--1".to_string(),
        format!("routectl-usage-test-{pid}x-{NONCE_A}"),
        format!("routectl-usage-test--{NONCE_A}"),
        format!("routectl-mitm-e2e-{pid}x-{NONCE_A}-tag-1"),
        format!("x-routectl-usage-test-{pid}-{NONCE_A}"),
        "routectl-usage-test-0".to_string(),
        "routectl-usage-test-4294967295".to_string(),
        "routectl-usage-test-99999999999999999999".to_string(),
        "unrelated".to_string(),
    ]
    .iter()
    .map(|name| make_dir(root.path(), name, OLD))
    .collect();
    let file = root.path().join(usage_name(pid, "0000000000000000"));
    std::fs::write(&file, b"x").unwrap();
    let control = make_dir(root.path(), &usage_name(pid, NONCE_A), OLD);

    reap_in(root.path(), GRACE);

    assert!(!control.exists(), "control: a matching dead dir is reaped");
    for dir in &kept {
        assert!(dir.exists(), "{} must be kept", dir.display());
    }
    assert!(file.exists(), "a plain file must be kept");
}

#[test]
fn missing_root_is_a_no_op() {
    let root = tempfile::tempdir().unwrap();
    reap_in(&root.path().join("absent"), GRACE);
}

#[test]
fn reused_pid_with_new_nonce_never_shares_the_reaped_path() {
    let root = tempfile::tempdir().unwrap();
    let pid = dead_pid();
    let stale = make_dir(root.path(), &usage_name(pid, NONCE_A), OLD);
    let recreated = make_dir(root.path(), &usage_name(pid, NONCE_B), YOUNG);

    reap_in(root.path(), GRACE);

    assert!(!stale.exists(), "control: the dead process's dir is reaped");
    assert!(
        recreated.exists(),
        "a reused pid's dir lives at a different path and survives"
    );
}

#[test]
fn produced_names_are_reapable_by_the_reaper() {
    let pid = std::process::id();
    let nonce = random_nonce();
    assert_eq!(nonce.len(), 16);
    assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(nonce, random_nonce(), "each draw is fresh");
    assert_eq!(
        usage_dir_name(&nonce),
        format!("routectl-usage-test-{pid}-{nonce}")
    );
    assert_eq!(
        secret_dir_name(&nonce),
        format!("routectl-secret-test-{pid}-{nonce}")
    );
    assert_eq!(
        mitm_dir_name(&nonce, "tag", 7),
        format!("routectl-mitm-e2e-{pid}-{nonce}-tag-7")
    );

    // Round-trip: rename the produced names onto a dead pid and confirm the
    // reaper's parser accepts the exact shapes the producers emit.
    let root = tempfile::tempdir().unwrap();
    let dead = dead_pid();
    let usage = make_dir(
        root.path(),
        &usage_dir_name(&nonce).replacen(&pid.to_string(), &dead.to_string(), 1),
        OLD,
    );
    let secret = make_dir(
        root.path(),
        &secret_dir_name(&nonce).replacen(&pid.to_string(), &dead.to_string(), 1),
        OLD,
    );
    let mitm = make_dir(
        root.path(),
        &mitm_dir_name(&nonce, "tag", 7).replacen(&pid.to_string(), &dead.to_string(), 1),
        OLD,
    );

    reap_in(root.path(), GRACE);

    assert!(!usage.exists() && !secret.exists() && !mitm.exists());
}

#[test]
fn keeps_malformed_nonce_names() {
    let root = tempfile::tempdir().unwrap();
    let pid = dead_pid();
    let kept: Vec<PathBuf> = [
        usage_name(pid, &"a".repeat(15)),
        usage_name(pid, &"a".repeat(17)),
        usage_name(pid, &"g".repeat(16)),
        format!("{}-extra", usage_name(pid, NONCE_A)),
        mitm_name(pid, &"a".repeat(15), "tag", 1),
        mitm_name(pid, &"g".repeat(16), "tag", 1),
        format!("routectl-mitm-e2e-{pid}-{NONCE_A}-1"),
        format!("routectl-mitm-e2e-{pid}-{NONCE_A}--1"),
        format!("routectl-mitm-e2e-{pid}-{NONCE_A}-tag-x"),
        format!("routectl-mitm-e2e-{pid}-{NONCE_A}-tag-"),
    ]
    .iter()
    .map(|name| make_dir(root.path(), name, OLD))
    .collect();

    reap_in(root.path(), GRACE);

    for dir in &kept {
        assert!(dir.exists(), "{} must be kept", dir.display());
    }
}

#[test]
fn exclusive_create_redraws_the_nonce_when_the_path_exists() {
    let root = tempfile::tempdir().unwrap();
    let taken = root.path().join(usage_dir_name(NONCE_A));
    std::fs::create_dir(&taken).unwrap();
    std::fs::write(taken.join("marker"), b"x").unwrap();
    let mut nonces = [NONCE_A, NONCE_B].into_iter();

    let created = create_exclusive_dir(
        root.path(),
        || nonces.next().unwrap().to_string(),
        usage_dir_name,
    );

    assert_eq!(created, root.path().join(usage_dir_name(NONCE_B)));
    assert!(created.is_dir(), "the redrawn path is freshly created");
    assert_eq!(
        std::fs::read_dir(&created).unwrap().count(),
        0,
        "the new dir is empty, not an adopted one"
    );
    assert!(
        taken.join("marker").exists(),
        "the colliding dir is left untouched"
    );
}

#[test]
fn exclusive_create_without_collision_uses_the_first_nonce() {
    let root = tempfile::tempdir().unwrap();

    let created = create_exclusive_dir(root.path(), || NONCE_A.to_string(), usage_dir_name);

    assert_eq!(created, root.path().join(usage_dir_name(NONCE_A)));
    assert!(created.is_dir());
}

#[test]
#[should_panic(expected = "could not create a unique test temp dir")]
fn exclusive_create_panics_when_every_nonce_collides() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(usage_dir_name(NONCE_A))).unwrap();

    create_exclusive_dir(root.path(), || NONCE_A.to_string(), usage_dir_name);
}

#[test]
#[should_panic(expected = "create test temp dir")]
fn exclusive_create_panics_on_a_non_collision_error() {
    let root = tempfile::tempdir().unwrap();

    create_exclusive_dir(
        &root.path().join("absent"),
        || NONCE_A.to_string(),
        usage_dir_name,
    );
}
