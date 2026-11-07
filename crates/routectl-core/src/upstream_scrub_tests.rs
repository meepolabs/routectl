use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::{names_aws_principal, scrub_upstream_text};

/// A key-shaped value assembled at runtime so the source holds no literal
/// credential shape.
fn key(prefix: &str, body: &str, len: usize) -> String {
    format!("{prefix}{}", body.repeat(len))
}

/// Asserts `input` scrubs to an owned string carrying the placeholder and
/// none of `planted`.
fn assert_redacts(name: &str, input: &str, planted: &str) {
    let out = scrub_upstream_text(input);
    assert!(
        matches!(out, Cow::Owned(_)),
        "{name}: expected a rewrite of {input:?}"
    );
    assert!(
        out.contains("[redacted]"),
        "{name}: no placeholder in {out:?}"
    );
    assert!(
        !out.contains(planted),
        "{name}: planted value survived in {out:?}"
    );
}

fn assert_clean(name: &str, input: &str) {
    assert!(
        matches!(scrub_upstream_text(input), Cow::Borrowed(_)),
        "{name}: clean text was rewritten: {input:?}"
    );
}

#[test]
fn redacts_plain_and_partition_arns_through_the_end_of_the_token() {
    let rows = [
        (
            "arn plain head, encoded separator",
            "User: arn:aws%3Aiam%3A%3A123456789012%3Arole%2Fx denied",
            "123456789012",
        ),
        (
            "arn plain",
            "User: arn:aws:iam::123456789012:user/alice is not authorized",
            "123456789012:user/alice",
        ),
        (
            "arn partition",
            "Principal arn:aws-us-gov:sts::123456789012:assumed-role/r/s denied",
            "assumed-role/r/s",
        ),
        (
            "arn in parens",
            "(arn:aws:bedrock:us-east-1:123456789012:model/x)",
            "model/x",
        ),
    ];
    for (name, input, planted) in rows {
        assert_redacts(name, input, planted);
        assert!(names_aws_principal(input), "{name}: not named a principal");
    }
}

#[test]
fn redacts_percent_encoded_arn_in_mixed_case() {
    let input = "redirect=https%3A%2F%2Fx%2Farn%3AAws%3Aiam%3A%3A123456789012%3Arole%2Fops failed";
    assert_redacts("arn percent-encoded mixed case", input, "123456789012");
    assert!(names_aws_principal("ARN%3aaws%3aiam%3a%3a1%3arole"));
    let mixed = "User: arn%3Aaws:iam::123456789012:role/x denied";
    assert_redacts("arn encoded head, plain separator", mixed, "123456789012");
    assert!(names_aws_principal(mixed));
}

#[test]
fn redacts_account_id_after_each_separator_and_word() {
    let id = "123456789012";
    let rows = [
        ("space", format!("account {id} denied")),
        ("colon", format!("Account:{id}")),
        ("equals", format!("account={id}")),
        ("hash", format!("account#{id}")),
        ("underscore", format!("account_{id}")),
        ("hyphen", format!("account-{id}")),
        ("double quote", format!("account \"{id}\"")),
        ("single quote", format!("account '{id}'")),
        ("parens", format!("account ({id})")),
        ("tab", format!("account\t{id}")),
        ("word id", format!("account id {id}")),
        ("word ID", format!("Account ID: {id}")),
        ("word number", format!("account number {id}")),
        ("word Number upper", format!("ACCOUNT NUMBER={id}")),
        ("word id joined", format!("accountId={id}")),
    ];
    for (name, input) in &rows {
        assert_redacts(name, input, id);
        assert!(names_aws_principal(input), "{name}: not named a principal");
    }
}

#[test]
fn redacts_account_id_after_no_break_space() {
    let input = "account\u{a0}123456789012 is suspended";
    assert_redacts("nbsp", input, "123456789012");
    assert!(names_aws_principal(input));
}

