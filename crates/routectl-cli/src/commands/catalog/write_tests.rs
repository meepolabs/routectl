use super::*;

use routectl_router::{CatalogRow, EffectiveRow, merge, save_catalog_overlay};

// -----------------------------------------------------------------------
// verify_at: stamps an EXISTING overlay cell -- verifying is a user
// act; creating cells is a separate import/set concern.
// -----------------------------------------------------------------------

#[test]
fn verify_at_stamps_existing_user_cell_updates_verified_at_only() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::User,
            verified_at: "2020-01-01".to_string(),
            wm: Some(1.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: Some(512),
            max_context_tokens: None,
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act
    verify_at("openai-compat:grok-*", &path).expect("verify");

    // Assert: source stays user, verified_at bumped to today, values kept.
    let overlay = load_catalog_overlay(&path).expect("load");
    let cell = overlay
        .cells
        .get("openai-compat:grok-*")
        .and_then(Option::as_ref)
        .expect("cell present");
    let today = today_verified_at();
    assert_eq!(cell.source, OverlaySource::User);
    assert_eq!(cell.verified_at, today, "verified_at bumped to UTC today");
    assert_eq!(cell.wm, Some(1.5));
    assert_eq!(cell.min_prefix_tokens, Some(512));
}

#[test]
fn verify_at_flips_import_cell_source_to_user() {
    // Arrange: an import cell -- verifying is a user act, so the source
    // flips even though the cell originated from an import.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2026-01-01".to_string(),
            wm: Some(0.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act
    verify_at("openai-compat:grok-*", &path).expect("verify");

    // Assert
    let overlay = load_catalog_overlay(&path).expect("load");
    let cell = overlay
        .cells
        .get("openai-compat:grok-*")
        .and_then(Option::as_ref)
        .expect("cell present");
    assert_eq!(cell.source, OverlaySource::User);
    assert_eq!(cell.wm, Some(0.5), "value fields carry through unchanged");
}

#[test]
fn verify_at_errors_when_no_overlay_cell_exists_for_selector() {
    // Arrange: an empty overlay (the selector is baked-only or unknown).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    save_catalog_overlay(&path, 0, BTreeMap::new()).expect("seed empty");

    // Act
    let err =
        verify_at("openai-compat:grok-*", &path).expect_err("no overlay cell must be an error");

    // Assert
    let msg = err.to_string();
    assert!(msg.contains("nothing to stamp"), "msg: {msg}");
    assert!(msg.contains("baked-only"), "msg: {msg}");
}

#[test]
fn verify_at_errors_when_overlay_cell_is_disabled() {
    // Arrange: the selector is explicitly disabled (JSON null).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert("openai-compat:grok-*".to_string(), None);
    save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act
    let err =
        verify_at("openai-compat:grok-*", &path).expect_err("a disabled cell must be an error");

    // Assert: never resurrects a disabled row.
    let msg = err.to_string();
    assert!(msg.contains("disabled"), "msg: {msg}");
    let overlay = load_catalog_overlay(&path).expect("load");
    assert_eq!(
        overlay.cells.get("openai-compat:grok-*"),
        Some(&None),
        "the disabled cell must remain untouched"
    );
}

#[test]
fn verify_at_rejects_malformed_selector() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    let err = verify_at("no-colon-here", &path).expect_err("malformed selector must be rejected");
    assert!(err.to_string().contains("invalid selector"), "msg: {err}");
    assert!(!path.exists(), "nothing should have been written");
}

// -----------------------------------------------------------------------
// set_at / disable_at: user-edit verbs.
// -----------------------------------------------------------------------

fn blank_user_cell() -> OverlayCell {
    OverlayCell {
        source: OverlaySource::User,
        verified_at: "2026-01-01".to_string(),
        wm: None,
        rm: None,
        ttl_seconds: None,
        min_prefix_tokens: None,
        max_context_tokens: None,
        max_output_tokens: None,
        input_cost_per_token: None,
        output_cost_per_token: None,
        capabilities: None,
    }
}

#[test]
fn disable_writes_a_null_cell_that_merges_to_disabled_through_the_real_merge_and_never_reuses_a_prior_cell()
 {
    // Arrange: seed a present user cell so a disable's discard-on-write
    // behavior is observable (not just "there was nothing there").
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            min_prefix_tokens: Some(9_999),
            ..blank_user_cell()
        }),
    );
    save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act
    disable_at("openai-compat:grok-*", &path).expect("disable must succeed");

    // Assert: the prior cell's fields are gone -- disabling writes a
    // bare JSON null, not a null-flavored copy of the old values.
    let overlay = load_catalog_overlay(&path).expect("load");
    assert_eq!(overlay.cells.get("openai-compat:grok-*"), Some(&None));
}

