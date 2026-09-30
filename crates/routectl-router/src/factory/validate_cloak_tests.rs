use super::validate_cloak_sensitive_words;
use crate::config::Config;
use crate::factory::collect_config_validation;
use routectl_providers::anthropic_api::{MAX_SENSITIVE_WORD_FOLDED_CHARS, MAX_SENSITIVE_WORDS};

/// A word no fixture otherwise contains, so an error that echoes a
/// configured word is caught by searching for it.
const SENTINEL: &str = "zqxsentinelword";

fn config_with_words(words: &[String]) -> Config {
    let list = words
        .iter()
        .map(|w| format!("{w:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let toml_text = format!(
        "[providers.lane]\nkind = \"anthropic-api\"\napi_key_ref = \"env://KEY\"\n\
         [providers.lane.cloak]\nsensitive_words = [{list}]\n"
    );
    toml::from_str(&toml_text).unwrap_or_else(|e| panic!("must parse: {e}\n{toml_text}"))
}

fn distinct_words(count: usize, len: usize) -> Vec<String> {
    (0..count)
        .map(|i| format!("{:0>len$}", format!("w{i}")))
        .collect()
}

fn suite_errors(config: &Config) -> Vec<String> {
    collect_config_validation(config).errors
}

#[test]
fn a_list_at_both_bounds_passes_the_whole_suite() {
    let cfg = config_with_words(&distinct_words(
        MAX_SENSITIVE_WORDS,
        MAX_SENSITIVE_WORD_FOLDED_CHARS,
    ));

    let result = validate_cloak_sensitive_words(&cfg);
    let suite = suite_errors(&cfg);

    assert!(result.is_ok(), "{result:?}");
    assert!(suite.is_empty(), "{suite:?}");
}

#[test]
fn one_entry_over_the_count_bound_is_refused_by_the_suite() {
    let cfg = config_with_words(&distinct_words(MAX_SENSITIVE_WORDS + 1, 2));

    let errors = suite_errors(&cfg);

    let cloak: Vec<_> = errors
        .iter()
        .filter(|e| e.contains("sensitive_words"))
        .collect();
    assert_eq!(cloak.len(), 1, "{errors:?}");
    assert!(cloak[0].contains("provider `lane`"), "{errors:?}");
    assert!(
        cloak[0].contains(&format!("at most {MAX_SENSITIVE_WORDS}")),
        "{errors:?}"
    );
}

#[test]
fn an_entry_over_the_folded_length_bound_is_refused_with_its_index() {
    let mut words = vec!["API".to_string(), "proxy".to_string()];
    words.push("a".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS + 1));
    let cfg = config_with_words(&words);

    let errors = suite_errors(&cfg);

    let cloak: Vec<_> = errors
        .iter()
        .filter(|e| e.contains("sensitive_words[2]"))
        .collect();
    assert_eq!(cloak.len(), 1, "{errors:?}");
}

#[test]
fn a_multibyte_entry_within_the_folded_bound_is_accepted() {
    // Two UTF-8 bytes per char, one folded char each: over the bound in
    // bytes, exactly at it folded.
    let cfg = config_with_words(&["\u{3A3}".repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS)]);

    assert!(validate_cloak_sensitive_words(&cfg).is_ok());
}

#[test]
fn the_refusal_never_echoes_a_configured_word() {
    let too_long = config_with_words(&[SENTINEL.repeat(MAX_SENSITIVE_WORD_FOLDED_CHARS)]);
    let too_many = config_with_words(
        &(0..=MAX_SENSITIVE_WORDS)
            .map(|i| format!("{SENTINEL}{i}"))
            .collect::<Vec<_>>(),
    );

    for cfg in [too_long, too_many] {
        let msg = validate_cloak_sensitive_words(&cfg)
            .unwrap_err()
            .to_string();
        let suite = suite_errors(&cfg).join("\n");

        assert!(msg.contains("sensitive_words"), "{msg}");
        assert!(!msg.contains(SENTINEL), "echoes a word: {msg}");
        assert!(!suite.contains(SENTINEL), "echoes a word: {suite}");
    }
}

#[test]
fn a_provider_with_no_cloak_table_is_accepted() {
    let cfg: Config =
        toml::from_str("[providers.lane]\nkind = \"anthropic-api\"\napi_key_ref = \"env://KEY\"\n")
            .expect("parses");

    assert!(validate_cloak_sensitive_words(&cfg).is_ok());
}