#[test]
fn redacts_each_token_prefix_at_its_floor() {
    let rows = [
        ("sk-ant-api03", key("sk-ant-api03-", "a", 16)),
        ("sk-ant-oat01", key("sk-ant-oat01-", "a", 16)),
        ("sk-ant-ort01", key("sk-ant-ort01-", "a", 16)),
        ("sk-ant-admin01", key("sk-ant-admin01-", "a", 16)),
        ("sk-proj", key("sk-proj-", "a", 16)),
        ("sk-svcacct", key("sk-svcacct-", "a", 16)),
        ("sk-or-v1", key("sk-or-v1-", "a", 16)),
        ("ghp", key("ghp_", "a", 16)),
        ("AKIA", key("AKIA", "A", 16)),
        ("sk-admin", key("sk-admin-", "a", 16)),
        ("ASIA", key("ASIA", "Z", 16)),
        ("AIza", key("AIza", "b", 35)),
        ("ya29 modern", key("ya29.", "c", 20)),
        ("ya29 legacy", key("ya29.12.", "c", 20)),
        (
            "jwt",
            format!(
                "{}.{}.{}",
                key("eyJ", "d", 10),
                "e".repeat(10),
                "f".repeat(10)
            ),
        ),
        ("nvapi", key("nvapi-", "g", 20)),
        ("bedrock-api-key", key("bedrock-api-key-", "h", 20)),
        ("ABSK", format!("{}==", key("ABSK", "i", 100))),
    ];
    for (name, token) in &rows {
        let input = format!("invalid key: {token}, check config");
        assert_redacts(name, &input, token);
        assert_eq!(
            scrub_upstream_text(&input),
            "invalid key: [redacted], check config",
            "{name}"
        );
        assert!(
            !names_aws_principal(&input),
            "{name}: a token is not a principal"
        );
    }
}

#[test]
fn leaves_each_token_prefix_one_below_its_floor() {
    let rows = [
        ("sk-proj", key("sk-proj-", "a", 15)),
        ("AKIA", key("AKIA", "A", 15)),
        ("ASIA", key("ASIA", "Z", 15)),
        ("AIza", key("AIza", "b", 34)),
        ("ya29", key("ya29.", "c", 19)),
        (
            "jwt",
            format!(
                "{}.{}.{}",
                key("eyJ", "d", 10),
                "e".repeat(10),
                "f".repeat(9)
            ),
        ),
        ("nvapi", key("nvapi-", "g", 19)),
        ("bedrock-api-key", key("bedrock-api-key-", "h", 19)),
        ("ABSK", key("ABSK", "i", 99)),
    ];
    for (name, token) in &rows {
        assert_clean(name, &format!("value {token} rejected"));
    }
}

#[test]
fn anchored_prefixes_inside_a_token_stay() {
    let jwt = format!(
        "{}.{}.{}",
        key("eyJ", "d", 10),
        "e".repeat(10),
        "f".repeat(10)
    );
    let rows = [
        ("sk-admin mid-token", key("sk-admin-", "a", 16)),
        ("ASIA mid-token", key("ASIA", "Z", 16)),
        ("AIza mid-token", key("AIza", "b", 35)),
        ("ya29 mid-token", key("ya29.", "c", 20)),
        ("jwt mid-token", jwt),
        ("nvapi mid-token", key("nvapi-", "g", 20)),
        (
            "bedrock-api-key mid-token",
            key("bedrock-api-key-", "h", 20),
        ),
        ("ABSK mid-token", key("ABSK", "i", 100)),
    ];
    for (name, token) in &rows {
        assert_clean(name, &format!("value x{token} rejected"));
    }
}

#[test]
fn anchored_prefix_after_a_json_escape_is_redacted() {
    let asia = key("ASIA", "Z", 16);
    let rows = [
        ("newline escape", format!(r"line\n{asia}")),
        ("tab escape", format!(r"field\t{asia}")),
        ("unicode escape", format!(r"quote\u0022{asia}")),
        (
            "tab escape after short unicode",
            format!(r"bad\u00\t{asia}"),
        ),
    ];
    for (name, input) in &rows {
        assert_redacts(name, input, &asia);
    }
}

