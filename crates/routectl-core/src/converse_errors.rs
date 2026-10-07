//! The wrapper Bedrock Converse puts around a model-level validation message.
//!
//! InvokeModel returns such a message bare; Converse returns the same text
//! behind `The model returned the following errors: `. Readers that match the
//! message exactly unwrap it here so both carriers share one definition.

/// The literal Converse places in front of a model-level validation message.
const CONVERSE_ERRORS_PREFIX: &str = "The model returned the following errors: ";

/// `message` with at most one leading Converse errors wrapper removed. The
/// message is otherwise returned byte-for-byte: no whitespace is trimmed, so
/// a caller matching an exact envelope still sees any deviation, and a
/// doubled wrapper keeps its inner copy.
#[must_use]
pub fn strip_converse_errors_prefix(message: &str) -> &str {
    message
        .strip_prefix(CONVERSE_ERRORS_PREFIX)
        .unwrap_or(message)
}

#[cfg(test)]
mod tests {
    use super::strip_converse_errors_prefix;

    const INNER: &str = "tool type 'advisor' is not supported for this model";

    #[test]
    fn bare_message_passes_through() {
        assert_eq!(strip_converse_errors_prefix(INNER), INNER);
    }

    #[test]
    fn one_wrapper_is_stripped() {
        let wrapped = format!("The model returned the following errors: {INNER}");

        assert_eq!(strip_converse_errors_prefix(&wrapped), INNER);
    }

    #[test]
    fn doubled_wrapper_loses_exactly_one() {
        let once = format!("The model returned the following errors: {INNER}");
        let twice = format!("The model returned the following errors: {once}");

        assert_eq!(strip_converse_errors_prefix(&twice), once);
    }

    #[test]
    fn drifted_singular_wrapper_is_untouched() {
        let drifted = format!("The model returned the following error: {INNER}");

        assert_eq!(strip_converse_errors_prefix(&drifted), drifted);
    }

    #[test]
    fn leading_space_is_not_trimmed() {
        let spaced = format!(" The model returned the following errors: {INNER}");

        assert_eq!(strip_converse_errors_prefix(&spaced), spaced);
    }
}
