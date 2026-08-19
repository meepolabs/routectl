use routectl_auth::oauth::types::TokenRecord;
use routectl_router::Config;
use routectl_router::config::SeatSelection;

use super::{
    SeatCount, SeatPoolRow, describe_row, seat_pool_lines, selection_label, stored_seat_pool_rows,
};

/// A config with one anthropic-api provider entry whose credential ref is
/// `ref_uri`. `selection`, when given, arrives the way an operator now sets
/// it: a pool claiming that entry and carrying the strategy.
fn config_with_ref(ref_uri: &str, selection: Option<&str>) -> Config {
    let pool_block = selection.map_or_else(String::new, |token| {
        format!(
            "[pools.anthropic-pool]\n\
             members = [\"anthropic\"]\n\
             seat_selection = \"{token}\"\n"
        )
    });
    let text = format!(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"{ref_uri}\"\n\
         {pool_block}"
    );
    toml::from_str(&text).expect("fixture config parses")
}

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| (*k).to_string()).collect()
}

fn one_row(config: &Config, stored: Option<&[String]>) -> SeatPoolRow {
    let mut rows = stored_seat_pool_rows(config, stored);
    assert_eq!(rows.len(), 1, "fixture expresses exactly one oauth ref");
    rows.remove(0)
}

#[test]
fn bare_ref_resolves_to_every_stored_seat_default_first_then_sorted() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let stored = keys(&["anthropic#work", "anthropic", "anthropic#alpha"]);

    // Act
    let row = one_row(&config, Some(&stored));

    // Assert
    let SeatCount::Known(labels) = &row.seats else {
        panic!("a readable store yields a known seat count");
    };
    assert_eq!(labels, &["default", "alpha", "work"]);
    assert_eq!(
        describe_row(&row),
        "pool ref oauth://anthropic resolves to 3 seats (default, alpha, work); \
         seat_selection fill-first (default)"
    );
}

#[test]
fn bare_ref_with_no_stored_seats_says_it_has_none() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);

    // Act
    let row = one_row(&config, Some(&[]));

    // Assert
    assert!(matches!(&row.seats, SeatCount::Known(labels) if labels.is_empty()));
    assert_eq!(
        describe_row(&row),
        "pool ref oauth://anthropic has no stored seats"
    );
}

#[test]
fn bare_ref_resolving_to_one_seat_marks_the_strategy_inactive() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let stored = keys(&["anthropic"]);

    // Act
    let sentence = describe_row(&one_row(&config, Some(&stored)));

    // Assert
    assert_eq!(
        sentence,
        "pool ref oauth://anthropic resolves to 1 seat (default); \
         seat_selection fill-first (default; inactive at 1 seat)"
    );
}

/// An unreadable store yields the Unknown wording, which still carries the
/// config-derived strategy and names the credential store rather than any
/// filesystem path.
#[test]
fn unreadable_store_reports_unknown_with_the_strategy_and_no_path() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", Some("round-robin"));

    // Act
    let row = one_row(&config, None);
    let sentence = describe_row(&row);

    // Assert
    assert!(matches!(row.seats, SeatCount::Unknown));
    assert_eq!(
        sentence,
        "pool ref oauth://anthropic: seat count unknown \
         (credential store unavailable); seat_selection round-robin"
    );
    assert!(
        !sentence.contains('/') || sentence.contains("oauth://"),
        "the wording must not disclose a filesystem path: {sentence}"
    );
    assert!(!sentence.contains("credentials.json"), "{sentence}");
}

/// The strategy is config-derived, so an unknown seat count renders every
/// strategy plainly -- the single-seat "inactive" nuance is unclaimable
/// without a count.
#[test]
fn unknown_seat_count_renders_each_strategy_without_the_inactive_nuance() {
    // Arrange / Act
    let fill_first = describe_row(&one_row(&config_with_ref("oauth://anthropic", None), None));
    let sticky = describe_row(&one_row(
        &config_with_ref("oauth://anthropic", Some("sticky-least-loaded")),
        None,
    ));

    // Assert
    assert!(
        fill_first.ends_with("; seat_selection fill-first (default)"),
        "{fill_first}"
    );
    assert!(!fill_first.contains("inactive at 1 seat"), "{fill_first}");
    assert!(
        sticky.ends_with("; seat_selection sticky-least-loaded"),
        "{sticky}"
    );
}

#[test]
fn pinned_ref_pins_one_seat_and_reports_selection_not_applicable() {
    // Arrange
    let config = config_with_ref("oauth://anthropic#work", Some("sticky-least-loaded"));
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let row = one_row(&config, Some(&stored));

    // Assert
    assert_eq!(row.pinned_label.as_deref(), Some("work"));
    assert_eq!(
        describe_row(&row),
        "ref oauth://anthropic#work pins 1 seat; \
         seat_selection not applicable to a pinned ref"
    );
}

