//! Usage-DB connection open: path handling, WAL, file perms, migrations.
//!
//! `open` creates the parent directory if missing, opens (or creates)
//! the SQLite file in WAL mode, tightens the file mode to 0600 on Unix,
//! and runs the migrate-on-open ladder. It returns a `UsageDb` wrapper
//! that owns the connection; a later writer task takes ownership of it.
//! Normal failures return an `OpenError` so the caller can degrade
//! gracefully rather than panicking.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::migrate::{MigrateError, migrate_to_current};
use crate::schema::SCHEMA_VERSION;

/// Owns an open usage-DB connection. The later writer task takes
/// ownership of this wrapper.
pub struct UsageDb {
    conn: Connection,
    path: PathBuf,
}

impl UsageDb {
    /// Borrow the underlying connection.
    pub const fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The on-disk path this DB was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consume the wrapper and return the owned connection.
    pub fn into_conn(self) -> Connection {
        self.conn
    }
}

/// Errors raised while opening the usage DB. Each is a normal,
/// recoverable failure path -- the caller degrades (disables usage
/// capture) rather than crashing the proxy.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Creating the parent directory failed.
    #[error("failed to create usage db directory {path}: {source}")]
    CreateDir {
        /// Directory path that could not be created.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Opening the SQLite file failed.
    #[error("failed to open usage db {path}: {source}")]
    Open {
        /// Path that could not be opened.
        path: String,
        /// The underlying SQLite error.
        #[source]
        source: rusqlite::Error,
    },

    /// A required PRAGMA (e.g. WAL) did not take effect.
    #[error("failed to configure usage db: {0}")]
    Pragma(#[source] rusqlite::Error),

    /// Tightening the file permissions failed (Unix only).
    #[error("failed to set usage db permissions {path}: {source}")]
    Permissions {
        /// Path whose permissions could not be set.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The migrate-on-open ladder failed.
    #[error("usage db migration failed: {0}")]
    Migrate(#[from] MigrateError),

    /// The read-only viewer found no usage data yet: either the file is
    /// absent or the `requests` table has never been created. The CLI can
    /// turn this into a friendly "no usage data yet" message.
    #[error("no usage data yet at {path}")]
    NoData {
        /// Path that held no usage data.
        path: String,
    },

    /// The on-disk DB reports a schema version newer than this binary
    /// understands. A read-only viewer refuses to guess at a future
    /// layout rather than silently misread it.
    #[error("usage db schema version {found} is newer than supported {supported}")]
    VersionTooNew {
        /// On-disk schema version.
        found: i64,
        /// Highest version this binary understands.
        supported: i64,
    },

    /// The on-disk DB predates this binary (found < supported). A read-only
    /// viewer refuses to query an unmigrated layout; start the service once
    /// to migrate it, then retry.
    #[error(
        "usage db schema version {found} predates this binary (supported {supported}); \
         start the service once to migrate it"
    )]
    VersionTooOld {
        /// On-disk schema version.
        found: i64,
        /// Version this binary requires.
        supported: i64,
    },

    /// A fast-fail read-only open found a journal mode other than WAL. A
    /// viewer must not flip journal modes on a live writer's DB, and a
    /// non-WAL journal means readers and the writer would contend, so the
    /// viewer fails closed rather than risk it.
    #[error("usage db journal mode is '{found}', expected 'wal'")]
    NotWal {
        /// The journal mode actually found.
        found: String,
    },
}

/// Current wall-clock time as epoch milliseconds. Saturates to 0 if the
/// clock is before the epoch (should not happen on a sane host).
fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Create the parent directory of `path` if it does not yet exist.
fn ensure_parent_dir(path: &Path) -> Result<(), OpenError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|source| OpenError::CreateDir {
        path: parent.display().to_string(),
        source,
    })
}

/// Pre-create the DB file with mode 0600 before SQLite opens it, closing
/// the window where `Connection::open` would otherwise create a
/// world-readable file (umask) ahead of the post-open chmod. SQLite then
/// opens the existing 0600 file and the WAL `-wal`/`-shm` sidecars inherit
/// that mode at creation. No-op if the file already exists. No-op on
/// non-Unix platforms.
#[cfg(unix)]
fn precreate_restricted(path: &Path) -> Result<(), OpenError> {
    use std::os::unix::fs::OpenOptionsExt;

    let result = std::fs::OpenOptions::new()
        .mode(0o600)
        .create(true)
        .truncate(false)
        .write(true)
        .open(path);
    match result {
        Ok(_handle) => Ok(()),
        Err(source) => Err(OpenError::Permissions {
            path: path.display().to_string(),
            source,
        }),
    }
}

#[cfg(not(unix))]
fn precreate_restricted(_path: &Path) -> Result<(), OpenError> {
    Ok(())
}

