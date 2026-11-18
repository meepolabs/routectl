//! Connection-level read limit: a SQLite progress handler that interrupts
//! whatever statement is running once an absolute [`Instant`] has passed or a
//! [`CancelFlag`] has been set.
//!
//! The handler is a property of the CONNECTION, not of a statement, so one
//! install bounds every statement a caller runs on that connection until the
//! guard drops. That is what lets a multi-statement read (the `/status/usage`
//! panel) carry a single budget without threading a deadline parameter through
//! each query function's public signature.

use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::db::UsageDb;

use super::QueryError;

/// A one-way cancel signal shared between a read and whoever may abandon it.
///
/// Clones share one flag. A default flag is never set unless [`cancel`]
/// is called on it or a clone of it.
///
/// [`cancel`]: CancelFlag::cancel
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// Ask every read holding this flag (or a clone of it) to stop. Idempotent.
    pub fn cancel(&self) {
        // Relaxed: the flag publishes no other data, it only has to be seen
        // eventually by the reader's next check.
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`CancelFlag::cancel`] has been called on this flag or a clone.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// [`QueryError::Interrupted`] once the flag is set. Cheap enough to call
    /// once per row in a Rust loop over a statement's results.
    pub(super) fn check(&self) -> Result<(), QueryError> {
        if self.is_cancelled() {
            return Err(QueryError::Interrupted);
        }
        Ok(())
    }
}

/// What bounds one read: an absolute deadline and a cancel flag. A bare
/// [`Instant`] converts into a limit whose flag is never set.
#[derive(Debug, Clone)]
pub struct ReadLimit {
    /// The read is interrupted once this instant has passed.
    pub deadline: Instant,
    /// The read is interrupted once this flag is set.
    pub cancel: CancelFlag,
}

impl From<Instant> for ReadLimit {
    fn from(deadline: Instant) -> Self {
        Self {
            deadline,
            cancel: CancelFlag::default(),
        }
    }
}

/// How often (in SQLite VM instructions) the limit is re-checked. Small
/// enough that a runaway scan is cut short promptly, large enough that the
/// callback is not a measurable share of the query's own work.
const PROGRESS_OPS: i32 = 10_000;

/// An installed read limit, removed again on every exit path of the scope
/// that holds it -- return, error, and unwind alike.
///
/// While it lives, any statement run on the connection is interrupted once
/// the limit's deadline has passed or its flag is set, surfacing as
/// [`QueryError::Interrupted`]. Dropping it detaches the handler: the
/// connection outlives most callers, and a stale expired limit left on it
/// would spuriously interrupt the next statement.
///
/// Either condition can overshoot by up to one callback quantum, and the
/// handler can only interrupt work SQLite itself is doing -- a caller-side
/// fold between statements runs to completion unless it checks the flag
/// itself.
///
/// The guard holds the only borrow of the [`UsageDb`] for its lifetime and
/// dereferences to it, so the reads it bounds run through the guard.
#[must_use = "the deadline lives only as long as this guard: dropping it \
              immediately detaches the handler and leaves the following reads \
              unbounded"]
pub struct DeadlineGuard<'a> {
    db: &'a mut UsageDb,
}

impl<'a> DeadlineGuard<'a> {
    /// Install `limit` (a [`ReadLimit`], or a bare deadline [`Instant`]) on
    /// `db`'s connection, replacing any handler already on it.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::Sqlite`] when the handler cannot be installed.
    pub fn install(db: &'a mut UsageDb, limit: impl Into<ReadLimit>) -> Result<Self, QueryError> {
        let ReadLimit { deadline, cancel } = limit.into();
        db.conn_mut().progress_handler(
            PROGRESS_OPS,
            Some(move || cancel.is_cancelled() || Instant::now() > deadline),
        )?;
        Ok(Self { db })
    }
}

impl Deref for DeadlineGuard<'_> {
    type Target = UsageDb;

    fn deref(&self) -> &UsageDb {
        self.db
    }
}

impl Drop for DeadlineGuard<'_> {
    fn drop(&mut self) {
        // A detach failure never masks the read's outcome -- the read is what
        // the caller asked for, and a handler that could not be removed is not
        // the caller's problem to interpret.
        let _ = self
            .db
            .conn_mut()
            .progress_handler(PROGRESS_OPS, None::<fn() -> bool>);
    }
}
