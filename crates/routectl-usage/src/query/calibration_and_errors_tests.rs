// The k-floor calibration summary and the errors-by-class breakdown. Split
// from `tests.rs` to keep each file under the size ceiling; `include!`d into
// the same `tests` module so the helpers there stay in scope. All imports come
// from the host `tests.rs` -- do not add `use` lines here.

/// Insert a calibration row: an optional `would_trim_k_floor` (None ->
/// uncalibrated, still counts as future reuse) plus the (session_id,
/// provider_kind, model) triple and a `cache_read` snapshot. `ts_start`
/// drives the remaining-future ordering.
#[allow(clippy::too_many_arguments)]
fn insert_calib_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: &str,
    provider_kind: &str,
    model: &str,
    k_floor: Option<f64>,
    cache_read: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, session_id, \
             stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
             fallback_count, would_trim_k_floor, cache_read) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, 'paid', ?4, ?5, \
             1, 'ok', 5, 0, 0, 1, 0, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                provider_kind,
                session_id,
                k_floor,
                cache_read,
            ],
        )
        .expect("insert calib row");
}

#[test]
fn k_calibration_coverage_uses_remaining_future_not_whole_session() {
    // Arrange: ONE session whose reuse is concentrated EARLY. Under the
    // old whole-session comparison every calibrated row would see the
    // group's total of 2 hits and all three would be "covered". Under the
    // remaining-future comparison a LATE over-prediction is correctly a
    // miss, because no reuse remains after it.
    //   r1 ts=100 hit,  floor=1.0  -> 1 future hit (r2)  -> covered (1>=1)
    //   r2 ts=200 hit,  UNCALIBRATED (feeds future reuse, not population)
    //   r3 ts=300 miss, floor=2.0  -> 0 future hits      -> MISS (0<2)
    //   r4 ts=400 miss, floor=0.5  -> 0 future hits      -> MISS (0<0.5)
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_calib_row(&db, "r1", 100, "s1", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "r2", 200, "s1", "anth", "m1", None, 5);
    insert_calib_row(&db, "r3", 300, "s1", "anth", "m1", Some(2.0), 0);
    insert_calib_row(&db, "r4", 400, "s1", "anth", "m1", Some(0.5), 0);

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: population is the 3 calibrated rows; remaining-future
    // coverage is 1/3 (whole-session would have been 3/3).
    assert_eq!(cal.n, 3, "only the calibrated rows form the population");
    assert!(
        (cal.coverage - 1.0 / 3.0).abs() < 1e-9,
        "remaining-future coverage must be 1/3, got {}",
        cal.coverage
    );
    // Per-row normalized errors: |1-1|/2=0, |2-0|/1=2, |0.5-0|/1=0.5;
    // sorted [0, 0.5, 2] -> median 0.5.
    assert!(
        (cal.accuracy - 0.5).abs() < 1e-9,
        "per-row-normalized median accuracy must be 0.5, got {}",
        cal.accuracy
    );
}

#[test]
fn k_calibration_hazard_decay_is_negative_for_decaying_session() {
    // Arrange: a 4-turn session whose reuse decays -- both first-half
    // turns reused, neither second-half turn did. first_rate=1.0,
    // second_rate=0.0 -> delta = -1.0. All rows calibrated so n>0 and the
    // main path computes hazard_decay.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_calib_row(&db, "d0", 100, "sd", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "d1", 200, "sd", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "d2", 300, "sd", "anth", "m1", Some(1.0), 0);
    insert_calib_row(&db, "d3", 400, "sd", "anth", "m1", Some(1.0), 0);

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: a material negative decay -- the age-conditioning trigger.
    assert!(
        (cal.hazard_decay + 1.0).abs() < 1e-9,
        "decaying session must yield hazard_decay = -1.0, got {}",
        cal.hazard_decay
    );
}

#[test]
fn k_calibration_hazard_decay_is_zero_for_flat_session() {
    // Arrange: a 4-turn session with a CONSTANT (flat) reuse rate -- every
    // turn reused. Both halves rate 1.0 -> delta 0.0.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for (i, ts) in [100, 200, 300, 400].into_iter().enumerate() {
        insert_calib_row(&db, &format!("f{i}"), ts, "sf", "anth", "m1", Some(1.0), 5);
    }

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert
    assert_eq!(cal.hazard_decay, 0.0, "flat reuse -> zero decay");
}

#[test]
fn k_calibration_hazard_decay_is_zero_when_no_group_has_enough_rows() {
    // Arrange: a session with fewer than HAZARD_DECAY_MIN_GROUP_ROWS rows
    // -- no group qualifies, so the halves would be too noisy to inform
    // the age-conditioning decision.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_calib_row(&db, "g0", 100, "sg", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "g1", 200, "sg", "anth", "m1", Some(1.0), 0);
    insert_calib_row(&db, "g2", 300, "sg", "anth", "m1", Some(1.0), 5);

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: no qualifying group -> hazard_decay defaults to 0.0.
    assert_eq!(cal.hazard_decay, 0.0);
}

#[test]
fn k_calibration_empty_db_is_all_zero_including_hazard_decay() {
    // Arrange: no rows at all.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");

    // Act
    let cal = k_calibration_summary(&db).expect("summary");

    // Assert: the n==0 early return zeroes every field.
    assert_eq!(cal.n, 0);
    assert_eq!(cal.coverage, 0.0);
    assert_eq!(cal.accuracy, 0.0);
    assert_eq!(cal.hazard_decay, 0.0);
}

