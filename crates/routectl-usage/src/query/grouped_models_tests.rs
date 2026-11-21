// The served-model split of alias groups: which groupings carry it, what it
// sums to, and that it costs the read no statement. Split from
// `grouped_tests.rs` to keep each file under the size ceiling; `include!`d into
// the same `tests` module so the fixtures there stay in scope. All imports come
// from the host file -- do not add `use` lines here.

/// Two aliases. `al` is served by `m1` on two upstreams of `pa` and by `m2`
/// on `pb`, and holds one row that was never dispatched (no served model or
/// provider; its model label coalesces to the requested model). `bl` is
/// served by `m1` only.
fn seed_alias_models(db: &UsageDb) {
    for (id, alias, model, provider, upstream) in [
        ("a", "al", Some("m1"), Some("pa"), Some("u1")),
        ("b", "al", Some("m1"), Some("pa"), Some("u2")),
        ("c", "al", Some("m1"), Some("pa"), Some("u1")),
        ("d", "al", Some("m2"), Some("pb"), Some("u1")),
        ("e", "al", None, None, None),
        ("f", "bl", Some("m1"), Some("pa"), Some("u1")),
    ] {
        insert(
            db,
            &Fixture {
                request_id: id,
                alias,
                model,
                provider,
                upstream,
                ts_start: if id < "d" { 100 } else { 1_500 },
                ..Fixture::default()
            },
        );
    }
}

fn models(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs.iter().map(|(m, n)| ((*m).to_string(), *n)).collect()
}

#[test]
fn alias_groups_split_their_dispatched_requests_by_served_model() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_alias_models(&db);

    // Act
    let result = query(&mut db, &spec(GroupDim::Alias), unpriced, no_deadline()).expect("query");

    // Assert: the undispatched row is in the group's requests and in no
    // model's count, so the map sums to requests minus that row.
    let al = group(&result, "al");
    assert_eq!(al.metrics.requests, 5);
    assert_eq!(al.requests_by_model, Some(models(&[("m1", 3), ("m2", 1)])));
    let bl = group(&result, "bl");
    assert_eq!(bl.metrics.requests, 1);
    assert_eq!(bl.requests_by_model, Some(models(&[("m1", 1)])));
}

#[test]
fn only_the_alias_grouping_carries_requests_by_model() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_alias_models(&db);

    for group_by in GroupDim::ALL {
        // Act
        let result = query(&mut db, &spec(group_by), unpriced, no_deadline()).expect("query");

        // Assert: alias is the positive control that the fixture yields a map.
        assert!(!result.groups.is_empty(), "{group_by:?}");
        for g in &result.groups {
            assert_eq!(
                g.requests_by_model.is_some(),
                group_by == GroupDim::Alias,
                "{group_by:?} {}",
                g.label
            );
        }
    }
}

#[test]
fn a_series_leaves_the_alias_split_whole_window_and_off_the_buckets() {
    // Arrange: the rows straddle two buckets of a 1000 ms grid.
    let (_dir, mut db) = open_db();
    seed_alias_models(&db);
    let bucketed = QuerySpec {
        bucket: Some(BucketSpec {
            width_ms: 1_000,
            count: 10,
        }),
        ..spec(GroupDim::Alias)
    };

    // Act
    let plain = query(&mut db, &spec(GroupDim::Alias), unpriced, no_deadline()).expect("query");
    let with_series = query(&mut db, &bucketed, unpriced, no_deadline()).expect("query");

    // Assert: the groups carry the same whole-window split either way.
    assert_eq!(with_series.groups, plain.groups);
    assert!(group(&with_series, "al").requests_by_model.is_some());
    // Assert: no bucket's serialized figures name the split.
    let json = serde_json::to_value(&with_series).expect("serialize");
    let buckets = json["series"]["buckets"].as_array().expect("buckets");
    assert!(buckets.iter().any(|b| b["metrics"]["requests"] != 0));
    for bucket in buckets {
        assert!(
            bucket["metrics"].get("requests_by_model").is_none(),
            "{bucket}"
        );
        assert!(bucket.get("requests_by_model").is_none(), "{bucket}");
    }
}

#[test]
fn requests_by_model_never_reaches_the_serialized_group() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_alias_models(&db);
    let result = query(&mut db, &spec(GroupDim::Alias), unpriced, no_deadline()).expect("query");
    assert!(result.groups.iter().all(|g| g.requests_by_model.is_some()));

    // Act
    let json = serde_json::to_value(&result).expect("serialize");

    // Assert: a group serializes as exactly its label and metrics.
    for g in json["groups"].as_array().expect("groups") {
        let mut keys: Vec<&str> = g
            .as_object()
            .expect("group")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["label", "metrics"], "{g}");
    }
}

/// Run `act` on `db` with an authorizer counting every SELECT compiled on
/// that connection, and return the count.
fn selects_compiled(db: &mut UsageDb, act: impl FnOnce(&mut UsageDb)) -> usize {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    db.conn_mut()
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(ctx.action, AuthAction::Select) {
                seen.fetch_add(1, Ordering::Relaxed);
            }
            Authorization::Allow
        }))
        .expect("authorizer");
    act(db);
    db.conn_mut()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("clear authorizer");
    count.load(Ordering::Relaxed)
}

/// The SELECTs one grouped read of `spec` compiles on `db`.
fn read_selects(db: &mut UsageDb, spec: &QuerySpec) -> usize {
    selects_compiled(db, |db| {
        query(db, spec, unpriced, no_deadline()).expect("query");
    })
}

#[test]
fn the_alias_split_compiles_no_statement_the_other_groupings_do_not() {
    // Arrange: a window short enough to read as one slice on `db`, so every
    // statement of the read is compiled on the counted connection.
    let (_dir, mut db) = open_db();
    seed_alias_models(&db);
    let bucketed = |group_by| QuerySpec {
        bucket: Some(BucketSpec {
            width_ms: 1_000,
            count: 10,
        }),
        ..spec(group_by)
    };

    for (path, at) in [
        ("unbucketed", spec as fn(GroupDim) -> QuerySpec),
        ("bucketed", bucketed),
    ] {
        // Act
        let alias = read_selects(&mut db, &at(GroupDim::Alias));
        let others: Vec<usize> = [GroupDim::Model, GroupDim::Provider, GroupDim::Seat]
            .into_iter()
            .map(|group_by| read_selects(&mut db, &at(group_by)))
            .collect();

        // Assert: the counter sees the read's statements at all, and the
        // alias read compiles exactly as many as each other grouping's.
        assert!(alias > 0, "{path}");
        assert_eq!(others, [alias; 3], "{path}");
    }
}

#[test]
fn the_select_counter_counts_a_statement_added_to_the_read() {
    // Arrange: the positive control that one extra statement on the read's
    // connection moves the count the grouping comparison relies on.
    let (_dir, mut db) = open_db();
    seed_alias_models(&db);
    let base = read_selects(&mut db, &spec(GroupDim::Alias));

    // Act
    let with_extra = selects_compiled(&mut db, |db| {
        query(db, &spec(GroupDim::Alias), unpriced, no_deadline()).expect("query");
        db.conn()
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .expect("extra statement");
    });

    // Assert
    assert_eq!(with_extra, base + 1);
}
