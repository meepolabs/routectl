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
        "GET https://gw.example/v1/chat?api_key=OPAQUE1&model=x failed",
        "grant_type=refresh_token&refresh_token=OPAQUE1",
        "Authorization: Bearer OPAQUE1 was rejected",
        r#"{"password":"x\\","model":"m"}"#,
        "redirect_token=a://user:p&ssw0rd@host/x",
        "api_key=https://x_token=://u,p4ss@h/x",
        "api_key=OPAQUE1,model=gpt-4o,max_tokens=5",
        "Config(api_key=OPAQUE1)",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let session = key("Tok", "Ab+/9=", 6);
    rows.push(format!("x-amz-security-token:{session}\nhost"));
    rows.push(format!(r#"{{\"x-amz-security-token\":\"{session}\"}}"#));
    rows.push(format!(
        "api_key=.{}&{}",
        key("bedrock-api-key-", "h", 24),
        "k".repeat(24)
    ));
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
fn value_scrub_rewrites_string_leaves_and_credential_keys_and_keeps_other_leaves() {
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
        "[redacted]": "plain"
    });
    assert_eq!(value, expected);
}

#[test]
fn value_scrub_judges_a_leaf_with_its_key() {
    let rows = [
        (
            "account id string under accountId",
            serde_json::json!({"error": {"accountId": "123456789012"}}),
            serde_json::json!({"error": {"accountId": "[redacted]"}}),
        ),
        (
            "account id number under accountId",
            serde_json::json!({"accountId": 123_456_789_012_u64}),
            serde_json::json!({"accountId": "[redacted]"}),
        ),
        (
            "capitalized Account key",
            serde_json::json!({"Account": "123456789012"}),
            serde_json::json!({"Account": "[redacted]"}),
        ),
        (
            "arn-shaped key",
            serde_json::json!({"arn:aws:iam::123456789012:role/ops": "x"}),
            serde_json::json!({"[redacted]": "x"}),
        ),
        (
            "credential-named key",
            serde_json::json!({"api_key": "abc123", "model": "m"}),
            serde_json::json!({"api_key": "[redacted]", "model": "m"}),
        ),
        (
            "tokenizer-symbol key keeps its value",
            serde_json::json!({"eos_token": "</s>"}),
            serde_json::json!({"eos_token": "</s>"}),
        ),
        (
            "non-account number under account key",
            serde_json::json!({"accountCount": 3, "account_name": "ops"}),
            serde_json::json!({"accountCount": 3, "account_name": "ops"}),
        ),
        (
            "12 digits under a key without account",
            serde_json::json!({"requestId": "123456789012"}),
            serde_json::json!({"requestId": "123456789012"}),
        ),
    ];
    for (name, mut value, expected) in rows {
        scrub_upstream_value(&mut value);
        assert_eq!(value, expected, "{name}");
    }
}

/// Every planted value the text scrub removes from the serialized body is
/// also absent after the value scrub.
#[test]
fn value_scrub_redacts_what_the_text_scrub_redacts_in_the_serialized_body() {
    let planted = [
        "123456789012",
        "arn:aws:iam::123456789012:role/ops",
        "abc123",
        "hunter2pass",
        "Pw0rd123",
    ];
    let body = serde_json::json!({
        "error": {
            "accountId": "123456789012",
            "Account": 123_456_789_012_u64,
            "message": "fetch https://alice:hunter2pass@example.com/x failed",
            "details": [{"api_key": "abc123"}, {"password": "Pw0rd123"}]
        },
        "arn:aws:iam::123456789012:role/ops": "x"
    });
    let text_scrubbed = scrub_upstream_text(&body.to_string()).into_owned();
    let mut value = body;
    scrub_upstream_value(&mut value);
    let value_scrubbed = value.to_string();
    for planted in planted {
        assert!(
            !text_scrubbed.contains(planted),
            "fixture control: text scrub kept {planted:?} in {text_scrubbed}"
        );
        assert!(
            !value_scrubbed.contains(planted),
            "value scrub kept {planted:?} in {value_scrubbed}"
        );
    }
}

