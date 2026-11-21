// The `model` request filter on `/status/query`: the groups, totals, error
// classes, cache-write pricing and timing percentiles of a filtered read
// describe that model's rows alone, a request never dispatched counts under
// the model it asked for, and a blank or misspelled filter is refused before
// the ledger is opened. `include!`d into `query_tests.rs`; all imports come
// from the host (its `use super::*`); do not add `use` lines here.

/// One row of the model-filter ledger.
struct FilterRow {
    request_id: &'static str,
    requested_model: &'static str,
    /// `None` with `provider: None` is a request never dispatched.
    model: Option<&'static str>,
    provider: Option<&'static str>,
    outcome: &'static str,
    class: Option<&'static str>,
    latency_ms: i64,
    cache_write_tokens: Option<i64>,
    cache_write_lifetimes: Option<&'static str>,
}

/// `m1` served by `p` (a success whose write has a lifetime, an error), a
/// request for `m1` never dispatched, and `m2` served by `q` (an error) and by
/// `p` (a success whose write has no lifetime, which leaves any metric set it
/// joins partially priced).
const FILTER_ROWS: [FilterRow; 5] = [
    FilterRow {
        request_id: "m1-ok",
        requested_model: "asked",
        model: Some("m1"),
        provider: Some("p"),
        outcome: "ok",
        class: None,
        latency_ms: 100,
        cache_write_tokens: Some(10),
        cache_write_lifetimes: Some(r#"{"300":10}"#),
    },
    FilterRow {
        request_id: "m1-error",
        requested_model: "asked",
        model: Some("m1"),
        provider: Some("p"),
        outcome: "upstream_error",
        class: Some("timeout"),
        latency_ms: 200,
        cache_write_tokens: None,
        cache_write_lifetimes: None,
    },
    FilterRow {
        request_id: "m1-undispatched",
        requested_model: "m1",
        model: None,
        provider: None,
        outcome: "gate_blocked",
        class: Some("rate-limited"),
        latency_ms: 1,
        cache_write_tokens: None,
        cache_write_lifetimes: None,
    },
    FilterRow {
        request_id: "m2-error",
        requested_model: "m1",
        model: Some("m2"),
        provider: Some("q"),
        outcome: "upstream_error",
        class: Some("http-5xx"),
        latency_ms: 900,
        cache_write_tokens: None,
        cache_write_lifetimes: None,
    },
    FilterRow {
        request_id: "m2-ok",
        requested_model: "asked",
        model: Some("m2"),
        provider: Some("p"),
        outcome: "ok",
        class: None,
        latency_ms: 800,
        cache_write_tokens: Some(30),
        cache_write_lifetimes: None,
    },
];

fn seed_filter_rows(path: &Path) {
    let db = open(path).expect("open ledger");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    for row in &FILTER_ROWS {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens, resolved_class, cache_write_tokens, \
                 cache_write_lifetimes) \
                 VALUES (?1, ?1, ?2, 'anthropic', ?3, 'a', ?4, ?5, 'u', 0, ?6, \
                 ?7, 0, 0, 1, 0, 100, 20, ?8, ?9, ?10)",
                rusqlite::params![
                    now,
                    row.request_id,
                    row.requested_model,
                    row.model,
                    row.provider,
                    row.outcome,
                    row.latency_ms,
                    row.class,
                    row.cache_write_tokens,
                    row.cache_write_lifetimes,
                ],
            )
            .expect("seed row");
    }
}

const M1_BODY: &str = r#"{"window":"today","group_by":"provider","model":"m1"}"#;
const ALL_MODELS_BODY: &str = r#"{"window":"today","group_by":"provider"}"#;

