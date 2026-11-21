// Alias step shares on `/status/query`: every alias group's requests split
// over the top-level steps of the chain its wire name resolves to, plus the
// served-off-chain and never-dispatched remainders. `include!`d into
// `query_tests.rs`; all imports come from the host (its `use super::*`); do not
// add `use` lines here.

/// Model nicknames the step configs name; `m-x` is in no chain.
const STEP_MODELS: [&str; 5] = ["m-a", "m-b", "m-c", "m-d", "m-x"];

/// A config naming [`STEP_MODELS`] and `aliases`.
fn steps_router(aliases: &[(&str, &[&str])]) -> Router {
    let mut config = Config::default();
    for nickname in STEP_MODELS {
        config.models.insert(
            nickname.to_string(),
            ModelEntry::new(format!("p-{nickname}"), format!("u-{nickname}")),
        );
    }
    for (key, targets) in aliases {
        let value = routectl_router::AliasValue::Chain(
            targets.iter().map(|target| (*target).to_string()).collect(),
        );
        config.aliases.insert((*key).to_string(), value);
    }
    Router::new(Arc::new(config))
}

/// Every precedence step the resolver walks: an exact key, a glob, the
/// `default` catch-all, and a chain nesting another alias (`mid`) whose
/// expansion also names a later top-level step (`m-b`).
fn precedence_steps_router() -> Router {
    steps_router(&[
        ("fast", &["m-a", "m-b"]),
        ("claude-*", &["m-b", "m-c"]),
        ("default", &["m-c"]),
        ("top", &["m-a", "mid", "m-b"]),
        ("mid", &["m-b", "m-d"]),
    ])
}

/// One ledger row per `(alias, served model or None)` at `ts`; a served row
/// names its model's provider, a row never dispatched names none.
fn seed_alias_rows(path: &Path, ts: i64, rows: &[(&str, Option<&str>)]) {
    let db = open(path).expect("open ledger");
    for (i, &(alias, model)) in rows.iter().enumerate() {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens) \
                 VALUES (?1, ?1, ?2, 'anthropic', ?3, ?3, ?4, ?5, ?6, 0, ?7, 10, 0, 0, 1, 0, \
                 10, 5)",
                rusqlite::params![
                    ts,
                    format!("{alias}-{ts}-{i}"),
                    alias,
                    model,
                    model.map(|m| format!("p-{m}")),
                    model.map(|m| format!("u-{m}")),
                    if model.is_some() {
                        "ok"
                    } else {
                        "client_disconnect"
                    },
                ],
            )
            .expect("seed alias row");
    }
}

/// `count` copies of `row`.
fn times<'a>(count: usize, row: (&'a str, Option<&'a str>)) -> Vec<(&'a str, Option<&'a str>)> {
    vec![row; count]
}

/// The precedence fixture's rows: each wire name served on its chain, off it,
/// and not at all.
fn precedence_rows() -> Vec<(&'static str, Option<&'static str>)> {
    [
        times(2, ("fast", Some("m-a"))),
        times(1, ("fast", Some("m-b"))),
        times(1, ("fast", Some("m-x"))),
        times(1, ("fast", None)),
        times(2, ("claude-opus-9", Some("m-b"))),
        times(1, ("claude-opus-9", Some("m-c"))),
        times(1, ("claude-opus-9", Some("m-a"))),
        times(1, ("claude-opus-9", None)),
        times(2, ("no-such-wire", Some("m-c"))),
        times(1, ("no-such-wire", Some("m-d"))),
        times(1, ("top", Some("m-a"))),
        times(2, ("top", Some("m-b"))),
        times(1, ("top", Some("m-d"))),
        times(1, ("top", Some("m-x"))),
        times(1, ("top", None)),
    ]
    .concat()
}

