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
    let table = rows(&[&["lane", "n", "cost"], &["a", "12", "3"]]);

    let out = render_table(&table);

    assert_eq!(out, "lane   n  cost\na     12     3\n");
}

#[test]
fn trims_trailing_padding_from_a_short_single_column_row() {
    let table = rows(&[&["long-name"], &["x"]]);

    let out = render_table(&table);

    assert_eq!(out, "long-name\nx\n");
}
