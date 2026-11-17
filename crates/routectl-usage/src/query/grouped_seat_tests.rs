// The grouping vocabulary and the seat grouping. Split from `grouped_tests.rs`
// to keep each file under the size ceiling; `include!`d into the same `tests`
// module so the fixtures there stay in scope. All imports come from the host
// file -- do not add `use` lines here.

#[test]
fn group_dim_all_lists_every_grouping_in_display_order() {
    let tokens: Vec<&str> = GroupDim::ALL.iter().map(|dim| dim.as_str()).collect();

    assert_eq!(tokens, ["model", "alias", "provider", "seat"]);
}

#[test]
fn group_dim_token_round_trips_through_parse_and_serde() {
    for dim in GroupDim::ALL {
        let token = dim.as_str();

        let parsed = GroupDim::parse(token);
        let deserialized: GroupDim =
            serde_json::from_value(serde_json::Value::from(token)).expect("known token");

        assert_eq!(parsed, Some(dim), "parse({token})");
        assert_eq!(deserialized, dim, "serde({token})");
    }
}

#[test]
fn group_dim_unknown_token_fails_parse_and_serde() {
    for token in ["upstream", "Seat", "seats", "", "client"] {
        let deserialized = serde_json::from_value::<GroupDim>(serde_json::Value::from(token));

        assert_eq!(GroupDim::parse(token), None, "parse({token:?})");
        assert!(deserialized.is_err(), "serde({token:?})");
    }
}

#[test]
fn group_dim_label_reads_its_own_key_column() {
    let key = GroupKey {
        model: Some("m1".into()),
        provider: Some("pa".into()),
        upstream: Some("u1".into()),
        alias: "al".into(),
        provider_kind: Some("anthropic-api".into()),
        seat: Some("pa#one".into()),
    };
    let cases = [
        (GroupDim::Model, "m1"),
        (GroupDim::Alias, "al"),
        (GroupDim::Provider, "pa"),
        (GroupDim::Seat, "pa#one"),
    ];
    for (dim, expected) in cases {
        assert_eq!(dim.label(&key), expected, "{}", dim.as_str());
    }
}

#[test]
fn group_dim_label_of_a_null_column_is_unattributed() {
    let key = GroupKey {
        alias: "al".into(),
        ..GroupKey::default()
    };
    for dim in [GroupDim::Model, GroupDim::Provider, GroupDim::Seat] {
        assert_eq!(dim.label(&key), UNATTRIBUTED, "{}", dim.as_str());
    }
}

#[test]
fn seat_grouping_reads_the_persisted_seat_and_splits_one_provider() {
    // Arrange: one provider serving under two seats plus a pre-seat row, all
    // under one (alias, provider, model, upstream).
    let (_dir, mut db) = open_db();
    let rows = [
        ("s1", Some("pa"), 10),
        ("s2", Some("pa"), 20),
        ("s3", Some("pa#two"), 300),
        ("s4", None, 4_000),
    ];
    for (request_id, seat, input) in rows {
        insert(
            &db,
            &Fixture {
                request_id,
                seat,
                input_tokens: Some(input),
                ..Fixture::default()
            },
        );
    }

    // Act
    let by_seat = query(&mut db, &spec(GroupDim::Seat), unpriced, no_deadline()).expect("query");
    let by_provider =
        query(&mut db, &spec(GroupDim::Provider), unpriced, no_deadline()).expect("query");

    // Assert: three seat groups, one provider group, same totals.
    let labels: Vec<&str> = by_seat.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, [UNATTRIBUTED, "pa", "pa#two"]);
    assert_eq!(group(&by_seat, "pa").metrics.requests, 2);
    assert_eq!(group(&by_seat, "pa").metrics.input_tokens, 30);
    assert_eq!(group(&by_seat, "pa#two").metrics.input_tokens, 300);
    assert_eq!(group(&by_seat, UNATTRIBUTED).metrics.input_tokens, 4_000);
    assert_eq!(by_provider.groups.len(), 1);
    assert_eq!(by_seat.totals, by_provider.totals);
}

#[test]
fn seat_grouping_holds_on_the_series_path() {
    // Arrange: two seats of one provider in two buckets.
    let (_dir, mut db) = open_db();
    for (request_id, ts_start, seat) in [("b1", 100, "pa"), ("b2", 1_100, "pa#two")] {
        insert(
            &db,
            &Fixture {
                request_id,
                ts_start,
                seat: Some(seat),
                ..Fixture::default()
            },
        );
    }
    let bucketed = QuerySpec {
        bucket: Some(BucketSpec {
            width_ms: 1_000,
            count: 10,
        }),
        ..spec(GroupDim::Seat)
    };

    // Act
    let result = query(&mut db, &bucketed, unpriced, no_deadline()).expect("query");

    // Assert
    let labels: Vec<&str> = result.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, ["pa", "pa#two"]);
    let series = result.series.expect("series requested");
    assert_eq!(series.buckets[0].metrics.requests, 1);
    assert_eq!(series.buckets[1].metrics.requests, 1);
}