/// QUERY `window` by alias over `router` and the ledger at `path`.
async fn alias_groups(path: PathBuf, router: Router, window: &str) -> Value {
    let state = state_over_router(path, router);
    let body = format!(r#"{{"window":"{window}","group_by":"alias"}}"#);
    let (status, json) = send(state, QUERY_METHOD, &body).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(json["unavailable"].is_null(), "{json}");
    json
}

/// A group's steps as `(step, kind, requests)`.
fn step_rows(group: &Value) -> Vec<(String, String, i64)> {
    group["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("steps array: {group}"))
        .iter()
        .map(|step| {
            (
                step["step"].as_str().expect("step").to_string(),
                step["kind"].as_str().expect("kind").to_string(),
                step["requests"].as_i64().expect("requests"),
            )
        })
        .collect()
}

fn owned_steps(rows: &[(&str, &str, i64)]) -> Vec<(String, String, i64)> {
    rows.iter()
        .map(|&(step, kind, requests)| (step.to_string(), kind.to_string(), requests))
        .collect()
}

/// On exact, glob-matched, default-matched and nested-alias groups, the step
/// shares plus `off_chain` plus `not_dispatched` equal the group's requests,
/// and each group splits as its resolved chain dictates.
#[tokio::test]
async fn alias_step_shares_and_remainders_sum_to_the_group_requests() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_alias_rows(&path, now, &precedence_rows());
    type Expected<'a> = (&'a str, i64, &'a [(&'a str, &'a str, i64)], i64, i64);
    let rows: [Expected<'_>; 4] = [
        ("fast", 5, &[("m-a", "model", 2), ("m-b", "model", 1)], 1, 1),
        (
            "claude-opus-9",
            5,
            &[("m-b", "model", 2), ("m-c", "model", 1)],
            1,
            1,
        ),
        ("no-such-wire", 3, &[("m-c", "model", 2)], 1, 0),
        (
            "top",
            6,
            &[
                ("m-a", "model", 1),
                ("mid", "alias", 3),
                ("m-b", "model", 0),
            ],
            1,
            1,
        ),
    ];

    // Act
    let json = alias_groups(path, precedence_steps_router(), "today").await;

    // Assert
    assert_eq!(
        json["data"]["groups"].as_array().expect("groups").len(),
        rows.len(),
        "{json}"
    );
    for (label, requests, steps, off_chain, not_dispatched) in rows {
        let group = group_by_label(&json, label);
        assert_eq!(group["metrics"]["requests"], requests, "{label}: {group}");
        assert_eq!(step_rows(group), owned_steps(steps), "{label}: {group}");
        assert_eq!(group["off_chain"], off_chain, "{label}: {group}");
        assert_eq!(group["not_dispatched"], not_dispatched, "{label}: {group}");
        let on_chain: i64 = step_rows(group).iter().map(|(_, _, n)| n).sum();
        assert_eq!(
            on_chain + off_chain + not_dispatched,
            requests,
            "{label}: the invariant"
        );
    }
}

/// A nested alias step reads `kind: "alias"` and owns every served model of
/// its expansion, including one a later top-level step also names: a model
/// counts at its first chain position.
#[tokio::test]
async fn a_nested_alias_step_owns_its_expansions_served_models() {
    // Arrange: `top` = [m-a, mid, m-b] with mid = [m-b, m-d].
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_alias_rows(
        &path,
        now,
        &[
            times(2, ("top", Some("m-b"))),
            times(1, ("top", Some("m-d"))),
            times(4, ("top", Some("m-a"))),
        ]
        .concat(),
    );

    // Act
    let json = alias_groups(path, precedence_steps_router(), "today").await;

    // Assert
    let group = group_by_label(&json, "top");
    assert_eq!(
        step_rows(group),
        owned_steps(&[
            ("m-a", "model", 4),
            ("mid", "alias", 3),
            ("m-b", "model", 0)
        ]),
        "{group}"
    );
    assert_eq!(group["off_chain"], 0, "{group}");
    assert_eq!(group["not_dispatched"], 0, "{group}");
}

/// A model served under the alias but absent from its chain counts in
/// `off_chain`, beside an on-chain model that counts on its step.
#[tokio::test]
async fn a_model_served_outside_the_chain_counts_off_chain() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_alias_rows(
        &path,
        now,
        &[
            times(3, ("fast", Some("m-x"))),
            times(1, ("fast", Some("m-a"))),
        ]
        .concat(),
    );

    // Act
    let json = alias_groups(path, precedence_steps_router(), "today").await;

    // Assert
    let group = group_by_label(&json, "fast");
    assert_eq!(group["off_chain"], 3, "{group}");
    assert_eq!(
        step_rows(group),
        owned_steps(&[("m-a", "model", 1), ("m-b", "model", 0)]),
        "{group}"
    );
}

