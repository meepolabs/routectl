//! The over-cap excerpt of [`super::sanitize_upstream_body`] is scrubbed
//! before it is cut, always ends in the truncation marker, and an at-or-under
//! cap body passes through as it was.

use super::{
    sanitize_for_log, sanitize_upstream_body, sanitize_upstream_body_with_cap,
    upstream_excerpt_for_log,
};
use crate::{MAX_ERROR_BODY_BYTES, MAX_LOG_BODY_EXCERPT, UpstreamBody};

const SENTINEL: &str = "excerpt-sentinel-kept";
const MARKER: &str = "... [truncated]";
const KEY: &str = "sk-ant-api03-SECRETBODY0123456789abcdefghij";

/// A three-segment JWT whose length alone runs past the excerpt cut.
fn long_jwt() -> String {
    let segment = "abcDEF1234".repeat(30);
    format!("eyJ{segment}.{segment}.{}", &segment[..40])
}

/// `(name, body, planted)`: each credential sits where a cut at
/// [`MAX_LOG_BODY_EXCERPT`] characters splits it short of its scrub rule.
fn planted_over_cap() -> Vec<(&'static str, String, &'static str)> {
    let key_at = MAX_LOG_BODY_EXCERPT - 23;
    let head = format!("{SENTINEL} ");
    let pad = "x".repeat(key_at - head.len() - 1);
    let straddling = format!("{head}{pad} {KEY} tail");
    assert_eq!(straddling.find(KEY), Some(key_at), "key offset");
    let jwt = long_jwt();
    let with_jwt = format!("{SENTINEL} rejected token {jwt} tail");
    assert!(
        with_jwt.len() > MAX_LOG_BODY_EXCERPT,
        "jwt runs past the cut"
    );
    vec![
        ("key straddling the cut", straddling, "sk-ant-api03-"),
        ("jwt longer than the cut", with_jwt, "eyJ"),
    ]
}

#[test]
fn over_cap_excerpt_redacts_a_credential_the_cut_would_split() {
    for (row, body, planted) in planted_over_cap() {
        let excerpt = sanitize_upstream_body(&body);

        assert!(!excerpt.contains(planted), "{row}: planted in {excerpt:?}");
        assert!(excerpt.contains("[redacted]"), "{row}: {excerpt:?}");
        assert!(excerpt.contains(SENTINEL), "{row}: sentinel lost");
        assert!(excerpt.ends_with(MARKER), "{row}: marker missing");
    }
}

