//! Fail-safe redaction of `parse_config` (toml/serde) diagnostics down to
//! provably-secret-free content, shared by every command that surfaces a
//! config parse error to the terminal (`config migrate` on a gate failure,
//! `doctor` on the read-only load error).
//!
//! The redaction is allowlist / fail-safe by construction: a line is retained
//! ONLY when its shape is recognized as value-free, and every backtick-quoted
//! name on a retained line is kept only when it passes a strict bare-key
//! allowlist; anything unrecognized is dropped. A blocklist would be wrong here
//! -- toml/serde echo the offending config VALUE in several places (the
//! numbered source-line preview, `invalid type:`/`invalid value:` clauses, the
//! backtick token of an `unknown variant` clause, and the backtick token of an
//! `unknown field`/`duplicate key` clause when the offending TOML key is a
//! quoted string). A mistyped secret in a non-string field or a secret used as
//! a quoted key would otherwise reach the terminal. Schema field/variant NAMES
//! (always bare keys) are safe to keep; user-controlled names and field VALUES
//! never are.

/// Redact a `parse_config` error down to provably-safe content. See the module
/// docs for why this is fail-safe (allowlist) rather than a blocklist.
pub(crate) fn redact_parse_error(err: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut field_from_snippet: Option<String> = None;
    for line in err.lines() {
        if is_header_line(line) {
            out.push(line.to_string());
        } else if is_source_snippet_row(line) {
            // The snippet echoes the value and is always dropped, but its key
            // (text left of `=`) is a safe field name we can thread into a
            // following type/value clause.
            if let Some(name) = snippet_field_name(line) {
                field_from_snippet = Some(name);
            }
        } else if let Some(safe) = sanitize_clause(line, field_from_snippet.as_deref()) {
            out.push(safe);
        }
    }
    out.join("\n")
}

/// Whether `line` is the toml diagnostic header (`TOML parse error at line N,
/// column M`), which carries only line/column numbers.
fn is_header_line(line: &str) -> bool {
    line.trim_start().starts_with("TOML parse error")
}

/// Whether `key` is a strict bare key: non-empty and drawn only from the
/// TOML bare-key alphabet (`[A-Za-z0-9_-.]`). A quoted TOML key can be an
/// arbitrary string -- a `literal:` credential, a token with `:` / `/` /
/// whitespace -- so any name that is NOT a bare key is treated as
/// user-controlled and unsafe to echo.
fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Extract the safe bare key (text left of the first `=`) from a numbered
/// snippet source row, e.g. `3 | port = "..."` yields `port`. Returns `None`
/// for separator/caret rows, table-header rows, or a quoted/non-bare key --
/// the value (right of `=`) is never inspected.
fn snippet_field_name(row: &str) -> Option<String> {
    let after_pipe = row.split_once('|')?.1;
    let key = after_pipe.split_once('=')?.0.trim();
    is_bare_key(key).then(|| key.to_string())
}

/// Render a recognized-safe clause line, or `None` to drop an unrecognized
/// shape. `invalid type:`/`invalid value:` clauses embed the raw value, so they
/// collapse to a generic message. For `unknown field`/`unknown variant` the
/// FIRST backtick token is the USER-controlled name/value the config carried
/// (a `literal:` credential, or even a bare-key-shaped secret) -- it is always
/// dropped, and only the schema candidate list after `expected` survives.
/// `duplicate key` collapses to a class message (its token is user-controlled
/// with no safe remainder). `missing field` names a SCHEMA field the config
/// omitted, so its token is kept when it is a bare key.
fn sanitize_clause(line: &str, field: Option<&str>) -> Option<String> {
    let clause = line.trim();
    if clause.contains("invalid type:") || clause.contains("invalid value:") {
        return Some(match field {
            Some(name) => format!("value rejected for field `{name}` (type/value mismatch)"),
            None => "value rejected (type/value mismatch)".to_string(),
        });
    }
    if clause.starts_with("unknown variant ") {
        return Some(drop_offending_keep_candidates(
            clause,
            "unknown variant rejected",
        ));
    }
    if clause.starts_with("unknown field ") {
        return Some(drop_offending_keep_candidates(
            clause,
            "unknown field rejected",
        ));
    }
    if clause.starts_with("duplicate key ") {
        return Some("duplicate key rejected".to_string());
    }
    if clause.starts_with("missing field ") {
        return Some(keep_schema_token(clause, "missing field"));
    }
    if clause.starts_with("did you mean ") {
        return Some(sanitize_backtick_names(clause));
    }
    None
}

