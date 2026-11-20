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

/// Entry -> account for two own-credential entries, as the caller derives it
/// from config. `sub` (an OAuth entry), `relay` (forwarded) and `gone`
/// (unconfigured) are deliberately absent.
fn entry_accounts() -> BTreeMap<String, String> {
    [("keyed", "#keyed"), ("cloud", "#cloud")]
        .into_iter()
        .map(|(entry, account)| (entry.to_string(), account.to_string()))
        .collect()
}

fn mapped_spec(group_by: GroupDim) -> QuerySpec {
    QuerySpec {
        entry_accounts: entry_accounts(),
        ..spec(group_by)
    }
}

/// `(case, group_by, seat, provider, expected label)`.
type LabelRow = (
    &'static str,
    GroupDim,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
);

#[test]
fn group_label_takes_a_persisted_seat_then_the_entry_account_then_unattributed() {
    let rows: [LabelRow; 10] = [
        (
            "persisted seat wins over a mapped provider",
            GroupDim::Seat,
            Some("anthropic#work"),
            Some("keyed"),
            "anthropic#work",
        ),
        (
            "null seat on an api-key entry",
            GroupDim::Seat,
            None,
            Some("keyed"),
            "#keyed",
        ),
        (
            "null seat on a cloud entry",
            GroupDim::Seat,
            None,
            Some("cloud"),
            "#cloud",
        ),
        (
            "null seat on an oauth entry",
            GroupDim::Seat,
            None,
            Some("sub"),
            UNATTRIBUTED,
        ),
        (
            "null seat on a forwarded entry",
            GroupDim::Seat,
            None,
            Some("relay"),
            UNATTRIBUTED,
        ),
        (
            "null seat on an unconfigured entry",
            GroupDim::Seat,
            None,
            Some("gone"),
            UNATTRIBUTED,
        ),
        ("null provider", GroupDim::Seat, None, None, UNATTRIBUTED),
        ("model grouping", GroupDim::Model, None, Some("keyed"), "m1"),
        ("alias grouping", GroupDim::Alias, None, Some("keyed"), "al"),
        (
            "provider grouping",
            GroupDim::Provider,
            None,
            Some("keyed"),
            "keyed",
        ),
    ];
    for (case, group_by, seat, provider, expected) in rows {
        // Arrange
        let key = GroupKey {
            model: Some("m1".into()),
            provider: provider.map(str::to_string),
            alias: "al".into(),
            seat: seat.map(str::to_string),
            ..GroupKey::default()
        };

        // Act
        let label = mapped_spec(group_by).group_label(&key);

        // Assert
        assert_eq!(label, expected, "{case}");
    }
}

#[test]
fn seat_grouping_splits_unseated_rows_by_their_entry_account() {
    // Arrange: two own-credential entries, an OAuth entry's pre-seat row, a
    // forwarded entry and a seated row, each with its own input count.
    let (_dir, mut db) = open_db();
    let rows = [
        ("k1", Some("keyed"), None, 1),
        ("k2", Some("keyed"), None, 2),
        ("c1", Some("cloud"), None, 40),
        ("o1", Some("sub"), None, 300),
        ("f1", Some("relay"), None, 5_000),
        ("s1", Some("keyed"), Some("anthropic#work"), 60_000),
    ];
    for (request_id, provider, seat, input) in rows {
        insert(
            &db,
            &Fixture {
                request_id,
                provider,
                seat,
                input_tokens: Some(input),
                ..Fixture::default()
            },
        );
    }

    // Act
    let mapped = query(
        &mut db,
        &mapped_spec(GroupDim::Seat),
        unpriced,
        no_deadline(),
    )
    .expect("query");
    let unmapped = query(&mut db, &spec(GroupDim::Seat), unpriced, no_deadline()).expect("query");

    // Assert
    let labels: Vec<&str> = mapped.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, ["#cloud", "#keyed", UNATTRIBUTED, "anthropic#work"]);
    assert_eq!(group(&mapped, "#keyed").metrics.requests, 2);
    assert_eq!(group(&mapped, "#keyed").metrics.input_tokens, 3);
    assert_eq!(group(&mapped, "#cloud").metrics.input_tokens, 40);
    assert_eq!(group(&mapped, UNATTRIBUTED).metrics.input_tokens, 5_300);
    assert_eq!(
        group(&mapped, "anthropic#work").metrics.input_tokens,
        60_000
    );
    assert_eq!(mapped.totals, unmapped.totals);
    let unmapped_labels: Vec<&str> = unmapped.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(
        unmapped_labels,
        [UNATTRIBUTED, "anthropic#work"],
        "control: without the map every unseated row is unattributed"
    );
}

#[test]
fn the_entry_map_leaves_other_groupings_and_the_series_unchanged() {
    // Arrange: unseated rows on a mapped and an unmapped entry, two buckets.
    let (_dir, mut db) = open_db();
    for (request_id, ts_start, provider) in [("a", 100, "keyed"), ("b", 1_100, "sub")] {
        insert(
            &db,
            &Fixture {
                request_id,
                ts_start,
                provider: Some(provider),
                ..Fixture::default()
            },
        );
    }
    let bucket = Some(BucketSpec {
        width_ms: 1_000,
        count: 3,
    });
    for group_by in [
        GroupDim::Model,
        GroupDim::Alias,
        GroupDim::Provider,
        GroupDim::Seat,
    ] {
        let plain = QuerySpec {
            bucket,
            ..spec(group_by)
        };
        let with_map = QuerySpec {
            entry_accounts: entry_accounts(),
            ..plain.clone()
        };

        // Act
        let without = query(&mut db, &plain, unpriced, no_deadline()).expect("query");
        let with = query(&mut db, &with_map, unpriced, no_deadline()).expect("query");

        // Assert
        assert_eq!(with.series, without.series, "{}", group_by.as_str());
        assert_eq!(with.totals, without.totals, "{}", group_by.as_str());
        if group_by == GroupDim::Seat {
            assert_ne!(
                with.groups, without.groups,
                "positive control: the map does relabel the seat grouping"
            );
        } else {
            assert_eq!(with.groups, without.groups, "{}", group_by.as_str());
        }
    }
}
