//! The endpoint-key derivation and the request walk's exclusion admission.

use routectl_core::ScopeRow;

use super::*;
use crate::config::{Config, ProviderEntry};

fn entries(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("fixture config parses")
}

fn key_of(cfg: &Config, name: &str) -> String {
    endpoint_key(name, cfg.providers.get(name))
        .as_str()
        .to_owned()
}

const DIRECT_PAIR: &str = r#"
[providers.lower]
kind = "anthropic-api"
api_key_ref = "env://A"
base_url = "https://api.anthropic.com"

[providers.upper]
kind = "anthropic-api"
api_key_ref = "env://A"
base_url = "https://API.Anthropic.COM/v1/"

[providers.explicit-port]
kind = "anthropic-api"
api_key_ref = "env://A"
base_url = "https://api.anthropic.com:443"

[providers.other-port]
kind = "anthropic-api"
api_key_ref = "env://A"
base_url = "https://api.anthropic.com:8443"

[providers.plain-http]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "http://LOCALHOST/v1"

[providers.plain-http-80]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "http://localhost:80/v1"

[providers.local-11434]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "http://localhost:11434/v1"

[providers.forwarded]
kind = "anthropic-api"
base_url = "https://Gateway.Example.com"
credential_source = "forwarded"

[providers.unparseable]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "not a url"
"#;

/// Each row names a pair of entries and whether they share an endpoint key.
/// The equal rows prove normalization folds what it must; the distinct rows
/// prove it does not fold what it must not (a key that ignored the host or
/// port would pass every equal row).
#[test]
fn endpoint_keys_fold_spelling_and_keep_origins_apart() {
    let cfg = entries(DIRECT_PAIR);
    let rows = [
        ("host case and path folded", "lower", "upper", true),
        (
            "explicit :443 equals implicit",
            "lower",
            "explicit-port",
            true,
        ),
        (
            "explicit :80 equals implicit http",
            "plain-http",
            "plain-http-80",
            true,
        ),
        ("a different port differs", "lower", "other-port", false),
        (
            "a local port differs from the default",
            "plain-http",
            "local-11434",
            false,
        ),
        ("a different host differs", "lower", "forwarded", false),
    ];
    for (name, a, b, same) in rows {
        assert_eq!(key_of(&cfg, a) == key_of(&cfg, b), same, "{name}");
    }
}

#[test]
fn endpoint_key_is_the_normalized_origin() {
    let cfg = entries(DIRECT_PAIR);
    let rows = [
        ("upper", "https://api.anthropic.com:443"),
        ("plain-http", "http://localhost:80"),
        ("forwarded", "https://gateway.example.com:443"),
    ];
    for (name, expected) in rows {
        assert_eq!(key_of(&cfg, name), expected, "{name}");
    }
}

/// A forwarded entry's key is its own forwarded base, never the Anthropic
/// default every other anthropic-api entry may share.
#[test]
fn a_forwarded_target_keys_on_its_forwarded_base() {
    let cfg = entries(DIRECT_PAIR);
    let entry = cfg.providers.get("forwarded").expect("forwarded entry");
    assert!(entry.forwarded_base_url().is_some(), "premise: forwarded");

    assert_eq!(key_of(&cfg, "forwarded"), "https://gateway.example.com:443");
}

#[test]
fn an_entry_with_no_classifiable_url_keys_on_its_provider_name() {
    let cfg = entries(DIRECT_PAIR);
    let rows = [
        ("unparseable url", "unparseable", Some(())),
        ("unknown provider name", "absent", None),
    ];
    for (name, provider, present) in rows {
        assert_eq!(cfg.providers.get(provider).map(|_| ()), present, "{name}");
        assert_eq!(
            key_of(&cfg, provider),
            format!("provider:{provider}"),
            "{name}"
        );
    }
}

#[cfg(feature = "bedrock")]
fn mantle_entry(region: &str) -> ProviderEntry {
    toml::from_str::<Config>(&format!(
        "[providers.m]\nkind = \"anthropic-api\"\napi_key_ref = \"\"\n\
         bedrock_mantle = {{ region = \"{region}\", creds = {{ kind = \"bearer-key\", \
         key_ref = \"env://AWS_BEARER_TOKEN_BEDROCK\" }} }}\n"
    ))
    .expect("parse mantle lane")
    .providers
    .remove("m")
    .expect("mantle provider")
}

