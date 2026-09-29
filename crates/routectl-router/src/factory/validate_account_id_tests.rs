use crate::config::Config;
use crate::factory::collect_config_validation;

fn errors_for(toml_text: &str) -> Vec<String> {
    let config: Config = toml::from_str(toml_text).expect("fixture must parse");
    collect_config_validation(&config).errors
}

fn has_account_id_error(errors: &[String]) -> bool {
    errors.iter().any(|e| e.contains("account_id_ref"))
}

#[test]
fn static_chatgpt_oauth_bearer_without_account_id_ref_is_refused() {
    // Arrange: the env var is never set, so a refusal cannot come from a
    // secret read.
    let text = "[providers.codex]\nkind = \"openai-responses\"\n\
                auth_kind = \"chatgpt-oauth\"\n\
                api_key_ref = \"env://ROUTECTL_TEST_UNSET_OPENAI_JWT\"\n";

    // Act
    let errors = errors_for(text);

    // Assert
    assert!(
        errors
            .iter()
            .any(|e| e.contains("requires `account_id_ref`") && e.contains("`codex`")),
        "{errors:?}"
    );
}

#[test]
fn static_chatgpt_oauth_bearer_with_account_id_ref_is_accepted() {
    let text = "[providers.codex]\nkind = \"openai-responses\"\n\
                auth_kind = \"chatgpt-oauth\"\n\
                api_key_ref = \"env://ROUTECTL_TEST_UNSET_OPENAI_JWT\"\n\
                account_id_ref = \"env://ROUTECTL_TEST_UNSET_OPENAI_ACCOUNT\"\n";

    let errors = errors_for(text);

    assert!(!has_account_id_error(&errors), "{errors:?}");
}

#[test]
fn oauth_bearer_may_omit_account_id_ref() {
    let text = "[providers.codex]\nkind = \"openai-responses\"\n\
                auth_kind = \"chatgpt-oauth\"\napi_key_ref = \"oauth://codex\"\n";

    let errors = errors_for(text);

    assert!(!has_account_id_error(&errors), "{errors:?}");
}

#[test]
fn api_key_auth_with_account_id_ref_is_refused() {
    let text = "[providers.openai]\nkind = \"openai-responses\"\n\
                auth_kind = \"api-key\"\napi_key_ref = \"env://ROUTECTL_TEST_UNSET_KEY\"\n\
                account_id_ref = \"env://ROUTECTL_TEST_UNSET_OPENAI_ACCOUNT\"\n";

    let errors = errors_for(text);

    assert!(
        errors
            .iter()
            .any(|e| e.contains("`account_id_ref` is only valid")),
        "{errors:?}"
    );
}
