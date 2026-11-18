// Terminal-safety tests for the ledger text `routectl usage` prints: group
// labels, quota fields, reduction tokens and the K-suppression finder.
// `include!`d into the `usage_tests.rs` module after the render fragments, so
// their helpers (`report_all`, `insert_seat_quota_row`, `reduction_row`,
// `decision_triple_row`, ...) are in scope. Do not add `use` lines here.

/// Client-reachable text carrying a CSI color, an OSC title sequence, a BEL,
/// a C1 CSI and a C1 NEL.
const HOSTILE_LEDGER_TEXT: &str = "ev\u{1b}[31mil\u{1b}]0;t\u{7}x\u{9b}2J\u{85}!";

/// What the shared terminal sanitizer leaves of [`HOSTILE_LEDGER_TEXT`].
const HOSTILE_LEDGER_TEXT_RENDERED: &str = "evil]0;tx!";

/// Insert one ok row with an explicit requested model, served model, alias
/// and seat, so each group label can carry text of the test's choosing.
fn labelled_row(
    db: &UsageDb,
    request_id: &str,
    requested_model: &str,
    model: Option<&str>,
    alias: &str,
    seat: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, seat) \
             VALUES (1000, 1000, ?1, 'openai', ?2, ?3, ?4, 'paid', 'up-paid', 0, 'ok', \
             5, 0, 0, 1, 0, 10, 20, ?5)",
            rusqlite::params![request_id, requested_model, alias, model, seat],
        )
        .expect("insert labelled row");
}

/// The control characters in `out` other than the line breaks the renderer
/// itself writes.
fn stray_controls(out: &str) -> Vec<char> {
    out.chars()
        .filter(|c| c.is_control() && *c != '\n')
        .collect()
}

struct HostileCase {
    name: &'static str,
    seed: fn(&UsageDb),
    by: Option<GroupDim>,
    detail: bool,
}

#[test]
fn ledger_text_reaches_the_terminal_without_control_sequences() {
    let cases = [
        HostileCase {
            name: "model label from an unserved requested model",
            seed: |db| labelled_row(db, "r1", HOSTILE_LEDGER_TEXT, None, "al", None),
            by: Some(GroupDim::Model),
            detail: false,
        },
        HostileCase {
            name: "alias label",
            seed: |db| labelled_row(db, "r1", "req", Some("m"), HOSTILE_LEDGER_TEXT, None),
            by: Some(GroupDim::Alias),
            detail: false,
        },
        HostileCase {
            name: "seat label",
            seed: |db| labelled_row(db, "r1", "req", Some("m"), "al", Some(HOSTILE_LEDGER_TEXT)),
            by: Some(GroupDim::Seat),
            detail: false,
        },
        HostileCase {
            name: "quota seat",
            seed: |db| {
                insert_seat_quota_row(
                    db,
                    "q1",
                    1000,
                    Some(HOSTILE_LEDGER_TEXT),
                    Some("ok"),
                    None,
                    None,
                    None,
                )
            },
            by: Some(GroupDim::Model),
            detail: false,
        },
        HostileCase {
            name: "quota status",
            seed: |db| {
                insert_seat_quota_row(
                    db,
                    "q1",
                    1000,
                    None,
                    Some(HOSTILE_LEDGER_TEXT),
                    None,
                    None,
                    None,
                )
            },
            by: Some(GroupDim::Model),
            detail: false,
        },
        HostileCase {
            name: "quota overage status",
            seed: |db| {
                insert_seat_quota_row(
                    db,
                    "q1",
                    1000,
                    None,
                    Some("ok"),
                    None,
                    Some(HOSTILE_LEDGER_TEXT),
                    None,
                )
            },
            by: Some(GroupDim::Model),
            detail: false,
        },
        HostileCase {
            name: "reduction decision token",
            seed: |db| reduction_row(db, "d1", Some(HOSTILE_LEDGER_TEXT), 0, 0, 0, 0),
            by: Some(GroupDim::Model),
            detail: true,
        },
        HostileCase {
            name: "suppressed-session model",
            seed: |db| {
                decision_triple_row(
                    db,
                    "k1",
                    1000,
                    "s",
                    "anthropic-api",
                    HOSTILE_LEDGER_TEXT,
                    Some(K_SUPPRESSION_TOKEN),
                    None,
                );
            },
            by: Some(GroupDim::Provider),
            detail: true,
        },
        HostileCase {
            name: "suppressed-session provider kind",
            seed: |db| {
                decision_triple_row(
                    db,
                    "k1",
                    1000,
                    "s",
                    HOSTILE_LEDGER_TEXT,
                    "m",
                    Some(K_SUPPRESSION_TOKEN),
                    None,
                );
            },
            by: Some(GroupDim::Model),
            detail: true,
        },
    ];
    for case in cases {
        // Arrange
        let (_dir, _path, mut db) = temp_db();
        (case.seed)(&db);

        // Act
        let report = report_all(&mut db, &cost_config(), case.by, case.detail);
        let out = render_report(&report, &zone());

        // Assert: the text was printed, and printed without its controls.
        assert!(
            out.contains(HOSTILE_LEDGER_TEXT_RENDERED),
            "{}: the seeded text never reached the output:\n{out}",
            case.name
        );
        assert_eq!(
            stray_controls(&out),
            Vec::<char>::new(),
            "{}:\n{out:?}",
            case.name
        );
    }
}

#[test]
fn printable_ledger_labels_render_byte_identical() {
    // Arrange
    let labels = ["claude-sonnet-4.5", "work#a", "mod\u{e8}le", "(x) = y"];
    let (_dir, _path, mut db) = temp_db();
    for (i, label) in labels.iter().enumerate() {
        labelled_row(&db, &format!("p{i}"), label, None, "al", None);
    }

    // Act
    let report = report_all(&mut db, &cost_config(), Some(GroupDim::Model), false);
    let out = render_report(&report, &zone());

    // Assert
    for label in labels {
        assert!(
            out.lines()
                .any(|line| line.starts_with(&format!("{label} "))),
            "{label:?} not printed verbatim:\n{out}"
        );
    }
}
