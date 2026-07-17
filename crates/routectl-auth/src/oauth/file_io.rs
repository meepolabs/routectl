//! Atomic load and save of `~/.config/routectl/credentials.json`.
//!
//! Read flow: open + fstat the open fd (TOCTOU-safe) + permissions
//! check + JSON parse. Returns `OAuthError::CorruptedFile` on parse
//! failure so the operator can delete the file and re-login. Returns
//! `Ok(CredentialsFile::empty())` on `NotFound` so first-run servers
//! do not have to special-case the missing file.
//!
//! Write flow: serialize to JSON and hand the bytes to the crate's
//! shared `0o600` atomic writer ([`crate::atomic_write`]): temp file in
//! the SAME directory (created `0o600` before any bytes are written, so a
//! partial write never has wider perms), `fsync` the temp file, atomic
//! `rename` over the target, then `fsync` the parent directory so the
//! rename survives a crash. The old file remains valid if the rename
//! fails.
//!
//! # Cross-process read-modify-write ([`update_under_lock`])
//!
//! The daemon and the `routectl login`/`refresh`/`logout` CLI processes
//! all mutate the SAME `credentials.json`. A naive "clone the in-memory
//! cache, upsert one seat, atomic-rename the whole file" write erases any
//! seat a sibling process wrote since the cache was last loaded. A cross-
//! process advisory lock alone does not fix this: a stale-clone write
//! still clobbers. [`update_under_lock`] closes the gap by RE-READING the
//! file from disk UNDER the lock and merging the caller's one-seat change
//! onto that disk-fresh state before writing.
//!
//! Lock ordering (see [`update_under_lock`]; never violated): per-seat
//! async refresh mutex -> network I/O (the refresh POST, OUTSIDE all file
//! locks) -> in-process `RwLock` write guard (owned by `OAuthStore`) ->
//! cross-process advisory flock (owned here). Every credential mutation
//! acquires them in that order.
//!
//! Non-goal (settled): the merge targets a LOCAL-DISK SINGLE-HOST
//! deployment -- the daemon and the CLI on one box are the only concurrent
//! writers. There is deliberately no network-filesystem-safe merge layer
//! (NFS/SMB advisory locking is not relied upon and is out of scope).
//!
//! Crash recovery needs no stale-lock protocol: the flock is kernel-owned
//! and released automatically on process death, and the atomic rename
//! leaves the previous file intact until the new one is fully written, so
//! a crash mid-mutation loses at most the in-flight change, never the
//! prior credentials.

use std::path::{Path, PathBuf};

use crate::oauth::types::{CredentialsFile, SCHEMA_VERSION};
use crate::oauth::{OAuthError, OAuthResult};

/// Default path: `$XDG_CONFIG_HOME/routectl/credentials.json`, falling
/// back to `~/.config/routectl/credentials.json`. Returns an error if
/// neither `XDG_CONFIG_HOME` nor `HOME` is set (rare; unset HOME on
/// Linux indicates a broken environment).
pub fn default_path() -> OAuthResult<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("routectl").join("credentials.json"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| OAuthError::Internal("neither XDG_CONFIG_HOME nor HOME is set".into()))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("routectl")
        .join("credentials.json"))
}

/// Load the credentials file. NotFound -> empty file (first-run).
/// Schema-version mismatch -> `OAuthError::SchemaMismatch` with
/// operator guidance (re-login is the only supported "migration").
///
/// Mirrors `MemoryStore::read_secret_file` defenses: open the file
/// once, `fstat` the open fd, refuse non-regular-file inodes, refuse
/// permissions wider than `0o600` on Unix. The file holds long-lived
/// refresh tokens; the same hygiene as `file://` secrets applies.
pub async fn load(path: &Path) -> OAuthResult<CredentialsFile> {
    let path_owned = path.to_path_buf();

    tokio::task::spawn_blocking(move || load_blocking(&path_owned))
        .await
        .map_err(|e| OAuthError::Internal(format!("load task failed: {e}")))?
}