/// One query's response over the model-filter ledger, priced by a table that
/// prices `u`'s input, output and 5-minute writes.
async fn filter_query(body: &str) -> Value {
    let dir = TempDir::new().unwrap();
    let ledger = dir.path().join("usage.db");
    seed_filter_rows(&ledger);
    rename_in(
        &dir.path().join("prices.json"),
        &table_with_5m_write(),
        1_000,
    );
    let (state, _swap) = priced_state(dir.path(), ledger);
    let (status, json) = send(state, QUERY_METHOD, body).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

fn group_labels(json: &Value) -> Vec<&str> {
    json["data"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| g["label"].as_str().expect("label"))
        .collect()
}

#[tokio::test]
async fn a_model_filtered_query_describes_that_models_rows_alone() {
    // Arrange: the unfiltered answer is the positive control; every figure
    // checked below differs there, so a leaked m2 row would show.
    let filtered = filter_query(M1_BODY).await;
    let unfiltered = filter_query(ALL_MODELS_BODY).await;
    let p = |json: &Value| group_by_label(json, "p")["metrics"].clone();
    let totals = |json: &Value| json["data"]["totals"].clone();
    // (read, figure under the m1 filter, the same figure unfiltered)
    let cases: [(&str, Value, Value); 5] = [
        (
            "groups",
            json!(group_labels(&filtered)),
            json!(group_labels(&unfiltered)),
        ),
        (
            "totals",
            totals(&filtered)["requests"].clone(),
            totals(&unfiltered)["requests"].clone(),
        ),
        (
            "error classes",
            totals(&filtered)["errors_by_class"].clone(),
            totals(&unfiltered)["errors_by_class"].clone(),
        ),
        (
            "cache lifetimes",
            json!([
                p(&filtered)["cache_write_tokens"],
                p(&filtered)["cost_status"]
            ]),
            json!([
                p(&unfiltered)["cache_write_tokens"],
                p(&unfiltered)["cost_status"]
            ]),
        ),
        (
            "timing percentiles",
            json!([
                totals(&filtered)["latency_p99_ms"],
                p(&filtered)["latency_p99_ms"]
            ]),
            json!([
                totals(&unfiltered)["latency_p99_ms"],
                p(&unfiltered)["latency_p99_ms"]
            ]),
        ),
    ];
    let expected: [(&str, Value, Value); 5] = [
        (
            "groups",
            json!(["(unattributed)", "p"]),
            json!(["(unattributed)", "p", "q"]),
        ),
        ("totals", json!(3), json!(5)),
        (
            "error classes",
            json!({"rate-limited": 1, "timeout": 1}),
            json!({"http-5xx": 1, "rate-limited": 1, "timeout": 1}),
        ),
        (
            "cache lifetimes",
            json!([10, "priced"]),
            json!([40, "partial"]),
        ),
        ("timing percentiles", json!([200, 200]), json!([900, 800])),
    ];

    for ((read, got_m1, got_all), (_, want_m1, want_all)) in cases.into_iter().zip(expected) {
        // Assert
        assert_eq!(got_m1, want_m1, "{read}: m1 filter");
        assert_eq!(got_all, want_all, "{read}: unfiltered control");
    }
}

#[tokio::test]
async fn errors_by_class_sums_to_errors_under_a_model_filter() {
    // Act
    let json = filter_query(M1_BODY).await;
    let data = &json["data"];

    // Assert: on every group and the totals.
    let sets = data["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| &g["metrics"])
        .chain(std::iter::once(&data["totals"]));
    for metrics in sets {
        let class_sum: i64 = metrics["errors_by_class"]
            .as_object()
            .expect("errors_by_class")
            .values()
            .map(|count| count.as_i64().expect("count"))
            .sum();
        assert_eq!(Some(class_sum), metrics["errors"].as_i64(), "{metrics}");
    }
    assert_eq!(data["totals"]["errors"], 2, "the filter kept m1's errors");
}

#[tokio::test]
async fn a_request_never_dispatched_counts_under_the_model_it_asked_for() {
    // Act
    let m1 = filter_query(M1_BODY).await;
    let asked = filter_query(r#"{"window":"today","group_by":"provider","model":"asked"}"#).await;

    // Assert: the undispatched request for m1 is m1's; a dispatched row asked
    // as m1 but served by m2 is not, and a served row is never matched by the
    // name it was requested under.
    let undispatched = group_by_label(&m1, UNATTRIBUTED);
    assert_eq!(undispatched["metrics"]["requests"], 1, "{m1}");
    assert_eq!(group_by_label(&m1, "p")["metrics"]["requests"], 2, "{m1}");
    assert_eq!(asked["data"]["totals"]["requests"], 0, "{asked}");
}

#[tokio::test]
async fn a_blank_model_or_an_unknown_key_is_refused_before_the_ledger_is_opened() {
    // Arrange: no ledger file, so a request that reached the read answers
    // 200 with an unavailable panel; only the parse answers 400.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("absent.db");
    let bodies = [
        (
            "blank model",
            r#"{"window":"today","group_by":"provider","model":"  "}"#,
        ),
        (
            "empty model",
            r#"{"window":"today","group_by":"provider","model":""}"#,
        ),
        (
            "unknown key",
            r#"{"window":"today","group_by":"provider","models":"m1"}"#,
        ),
    ];
    let (control, read) = send(state_with_ledger(path.clone()), QUERY_METHOD, M1_BODY).await;
    assert_eq!(
        control,
        StatusCode::OK,
        "a valid model body reaches the read"
    );
    assert_eq!(read["unavailable"], codes::NO_DATA, "{read}");

    for (case, body) in bodies {
        // Act
        let (status, json) = send(state_with_ledger(path.clone()), QUERY_METHOD, body).await;

        // Assert
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {json}");
        assert_eq!(json["error"]["code"], INVALID_QUERY, "{case}");
        assert!(
            json.get("unavailable").is_none(),
            "{case}: reached the read"
        );
    }
}
