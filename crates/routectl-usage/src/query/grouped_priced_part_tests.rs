// A row priced only in part: its covered cost reaches `cost_usd` and the
// metric set reads `partial`, with the row's missing dimensions listed; and
// how completely a metric set's API-equivalent value covers its subscription
// rows (`equivalent_status`).
// `include!`d into `grouped_tests.rs`, so the fixtures there stay in scope.
// All imports come from the host -- do not add `use` lines here.

const PART_LABEL: &str = "cache write (unknown lifetime)";

fn seed_part_rows(db: &UsageDb) {
    for (id, model, upstream) in [("a", "m1", "u-part"), ("b", "m2", "u-full")] {
        insert(
            db,
            &Fixture {
                request_id: id,
                model: Some(model),
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }
}

fn part_pricer(row: &AggRow) -> RowCost {
    match row.key.upstream.as_deref() {
        Some("u-part") => RowCost::PricedPart(2.5, vec![PART_LABEL.to_string()]),
        _ => RowCost::Priced(1.25),
    }
}

#[test]
fn a_priced_part_row_reads_partial_with_its_covered_cost() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_part_rows(&db);

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), part_pricer, no_deadline()).expect("query");

    // Assert: alone in its group, the row still reads partial.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Partial);
    assert_eq!(m.cost_usd, Some(2.5));
    assert_eq!(m.missing_price_dimensions, vec![PART_LABEL]);
    assert_eq!(m.equivalent_cost_usd, None);
}

#[test]
fn a_group_of_one_fully_priced_row_stays_priced() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_part_rows(&db);

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), part_pricer, no_deadline()).expect("query");

    // Assert
    let m = &group(&result, "m2").metrics;
    assert_eq!(m.cost_status, CostStatus::Priced);
    assert_eq!(m.cost_usd, Some(1.25));
    assert!(m.missing_price_dimensions.is_empty());
}

#[test]
fn totals_over_a_priced_part_row_read_partial_with_the_priced_subtotal() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_part_rows(&db);

    // Act: both rows land in one alias group.
    let result = query(&mut db, &spec(GroupDim::Alias), part_pricer, no_deadline()).expect("query");

    // Assert: the covered cost plus the priced row's, never more.
    for (name, m) in [
        ("group", &group(&result, "al").metrics),
        ("totals", &result.totals),
    ] {
        assert_eq!(m.cost_status, CostStatus::Partial, "{name}");
        assert_eq!(m.cost_usd, Some(3.75), "{name}");
        assert_eq!(m.missing_price_dimensions, vec![PART_LABEL], "{name}");
    }
}

