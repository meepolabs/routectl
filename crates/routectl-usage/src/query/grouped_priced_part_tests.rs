// A row priced only in part: its covered cost reaches `cost_usd` and the
// metric set reads `partial`, with the row's missing dimensions listed.
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
