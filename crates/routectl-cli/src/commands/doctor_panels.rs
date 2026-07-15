//! Doctor structured panels: the read-only would-trim opportunity panel.
//!
//! Computes the steady-state would-trim panel from the usage DB WITHOUT
//! writing it, and renders it as a human block. The doctor command attaches
//! the computed panel to `DoctorReport.panels` and calls the render helper for
//! its human battery. Kept apart from the doctor orchestration so the
//! compute/map/render seam is unit-testable against a temp DB.

use chrono::{DateTime, Local};

use routectl_router::{Config, WouldTrimPanel};
use routectl_usage::{OpenError, WouldTrimSummary, open_readonly, would_trim_summary};

use super::usage::{WindowFlag, human_count, window_bounds};

/// The calendar window the doctor panel summarizes. All-time gives the
/// fullest cumulative signal for a one-shot diagnostic, so a day with no
/// recorded traffic never blanks the panel.
const DEFAULT_WINDOW: WindowFlag = WindowFlag::All;

/// Path-free class token for an unexpected usage-DB open failure. Several
/// `OpenError` variants embed the DB PATH in their Display, so the logging
/// site on the unauthenticated status surface must never emit the Display --
/// it logs this fixed variant class instead. A new variant is a compile error
/// here, forcing a deliberate log-hygiene decision for any new failure mode.
const fn open_error_class(err: &OpenError) -> &'static str {
    match err {
        OpenError::CreateDir { .. } => "create_dir",
        OpenError::Open { .. } => "open",
        OpenError::Pragma(_) => "pragma",
        OpenError::Permissions { .. } => "permissions",
        OpenError::Migrate(_) => "migrate",
        OpenError::VersionTooNew { .. } => "version_too_new",
        OpenError::NotWal { .. } => "not_wal",
        OpenError::NoData { .. } | OpenError::VersionTooOld { .. } => "expected",
    }
}

/// Compute the would-trim panel from the usage DB, read-only.
///
/// Opens the usage DB via `open_readonly` and summarizes the would-trim
/// opportunity over the default window (`window_bounds(DEFAULT_WINDOW, now)`).
/// Returns `None` when there is no panel to show -- a missing / unmigrated DB,
/// or any other read failure -- so the panel is best-effort and never fails the
/// surrounding diagnostic. A present DB with zero candidates returns a `Some`
/// all-zero panel, which is distinct from "no data".
///
/// `now` is passed in (never read from the clock here) so the window is
/// deterministic under test.
pub(crate) fn compute_would_trim_panel(
    config: &Config,
    now: DateTime<Local>,
) -> Option<WouldTrimPanel> {
    let db = match open_readonly(&config.usage.db_path) {
        Ok(db) => db,
        Err(OpenError::NoData { .. } | OpenError::VersionTooOld { .. }) => return None,
        Err(e) => {
            // `OpenError` Display embeds the DB PATH; this poll fires on the
            // unauthenticated status surface whose logs ship off-host, so only
            // a fixed path-free variant class is logged, never the Display.
            tracing::debug!(
                reason = open_error_class(&e),
                "would-trim panel: usage db unavailable, omitting"
            );
            return None;
        }
    };
    let bounds = window_bounds(DEFAULT_WINDOW, now);
    match would_trim_summary(&db, bounds.from_ms, bounds.to_ms) {
        Ok(summary) => Some(panel_from_summary(summary)),
        Err(e) => {
            tracing::debug!(error = %e, "would-trim panel: summary query failed, omitting");
            None
        }
    }
}

/// Map the usage-crate summary onto the router-side panel, field for field.
const fn panel_from_summary(summary: WouldTrimSummary) -> WouldTrimPanel {
    WouldTrimPanel {
        candidate_requests: summary.candidate_requests,
        would_trim_tokens: summary.would_trim_tokens,
        verdict_met: summary.verdict_met,
        verdict_unmet: summary.verdict_unmet,
        verdict_cold: summary.verdict_cold,
        verdict_unpriced: summary.verdict_unpriced,
    }
}