/// Append `suffix` to the file name of `path` (e.g. ".db" -> ".db-wal").
/// Unlike `with_extension`, this preserves the existing extension.
#[cfg(unix)]
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Tighten the DB file mode to 0600 (owner read/write only) and do the
/// same for any existing `-wal`/`-shm` sidecars. The main-file chmod
/// covers the upgrade path for installs whose file predates the
/// pre-create step; the sidecar chmod is belt-and-suspenders. No-op on
/// non-Unix platforms.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), OpenError> {
    use std::os::unix::fs::PermissionsExt;

    let chmod_0600 = |target: &Path| -> Result<(), OpenError> {
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(target, perms).map_err(|source| OpenError::Permissions {
            path: target.display().to_string(),
            source,
        })
    };

    chmod_0600(path)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if sidecar.exists() {
            chmod_0600(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), OpenError> {
    Ok(())
}

/// Open (or create) the usage DB at `path` and bring it to the current
/// schema. Steps: ensure the parent dir, pre-create the file 0600 on
/// Unix, open the file, enable + verify WAL, set a busy timeout, restrict
/// perms (upgrade path + sidecars), then run the migration ladder.
pub fn open(path: impl AsRef<Path>) -> Result<UsageDb, OpenError> {
    let path = path.as_ref().to_path_buf();
    ensure_parent_dir(&path)?;
    precreate_restricted(&path)?;

    let conn = Connection::open(&path).map_err(|source| OpenError::Open {
        path: path.display().to_string(),
        source,
    })?;

    enable_wal(&conn)?;

    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(OpenError::Pragma)?;

    restrict_permissions(&path)?;

    let version = migrate_to_current(&conn, now_epoch_ms())?;
    tracing::info!(
        target: "routectl_usage::db",
        path = %path.display(),
        schema_version = version,
        "opened usage db"
    );

    Ok(UsageDb { conn, path })
}

/// Open the usage DB at `path` for reading only. Unlike `open`, this does
/// NOT create the file, chmod it, switch journal modes, or migrate -- the
/// daemon is the sole writer and may be live; a viewer must not mutate the
/// file out from under it. WAL readers do not block the writer, but a busy
/// timeout is still set for safety. A missing file or a missing `requests`
/// table surfaces as `NoData` so the CLI can print a friendly message; a
/// DB whose schema version does not exactly match this binary surfaces as
/// `VersionTooNew` or `VersionTooOld` rather than being misread or crashing
/// on a missing column.
pub fn open_readonly(path: impl AsRef<Path>) -> Result<UsageDb, OpenError> {
    open_readonly_with_timeout(path, BUSY_TIMEOUT_MS, false)
}

/// Open the usage DB at `path` for reading only with a small busy timeout,
/// for the status surface that polls under load. Like `open_readonly` it
/// does NOT create, chmod, migrate, or switch journal modes -- the daemon
/// is the sole writer and may be live -- and it classifies a missing file /
/// missing `requests` table / schema mismatch identically (`NoData`,
/// `VersionTooNew`, `VersionTooOld`). It differs in two ways: the busy
/// timeout is `FASTFAIL_BUSY_TIMEOUT_MS` (well under the CLI viewer's 5s) so
/// a poll loop sheds to a busy error instead of holding a request open for
/// seconds; and it confirms WAL by READING the journal-mode pragma without
/// mutating it, failing closed with `NotWal` on any other mode rather than
/// risk reader/writer contention on a live writer's DB.
pub fn open_readonly_fastfail(path: impl AsRef<Path>) -> Result<UsageDb, OpenError> {
    open_readonly_with_timeout(path, FASTFAIL_BUSY_TIMEOUT_MS, true)
}

/// Open the usage DB at `path` for READ-WRITE while NEVER creating or
/// migrating it. It sits between [`open`] (which creates, chmods, switches
/// journal modes, and runs the migration ladder) and [`open_readonly`]
/// (which forbids writes): the connection may `INSERT`, but a missing file
/// is still rejected as [`OpenError::NoData`] rather than materializing a
/// second, unmigrated database beside the daemon's.
///
/// The one-shot CLI writer (the capability probe's synchronous capability-
/// event insert) uses this so it attaches to the EXISTING WAL database the
/// daemon created and owns, never forking its own schema. A missing or
/// older-schema DB is a clear error here, never a silent migrate -- the
/// out-of-band-migration incident class.
///
/// The open sequence is a thin sibling of `open_readonly_with_timeout`,
/// reusing `verify_readable_version` and `ensure_requests_table`: reject
/// a missing file as `NoData`, open `SQLITE_OPEN_READ_WRITE` (never `CREATE`),
/// apply the standard `BUSY_TIMEOUT_MS` busy timeout, verify the schema
/// version matches this binary exactly ([`OpenError::VersionTooOld`] /
/// [`OpenError::VersionTooNew`] on any mismatch), treat a missing `requests`
/// table as `NoData`, and confirm WAL by READING the journal-mode pragma
/// (never writing it, so a live daemon's mode is untouched; a non-WAL mode
/// fails closed with [`OpenError::NotWal`]).
pub fn open_rw(path: impl AsRef<Path>) -> Result<UsageDb, OpenError> {
    let path = path.as_ref().to_path_buf();
    if !path.exists() {
        return Err(OpenError::NoData {
            path: path.display().to_string(),
        });
    }

    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(
        |source| OpenError::Open {
            path: path.display().to_string(),
            source,
        },
    )?;

    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(OpenError::Pragma)?;

    verify_readable_version(&conn)?;
    ensure_requests_table(&conn, &path)?;
    verify_wal_readonly(&conn)?;

    Ok(UsageDb { conn, path })
}

/// Shared read-only open sequence behind [`open_readonly`] and
/// [`open_readonly_fastfail`]: reject a missing file as `NoData`, open the
/// file `SQLITE_OPEN_READ_ONLY` (never `CREATE`, so a missing file is still
/// rejected and nothing is created or migrated). It then applies
/// `busy_timeout_ms`, verifies the schema version matches this binary, and
/// treats a missing `requests` table as `NoData`. When `check_wal` is set it
/// additionally confirms WAL by READING the journal-mode pragma (never writing
/// it), failing closed with `NotWal`. Neither path creates, chmods, migrates,
/// or switches journal modes -- the daemon is the sole writer and may be live.
fn open_readonly_with_timeout(
    path: impl AsRef<Path>,
    busy_timeout_ms: u64,
    check_wal: bool,
) -> Result<UsageDb, OpenError> {
    let path = path.as_ref().to_path_buf();
    if !path.exists() {
        return Err(OpenError::NoData {
            path: path.display().to_string(),
        });
    }

    let conn =
        Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            OpenError::Open {
                path: path.display().to_string(),
                source,
            }
        })?;

    conn.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))
        .map_err(OpenError::Pragma)?;

    verify_readable_version(&conn)?;
    ensure_requests_table(&conn, &path)?;
    if check_wal {
        verify_wal_readonly(&conn)?;
    }

    Ok(UsageDb { conn, path })
}

