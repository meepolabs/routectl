//! Test-only fully populated `/status/config` panel, built through this
//! panel's own projection from the derived effective view.

use routectl_auth::LocalProbe;
use routectl_router::prices::parse_and_validate;
use routectl_router::{compute_activation, derive_effective_view};
use serde_json::Value;

use super::super::daemon_meta::DaemonMetaSnapshot;
use super::super::panel_fixtures::{fixture_as_of, fixture_config};
use super::{ConfigPanel, Panel, SCHEMA_VERSION, build_panel};

/// The serialized populated config envelope.
pub fn populated_config() -> Value {
    serde_json::to_value(populated_config_panel()).expect("the config panel serializes")
}

/// The fixture config's effective view, priced against a refreshed and a
/// shipped table so every price source appears, beside an activation
/// inventory holding an activated and an unresolved provider.
pub fn populated_config_panel() -> Panel<ConfigPanel> {
    let config = fixture_config();
    let refreshed = price_table("2026-11-05", 3.0);
    let shipped = price_table("2026-10-01", 2.5);
    let effective = derive_effective_view(&config, Some(&refreshed), Some(&shipped));
    let activation = compute_activation(
        &[
            ("anthropic", LocalProbe::Present),
            ("codex", LocalProbe::Missing),
        ],
        &config,
    );
    let daemon = DaemonMetaSnapshot {
        listen_addr: "127.0.0.1:8787".to_string(),
        version: env!("CARGO_PKG_VERSION"),
        config_loaded_age_ms: Some(42_000),
    };
    let dto = build_panel(
        effective,
        &activation,
        Some("/fixture/routectl/config.toml"),
        daemon,
    );
    Panel::available(SCHEMA_VERSION, fixture_as_of(), dto)
}

/// A price table pricing the fixture's anthropic sonnet input and the compat
/// model's input and cache read.
fn price_table(date: &str, sonnet_input: f64) -> routectl_router::prices::PriceTable {
    let json = serde_json::json!({
        "schema_version": 1,
        "generated_at": date,
        "providers": [
            {
                "id": "anthropic",
                "models": [{ "id": "claude-sonnet-4-5", "input": sonnet_input }],
            },
            {
                "id": "fixture-compat",
                "api_host": "https://compat.example/v1",
                "models": [{ "id": "glm-5", "input": 1.0, "cache_read": 0.1 }],
            },
        ],
    });
    parse_and_validate(&serde_json::to_vec(&json).expect("json")).expect("a valid price table")
}
