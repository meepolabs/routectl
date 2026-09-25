//! Shared ledger-row fixtures for the opening-accuracy unit tests. Every row
//! is built from the producer's own label constants, not re-typed strings.

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::handlers::opening_diagnostics::{UPSTREAM_OPENER_REASON, key, source, terminal};
use crate::ingress::anthropic::context_anchor::OpeningReason;
use routectl_usage::{UsageDb, open};

/// A served lane as persisted: `(provider_kind, provider, model, upstream)`.
pub(super) type Lane = (&'static str, &'static str, &'static str, &'static str);

pub(super) const LANE_A: Lane = ("anthropic-api", "anth", "opus", "claude-opus-4-7");
pub(super) const LANE_B: Lane = ("openai-compat", "compat", "glm", "glm-4.6");
pub(super) const LANE_A_KEY: &str = "anthropic-api:anth/opus@claude-opus-4-7";
pub(super) const LANE_B_KEY: &str = "openai-compat:compat/glm@glm-4.6";

pub(super) const ANCHOR: &str = "anchor";
pub(super) const CALIBRATED: &str = "calibrated";
pub(super) const RAW: &str = "raw";

/// One ledger row, with full control over what the report reads.
pub(super) struct Row {
    pub(super) ts: i64,
    pub(super) dialect: &'static str,
    pub(super) stream: bool,
    pub(super) lane: Option<Lane>,
    pub(super) outcome: &'static str,
    pub(super) extra: Option<String>,
}

impl Row {
    pub(super) const fn anthropic(
        lane: Option<Lane>,
        outcome: &'static str,
        extra: Option<String>,
    ) -> Self {
        Self {
            ts: 1_000,
            dialect: "anthropic",
            stream: true,
            lane,
            outcome,
            extra,
        }
    }

    pub(super) fn ok(extra: String) -> Self {
        Self::anthropic(Some(LANE_A), "ok", Some(extra))
    }
}

pub(super) fn insert(db: &UsageDb, id: usize, row: &Row) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, provider_kind, provider, model, upstream, stream, \
             outcome, latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             extra) \
             VALUES (?1, ?1, ?2, ?3, 'req', 'al', ?4, ?5, ?6, ?7, ?8, ?9, 5, 0, 1, 1, 0, ?10)",
            rusqlite::params![
                row.ts,
                format!("r{id}"),
                row.dialect,
                row.lane.map(|l| l.0),
                row.lane.map(|l| l.1),
                row.lane.map(|l| l.2),
                row.lane.map(|l| l.3),
                i64::from(row.stream),
                row.outcome,
                row.extra,
            ],
        )
        .expect("insert");
}

pub(super) fn seed_at(path: &std::path::Path, rows: &[Row]) {
    let db = open(path).expect("open");
    for (i, row) in rows.iter().enumerate() {
        insert(&db, i, row);
    }
}

pub(super) const WINDOW: WindowBounds = WindowBounds {
    from_ms: 500,
    to_ms: 2_000,
};

pub(super) fn report_of(rows: &[Row]) -> OpeningAccuracyReport {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    seed_at(&path, rows);
    let db = routectl_usage::open_readonly(&path).expect("read-only");
    build_report(&db, WINDOW).expect("report")
}

/// The reason the producer pairs with each source in these fixtures.
fn reason_for(source_label: &str) -> &'static str {
    if source_label == ANCHOR {
        OpeningReason::AnchorHit.as_str()
    } else if source_label.starts_with(source::UPSTREAM_WIRE) {
        UPSTREAM_OPENER_REASON
    } else {
        MissReason::Cold.as_str()
    }
}

/// A known row's `extra`, shaped as the producer writes it: `source`, an
/// optional opening count, and an optional terminal `(label, input,
/// vendor_verified)`; no terminal is the producer's `missing`.
pub(super) fn opened(
    source_label: &str,
    opening: Option<u64>,
    terminal_report: Option<(&str, Value, bool)>,
) -> String {
    let mut extra = json!({
        key::OPENING_PRESENT: true,
        key::OPENING_SOURCE: source_label,
        key::OPENING_REASON: reason_for(source_label),
        key::OPENING_PROVISIONAL: false,
    });
    if let Some(opening) = opening {
        extra[key::OPENING_INPUT] = json!(opening);
    }
    match terminal_report {
        Some((label, input, verified)) => {
            extra[key::TERMINAL_SOURCE] = json!(label);
            extra[key::TERMINAL_INPUT] = input;
            extra[key::TERMINAL_VENDOR_VERIFIED] = json!(verified);
        }
        None => {
            extra[key::TERMINAL_SOURCE] = json!(terminal::MISSING);
            extra[key::TERMINAL_VENDOR_VERIFIED] = json!(false);
        }
    }
    extra.to_string()
}

pub(super) fn explicit(n: u64, verified: bool) -> Option<(&'static str, Value, bool)> {
    Some((terminal::EXPLICIT_FINAL, json!(n), verified))
}

pub(super) fn anchored(opening: u64, terminal_input: u64, verified: bool) -> String {
    opened(ANCHOR, Some(opening), explicit(terminal_input, verified))
}

