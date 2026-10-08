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
    ];
    for (name, entries, expected) in rows {
        let out = normalize_client_betas("test", entries.iter().copied()).expect(name);
        assert_eq!(out, *expected, "row {name}");
    }
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
fn is_safe_beta_value_rejects_crlf_strings() {
    for ok in [
        "legit-beta",
        "",
        "with spaces",
        "with-special!@#$%^&*()chars",
    ] {
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
    ] {
        assert!(!is_safe_beta_value(bad), "{bad:?}");
    }
}