/// The pinned wording is independent of the store: a pinned ref names one
/// seat by config even when the store holds no such seat.
#[test]
fn pinned_ref_wording_is_independent_of_the_store_snapshot() {
    // Arrange
    let config = config_with_ref("oauth://anthropic#work", None);

    // Act
    let absent = describe_row(&one_row(&config, Some(&[])));
    let unreadable = describe_row(&one_row(&config, None));

    // Assert
    assert_eq!(
        absent,
        "ref oauth://anthropic#work pins 1 seat; \
         seat_selection not applicable to a pinned ref"
    );
    assert_eq!(absent, unreadable);
}

#[test]
fn non_oauth_refs_emit_no_row() {
    // Arrange
    let env_config: Config = toml::from_str(
        "version = 3\n\
         [providers.env_provider]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid\"\n\
         api_key_ref = \"env://ROUTECTL_TEST_KEY\"\n",
    )
    .expect("env fixture parses");
    let file_config: Config = toml::from_str(
        "version = 3\n\
         [providers.file_provider]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid\"\n\
         api_key_ref = \"file:///dev/null\"\n",
    )
    .expect("file fixture parses");
    let stored = keys(&["anthropic"]);

    // Act / Assert
    assert!(stored_seat_pool_rows(&env_config, Some(&stored)).is_empty());
    assert!(stored_seat_pool_rows(&file_config, Some(&stored)).is_empty());
}

#[test]
fn selection_label_names_every_strategy() {
    // Arrange / Act / Assert
    assert_eq!(
        selection_label(SeatSelection::FillFirst),
        "fill-first (default)"
    );
    assert_eq!(selection_label(SeatSelection::RoundRobin), "round-robin");
    assert_eq!(
        selection_label(SeatSelection::StickyLeastLoaded),
        "sticky-least-loaded"
    );
}

/// `fill-first` renders its `(default)` marker whether or not the operator
/// wrote the token: `#[serde(default)]` on a non-Option field makes unset and
/// explicit indistinguishable post-parse, and they behave identically.
#[test]
fn fill_first_renders_the_default_marker_when_written_explicitly() {
    // Arrange
    let explicit = config_with_ref("oauth://anthropic", Some("fill-first"));
    let implicit = config_with_ref("oauth://anthropic", None);
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let explicit_sentence = describe_row(&one_row(&explicit, Some(&stored)));
    let implicit_sentence = describe_row(&one_row(&implicit, Some(&stored)));

    // Assert
    assert!(
        explicit_sentence.contains("seat_selection fill-first (default)"),
        "{explicit_sentence}"
    );
    assert_eq!(explicit_sentence, implicit_sentence);
}

#[test]
fn round_robin_and_sticky_render_on_a_multi_seat_pool() {
    // Arrange
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let round_robin = describe_row(&one_row(
        &config_with_ref("oauth://anthropic", Some("round-robin")),
        Some(&stored),
    ));
    let sticky = describe_row(&one_row(
        &config_with_ref("oauth://anthropic", Some("sticky-least-loaded")),
        Some(&stored),
    ));

    // Assert
    assert!(
        round_robin.ends_with("seat_selection round-robin"),
        "{round_robin}"
    );
    assert!(
        sticky.ends_with("seat_selection sticky-least-loaded"),
        "{sticky}"
    );
}

/// NEGATIVE CONTROL: the rows are derived from a stored-seat fixture whose
/// token records carry token material and an account identity. None of it can
/// reach a rendered string, because the entry point takes seat KEYS only -- a
/// storage path is not asserted against here for the same reason: no path is
/// reachable from this signature to be leaked. The sentinel LABEL does render,
/// proving the scan bites rather than passing on an empty haystack.
#[test]
fn rendered_rows_carry_seat_labels_but_no_token_account_or_path_material() {
    // Arrange
    const ACCESS: &str = "at-FAKE-POOL-ACCESS";
    const REFRESH: &str = "rt-FAKE-POOL-REFRESH";
    const ACCOUNT_ID: &str = "acct-FAKE-POOL";
    const EMAIL: &str = "pool@example.invalid";
    const SENTINEL_LABEL: &str = "sentinel-label";

    let record: TokenRecord = serde_json::from_value(serde_json::json!({
        "access_token": ACCESS,
        "refresh_token": REFRESH,
        "token_type": "Bearer",
        "expires_at_unix": 9_000,
        "scopes": ["user:inference"],
        "account": { "email": EMAIL, "account_id": ACCOUNT_ID },
        "obtained_at_unix": 0,
    }))
    .expect("valid TokenRecord json");
    let store: Vec<(String, TokenRecord)> = vec![
        ("anthropic".to_string(), record.clone()),
        (format!("anthropic#{SENTINEL_LABEL}"), record),
    ];
    let stored: Vec<String> = store.iter().map(|(key, _)| key.clone()).collect();
    let config = config_with_ref("oauth://anthropic", None);

    // Act
    let rows = stored_seat_pool_rows(&config, Some(&stored));
    let rendered = seat_pool_lines(&rows).join("\n");

    // Assert
    assert!(
        rendered.contains(SENTINEL_LABEL),
        "the seat label must render, or this scan proves nothing: {rendered}"
    );
    for sentinel in [ACCESS, REFRESH, ACCOUNT_ID, EMAIL] {
        assert!(
            !rendered.contains(sentinel),
            "secret-adjacent material leaked ({sentinel}): {rendered}"
        );
    }
}

