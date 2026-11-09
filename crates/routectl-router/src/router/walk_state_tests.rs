//! The endpoint-key derivation and the request walk's exclusion admission.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, ScopeRow};
use serde_json::{Value, json};

use super::*;
use crate::config::Config;
#[cfg(feature = "bedrock")]
use crate::config::ProviderEntry;
use crate::resolved::ResolvedModel;
use crate::router::chain::into_one_dispatch_target;

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

[providers.trailing-dot]
kind = "anthropic-api"
api_key_ref = "env://A"
base_url = "https://api.anthropic.com./v1"

[providers.v6]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "http://[::1]:8080/v1"

[providers.v6-spelled]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "http://[0:0:0:0:0:0:0:1]:8080/v1"

[providers.userinfo]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "https://user:sk-live-FAKE@gateway.example.com/v1"

[providers.demoted-host]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "https://sk-live-FAKE:8080/x@gateway.example.com/v1"

[providers.double-at]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "https://x@sk-live-FAKE@gateway.example.com/v1"
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
        ("a trailing dot folded", "lower", "trailing-dot", true),
        ("IPv6 spellings folded", "v6", "v6-spelled", true),
        (
            "IPv6 loopback differs from localhost",
            "v6",
            "local-11434",
            false,
        ),
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
        ("trailing-dot", "https://api.anthropic.com:443"),
        ("v6", "http://[::1]:8080"),
        ("userinfo", "https://gateway.example.com:443"),
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

/// No credential byte written into a base URL reaches the key: userinfo is
/// dropped, and an authority the parser could read with a secret in the host
/// slot (a demoted host, a second `@`) falls back to the provider name. The
/// key's Debug and Display print the same origin-only string.
#[test]
fn credential_bytes_in_a_base_url_never_reach_the_key() {
    let cfg = entries(DIRECT_PAIR);
    let rows = [
        ("userinfo", "https://gateway.example.com:443"),
        ("demoted-host", "provider:demoted-host"),
        ("double-at", "provider:double-at"),
    ];
    for (name, expected) in rows {
        let key = endpoint_key(name, cfg.providers.get(name));

        assert_eq!(key.as_str(), expected, "{name}");
        for printed in [format!("{key}"), format!("{key:?}")] {
            assert_eq!(printed, expected, "{name}");
            assert!(!printed.contains("sk-live"), "{name}: {printed}");
        }
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

#[cfg(feature = "gemini")]
const GEMINI_SEATS: &str = r#"
[providers.cc-a]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"

[providers.cc-b]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"

[providers.cc-prod]
kind = "gemini"
api_key_ref = "oauth://antigravity"
auth_mode = "cloud-code"
base_url = "https://cloudcode-pa.googleapis.com"

[providers.key-a]
kind = "gemini"
api_key_ref = "env://G1"

[providers.key-b]
kind = "gemini"
api_key_ref = "env://G2"
"#;

/// Seats on one effective base share one key, so an endpoint exclusion set
/// by one seat covers its sibling; a target on another origin is kept.
#[test]
fn pool_seats_share_an_endpoint_key() {
    #[cfg(feature = "gemini")]
    let cfg = entries(&format!("{POOL}{GEMINI_SEATS}"));
    #[cfg(not(feature = "gemini"))]
    let cfg = entries(POOL);
    #[cfg_attr(not(feature = "gemini"), allow(unused_mut))]
    let mut rows = vec![
        ("anthropic default base", "seat-a", "seat-b", true),
        ("another origin", "seat-a", "gateway", false),
    ];
    #[cfg(feature = "gemini")]
    rows.extend([
        ("cloud-code seats on the default", "cc-a", "cc-b", true),
        ("api-key seats on the default", "key-a", "key-b", true),
        (
            "cloud-code default is not the api-key default",
            "cc-a",
            "key-a",
            false,
        ),
        (
            "a pinned cloud-code host differs from the default",
            "cc-a",
            "cc-prod",
            false,
        ),
    ]);
    for (name, a, b, same) in rows {
        assert_eq!(key_of(&cfg, a) == key_of(&cfg, b), same, "{name}");
        assert!(!key_of(&cfg, a).starts_with("provider:"), "{name}: {a}");
    }
}

/// A cloud-code seat with no pin dials the daily Cloud Code host, the same
/// host the effective-config view reports for it.
#[cfg(feature = "gemini")]
#[test]
fn a_cloud_code_seat_keys_on_its_effective_host() {
    let cfg = entries(GEMINI_SEATS);

    assert_eq!(
        key_of(&cfg, "cc-a"),
        "https://daily-cloudcode-pa.googleapis.com:443"
    );
    assert_eq!(
        key_of(&cfg, "key-a"),
        "https://generativelanguage.googleapis.com:443"
    );
}

fn target<'a>(cfg: &'a Config, provider: &'a str, model: &'a str, run: usize) -> WalkTarget<'a> {
    WalkTarget::fixture(provider, cfg.providers.get(provider), model, run)
}

