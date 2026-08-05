use super::*;

use routectl_router::{OverlayCell, load_catalog_overlay};

// -----------------------------------------------------------------------
// overlay_summary_line: list's header (revision + counts by source).
// -----------------------------------------------------------------------

#[test]
fn overlay_summary_line_counts_user_import_and_disabled_cells() {
    // Arrange: one of each state, at a non-zero revision.
    let mut overlay = CatalogOverlay {
        revision: 4,
        ..CatalogOverlay::default()
    };
    overlay.cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            ..blank_user_cell()
        }),
    );
    overlay.cells.insert(
        "openai-compat:mistral-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::Import,
            ..blank_user_cell()
        }),
    );
    overlay
        .cells
        .insert("openai-compat:disabled-model".to_string(), None);

    // Act
    let line = overlay_summary_line(&overlay);

    // Assert
    assert!(line.contains("revision 4"), "line: {line}");
    assert!(line.contains("3 cell(s)"), "line: {line}");
    assert!(line.contains("1 user"), "line: {line}");
    assert!(line.contains("1 import"), "line: {line}");
    assert!(line.contains("1 disabled"), "line: {line}");
}

#[test]
fn overlay_summary_line_on_an_empty_overlay_reports_zero_counts() {
    let line = overlay_summary_line(&CatalogOverlay::default());
    assert!(line.contains("revision 0"), "line: {line}");
    assert!(line.contains("0 cell(s)"), "line: {line}");
}

// -----------------------------------------------------------------------
// build_list_data: PRESENT (baked / import / user), DISABLED, punch-list
// -----------------------------------------------------------------------

#[test]
fn present_baked_row_shows_baked_source_and_no_stale_warn() {
    // Arrange: no overlay entry -- the baked cell wins as-is.
    let overlay = CatalogOverlay::default();

    // Act
    let (rows, _) = build_list_data(&overlay);

    // Assert
    let row = find_row(&rows, "openai-compat", "grok-*").expect("baked row present");
    assert_eq!(row[2], "PRESENT");
    assert_eq!(row[10], "baked");
    assert_eq!(row[12], "-", "the fresh baked snapshot date is not stale");
}

#[test]
fn present_import_cell_overrides_baked_and_shows_import_source() {
    // Arrange: an import cell for a real baked selector, overriding wm.
    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-07-01".to_string(),
            wm: Some(0.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );

    // Act
    let (rows, _) = build_list_data(&overlay);

    // Assert
    let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
    assert_eq!(row[2], "PRESENT");
    assert_eq!(row[4], "0.5000", "wm overridden by the import cell");
    assert_eq!(row[10], "import");
    assert_eq!(row[11], "2026-07-01");
}

#[test]
fn present_user_cell_with_no_baked_match_renders_from_sentinel_base() {
    // Arrange: a user cell naming a selector no baked cell backs.
    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(
        "openai-compat:totally-new-model-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-05".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: Some(999),
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );

    // Act
    let (rows, _) = build_list_data(&overlay);

    // Assert: sentinel base (wm 2.0) with the user's min_prefix override.
    let row = find_row(&rows, "openai-compat", "totally-new-model-*").expect("row present");
    assert_eq!(row[2], "PRESENT");
    assert_eq!(row[4], "2.0000", "sentinel wm as the base");
    assert_eq!(row[7], "999", "user min_prefix override applied");
    assert_eq!(row[10], "user");
}

#[test]
fn disabled_cell_renders_disabled_status_regardless_of_baked() {
    // Arrange: a null overlay entry for a real baked selector.
    let mut overlay = CatalogOverlay::default();
    overlay
        .cells
        .insert("openai-compat:grok-*".to_string(), None);

    // Act
    let (rows, _) = build_list_data(&overlay);

    // Assert: every economics / provenance column dashes out.
    let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
    assert_eq!(row[2], "DISABLED");
    for col in &row[3..] {
        assert_eq!(col, "-");
    }
}

