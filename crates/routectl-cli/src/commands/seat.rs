//! Shared `--label` validation for the seat-aware OAuth commands
//! (`login` / `logout` / `refresh`). Mirrors the `SecretRef` parser's
//! rule so a label accepted on the CLI round-trips through an
//! `oauth://<provider>#<label>` ref: an empty or whitespace-only label
//! is rejected, anything else is accepted verbatim.

use routectl_core::{Error, Result};

/// Validate an optional seat label. `None` (no `--label` given) passes
/// through unchanged so every command stays byte-for-byte today's
/// behavior on the default seat. `Some(label)` is rejected when it is
/// empty or whitespace-only -- the same constraint the `oauth://` parser
/// enforces on the seat label after `#`. The error is deliberately
/// secret-free (a label is not secret material, but the message names
/// only the constraint, never echoes anything sensitive).
pub fn validate_label(label: Option<&str>) -> Result<Option<&str>> {
    match label {
        Some(l) if l.trim().is_empty() => Err(Error::Auth(
            "--label must not be empty or whitespace-only".into(),
        )),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_label;

    #[test]
    fn none_passes_through() {
        assert_eq!(validate_label(None).unwrap(), None);
    }

    #[test]
    fn non_empty_label_is_accepted_verbatim() {
        assert_eq!(validate_label(Some("seat-b")).unwrap(), Some("seat-b"));
    }

    #[test]
    fn empty_label_is_rejected() {
        let err = validate_label(Some("")).unwrap_err();
        assert!(
            err.to_string().contains("--label must not be empty"),
            "expected empty-label error, got: {err}"
        );
    }

    #[test]
    fn whitespace_only_label_is_rejected() {
        let err = validate_label(Some("   ")).unwrap_err();
        assert!(err.to_string().contains("--label must not be empty"));
    }
}