#[test]
fn redacts_double_encoded_arn_in_a_signing_canonical_request() {
    let canonical = "POST\n/model/arn%253Aaws%253Abedrock%253Aus-east-1%253A123456789012\
                     %253Ainference-profile%252Fabc/converse\n";
    assert_redacts(
        "arn double-encoded canonical path",
        canonical,
        "123456789012",
    );
    assert!(names_aws_principal(canonical));
    let boundary = "path=%252Farn%253aAWS%253Aiam%253A%253A123456789012%253Arole x";
    assert_redacts(
        "arn double-encoded after encoded slash",
        boundary,
        "123456789012",
    );
    assert!(names_aws_principal(boundary));
}

#[test]
fn redacts_echoed_session_token_value_and_keeps_the_header_name() {
    let token = key("Tok", "Ab+/9=", 6);
    let rows = [
        (
            "canonical header colon",
            format!("x-amz-security-token:{token}\nhost"),
            "x-amz-security-token:[redacted]\nhost",
        ),
        (
            "query string equals",
            format!("X-Amz-Security-Token={token}&X-Amz-Date=1"),
            "X-Amz-Security-Token=[redacted]&X-Amz-Date=1",
        ),
        (
            "mixed case header with space",
            format!("X-AMZ-security-Token: {token}"),
            "X-AMZ-security-Token: [redacted]",
        ),
        (
            "percent-encoded value",
            format!(
                "x-amz-security-token={}%2F{}",
                "a".repeat(10),
                "b".repeat(10)
            ),
            "x-amz-security-token=[redacted]",
        ),
    ];
    for (name, input, expected) in &rows {
        assert_redacts(name, input, &token);
        assert_eq!(scrub_upstream_text(input), *expected, "{name}");
    }
}

#[test]
fn short_session_token_value_stays() {
    assert_clean(
        "session token below floor",
        &format!("x-amz-security-token:{}", "a".repeat(19)),
    );
    assert_clean(
        "signed headers list",
        &["host", "x-amz-date", "x-amz-security-token"].join(";"),
    );
}

#[test]
fn redacts_url_userinfo_and_keeps_scheme_and_host() {
    let rows = [
        (
            "user and password",
            "fetch https://alice:hunter2pass@example.com/x failed",
            "hunter2pass",
        ),
        (
            "bare token",
            "proxy http://tok3n@proxy.example:8080 refused",
            "tok3n",
        ),
        ("at sign in password", "s3://u:p@ss@bucket/key", "p@ss"),
        (
            "escaped slashes",
            r#"{\"url\":\"https:\/\/u:pw0rd@host\/x\"}"#,
            "pw0rd",
        ),
        (
            "scheme after arrow",
            "redirect ->https://u:pw0rd@host",
            "pw0rd",
        ),
        (
            "scheme after double hyphen",
            "--https://u:pw0rd@host",
            "pw0rd",
        ),
        (
            "scheme after newline escape",
            r"x\nhttps://u:pw0rd@host",
            "pw0rd",
        ),
    ];
    for (name, input, planted) in rows {
        assert_redacts(name, input, planted);
    }
    assert_eq!(
        scrub_upstream_text("fetch https://alice:pw@example.com/x"),
        "fetch https://[redacted]@example.com/x"
    );
    assert_eq!(
        scrub_upstream_text(r"https:\/\/u:pw@host"),
        r"https:\/\/[redacted]@host"
    );
}

#[test]
fn userinfo_without_a_valid_scheme_stays() {
    let rows = [
        ("no scheme", "parse error: ://alice@host"),
        ("scheme starting with a digit", "1abc://x@y"),
        ("scheme inside a token", "x_https://u:pw@host"),
        ("at sign after a backslash path", r"https://host\a@b"),
    ];
    for (name, input) in rows {
        assert_clean(name, input);
    }
}

