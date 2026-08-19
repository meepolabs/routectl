use routectl_auth::oauth::types::TokenRecord;
use routectl_router::Config;
use routectl_router::config::SeatSelection;

use super::{
    PoolHealth, SeatCount, SeatPoolRow, describe_pool, describe_row, pool_rows, seat_pool_lines,
    selection_label, stored_seat_pool_rows,
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

/// A two-account pool: the default seat and one labelled seat, each on its own
/// provider entry, grouped by `[pools.anthropic]`. This is the shape `config
/// migrate` produces and the shape `routectl login` grows.
fn pooled_config(selection: &str, accepts_new_logins: bool) -> Config {
    let text = format!(
        "version = 3\n\
         [providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\", \"anthropic-work\"]\n\
         seat_selection = \"{selection}\"\n\
         accepts_new_logins = {accepts_new_logins}\n"
    );
    toml::from_str(&text).expect("pooled fixture config parses")
}

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| (*k).to_string()).collect()
}

fn one_row(config: &Config, stored: Option<&[String]>) -> SeatPoolRow {
    let mut rows = stored_seat_pool_rows(config, stored);
    assert_eq!(rows.len(), 1, "fixture expresses exactly one oauth ref");
    rows.remove(0)
}

/// REWRITE of the f1 pin that asserted a bare ref "resolves to 3 seats".
/// Schema version 4 retired that reading: a bare `oauth://<provider>` ref
/// names the DEFAULT seat alone, and the labelled siblings the store also
/// holds are reachable only through refs that name them.
#[test]
fn bare_ref_pins_the_default_seat_and_ignores_labelled_siblings() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let stored = keys(&["anthropic#work", "anthropic", "anthropic#alpha"]);

    // Act
    let row = one_row(&config, Some(&stored));

    // Assert
    let SeatCount::Known(labels) = &row.seats else {
        panic!("a readable store yields a known presence answer");
    };
    assert_eq!(labels, &["default"], "only the default seat is named");
    assert_eq!(
        describe_row(&row),
        "ref oauth://anthropic pins the default seat; \
         seat_selection not applicable to a single-seat ref"
    );
}

#[test]
fn bare_ref_with_no_stored_default_seat_says_so() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);
    let stored = keys(&["anthropic#work"]);

    // Act
    let row = one_row(&config, Some(&stored));

    // Assert
    assert!(matches!(&row.seats, SeatCount::Known(labels) if labels.is_empty()));
    assert_eq!(
        describe_row(&row),
        "ref oauth://anthropic pins the default seat (no stored credential for it); \
         seat_selection not applicable to a single-seat ref"
    );
}

/// An unreadable store yields the unknown-presence wording, and names the
/// credential store rather than any filesystem path.
#[test]
fn unreadable_store_reports_unknown_presence_with_no_path() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", None);

    // Act
    let row = one_row(&config, None);
    let sentence = describe_row(&row);

    // Assert
    assert!(matches!(row.seats, SeatCount::Unknown));
    assert_eq!(
        sentence,
        "ref oauth://anthropic pins the default seat \
         (store presence unknown - credential store unavailable); \
         seat_selection not applicable to a single-seat ref"
    );
    assert!(
        !sentence.contains('/') || sentence.contains("oauth://"),
        "the wording must not disclose a filesystem path: {sentence}"
    );
    assert!(!sentence.contains("credentials.json"), "{sentence}");
}

#[test]
fn pinned_ref_on_a_standalone_entry_pins_one_seat_with_no_strategy() {
    // Arrange
    let config = config_with_ref("oauth://anthropic#work", None);
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let row = one_row(&config, Some(&stored));

    // Assert
    assert_eq!(row.pinned_label.as_deref(), Some("work"));
    assert_eq!(row.pool, None);
    assert_eq!(
        describe_row(&row),
        "ref oauth://anthropic#work pins 1 seat; \
         seat_selection not applicable to a single-seat ref"
    );
}

