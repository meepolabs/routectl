use super::render_table;

fn rows(cells: &[&[&str]]) -> Vec<Vec<String>> {
    cells
        .iter()
        .map(|row| row.iter().map(|c| (*c).to_string()).collect())
        .collect()
}

#[test]
fn renders_nothing_for_no_rows() {
    assert_eq!(render_table(&[]), "");
}

#[test]
fn left_aligns_first_column_and_right_aligns_the_rest() {
    let cases: &[(&str, &[&[&str]], &str)] = &[
        (
            "cells narrower and wider than their header",
            &[&["lane", "n", "cost"], &["a", "12", "3"]],
            "lane   n  cost\na     12     3\n",
        ),
        (
            "short label and count under wide headers",
            &[&["provider", "reqs", "in"], &["paid", "1", "7"]],
            "provider  reqs  in\npaid         1   7\n",
        ),
    ];
    for (name, cells, expected) in cases {
        let out = render_table(&rows(cells));

        assert_eq!(out, *expected, "{name}");
    }
}

#[test]
fn trims_trailing_padding_from_a_short_single_column_row() {
    let table = rows(&[&["long-name"], &["x"]]);

    let out = render_table(&table);

    assert_eq!(out, "long-name\nx\n");
}