/// Reject a DB whose on-disk `PRAGMA user_version` is out of range for this
/// binary. Too-new means the layout is unknown; too-old means the schema has
/// unmigrated columns that would produce raw SQLite errors on first query.
/// Equal is the only readable state for a non-migrating viewer.
fn verify_readable_version(conn: &Connection) -> Result<(), OpenError> {
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(OpenError::Pragma)?;
    if found > SCHEMA_VERSION {
        return Err(OpenError::VersionTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found < SCHEMA_VERSION {
        return Err(OpenError::VersionTooOld {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Treat a DB that exists but has no `requests` table (a fresh,
/// never-written file) as having no usage data yet. A missing table makes
/// the existence probe return zero rows (`QueryReturnedNoRows`); any other
/// SQLite error (corruption, I/O, lock) is a real failure and must surface
/// as such rather than being masked as "no data".
fn ensure_requests_table(conn: &Connection, path: &Path) -> Result<(), OpenError> {
    match conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='requests'",
        [],
        |_| Ok(()),
    ) {
        Ok(()) => Ok(()),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(OpenError::NoData {
            path: path.display().to_string(),
        }),
        Err(source) => Err(OpenError::Open {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Confirm the connection's journal mode is WAL by READING the pragma,
/// never writing it. Unlike `enable_wal`, which uses a writing pragma, a
/// read-only viewer must not flip journal modes on a live writer's DB; a
/// non-WAL journal risks reader/writer contention, so this fails closed
/// with `NotWal` rather than proceeding.
fn verify_wal_readonly(conn: &Connection) -> Result<(), OpenError> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(OpenError::Pragma)?;
    if mode.eq_ignore_ascii_case("wal") {
        Ok(())
    } else {
        Err(OpenError::NotWal { found: mode })
    }
}

/// SQLite busy timeout for this open path, in milliseconds. The
/// one-writer/N-reader contract must not rely on a crate default.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// SQLite busy timeout for the fast-fail read-only open path, in
/// milliseconds. The status surface polls under load and must shed to a
/// busy error quickly rather than hold a request open for seconds, so this
/// is far smaller than `BUSY_TIMEOUT_MS`.
const FASTFAIL_BUSY_TIMEOUT_MS: u64 = 50;

const _: () = assert!(FASTFAIL_BUSY_TIMEOUT_MS < BUSY_TIMEOUT_MS);

/// Switch the connection to WAL and confirm the mode actually took. On
/// some filesystems the pragma is silently ignored and the journal stays
/// e.g. "delete"; the daemon-writer + direct-CLI-reader concurrency
/// contract depends on WAL, so a non-WAL result is a hard error.
fn enable_wal(conn: &Connection) -> Result<(), OpenError> {
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(OpenError::Pragma)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::Pragma(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!("journal_mode is '{mode}', expected 'wal'")),
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