#[test]
fn set_at_writes_a_user_cell_for_a_baked_selector_with_the_field_landing() {
    // Arrange: an empty overlay -- "openai-compat:grok-*" is baked-known
    // but has no overlay cell yet.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    // Act
    set_at(
        "openai-compat:grok-*",
        &["min_prefix_tokens=777".to_string()],
        false,
        &path,
    )
    .expect("set must succeed for a known baked selector");

    // Assert
    let overlay = load_catalog_overlay(&path).expect("load");
    let cell = overlay
        .cells
        .get("openai-compat:grok-*")
        .and_then(Option::as_ref)
        .expect("cell present");
    assert_eq!(cell.source, OverlaySource::User);
    assert_eq!(cell.min_prefix_tokens, Some(777));
    let today = today_verified_at();
    assert_eq!(cell.verified_at, today, "verified_at auto-stamped to today");
}

#[test]
fn set_at_on_an_import_cell_flips_source_to_user_and_keeps_unset_fields() {
    // Arrange: an existing import cell for a baked selector.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            source: OverlaySource::Import,
            verified_at: "2020-01-01".to_string(),
            wm: Some(0.5),
            rm: None,
            ttl_seconds: None,
            min_prefix_tokens: None,
            max_context_tokens: None,
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act: set only rm -- this IS ratification-by-edit of the import cell.
    set_at(
        "openai-compat:grok-*",
        &["rm=0.2".to_string()],
        false,
        &path,
    )
    .expect("set must succeed");

    // Assert
    let overlay = load_catalog_overlay(&path).expect("load");
    let cell = overlay
        .cells
        .get("openai-compat:grok-*")
        .and_then(Option::as_ref)
        .expect("cell present");
    assert_eq!(cell.source, OverlaySource::User);
    assert_eq!(cell.rm, Some(0.2));
    assert_eq!(
        cell.wm,
        Some(0.5),
        "the unset wm field carries through from the prior import cell"
    );
}

#[test]
fn disable_writes_a_null_cell_that_merges_to_disabled_through_the_real_merge() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    // Act
    disable_at("openai-compat:grok-*", &path)
        .expect("disable must succeed for a known baked selector");

    // Assert: JSON null on disk, and the real two-layer merge reports
    // Disabled regardless of the baked row underneath.
    let overlay = load_catalog_overlay(&path).expect("load");
    assert_eq!(overlay.cells.get("openai-compat:grok-*"), Some(&None));

    let baked = baked_table_rows();
    let baked_map: BTreeMap<String, CatalogRow> = baked
        .into_iter()
        .map(|c| (format!("{}:{}", c.provider_kind, c.model_glob), c.row))
        .collect();
    let effective = merge(
        baked_map.get("openai-compat:grok-*"),
        overlay.cells.get("openai-compat:grok-*"),
    );
    assert_eq!(effective, EffectiveRow::Disabled);
}

#[test]
fn set_at_rejects_an_unknown_selector() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    let err = set_at(
        "openai-compat:totally-unknown-model-xyz",
        &["min_prefix_tokens=1".to_string()],
        false,
        &path,
    )
    .expect_err("an unknown selector must be rejected");

    assert!(
        matches!(err, CatalogWriteError::UnknownSelector(_)),
        "{err}"
    );
    assert!(!path.exists(), "nothing should have been written");
}

#[test]
fn set_at_reports_unknown_selector_even_when_the_value_would_also_fail_validation() {
    // Admission is checked before value validation: a typo'd selector
    // paired with a bad value reads as "unknown selector", not a
    // confusing validation error about a selector that will be
    // rejected anyway.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    let err = set_at(
        "openai-compat:totally-unknown-model-xyz",
        &["wm=1.0".to_string()],
        false,
        &path,
    )
    .expect_err("an unknown selector must be rejected first");

    assert!(
        matches!(err, CatalogWriteError::UnknownSelector(_)),
        "{err}"
    );
    assert!(!path.exists(), "nothing should have been written");
}

#[test]
fn disable_at_rejects_an_unknown_selector() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    let err = disable_at("openai-compat:totally-unknown-model-xyz", &path)
        .expect_err("an unknown selector must be rejected");

    assert!(
        matches!(err, CatalogWriteError::UnknownSelector(_)),
        "{err}"
    );
    assert!(!path.exists(), "nothing should have been written");
}

