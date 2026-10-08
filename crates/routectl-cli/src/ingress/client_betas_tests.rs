use routectl_core::Error;

use super::*;

#[test]
fn entries_are_split_trimmed_and_deduplicated_in_source_order() {
    let rows: &[(&str, &[&str], &[&str])] = &[
        ("clean", &["a-1"], &["a-1"]),
        ("padded", &[" a-1 "], &["a-1"]),
        ("comma-joined", &["a-1,b-2"], &["a-1", "b-2"]),
        ("padded comma-joined", &[" a-1 , b-2 "], &["a-1", "b-2"]),
        ("empty pieces", &["", " ", ",,a-1,"], &["a-1"]),
        (
            "repeat across entries",
            &["a-1", " a-1", "b-2,a-1"],
            &["a-1", "b-2"],
        ),
        (
            "CR/LF dropped",
            &["evil\r\nX-Injected: bad", "a-1", "b\nc"],
            &["a-1"],
        ),
        ("SOH dropped", &["a\u{0001}b", "a-1"], &["a-1"]),
        ("NUL dropped", &["a\u{0000}b", "a-1"], &["a-1"]),
        ("DEL dropped", &["a\u{007f}b", "a-1"], &["a-1"]),
        ("ESC dropped", &["\u{001b}[31m", "a-1"], &["a-1"]),
        ("interior space dropped", &["a b", "a-1"], &["a-1"]),
        (
            "non-ASCII letter dropped",
            &["caf\u{00e9}-1", "a-1"],
            &["a-1"],
        ),
        (
            "real flag kept",
            &["interleaved-thinking-2025-05-14"],
            &["interleaved-thinking-2025-05-14"],
        ),
    ];
    let mismatched: Vec<String> = rows
        .iter()
        .filter_map(|(name, entries, expected)| {
            let out = normalize_client_betas("test", entries.iter().copied()).expect(name);
            (out != *expected).then(|| format!("{name}: got {out:?}, want {expected:?}"))
        })
        .collect();
    assert!(mismatched.is_empty(), "mismatched rows: {mismatched:#?}");
}

#[test]
fn cap_admits_exactly_the_maximum_distinct_flags() {
    let flags: Vec<String> = (0..MAX_CLIENT_BETA_FLAGS)
        .map(|i| format!("f-{i}"))
        .collect();

    let out = normalize_client_betas("test", flags.iter().map(String::as_str)).expect("at cap");

    assert_eq!(out.len(), MAX_CLIENT_BETA_FLAGS);
}

#[test]
fn cap_rejects_one_distinct_flag_over_the_maximum() {
    let flags: Vec<String> = (0..=MAX_CLIENT_BETA_FLAGS)
        .map(|i| format!("f-{i}"))
        .collect();

    let err = normalize_client_betas("test", flags.iter().map(String::as_str))
        .expect_err("one over the cap");

    assert!(matches!(err, Error::Validation(_)), "{err:?}");
}

#[test]
fn repeats_do_not_count_toward_the_cap() {
    let mut flags: Vec<String> = (0..MAX_CLIENT_BETA_FLAGS)
        .map(|i| format!("f-{i}"))
        .collect();
    flags.extend(flags.clone());

    let out = normalize_client_betas("test", flags.iter().map(String::as_str)).expect("repeats");

    assert_eq!(out.len(), MAX_CLIENT_BETA_FLAGS);
}

#[test]
fn is_safe_beta_value_accepts_only_visible_ascii() {
    for ok in ["legit-beta", "", "with-special!@#$%^&*()chars"] {
        assert!(is_safe_beta_value(ok), "{ok:?}");
    }
    for bad in [
        "evil\r\nX-Injected: bad",
        "evil\rmid",
        "evil\nmid",
        "\revil-leading-cr",
        "\nevil-leading-lf",
        "evil-trailing\r",
        "evil-trailing\n",
        "with spaces",
        "tab\tinside",
    ] {
        assert!(!is_safe_beta_value(bad), "{bad:?}");
    }
}

const DROP_MESSAGE: &str =
    "ingress: anthropic-beta values contain a byte outside visible ASCII; dropping";

#[test]
fn dropped_flags_log_one_aggregated_warning_per_call() {
    let rows: &[(&str, &[&str], usize, usize)] = &[
        ("one unsafe piece", &["a b", "a-1"], 1, 3),
        (
            "many unsafe pieces",
            &["a b,c\td", "a\u{0001}b", "a-1", "caf\u{00e9}-12", "a b"],
            5,
            8,
        ),
    ];
    for (name, entries, dropped, value_len_max) in rows {
        let events = routectl_testkit::capture_events(|| {
            normalize_client_betas("test", entries.iter().copied()).expect(name);
        });

        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.message == DROP_MESSAGE)
            .collect();
        assert_eq!(warns.len(), 1, "{name}: {events:?}");
        assert_eq!(warns[0].level, tracing::Level::WARN, "{name}");
        assert_eq!(
            warns[0].field("dropped"),
            Some(dropped.to_string().as_str()),
            "{name}"
        );
        assert_eq!(
            warns[0].field("value_len_max"),
            Some(value_len_max.to_string().as_str()),
            "{name}"
        );
        let names: Vec<&str> = warns[0].fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["dialect", "dropped", "value_len_max"], "{name}");
    }
}

#[test]
fn clean_flags_log_no_drop_warning() {
    let events = routectl_testkit::capture_events(|| {
        normalize_client_betas("test", [" a-1 ", "b-2,a-1"]).expect("clean");
    });

    assert!(
        events.iter().all(|e| e.level != tracing::Level::WARN),
        "{events:?}"
    );
}
