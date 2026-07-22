//! Serde round-trip and default-impl pins for `RetryPolicy` fields
//! (probe/backstop/honored-retry-after knobs) plus adjacent config
//! deny-unknown-field guards. Each test names the input shape it
//! exercises.
use super::RetryPolicy;

#[test]
fn probe_max_tokens_defaults_to_one_when_omitted() {
    // A `[retry]` block that omits `probe_max_tokens` defaults to 1
    // (Claude Code's max_tokens=1 probe is detected out of the box).
    use crate::config::Config;
    let toml_text = r"
[retry]
max_attempts = 3
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(cfg.retry.probe_max_tokens, 1);
    assert_eq!(cfg.retry.max_attempts, 3, "other fields unaffected");
}

#[test]
fn probe_max_tokens_zero_parses_to_disable() {
    // `probe_max_tokens = 0` is the disable sentinel and round-trips.
    use crate::config::Config;
    let toml_text = r"
[retry]
probe_max_tokens = 0
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(cfg.retry.probe_max_tokens, 0);
}

#[test]
fn default_retry_policy_has_probe_max_tokens_one() {
    // The Default impl (no `[retry]` block at all) also yields 1.
    assert_eq!(RetryPolicy::default().probe_max_tokens, 1);
}

/// The code default for `stream_first_byte_timeout_ms` is `Some`,
/// not `None` -- a pinging-but-contentless upstream must have a
/// bound even when the operator sets no `[retry]` block at all.
#[test]
fn default_retry_policy_has_stream_first_byte_timeout_backstop() {
    assert_eq!(
        RetryPolicy::default().stream_first_byte_timeout_ms,
        Some(600_000),
        "default must be Some(600000) as a total-silence backstop"
    );
}

/// An operator that sets `stream_first_byte_timeout_ms` explicitly
/// must get exactly that value back, unaffected by the new default.
#[test]
fn stream_first_byte_timeout_ms_explicit_override_round_trips_unchanged() {
    use crate::config::Config;
    let toml_text = r"
[retry]
stream_first_byte_timeout_ms = 120000
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(cfg.retry.stream_first_byte_timeout_ms, Some(120_000));
}

/// A `[retry]` block that sets some OTHER field but omits
/// `stream_first_byte_timeout_ms` must still get the `Some(600000)`
/// backstop, not `None`. This is the case the struct-level
/// `Default` impl does NOT cover, since that only applies when the
/// whole `[retry]` table is absent.
#[test]
fn stream_first_byte_timeout_ms_defaults_to_backstop_when_omitted() {
    use crate::config::Config;
    let toml_text = r"
[retry]
max_attempts = 5
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(
        cfg.retry.stream_first_byte_timeout_ms,
        Some(600_000),
        "omitting the key inside a present [retry] block must not lose the backstop"
    );
    assert_eq!(cfg.retry.max_attempts, 5, "other fields unaffected");
}

/// A `[retry]` block omitting `max_honored_retry_after_ms` resolves
/// to the documented 1h default via the getter.
#[test]
fn max_honored_retry_after_defaults_to_one_hour_when_omitted() {
    use crate::config::Config;
    use std::time::Duration;

    let toml_text = r"
[retry]
max_attempts = 3
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert!(
        cfg.retry.max_honored_retry_after_ms.is_none(),
        "field must default to None when omitted"
    );
    assert_eq!(
        cfg.retry.max_honored_retry_after(),
        Duration::from_hours(1),
        "None must resolve to the 1h ceiling"
    );
}

/// An explicit `max_honored_retry_after_ms` parses and the getter
/// returns the configured duration.
#[test]
fn max_honored_retry_after_uses_configured_value() {
    use crate::config::Config;
    use std::time::Duration;

    let toml_text = r"
[retry]
max_honored_retry_after_ms = 90000
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(cfg.retry.max_honored_retry_after_ms, Some(90_000));
    assert_eq!(
        cfg.retry.max_honored_retry_after(),
        Duration::from_secs(90),
        "Some(90000) must resolve to 90s"
    );
}