#[test]
fn set_at_and_disable_at_surface_a_corrupt_overlay_as_a_transparent_overlay_error() {
    // Arrange: a corrupt overlay file -- `with_overlay_write_lock`'s own
    // `load` fails closed, and that `OverlayError` must propagate through
    // `CatalogWriteError::Overlay` rather than being swallowed or
    // misreported as an admission/validation failure.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    std::fs::write(&path, b"not json {{{").unwrap();

    let err = set_at(
        "openai-compat:grok-*",
        &["min_prefix_tokens=1".to_string()],
        false,
        &path,
    )
    .expect_err("a corrupt overlay must surface as an error");
    assert!(matches!(err, CatalogWriteError::Overlay(_)), "{err}");

    let err = disable_at("openai-compat:grok-*", &path).expect_err("a corrupt overlay must error");
    assert!(matches!(err, CatalogWriteError::Overlay(_)), "{err}");
}

#[test]
fn set_at_rejects_auto_cacher_naming_the_limitation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    let err = set_at(
        "openai-compat:grok-*",
        &["auto_cacher=true".to_string()],
        false,
        &path,
    )
    .expect_err("auto_cacher must be hard-rejected");

    match err {
        CatalogWriteError::UnsupportedField { field, reason } => {
            assert_eq!(field, "auto_cacher");
            assert!(!reason.is_empty(), "the error must name the limitation");
        }
        other => panic!("expected UnsupportedField, got {other:?}"),
    }
    assert!(!path.exists(), "nothing should have been written");
}

#[test]
fn set_at_rejects_storage_rent_fields_and_verified_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    for raw in [
        "has_storage_rent=true",
        "storage_rent=1.0",
        "verified_at=2020-01-01",
    ] {
        let err = set_at("openai-compat:grok-*", &[raw.to_string()], false, &path)
            .expect_err("field must be hard-rejected");
        assert!(
            matches!(err, CatalogWriteError::UnsupportedField { .. }),
            "raw={raw} err={err}"
        );
    }
    assert!(!path.exists());
}

#[test]
fn set_at_rejects_below_sentinel_wm_without_ack_and_accepts_with_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    // Act / Assert: rejected without the ack flag.
    let err = set_at(
        "openai-compat:grok-*",
        &["wm=1.0".to_string()],
        false,
        &path,
    )
    .expect_err("a below-sentinel wm without ack must be rejected");
    assert!(matches!(err, CatalogWriteError::Validation(_)), "{err}");
    assert!(!path.exists(), "nothing should have been written");

    // Act / Assert: the SAME wm, with the ack flag, is accepted.
    set_at("openai-compat:grok-*", &["wm=1.0".to_string()], true, &path)
        .expect("the same wm with the ack flag must be accepted");
    let overlay = load_catalog_overlay(&path).expect("load");
    let cell = overlay
        .cells
        .get("openai-compat:grok-*")
        .and_then(Option::as_ref)
        .expect("cell present");
    assert_eq!(cell.wm, Some(1.0));
}

#[test]
fn validate_updates_enforces_the_override_validate_contract() {
    // rm <= 0 rejected unconditionally.
    assert!(validate_updates(&[FieldUpdate::Rm(0.0)], true).is_err());

    // max_context_tokens == 0 (the "window") rejected.
    assert!(validate_updates(&[FieldUpdate::MaxContextTokens(0)], true).is_err());

    // max_output_tokens == 0 (the "ceiling") rejected on the same contract;
    // a real ceiling passes.
    assert!(validate_updates(&[FieldUpdate::MaxOutputTokens(0)], true).is_err());
    assert!(validate_updates(&[FieldUpdate::MaxOutputTokens(64_000)], false).is_ok());

    // below-sentinel wm needs the ack flag; the same value with the ack
    // flag is accepted.
    assert!(validate_updates(&[FieldUpdate::Wm(1.0)], false).is_err());
    assert!(validate_updates(&[FieldUpdate::Wm(1.0)], true).is_ok());

    // A field that is not being touched at all needs no ack -- an
    // untouched, already-below-sentinel `wm` inherited from a prior
    // cell is never re-validated by this call.
    assert!(validate_updates(&[FieldUpdate::Rm(0.2)], false).is_ok());
}