/// Synchronous load + validate + parse + schema-check. The blocking core
/// shared by [`load`] (via `spawn_blocking`) and by the re-read step of
/// [`update_under_lock`] (already inside a blocking task, under the lock).
/// NotFound -> empty file (first-run). A corrupt or wrong-schema file is
/// an error: callers that mutate MUST NOT overwrite a file they could not
/// read.
fn load_blocking(path: &Path) -> OAuthResult<CredentialsFile> {
    let display = path.display().to_string();

    let bytes = match read_validated_blocking(path)? {
        Some(b) => b,
        None => return Ok(CredentialsFile::empty()),
    };

    let parsed: CredentialsFile =
        serde_json::from_slice(&bytes).map_err(|e| OAuthError::CorruptedFile {
            path: display.clone(),
            detail: e.to_string(),
        })?;

    if parsed.schema_version != SCHEMA_VERSION {
        return Err(OAuthError::SchemaMismatch {
            found: parsed.schema_version,
            expected: SCHEMA_VERSION,
            path: display,
        });
    }
    Ok(parsed)
}

/// Read the credentials file with TOCTOU-safe defenses.
/// Returns `Ok(None)` on `NotFound` so the caller can lift the
/// first-run case into a fresh empty file.
fn read_validated_blocking(path: &Path) -> OAuthResult<Option<Vec<u8>>> {
    use std::io::Read;

    let display = path.display().to_string();
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(OAuthError::Io(format!("open {display}: {e}"))),
    };

    let meta = file
        .metadata()
        .map_err(|e| OAuthError::Io(format!("fstat {display}: {e}")))?;

    if !meta.is_file() {
        return Err(OAuthError::Io(format!(
            "credentials file {display} is not a regular file (refusing symlink to special file or directory)",
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(OAuthError::Io(format!(
                "credentials file {} has permissions {:o}; \
                 use chmod 600 to restrict reads to the owner",
                display,
                mode & 0o7777
            )));
        }
    }

    // Cap the read at 1 MiB. Real credentials.json files are
    // bytes-to-kilobytes; a misconfigured or attacker-replaced inode
    // pointing at /dev/zero (via TOCTOU symlink swap that got past the
    // regular-file check) would otherwise drive the loader toward OOM.
    const MAX_CREDENTIALS_BYTES: u64 = 1 << 20;
    let len = meta.len();
    if len > MAX_CREDENTIALS_BYTES {
        return Err(OAuthError::Io(format!(
            "credentials file {display} is {len} bytes; refusing to load (cap is {MAX_CREDENTIALS_BYTES} bytes)",
        )));
    }

    let mut buf = Vec::with_capacity(len as usize);
    (&file)
        .take(MAX_CREDENTIALS_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| OAuthError::Io(format!("read {display}: {e}")))?;
    Ok(Some(buf))
}

/// Whether a mutation applied to the disk-fresh state should be persisted.
/// Returned by the closure passed to [`update_under_lock`].
pub enum WriteDirective {
    /// Persist the mutated state via the `0o600` atomic writer.
    Write,
    /// Leave the file byte-identical: the mutation was a no-op against the
    /// disk-fresh state (nothing to remove, or a sibling made the change
    /// moot). This is how the remove-absent and refresh-no-resurrect cases
    /// avoid re-adding a seat or forcing a needless write.
    Skip,
}

/// What a [`update_under_lock`] mutation closure hands back: a persist
/// directive plus the caller's own outcome value (e.g. "was the seat
/// present in the disk-fresh state?"). A struct rather than borrowed
/// `&mut` locals so nothing crosses the `spawn_blocking` boundary by
/// reference.
pub struct Mutation<R> {
    pub directive: WriteDirective,
    pub report: R,
}

/// Log a wait on the cross-process credentials lock once it exceeds this
/// threshold. The critical section is commit-only and short, so a wait
/// beyond a handful of tens of milliseconds is worth a diagnostic line
/// (another process holding the lock, or an overloaded box).
const LOCK_WAIT_LOG_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(50);

/// Apply a single-seat mutation to `credentials.json` under the sibling
/// advisory lock, RE-READING the disk-fresh state under the lock before
/// mutating so a stale in-memory cache never clobbers a seat a sibling
/// process wrote concurrently.
///
/// One `spawn_blocking` performs, in order:
///   1. Ensure the parent directory exists (`0o700`) so the sibling lock
///      file can be created on a first-ever write.
///   2. Open the sibling lock file and acquire its advisory WRITE guard.
///      Contention policy: BLOCK, no timeout. Correctness boundary -- the
///      critical section is commit-only and short, so a bounded wait is
///      preferable to a spurious failure. A wait past
///      [`LOCK_WAIT_LOG_THRESHOLD`] is logged.
///   3. Re-read the file from disk applying the SAME validation as
///      [`load`] (TOCTOU-safe open + perms + parse + schema check; empty
///      on NotFound). A corrupt / wrong-schema disk-fresh state ABORTS the
///      update (the error propagates) rather than overwriting -- never
///      clobber a file we could not read.
///   4. Run `mutate` against the disk-fresh [`CredentialsFile`]. On
///      [`WriteDirective::Write`], serialize and `write_0600_atomic`; on
///      [`WriteDirective::Skip`], write nothing.
///   5. Return the (possibly mutated) disk-fresh file plus the closure's
///      report. The caller commits the returned file to its in-memory
///      cache -- never a stale clone. The lock releases on drop.
///
/// # Lock ordering (never violated)
///
/// per-seat async refresh mutex -> network I/O (refresh POST, OUTSIDE all
/// file locks) -> in-process `RwLock` write guard (held by the `OAuthStore`
/// caller across this call) -> the cross-process flock acquired here. The
/// lock is held for the re-read + commit ONLY; no network I/O or
/// interactive prompt ever runs under it.
///
/// # On-disk naming convention (permanent)
///
/// The lock file is the target path with `.lock` appended
/// (`credentials.json` -> `credentials.json.lock`), the SAME sibling-file
/// convention `routectl-router`'s `config.toml` writer uses. This is a
/// permanent on-disk contract: changing it would let an old build and a
/// new build take different lock files and write the same credentials file
/// unserialized. Never change it without a migration story.
///
/// # Non-goal
///
/// Local-disk single-host only (see the module doc). No network-filesystem
/// merge layer.
pub async fn update_under_lock<F, R>(path: &Path, mutate: F) -> OAuthResult<(CredentialsFile, R)>
where
    F: FnOnce(&mut CredentialsFile) -> Mutation<R> + Send + 'static,
    R: Send + 'static,
{
    let path_owned = path.to_path_buf();

    tokio::task::spawn_blocking(move || update_under_lock_blocking(&path_owned, mutate))
        .await
        .map_err(|e| OAuthError::Internal(format!("update task failed: {e}")))?
}

/// Blocking core of [`update_under_lock`]: parent-dir ensure, flock
/// acquire, re-read, mutate, conditional atomic write. Runs entirely
/// inside one `spawn_blocking` so the advisory lock (an OS fd guard) is
/// held on a single thread for its whole lifetime.
fn update_under_lock_blocking<F, R>(path: &Path, mutate: F) -> OAuthResult<(CredentialsFile, R)>
where
    F: FnOnce(&mut CredentialsFile) -> Mutation<R>,
{
    let display = path.display().to_string();

    // Step 1: the sibling lock file lives next to the credentials file, so
    // the parent directory must exist before we can create it -- the very
    // first login writes into a not-yet-created config dir. The Write path
    // below repeats this ensure inside write_0600_atomic by design: the
    // ensure is idempotent, and the Skip path (no write) still needs the
    // parent here for the lock file, so this call is not redundant.
    let parent = path
        .parent()
        .ok_or_else(|| OAuthError::Io(format!("path has no parent directory: {display}")))?;
    crate::atomic_write::ensure_dir_0700(parent).map_err(OAuthError::Io)?;

    // Step 2: acquire the cross-process advisory write lock (block, no
    // timeout). Do not name the path in the log -- it embeds the operator's
    // home directory.
    let lock_path = lock_path_for(path);
    let lock_file = open_lock_file(&lock_path)
        .map_err(|e| OAuthError::Io(format!("open credentials lock file: {e}")))?;
    let mut file_lock = fd_lock::RwLock::new(lock_file);
    let wait_start = std::time::Instant::now();
    let _guard = file_lock
        .write()
        .map_err(|e| OAuthError::Io(format!("acquire credentials write lock: {e}")))?;
    let waited = wait_start.elapsed();
    if waited >= LOCK_WAIT_LOG_THRESHOLD {
        tracing::debug!(
            waited_ms = waited.as_millis() as u64,
            "waited on cross-process credentials write lock"
        );
    }

    // Step 3: re-read the disk-fresh state under the lock. A corrupt /
    // wrong-schema file aborts rather than being overwritten.
    let mut fresh = load_blocking(path)?;

    // Step 4: apply the one-seat mutation and conditionally persist.
    let Mutation { directive, report } = mutate(&mut fresh);
    match directive {
        WriteDirective::Write => {
            let json = serde_json::to_vec_pretty(&fresh)
                .map_err(|e| OAuthError::Internal(format!("serialize: {e}")))?;
            crate::atomic_write::write_0600_atomic(path, &json).map_err(OAuthError::Io)?;
        }
        WriteDirective::Skip => {}
    }

    // Step 5: hand the merged disk-fresh file back so the caller replaces
    // its in-memory cache with it (never a stale clone). Lock releases when
    // `_guard` drops at end of scope.
    Ok((fresh, report))
}

/// Path to the sibling advisory-lock file for `path`, e.g.
/// `credentials.json` -> `credentials.json.lock`. The lock file's own
/// contents are never read or written -- it exists purely as a kernel lock
/// handle, so a stale empty lock file left behind by an old build is
/// harmless. See [`update_under_lock`] for why this naming is a permanent
/// on-disk convention.
fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Open (creating if absent) the sibling lock file for [`fd_lock::RwLock`]
/// to hold. Read+write access, never truncated -- the file's contents are
/// unused.
fn open_lock_file(lock_path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord};

    fn rec() -> TokenRecord {
        TokenRecord {
            access_token: SecretToken::new("tok"),
            refresh_token: SecretToken::new("rtok"),
            token_type: "Bearer".into(),
            expires_at_unix: 1_900_000_000,
            scopes: vec!["user:inference".into()],
            account: AccountInfo {
                email: Some("u@example.com".into()),
                account_id: None,
            },
            obtained_at_unix: 1_899_000_000,
            session_id: None,
            cloud_project_id: None,
        }
    }

    /// Write `cf` to `path` through the live locked write path, replacing
    /// whatever disk-fresh state is present. Test convenience mirroring the
    /// production single-seat writers, which all commit through
    /// `update_under_lock`.
    async fn write_all(path: &Path, cf: &CredentialsFile) {
        let cf = cf.clone();
        update_under_lock(path, move |disk| {
            *disk = cf;
            Mutation {
                directive: WriteDirective::Write,
                report: (),
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn load_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let cf = load(&path).await.unwrap();
        assert_eq!(cf.schema_version, SCHEMA_VERSION);
        assert!(cf.providers.is_empty());
    }

    #[tokio::test]
    async fn write_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut cf = CredentialsFile::empty();
        cf.upsert("anthropic", rec());
        write_all(&path, &cf).await;

        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(
            loaded.providers["anthropic"].access_token.expose(),
            cf.providers["anthropic"].access_token.expose()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut cf = CredentialsFile::empty();
        cf.upsert("anthropic", rec());
        write_all(&path, &cf).await;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "credentials.json must be 0600, got {:o}",
            mode & 0o777
        );
    }

    #[tokio::test]
    async fn write_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c.json");
        let cf = CredentialsFile::empty();
        write_all(&nested, &cf).await;
        assert!(nested.exists());
    }

    #[tokio::test]
    async fn load_corrupted_returns_corrupted_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tokio::fs::write(&path, b"not json").await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = load(&path).await.unwrap_err();
        match err {
            OAuthError::CorruptedFile { .. } => {}
            other => panic!("expected CorruptedFile, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_wrong_schema_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tokio::fs::write(&path, br#"{"schema_version":99,"providers":{}}"#)
            .await
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = load(&path).await.unwrap_err();
        match err {
            OAuthError::SchemaMismatch {
                found, expected, ..
            } => {
                assert_eq!(found, 99);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        let mut cf = CredentialsFile::empty();
        cf.upsert("anthropic", rec());
        write_all(&path, &cf).await;

        let mut cf2 = CredentialsFile::empty();
        let mut r2 = rec();
        r2.access_token = SecretToken::new("tok-NEW");
        cf2.upsert("anthropic", r2);
        write_all(&path, &cf2).await;

        let loaded = load(&path).await.unwrap();
        assert_eq!(
            loaded.providers["anthropic"].access_token.expose(),
            "tok-NEW"
        );

        // No `.tmp.` files should be left behind.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "atomic write left tempfiles: {leftover:?}"
        );
    }

    /// The core RMW guarantee: `update_under_lock` hands the mutation
    /// closure the DISK-FRESH state, not the caller's snapshot, so a
    /// single-seat upsert merges onto -- rather than clobbers -- a seat
    /// written to disk since the caller last read it.
    #[tokio::test]
    async fn update_under_lock_merges_onto_disk_fresh_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        // Seed seat A on disk.
        update_under_lock(&path, |cf| {
            cf.upsert("anthropic", rec());
            Mutation {
                directive: WriteDirective::Write,
                report: (),
            }
        })
        .await
        .unwrap();

        // A caller that never read seat A upserts only seat B. Because the
        // closure receives the disk-fresh state, A survives.
        let (merged, ()) = update_under_lock(&path, |cf| {
            cf.upsert("codex", rec());
            Mutation {
                directive: WriteDirective::Write,
                report: (),
            }
        })
        .await
        .unwrap();

        assert!(
            merged.get("anthropic").is_some(),
            "seat A must be merged in"
        );
        assert!(merged.get("codex").is_some(), "seat B must be added");
        let loaded = load(&path).await.unwrap();
        assert_eq!(loaded.providers.len(), 2, "both seats persisted");
    }

    /// A corrupt disk-fresh state ABORTS the update rather than being
    /// overwritten -- never clobber a file we could not read.
    #[tokio::test]
    async fn update_under_lock_aborts_on_corrupt_disk_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, b"<<corrupt-json>>").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let err = update_under_lock(&path, |cf| {
            cf.upsert("anthropic", rec());
            Mutation {
                directive: WriteDirective::Write,
                report: (),
            }
        })
        .await
        .unwrap_err();

        match err {
            OAuthError::CorruptedFile { .. } => {}
            other => panic!("expected CorruptedFile, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "an unreadable file must not be overwritten"
        );
    }

    /// A `Skip` directive persists nothing and carries the closure's
    /// report back out; the returned file is the unmodified disk-fresh
    /// state.
    #[tokio::test]
    async fn update_under_lock_skip_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut seeded = CredentialsFile::empty();
        seeded.upsert("anthropic", rec());
        write_all(&path, &seeded).await;
        let before = std::fs::read(&path).unwrap();

        let (merged, report) = update_under_lock(&path, |_cf| Mutation {
            directive: WriteDirective::Skip,
            report: 7_u32,
        })
        .await
        .unwrap();

        assert_eq!(report, 7, "closure report must round-trip");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "Skip must leave the file byte-identical"
        );
        assert_eq!(
            merged.providers.len(),
            1,
            "returned file is the disk-fresh state"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_refuses_world_readable_credentials_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        tokio::fs::write(&path, br#"{"schema_version":1,"providers":{}}"#)
            .await
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load(&path).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("permissions"), "got: {msg}");
        assert!(msg.contains("chmod 600"), "got: {msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_refuses_directory_at_credentials_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::create_dir(&path).unwrap();
        let err = load(&path).await.unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "got: {err}");
    }

    #[test]
    #[serial_test::serial]
    fn default_path_uses_xdg_config_home() {
        // Save & restore to be polite to other tests that rely on env.
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/x/y") };
        let p = default_path().unwrap();
        assert_eq!(p, PathBuf::from("/x/y/routectl/credentials.json"));
        match prev_xdg {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }
}
