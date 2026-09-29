use super::{MAX_PROVIDER_NAME_BYTES, validate_state_key_names};
use crate::config::{Config, LEAN_EXAMPLE_CONFIG};
use crate::factory::collect_config_validation;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture must parse")
}

const PROVIDER: &str = "[providers.anthropic-work]\n\
     kind = \"anthropic-api\"\n\
     api_key_ref = \"oauth://anthropic#work\"\n";

fn model(nickname: &str, provider: &str, selectable: bool) -> String {
    format!(
        "[models.\"{nickname}\"]\nprovider = \"{provider}\"\nupstream = \"u\"\n\
         selectable = {selectable}\n"
    )
}

fn errors(config: &Config) -> String {
    validate_state_key_names(config)
        .expect_err("config must be refused")
        .to_string()
}

#[test]
fn a_model_nickname_carrying_the_separator_is_refused() {
    // Arrange
    let config = parse(&format!(
        "{PROVIDER}{}",
        model("a#b", "anthropic-work", true)
    ));

    // Act
    let err = errors(&config);

    // Assert
    assert!(err.contains("model nickname `a#b`"), "{err}");
    assert!(err.contains("seat-pool state-key separator"), "{err}");
}

#[test]
fn an_unselectable_model_nickname_carrying_the_separator_is_refused() {
    // The factory skips unselectable entries before its own `#` check, so
    // config validation is the only boundary that sees this one.
    let config = parse(&format!(
        "{PROVIDER}{}",
        model("off#line", "anthropic-work", false)
    ));

    let err = errors(&config);

    assert!(err.contains("model nickname `off#line`"), "{err}");
}

#[test]
fn an_unreferenced_model_nickname_carrying_the_separator_is_refused() {
    // No alias names it and its provider does not exist: nothing else in the
    // suite would ever look at this entry.
    let config = parse(&model("orphan#seat", "missing", true));

    let err = errors(&config);

    assert!(err.contains("model nickname `orphan#seat`"), "{err}");
}

#[test]
fn a_provider_name_carrying_the_separator_is_refused() {
    // `opus#anthropic-work` is exactly the key a pooled seat of model `opus`
    // on member `anthropic-work` composes; a provider holding it would own
    // that seat's breaker and RPM slot.
    let config = parse(
        "[providers.\"opus#anthropic-work\"]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://KEY\"\n",
    );

    let err = errors(&config);

    assert!(err.contains("provider name `opus#anthropic-work`"), "{err}");
    assert!(err.contains("seat-pool state-key separator"), "{err}");
}

/// A second provider, so a model can be named after one provider while
/// dispatching through another.
const OTHER: &str = "[providers.other]\nkind = \"anthropic-api\"\napi_key_ref = \"env://K\"\n";

#[test]
fn a_model_nickname_equal_to_another_providers_name_is_refused() {
    // The provider's slot is seeded from `anthropic-work`'s policy while the
    // model dispatches through `other`: one key, two different gates.
    let config = parse(&format!(
        "{PROVIDER}{OTHER}{}",
        model("anthropic-work", "other", true)
    ));

    let err = errors(&config);

    assert!(err.contains("model nickname `anthropic-work`"), "{err}");
    assert!(err.contains("provider name"), "{err}");
    assert!(err.contains("dispatches through `other`"), "{err}");
    assert!(err.contains("runtime state"), "{err}");
}

#[test]
fn a_model_named_after_its_own_provider_passes() {
    // The shape `routectl init` writes: one slot, seeded from the provider
    // the model dispatches through, so no second identity shares it.
    let config = parse(&format!(
        "{PROVIDER}{}",
        model("anthropic-work", "anthropic-work", true)
    ));

    assert!(validate_state_key_names(&config).is_ok());
}

#[test]
fn every_offending_key_is_reported_in_one_pass() {
    // Arrange: one of each refusal class at once.
    let config = parse(&format!(
        "[providers.\"p#1\"]\nkind = \"anthropic-api\"\napi_key_ref = \"env://K\"\n\
         {PROVIDER}{OTHER}{}{}",
        model("m#1", "anthropic-work", false),
        model("anthropic-work", "other", true),
    ));

    // Act
    let err = errors(&config);

    // Assert
    assert!(err.contains("provider name `p#1`"), "{err}");
    assert!(err.contains("model nickname `m#1`"), "{err}");
    assert!(err.contains("equals"), "{err}");
    assert_eq!(err.lines().count(), 3, "one line per offending key: {err}");
}

