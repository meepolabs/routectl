//! Test-only fully populated `/status/config` panel, built through this
//! panel's own projection from the derived effective view.

use routectl_auth::LocalProbe;
use routectl_router::config::{PoolEntry, PricingConfig, SeatSelection};
use routectl_router::prices::parse_and_validate;
use routectl_router::{Config, compute_activation, derive_effective_view};
use serde_json::Value;

use super::super::daemon_meta::DaemonMetaSnapshot;
use super::super::panel_fixtures::{fixture_as_of, fixture_config};
use super::{ConfigPanel, Panel, SCHEMA_VERSION, build_panel};

/// The serialized populated config envelope.
pub fn populated_config() -> Value {
    serde_json::to_value(populated_config_panel()).expect("the config panel serializes")
}

/// The pool the populated panel serves.
pub const FIXTURE_POOL: &str = "claude-pool";

/// The pool's members in configured order: a seat no model dispatches on
/// (so it carries no login label) ahead of the labeled `claude-seat`.
pub const FIXTURE_POOL_MEMBERS: [&str; 2] = ["seat-b", "claude-seat"];

/// [`fixture_config`] plus what only this panel reads: a fill-first pool
/// over two Anthropic seats, and `glm` priced in config for 5m, 30m and 1h
/// cache writes. No model is added, so every panel listing models still
/// lists the shared fixture's.
pub fn populated_config_source() -> Config {
    let mut config = fixture_config();
    let seat = config
        .providers
        .get("claude-seat")
        .expect("the shared fixture has claude-seat")
        .clone();
    config.providers.insert(
        "seat-b".to_string(),
        seat.with_credential("oauth://anthropic#b"),
    );
    config.pools.insert(
        FIXTURE_POOL.to_string(),
        PoolEntry::new(FIXTURE_POOL_MEMBERS.map(str::to_string).to_vec())
            .with_seat_selection(SeatSelection::FillFirst),
    );
    let mut pricing = PricingConfig::default();
    for (seconds, rate) in [(300, 1.25), (1800, 1.5), (3600, 2.0)] {
        pricing.set_cache_write_rate(seconds, rate);
    }
    config
        .models
        .get_mut("glm")
        .expect("the shared fixture has glm")
        .pricing = Some(pricing);
    config
}

/// [`populated_config_source`]'s effective view, priced against a refreshed
/// and a shipped table so every price source appears, beside an activation
/// inventory holding an activated and an unresolved provider.
pub fn populated_config_panel() -> Panel<ConfigPanel> {
    let config = populated_config_source();
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

/// `config`'s effective view with no price table, served from `listen_addr`
/// and stamped `as_of`, beside an activation inventory that probes no login.
pub fn config_panel_for(config: &Config, listen_addr: &str, as_of: String) -> Panel<ConfigPanel> {
    let effective = derive_effective_view(config, None, None);
    let activation = compute_activation(&[], config);
    let daemon = DaemonMetaSnapshot {
        listen_addr: listen_addr.to_string(),
        version: env!("CARGO_PKG_VERSION"),
        config_loaded_age_ms: Some(42_000),
    };
    let dto = build_panel(
        effective,
        &activation,
        Some("/fixture/routectl/config.toml"),
        daemon,
    );
    Panel::available(SCHEMA_VERSION, as_of, dto)
}