/// A label that resolves to no chain, and one whose chain nests past the
/// router's depth bound, carry `steps: []` with every served request off the
/// chain; a resolving label in the same answer keeps its steps.
#[tokio::test]
async fn a_label_with_no_chain_carries_no_steps() {
    // Arrange: no `default`, so `nowhere` resolves to nothing; `loop` cycles.
    let router = steps_router(&[
        ("fast", &["m-a"]),
        ("loop", &["round"]),
        ("round", &["loop"]),
    ]);
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_alias_rows(
        &path,
        now,
        &[
            times(2, ("nowhere", Some("m-a"))),
            times(1, ("nowhere", None)),
            times(3, ("loop", Some("m-b"))),
            times(1, ("fast", Some("m-a"))),
        ]
        .concat(),
    );

    // Act
    let json = alias_groups(path, router, "today").await;

    // Assert
    let rows: [(&str, i64, i64); 2] = [("nowhere", 2, 1), ("loop", 3, 0)];
    for (label, off_chain, not_dispatched) in rows {
        let group = group_by_label(&json, label);
        assert_eq!(group["steps"], serde_json::json!([]), "{label}: {group}");
        assert_eq!(group["off_chain"], off_chain, "{label}: {group}");
        assert_eq!(group["not_dispatched"], not_dispatched, "{label}: {group}");
    }
    let fast = group_by_label(&json, "fast");
    assert_eq!(
        step_rows(fast),
        owned_steps(&[("m-a", "model", 1)]),
        "{fast}"
    );
}

/// The shares count only the selected range: rows before today's window are
/// absent from a `today` answer and present in an `all` answer over the same
/// ledger.
#[tokio::test]
async fn alias_step_shares_count_the_selected_range_only() {
    // Arrange: three days ago is before today's start in every zone.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_alias_rows(&path, now, &[("fast", Some("m-a"))]);
    seed_alias_rows(
        &path,
        now - 3 * 86_400_000,
        &[
            times(5, ("fast", Some("m-a"))),
            times(2, ("fast", Some("m-x"))),
            times(1, ("fast", None)),
        ]
        .concat(),
    );
    let rows: [(&str, i64, i64, i64); 2] = [("today", 1, 0, 0), ("all", 6, 2, 1)];

    for (window, on_step, off_chain, not_dispatched) in rows {
        // Act
        let json = alias_groups(path.clone(), precedence_steps_router(), window).await;

        // Assert
        let group = group_by_label(&json, "fast");
        assert_eq!(
            step_rows(group),
            owned_steps(&[("m-a", "model", on_step), ("m-b", "model", 0)]),
            "{window}: {group}"
        );
        assert_eq!(group["off_chain"], off_chain, "{window}: {group}");
        assert_eq!(group["not_dispatched"], not_dispatched, "{window}: {group}");
    }
}

/// The alias group's wire shape: `steps` entries carry exactly `step`, `kind`
/// and `requests`, and `off_chain` / `not_dispatched` sit beside `label` and
/// `metrics`; the per-model counts the split reads stay off the wire.
#[test]
fn alias_group_wire_shape_pins_the_step_tokens() {
    // Arrange
    let pricer = state_over_router(PathBuf::new(), precedence_steps_router())
        .pricer()
        .pin();
    let metrics = QueryMetrics {
        requests: 7,
        ..QueryMetrics::default()
    };
    let read = QueryRead {
        result: QueryResult {
            groups: vec![QueryGroup {
                label: "top".to_string(),
                metrics: metrics.clone(),
                requests_by_model: Some(std::collections::BTreeMap::from([
                    ("m-a".to_string(), 1),
                    ("m-d".to_string(), 2),
                    ("m-x".to_string(), 3),
                ])),
            }],
            totals: metrics.clone(),
            series: None,
        },
        earliest_ms: None,
    };
    let identities = AccountIdentities::from_config(&Config::default());

    // Act
    let payload = name_groups(read, GroupDim::Alias, &identities, &pricer);

    // Assert
    let metrics = serde_json::to_value(&metrics).unwrap();
    let expected = serde_json::json!({
        "groups": [{
            "label": "top",
            "steps": [
                {"step": "m-a", "kind": "model", "requests": 1},
                {"step": "mid", "kind": "alias", "requests": 2},
                {"step": "m-b", "kind": "model", "requests": 0},
            ],
            "off_chain": 3,
            "not_dispatched": 1,
            "metrics": metrics,
        }],
        "totals": metrics,
        "series": null,
        "earliest_ms": null,
    });
    assert_eq!(serde_json::to_value(&payload).unwrap(), expected);
}
