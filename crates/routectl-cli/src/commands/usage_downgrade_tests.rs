use tempfile::TempDir;

use super::*;

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .expect("reopen")
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version")
}

#[test]
fn downgrades_a_fresh_db_to_each_target_and_names_the_path() {
    for target in [17_i64, 16] {
        // Arrange: a fresh file at the current version.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("usage.db");
        drop(routectl_usage::open(&path).expect("fresh db"));

        // Act
        let message = run(&path, target).expect("downgrade");

        // Assert
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains(&format!("v{target}")), "{message}");
        assert_eq!(user_version(&path), target);
    }
}

#[test]
fn refuses_while_the_daemon_writer_holds_the_db() {
    // Arrange: a running usage writer -- the connection the daemon holds for
    // its whole lifetime.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let (handle, writer) = routectl_usage::UsageWriter::start(path.clone(), 8, 0, true);
    let opened = std::time::Instant::now();
    while !path.exists() && opened.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::yield_now();
    }
    // The writer opens and migrates on its own thread; a commit through it
    // proves its connection is live before the downgrade runs.
    assert!(matches!(
        handle.commit_capability_events_blocking(
            vec![routectl_usage::CapabilityEvent::tombstone(1, 1, 0)],
            1
        ),
        routectl_usage::BatchCommit::Committed { .. }
    ));

    // Act
    let result = run(&path, 17);

    // Assert
    assert!(matches!(result, Err(DowngradeError::InUse)), "{result:?}");
    drop(handle);
    writer.shutdown();
    assert_eq!(user_version(&path), routectl_usage::SCHEMA_VERSION);
    let message = run(&path, 16).expect("downgrade succeeds once the writer stopped");
    assert!(message.contains("v16"));
}
