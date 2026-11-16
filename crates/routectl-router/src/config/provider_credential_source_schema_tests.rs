//! Config-BOUNDARY (parse-via-`toml`) schema tests for the forwarded seat
//! shape: `credential = "forwarded"`. Which formats, hosts and auths may
//! carry it is the provider plan's rule (`factory::plan_tests`); this module
//! pins only the serde SHAPE: the value parses as the forwarded seat, names
//! no secret, round-trips, and the retired `credential_source` key is
//! unrepresentable on every format.

use crate::config::{Auth, Config};

const FORWARDED_ENTRY: &str = r#"
[providers.anthropic-forwarded]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "forwarded"
auth = "bearer"
"#;

#[test]
fn an_entry_with_its_own_credential_is_not_the_forwarded_seat() {
    let toml_text = r#"
[providers.anthropic]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse own-credential entry");
    let entry = cfg.providers.get("anthropic").expect("anthropic provider");
    assert_eq!(entry.forwarded_base_url(), None);
    assert_eq!(entry.api_key_ref(), Some("env://ANTHROPIC_API_KEY"));
}

/// The forwarded block is four lines: no separate key reference at all.
#[test]
fn forwarded_entry_parses_as_the_forwarded_seat() {
    let cfg: Config = toml::from_str(FORWARDED_ENTRY).expect("forwarded block must parse");
    let entry = cfg
        .providers
        .get("anthropic-forwarded")
        .expect("anthropic-forwarded provider");
    assert_eq!(
        entry.forwarded_base_url(),
        Some("https://api.anthropic.com")
    );
    assert_eq!(entry.auth, Auth::Bearer);
    assert_eq!(
        entry.api_key_ref(),
        None,
        "the forwarded seat names no store reference"
    );
}

/// The forwarded seat must NOT surface as a secret URI to resolve:
/// `commands::config::check` iterates every `secret_uris()` entry through
/// `SecretRef::parse`, which would reject `forwarded` as an unrecognized
/// scheme and fail an otherwise-clean forwarded provider.
#[test]
fn forwarded_entry_reports_no_secret_uris() {
    let cfg: Config = toml::from_str(FORWARDED_ENTRY).expect("parse");
    let entry = cfg.providers.get("anthropic-forwarded").unwrap();
    assert!(
        entry.secret_uris().is_empty(),
        "got: {:?}",
        entry.secret_uris()
    );
}

#[test]
fn forwarded_credential_round_trips_through_toml() {
    let cfg: Config = toml::from_str(FORWARDED_ENTRY).expect("parse");
    let reserialized = toml::to_string(&cfg).expect("re-serialize");
    let cfg2: Config = toml::from_str(&reserialized).expect("re-parse");
    let entry = cfg2.providers.get("anthropic-forwarded").unwrap();
    assert_eq!(entry.credential, "forwarded");
    assert_eq!(
        entry.forwarded_base_url(),
        Some("https://api.anthropic.com")
    );
}

/// The retired `credential_source` key is an unknown field on every format,
/// including the one that may carry the forwarded seat. Each row's entry
/// parses without the key, so the refusal is about the key alone.
#[test]
fn credential_source_key_is_rejected_on_every_format() {
    #[cfg_attr(not(feature = "openai-responses"), allow(unused_mut))]
    let mut rows = vec![
        ("anthropic-messages", "https://api.anthropic.com"),
        ("openai-chat", "https://api.openai.com/v1"),
    ];
    #[cfg(feature = "openai-responses")]
    rows.push(("openai-responses", "https://api.openai.com/v1"));

    for (format, base_url) in rows {
        let clean = format!(
            "[providers.example]\nformat = \"{format}\"\nbase_url = \"{base_url}\"\n\
             credential = \"forwarded\"\nauth = \"bearer\"\n"
        );
        let with_key = format!("{clean}credential_source = \"forwarded\"\n");

        let parsed = toml::from_str::<Config>(&clean);
        let refused = toml::from_str::<Config>(&with_key);

        assert!(
            parsed.is_ok(),
            "{format}: clean control must parse: {parsed:?}"
        );
        let msg = refused
            .expect_err("credential_source must not parse")
            .to_string();
        assert!(
            msg.contains("unknown field `credential_source`"),
            "{format}: expected an unknown-field error naming credential_source; got: {msg}"
        );
    }
}