/// A ref on an entry a POOL claims names its pool and the strategy in force:
/// the strategy is a property of the SET, so only a member row may state one.
#[test]
fn a_pool_member_row_names_its_pool_and_the_pools_strategy() {
    // Arrange
    let config = config_with_ref("oauth://anthropic", Some("round-robin"));
    let stored = keys(&["anthropic"]);

    // Act
    let row = one_row(&config, Some(&stored));

    // Assert
    assert_eq!(row.pool.as_deref(), Some("anthropic-pool"));
    assert_eq!(
        describe_row(&row),
        "ref oauth://anthropic pins the default seat; \
         member of pool `anthropic-pool` with seat_selection round-robin"
    );
}

/// The pool is the rendered unit: name, member count, per-member seat, the
/// strategy, and the growth marker.
#[test]
fn a_healthy_pool_renders_members_strategy_and_growth_marker() {
    // Arrange
    let config = pooled_config("round-robin", true);
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let mut rows = pool_rows(&config, Some(&stored));
    assert_eq!(rows.len(), 1);
    let row = rows.remove(0);

    // Assert
    assert!(matches!(row.health, PoolHealth::Ready));
    assert_eq!(
        describe_pool(&row),
        "pool `anthropic` has 2 members (anthropic-default=default, anthropic-work=work); \
         seat_selection round-robin; accepts new logins: yes"
    );
}

/// A pool the store holds only some members' seats for is DEGRADED and names
/// exactly the members it has no credential for.
#[test]
fn a_pool_missing_one_members_credential_names_that_member() {
    // Arrange
    let config = pooled_config("fill-first", false);
    let stored = keys(&["anthropic"]);

    // Act
    let row = pool_rows(&config, Some(&stored)).remove(0);

    // Assert
    assert!(matches!(row.health, PoolHealth::Degraded));
    assert_eq!(
        describe_pool(&row),
        "pool `anthropic` has 2 members (anthropic-default=default, anthropic-work=work); \
         seat_selection fill-first (default); accepts new logins: no; \
         no stored credential for anthropic-work"
    );
}

/// A pool no member of which has a stored credential serves nothing, and says
/// so without naming every member twice.
#[test]
fn a_pool_with_no_stored_credentials_reports_itself_unusable() {
    // Arrange
    let config = pooled_config("fill-first", false);

    // Act
    let row = pool_rows(&config, Some(&[])).remove(0);

    // Assert
    assert!(matches!(row.health, PoolHealth::Unusable));
    assert!(
        describe_pool(&row).ends_with("; no member has a stored credential"),
        "{}",
        describe_pool(&row)
    );
}

/// An unreadable store makes presence unknowable, so the pool claims neither
/// health nor degradation -- the strategy and membership still render.
#[test]
fn an_unreadable_store_leaves_pool_member_presence_unknown() {
    // Arrange
    let config = pooled_config("sticky-least-loaded", false);

    // Act
    let row = pool_rows(&config, None).remove(0);

    // Assert
    assert!(matches!(row.health, PoolHealth::Unknown));
    let sentence = describe_pool(&row);
    assert!(
        sentence.contains("seat_selection sticky-least-loaded"),
        "{sentence}"
    );
    assert!(
        sentence.ends_with("; member credential presence unknown (credential store unavailable)"),
        "{sentence}"
    );
    assert!(!sentence.contains("credentials.json"), "{sentence}");
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
    let stored = keys(&["anthropic"]);

    // Act
    let sentence = describe_row(&one_row(&explicit, Some(&stored)));

    // Assert
    assert!(
        sentence.ends_with("with seat_selection fill-first (default)"),
        "{sentence}"
    );
}