/// Render the would-trim panel as a human block with a remediation hint. An
/// all-zero panel renders a single clean "no opportunity" line; a panel with
/// candidates mirrors the `routectl usage` would-trim block and points at
/// `prompt-size --steady-state` for a per-request inspection.
pub(crate) fn render_would_trim_panel(panel: &WouldTrimPanel) -> String {
    if panel.candidate_requests == 0 {
        return "would-trim: no would-trim opportunity recorded (advisory)\n".to_string();
    }
    format!(
        "would-trim: {} reqs with a would-cut candidate, {} tokens (advisory; not applied)\n  \
         verdict: met={} unmet={} cold={} unpriced={}\n  \
         inspect a request: routectl prompt-size --steady-state\n",
        panel.candidate_requests,
        human_count(panel.would_trim_tokens),
        panel.verdict_met,
        panel.verdict_unmet,
        panel.verdict_cold,
        panel.verdict_unpriced,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use routectl_usage::{UsageDb, open};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn fixed_now() -> DateTime<Local> {
        Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2026, 6, 11)
                    .expect("valid date")
                    .and_hms_opt(14, 30, 0)
                    .expect("valid time"),
            )
            .earliest()
            .expect("unambiguous local time")
    }

    fn temp_db() -> (TempDir, PathBuf, UsageDb) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("usage.db");
        let db = open(&path).expect("open");
        (dir, path, db)
    }

    fn config_for(path: &Path) -> Config {
        let mut config = Config::default();
        config.usage.db_path = path.to_path_buf();
        config
    }

    /// Insert a row carrying an optional would-trim candidate. `None` tokens
    /// records a request with no candidate (COUNT ignores it).
    fn insert_would_trim_row(db: &UsageDb, request_id: &str, ts_start: i64, tokens: Option<i64>) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count, would_trim_tokens) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, ?3)",
                rusqlite::params![ts_start, request_id, tokens],
            )
            .expect("insert would-trim row");
    }

    #[test]
    fn panel_from_summary_preserves_every_field() {
        // Arrange: distinct values in every field so a mis-wired field shows.
        let summary = WouldTrimSummary {
            candidate_requests: 7,
            would_trim_tokens: 60_000,
            verdict_met: 3,
            verdict_unmet: 2,
            verdict_cold: 1,
            verdict_unpriced: 4,
        };

        // Act
        let panel = panel_from_summary(summary);

        // Assert
        assert_eq!(panel.candidate_requests, 7);
        assert_eq!(panel.would_trim_tokens, 60_000);
        assert_eq!(panel.verdict_met, 3);
        assert_eq!(panel.verdict_unmet, 2);
        assert_eq!(panel.verdict_cold, 1);
        assert_eq!(panel.verdict_unpriced, 4);
    }

    #[test]
    fn zero_summary_maps_to_zero_panel_and_renders_clean_line() {
        // Arrange + Act
        let panel = panel_from_summary(WouldTrimSummary::default());
        let rendered = render_would_trim_panel(&panel);

        // Assert: every field zero, and the clean no-opportunity line.
        assert_eq!(panel.candidate_requests, 0);
        assert_eq!(panel.would_trim_tokens, 0);
        assert_eq!(panel.verdict_met, 0);
        assert_eq!(panel.verdict_unmet, 0);
        assert_eq!(panel.verdict_cold, 0);
        assert_eq!(panel.verdict_unpriced, 0);
        assert!(rendered.contains("no would-trim opportunity"));
    }

    #[test]
    fn render_panel_with_candidates_shows_counts_and_remediation() {
        // Arrange
        let panel = WouldTrimPanel {
            candidate_requests: 5,
            would_trim_tokens: 42_000,
            verdict_met: 2,
            verdict_unmet: 1,
            verdict_cold: 1,
            verdict_unpriced: 1,
        };

        // Act
        let rendered = render_would_trim_panel(&panel);

        // Assert: counts, verdict breakdown, and the remediation hint. Tokens
        // render through the shared humanizer, matching the usage surface.
        assert!(rendered.contains("5 reqs"));
        assert!(rendered.contains("42K tokens"));
        assert!(rendered.contains("met=2 unmet=1 cold=1 unpriced=1"));
        assert!(rendered.contains("prompt-size --steady-state"));
    }

    #[test]
    fn compute_returns_none_for_missing_db() {
        // Arrange: a config pointing at a path with no DB file.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("absent.db");
        let config = config_for(&path);

        // Act + Assert: NoData collapses to no panel, never an error.
        assert!(compute_would_trim_panel(&config, fixed_now()).is_none());
    }

    #[test]
    fn compute_returns_none_for_unmigrated_db() {
        // Arrange: a DB whose schema predates this binary. Roll a freshly
        // seeded DB's user_version below the supported schema so open_readonly
        // reports VersionTooOld.
        let (_dir, path, db) = temp_db();
        db.conn()
            .execute_batch("PRAGMA user_version = 1")
            .expect("roll version back");
        drop(db);

        // Act + Assert: VersionTooOld collapses to no panel, never an error.
        assert!(compute_would_trim_panel(&config_for(&path), fixed_now()).is_none());
    }

    #[test]
    fn compute_summarizes_candidates_over_default_window() {
        // Arrange: two in-window candidates, one non-candidate, and one row
        // stamped AFTER `now` -- excluded by the default window's upper bound.
        let (_dir, path, db) = temp_db();
        let now = fixed_now();
        let now_ms = now.timestamp_millis();
        insert_would_trim_row(&db, "w1", now_ms - 2_000, Some(40_000));
        insert_would_trim_row(&db, "w2", now_ms - 1_000, Some(20_000));
        insert_would_trim_row(&db, "plain", now_ms - 500, None);
        insert_would_trim_row(&db, "future", now_ms + 10_000, Some(99_000));
        drop(db);

        // Act
        let panel = compute_would_trim_panel(&config_for(&path), now).expect("panel");

        // Assert: the future candidate is excluded by the window's upper bound.
        assert_eq!(panel.candidate_requests, 2);
        assert_eq!(panel.would_trim_tokens, 60_000);
    }

    #[test]
    fn compute_leaves_db_file_byte_identical() {
        // Arrange: seed a candidate, then drop the writer so the DB is quiescent.
        let (_dir, path, db) = temp_db();
        let now = fixed_now();
        insert_would_trim_row(&db, "w1", now.timestamp_millis() - 1_000, Some(30_000));
        drop(db);
        let before = std::fs::read(&path).expect("read db before");

        // Act: the read-only compute must not touch the file.
        let panel = compute_would_trim_panel(&config_for(&path), now).expect("panel");

        // Assert: candidate seen, and the DB file is byte-for-byte unchanged.
        assert_eq!(panel.candidate_requests, 1);
        let after = std::fs::read(&path).expect("read db after");
        assert_eq!(
            before, after,
            "read-only compute must not mutate the db file"
        );
    }

    #[test]
    fn docs_cover_steady_state_flag_and_would_trim_fields() {
        // The docs absorb the would-trim documentation task: the offline flag
        // and every would_trim_* field must stay documented. include_str! also
        // makes a moved/renamed docs file a compile error.
        let docs = include_str!("../../../../docs/CONFIGURATION.md");
        assert!(docs.contains("`--steady-state`"), "flag not documented");
        assert!(
            docs.contains("`candidate_requests`"),
            "field not documented"
        );
        assert!(docs.contains("`would_trim_tokens`"), "field not documented");
        for verdict in ["`met`", "`unmet`", "`cold`", "`unpriced`"] {
            assert!(docs.contains(verdict), "verdict {verdict} not documented");
        }
    }

    #[test]
    fn open_error_class_is_path_free_for_every_variant() {
        // Every class token is a fixed discriminant, never a path. The
        // path-bearing variants (Display embeds the DB path) must still map to
        // a clean token.
        let cases = [
            open_error_class(&OpenError::Open {
                path: "/secret/usage.db".into(),
                source: rusqlite::Error::QueryReturnedNoRows,
            }),
            open_error_class(&OpenError::CreateDir {
                path: "/secret/dir".into(),
                source: std::io::Error::other("x"),
            }),
        ];
        for token in cases {
            assert!(
                !token.contains('/') && !token.contains("secret"),
                "class token must be path-free: {token}"
            );
        }
        assert_eq!(
            open_error_class(&OpenError::Open {
                path: "/secret/usage.db".into(),
                source: rusqlite::Error::QueryReturnedNoRows,
            }),
            "open"
        );
    }

    #[test]
    fn compute_log_on_open_failure_carries_no_path() {
        use routectl_testkit::capture_events;

        // Arrange: point the usage path at a DIRECTORY so `open_readonly`
        // fails with `OpenError::Open` -- whose Display embeds the path -- via
        // the catch-all `Err(e)` branch (not NoData / VersionTooOld).
        let dir = TempDir::new().expect("tempdir");
        let db_dir = dir.path().join("usage-as-a-dir");
        std::fs::create_dir(&db_dir).expect("create dir at db path");
        let secret_fragment = db_dir.to_string_lossy().into_owned();
        let config = config_for(&db_dir);

        // Act: capture the omission log emitted while computing the panel.
        let events = capture_events(|| {
            assert!(compute_would_trim_panel(&config, fixed_now()).is_none());
        });

        // Assert: the panel is omitted, exactly the path-free class is logged,
        // and no captured event (message OR any field) carries the DB path.
        let event = events
            .iter()
            .find(|e| e.message.contains("would-trim panel: usage db unavailable"))
            .expect("omission log emitted");
        assert_eq!(event.field("reason"), Some("open"));
        for e in &events {
            assert!(
                !e.message.contains(&*secret_fragment),
                "log message leaked the db path: {}",
                e.message
            );
            for (name, value) in &e.fields {
                assert!(
                    !value.contains(&*secret_fragment),
                    "log field `{name}` leaked the db path: {value}"
                );
            }
        }
    }
}
