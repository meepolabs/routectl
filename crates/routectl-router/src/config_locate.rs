//! Locate the 1-based source line of a dotted TOML key path in raw config
//! text, so a semantic validation error can point the operator at the line
//! that produced it.
//!
//! Uses `toml_edit`'s span-retaining immutable [`Document`] parse. A mutable
//! `DocumentMut` despans on parse and would lose the byte ranges this needs.

use toml_edit::{Document, TableLike};

/// Return the 1-based line number of the dotted key `dotted` within
/// `raw_text`, or `None` when the text does not parse, the path is absent,
/// or the located key/item carries no span.
///
/// Segments are split on `.` and matched as bare keys. A path segment that
/// does not match a table or key (for example a provider name quoted in the
/// TOML because it contains a `.`) yields `None`, and the caller falls back
/// to the plain message rather than reporting a wrong or missing line.
pub fn locate_dotted_path(raw_text: &str, dotted: &str) -> Option<usize> {
    let doc = Document::parse(raw_text.to_owned()).ok()?;
    let mut current: &dyn TableLike = doc.as_table();

    let mut segments = dotted.split('.').peekable();
    while let Some(seg) = segments.next() {
        if segments.peek().is_none() {
            // Final segment: prefer the key's own span; fall back to the
            // item's span (a key and its value share a source line).
            let span = current
                .key(seg)
                .and_then(|k| k.span())
                .or_else(|| current.get(seg).and_then(|item| item.span()))?;
            return Some(line_of_offset(raw_text, span.start));
        }
        current = current.get(seg)?.as_table_like()?;
    }
    None
}

/// 1-based line number of the byte at `offset` within `text`.
fn line_of_offset(text: &str, offset: usize) -> usize {
    text.char_indices()
        .take_while(|(i, _)| *i < offset)
        .filter(|(_, c)| *c == '\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::locate_dotted_path;

    #[test]
    fn top_level_table_header_maps_to_its_line() {
        let raw = "version = 3\n\n[retry]\nmax_attempts = 2\n";

        assert_eq!(locate_dotted_path(raw, "retry"), Some(3));
    }

    #[test]
    fn nested_table_header_maps_to_its_line() {
        let raw = "[server]\nhost = \"127.0.0.1\"\n\n[models.fast]\nprovider = \"mock\"\n";

        assert_eq!(locate_dotted_path(raw, "models.fast"), Some(4));
    }

    #[test]
    fn deeply_nested_header_maps_to_its_line() {
        let raw = "[retry]\n\n[retry.classes.feature-unsupported]\nfallback = false\n";

        assert_eq!(
            locate_dotted_path(raw, "retry.classes.feature-unsupported"),
            Some(3)
        );
    }

    #[test]
    fn key_within_a_table_maps_to_its_line() {
        let raw = "[aliases]\nfast = \"ghost\"\nslow = \"other\"\n";

        assert_eq!(locate_dotted_path(raw, "aliases.slow"), Some(3));
    }

    #[test]
    fn absent_path_returns_none() {
        let raw = "[server]\nhost = \"127.0.0.1\"\n";

        assert_eq!(locate_dotted_path(raw, "retry"), None);
        assert_eq!(locate_dotted_path(raw, "models.fast"), None);
    }

    #[test]
    fn unparseable_text_returns_none() {
        assert_eq!(locate_dotted_path("this = = broken", "this"), None);
    }
}