#[test]
fn set_at_round_trips_max_output_tokens_through_the_overlay() {
    // Arrange: a baked-known selector with no overlay cell yet.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    // Act
    set_at(
        "openai-compat:grok-*",
        &["max_output_tokens=32000".to_string()],
        false,
        &path,
    )
    .expect("set must accept max_output_tokens");

    // Assert: the value survives the write and the fail-closed load.
    let overlay = load_catalog_overlay(&path).expect("load");
    let cell = overlay
        .cells
        .get("openai-compat:grok-*")
        .and_then(Option::as_ref)
        .expect("cell present");
    assert_eq!(cell.max_output_tokens, Some(32_000));
    assert_eq!(cell.source, OverlaySource::User);
}

#[test]
fn set_at_rejects_a_zero_max_output_tokens_before_writing() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    // Act
    let err = set_at(
        "openai-compat:grok-*",
        &["max_output_tokens=0".to_string()],
        false,
        &path,
    )
    .expect_err("a zero output ceiling must be rejected");

    // Assert
    match err {
        CatalogWriteError::Validation(reason) => {
            assert!(reason.contains("max_output_tokens"), "reason: {reason}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
    assert!(!path.exists(), "nothing should have been written");
}

/// The WELD between [`OverlayCell`]'s field set and what `catalog set` can
/// write: every field name on a serialized `OverlayCell` is either accepted
/// by `parse_field` or named in `UNSUPPORTED_FIELDS` with a reason. A new
/// overlay field lands here as a failure until it is deliberately wired or
/// deliberately excluded, which is what keeps adding the next one a
/// one-touch change instead of a silently-dropped write.
///
/// Field names come from `serde_json` rather than a hand-maintained list --
/// a hand list is exactly the drift this test exists to catch.
#[test]
fn every_overlay_cell_field_is_settable_or_deliberately_excluded() {
    // Arrange: a cell with every Option field SET, so nothing is skipped by
    // `skip_serializing_if` and the serialized object names the full field
    // set.
    let fully_populated = OverlayCell {
        source: OverlaySource::User,
        verified_at: "2026-01-01".to_string(),
        wm: Some(2.0),
        rm: Some(0.1),
        ttl_seconds: Some(300),
        min_prefix_tokens: Some(1024),
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        input_cost_per_token: Some(3.0e-6),
        output_cost_per_token: Some(1.5e-5),
        capabilities: Some(BTreeMap::from([("web_search".to_string(), true)])),
    };
    let serialized = serde_json::to_value(&fully_populated).expect("serialize");
    let field_names: Vec<String> = serialized
        .as_object()
        .expect("an OverlayCell serializes to a JSON object")
        .keys()
        .cloned()
        .collect();
    assert!(
        field_names.len() >= 11,
        "every field must serialize; got {field_names:?}"
    );

    // Act / Assert
    for field in &field_names {
        let excluded = UNSUPPORTED_FIELDS.iter().find(|(name, _)| name == field);
        if let Some((_, reason)) = excluded {
            assert!(
                !reason.is_empty(),
                "excluded field `{field}` must carry a reason"
            );
            continue;
        }
        // Not excluded -> `parse_field` must accept it. The value is a
        // plausible one for every numeric field; a parse failure here means
        // the field has no `FieldUpdate` arm.
        parse_field(&format!("{field}=1")).unwrap_or_else(|e| {
            panic!(
                "OverlayCell field `{field}` is neither settable by `catalog set` nor named in \
                 UNSUPPORTED_FIELDS with a reason: {e}"
            )
        });
    }
}

#[test]
fn parse_field_accepts_a_capability_flag_and_rejects_a_malformed_pair() {
    match parse_field("cap:web_search=true") {
        Ok(FieldUpdate::Capability(name, flag)) => {
            assert_eq!(name, "web_search");
            assert!(flag);
        }
        other => panic!("expected a Capability update, got {other:?}"),
    }

    let err = parse_field("no-equals-sign").expect_err("must reject a pair with no `=`");
    assert!(
        matches!(err, CatalogWriteError::InvalidField { .. }),
        "{err}"
    );

    let err = parse_field("wm=not-a-number").expect_err("must reject a malformed number");
    assert!(
        matches!(err, CatalogWriteError::InvalidField { .. }),
        "{err}"
    );
}

// -----------------------------------------------------------------------
// export_at: read-only overlay dump -- round-trips, never writes, and
// carries no credential material.
// -----------------------------------------------------------------------

#[test]
fn export_at_round_trips_back_into_an_equal_overlay() {
    // Arrange: an overlay with one import cell, one user cell, and one
    // disabled cell, persisted through the real writer.
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
            max_output_tokens: None,
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
            max_output_tokens: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            capabilities: None,
        }),
    );
    cells.insert("openai-compat:disabled-model".to_string(), None);
    let saved = save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act
    let json = export_at(&path).expect("export");

    // Assert: the JSON deserializes back into an overlay equal to the
    // one on disk -- every cell (present, user, disabled) preserved.
    let restored: CatalogOverlay =
        serde_json::from_str(&json).expect("export output must deserialize");
    assert_eq!(restored, saved);
}