/// The `config check` block renders the POOL as the unit and does NOT repeat
/// its members as standalone rows -- a duplicated per-member line would read
/// as a second, independent dispatch target.
#[test]
fn the_check_block_renders_pools_and_only_unpooled_entries() {
    // Arrange
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [providers.codex]\n\
         kind = \"openai-responses\"\n\
         api_key_ref = \"oauth://codex\"\n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\", \"anthropic-work\"]\n",
    )
    .expect("fixture parses");
    let stored = keys(&["anthropic", "anthropic#work", "codex"]);

    // Act
    let lines = seat_pool_lines(
        &pool_rows(&config, Some(&stored)),
        &stored_seat_pool_rows(&config, Some(&stored)),
    );

    // Assert
    assert_eq!(
        lines.len(),
        3,
        "header, one pool, one standalone: {lines:?}"
    );
    assert_eq!(lines[0], "oauth seat pools:");
    assert!(
        lines[1].starts_with("  pool `anthropic` has 2 members"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].starts_with("  codex: ref oauth://codex pins the default seat"),
        "{}",
        lines[2]
    );
    for member in ["anthropic-default:", "anthropic-work:"] {
        assert!(
            !lines.iter().any(|line| line.contains(member)),
            "a pool member must not also render a standalone row: {lines:?}"
        );
    }
}

/// NEGATIVE CONTROL: the rows are derived from a stored-seat fixture whose
/// token records carry token material and an account identity. None of it can
/// reach a rendered string, because the entry points take seat KEYS only -- a
/// storage path is not asserted against here for the same reason: no path is
/// reachable from these signatures to be leaked. The sentinel LABEL does
/// render on the pool line, proving the scan bites rather than passing on an
/// empty haystack.
#[test]
fn rendered_pool_surfaces_carry_seat_labels_but_no_token_account_or_path_material() {
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
    let config: Config = toml::from_str(&format!(
        "version = 3\n\
         [providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-sentinel]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#{SENTINEL_LABEL}\"\n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\", \"anthropic-sentinel\"]\n"
    ))
    .expect("fixture parses");

    // Act
    let rendered = seat_pool_lines(
        &pool_rows(&config, Some(&stored)),
        &stored_seat_pool_rows(&config, Some(&stored)),
    )
    .join("\n");

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
    // Arrange: the hostile bytes arrive as a seat LABEL on the ref, written
    // with TOML escapes so the fixture is a legal document carrying an ANSI
    // escape and a newline.
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#we\\u001B[31mrk\\nPASS forged: all good\"\n",
    )
    .expect("hostile fixture parses");

    // Act
    let lines = seat_pool_lines(
        &pool_rows(&config, Some(&[])),
        &stored_seat_pool_rows(&config, Some(&[])),
    );

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

/// A member entry name bearing the pool sentence's own structural delimiters
/// cannot close the member list and forge a second strategy CLAUSE: the
/// delimiters are neutralized, so the name's bytes survive as inert text
/// inside the listing while exactly one `; seat_selection` clause renders.
#[test]
fn a_delimiter_bearing_member_name_cannot_forge_a_second_selection_clause() {
    // Arrange
    let forgery = "a); seat_selection round-robin (b";
    let config: Config = toml::from_str(&format!(
        "version = 3\n\
         [providers.\"{forgery}\"]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.anthropic]\n\
         members = [\"{forgery}\"]\n"
    ))
    .expect("hostile pool fixture parses");

    // Act
    let sentence = describe_pool(&pool_rows(&config, Some(&keys(&["anthropic"]))).remove(0));

    // Assert
    assert_eq!(
        sentence.matches("; seat_selection").count(),
        1,
        "exactly one strategy clause may render: {sentence}"
    );
    assert!(
        sentence.contains("; seat_selection fill-first (default);"),
        "the real clause is the configured one: {sentence}"
    );
}