/// Insert a row with an explicit `outcome` and (nullable) `resolved_class`
/// so the errors-by-class breakdown's classify/NULL paths are testable.
/// `model` is fixed so the group key is `(provider, upstream, alias)`.
#[allow(clippy::too_many_arguments)]
fn insert_class_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    provider: &str,
    upstream: &str,
    alias: &str,
    outcome: &str,
    resolved_class: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             resolved_class) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, 'm', ?4, ?5, 0, ?6, 5, \
             0, 0, 1, 0, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                alias,
                provider,
                upstream,
                outcome,
                resolved_class,
            ],
        )
        .expect("insert class row");
}

#[test]
fn errors_by_class_sums_to_errors_per_group_and_at_totals() {
    use std::collections::HashMap;

    // Arrange: two groups with a mix of ok / client_disconnect (excluded)
    // and classified / NULL-class error rows.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Group A (pa, ua): 4 errors across 3 classes incl. an unclassified.
    insert_class_row(&db, "a-ok", 100, "pa", "ua", "al", "ok", None);
    insert_class_row(
        &db,
        "a-cd",
        105,
        "pa",
        "ua",
        "al",
        "client_disconnect",
        None,
    );
    insert_class_row(
        &db,
        "a-e1",
        110,
        "pa",
        "ua",
        "al",
        "upstream_error",
        Some("http-5xx"),
    );
    insert_class_row(
        &db,
        "a-e2",
        120,
        "pa",
        "ua",
        "al",
        "upstream_error",
        Some("http-5xx"),
    );
    insert_class_row(&db, "a-e3", 130, "pa", "ua", "al", "gate_blocked", None);
    insert_class_row(
        &db,
        "a-e4",
        140,
        "pa",
        "ua",
        "al",
        "upstream_error",
        Some("timeout"),
    );
    // Group B (pb, ub): 1 classified error.
    insert_class_row(
        &db,
        "b-e1",
        150,
        "pb",
        "ub",
        "al",
        "upstream_error",
        Some("rate-limited"),
    );

    // Act
    let agg = aggregate(&db, 0, 1000).expect("aggregate");
    let breakdown = errors_by_class(&db, 0, 1000).expect("errors_by_class");

    // Assert: per-group class counts sum EXACTLY to that group's errors.
    let mut per_group: HashMap<GroupKey, i64> = HashMap::new();
    for (key, _class, count) in &breakdown {
        *per_group.entry(key.clone()).or_default() += *count;
    }
    for row in &agg {
        let class_sum = per_group.get(&row.key).copied().unwrap_or(0);
        assert_eq!(
            class_sum, row.errors,
            "group {:?} class sum {class_sum} != errors {}",
            row.key, row.errors
        );
    }
    // Group A breakdown: http-5xx=2, unclassified=1, timeout=1.
    let a_key = agg
        .iter()
        .find(|r| r.key.provider.as_deref() == Some("pa"))
        .expect("group A")
        .key
        .clone();
    let a_classes: std::collections::BTreeMap<String, i64> = breakdown
        .iter()
        .filter(|(k, _, _)| *k == a_key)
        .map(|(_, c, n)| (c.clone(), *n))
        .collect();
    assert_eq!(a_classes.get("http-5xx"), Some(&2));
    assert_eq!(a_classes.get("unclassified"), Some(&1));
    assert_eq!(a_classes.get("timeout"), Some(&1));

    // Totals: the breakdown sums to the summed errors across all groups.
    let total_errors: i64 = agg.iter().map(|r| r.errors).sum();
    let total_breakdown: i64 = breakdown.iter().map(|(_, _, n)| *n).sum();
    assert_eq!(total_breakdown, total_errors);
    assert_eq!(total_breakdown, 5);
}

#[test]
fn errors_by_class_empty_window_returns_no_rows() {
    // Arrange: an ok row and a client_disconnect row (neither is an error),
    // plus an out-of-window error row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_class_row(&db, "ok", 100, "p", "u", "a", "ok", None);
    insert_class_row(&db, "cd", 110, "p", "u", "a", "client_disconnect", None);
    insert_class_row(
        &db,
        "out",
        5,
        "p",
        "u",
        "a",
        "upstream_error",
        Some("http-5xx"),
    );

    // Act: window [100, 1000) has zero qualifying error rows.
    let breakdown = errors_by_class(&db, 100, 1000).expect("errors_by_class");

    // Assert
    assert!(breakdown.is_empty());
}

#[test]
fn errors_by_class_uses_ts_start_index() {
    // The breakdown must ride idx_requests_ts_start for its window range,
    // not degrade to a full table scan. If this ever fails, add a covering
    // index rather than accepting the scan.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    for i in 0..64 {
        insert_class_row(
            &db,
            &format!("e{i}"),
            100 + i,
            "p",
            "u",
            "a",
            "upstream_error",
            Some("http-5xx"),
        );
    }

    let plan: Vec<String> = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {ERRORS_BY_CLASS_SQL}"))
        .expect("prepare explain")
        .query_map([0_i64, 1000_i64], |row| row.get::<_, String>(3))
        .expect("query explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect explain");

    assert!(
        plan.iter().any(|d| d.contains("idx_requests_ts_start")),
        "breakdown query must use idx_requests_ts_start; plan was {plan:?}"
    );
}
