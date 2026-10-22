//! A test build's scoped opt-in of one row into the same-field re-repair
//! predicate, so the same-field path can be exercised without a production
//! row that takes it.

use std::cell::Cell;

use super::FieldRepairRow;

thread_local! {
    static OPTED_IN: Cell<Option<&'static str>> = const { Cell::new(None) };
}

pub(super) fn covers(row: &FieldRepairRow) -> bool {
    OPTED_IN.with(Cell::get) == Some(row.path)
}

/// Make the row at `path` re-repair its field on this thread until the
/// returned guard drops.
pub(in crate::router) fn opt_in(path: &'static str) -> OptIn {
    OPTED_IN.with(|cell| cell.set(Some(path)));
    OptIn(())
}

/// Clears the opt-in on drop.
pub(in crate::router) struct OptIn(());

impl Drop for OptIn {
    fn drop(&mut self) {
        OPTED_IN.with(|cell| cell.set(None));
    }
}