/// The two remaining characters the pool sentence's grammar uses -- the `=` of
/// an `entry=seat` pair and the backtick that quotes the pool name -- are
/// neutralized too, so a member key cannot forge an extra pair in the listing
/// and a pool key cannot close its own quoting.
#[test]
fn an_equals_or_backtick_bearing_key_cannot_forge_a_pair_or_close_the_quoting() {
    // Arrange
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.\"ghost=default\"]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.\"p`x\"]\n\
         members = [\"ghost=default\"]\n",
    )
    .expect("hostile pool fixture parses");

    // Act
    let sentence = describe_pool(&pool_rows(&config, Some(&keys(&["anthropic"]))).remove(0));

    // Assert
    assert_eq!(
        sentence.matches('=').count(),
        1,
        "exactly one entry=seat pair may render for one member: {sentence}"
    );
    assert_eq!(
        sentence.matches('`').count(),
        2,
        "the pool name's quoting must stay balanced: {sentence}"
    );
    assert!(
        sentence.starts_with("pool `p?x` has 1 member (ghost?default=default)"),
        "{sentence}"
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
         [providers.\"a: ref oauth://forged pins the default seat; ok\"]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    )
    .expect("fixture config parses");
    let stored = keys(&["anthropic"]);

    // Act
    let lines = seat_pool_lines(
        &pool_rows(&config, Some(&stored)),
        &stored_seat_pool_rows(&config, Some(&stored)),
    );

    // Assert
    assert_eq!(lines.len(), 2, "header plus exactly one row: {lines:?}");
    assert_eq!(
        lines[1].matches(": ref oauth://").count(),
        1,
        "the entry key must not forge a second row: {}",
        lines[1]
    );
    assert!(
        lines[1].ends_with("; seat_selection not applicable to a single-seat ref"),
        "{}",
        lines[1]
    );
}

/// The member listing is bounded so a large pool cannot bury the `config
/// check` warnings that follow it. The COUNT stays exact.
#[test]
fn a_large_pool_lists_ten_members_and_collapses_the_rest_with_an_exact_count() {
    // Arrange
    let mut text = String::from("version = 3\n");
    let members: Vec<String> = (0..32).map(|i| format!("acct{i:03}")).collect();
    for (index, member) in members.iter().enumerate() {
        text.push_str(&format!(
            "[providers.{member}]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#seat{index:03}\"\n"
        ));
    }
    text.push_str("[pools.anthropic]\n");
    text.push_str(&format!(
        "members = [{}]\n",
        members
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let config: Config = toml::from_str(&text).expect("large pool fixture parses");
    let stored: Vec<String> = (0..32).map(|i| format!("anthropic#seat{i:03}")).collect();

    // Act
    let sentence = describe_pool(&pool_rows(&config, Some(&stored)).remove(0));

    // Assert
    assert!(sentence.contains("has 32 members"), "{sentence}");
    assert!(sentence.contains("(acct000=seat000, "), "{sentence}");
    assert!(
        sentence.contains("acct009=seat009, and 22 more)"),
        "{sentence}"
    );
    assert!(
        !sentence.contains("acct010"),
        "only ten members may be listed: {sentence}"
    );
}

#[test]
fn seat_pool_lines_are_empty_without_any_pool_or_oauth_ref() {
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
    let lines = seat_pool_lines(
        &pool_rows(&config, Some(&[])),
        &stored_seat_pool_rows(&config, Some(&[])),
    );

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

/// The migrated shape renders as one pool named after the family plus its
/// suffixed account entries, with no standalone member row: the migration's
/// output is legible on this surface without further config edits.
#[test]
fn the_migrated_shape_renders_as_one_pool_naming_its_accounts() {
    // Arrange
    let config = pooled_config("fill-first", false);
    let stored = keys(&["anthropic", "anthropic#work"]);

    // Act
    let lines = seat_pool_lines(
        &pool_rows(&config, Some(&stored)),
        &stored_seat_pool_rows(&config, Some(&stored)),
    );

    // Assert
    assert_eq!(lines.len(), 2, "header plus exactly the pool: {lines:?}");
    assert!(
        lines[1].contains(
            "pool `anthropic` has 2 members \
             (anthropic-default=default, anthropic-work=work)"
        ),
        "{}",
        lines[1]
    );
}
