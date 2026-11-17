//! Loaded via `#[cfg(test)] #[path = "provider_entry_tests.rs"] mod tests;`
//! in `provider_entry.rs`.

use super::*;

use routectl_providers::anthropic_api::CloakMode;
use routectl_router::{Auth, Format};

/// The shipped example config: the reference shape every emitted block
/// must reproduce byte for byte.
const EXAMPLE_CONFIG: &str = include_str!("../../../../examples/config.toml");
/// The configuration reference, which shows the same shapes.
const CONFIGURATION_DOC: &str = include_str!("../../../../docs/CONFIGURATION.md");

fn bedrock_west() -> ProviderEntry {
    ProviderEntry::cloud_aws(
        Format::BedrockInvoke,
        "us-west-2",
        "aws://chain",
        Auth::AwsSigv4,
    )
}

fn rendered(name: &str, entry: &ProviderEntry) -> String {
    render_provider_block(
        name,
        &provider_entry_table(entry).expect("entry serializes"),
    )
}

/// serde's document serializer alone writes `cloud` as a trailing `[cloud]`
/// sub-table after every plain key, so the identity order cannot come from
/// `toml::to_string`; the emitter has to place it.
#[test]
fn serde_alone_moves_cloud_into_a_trailing_sub_table() {
    // Act
    let text = toml::to_string(&bedrock_west()).expect("serializes");

    // Assert
    assert_eq!(
        text,
        "format = \"bedrock-invoke\"\n\
         credential = \"aws://chain\"\n\
         auth = \"aws-sigv4\"\n\
         \n\
         [cloud]\n\
         name = \"aws\"\n\
         region = \"us-west-2\"\n"
    );
}

#[test]
fn a_base_url_block_is_written_in_identity_order() {
    // Arrange
    let entry =
        ProviderEntry::openai_compat("https://openrouter.ai/api/v1", "env://OPENROUTER_API_KEY");

    // Act
    let block = rendered("openrouter", &entry);

    // Assert: the literal shape, and that it IS the shipped example block.
    assert_eq!(
        block,
        "[providers.openrouter]\n\
         format     = \"openai-chat\"\n\
         base_url   = \"https://openrouter.ai/api/v1\"\n\
         credential = \"env://OPENROUTER_API_KEY\"\n\
         auth       = \"bearer\"\n"
    );
    assert!(EXAMPLE_CONFIG.contains(&block), "{block}");
    assert!(CONFIGURATION_DOC.contains(&block), "{block}");
}

#[test]
fn a_cloud_block_writes_cloud_inline_in_the_carrier_slot() {
    // Act
    let block = rendered("bedrock", &bedrock_west());

    // Assert
    assert_eq!(
        block,
        "[providers.bedrock]\n\
         format     = \"bedrock-invoke\"\n\
         cloud      = { name = \"aws\", region = \"us-west-2\" }\n\
         credential = \"aws://chain\"\n\
         auth       = \"aws-sigv4\"\n"
    );
    assert!(EXAMPLE_CONFIG.contains(&block), "{block}");
}

#[test]
fn the_written_table_carries_cloud_inline_not_as_a_sub_table() {
    // Act
    let table = provider_entry_table(&bedrock_west()).expect("serializes");

    // Assert
    assert_eq!(
        table.to_string(),
        "format = \"bedrock-invoke\"\n\
         cloud = { name = \"aws\", region = \"us-west-2\" }\n\
         credential = \"aws://chain\"\n\
         auth = \"aws-sigv4\"\n"
    );
}