#[test]
fn a_fresh_walk_admits_every_target() {
    let cfg = entries(POOL);
    let mut walk = RequestWalk::new();

    for name in ["seat-a", "seat-b", "gateway"] {
        assert!(walk.admits(&target(&cfg, name, "m", 0)), "{name}");
    }
    assert_eq!(walk.excluded_by(), ExcludedBy::default());
}

/// Each exclusion kind skips exactly the targets in its scope and is
/// tallied under that scope; the targets outside it stay admitted.
#[test]
fn each_exclusion_skips_only_its_scope() {
    let cfg = entries(POOL);
    let seat_a = target(&cfg, "seat-a", "opus", 0);
    let seat_b_same_model = target(&cfg, "seat-b", "opus", 0);
    let seat_b_other_model = target(&cfg, "seat-b", "sonnet", 2);
    let gateway = target(&cfg, "gateway", "opus-gw", 3);
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
            Exclusion::Model("opus".into()),
            [false, true, true],
            ExcludedBy {
                model: 1,
                ..ExcludedBy::default()
            },
        ),
        (
            "outside run",
            Exclusion::OutsideRun(seat_a.run()),
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
    let key = target(&cfg, "seat-a", "m", 0).endpoint_key();
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

/// A provider that only lets a dispatch target be expanded; nothing here
/// dispatches through it.
struct Undialed;

#[async_trait]
impl Provider for Undialed {
    fn id(&self) -> &'static str {
        "undialed"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("undialed", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("walk targets are never dialed")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("walk targets are never dialed")
    }
}

fn resolved(nickname: &str, provider: &str) -> Arc<ResolvedModel> {
    Arc::new(ResolvedModel::new(
        nickname,
        provider,
        Arc::new(Undialed),
        "claude-opus",
    ))
}

/// The walk view of a chain target reads the provider entry, the `[models]`
/// entry and the chain run off the same target, so the three cannot be
/// paired across targets.
#[test]
fn a_walk_target_reads_its_fields_off_one_chain_target() {
    let cfg = entries(POOL);
    let chain = [
        into_one_dispatch_target(resolved("opus", "seat-a")),
        into_one_dispatch_target(resolved("sonnet", "gateway")),
    ];

    let first = WalkTarget::at(&chain, 0, &cfg).expect("in range");
    let second = WalkTarget::at(&chain, 1, &cfg).expect("in range");

    assert_eq!((first.model(), first.run()), ("opus", ChainRun(0)));
    assert_eq!(
        first.endpoint_key(),
        endpoint_key("seat-a", cfg.providers.get("seat-a"))
    );
    assert_eq!((second.model(), second.run()), ("sonnet", ChainRun(1)));
    assert_eq!(
        second.endpoint_key(),
        endpoint_key("gateway", cfg.providers.get("gateway"))
    );
    assert!(WalkTarget::at(&chain, 2, &cfg).is_none());
}

/// A probe's fence is the contiguous run of its `[models]` entry: in a
/// chain `[opus, opus, sonnet, opus]` a probe that failed on the first opus
/// target may reach the second, never sonnet, and never the later opus
/// run, though it names the same entry.
#[test]
fn a_probe_fence_covers_only_the_contiguous_run_of_its_entry() {
    let cfg = entries(POOL);
    let opus = resolved("opus", "seat-a");
    let chain = [
        into_one_dispatch_target(Arc::clone(&opus)),
        into_one_dispatch_target(Arc::clone(&opus)),
        into_one_dispatch_target(resolved("sonnet", "gateway")),
        into_one_dispatch_target(Arc::clone(&opus)),
    ];
    let failed = WalkTarget::at(&chain, 0, &cfg).expect("in range");
    let mut walk = RequestWalk::new();
    walk.exclude(Exclusion::OutsideRun(failed.run()));

    let admitted: Vec<bool> = (1..chain.len())
        .map(|i| walk.admits(&WalkTarget::at(&chain, i, &cfg).expect("in range")))
        .collect();

    assert_eq!(admitted, [true, false, false]);
    assert_eq!(
        WalkTarget::at(&chain, 3, &cfg).map(|t| t.model()),
        Some("opus"),
        "premise: the later run names the same entry"
    );
}