/// Seed one row per `(upstream, model, cost)` entry and price each row with
/// the cost its upstream names.
fn query_equivalent_rows(
    rows: &[(&'static str, &'static str, RowCost)],
    group_by: GroupDim,
) -> QueryResult {
    let (_dir, mut db) = open_db();
    let ids = ["a", "b", "c", "d"];
    for ((upstream, model, _), id) in rows.iter().zip(ids) {
        insert(
            &db,
            &Fixture {
                request_id: id,
                model: Some(model),
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }
    let pricer = |row: &AggRow| {
        rows.iter()
            .find(|(upstream, _, _)| row.key.upstream.as_deref() == Some(*upstream))
            .map(|(_, _, cost)| cost.clone())
            .expect("every seeded upstream has a cost")
    };
    query(&mut db, &spec(group_by), pricer, no_deadline()).expect("query")
}

fn sub(equivalent: Option<f64>, missing: &[&str]) -> RowCost {
    RowCost::Subscription(
        equivalent,
        missing.iter().map(ToString::to_string).collect(),
    )
}

#[test]
fn equivalent_status_reads_whether_every_subscription_row_was_valued_in_full() {
    let cases = [
        (
            "no subscription rows",
            vec![
                ("u-a", "m1", RowCost::Priced(1.0)),
                ("u-b", "m1", RowCost::Unpriced(vec!["input".to_string()])),
            ],
            EquivalentStatus::NoSubscription,
            None,
        ),
        (
            "all subscription rows fully priced",
            vec![
                ("u-a", "m1", sub(Some(2.0), &[])),
                ("u-b", "m1", sub(Some(3.0), &[])),
            ],
            EquivalentStatus::Complete,
            Some(5.0),
        ),
        (
            "None beside priced",
            vec![
                ("u-a", "m1", sub(None, &["input"])),
                ("u-b", "m1", sub(Some(2.0), &[])),
            ],
            EquivalentStatus::Partial,
            Some(2.0),
        ),
        (
            "one covered-part row",
            vec![("u-a", "m1", sub(Some(2.5), &[PART_LABEL]))],
            EquivalentStatus::Partial,
            Some(2.5),
        ),
    ];
    for (name, rows, expected, equivalent) in cases {
        // Act: every row lands in one model group.
        let result = query_equivalent_rows(&rows, GroupDim::Model);

        // Assert
        let m = &group(&result, "m1").metrics;
        assert_eq!(m.equivalent_status, expected, "{name}");
        assert_eq!(m.equivalent_cost_usd, equivalent, "{name}");
    }
}

#[test]
fn totals_equivalent_status_folds_from_the_group_accumulators() {
    let cases = [
        (
            "one partial group",
            vec![
                ("u-a", "m1", sub(None, &["input"])),
                ("u-b", "m2", sub(Some(2.0), &[])),
            ],
            [EquivalentStatus::Partial, EquivalentStatus::Complete],
            EquivalentStatus::Partial,
        ),
        (
            "complete beside no subscription",
            vec![
                ("u-a", "m1", RowCost::Priced(1.0)),
                ("u-b", "m2", sub(Some(2.0), &[])),
            ],
            [EquivalentStatus::NoSubscription, EquivalentStatus::Complete],
            EquivalentStatus::Complete,
        ),
        (
            "no subscription anywhere",
            vec![
                ("u-a", "m1", RowCost::Priced(1.0)),
                ("u-b", "m2", RowCost::Priced(2.0)),
            ],
            [
                EquivalentStatus::NoSubscription,
                EquivalentStatus::NoSubscription,
            ],
            EquivalentStatus::NoSubscription,
        ),
    ];
    for (name, rows, groups, expected) in cases {
        // Act: one row per model group.
        let result = query_equivalent_rows(&rows, GroupDim::Model);

        // Assert: each group as built, then the totals over them.
        for (label, status) in ["m1", "m2"].into_iter().zip(groups) {
            assert_eq!(
                group(&result, label).metrics.equivalent_status,
                status,
                "{name} {label}"
            );
        }
        assert_eq!(result.totals.equivalent_status, expected, "{name} totals");
    }
}

#[test]
fn the_equivalent_status_tokens_are_the_wire_tokens() {
    for (status, token) in [
        (EquivalentStatus::NoSubscription, "none"),
        (EquivalentStatus::Complete, "complete"),
        (EquivalentStatus::Partial, "partial"),
    ] {
        assert_eq!(serde_json::to_value(status).expect("serializes"), token);
        assert_eq!(status.as_str(), token);
    }
}

#[test]
fn an_unpriced_passthrough_step_leaves_an_alias_equivalent_complete() {
    let priced_steps = [
        ("u-a", "m1", sub(Some(2.0), &[])),
        ("u-b", "m1", sub(Some(3.0), &[])),
        (
            "u-c",
            "m2",
            RowCost::Unpriced(vec!["input".to_string(), "output".to_string()]),
        ),
    ];
    let mut with_sub_gap = priced_steps.to_vec();
    with_sub_gap.push(("u-d", "m1", sub(Some(1.0), &["input"])));
    let cases = [
        (
            "passthrough gap only",
            priced_steps.to_vec(),
            EquivalentStatus::Complete,
            Some(5.0),
        ),
        (
            "passthrough gap and a subscription gap",
            with_sub_gap,
            EquivalentStatus::Partial,
            Some(6.0),
        ),
    ];
    for (name, rows, expected, equivalent) in cases {
        // Act: both steps land in the one alias group.
        let result = query_equivalent_rows(&rows, GroupDim::Alias);

        // Assert: the passthrough gap is still listed as a missing price.
        let m = &group(&result, "al").metrics;
        assert_eq!(m.equivalent_status, expected, "{name}");
        assert_eq!(m.equivalent_cost_usd, equivalent, "{name}");
        assert!(
            m.missing_prices
                .iter()
                .any(|gap| gap.model == "m2" && gap.dimension == "input"),
            "{name}: {:?}",
            m.missing_prices
        );
    }
}