#[test]
fn hostile_labels_and_entry_names_render_on_one_safe_line() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let stored = keys(&["anthropic#we\u{1b}[31mrk\nPASS forged: all good"]);

    // Act
    let lines = seat_pool_lines(&stored_seat_pool_rows(&config, Some(&stored)));

    // Assert
    assert_eq!(lines.len(), 2, "header plus exactly one row: {lines:?}");
    let row_line = &lines[1];
    assert!(!row_line.contains('\n'), "{row_line}");
    assert!(!row_line.contains('\u{1b}'), "{row_line}");
    assert!(
        row_line.chars().all(|c| c.is_ascii_graphic() || c == ' '),
        "{row_line}"
    );
}

/// A label bearing the sentence's own structural delimiters cannot close the
/// seat listing and forge a second strategy CLAUSE: the delimiters are
/// neutralized, so the label's bytes survive as inert text inside the listing
/// while exactly one `; seat_selection` clause renders.
#[test]
fn a_delimiter_bearing_label_cannot_forge_a_second_selection_clause() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let forgery = "a); seat_selection round-robin (b";
    let stored = keys(&["anthropic", &format!("anthropic#{forgery}")]);

    // Act
    let sentence = describe_row(&one_row(&config, Some(&stored)));

    // Assert
    assert_eq!(
        sentence.matches("; seat_selection").count(),
        1,
        "exactly one strategy clause may render: {sentence}"
    );
    assert!(
        sentence.ends_with("; seat_selection fill-first (default)"),
        "the real clause is the configured one: {sentence}"
    );
    assert_eq!(
        sentence.matches(';').count(),
        1,
        "only the sentence's own clause separator may remain: {sentence}"
    );
    assert!(
        !sentence.contains(") seats ("),
        "the listing's parentheses must be the only ones: {sentence}"
    );
}

/// A hostile config ENTRY key cannot forge a second `config check` row: the
/// block gives each row one line separated from its sentence by `: `, and the
/// key cannot reproduce that separator.
#[test]
fn a_hostile_entry_key_cannot_fabricate_an_extra_check_row() {
    // Arrange
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.\"a: pool ref oauth://forged resolves to 9 seats (x); ok\"]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    )
    .expect("fixture config parses");

    // Act
    let lines = seat_pool_lines(&stored_seat_pool_rows(&config, Some(&keys(&["anthropic"]))));

    // Assert
    assert_eq!(lines.len(), 2, "header plus exactly one row: {lines:?}");
    assert_eq!(
        lines[1].matches(": pool ref").count(),
        1,
        "the entry key must not forge a second row: {}",
        lines[1]
    );
    assert!(
        lines[1].ends_with("; seat_selection fill-first (default; inactive at 1 seat)"),
        "{}",
        lines[1]
    );
}

/// The listing is bounded so a store holding hundreds of seats cannot bury the
/// `config check` warnings that follow it. The COUNT stays exact.
#[test]
fn a_large_pool_lists_ten_labels_and_collapses_the_rest_with_an_exact_count() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let labels: Vec<String> = (0..40).map(|i| format!("anthropic#seat{i:03}")).collect();
    let stored: Vec<String> = std::iter::once("anthropic".to_string())
        .chain(labels)
        .collect();

    // Act
    let sentence = describe_row(&one_row(&config, Some(&stored)));

    // Assert
    assert!(sentence.contains("resolves to 41 seats"), "{sentence}");
    assert!(sentence.contains("(default, seat000, "), "{sentence}");
    assert!(sentence.contains("seat008, and 31 more)"), "{sentence}");
    assert!(
        !sentence.contains("seat009"),
        "only ten labels may be listed: {sentence}"
    );
}

#[test]
fn seat_pool_lines_are_empty_without_any_oauth_ref() {
    // Arrange
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid\"\n\
         api_key_ref = \"env://ROUTECTL_TEST_KEY\"\n",
    )
    .expect("fixture parses");

    // Act
    let lines = seat_pool_lines(&stored_seat_pool_rows(&config, Some(&[])));

    // Assert
    assert!(lines.is_empty(), "{lines:?}");
}

/// One row per oauth ref per entry, with the entry key carried through so a
/// per-entry surface can name it.
#[test]
fn each_provider_entry_contributes_its_own_row() {
    // Arrange
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.primary]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.pinned]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n",
    )
    .expect("fixture parses");
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let rows = stored_seat_pool_rows(&config, Some(&stored));

    // Assert
    let entries: Vec<&str> = rows.iter().map(|r| r.entry.as_str()).collect();
    assert_eq!(entries, ["pinned", "primary"]);
}