/// The alphanumeric runs of 6+ characters in `text`.
fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() >= 6)
        .collect()
}

#[test]
fn value_scrub_matches_the_text_scrub_over_the_planted_fixture() {
    for row in planted_fixture() {
        for placement in [
            serde_json::json!({"message": row.clone()}),
            serde_json::json!({row.clone(): "x"}),
            serde_json::json!({"error": {"accountId": row.clone()}}),
        ] {
            let serialized = placement.to_string();
            let text_scrubbed = scrub_upstream_text(&serialized).into_owned();
            let mut value = placement;
            scrub_upstream_value(&mut value);
            let value_scrubbed = value.to_string();
            for word in words(&row) {
                if !text_scrubbed.contains(word) {
                    assert!(
                        !value_scrubbed.contains(word),
                        "{row:?}: value scrub kept {word:?} in {value_scrubbed}"
                    );
                }
            }
        }
    }
}

#[test]
fn credential_shaped_keys_are_redacted_on_the_first_pass() {
    let jwt = format!(
        "{}.{}.{}",
        key("eyJ", "a", 40),
        "b".repeat(60),
        "c".repeat(43)
    );
    let proj = key("sk-proj-", "p", 156);
    let rows = [
        (
            "jwt-shaped key",
            format!(r#"{{"{jwt}":"expired"}}"#),
            jwt.clone(),
        ),
        (
            "project-key-shaped key",
            format!(r#"{{"{proj}":"quota exceeded"}}"#),
            proj.clone(),
        ),
        (
            "account id inside a credential-named key",
            "{'account_123456789012_secret': 'x'}".to_owned(),
            "123456789012".to_owned(),
        ),
    ];
    for (name, input, planted) in &rows {
        assert_redacts(name, input, planted);
        let once = scrub_upstream_text(input).into_owned();
        assert_eq!(
            scrub_upstream_text(&once),
            once,
            "{name}: second pass changed"
        );
    }
    assert_eq!(
        scrub_upstream_text("{'account_123456789012_secret': 'x'}"),
        "{'account_[redacted]_secret': '[redacted]'}"
    );
}

#[test]
fn rules_matching_at_one_position_all_apply() {
    assert_eq!(
        scrub_upstream_text("account_123456789012_secret=OPAQUE1 rejected"),
        "account_[redacted]_secret=[redacted] rejected"
    );
}

#[test]
fn redacts_unquoted_query_and_form_credential_values() {
    let rows = [
        (
            "query string",
            "GET https://gw.example/v1/chat?api_key=OPAQUE1&model=x failed",
            "GET https://gw.example/v1/chat?api_key=[redacted]&model=x failed",
        ),
        (
            "form body",
            "grant_type=refresh_token&refresh_token=OPAQUE1",
            "grant_type=refresh_token&refresh_token=[redacted]",
        ),
        (
            "value ends at whitespace",
            "request token=OPAQUE1 rejected",
            "request token=[redacted] rejected",
        ),
        (
            "value ends at a quote",
            r#"{"url":"https://x/v1?key=1&access_token=OPAQUE1"}"#,
            r#"{"url":"https://x/v1?key=1&access_token=[redacted]"}"#,
        ),
        (
            "value ends at an escaped quote",
            r#"{\"url\":\"https://x/v1?client_secret=OPAQUE1\"}"#,
            r#"{\"url\":\"https://x/v1?client_secret=[redacted]\"}"#,
        ),
        (
            "unquoted value at the end",
            "api_key=unset",
            "api_key=[redacted]",
        ),
    ];
    for (name, input, expected) in rows {
        assert_redacts(name, input, "OPAQUE1");
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

#[test]
fn redacts_the_token_after_bearer() {
    let long = "abcdefghijklmnopqrstuvwxyz";
    let rows = [
        (
            "authorization header echo",
            "Authorization: Bearer OPAQUE1 was rejected".to_owned(),
            "Authorization: Bearer [redacted] was rejected".to_owned(),
        ),
        (
            "quoted header value",
            r#"{"headers":"authorization: bearer OPAQUE1=="}"#.to_owned(),
            r#"{"headers":"authorization: bearer [redacted]"}"#.to_owned(),
        ),
        (
            "long token without a digit",
            format!("BEARER {long}"),
            "BEARER [redacted]".to_owned(),
        ),
    ];
    for (name, input, expected) in &rows {
        assert_eq!(scrub_upstream_text(input), *expected, "{name}");
    }
}

#[test]
fn query_form_and_bearer_rules_leave_ordinary_text() {
    let rows = [
        ("sampling parameters", "top_p=0.9&max_tokens=5"),
        (
            "query without credentials",
            "GET /v1/chat?model=x&stream=true",
        ),
        ("bearer in prose", "the bearer of bad news"),
        ("short bearer word with a digit", "Bearer v2 is required"),
        (
            "auth scheme name",
            r#"WWW-Authenticate: Bearer realm="api""#,
        ),
        ("credential key with spaced equals", "api_key = unset"),
        ("empty form value", "api_key=&model=x"),
        ("grant type naming a token", "grant_type=refresh_token"),
    ];
    for (name, input) in rows {
        assert_clean(name, input);
    }
}

#[test]
fn tokenizer_symbol_keys_keep_their_values() {
    let rows = [
        ("eos_token", "{'eos_token': '</s>', 'bos_token': '<s>'}"),
        (
            "pad and unk",
            r#"{"pad_token":"<pad>","unk_token":"<unk>"}"#,
        ),
        ("stop_token", r#"{"stop_token":"<|eot_id|>"}"#),
        (
            "sep cls mask",
            r#"{"sep_token":"[SEP]","cls_token":"[CLS]","mask_token":"[MASK]"}"#,
        ),
        ("upper case", r#"{"EOS_TOKEN":"</s>"}"#),
        ("dotted path", r#"{"tokenizer.eos_token":"</s>"}"#),
        ("form pair", "eos_token=</s>"),
    ];
    for (name, input) in rows {
        assert_clean(name, input);
    }
}

/// Accepted over-redaction: a bare `token` key and any other `*_token` key
/// are judged credential-named, whatever the value means.
#[test]
fn other_token_keys_stay_redacted() {
    let rows = [
        ("bare token", r#"{"token":"Hello","logprob":-0.1}"#, "Hello"),
        ("page token", r#"{"next_page_token":"CgVhYmM"}"#, "CgVhYmM"),
        (
            "request token",
            r#"{"clientRequestToken":"job-1234"}"#,
            "job-1234",
        ),
        (
            "prefix of a vocabulary key",
            r#"{"xeos_token":"</s>"}"#,
            "</s>",
        ),
    ];
    for (name, input, planted) in rows {
        assert_redacts(name, input, planted);
    }
}

#[test]
fn an_even_backslash_run_closes_a_quoted_value() {
    let rows = [
        (
            "escaped backslash before the closing quote",
            r#"{"password":"x\\","model":"m"}"#,
            r#"{"password":"[redacted]","model":"m"}"#,
        ),
        (
            "two escaped backslashes",
            r#"{"password":"x\\\\","model":"m"}"#,
            r#"{"password":"[redacted]","model":"m"}"#,
        ),
        (
            "escaped quote stays inside",
            r#"{"password":"x\\\"y","model":"m"}"#,
            r#"{"password":"[redacted]","model":"m"}"#,
        ),
    ];
    for (name, input, expected) in rows {
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

/// A bedrock API key holding an unencoded `&`, which ends a form value but
/// not the key; assembled at runtime.
fn bedrock_key_with_ampersand() -> (String, String) {
    let tail = "k".repeat(24);
    (format!("{}&{tail}", key("bedrock-api-key-", "h", 24)), tail)
}

/// Each row's credential starts inside another rule's span and runs past
/// that span's end; one row per such rule.
#[test]
fn a_credential_starting_inside_a_span_is_redacted_to_its_own_end() {
    let (bedrock, tail) = bedrock_key_with_ampersand();
    let session = "A".repeat(20);
    let rows = [
        (
            "form value",
            format!("api_key=.{bedrock}"),
            "api_key=[redacted]".to_owned(),
            tail.clone(),
        ),
        (
            "arn",
            "arn:aws:s3:::b/https://u:p,w0rdtail@host/x".to_owned(),
            "[redacted]@host/x".to_owned(),
            "w0rdtail".to_owned(),
        ),
        (
            "session token value",
            format!("x-amz-security-token={session}/{bedrock}"),
            "x-amz-security-token=[redacted]".to_owned(),
            tail.clone(),
        ),
        (
            "bearer token",
            format!("Bearer abc123/{bedrock}"),
            "Bearer [redacted]".to_owned(),
            tail.clone(),
        ),
    ];
    for (name, input, expected, planted) in &rows {
        assert_redacts(name, input, planted);
        assert_eq!(scrub_upstream_text(input), *expected, "{name}");
    }
}

/// The userinfo rule, probing inside the form value, carries the redaction
/// past a delimiter the userinfo holds.
#[test]
fn userinfo_inside_a_form_value_is_redacted_on_the_first_pass() {
    let rows = [
        (
            "userinfo holding an ampersand",
            "redirect_token=a://user:p&ssw0rd@host/x",
            "redirect_token=[redacted]@host/x",
            "ssw0rd",
        ),
        (
            "userinfo holding a comma behind a nested key",
            "api_key=https://x_token=://u,p4ss@h/x",
            "api_key=[redacted]@h/x",
            "p4ss",
        ),
    ];
    for (name, input, expected, planted) in rows {
        assert_redacts(name, input, planted);
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

/// A URL with no path in a form value ends at the form delimiter like any
/// other value, the same whether the value stands alone or nested.
#[test]
fn a_form_value_holding_a_url_keeps_the_text_after_it() {
    let rows = [
        (
            "comma list",
            "api_key=https://h.example.com,model=gpt-4o,max_tokens=5",
            "api_key=[redacted],model=gpt-4o,max_tokens=5",
        ),
        (
            "query string",
            "?id_token=https://issuer.example.com&model=gpt-4o&max_tokens=5 rejected",
            "?id_token=[redacted]&model=gpt-4o&max_tokens=5 rejected",
        ),
        (
            "python repr",
            "Config(authorization_endpoint=https://auth.example.com), retry=3",
            "Config(authorization_endpoint=[redacted]), retry=3",
        ),
        (
            "quoted set entry",
            "{'redirect_token=https://h'} next",
            "{'redirect_token=[redacted]'} next",
        ),
        (
            "port before a comma",
            "api_key=https://h.example.com:443,model=m",
            "api_key=[redacted],model=m",
        ),
        (
            "host holding a comma",
            "x_token=://cb,s3cr3tpart.example/x",
            "x_token=[redacted],s3cr3tpart.example/x",
        ),
        (
            "host holding a comma behind a nested key",
            "api_key=https://x_token=://cb,s3cr3tpart.example/x",
            "api_key=[redacted],s3cr3tpart.example/x",
        ),
    ];
    for (name, input, expected) in rows {
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

/// A name closing an ARN, session-token value or bearer token takes no
/// value past that span's end, so the quotes and separators after it stay.
/// One row per fencing span.
#[test]
fn a_name_closing_a_span_takes_no_value_past_it() {
    let session = "A".repeat(20);
    let rows = [
        (
            "arn closed by a credential-named key",
            r#"{"Resource": "arn:aws:s3:::bucket/session_token=", "Other": "visible"}"#.to_owned(),
            r#"{"Resource": "[redacted]", "Other": "visible"}"#.to_owned(),
        ),
        (
            "bearer token closed by a credential-named key",
            r#"Bearer abc123/xyzToken=", "model": "m"}"#.to_owned(),
            r#"Bearer [redacted]", "model": "m"}"#.to_owned(),
        ),
        (
            "arn closed by the bearer name",
            "arn:aws:iam::123456789012:role/bearer abc12345 is denied".to_owned(),
            "[redacted] abc12345 is denied".to_owned(),
        ),
        (
            "session token closed by a credential-named key",
            format!(r#"{{"h": "x-amz-security-token={session}/api_key=", "model": "m"}}"#),
            r#"{"h": "x-amz-security-token=[redacted]", "model": "m"}"#.to_owned(),
        ),
    ];
    for (name, input, expected) in &rows {
        assert_eq!(scrub_upstream_text(input), *expected, "{name}");
    }
}

#[test]
fn a_form_value_ends_at_a_list_or_bracket_delimiter() {
    let rows = [
        (
            "comma list",
            "api_key=OPAQUE1,model=gpt-4o,max_tokens=5",
            "api_key=[redacted],model=gpt-4o,max_tokens=5",
        ),
        (
            "python repr",
            "Config(api_key=OPAQUE1)",
            "Config(api_key=[redacted])",
        ),
        (
            "java toString",
            "{api_key=OPAQUE1, model=x}",
            "{api_key=[redacted], model=x}",
        ),
        (
            "semicolon list",
            "password=OPAQUE1;timeout=30",
            "password=[redacted];timeout=30",
        ),
    ];
    for (name, input, expected) in rows {
        assert_redacts(name, input, "OPAQUE1");
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

/// A run longer than any judged key before `=` is base64 padding, not a
/// form pair.
#[test]
fn a_long_bare_run_before_equals_is_not_a_key() {
    let blob = format!("gAAAAAB{}", "x_Y-z9".repeat(30));
    let data = "iVBORw0KGgo".repeat(15);
    let rows = [
        (
            "padding before a parenthesis",
            format!("invalid encrypted_content: {blob}== (expired)"),
        ),
        (
            "padding before a comma",
            format!("invalid encrypted_content: {blob}==, retry"),
        ),
        (
            "data url",
            format!(r#"{{"url":"data:image/png;base64,{data}=="}}"#),
        ),
    ];
    for (name, input) in &rows {
        assert_clean(name, input);
    }
    let longest = format!("{}_token", "a".repeat(122));
    assert_redacts(
        "128-char bare key is still judged",
        &format!("{longest}=OPAQUE1"),
        "OPAQUE1",
    );
}

/// A bare key longer than any judged key is judged by its tail, which holds
/// its last dotted or hyphenated segment.
#[test]
fn a_long_bare_key_is_judged_by_its_last_segment() {
    let rows = [
        (
            "underscore name",
            format!("{}_api_key=OPAQUE1 next", "a".repeat(130)),
        ),
        (
            "dotted path",
            format!("{}.access_token=OPAQUE1 next", "seg.".repeat(40)),
        ),
        (
            "hyphenated name",
            format!("x-{}-password=OPAQUE1 next", "b".repeat(130)),
        ),
    ];
    for (name, input) in &rows {
        let expected = input.replace("OPAQUE1", "[redacted]");
        assert_eq!(scrub_upstream_text(input), expected, "{name}");
    }
}

#[test]
fn value_scrub_keeps_colliding_scrubbed_keys_distinct() {
    let mut value = serde_json::json!({
        "arn:aws:iam::111111111111:role/a": "first",
        "arn:aws:iam::222222222222:role/b": "second",
        "[redacted]": "literal",
        "model": "m"
    });
    scrub_upstream_value(&mut value);
    let expected = serde_json::json!({
        "[redacted]": "literal",
        "[redacted]#2": "first",
        "[redacted]#3": "second",
        "model": "m"
    });
    assert_eq!(value, expected);
}