/// The entry with tuning keys set on both sides of the flattened runtime
/// policy, and a nested JSON value: every field the typed entry carries.
fn tuned_entry() -> ProviderEntry {
    let mut entry = ProviderEntry::anthropic_api("env://ANTHROPIC_API_KEY");
    entry.cloak_mode = Some(CloakMode::Always);
    entry.cloak_strict_mode = Some(true);
    entry.context_management = Some(false);
    entry.user_agent = Some("ops-probe/1".into());
    entry
        .header_extras
        .insert("anthropic-beta".into(), "context-1m-2025-08-07".into());
    entry.payload_extras = Some(serde_json::json!({ "metadata": { "tier": "gold" } }));
    entry.runtime.rpm_limit = Some(120);
    entry.runtime.request_timeout_ms = Some(90_000);
    entry
}

#[test]
fn a_tuned_entry_reparses_to_the_same_entry() {
    // Arrange
    let entry = tuned_entry();

    // Act
    let table = provider_entry_table(&entry).expect("serializes");
    let reparsed: ProviderEntry = toml::from_str(&table.to_string()).expect("reparses");

    // Assert
    assert_eq!(
        serde_json::to_value(&reparsed).expect("json"),
        serde_json::to_value(&entry).expect("json")
    );
    assert!(table.contains_key("cloak_mode"), "{table}");
    assert!(table.contains_key("cloak_strict_mode"), "{table}");
    assert!(table.contains_key("rpm_limit"), "{table}");
}

#[test]
fn tuning_keys_follow_the_identity_keys_in_declaration_order() {
    // Act
    let table = provider_entry_table(&tuned_entry()).expect("serializes");

    // Assert
    let keys: Vec<&str> = table.iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        [
            "format",
            "base_url",
            "credential",
            "auth",
            "context_management",
            "cloak_mode",
            "cloak_strict_mode",
            "header_extras",
            "payload_extras",
            "user_agent",
            "rpm_limit",
            "request_timeout_ms",
        ]
    );
}

#[test]
fn an_empty_collection_is_not_written() {
    // Arrange: the control key proves the tail is emitted at all.
    let mut entry = ProviderEntry::openai_compat("https://api.x.example/v1", "env://KEY");
    entry.forward_client_headers = Some(Vec::new());
    entry.user_agent = Some("ua/1".into());

    // Act
    let table = provider_entry_table(&entry).expect("serializes");

    // Assert
    assert!(!table.contains_key("forward_client_headers"), "{table}");
    assert!(table.contains_key("user_agent"), "{table}");
}

#[test]
fn a_null_inside_payload_extras_is_an_error_not_a_dropped_field() {
    // Arrange
    let mut entry = ProviderEntry::openai_compat("https://api.x.example/v1", "env://KEY");
    entry.payload_extras = Some(serde_json::json!({ "a": null }));

    // Act
    let result = provider_entry_table(&entry);

    // Assert
    assert!(result.is_err());
}

#[test]
fn a_key_with_toml_punctuation_is_quoted_in_the_block_header() {
    // Arrange
    let entry = ProviderEntry::openai_compat("https://api.x.example/v1", "env://KEY");

    // Act
    let block = rendered("seat b", &entry);

    // Assert
    assert!(block.starts_with("[providers.\"seat b\"]\n"), "{block}");
}

/// A control character anywhere in a value (top-level string, a map value
/// inside an inline table) is written escaped on one line, and the block
/// still parses back to the same entry.
#[test]
fn a_control_character_in_a_value_is_escaped_on_one_line() {
    // Arrange
    let mut entry = ProviderEntry::openai_compat("https://api.x.example/v1", "oauth://x#a\nb");
    entry.header_extras.insert("x-note".into(), "c\td".into());

    // Act
    let block = rendered("p", &entry);

    // Assert
    assert!(
        block.contains("credential    = \"oauth://x#a\\nb\"\n"),
        "{block}"
    );
    assert!(
        block.contains("header_extras = { x-note = \"c\\td\" }\n"),
        "{block}"
    );
    assert_eq!(block.lines().count(), 6, "{block}");
    let reparsed: toml::Table = toml::from_str(&block).expect("block parses");
    let credential = reparsed["providers"]["p"]["credential"].as_str();
    assert_eq!(credential, Some("oauth://x#a\nb"));
}
