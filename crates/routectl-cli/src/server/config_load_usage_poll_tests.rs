//! `[usage] poll` on the path `config check` loads through: a boolean is
//! accepted, anything else is refused naming the key.

use routectl_router::CURRENT_CONFIG_VERSION;

use super::*;

fn load(poll_value: &str) -> Result<LoadedConfig, String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let usage_db = dir.path().join("usage.db");
    std::fs::write(
        &path,
        format!(
            "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\npoll = {poll_value}\n",
            usage_db.display()
        ),
    )
    .expect("write config");
    load_effective_config_unvalidated(&path)
}

#[test]
fn a_boolean_poll_value_loads() {
    for (value, expected) in [("true", true), ("false", false)] {
        let loaded = load(value).unwrap_or_else(|e| panic!("{value}: {e}"));

        assert_eq!(loaded.config.usage.poll, expected, "{value}");
    }
}

#[test]
fn a_poll_value_that_is_not_a_boolean_is_refused_naming_the_key() {
    for value in ["\"false\"", "\"off\"", "0", "1", "[]"] {
        let Err(error) = load(value) else {
            panic!("{value}: a non-boolean poll value must be refused");
        };

        assert!(error.contains("poll"), "{value}: {error}");
    }
}
