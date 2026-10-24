use super::*;

// -----------------------------------------------------------------------
// Legacy sidecar: read side only (round-trips a manually-written file --
// the write side that used to produce it is gone).
// -----------------------------------------------------------------------

#[test]
fn load_verifications_reads_a_manually_written_sidecar_file() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pricing_verifications.json");
    std::fs::write(
        &path,
        r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
    )
    .unwrap();

    // Act
    let loaded = load_verifications(&path).unwrap();

    // Assert
    assert_eq!(
        loaded
            .verified
            .get("openai-compat:grok-*")
            .map(String::as_str),
        Some("2026-06-30")
    );
}

#[test]
fn load_verifications_missing_path_returns_empty_default() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does_not_exist.json");

    // Act
    let result = load_verifications(&path);

    // Assert -- not an error; the map is empty
    assert!(result.is_ok());
    assert!(result.unwrap().verified.is_empty());
}

#[test]
fn load_verifications_malformed_json_returns_error() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, b"not valid json {{{").unwrap();

    // Act
    let result = load_verifications(&path);

    // Assert
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("malformed"), "expected 'malformed' in: {msg}");
}