#[test]
fn over_cap_json_excerpt_never_parses() {
    let jwt = long_jwt();
    let rows = [
        (
            "longer than the cut after the scrub",
            format!(r#"{{"error":{{"message":"{}"}}}}"#, "m".repeat(600)),
        ),
        (
            "shrunk under the cut by the scrub",
            format!(r#"{{"error":{{"message":"{SENTINEL} {jwt}"}}}}"#),
        ),
    ];
    for (row, body) in rows {
        assert!(body.len() > MAX_LOG_BODY_EXCERPT, "{row}: over the cap");
        assert!(
            UpstreamBody::new(body.as_str()).parsed().is_some(),
            "{row}: the input itself parses"
        );

        let excerpt = sanitize_upstream_body(&body);

        assert!(excerpt.ends_with(MARKER), "{row}: {excerpt:?}");
        assert!(
            UpstreamBody::new(excerpt.as_str()).parsed().is_none(),
            "{row}: the excerpt parses: {excerpt:?}"
        );
    }
}

#[test]
fn at_or_under_cap_body_passes_through_unscrubbed() {
    let at_cap = format!("{KEY} {}", "y".repeat(MAX_LOG_BODY_EXCERPT - KEY.len() - 1));
    assert_eq!(at_cap.len(), MAX_LOG_BODY_EXCERPT);
    let rows = [
        ("short", format!("  {SENTINEL} {KEY}  ")),
        ("exactly at the cap", at_cap),
        ("json", format!(r#"{{"error":{{"message":"{KEY}"}}}}"#)),
    ];
    for (row, body) in rows {
        assert_eq!(sanitize_upstream_body(&body), body.trim(), "{row}");
    }
}

#[test]
fn excerpt_cut_counts_chars_of_the_scrubbed_text() {
    let body = format!("{KEY} {}", "z".repeat(40));

    let excerpt = sanitize_upstream_body_with_cap(&body, 20);

    assert_eq!(excerpt, format!("[redacted] {}{MARKER}", "z".repeat(9)));
}

/// `SENTINEL`, then an ARN padded so `tail` starts at byte `tail_at`, then
/// `tail`. The scrub collapses the ARN to `[redacted]`, which pulls whatever
/// follows it to the front of the excerpt.
fn arn_then(tail_at: usize, tail: &str) -> String {
    let head = format!("{SENTINEL} arn:aws:iam::123456789012:role/");
    let body = format!("{head}{} {tail}", "r".repeat(tail_at - head.len() - 1));
    assert_eq!(body.find(tail), Some(tail_at), "tail offset");
    body
}

/// A JWT whose third segment the error-body bound leaves 5 characters of,
/// short of the scrub's 10-character segment floor.
fn jwt_split_by_the_bound() -> (usize, String) {
    let segment = "abcDEF1234".repeat(3);
    let jwt = format!("eyJ{segment}.{segment}.{segment}");
    let kept = "eyJ".len() + 2 * (segment.len() + 1) + 5;
    (MAX_ERROR_BODY_BYTES - kept, jwt)
}

#[test]
fn excerpt_drops_a_credential_the_error_body_bound_splits() {
    let (jwt_at, jwt) = jwt_split_by_the_bound();
    const QUOTED: &str = r#""api_key":"planted one planted two""#;
    let quoted_at = MAX_ERROR_BODY_BYTES - (QUOTED.len() - 4);
    let rows = [
        (
            "jwt whose last segment the bound leaves short",
            arn_then(jwt_at, &jwt),
            "eyJ",
            format!("{SENTINEL} [redacted]{MARKER}"),
        ),
        (
            "quoted value the bound cuts after a space inside it",
            arn_then(quoted_at, QUOTED),
            "planted",
            format!(r#"{SENTINEL} [redacted] "api_key":"[redacted]{MARKER}"#),
        ),
    ];
    for (row, body, planted, want) in rows {
        assert!(body.len() > MAX_ERROR_BODY_BYTES, "{row}: over the bound");

        let excerpt = sanitize_upstream_body(&body);

        assert!(!excerpt.contains(planted), "{row}: fragment in {excerpt:?}");
        assert_eq!(excerpt, want, "{row}");
    }
}

#[test]
fn excerpt_of_one_token_filling_the_error_body_bound_is_the_marker_alone() {
    let segment = "abcDEF1234".repeat(3);
    let body = format!(
        "eyJ{}.{segment}.{segment}",
        "a".repeat(MAX_ERROR_BODY_BYTES)
    );

    let excerpt = sanitize_upstream_body(&body);

    assert_eq!(excerpt, MARKER);
}

#[test]
fn excerpt_keeps_the_last_token_of_a_body_the_error_body_bound_does_not_cut() {
    const TAIL: &str = "last-token-kept";
    let rows = [
        ("between the cap and the bound", arn_then(2000, TAIL)),
        (
            "exactly at the bound",
            arn_then(MAX_ERROR_BODY_BYTES - TAIL.len(), TAIL),
        ),
    ];
    for (row, body) in rows {
        assert!(
            body.len() <= MAX_ERROR_BODY_BYTES,
            "{row}: within the bound"
        );

        let excerpt = sanitize_upstream_body(&body);

        assert_eq!(
            excerpt,
            format!("{SENTINEL} [redacted] {TAIL}{MARKER}"),
            "{row}"
        );
    }
}

/// A JSON envelope `len` characters long whose message ends in a vendor key
/// the scrub collapses, so the scrubbed text is far shorter than `len`.
fn envelope_with_key(len: usize) -> String {
    let open = r#"{"error":{"message":"ref sk-ant-api03-"#;
    let close = r#""}}"#;
    let body = format!(
        "{open}{}{close}",
        "S".repeat(len - open.len() - close.len())
    );
    assert_eq!(body.chars().count(), len);
    body
}

#[test]
fn log_excerpt_at_or_under_its_cut_is_the_scrubbed_filtered_text() {
    let json_at_cut = envelope_with_key(256);
    let multibyte = format!("caf\u{e9} {}", "\u{e9}".repeat(251));
    let control = "line\r\nnext\u{1b}[31m".to_string();
    let rows = [
        (
            "json at the cut",
            json_at_cut,
            r#"{"error":{"message":"ref [redacted]"}}"#.to_string(),
        ),
        (
            "multibyte at the cut",
            multibyte.clone(),
            sanitize_for_log(&multibyte),
        ),
        (
            "control characters",
            control.clone(),
            sanitize_for_log(&control),
        ),
        (
            "bearer token after a tab",
            "denied Bearer\tplantedTok3n9xyz".to_string(),
            "denied Bearer?[redacted]".to_string(),
        ),
    ];
    for (row, text, want) in rows {
        assert!(text.chars().count() <= 256, "{row}: at or under the cut");

        assert_eq!(upstream_excerpt_for_log(&text), want, "{row}");
    }
}

#[test]
fn log_excerpt_past_its_cut_ends_in_the_marker_even_when_the_scrub_shrinks_it() {
    let text = envelope_with_key(257);

    let excerpt = upstream_excerpt_for_log(&text);

    assert_eq!(
        excerpt,
        format!(r#"{{"error":{{"message":"ref [redacted]"}}}}{MARKER}"#)
    );
    assert!(UpstreamBody::new(excerpt.as_str()).parsed().is_none());
}

/// `{"password":"a","x":"<pad>"}` padded so the scrub grows it to exactly
/// 256 characters, then a short tail: under the cut before the scrub, past
/// it after, and a cut at 256 would leave exactly the scrubbed object.
fn object_the_scrub_grows_to_the_cut() -> String {
    let open = r#"{"password":"a","x":""#;
    let close = r#""}"#;
    let pad = "p".repeat(256 - "[redacted]".len() + 1 - open.len() - close.len());
    format!("{open}{pad}{close} t")
}

#[test]
fn log_excerpt_the_scrub_grows_past_its_cut_ends_in_the_marker() {
    let rows = [
        ("short credential values", "secret=q1 ".repeat(25), "q1"),
        (
            "json object the scrub grows to the cut",
            object_the_scrub_grows_to_the_cut(),
            r#""a""#,
        ),
    ];
    for (row, text, planted) in rows {
        assert!(text.chars().count() <= 256, "{row}: under the cut");
        assert!(
            crate::scrub_upstream_text(&text).chars().count() > 256,
            "{row}: the scrub grows it past the cut"
        );

        let excerpt = upstream_excerpt_for_log(&text);

        assert!(!excerpt.contains(planted), "{row}: fragment in {excerpt:?}");
        let uncut = UpstreamBody::new(text.as_str())
            .parsed()
            .map(|p| p.value().clone());
        let cut = UpstreamBody::new(excerpt.as_str())
            .parsed()
            .map(|p| p.value().clone());
        assert!(
            cut.is_none() || cut == uncut,
            "{row}: the excerpt parses as {cut:?}, the uncut text as {uncut:?}"
        );
        assert!(excerpt.ends_with(MARKER), "{row}: {excerpt:?}");
    }
}

#[test]
fn excerpt_bound_inside_a_multibyte_char_drops_the_split_credential() {
    let (jwt_at, jwt) = jwt_split_by_the_bound();
    let kept = MAX_ERROR_BODY_BYTES - jwt_at;
    let tail = format!("{}\u{4e2d}\u{4e2d} tail", &jwt[..kept - 1]);
    let body = arn_then(jwt_at, &tail);
    let straddling = body.find('\u{4e2d}').expect("cjk");
    assert!(
        straddling < MAX_ERROR_BODY_BYTES && MAX_ERROR_BODY_BYTES < straddling + 3,
        "a 3-byte char straddles the bound"
    );

    let excerpt = sanitize_upstream_body(&body);

    assert!(!excerpt.contains("eyJ"), "fragment in {excerpt:?}");
    assert_eq!(excerpt, format!("{SENTINEL} [redacted]{MARKER}"));
}

/// The Cloud Code onboarding excerpt as its lane composes it: the 512-char
/// first pass, then the 256-char log excerpt of that.
fn onboarding_excerpt(body: &str) -> String {
    upstream_excerpt_for_log(&sanitize_upstream_body(body))
}

/// `(name, body, planted run, want)`: a body over 512 bytes whose last value
/// is credential-named and never closed, so the first pass ends in
/// `[redacted]` and its marker directly after it.
fn open_value_past_the_first_cut() -> Vec<(&'static str, String, &'static str, String)> {
    let a = "a".repeat(173);
    vec![
        (
            "single-quoted form value",
            format!("{a} password='{}", "b".repeat(330)),
            "bb",
            format!("{a} password='[redacted]{MARKER}"),
        ),
        (
            "double-quoted json key",
            format!(r#"{a} {{"api_key":"{}"#, "Q".repeat(400)),
            "QQ",
            format!(r#"{a} {{"api_key":"[redacted]{MARKER}"#),
        ),
        (
            "unquoted form value",
            format!("{a} token={}", "Z".repeat(400)),
            "ZZ",
            format!("{a} token=[redacted]{MARKER}"),
        ),
    ]
}

#[test]
fn log_excerpt_of_a_first_pass_cut_inside_a_redacted_value_keeps_one_marker() {
    for (row, body, planted, want) in open_value_past_the_first_cut() {
        assert!(
            body.len() > MAX_LOG_BODY_EXCERPT,
            "{row}: over the first cut"
        );

        let excerpt = onboarding_excerpt(&body);

        assert!(
            excerpt.ends_with(MARKER) && excerpt.matches(MARKER).count() == 1,
            "{row}: not exactly one trailing marker in {excerpt:?}"
        );
        assert!(!excerpt.contains(planted), "{row}: fragment in {excerpt:?}");
        assert_eq!(excerpt, want, "{row}");
    }
}

#[test]
fn log_excerpt_of_a_short_first_pass_cut_the_scrub_leaves_alone_is_unchanged() {
    let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "\u{4e2d}".repeat(200));
    assert!(body.len() > MAX_LOG_BODY_EXCERPT && body.chars().count() < 241);

    let excerpt = onboarding_excerpt(&body);

    assert!(excerpt.chars().count() <= 256);
    assert_eq!(
        excerpt,
        format!(r#"{{"error":{{"message":"{}"}}}}{MARKER}"#, "?".repeat(200))
    );
}

#[test]
fn log_excerpt_of_a_marked_text_the_scrub_shrinks_under_its_cut_keeps_one_marker() {
    let text = format!("{KEY}{} kept{MARKER}", "k".repeat(300));
    assert!(text.chars().count() > 256, "past the cut before the scrub");

    let excerpt = upstream_excerpt_for_log(&text);

    assert_eq!(excerpt, format!("[redacted] kept{MARKER}"));
}

#[test]
fn upstream_error_rendering_of_a_carried_cut_inside_a_redacted_value_keeps_one_marker() {
    for (row, body, planted, _) in open_value_past_the_first_cut() {
        let err = crate::Error::upstream("p", 400, sanitize_upstream_body(&body));

        let display = format!("{err}");
        let debug = format!("{err:?}");

        assert!(
            display.ends_with(MARKER) && display.matches(MARKER).count() == 1,
            "{row}: display {display:?}"
        );
        assert!(
            debug.contains(&format!("[redacted]{MARKER}\"")) && debug.matches(MARKER).count() == 1,
            "{row}: debug {debug:?}"
        );
        assert!(
            !display.contains(planted) && !debug.contains(planted),
            "{row}: fragment"
        );
    }
}

/// A flat Bedrock-style validation envelope whose message ends in
/// `Bearer {token}`, plus 200 empty `*_token` keys the credential-key
/// redaction grows by 16 bytes each, padded so the carried text routectl
/// builds from it (redacted, then cut at [`MAX_ERROR_BODY_BYTES`] bytes)
/// keeps exactly `kept` chars of the token before the marker. Returns the
/// raw body and the carried text.
fn bedrock_envelope_cut_inside_bearer(token: &str, kept: usize) -> (String, String) {
    let envelope = |pad: usize| {
        let mut map = serde_json::Map::new();
        map.insert("__type".into(), "ValidationException".into());
        for i in 0..200 {
            map.insert(format!("k{i:03}_token"), "".into());
        }
        let message = format!("{} Bearer {token}", "x".repeat(pad));
        map.insert("message".into(), message.into());
        serde_json::Value::Object(map)
    };
    let carry = |value: &serde_json::Value| {
        super::sanitize_upstream_body_with_byte_cap(
            &super::redact_prompts_with_flag(value, false).to_string(),
            MAX_ERROR_BODY_BYTES,
        )
    };
    let unpadded = super::redact_prompts_with_flag(&envelope(0), false).to_string();
    let token_at = unpadded
        .find(token)
        .expect("token in the redacted envelope");
    let pad = MAX_ERROR_BODY_BYTES - MARKER.len() - kept - token_at;
    let value = envelope(pad);
    let raw = value.to_string();
    let carried = carry(&value);
    assert!(
        raw.len() < MAX_ERROR_BODY_BYTES,
        "raw body is under the cut"
    );
    assert!(
        carried.ends_with(&format!(" Bearer {}{MARKER}", &token[..kept])),
        "the cut keeps {kept} token chars: {:?}",
        &carried[carried.len() - 60..]
    );
    (raw, carried)
}

/// Every 3-char window of `kept` that appears in `text`.
fn surviving_windows<'a>(kept: &'a str, text: &str) -> Vec<&'a str> {
    (0..kept.len().saturating_sub(2))
        .map(|i| &kept[i..i + 3])
        .filter(|w| text.contains(w))
        .collect()
}

#[test]
fn upstream_error_display_of_an_envelope_cut_inside_a_bearer_token_redacts_it() {
    const PLAIN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ";
    const DIGIT: &str = "a1b2c3d4e5f6g7h8i9j0";
    let rows = [
        ("no digit, 17 chars kept", PLAIN, 17),
        ("no digit, 18 chars kept", PLAIN, 18),
        ("no digit, 19 chars kept", PLAIN, 19),
        ("digit, 3 chars kept", DIGIT, 3),
        ("digit, 4 chars kept", DIGIT, 4),
        ("digit, 5 chars kept", DIGIT, 5),
    ];
    for (row, token, kept) in rows {
        let (_, carried) = bedrock_envelope_cut_inside_bearer(token, kept);
        let err = crate::Error::upstream("p", 400, carried);

        let display = format!("{err}");

        let survivors = surviving_windows(&token[..kept], &display);
        assert!(survivors.is_empty(), "{row}: {survivors:?} survive");
        assert!(
            display.ends_with(&format!(" Bearer [redacted]{MARKER}"))
                && display.matches(MARKER).count() == 1,
            "{row}: {:?}",
            &display[display.len() - 60..]
        );
    }
}

#[test]
fn upstream_error_display_of_a_raw_body_ending_in_a_cut_bearer_token_redacts_it() {
    let body = format!("denied Bearer abcdefghijklmnopq{MARKER}");
    let err = crate::Error::upstream("p", 400, body.as_str());

    let display = format!("{err}");

    assert_eq!(
        crate::scrub_upstream_text(&body),
        "denied Bearer [redacted] [truncated]"
    );
    assert_eq!(
        display,
        format!("provider `p`: upstream HTTP 400: denied Bearer [redacted]{MARKER}")
    );
}

/// `(shape, text before the credential, credential)`: one row per scrub
/// rule shape, each credential long enough for its rule.
const MARKED_SHAPES: [(&str, &str, &str); 9] = [
    (
        "bearer, no digit",
        "denied Bearer ",
        "abcdefghijklmnopqrstuvwxyzABCD",
    ),
    ("bearer, digit", "denied Bearer ", "a1b2c3d4e5f6g7h8"),
    (
        "session token",
        "x-amz-security-token: ",
        "FQoGZXIvYXdzEJr//////////wEaDK7sess",
    ),
    ("arn", "role ", "arn:aws:iam::123456789012:role/SecretRole"),
    ("vendor key", "key ", KEY),
    ("form value", "?x=1&token=", "ZpQ8sVx2LmN4"),
    ("quoted value", r#"{"api_key":""#, "Qw9Er7Ty5Ui3"),
    ("account id", "account ", "123456789012"),
    (
        "url userinfo",
        "see https://",
        "alice:s3cr3tpw@host.example",
    ),
];

/// Each maximal run of ASCII letters and digits, 2+ chars long.
fn alnum_runs(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|run| run.len() >= 2)
        .collect()
}

/// The runs of `shown` that `plain` holds fewer of: text the scrub let
/// through that the plain scrub of the same input redacted.
fn runs_beyond<'a>(shown: &'a str, plain: &str) -> Vec<&'a str> {
    let mut budget = alnum_runs(plain);
    alnum_runs(shown)
        .into_iter()
        .filter(|run| match budget.iter().position(|r| r == run) {
            Some(i) => {
                budget.swap_remove(i);
                false
            }
            None => true,
        })
        .collect()
}

#[test]
fn marked_excerpt_scrub_never_shows_what_the_plain_scrub_redacts() {
    let mut failures = Vec::new();
    let mut inputs = 0;
    for (shape, before, credential) in MARKED_SHAPES {
        for cut in 1..=credential.len() {
            let input = format!("{SENTINEL} {before}{}{MARKER}", &credential[..cut]);
            inputs += 1;

            let shown = super::scrub_marked_excerpt(&input);

            let plain = crate::scrub_upstream_text(&input);
            if plain.ends_with(MARKER) && shown != plain {
                failures.push(format!("{shape} cut {cut}: {shown:?} is not {plain:?}"));
            }
            match shown.strip_suffix(MARKER) {
                Some(head) if !head.contains(MARKER) => {
                    let beyond = runs_beyond(head, &plain);
                    if !beyond.is_empty() {
                        failures.push(format!("{shape} cut {cut}: {beyond:?} in {shown:?}"));
                    }
                }
                _ => failures.push(format!("{shape} cut {cut}: not one marker in {shown:?}")),
            }
        }
    }
    assert_eq!(inputs, 228, "table size");
    assert!(
        failures.is_empty(),
        "{} of {inputs}:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn marked_excerpt_scrub_is_idempotent() {
    let mut rows: Vec<(String, String)> = MARKED_SHAPES
        .iter()
        .map(|(shape, before, credential)| {
            let cut = credential.len() / 2 + 1;
            (
                (*shape).to_string(),
                format!("{before}{}{MARKER}", &credential[..cut]),
            )
        })
        .collect();
    rows.push((
        "bearer, already redacted".into(),
        format!("Bearer [redacted]{MARKER}"),
    ));
    rows.push((
        "quoted, already redacted".into(),
        format!("password='[redacted]{MARKER}"),
    ));
    for (row, input) in rows {
        let once = super::scrub_marked_excerpt(&input).into_owned();

        let twice = super::scrub_marked_excerpt(&once).into_owned();

        assert_eq!(twice, once, "{row}");
    }
    assert_eq!(
        super::scrub_marked_excerpt(&format!("Bearer [redacted]{MARKER}")),
        format!("Bearer [redacted]{MARKER}")
    );
    assert_eq!(
        super::scrub_marked_excerpt(&format!("password='[redacted]{MARKER}")),
        format!("password='[redacted]{MARKER}")
    );
}
