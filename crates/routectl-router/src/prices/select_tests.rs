//! Provider selection: origin match with longest path prefix, loopback,
//! ambiguity, first-party fallback, exact upstream lookup, and pools.

use serde_json::{Value, json};

use super::prices_test_support::{bytes, table};
use super::select::HostMatch;
use super::*;

/// A provider row whose one model `m` costs `input` per Mtok, so each row
/// identifies which provider answered.
fn provider(id: &str, api_host: Option<&str>, input: f64) -> Value {
    let mut row = json!({ "id": id, "models": [{ "id": "m", "input": input }] });
    if let Some(host) = api_host {
        row["api_host"] = json!(host);
    }
    row
}

fn selection_table() -> PriceTable {
    parse_and_validate(&bytes(&table(json!([
        provider("opencode", Some("https://opencode.ai/zen/v1"), 1.0),
        provider("opencode-go", Some("https://opencode.ai/zen/go/v1"), 2.0),
        provider("ollama-cloud", Some("https://ollama.com/v1"), 3.0),
        provider("lmstudio", Some("http://127.0.0.1:1234/v1"), 4.0),
        provider("local-name", Some("http://localhost:8080/v1"), 12.0),
        provider("local-v6", Some("http://[::1]:9000/v1"), 13.0),
        provider("any-addr", Some("http://0.0.0.0:1234/v1"), 14.0),
        provider("sub-local", Some("http://foo.localhost/v1"), 15.0),
        provider("mapped-v4", Some("http://[::ffff:127.0.0.1]:1234/v1"), 16.0),
        provider("twin-a", Some("https://shared.example/v1"), 5.0),
        provider("twin-b", Some("https://shared.example/v1/"), 6.0),
        provider("anthropic", None, 7.0),
        provider("openai", None, 8.0),
        provider("google", None, 9.0),
        provider("amazon-bedrock", None, 10.0),
        provider(
            "first-party-shadow",
            Some("https://api.anthropic.example/v1"),
            11.0
        ),
    ]))))
    .expect("selection fixture parses")
}

fn input_for(kind: &str, base_url: Option<&str>) -> Option<f64> {
    selection_table()
        .price_for_target(kind, base_url, "m")
        .and_then(|p| p.input_per_mtok)
}

#[test]
fn origin_match_selects_by_host_and_longest_path_prefix() {
    let rows = [
        (
            "opencode-go by longest prefix",
            "https://opencode.ai/zen/go/v1",
            Some(2.0),
        ),
        (
            "opencode-go deeper path",
            "https://opencode.ai/zen/go/v1/chat",
            Some(2.0),
        ),
        (
            "opencode by its own path",
            "https://opencode.ai/zen/v1",
            Some(1.0),
        ),
        ("ollama-cloud by host", "https://ollama.com/v1", Some(3.0)),
        (
            "ollama-cloud host case and port",
            "https://OLLAMA.com:443",
            Some(3.0),
        ),
        (
            "ollama-cloud trailing root dot",
            "https://ollama.com./v1",
            Some(3.0),
        ),
        (
            "other port is another origin",
            "https://ollama.com:8443/v1",
            None,
        ),
        (
            "other scheme is another origin",
            "http://ollama.com/v1",
            None,
        ),
        ("unknown host", "https://elsewhere.example/v1", None),
    ];
    for (name, base, expected) in rows {
        assert_eq!(input_for("openai-compat", Some(base)), expected, "{name}");
    }
}

#[test]
fn a_loopback_base_url_never_matches_a_table_host() {
    let t = selection_table();
    let rows = [
        ("ipv4 loopback", "http://127.0.0.1:1234/v1"),
        ("localhost", "http://localhost:8080/v1"),
        ("ipv6 loopback", "http://[::1]:9000/v1"),
        ("unspecified ipv4", "http://0.0.0.0:1234/v1"),
        ("localhost subdomain", "http://foo.localhost/v1"),
        ("ipv4-mapped loopback", "http://[::ffff:127.0.0.1]:1234/v1"),
    ];
    for (name, base) in rows {
        let parsed = format::ApiBase::parse(base).expect("base parses");
        assert!(
            t.match_host(&parsed).is_one(),
            "{name}: positive control, the origin alone would match a table row"
        );
        assert_eq!(input_for("openai-compat", Some(base)), None, "{name}");
        assert_eq!(
            input_for("anthropic-api", Some(base)),
            Some(7.0),
            "{name}: falls through to the first-party fallback"
        );
    }
}

#[test]
fn two_providers_on_one_host_and_path_are_unknown() {
    assert_eq!(
        input_for("openai-compat", Some("https://shared.example/v1/chat")),
        None
    );
    assert_eq!(
        input_for("anthropic-api", Some("https://shared.example/v1")),
        None,
        "an ambiguous origin does not fall back"
    );
}

#[test]
fn first_party_fallback_covers_the_four_fixed_vendor_kinds() {
    let rows = [
        ("anthropic-api", Some(7.0)),
        ("openai-responses", Some(8.0)),
        ("gemini", Some(9.0)),
        ("bedrock", Some(10.0)),
        ("openai-compat", None),
    ];
    for (kind, expected) in rows {
        assert_eq!(input_for(kind, None), expected, "{kind} with no base url");
        assert_eq!(
            input_for(kind, Some("https://unlisted.example/v1")),
            expected,
            "{kind} with an unmatched base url"
        );
    }
}

#[test]
fn an_origin_match_wins_over_the_first_party_fallback() {
    assert_eq!(
        input_for(
            "anthropic-api",
            Some("https://api.anthropic.example/v1/messages")
        ),
        Some(11.0)
    );
}

#[test]
fn the_upstream_id_is_looked_up_exactly() {
    let t = selection_table();
    let rows = [
        ("m", true),
        ("M", false),
        ("m ", false),
        ("m-2", false),
        ("", false),
    ];
    for (upstream, found) in rows {
        assert_eq!(
            t.price_for_target("gemini", None, upstream).is_some(),
            found,
            "{upstream:?}"
        );
    }
}

const POOL_CONFIG: &str = r#"
[providers.go-seat-1]
kind = "openai-compat"
api_key_ref = "env://A"
base_url = "https://opencode.ai/zen/go/v1"

[providers.go-seat-2]
kind = "openai-compat"
api_key_ref = "env://B"
base_url = "https://opencode.ai/zen/go/v1"

[providers.direct]
kind = "openai-compat"
api_key_ref = "env://C"
base_url = "https://ollama.com/v1"

[providers.claude]
kind = "anthropic-api"
api_key_ref = "env://D"
base_url = "https://api.anthropic.com"

[pools.go]
members = ["go-seat-1", "go-seat-2"]

[pools.dangling]
members = ["absent"]
"#;

#[test]
fn config_targets_select_through_their_egress_base_url_and_pools_through_a_member() {
    let config: crate::config::Config = toml::from_str(POOL_CONFIG).expect("config parses");
    let t = selection_table();
    let rows = [
        ("pool selects through a member seat", "go", Some(2.0)),
        ("direct provider entry", "direct", Some(3.0)),
        (
            "first-party kind with its default host",
            "claude",
            Some(7.0),
        ),
        ("pool with no resolvable member", "dangling", None),
        ("unknown target", "absent", None),
    ];
    for (name, target, expected) in rows {
        assert_eq!(
            t.price_for(&config, target, "m")
                .and_then(|p| p.input_per_mtok),
            expected,
            "{name}"
        );
    }
}

impl HostMatch<'_> {
    const fn is_one(&self) -> bool {
        matches!(self, Self::One(_))
    }
}