/// `extra` with one key replaced (`None` removes it).
pub(super) fn with(extra: &str, field: &str, value: Option<Value>) -> String {
    let mut map: serde_json::Map<String, Value> = serde_json::from_str(extra).expect("json");
    match value {
        Some(v) => map.insert(field.to_string(), v),
        None => map.remove(field),
    };
    Value::Object(map).to_string()
}

pub(super) fn no_opening() -> String {
    json!({key::OPENING_PRESENT: false}).to_string()
}

/// `pass` rows exactly on target and `fail` rows 100% over, all anchored.
pub(super) fn anchored_rows(pass: usize, fail: usize, verified: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = (0..pass)
        .map(|_| Row::ok(anchored(1_000, 1_000, verified)))
        .collect();
    rows.extend((0..fail).map(|_| Row::ok(anchored(2_000, 1_000, verified))));
    rows
}

pub(super) fn counts(pairs: &[(&'static str, u64)]) -> BTreeMap<&'static str, u64> {
    pairs.iter().copied().collect()
}

/// The hand-computed mixed fixture (see the per-row comments), plus three
/// rows outside the universe.
pub(super) fn mixed_fixture() -> Vec<Row> {
    let a = Some(LANE_A);
    let b = Some(LANE_B);
    vec![
        // 1: pass, exact, verified. Bucket 0-5.
        Row::anthropic(a, "ok", Some(anchored(1_000, 1_000, true))),
        // 2: pass at exactly 5% over, unverified. Bucket 0-5.
        Row::anthropic(a, "ok", Some(anchored(1_050, 1_000, false))),
        // 3: one token past 5%, unverified. Bucket 5-10.
        Row::anthropic(a, "ok", Some(anchored(1_051, 1_000, false))),
        // 4: a large miss against a verified vendor-opening terminal. >20.
        Row::anthropic(
            b,
            "ok",
            Some(opened(
                ANCHOR,
                Some(5_000),
                Some((terminal::VENDOR_OPENING, json!(2_000), true)),
            )),
        ),
        // 5: a failed turn with no terminal.
        Row::anthropic(b, "upstream_error", Some(opened(ANCHOR, Some(1_000), None))),
        // 6: a completed turn whose terminal never reported input.
        Row::anthropic(b, "ok", Some(opened(ANCHOR, Some(1_000), None))),
        // 7: a proxy-opening carry is not evidence; unverified report.
        Row::anthropic(
            b,
            "ok",
            Some(opened(
                ANCHOR,
                Some(1_000),
                Some((terminal::PROXY_OPENING, json!(1_000), false)),
            )),
        ),
        // 8: an explicit zero terminal; unverified report.
        Row::anthropic(
            b,
            "ok",
            Some(opened(ANCHOR, Some(1_000), explicit(0, false))),
        ),
        // 9: an anchored opening whose count was not stated; unverified.
        Row::anthropic(a, "ok", Some(opened(ANCHOR, None, explicit(1_000, false)))),
        // 10: cold calibrated, 12% under; unverified. Bucket 10-20.
        Row::anthropic(
            a,
            "ok",
            Some(opened(CALIBRATED, Some(880), explicit(1_000, false))),
        ),
        // 11: cold raw, 60% under; verified. Bucket >20.
        Row::anthropic(a, "ok", Some(opened(RAW, Some(400), explicit(1_000, true)))),
        // 12: an unverified upstream-wire opening. Bucket 0-5.
        Row::anthropic(
            a,
            "ok",
            Some(opened(
                source::UPSTREAM_WIRE_UNVERIFIED,
                Some(1_000),
                explicit(1_000, false),
            )),
        ),
        // 13: no opening, no lane.
        Row::anthropic(None, "upstream_error", Some(no_opening())),
        // 14, 15: legacy rows with no diagnostics.
        Row::anthropic(a, "ok", None),
        Row::anthropic(a, "ok", Some(json!({"stream_stage": "body"}).to_string())),
        // 16: unclassifiable.
        Row::anthropic(b, "ok", Some("{not json".to_string())),
        // 17: an exact anchor whose marker is not a bool: a known anchor
        // non-pass with a marker defect.
        Row::anthropic(
            b,
            "ok",
            Some(json!({key::OPENING_PRESENT: "true", key::OPENING_SOURCE: ANCHOR}).to_string()),
        ),
        // 18: an anchored row whose terminal input is a string; unverified.
        Row::anthropic(
            a,
            "ok",
            Some(opened(
                ANCHOR,
                Some(1_000),
                Some((terminal::EXPLICIT_FINAL, json!("1000"), false)),
            )),
        ),
        // Outside the universe: another dialect, a unary row, out of window.
        Row {
            dialect: "openai",
            ..Row::anthropic(a, "ok", Some(anchored(1, 1_000, false)))
        },
        Row {
            stream: false,
            ..Row::anthropic(a, "ok", Some(anchored(1, 1_000, false)))
        },
        Row {
            ts: 2_000,
            ..Row::anthropic(a, "ok", Some(anchored(1, 1_000, false)))
        },
    ]
}