#[test]
fn stale_verified_at_renders_warn_marker() {
    // Arrange: an import cell stamped far in the past.
    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2020-01-01".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );

    // Act
    let (rows, _) = build_list_data(&overlay);

    // Assert
    let row = find_row(&rows, "openai-compat", "grok-*").expect("row present");
    assert_eq!(row[12], "WARN");
}

#[test]
fn round_trip_display_renders_import_user_and_disabled_states() {
    // Arrange: one import cell, one user cell, one null-disabled cell,
    // all round-tripped through the real overlay writer/loader.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-07-01".to_string(),
            wm: Some(0.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    cells.insert(
        "anthropic-api:claude-opus-4-8*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-05".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: Some(1024),
            max_context_tokens: Some(200_000),
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    cells.insert("openai-compat:disabled-model".to_string(), None);
    routectl_router::save_catalog_overlay(&path, 0, cells).expect("save");

    // Act
    let overlay = load_catalog_overlay(&path).expect("load");
    let (rows, _) = build_list_data(&overlay);

    // Assert: all three states render distinctly.
    let import_row = find_row(&rows, "openai-compat", "grok-*").expect("import row");
    assert_eq!(import_row[2], "PRESENT");
    assert_eq!(import_row[10], "import");

    let user_row = find_row(&rows, "anthropic-api", "claude-opus-4-8*").expect("user row");
    assert_eq!(user_row[2], "PRESENT");
    assert_eq!(user_row[10], "user");

    let disabled_row = find_row(&rows, "openai-compat", "disabled-model").expect("disabled row");
    assert_eq!(disabled_row[2], "DISABLED");
}

#[test]
fn missing_state_renders_missing_status() {
    // MISSING requires calling `merge` with neither layer present, which
    // cannot arise from `build_list_data`'s own baked-union-overlay key
    // enumeration (every enumerated key backs onto at least one layer by
    // construction). Exercised directly here so the render path's
    // MISSING arm is covered.
    let effective = merge(None, None);
    assert_eq!(effective, EffectiveRow::Missing);

    let row = render_row("openai-compat:nowhere-*", &effective);
    assert_eq!(row[2], "MISSING");
    for col in &row[3..] {
        assert_eq!(col, "-");
    }
}

#[test]
fn punch_list_names_present_row_with_unknown_max_context_tokens() {
    // Arrange: the anthropic-api "*" catch-all is baked with an unknown
    // window; no overlay entry supplies one.
    let overlay = CatalogOverlay::default();

    // Act
    let (_, punch_list) = build_list_data(&overlay);

    // Assert
    assert!(
        punch_list.contains(&"anthropic-api:*".to_string()),
        "punch_list: {punch_list:?}"
    );
    assert!(
        !punch_list.contains(&"anthropic-api:claude-opus-4-8*".to_string()),
        "a known-window model must not appear: {punch_list:?}"
    );
}

#[test]
fn punch_list_cleared_when_overlay_supplies_a_window() {
    // Arrange: an overlay cell supplies the missing window for the
    // otherwise-unknown anthropic-api "*" catch-all.
    let mut overlay = CatalogOverlay::default();
    overlay.cells.insert(
        "anthropic-api:*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2026-07-05".to_string(),
            wm: None,
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: Some(200_000),
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );

    // Act
    let (_, punch_list) = build_list_data(&overlay);

    // Assert
    assert!(
        !punch_list.contains(&"anthropic-api:*".to_string()),
        "punch_list: {punch_list:?}"
    );
}

/// Locate a rendered row by `(provider_kind, model_glob)`.
fn find_row<'a>(
    rows: &'a [Vec<String>],
    provider_kind: &str,
    model_glob: &str,
) -> Option<&'a Vec<String>> {
    rows.iter()
        .skip(1)
        .find(|r| r[0] == provider_kind && r[1] == model_glob)
}

fn blank_user_cell() -> OverlayCell {
    OverlayCell {
        source: OverlaySource::User,
        verified_at: "2026-01-01".to_string(),
        wm: None,
        rm: None,
        ttl_seconds: None,
        min_prefix_tokens: None,
        max_context_tokens: None,
        input_cost_per_token: None,
        output_cost_per_token: None,
        capabilities: None,
    }
}