#[test]
fn near_misses_return_borrowed() {
    let rows = [
        ("yarn marker", "yarn:aws:plugin failed to load"),
        ("learn marker", "learn: see docs"),
        ("urn uuid", "urn:uuid:6e8bc430-9c3a-11d9-9669-0800200c9a66"),
        ("arn other partition word", "arn:partner:thing"),
        ("eleven digits", "account 12345678901 not found"),
        ("thirteen digits", "account 1234567890123 not found"),
        (
            "timestamp without account",
            "request at 202611061230 failed",
        ),
        (
            "url without userinfo",
            "see https://docs.example.com/errors#e1 for details",
        ),
        ("email in prose", "contact support@example.com"),
        ("empty", ""),
        ("account word in prose", "check your account settings"),
        ("short number after account", "account 12345 of 3"),
        (
            "at sign in url path",
            "see https://docs.example.com/a@b/c for details",
        ),
        ("arn word in prose", "learn: arn: aws docs"),
        (
            "at sign in escaped url path",
            r"see https:\/\/docs.example.com\/a@b\/c",
        ),
    ];
    for (name, input) in rows {
        assert_clean(name, input);
        assert!(!names_aws_principal(input), "{name}: named a principal");
    }
}

#[test]
fn scrub_is_deterministic() {
    let input = format!(
        "account 123456789012 arn:aws:iam::1:root {} https://u:p@h",
        key("sk-proj-", "a", 16)
    );
    let outputs: Vec<_> = (0..3).map(|_| scrub_upstream_text(&input)).collect();
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn scrub_preserves_multibyte_text_around_redactions() {
    let input = "caf\u{e9} account 123456789012 \u{2014} arn:aws:iam::1:root";
    assert_eq!(
        scrub_upstream_text(input),
        "caf\u{e9} account [redacted] \u{2014} [redacted]"
    );
}

#[test]
fn redacts_arn_head_in_any_case() {
    let rows = [
        ("upper ARN", "ARN:aws:iam::123456789012:role/x denied"),
        ("upper AWS", "arn:AWS:iam::123456789012:role/x denied"),
    ];
    for (name, input) in rows {
        assert_redacts(name, input, "role/x");
        assert!(names_aws_principal(input), "{name}: not named a principal");
    }
}

#[test]
fn redacts_account_id_after_escaped_quotes() {
    let input = r#"{\"accountId\":\"123456789012\"}"#;
    assert_redacts("escaped quotes", input, "123456789012");
    assert!(names_aws_principal(input));
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../routectl-router/tests/fixtures")
}

fn json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            json_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
}

/// Collects every `message` string value, descending into string values
/// that are themselves a JSON document (a captured error `body`).
fn collect_messages(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "message"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_owned());
                }
                collect_messages(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_messages(item, out);
            }
        }
        serde_json::Value::String(text) => {
            let trimmed = text.trim_start();
            if (trimmed.starts_with('{') || trimmed.starts_with('['))
                && let Ok(inner) = serde_json::from_str::<serde_json::Value>(text)
            {
                collect_messages(&inner, out);
            }
        }
        _ => {}
    }
}

#[test]
fn real_upstream_error_messages_are_left_unchanged() {
    let mut files = Vec::new();
    json_files(&fixtures_dir(), &mut files);
    let mut files_read = 0;
    let mut messages = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read fixture");
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let before = messages.len();
        collect_messages(&value, &mut messages);
        if messages.len() > before {
            files_read += 1;
        }
    }
    assert!(
        files_read >= 50,
        "only {files_read} fixture files carried a message"
    );
    assert!(
        messages.len() >= 90,
        "only {} messages collected",
        messages.len()
    );
    for message in &messages {
        assert_clean("fixture message", message);
    }
}
