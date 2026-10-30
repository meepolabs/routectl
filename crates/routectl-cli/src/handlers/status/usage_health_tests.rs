//! Coverage for the writer-health read the status surface derives from the usage
//! writer's shared counters.

use super::*;

use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

/// A writer that has FAILED a write reports degraded.
///
/// Caused by making the writer open a database whose parent path is a regular
/// file -- the mechanism a real persistent failure takes. No forged counter: the
/// writer bumps it itself; the test controls only the filesystem layout.
///
/// Mutation check: return `false` from `writer_degraded` -> red here.
#[test]
fn a_writer_that_failed_a_write_reports_degraded() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("plant the blocker");
    let broken = blocker.join("usage.db");
    let (handle, writer) = UsageWriter::start(broken, CHANNEL_CAPACITY, 0, true);
    // The open runs on the writer's spawned thread; a busy schedule can push the
    // failure past 50ms.
    std::thread::sleep(std::time::Duration::from_millis(200));

    let view = UsageHealthView::new(std::sync::Arc::clone(handle.counters()));

    assert!(
        view.writer_degraded(),
        "a writer that could not open its database is degraded from the start",
    );
    assert!(
        handle.counters().write_errors() > 0,
        "and the write-error counter is nonzero, which is the signal the derivation \
         reads",
    );
    drop(handle);
    writer.shutdown();
}

/// A writer over a healthy database reports not degraded.
///
/// The paired control for the case above: without it, a derivation that always
/// answered degraded would pass.
///
/// Mutation check: return `true` from `writer_degraded` -> red here.
#[test]
fn a_healthy_writer_reports_not_degraded() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    drop(routectl_usage::open(&path).expect("migrating open"));
    let (handle, writer) = UsageWriter::start(path, CHANNEL_CAPACITY, 0, true);

    let view = UsageHealthView::new(std::sync::Arc::clone(handle.counters()));

    assert!(
        !view.writer_degraded(),
        "a writer with no failed write is not degraded",
    );
    drop(handle);
    writer.shutdown();
}

/// SQL-shaped needles a status source must not carry: any of them means a
/// panel is reading the ledger with its own statement instead of the
/// routectl-usage readers.
const LEDGER_SQL_NEEDLES: [&str; 7] = [
    concat!(".con", "n()"),
    concat!("query_", "row("),
    concat!(".prep", "are("),
    concat!(".exec", "ute("),
    concat!("SEL", "ECT "),
    concat!("FROM ", "requests"),
    concat!("FROM ", "ledger_gaps"),
];

/// The needles present in the production part of `src`.
fn ledger_sql_in(src: &str) -> Vec<&'static str> {
    let production = crate::handlers::status::production_source::production_source(src);
    LEDGER_SQL_NEEDLES
        .into_iter()
        .filter(|needle| production.contains(needle))
        .collect()
}

/// `/status/usage` and `/status/doctor` read the window's observation totals
/// through the one shared reader, which calls routectl-usage's
/// `observation_totals`; none of the three sources issues SQL of its own.
///
/// Mutation check: replace the `observation_totals(` call in
/// `window_observation` with an inline `db.conn().query_row(..)` -> red; drop
/// the `window_observation(` call from either panel -> red.
#[test]
fn status_panels_read_observation_totals_without_their_own_sql() {
    let sources = [
        ("usage.rs", include_str!("usage.rs")),
        ("doctor.rs", include_str!("doctor.rs")),
        ("usage_health.rs", include_str!("usage_health.rs")),
    ];
    for (name, src) in sources {
        assert_eq!(
            ledger_sql_in(src),
            Vec::<&str>::new(),
            "{name} must not carry its own ledger SQL"
        );
    }
    let production =
        |src| crate::handlers::status::production_source::production_source(src).to_string();
    assert!(
        production(include_str!("usage_health.rs")).contains(concat!("observation_totals", "(db"))
    );
    for (name, src) in &sources[..2] {
        assert!(
            production(src).contains(concat!("window_observation", "(")),
            "{name} must read its observation block through window_observation"
        );
    }

    // Positive control: the scanner trips on an inline statement.
    let inline = concat!(
        "fn f(db: &UsageDb) { db.con",
        "n().query_",
        "row(\"SEL",
        "ECT 1 FROM ",
        "ledger_gaps\", [], |r| r.get(0)); }"
    );
    assert_eq!(ledger_sql_in(inline).len(), 4);
}