/// Two Mantle regions dial two hosts, so they are two endpoints, and
/// neither is the Anthropic default the entry's `base_url` field still holds.
#[cfg(feature = "bedrock")]
#[test]
fn distinct_mantle_regions_are_distinct_endpoints() {
    let west = endpoint_key("m", Some(&mantle_entry("us-west-2")));
    let east = endpoint_key("m", Some(&mantle_entry("us-east-1")));
    let direct = endpoint_key("lower", entries(DIRECT_PAIR).providers.get("lower"));

    assert_eq!(
        west.as_str(),
        "https://bedrock-mantle.us-west-2.api.aws:443"
    );
    assert_ne!(west, east);
    assert_ne!(west, direct);
}

const POOL: &str = r#"
[providers.seat-a]
kind = "anthropic-api"
api_key_ref = "env://A"

[providers.seat-b]
kind = "anthropic-api"
api_key_ref = "env://B"

[providers.gateway]
kind = "openai-compat"
api_key_ref = "env://C"
base_url = "https://gateway.example.com/v1"
"#;

/// Two seats on the default base share one key, so an endpoint exclusion
/// set by one seat covers its sibling; a target on another origin is kept.
#[test]
fn pool_seats_share_an_endpoint_key() {
    let cfg = entries(POOL);

    assert_eq!(key_of(&cfg, "seat-a"), key_of(&cfg, "seat-b"));
    assert_ne!(key_of(&cfg, "seat-a"), key_of(&cfg, "gateway"));
}

fn target<'a>(
    cfg: &'a Config,
    provider: &'a str,
    upstream: &'a str,
    pool: &'a str,
) -> WalkTarget<'a> {
    WalkTarget {
        provider_name: provider,
        entry: cfg.providers.get(provider),
        upstream,
        pool,
    }
}

#[test]
fn a_fresh_walk_admits_every_target() {
    let cfg = entries(POOL);
    let mut walk = RequestWalk::new();

    for name in ["seat-a", "seat-b", "gateway"] {
        assert!(walk.admits(&target(&cfg, name, "m", "pool")), "{name}");
    }
    assert_eq!(walk.excluded_by(), ExcludedBy::default());
}

/// Each exclusion kind skips exactly the targets in its scope and is
/// tallied under that scope; the targets outside it stay admitted.
#[test]
fn each_exclusion_skips_only_its_scope() {
    let cfg = entries(POOL);
    let seat_a = target(&cfg, "seat-a", "opus", "pool-a");
    let seat_b_same_model = target(&cfg, "seat-b", "opus", "pool-a");
    let seat_b_other_model = target(&cfg, "seat-b", "sonnet", "pool-b");
    let gateway = target(&cfg, "gateway", "opus", "pool-c");
    let rows = [
        (
            "endpoint",
            Exclusion::Endpoint(seat_a.endpoint_key()),
            [false, false, true],
            ExcludedBy {
                endpoint: 2,
                ..ExcludedBy::default()
            },
        ),
        (
            "model",
            Exclusion::Model {
                endpoint: seat_a.endpoint_key(),
                upstream: "opus".into(),
            },
            [false, true, true],
            ExcludedBy {
                model: 1,
                ..ExcludedBy::default()
            },
        ),
        (
            "outside pool",
            Exclusion::OutsidePool("pool-a".into()),
            [true, false, false],
            ExcludedBy {
                probe_pool: 2,
                ..ExcludedBy::default()
            },
        ),
    ];
    for (name, exclusion, admitted, tally) in rows {
        let mut walk = RequestWalk::new();
        walk.exclude(exclusion);

        let got = [
            walk.admits(&seat_b_same_model),
            walk.admits(&seat_b_other_model),
            walk.admits(&gateway),
        ];

        assert_eq!(got, admitted, "{name}");
        assert_eq!(walk.excluded_by(), tally, "{name}");
    }
}

#[test]
fn the_same_exclusion_is_held_once() {
    let cfg = entries(POOL);
    let key = target(&cfg, "seat-a", "m", "p").endpoint_key();
    let mut walk = RequestWalk::new();

    walk.exclude(Exclusion::Endpoint(key.clone()));
    walk.exclude(Exclusion::Endpoint(key));

    assert_eq!(walk.exclusions().len(), 1);
}

#[test]
fn retries_and_recognition_accumulate_across_the_walk() {
    let mut walk = RequestWalk::new();

    walk.spend_retry();
    walk.spend_retry();
    walk.recognize(ScopeRow::AnthropicBareRateLimit);

    assert_eq!(walk.retries_used(), 2);
    assert_eq!(walk.recognized(), Some(ScopeRow::AnthropicBareRateLimit));
}
