use super::Config;

/// The whole `Config` tree renders a JSON schema without panicking.
/// Guards the derive coverage: any config-surface type missing the
/// `JsonSchema` derive is a compile error, and any structural issue
/// (recursion, unsupported shape) surfaces here.
#[test]
fn config_schema_renders() {
    let schema = schemars::schema_for!(Config);
    let value = serde_json::to_value(&schema).expect("schema serializes to JSON");
    assert!(value.is_object(), "root schema must be a JSON object");
    assert_eq!(
        value
            .pointer("/$defs/RetryPolicy/properties/classes/additionalProperties/$ref")
            .and_then(serde_json::Value::as_str),
        Some("#/$defs/ClassPolicy"),
        "the per-class retry overlay must render as a map of ClassPolicy"
    );
}
