//! Pin the `seat_selection` per-provider knob: a default, an
//! explicit `round-robin`, and a rejected unknown value. The field
//! flattens off `ProviderRuntimePolicy` onto every `[providers.X]`.
use crate::config::{Config, SeatSelection};

fn runtime_of<'a>(cfg: &'a Config, name: &str) -> &'a super::ProviderRuntimePolicy {
    cfg.providers.get(name).expect("provider").runtime()
}

#[test]
fn seat_selection_defaults_to_fill_first() {
    // Arrange: a provider entry omitting seat_selection.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    // Assert
    assert_eq!(
        runtime_of(&cfg, "anthropic").seat_selection,
        SeatSelection::FillFirst
    );
}

#[test]
fn seat_selection_parses_round_robin() {
    // Arrange
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
seat_selection = "round-robin"
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    // Assert
    assert_eq!(
        runtime_of(&cfg, "anthropic").seat_selection,
        SeatSelection::RoundRobin
    );
}

#[test]
fn seat_selection_rejects_unknown_value() {
    // Arrange
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
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

/// `ProviderRuntimePolicy::default()` carries `FillFirst`, so a
/// programmatically-built provider matches the TOML-omitted default.
#[test]
fn provider_runtime_policy_default_is_fill_first() {
    assert_eq!(
        super::ProviderRuntimePolicy::default().seat_selection,
        SeatSelection::FillFirst
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