#[test]
fn restore_round_trips_an_exported_overlay_through_the_fs_load_path() {
    // The documented restore is contract-free: place the exported JSON
    // at the overlay path and the next load picks it up. Prior coverage
    // proved only the serde round-trip; this pins the FILESYSTEM seam --
    // export, write to `catalog_overlay.json`, reload via the real
    // fail-closed load path.
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("source_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "anthropic-api:claude-opus-4-8*".to_string(),
        Some(user_cell_with_capability()),
    );
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            min_prefix_tokens: Some(512),
            ..blank_user_cell()
        }),
    );
    cells.insert("openai-compat:disabled-model".to_string(), None);
    let source = save_catalog_overlay(&source_path, 0, cells).expect("seed source overlay");

    // Export, then restore by writing the JSON verbatim at a fresh
    // overlay path -- byte-for-byte what the `export` verb emits.
    let exported = export_at(&source_path).expect("export");
    let restore_path = dir.path().join("catalog_overlay.json");
    std::fs::write(&restore_path, format!("{exported}\n"))
        .expect("write exported JSON to the overlay path");

    // Reload through the real fail-closed fs load path.
    let restored = load_catalog_overlay(&restore_path).expect("load restored overlay");

    // Cell equality with the source overlay: every present, user, and
    // disabled cell survives the disk round-trip.
    assert_eq!(restored.cells, source.cells);
    assert_eq!(restored, source);
}

#[test]
fn export_at_is_read_only_and_leaves_the_overlay_byte_identical() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "openai-compat:grok-*".to_string(),
        Some(OverlayCell {
            min_prefix_tokens: Some(512),
            ..blank_user_cell()
        }),
    );
    save_catalog_overlay(&path, 0, cells).expect("seed");
    let before = std::fs::read(&path).unwrap();

    // Act
    export_at(&path).expect("export");

    // Assert: the exporter never opened the overlay for writing.
    let after = std::fs::read(&path).unwrap();
    assert_eq!(before, after, "export must not mutate the overlay file");
}

#[test]
fn export_at_output_carries_no_credential_material() {
    // Arrange: a fully-populated cell -- the export shape is catalog
    // cells only, so none of the secret-shaped keys a credentials store
    // would carry can ever appear in it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");
    let mut cells = BTreeMap::new();
    cells.insert(
        "anthropic-api:claude-opus-4-8*".to_string(),
        Some(user_cell_with_capability()),
    );
    save_catalog_overlay(&path, 0, cells).expect("seed");

    // Act
    let json = export_at(&path).expect("export");

    // Assert: no credential-shaped JSON key appears (quoted-key form so
    // the legitimate `*_tokens` cell fields are not false positives).
    for secret_key in [
        "\"api_key\"",
        "\"secret\"",
        "\"password\"",
        "\"credential\"",
        "\"access_token\"",
        "\"refresh_token\"",
    ] {
        assert!(
            !json.to_ascii_lowercase().contains(secret_key),
            "export must not carry {secret_key}: {json}"
        );
    }
}

#[test]
fn export_at_on_a_missing_overlay_emits_the_empty_default() {
    // Arrange: no overlay file exists yet (first run).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog_overlay.json");

    // Act
    let json = export_at(&path).expect("export must succeed on first run");

    // Assert: an empty, current-schema overlay -- and still no write.
    let restored: CatalogOverlay = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, CatalogOverlay::default());
    assert!(!path.exists(), "export must not create the overlay file");
}

fn user_cell_with_capability() -> OverlayCell {
    OverlayCell {
        source: OverlaySource::User,
        verified_at: "2026-07-05".to_string(),
        wm: None,
        rm: None,
        ttl_seconds: None,
        min_prefix_tokens: Some(1024),
        max_context_tokens: Some(200_000),
        max_output_tokens: None,
        input_cost_per_token: None,
        output_cost_per_token: None,
        capabilities: Some(BTreeMap::from([("web_search".to_string(), true)])),
    }
}