/// context_management = true round-trips through TOML deserialization.
#[test]
fn provider_entry_anthropic_api_context_management_round_trips_true() {
    use crate::config::{Config, ProviderEntry};
    // Arrange
    let toml_text = r#"
[providers.deepseek]
kind = "anthropic-api"
base_url = "https://api.deepseek.com/anthropic"
api_key_ref = "env://DS_KEY"
auth_kind = "oauth-bearer"
context_management = true
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get("deepseek").expect("deepseek provider");

    // Assert
    match entry {
        ProviderEntry::AnthropicApi {
            context_management, ..
        } => assert!(
            *context_management,
            "context_management = true must deserialize as true"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

/// context_management omitted from TOML defaults to false.
#[test]
fn provider_entry_anthropic_api_context_management_defaults_false() {
    use crate::config::{Config, ProviderEntry};
    // Arrange: no context_management key in TOML.
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");

    // Assert
    match entry {
        ProviderEntry::AnthropicApi {
            context_management, ..
        } => assert!(
            !context_management,
            "context_management must default to false when omitted; got {context_management}"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

/// context_management = false round-trips through TOML deserialization.
#[test]
fn provider_entry_anthropic_api_context_management_round_trips_false() {
    use crate::config::{Config, ProviderEntry};
    // Arrange
    let toml_text = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-test"
context_management = false
"#;
    // Act
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");

    // Assert
    match entry {
        ProviderEntry::AnthropicApi {
            context_management, ..
        } => assert!(
            !context_management,
            "context_management = false must deserialize as false"
        ),
        other => panic!("expected AnthropicApi entry; got {other:?}"),
    }
}

// v0.8 cap-relaxation knobs: serde round-trip pins so a default,
// an explicit override, and a typo all surface correctly.

/// Server-level `max_body_bytes` defaults to the documented value
/// when omitted from `[server]`.
#[test]
fn server_cap_knobs_default_when_omitted() {
    use crate::config::Config;
    let toml_text = r#"
[server]
host = "127.0.0.1"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(cfg.server.max_body_bytes, 32 * 1024 * 1024);
}

/// Explicit value for the `[server] max_body_bytes` knob parses
/// and round-trips through serde.
#[test]
fn server_cap_knobs_explicit_values_round_trip() {
    use crate::config::Config;
    let toml_text = r"
[server]
max_body_bytes = 67108864
";
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    assert_eq!(cfg.server.max_body_bytes, 67_108_864);
}

/// Per-model `max_output_tokens` defaults to None when omitted and
/// round-trips when set.
#[test]
fn model_entry_max_output_tokens_round_trip() {
    use crate::config::Config;
    let toml_text = r#"
[models.opus4]
provider = "anthropic"
upstream = "claude-opus-4"
max_output_tokens = 32000
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse");
    let m = cfg.models.get("opus4").expect("entry");
    assert_eq!(m.max_output_tokens, Some(32000));

    let toml_default = r#"
[models.haiku]
provider = "anthropic"
upstream = "claude-haiku-4-5"
"#;
    let cfg: Config = toml::from_str(toml_default).expect("parse");
    let m = cfg.models.get("haiku").expect("entry");
    assert!(m.max_output_tokens.is_none(), "default must be None");
}

/// A typo on the per-model `max_output_tokens` knob surfaces at
/// parse time (the per-model table opts into `deny_unknown_fields`).
#[test]
fn model_entry_rejects_typo_on_max_output_tokens() {
    use crate::config::Config;
    let toml_text = r#"
[models.x]
provider = "p"
upstream = "u"
max_output_token = 32000
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("max_output_token") || msg.contains("unknown field"),
        "expected unknown-field error; got: {msg}"
    );
}

#[test]
fn server_rejects_auths_typo_for_auth_block() {
    // `[server.auths]` (typo for `[server.auth]`) must be rejected.
    // Without deny_unknown_fields it parsed fine and left auth
    // disabled -- a silent auth-disable footgun.
    use crate::config::Config;
    let toml_text = r#"
[server]
host = "127.0.0.1"

[server.auths]
tokens = ["literal:abc"]
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("auths") || msg.contains("unknown field"),
        "expected unknown-field error naming `auths`; got: {msg}"
    );
}

#[test]
fn server_auth_rejects_token_typo_for_tokens() {
    // `token` (singular, typo for `tokens`) under `[server.auth]`
    // must be rejected so a misspelled key cannot silently leave
    // the listener unauthenticated.
    use crate::config::Config;
    let toml_text = r#"
[server.auth]
token = ["literal:abc"]
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("token") || msg.contains("unknown field"),
        "expected unknown-field error naming the unknown key; got: {msg}"
    );
}

#[test]
fn server_auth_tokens_round_trips() {
    // A valid `[server.auth]` with the correct `tokens` key still
    // deserializes after deny_unknown_fields is added.
    use crate::config::Config;
    let toml_text = r#"
[server.auth]
tokens = ["literal:abc", "env://TOK"]
"#;
    let cfg: Config = toml::from_str(toml_text).expect("valid auth block parses");
    let auth = cfg.server.auth.expect("auth present");
    assert_eq!(auth.tokens, vec!["literal:abc", "env://TOK"]);
}