/// Drop the FIRST (user-controlled) backtick token of a clause and keep only
/// the schema candidate list after `expected`. serde renders the offending
/// field/variant the config carried in that first token -- which can be a
/// `literal:` credential or a bare-key-shaped secret -- so it is dropped
/// unconditionally, never allowlisted. The value token is consumed by matching
/// its two backticks BEFORE searching for `expected`, so a token that itself
/// contains the word `expected` cannot slice the candidate list. Collapses to
/// `class` alone when the grammar does not match.
fn drop_offending_keep_candidates(clause: &str, class: &str) -> String {
    let after_token = clause
        .split_once('`')
        .and_then(|(_, r)| r.split_once('`'))
        .map(|(_, tail)| tail);
    match after_token.and_then(|t| t.split_once("expected")) {
        Some((_, candidates)) => {
            format!("{class}, expected{}", sanitize_backtick_names(candidates))
        }
        None => class.to_string(),
    }
}

/// Keep the single backtick token of a `missing field `X`` clause when it is a
/// bare key. Unlike unknown-field, `X` here is the SCHEMA field the config
/// omitted (not user input), so naming it is safe. Collapses to `class` when
/// the token is absent or non-bare.
fn keep_schema_token(clause: &str, class: &str) -> String {
    match clause
        .split_once('`')
        .and_then(|(_, r)| r.split_once('`'))
        .map(|(name, _)| name)
    {
        Some(name) if is_bare_key(name) => format!("{class} `{name}`"),
        _ => class.to_string(),
    }
}

/// Rebuild a schema candidate list keeping every backtick-quoted token ONLY
/// when it passes the bare-key allowlist, dropping the rest. Applied to the
/// `expected one of `a`, `b`` names (and the `did you mean` suggestion) -- all
/// schema-controlled, so bare-gating is a belt-and-suspenders strip rather than
/// the primary defense (the user-controlled offending token is dropped by the
/// caller before this ever runs). Text outside backticks is preserved verbatim.
fn sanitize_backtick_names(clause: &str) -> String {
    let mut out = String::with_capacity(clause.len());
    let mut rest = clause;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            // Unbalanced trailing backtick: drop the remainder to fail safe.
            return out;
        };
        let token = &after_open[..close];
        if is_bare_key(token) {
            out.push('`');
            out.push_str(token);
            out.push('`');
        }
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether `line` is a toml snippet row that echoes source bytes: the `|`
/// separator/caret rows (`  |`, `  | ^^^`) or a numbered source row
/// (`<n> | <verbatim source>`).
fn is_source_snippet_row(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('|') {
        return true;
    }
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    digits_end > 0 && trimmed[digits_end..].trim_start().starts_with('|')
}

#[cfg(test)]
mod tests {
    use super::redact_parse_error;

