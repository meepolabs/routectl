use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::{
    names_aws_principal, scrub_upstream_text, scrub_upstream_value, upstream_text_for_log,
};

/// A key-shaped value assembled at runtime so the source holds no literal
/// credential shape.
fn key(prefix: &str, body: &str, len: usize) -> String {
    format!("{prefix}{}", body.repeat(len))
}

/// An older OpenAI user key: `sk-`, `before` opaque chars, the `T3BlbkFJ`
/// infix, then `after` opaque chars; assembled at runtime.
fn openai_infix_key(before: usize, after: usize) -> String {
    format!("sk-{}T3BlbkFJ{}", "j".repeat(before), "k".repeat(after))
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
        ("sk-None", key("sk-None-", "m", 16)),
        ("openai infix", openai_infix_key(8, 16)),
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
        ("sk-None", key("sk-None-", "m", 15)),
        ("openai infix short tail", openai_infix_key(8, 15)),
        ("openai infix short head", openai_infix_key(7, 16)),
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
        ("sk-None mid-token", key("sk-None-", "m", 16)),
        ("openai infix mid-token", openai_infix_key(8, 16)),
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
    let encoded = format!("{}%2F{}", "a".repeat(10), "b".repeat(10));
    let rows = [
        (
            "canonical header colon",
            &token,
            format!("x-amz-security-token:{token}\nhost"),
            "x-amz-security-token:[redacted]\nhost",
        ),
        (
            "query string equals",
            &token,
            format!("X-Amz-Security-Token={token}&X-Amz-Date=1"),
            "X-Amz-Security-Token=[redacted]&X-Amz-Date=1",
        ),
        (
            "mixed case header with space",
            &token,
            format!("X-AMZ-security-Token: {token}"),
            "X-AMZ-security-Token: [redacted]",
        ),
        (
            "percent-encoded value",
            &encoded,
            format!("x-amz-security-token={encoded}"),
            "x-amz-security-token=[redacted]",
        ),
        (
            "escaped-quote json key",
            &token,
            format!(r#"{{\"x-amz-security-token\":\"{token}\"}}"#),
            r#"{\"x-amz-security-token\":\"[redacted]\"}"#,
        ),
        (
            "single-quoted key",
            &token,
            format!("{{'x-amz-security-token': '{token}'}}"),
            "{'x-amz-security-token': '[redacted]'}",
        ),
    ];
    for (name, planted, input, expected) in &rows {
        assert_redacts(name, input, planted);
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
        (
            "domain user in proxy url",
            r"proxy http://CORP\alice:Pw0rd123@proxy:8080 refused",
            "Pw0rd123",
        ),
        (
            "domain user in nested json url",
            r"http:\/\/CORP\\alice:pw0rd@proxy",
            "pw0rd",
        ),
        ("scheme inside a token", "cfg_https://u:pw0rd@h", "pw0rd"),
        ("no scheme", "://u:pw0rd@host", "pw0rd"),
        ("at sign after a backslash", r"https://host\a@b", "host"),
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
    assert_eq!(
        scrub_upstream_text(r"proxy http://CORP\alice:pw@proxy:8080"),
        "proxy http://[redacted]@proxy:8080"
    );
}

#[test]
fn userinfo_shape_is_redacted_whatever_precedes_the_slashes() {
    assert_eq!(
        scrub_upstream_text("parse error: ://alice@host"),
        "parse error: ://[redacted]@host"
    );
    assert_eq!(scrub_upstream_text("1abc://x@y"), "1abc://[redacted]@y");
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
fn redacts_account_id_after_json_escaped_separators() {
    let id = "123456789012";
    let rows = [
        ("tab escape", format!(r"account\t{id}")),
        ("no-break space escape", format!(r"account\u00a0{id}")),
        ("upper hex escape", format!(r"account\u00A0{id}")),
        ("quote escape", format!(r"account\u0022{id}\u0022")),
    ];
    for (name, input) in &rows {
        assert_redacts(name, input, id);
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

#[test]
fn redacts_the_quoted_value_of_a_credential_named_key_and_keeps_the_key() {
    let rows = [
        (
            "single-quoted python repr",
            "{'authorization_token': 'abc123', 'type': 'url'}",
            "{'authorization_token': '[redacted]', 'type': 'url'}",
        ),
        (
            "double-quoted json",
            r#"{"api_key":"abc123","model":"m"}"#,
            r#"{"api_key":"[redacted]","model":"m"}"#,
        ),
        (
            "json-escaped json",
            r#"{\"secret\":\"abc123\"}"#,
            r#"{\"secret\":\"[redacted]\"}"#,
        ),
        (
            "bare key equals single-quoted value",
            "login failed: password='abc123'",
            "login failed: password='[redacted]'",
        ),
        (
            "bare key equals double-quoted value",
            "api_key=\"abc123\" rejected",
            "api_key=\"[redacted]\" rejected",
        ),
        (
            "spaces around the separator",
            r#""access_token" : "abc123""#,
            r#""access_token" : "[redacted]""#,
        ),
        (
            "mixed-case header name",
            r#"{"Authorization": "Bearer abc123"}"#,
            r#"{"Authorization": "[redacted]"}"#,
        ),
        (
            "json escaped twice",
            r#"{\\\"client_secret\\\":\\\"abc123\\\"}"#,
            r#"{\\\"client_secret\\\":\\\"[redacted]\\\"}"#,
        ),
        (
            "deeper-escaped quote stays inside the value",
            r#"{"password":"ab\"c123"}"#,
            r#"{"password":"[redacted]"}"#,
        ),
        (
            "unterminated value runs to the next control",
            "{\"api_key\":\"abc123\nnext line",
            "{\"api_key\":\"[redacted]\nnext line",
        ),
        (
            "dotted key",
            r#"{"mcp_servers.0.authorization_token":"abc123"}"#,
            r#"{"mcp_servers.0.authorization_token":"[redacted]"}"#,
        ),
    ];
    for (name, input, expected) in rows {
        assert_redacts(name, input, "abc123");
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

#[test]
fn credential_key_rule_leaves_non_credential_and_unquoted_values() {
    let rows = [
        ("unquoted token count", "max_tokens: 5"),
        ("quoted token count", r#""input_tokens": "7""#),
        ("json number token count", r#"{"max_tokens": 5}"#),
        (
            "plural tokens key",
            r#"{"cache_creation_input_tokens":"12"}"#,
        ),
        ("token in prose", "unexpected token: '}' at position 4"),
        ("bare key with colon", "password: 'see the docs'"),
        (
            "credential word as a value",
            r#"{"type":"authentication_error"}"#,
        ),
        (
            "credential key before an object",
            r#"{"authorization":{"type":"none"}}"#,
        ),
        ("empty credential value", r#"{"api_key":""}"#),
        ("key ending mid-word", r#"{"tokenizer":"cl100k"}"#),
        ("budget tokens key", r#""budget_tokens":"1024""#),
    ];
    for (name, input) in rows {
        assert_clean(name, input);
    }
    assert_clean(
        "unquoted credential value",
        &format!("{}={}", "api_key", "unset"),
    );
}

#[test]
fn credential_key_judgment_is_bounded_by_key_length() {
    let long = format!("{}_name", "a".repeat(123));
    assert_clean(
        "128-char non-credential key",
        &format!(r#"{{"{long}":"abc123"}}"#),
    );
    let longer = format!("{}_name", "a".repeat(124));
    assert_redacts(
        "129-char key fails closed",
        &format!(r#"{{"{longer}":"abc123"}}"#),
        "abc123",
    );
}

/// Realistic upstream error texts that must pass byte-identical. Each row
/// names its source: a committed capture under
/// `crates/routectl-router/tests/fixtures`, or the error shape a provider's
/// public API reference documents.
#[test]
fn realistic_upstream_error_texts_are_left_unchanged() {
    let rows = [
        (
            "anthropic context window, fixtures rejections/anthropic-api/context-window-exceeded-messages.json",
            r#"{"error":{"message":"prompt is too long: 300042 tokens > 200000 maximum","type":"invalid_request_error"},"type":"error"}"#,
        ),
        (
            "anthropic max_tokens over limit, fixtures mantle_capture.json",
            "max_tokens: 1000000 > 64000, which is the maximum allowed number of output tokens for anthropic.claude-haiku-4-5",
        ),
        (
            "anthropic tool_choice, fixtures forced_tool_choice_capture.json",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"tool_choice: type \"tool\" and \"any\" are not supported for this model."}}"#,
        ),
        (
            "anthropic orphan tool_result, Messages API invalid_request_error shape",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.2.content.0: unexpected `tool_use_id` found in `tool_result` blocks: toolu_01A09q90qw90lq917835lq9. Each `tool_result` block must have a corresponding `tool_use` block in the previous message."}}"#,
        ),
        (
            "anthropic cache_control limit, Messages API invalid_request_error shape",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"A maximum of 4 blocks with cache_control may be provided. Found 5."}}"#,
        ),
        (
            "anthropic 401, Messages API authentication_error shape",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        ),
        (
            "anthropic 429, Messages API rate_limit_error shape",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the rate limit for your organization of 40,000 input tokens per minute. Please reduce the prompt length or the maximum tokens requested."}}"#,
        ),
        (
            "openai effort value, fixtures rejections/openai-responses/reasoning-effort-invalid-value-codex.json",
            r#"{"error":{"code":"invalid_value","message":"Invalid value: 'bogus'. Supported values are: 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', and 'max'.","param":"reasoning.effort","type":"invalid_request_error"}}"#,
        ),
        (
            "openai max_tokens unsupported, Chat Completions invalid_request_error shape",
            r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","param":"max_tokens","code":"unsupported_parameter"}}"#,
        ),
        (
            "openai 429, Chat Completions rate_limit_exceeded shape",
            r#"{"error":{"message":"Rate limit reached for gpt-4o in organization org-abc on tokens per min (TPM): Limit 30000, Used 29123, Requested 1500. Please try again in 1.246s.","type":"tokens","param":null,"code":"rate_limit_exceeded"}}"#,
        ),
        (
            "openai bad key, Chat Completions invalid_api_key shape",
            r#"{"error":{"message":"Incorrect API key provided: sk-abc***wxyz. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#,
        ),
        (
            "bedrock max tokens, fixtures rejections/bedrock/max-tokens-above-model-limit.json",
            r#"{"message":"The maximum tokens you requested exceeds the model limit of 8192. Try again with a maximum tokens value that is lower than 8192."}"#,
        ),
        (
            "bedrock throttling, Bedrock Runtime ThrottlingException shape",
            r#"{"message":"Too many requests, please wait before trying again."}"#,
        ),
        (
            "bedrock extraneous key, Bedrock Runtime ValidationException shape",
            r#"{"message":"Malformed input request: #: extraneous key [max_tokens] is not permitted, please reformat your input and try again."}"#,
        ),
        (
            "gemini quota, Gemini API RESOURCE_EXHAUSTED shape",
            r#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED"}}"#,
        ),
        (
            "gemini bad key, Gemini API INVALID_ARGUMENT ErrorInfo shape",
            r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com","metadata":{"service":"generativelanguage.googleapis.com"}}]}}"#,
        ),
        (
            "openai usage echo, Chat Completions usage object",
            r#"{"usage":{"prompt_tokens":"12","completion_tokens":"7","input_tokens":"7","total_tokens":"19"}}"#,
        ),
    ];
    for (name, input) in rows {
        assert_clean(name, input);
    }
}

/// Collects every string value under a `body` key: a captured raw error
/// body, JSON text with its keys still quoted.
fn collect_bodies(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "body"
                    && let Some(text) = child.as_str()
                {
                    out.push(text.to_owned());
                }
                collect_bodies(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_bodies(item, out);
            }
        }
        _ => {}
    }
}

#[test]
fn real_upstream_error_bodies_are_left_unchanged() {
    let mut files = Vec::new();
    json_files(&fixtures_dir(), &mut files);
    let mut bodies = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read fixture");
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        collect_bodies(&value, &mut bodies);
    }
    let quoted_pairs = bodies.iter().filter(|body| body.contains("\":\"")).count();
    assert!(bodies.len() >= 60, "only {} bodies collected", bodies.len());
    assert!(
        quoted_pairs >= 40,
        "only {quoted_pairs} bodies carry a quoted key and value"
    );
    for body in &bodies {
        assert_clean("fixture body", body);
    }
}

/// One input per redaction rule and spelling the scrub covers.
fn planted_fixture() -> Vec<String> {
    let mut rows: Vec<String> = [
        "User: arn:aws:iam::123456789012:user/alice is not authorized",
        "Principal arn:aws-us-gov:sts::123456789012:assumed-role/r/s denied",
        "redirect=https%3A%2F%2Fx%2Farn%3AAws%3Aiam%3A%3A123456789012%3Arole%2Fops",
        "path=%252Farn%253aAWS%253Aiam%253A%253A123456789012%253Arole x",
        "account\u{a0}123456789012 is suspended",
        r#"{\"accountId\":\"123456789012\"}"#,
        r"account\u0022123456789012\u0022",
        "fetch https://alice:hunter2pass@example.com/x failed",
        r#"{\"url\":\"https:\/\/u:pw0rd@host\/x\"}"#,
        r"proxy http://CORP\alice:Pw0rd123@proxy:8080 refused",
        "{'authorization_token': 'abc123', 'type': 'url'}",
        r#"{"api_key":"abc123"}"#,
        r#"{\"secret\":\"abc123\"}"#,
        "password='abc123'",
        r#"{\\\"client_secret\\\":\\\"abc123\\\"}"#,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let session = key("Tok", "Ab+/9=", 6);
    rows.push(format!("x-amz-security-token:{session}\nhost"));
    rows.push(format!(r#"{{\"x-amz-security-token\":\"{session}\"}}"#));
    rows.extend(
        [
            key("sk-ant-api03-", "a", 16),
            key("sk-proj-", "a", 16),
            key("AKIA", "A", 16),
            key("ASIA", "Z", 16),
            key("AIza", "b", 35),
            key("ya29.12.", "c", 20),
            format!(
                "{}.{}.{}",
                key("eyJ", "d", 10),
                "e".repeat(10),
                "f".repeat(10)
            ),
            key("nvapi-", "g", 20),
            key("bedrock-api-key-", "h", 20),
            format!("{}==", key("ABSK", "i", 100)),
            key("sk-None-", "m", 16),
            openai_infix_key(8, 16),
        ]
        .iter()
        .map(|token| format!("invalid key: {token}, check config")),
    );
    rows
}

#[test]
fn scrub_is_idempotent_over_the_planted_fixture() {
    for input in planted_fixture() {
        let once = scrub_upstream_text(&input).into_owned();
        assert_ne!(once, input, "planted row was not redacted: {input:?}");
        assert_eq!(
            scrub_upstream_text(&once),
            once,
            "second pass changed {input:?}"
        );
    }
}

#[test]
fn log_rendering_scrubs_before_the_length_cap() {
    // Each credential starts inside the 256-char cap and ends past it, so a
    // cap applied first leaves a fragment too short for its rule.
    let pad = "x".repeat(240);
    let rows = [
        (
            "token straddling the cap",
            format!("{pad} {} tail", key("sk-proj-", "a", 40)),
            "sk-proj-",
        ),
        (
            "account id straddling the cap",
            format!("{pad}account 123456789012 tail"),
            "1234",
        ),
    ];
    for (name, input, planted) in &rows {
        let out = upstream_text_for_log(input);
        assert!(out.chars().count() <= 256, "{name}: not capped");
        assert!(out.contains("[redact"), "{name}: no placeholder in {out:?}");
        assert!(
            !out.contains(planted),
            "{name}: planted fragment survived in {out:?}"
        );
        assert!(out.starts_with(&pad), "{name}: clean prefix changed");
    }
}

#[test]
fn log_rendering_filters_controls_and_keeps_clean_text() {
    assert_eq!(
        upstream_text_for_log("bad\nline account 123456789012 \u{e9}"),
        "bad?line account [redacted] ?"
    );
    let clean = "max_tokens: 1000000 > 64000, which is the maximum allowed";
    assert_eq!(upstream_text_for_log(clean), clean);
}

#[test]
fn value_scrub_rewrites_string_leaves_and_keeps_keys_and_other_leaves() {
    let arn_key = "arn:aws:iam::123456789012:root";
    let mut value = serde_json::json!({
        "error": {
            "message": "User: arn:aws:iam::123456789012:user/alice denied",
            "details": [
                {"reason": "account 123456789012 suspended", "retry": false},
                "fetch https://u:pw0rd@host/x",
                null
            ],
            "code": 123_456_789_012_u64,
            "ratio": 0.5,
            "type": "access_denied"
        },
        arn_key: "plain"
    });
    scrub_upstream_value(&mut value);
    let expected = serde_json::json!({
        "error": {
            "message": "User: [redacted] denied",
            "details": [
                {"reason": "account [redacted] suspended", "retry": false},
                "fetch https://[redacted]@host/x",
                null
            ],
            "code": 123_456_789_012_u64,
            "ratio": 0.5,
            "type": "access_denied"
        },
        arn_key: "plain"
    });
    assert_eq!(value, expected);
}
