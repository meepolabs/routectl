//! Smoke tests: every provider-crate type reachable from the router's
//! `ProviderEntry` config surface must produce a JSON Schema without
//! panicking. These guard the `schemars::JsonSchema` derives the
//! router-side `Config` schema generation depends on.
//!
//! Each test is gated to the feature that owns its type, so the suite
//! stays green under `--no-default-features` builds that omit a provider.

fn assert_renders(schema: schemars::Schema) {
    // A generated schema is an object at the root; serializing it is the
    // operation the schema artifact pipeline performs, so exercise it.
    let json = serde_json::to_value(&schema).expect("schema serializes to JSON");
    assert!(
        json.is_object() || json.is_boolean(),
        "schema root is a JSON Schema value"
    );
}

#[cfg(feature = "anthropic-api")]
#[test]
fn anthropic_auth_kind_schema_renders() {
    assert_renders(schemars::schema_for!(
        routectl_providers::anthropic_api::AuthKind
    ));
}

#[cfg(feature = "anthropic-api")]
#[test]
fn cloak_config_schema_renders() {
    assert_renders(schemars::schema_for!(
        routectl_providers::anthropic_api::CloakConfig
    ));
}

#[cfg(feature = "anthropic-api")]
#[test]
fn cloak_mode_schema_renders() {
    assert_renders(schemars::schema_for!(
        routectl_providers::anthropic_api::CloakMode
    ));
}

#[cfg(feature = "anthropic-api")]
#[test]
fn tool_rename_schema_renders() {
    assert_renders(schemars::schema_for!(
        routectl_providers::anthropic_api::ToolRename
    ));
}

#[cfg(feature = "openai-responses")]
#[test]
fn openai_responses_auth_kind_schema_renders() {
    assert_renders(schemars::schema_for!(
        routectl_providers::openai_responses::AuthKind
    ));
}

#[cfg(feature = "gemini")]
#[test]
fn gemini_auth_mode_schema_renders() {
    assert_renders(schemars::schema_for!(
        routectl_providers::gemini::GeminiAuthMode
    ));
}