    #[test]
    fn redact_parse_error_keeps_header_and_drops_unknown_field_token() {
        let raw = "TOML parse error at line 6, column 1\n  |\n6 | api_key_ref = \"literal:top-secret\"\n  | ^^^^^^^^^^^\nunknown field `api_key_ref`";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("top-secret"), "{redacted}");
        assert!(redacted.contains("line 6, column 1"), "{redacted}");
        assert!(redacted.contains("unknown field"), "{redacted}");
        // The offending field name is USER-controlled (a config key can be a
        // quoted secret), so it is dropped even when it is a bare key.
        assert!(!redacted.contains("api_key_ref"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_drops_a_bare_key_shaped_unknown_field_token() {
        // The offending key fits the bare-key alphabet yet is user-controlled
        // (a secret can be shaped `sk-live-ABC_123`), so it must NOT survive;
        // only the schema candidate names do.
        let raw = "unknown field `sk-live-ABC_123`, expected one of `kind`, `base_url`";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("sk-live-ABC_123"), "{redacted}");
        assert!(!redacted.contains("ABC_123"), "{redacted}");
        assert!(redacted.contains("kind"), "{redacted}");
        assert!(redacted.contains("base_url"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_keeps_missing_field_schema_token() {
        // `missing field` names a SCHEMA field the config omitted (not user
        // input), so the token is kept when it is a bare key.
        let raw = "missing field `base_url`";
        let redacted = redact_parse_error(raw);
        assert!(redacted.contains("base_url"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_drops_a_secret_shaped_quoted_key_from_unknown_field() {
        // A TOML key can be an arbitrary quoted string; serde renders it as the
        // (unquoted) backtick token of an `unknown field` clause. A `literal:`
        // credential used as a key must be dropped, while the safe schema
        // candidate names survive.
        let raw = "unknown field `literal:sk-live-LEAKED`, expected one of `server`, `providers`";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("LEAKED"), "{redacted}");
        assert!(!redacted.contains("literal:"), "{redacted}");
        assert!(redacted.contains("server"), "{redacted}");
        assert!(redacted.contains("providers"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_drops_a_secret_shaped_duplicate_key() {
        let raw = "duplicate key `literal:sk-DUPE-LEAKED` in table `providers`";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("LEAKED"), "{redacted}");
        assert!(!redacted.contains("literal:"), "{redacted}");
        // The duplicated key is user-controlled with no safe remainder, so the
        // whole clause collapses to a class-only message.
        assert_eq!(redacted, "duplicate key rejected", "{redacted}");
    }

    #[test]
    fn redact_parse_error_strips_value_from_type_mismatch_clause() {
        // serde embeds the raw offending value verbatim in a type-mismatch
        // clause; the numbered snippet row echoes it too. Both must be gone.
        let raw = "TOML parse error at line 5, column 8\n  |\n5 | port = \"sk-live-LEAKED\"\n  |        ^^^^^^^^^^^^^^^^\ninvalid type: string \"sk-live-LEAKED\", expected u16";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("sk-live-LEAKED"), "{redacted}");
        assert!(!redacted.contains("LEAKED"), "{redacted}");
        assert!(redacted.contains("line 5, column 8"), "{redacted}");
        // The field name recovered from the snippet key stays useful.
        assert!(redacted.contains("port"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_strips_value_from_unknown_variant_clause() {
        // A secret placed as an enum value lands in the backtick-quoted token
        // of an `unknown variant` clause -- that token must be dropped, the
        // schema candidate list kept.
        let raw = "TOML parse error at line 4, column 6\n  |\n4 | mode = \"sk-live-LEAKED\"\n  |        ^^^^^^^^^^^^^^^\nunknown variant `sk-live-LEAKED`, expected `fast` or `slow`";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("LEAKED"), "{redacted}");
        assert!(redacted.contains("fast"), "{redacted}");
        assert!(redacted.contains("slow"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_unknown_variant_token_containing_expected_does_not_leak() {
        // The offending value can itself contain the word `expected`; the
        // redactor must consume the value token before reading the candidate
        // list rather than split on the first `expected`.
        let raw = "unknown variant `sk-expected-LEAKED`, expected `fast` or `slow`";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("LEAKED"), "{redacted}");
        assert!(!redacted.contains("sk-expected"), "{redacted}");
        assert!(redacted.contains("fast"), "{redacted}");
        assert!(redacted.contains("slow"), "{redacted}");
    }

    #[test]
    fn redact_parse_error_drops_unrecognized_line_shapes() {
        // A message shape the allowlist does not recognize is dropped wholesale
        // rather than echoed, even when it carries config bytes.
        let raw = "TOML parse error at line 2, column 1\n  |\n2 | secret = \"sk-live-LEAKED\"\n  | ^^^^^^\nsome unforeseen serde diagnostic mentioning sk-live-LEAKED inline";
        let redacted = redact_parse_error(raw);
        assert!(!redacted.contains("sk-live-LEAKED"), "{redacted}");
        assert!(!redacted.contains("unforeseen"), "{redacted}");
        assert_eq!(
            redacted, "TOML parse error at line 2, column 1",
            "{redacted}"
        );
    }
}
