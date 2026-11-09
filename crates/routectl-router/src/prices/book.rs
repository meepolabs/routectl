//! The refreshed price table file, re-read when it changes on disk.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use parking_lot::Mutex;

use super::{MAX_INPUT_BYTES, PriceTable, PriceTableError, parse_and_validate};

/// The refreshed price table at one file path, checked on every read.
///
/// [`PriceBook::current`] stats the file and reparses it only when its
/// modification time or length differs from the state last seen, so a
/// table renamed into place is visible on the next read with no restart.
/// A present file that cannot be read or parsed keeps the last good table
/// and logs one WARN per distinct file state, naming only the retained
/// table's date. An absent file is no table and no WARN.
///
/// The book reads only its own path. It is shared across tasks by
/// reference; no lock is held while the file is read or parsed.
#[derive(Debug)]
pub struct PriceBook {
    path: PathBuf,
    state: Mutex<BookState>,
    parses: AtomicUsize,
}

#[derive(Debug, Default)]
struct BookState {
    seen: Option<FileStamp>,
    table: Option<Arc<PriceTable>>,
}

/// What a read observed about the file; a reparse happens only when it
/// differs from the last recorded stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStamp {
    Present {
        modified: Option<SystemTime>,
        len: u64,
    },
    Unreadable(io::ErrorKind),
}

/// Why a present file yielded no table.
enum Refusal {
    Io(io::ErrorKind),
    Table(PriceTableError),
}

impl PriceBook {
    /// A book over the table file at `path`. Nothing is read until
    /// [`Self::current`].
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: Mutex::new(BookState::default()),
            parses: AtomicUsize::new(0),
        }
    }

    /// The refreshed table: the file's table when it parses, the last good
    /// table when the file is present but refused, `None` when the file is
    /// absent or no good table has been read since.
    pub fn current(&self) -> Option<Arc<PriceTable>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return self.forget(),
            Err(e) => return self.record_refusal(e.kind()),
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(e) => return self.record_refusal(e.kind()),
        };
        let stamp = FileStamp::Present {
            modified: meta.modified().ok(),
            len: meta.len(),
        };
        {
            let state = self.state.lock();
            if state.seen == Some(stamp) {
                return state.table.clone();
            }
        }
        let outcome = self.load(file, meta.len());
        self.record(stamp, outcome)
    }

    /// Reads at most one byte past the input bound, as the refresh does.
    fn load(&self, file: File, len: u64) -> Result<PriceTable, Refusal> {
        self.parses.fetch_add(1, Ordering::Relaxed);
        if len > MAX_INPUT_BYTES as u64 {
            return Err(Refusal::Table(PriceTableError::TooLarge));
        }
        let mut bytes = Vec::new();
        file.take(MAX_INPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Refusal::Io(e.kind()))?;
        parse_and_validate(&bytes).map_err(Refusal::Table)
    }

    fn forget(&self) -> Option<Arc<PriceTable>> {
        let mut state = self.state.lock();
        state.seen = None;
        state.table = None;
        None
    }

    fn record_refusal(&self, kind: io::ErrorKind) -> Option<Arc<PriceTable>> {
        self.record(FileStamp::Unreadable(kind), Err(Refusal::Io(kind)))
    }

    /// Stores what a read of `stamp` produced. A reader that finds `stamp`
    /// already recorded by a concurrent reader takes that result, so each
    /// state warns at most once.
    fn record(
        &self,
        stamp: FileStamp,
        outcome: Result<PriceTable, Refusal>,
    ) -> Option<Arc<PriceTable>> {
        let mut state = self.state.lock();
        if state.seen != Some(stamp) {
            state.seen = Some(stamp);
            match outcome {
                Ok(table) => state.table = Some(Arc::new(table)),
                Err(refusal) => warn_refused(state.table.as_deref(), &refusal),
            }
        }
        state.table.clone()
    }

    #[cfg(test)]
    fn parse_count(&self) -> usize {
        self.parses.load(Ordering::Relaxed)
    }
}

/// The WARN names the retained table by date only: nothing from the file.
fn warn_refused(retained: Option<&PriceTable>, refusal: &Refusal) {
    let retained = retained.map_or_else(|| "none".to_owned(), |t| t.generated_at().to_string());
    match refusal {
        Refusal::Io(kind) => tracing::warn!(
            retained_table = %retained,
            io_error = ?kind,
            "price table file unreadable; keeping the retained table"
        ),
        Refusal::Table(err) => tracing::warn!(
            retained_table = %retained,
            error = %err,
            "price table file refused; keeping the retained table"
        ),
    }
}

#[cfg(test)]
#[path = "book_tests.rs"]
mod tests;
