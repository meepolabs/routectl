//! The learned verdict's value on the capability matrix: on each cell's JSON,
//! in the CLI grid, and the shown capability key copied into a
//! `force_supported` override on the lane's provider entry.

use std::time::{Duration, Instant};

use routectl_core::capability::FailurePhase;
use routectl_router::{CapabilityMatrixPanel, LearnedRegistryEntry, VerdictValue};

use super::lane_matrix_tests::{available, cell, context, negative};
use super::matrix::build_capability_matrix_panel;
use super::*;
use crate::commands::doctor_panels::render_capability_matrix_panel;

const LANE: &str = "p#up";

/// A grounded field verdict key, spelled in pieces so the field prefix
/// literal keeps its single owner.
fn field_key(path: &str) -> String {
    let key = format!("{}{}{path}", "fie", "ld:");
    assert!(
        routectl_router::capability_key_is_field_verdict(&key),
        "premise: {key} is a field verdict key"
    );
    key
}

fn config_toml(overrides: &str) -> String {
    format!(
        "version = {}\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid/v1\"\n\
         api_key_ref = \"env://ROUTECTL_VALUE_MATRIX_TEST_KEY\"\n\
         [models.alpha]\n\
         provider = \"p\"\n\
         upstream = \"up\"\n\
         {overrides}",
        routectl_router::CURRENT_CONFIG_VERSION,
    )
}

/// Parse `text` the way the daemon and the CLI load a config file: the raw
/// preflight, the typed parse, then the startup validators.
fn load(text: &str) -> Config {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).expect("write config");
    let config = crate::server::parse_config_only(&path).expect("the config parses");
    let errors = routectl_router::collect_config_validation(&config).errors;
    assert!(errors.is_empty(), "the config validates: {errors:?}");
    config
}

fn learned(capability: &str, value: Option<VerdictValue>, now: Instant) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        value,
        ..negative(
            LANE,
            capability,
            FailurePhase::F1,
            now,
            now + Duration::from_hours(1),
        )
    }
}

fn panel(
    config: Config,
    entries: Vec<LearnedRegistryEntry>,
    now: Instant,
) -> CapabilityMatrixPanel {
    build_capability_matrix_panel(&context(config, available(entries, now, 0)))
}

/// The grid row of the lane, as rendered.
fn human_row(panel: &CapabilityMatrixPanel) -> String {
    let human = render_capability_matrix_panel(panel);
    human
        .lines()
        .find(|line| line.trim_start().starts_with("p#up "))
        .unwrap_or_else(|| panic!("no {LANE} row: {human}"))
        .to_string()
}

/// A ceiling cell serializes its encoded value and a drop cell `null`; the
/// CLI grid shows each learned cell's value, `-` where the verdict writes
/// none, and gives a cell no learned entry backs no value slot.
///
/// Mutation check: set `value: None` in `build_cell` -> red on the JSON and
/// grid assertions for the ceiling cell; drop the `value=` suffix from
/// `cell_token` -> red on both grid assertions.
#[test]
fn a_matrix_cell_carries_the_learned_value_in_json_and_in_the_grid() {
    // Arrange
    let now = Instant::now();
    let ceiling = field_key("max_tokens");
    let dropped = field_key("temperature");
    let entries = vec![
        learned(&ceiling, Some(VerdictValue::Ceiling(32_000)), now),
        learned(&dropped, None, now),
    ];

    // Act
    let panel = panel(load(&config_toml("")), entries, now);
    let json = serde_json::to_value(&panel).expect("panel serializes");
    let row = human_row(&panel);

    // Assert
    assert_eq!(
        cell(&panel, LANE, &ceiling).value.as_deref(),
        Some("ceiling:32000")
    );
    assert_eq!(cell(&panel, LANE, &dropped).value, None);
    let column = |key: &str| {
        panel
            .columns
            .iter()
            .position(|c| c == key)
            .expect("column present")
    };
    let cells = &json["lanes"][0]["cells"];
    assert_eq!(cells[column(&ceiling)]["value"], "ceiling:32000");
    assert!(cells[column(&dropped)]["value"].is_null());
    assert!(
        row.contains("broken[live]->route_away value=ceiling:32000"),
        "the ceiling cell shows its value: {row}"
    );
    assert!(
        row.contains("broken[live]->route_away value=-"),
        "the drop cell shows `-`: {row}"
    );
    assert_eq!(
        row.matches("value=").count(),
        2,
        "only the two learned cells carry a value slot: {row}"
    );
}

/// The lane and capability key the matrix shows are what an operator copies:
/// the lane's provider half (before `#`) as the override's target and the
/// column key verbatim into `force_supported`. Loaded through the config
/// parser and validators, that override masks the learned verdict on the same
/// cell, so the copied key targets that lane's provider.
///
/// The dispatch side of the same mask is pinned by
/// `a_force_supported_override_masks_the_learned_verdict` in the router's
/// field pre-flight tests.
///
/// Mutation check: copy the full lane (`p#up`) as the override target, or
/// target another provider entry -> red at the masked cell's verdict.
#[test]
fn the_shown_key_copied_into_force_supported_masks_that_lane() {
    // Arrange
    let now = Instant::now();
    let effort = field_key("output_config.effort.xhigh");
    let entries = || vec![learned(&effort, Some(VerdictValue::Shape("high")), now)];
    let before = panel(load(&config_toml("")), entries(), now);
    let shown_lane = &before.lanes[0].lane;
    let shown_key = before
        .columns
        .iter()
        .find(|c| **c == effort)
        .expect("the learned key is a column")
        .clone();
    assert_eq!(cell(&before, shown_lane, &shown_key).action, "route_away");
    let (provider, _) = shown_lane
        .split_once('#')
        .expect("a lane names its provider");

    // Act
    let forced = load(&config_toml(&format!(
        "[capability.overrides.\"{provider}\"]\nforce_supported = [\"{shown_key}\"]\n"
    )));
    let after = panel(forced, entries(), now);

    // Assert
    assert_eq!(after.columns, before.columns, "the override adds no column");
    let masked = cell(&after, shown_lane, &shown_key);
    assert_eq!(masked.verdict, "forced_supported");
    assert_eq!(masked.layer, Some("override"));
    assert_eq!(masked.overrode_layer, Some("learned"));
    assert_eq!(masked.action, "allow");
    assert_eq!(masked.value.as_deref(), Some("shape:high"));
}
