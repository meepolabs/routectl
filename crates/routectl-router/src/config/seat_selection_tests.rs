//! Pin the `seat_selection` pool knob: a default, an explicit
//! `round-robin`, and a rejected unknown value. The field lives on the
//! `pools.<name>` block, not on a provider entry.
use crate::config::{Config, PoolEntry, SeatSelection};

fn pool_of<'a>(cfg: &'a Config, name: &str) -> &'a PoolEntry {
    cfg.pools.get(name).expect("pool")
}

#[test]
fn seat_selection_defaults_to_fill_first() {
    // Arrange: a pool block omitting seat_selection.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"

[pools.anthropic-pool]
members = ["anthropic"]
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    // Assert
    assert_eq!(
        pool_of(&cfg, "anthropic-pool").seat_selection,
        SeatSelection::FillFirst
    );
}

#[test]
fn seat_selection_parses_round_robin() {
    // Arrange
    let toml_text = r#"
[pools.anthropic-pool]
members = ["anthropic"]
seat_selection = "round-robin"
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    // Assert
    assert_eq!(
        pool_of(&cfg, "anthropic-pool").seat_selection,
        SeatSelection::RoundRobin
    );
}

#[test]
fn seat_selection_rejects_unknown_value() {
    // Arrange
    let toml_text = r#"
[pools.anthropic-pool]
members = ["anthropic"]
seat_selection = "bogus"
"#;
    // Act
    let result = toml::from_str::<Config>(toml_text);
    // Assert: an unknown variant is a clean deserialize Err.
    assert!(
        result.is_err(),
        "unknown seat_selection value must reject; got Ok"
    );
}

/// `PoolEntry::default()` carries `FillFirst`, so a programmatically-built
/// pool matches the TOML-omitted default.
#[test]
fn pool_entry_default_is_fill_first() {
    assert_eq!(
        PoolEntry::default().seat_selection,
        SeatSelection::FillFirst
    );
    assert!(!PoolEntry::default().accepts_new_logins);
}

/// The growth marker is opt-in: a pool that does not write it is pinned.
#[test]
fn accepts_new_logins_defaults_to_false_and_parses_true() {
    // Arrange
    let pinned: Config = toml::from_str(
        "[pools.p]\n\
         members = [\"anthropic\"]\n",
    )
    .expect("parse pinned pool");
    let growing: Config = toml::from_str(
        "[pools.p]\n\
         members = [\"anthropic\"]\n\
         accepts_new_logins = true\n",
    )
    .expect("parse growth-marked pool");

    // Act / Assert
    assert!(!pool_of(&pinned, "p").accepts_new_logins);
    assert!(pool_of(&growing, "p").accepts_new_logins);
}

/// `members` carries no serde default: a pool block that omits it is a
/// deserialize error, not an empty pool.
#[test]
fn pool_block_requires_members() {
    let result = toml::from_str::<Config>("[pools.p]\nseat_selection = \"round-robin\"\n");
    assert!(
        result.is_err(),
        "a pool without members must reject; got Ok"
    );
}

/// `deny_unknown_fields` rejects a typo'd key inside a pool block.
#[test]
fn pool_block_rejects_unknown_field() {
    let result = toml::from_str::<Config>("[pools.p]\nmembers = []\nbogus_key = 1\n");
    assert!(result.is_err(), "unknown pool key must reject; got Ok");
}

/// `seat_selection` no longer exists on a provider entry: the knob's only
/// home is the pool block, and `deny_unknown_fields` on `ProviderEntry`
/// makes the stale placement a hard parse error rather than a silently
/// ignored key. Configs written against the previous schema version are
/// caught earlier still, by the version preflight.
#[test]
fn provider_entry_no_longer_accepts_seat_selection() {
    let result = toml::from_str::<Config>(
        "[providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"literal:sk-ant-test\"\n\
         seat_selection = \"round-robin\"\n",
    );
    assert!(
        result.is_err(),
        "seat_selection on a provider entry must reject; got Ok"
    );
}

/// A config with no `[usage]` block deserializes to the documented
/// defaults: enabled, 90-day retention, and a db under the resolved
/// user config dir with no literal `~` left in the path.
#[test]
fn usage_block_absent_yields_defaults() {
    // Arrange: a config that mentions usage nowhere.
    let toml_text = r#"
[server]
host = "127.0.0.1"
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse without usage block");

    // Assert
    assert!(cfg.usage.enabled, "enabled must default true");
    assert_eq!(cfg.usage.retention_days, 90);
    let db = cfg.usage.db_path.to_string_lossy();
    assert!(
        db.ends_with("routectl/usage.db"),
        "db_path must end with routectl/usage.db; got {db}"
    );
    assert!(
        !db.contains('~'),
        "no literal ~ may reach the path; got {db}"
    );
}

/// Explicit `[usage]` values override every default.
#[test]
fn usage_block_explicit_overrides_defaults() {
    // Arrange
    let toml_text = r#"
[usage]
enabled = false
db_path = "/var/lib/routectl/usage.db"
retention_days = 7
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit usage block");

    // Assert
    assert!(!cfg.usage.enabled);
    assert_eq!(
        cfg.usage.db_path,
        std::path::PathBuf::from("/var/lib/routectl/usage.db")
    );
    assert_eq!(cfg.usage.retention_days, 7);
}

/// `deny_unknown_fields` rejects a typo'd key inside `[usage]`.
#[test]
fn usage_block_rejects_unknown_field() {
    // Arrange
    let toml_text = r"
[usage]
enabled = true
bogus_key = 1
";
    // Act
    let result = toml::from_str::<Config>(toml_text);
    // Assert
    assert!(result.is_err(), "unknown [usage] key must reject; got Ok");
}
