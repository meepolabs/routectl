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
    assert!(
        value.pointer("/$defs/ConfigFailureClass").is_some(),
        "the config-facing failure-class enum must land in the schema $defs"
    );
}

/// `class_overrides` is declared `BTreeMap<u16, ConfigFailureClass>` but
/// serializes with STRING keys via `#[serde(with)]`. The matching
/// `#[schemars(with = "BTreeMap<String, ..>")]` attribute is mandatory:
/// without it the derive tries to use the serde `with` module path as a
/// type and fails to compile. This pins the rendered shape to a
/// string-keyed object whose values reference `ConfigFailureClass`, so a
/// future edit that drops the attribute is caught by a schema assertion
/// as well as by the compiler.
#[test]
fn class_overrides_renders_string_keyed() {
    let schema = schemars::schema_for!(Config);
    let value = serde_json::to_value(&schema).expect("schema serializes to JSON");

    // `runtime` is flattened onto every ProviderEntry variant, so
    // `class_overrides` appears as a property on each oneOf arm.
    let arms = value
        .pointer("/$defs/ProviderEntry/oneOf")
        .and_then(serde_json::Value::as_array)
        .expect("ProviderEntry renders as a oneOf of tagged variants");

    let mut seen = 0usize;
    for arm in arms {
        let Some(field) = arm.pointer("/properties/class_overrides") else {
            continue;
        };
        seen += 1;
        assert_eq!(
            field.pointer("/type").and_then(serde_json::Value::as_str),
            Some("object"),
            "class_overrides must be a string-keyed object, got: {field}"
        );
        assert_eq!(
            field
                .pointer("/additionalProperties/$ref")
                .and_then(serde_json::Value::as_str),
            Some("#/$defs/ConfigFailureClass"),
            "class_overrides values must reference ConfigFailureClass, got: {field}"
        );
    }
    assert!(
        seen > 0,
        "class_overrides must surface on at least one provider variant"
    );
}
