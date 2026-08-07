//! Connection-level read deadline: a SQLite progress handler that interrupts
//! whatever statement is running once an absolute [`Instant`] has passed.
//!
//! The handler is a property of the CONNECTION, not of a statement, so one
//! install bounds every statement a caller runs on that connection until the
//! guard drops. That is what lets a multi-statement read (the `/status/usage`
//! panel) carry a single budget without threading a deadline parameter through
//! each query function's public signature.

use std::time::Instant;

use crate::db::UsageDb;

use super::QueryError;

/// How often (in SQLite VM instructions) the deadline is re-checked. Small
/// enough that a runaway scan is cut short promptly, large enough that the
/// callback is not a measurable share of the query's own work.
const PROGRESS_OPS: i32 = 10_000;

/// An installed read deadline, removed again on every exit path of the scope
/// that holds it -- return, error, and unwind alike.
///
/// While it lives, any statement run on the connection is interrupted once
/// `deadline` has passed, surfacing as [`QueryError::Interrupted`]. Dropping it
/// detaches the handler: the connection outlives most callers, and a stale
/// expired deadline left on it would spuriously interrupt the next statement.
///
/// The deadline can overshoot by up to one callback quantum, and it can only
/// interrupt work SQLite itself is doing -- a caller-side fold between
/// statements runs to completion.
#[must_use = "the deadline lives only as long as this guard: dropping it \
              immediately detaches the handler and leaves the following reads \
              unbounded"]
pub struct DeadlineGuard<'a> {
    conn: &'a rusqlite::Connection,
}

impl<'a> DeadlineGuard<'a> {
    /// Install `deadline` on `db`'s connection, replacing any handler already
    /// on it.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::Sqlite`] when the handler cannot be installed.
    pub fn install(db: &'a UsageDb, deadline: Instant) -> Result<Self, QueryError> {
        let conn = db.conn();
        conn.progress_handler(PROGRESS_OPS, Some(move || Instant::now() > deadline))?;
        Ok(Self { conn })
    }
}

impl Drop for DeadlineGuard<'_> {
    fn drop(&mut self) {
        // A detach failure never masks the read's outcome -- the read is what
        // the caller asked for, and a handler that could not be removed is not
        // the caller's problem to interpret.
        let _ = self
            .conn
            .progress_handler(PROGRESS_OPS, None::<fn() -> bool>);
    }
}