#[test]
fn each_refusal_surfaces_through_the_collected_suite() {
    // Wiring proof: serve, reload, `config check` and doctor all read the
    // one collected suite.
    let cases = [
        format!("{PROVIDER}{}", model("x#y", "anthropic-work", false)),
        "[providers.\"p#q\"]\nkind = \"anthropic-api\"\napi_key_ref = \"env://K\"\n".to_string(),
        format!(
            "{PROVIDER}{OTHER}{}",
            model("anthropic-work", "other", true)
        ),
    ];
    for text in cases {
        let config = parse(&text);

        let found = collect_config_validation(&config)
            .errors
            .iter()
            .any(|e| e.contains("runtime state") || e.contains("seat-pool state-key separator"));

        assert!(found, "must surface through the collected suite: {text}");
    }
}

#[test]
fn ordinary_direct_and_pooled_configs_pass() {
    // Positive control: distinct provider, pool, and model names -- the
    // shape the shipped example and every generated config use.
    let config = parse(&format!(
        "{PROVIDER}\
         [providers.anthropic-default]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n\
         [pools.anthropic]\nmembers = [\"anthropic-default\", \"anthropic-work\"]\n\
         {}{}",
        model("opus", "anthropic", true),
        model("haiku-pinned", "anthropic-work", false),
    ));

    assert!(
        validate_state_key_names(&config).is_ok(),
        "{:?}",
        validate_state_key_names(&config).err()
    );
}

#[test]
fn the_lean_example_config_passes() {
    let config = parse(LEAN_EXAMPLE_CONFIG);

    assert!(validate_state_key_names(&config).is_ok());
}

#[test]
fn a_model_nickname_equal_to_a_provider_name_dispatching_through_a_pool_is_refused() {
    // Arrange: `anthropic-work` is a provider AND a model whose traffic goes
    // through pool `anthropic`, even though that pool's only member is the
    // same-named provider. The provider slot and the pooled model's own slot
    // would share one key.
    let config = parse(&format!(
        "{PROVIDER}\
         [pools.anthropic]\nmembers = [\"anthropic-work\"]\n\
         {}",
        model("anthropic-work", "anthropic", true),
    ));

    // Act
    let err = errors(&config);

    // Assert
    assert!(err.contains("model nickname `anthropic-work`"), "{err}");
    assert!(err.contains("dispatches through `anthropic`"), "{err}");
    assert!(
        collect_config_validation(&config)
            .errors
            .iter()
            .any(|e| e.contains("runtime state")),
        "the refusal must surface through the collected suite"
    );
}

fn provider_named(name: &str) -> Config {
    parse(&format!(
        "[providers.\"{name}\"]\nkind = \"anthropic-api\"\napi_key_ref = \"env://K\"\n"
    ))
}

#[test]
fn a_provider_name_at_the_byte_ceiling_passes() {
    // Positive control for the refusal below: same shape, one byte shorter.
    let config = provider_named(&"p".repeat(MAX_PROVIDER_NAME_BYTES));

    assert!(
        validate_state_key_names(&config).is_ok(),
        "{:?}",
        validate_state_key_names(&config).err()
    );
}

#[test]
fn a_provider_name_over_the_byte_ceiling_is_refused_without_echoing_it() {
    // Arrange: multi-byte characters, so the ceiling is proven to count
    // bytes rather than characters.
    let name = "\u{e9}".repeat(MAX_PROVIDER_NAME_BYTES / 2 + 1);
    assert!(name.chars().count() <= MAX_PROVIDER_NAME_BYTES);
    let config = provider_named(&name);

    // Act
    let err = errors(&config);

    // Assert
    let ceiling = format!("the maximum is {MAX_PROVIDER_NAME_BYTES}");
    assert!(err.contains(&ceiling), "{err}");
    assert!(
        !err.contains(&name),
        "the oversized name must not be echoed: {err}"
    );
    assert!(
        collect_config_validation(&config)
            .errors
            .iter()
            .any(|e| e.contains(&ceiling)),
        "the refusal must surface through the collected suite"
    );
}
