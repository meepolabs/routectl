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
