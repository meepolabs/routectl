//! `[mitm]` schema round-trip: absence leaves the feature off,
//! presence fills in every documented default, explicit values
//! survive serde untouched, and an unknown key -- including the
//! removed `credential_source` -- rejects at parse time (same
//! `deny_unknown_fields` footgun-closing convention as
//! `[server.auth]`). The actionable-error path for the legacy key
//! specifically is pinned in
//! `legacy_mitm_credential_source_preflight_tests` above.

use crate::config::{Config, MitmConfig};

#[test]
fn mitm_absent_leaves_config_none() {
    let cfg: Config = toml::from_str("").expect("parse empty config");
    assert!(cfg.mitm.is_none(), "mitm must default to None when absent");
}

#[test]
fn mitm_present_with_all_fields_omitted_uses_defaults() {
    let toml_text = "[mitm]\n";
    let cfg: Config = toml::from_str(toml_text).expect("parse bare [mitm] block");
    let mitm = cfg.mitm.expect("mitm present once the block is declared");
    assert_eq!(mitm.upstream_origin, "https://api.anthropic.com");
    assert_eq!(mitm.listen_port, 8443);
    assert_eq!(mitm.mitm_host, "api.anthropic.com");
    assert!(mitm.tested_cc_version.is_none());
    assert!(
        mitm.cert_dir.ends_with("routectl/mitm-certs"),
        "cert_dir: {:?}",
        mitm.cert_dir
    );
    assert_eq!(mitm, MitmConfig::default());
}

#[test]
fn mitm_explicit_values_round_trip() {
    let toml_text = r#"
[mitm]
upstream_origin = "https://api.example.com"
listen_port = 9443
cert_dir = "/tmp/routectl-mitm-certs"
mitm_host = "api.example.com"
tested_cc_version = "2.1.143"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("parse explicit [mitm]");
    let mitm = cfg.mitm.expect("mitm present");
    assert_eq!(mitm.upstream_origin, "https://api.example.com");
    assert_eq!(mitm.listen_port, 9443);
    assert_eq!(
        mitm.cert_dir,
        std::path::PathBuf::from("/tmp/routectl-mitm-certs")
    );
    assert_eq!(mitm.mitm_host, "api.example.com");
    assert_eq!(mitm.tested_cc_version, Some("2.1.143".to_string()));
}

#[test]
fn mitm_rejects_unknown_field() {
    let toml_text = r#"
[mitm]
upstream_origin = "https://api.anthropic.com"
listen_prot = 9443
"#;
    let err = toml::from_str::<Config>(toml_text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("listen_prot") || msg.contains("unknown field"),
        "expected unknown-field error naming the typo; got: {msg}"
    );
}

/// The removed `credential_source` key is exactly as unrepresentable
/// on `[mitm]` as any other unknown field -- `deny_unknown_fields`
/// rejects the typed deserialize regardless of the value. The
/// actionable replacement text lives in the preflight check
/// (`legacy_mitm_credential_source_preflight_tests`), not here.
#[test]
fn mitm_rejects_legacy_credential_source_field() {
    let toml_text = "[mitm]\ncredential_source = \"forwarded\"\n";
    let err = toml::from_str::<Config>(toml_text)
        .expect_err("the removed credential_source key must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("credential_source") || msg.contains("unknown field"),
        "expected unknown-field error naming credential_source; got: {msg}"
    );
}

/// Acceptance: a transport-only `[mitm]` block -- the original shape, no
/// credential knob -- still validates cleanly via the full config
/// boundary (not a bare-struct construction).
#[test]
fn mitm_transport_only_block_still_validates() {
    let toml_text = r#"
[mitm]
upstream_origin = "https://api.anthropic.com"
listen_port = 8443
mitm_host = "api.anthropic.com"
"#;
    let cfg: Config = toml::from_str(toml_text).expect("transport-only [mitm] must parse");
    assert!(cfg.mitm.is_some());
}
