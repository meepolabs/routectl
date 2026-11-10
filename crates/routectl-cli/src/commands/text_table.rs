//! Plain-text column alignment shared by the CLI's tabular renders.

/// Left-align column 0, right-align the rest, padded to the widest cell in
/// each column. ASCII spaces only; trailing spaces are trimmed from every
/// line. Callers must pass rows of uniform column count; a ragged row renders
/// misaligned (never panics).
pub fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(std::vec::Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            if i == 0 {
                line.push_str(&format!("{cell:<width$}", width = widths[i]));
            } else {
                line.push_str(&format!("{cell:>width$}", width = widths[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[path = "text_table_tests.rs"]
mod tests;
