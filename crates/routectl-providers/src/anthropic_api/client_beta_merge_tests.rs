//! The `anthropic-beta` header merges the client flags, then the provider
//! `header_extras["anthropic-beta"]` list, then the operator pins: each entry
//! is trimmed, an empty one is skipped, and a flag ships once at the position
//! of its first source. A client flag the config also names therefore keeps
//! its client position.
//!
//! Every row composes through `build_headers` on the plain API-key lane (no
//! minted floor, no withheld set) and asserts the whole header string.

use super::*;
use routectl_core::{ChatRequest, StaticToken};

const PLAIN_HOST: &str = "http://127.0.0.1:18080";
const A: &str = "a-2099-01-01";
const B: &str = "b-2099-01-01";
const C: &str = "c-2099-01-01";
const D: &str = "d-2099-01-01";

fn provider(config_betas: Option<&str>) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "beta-merge-test".into(),
        auth: Arc::new(StaticToken::new("test-token")),
        base_url: PLAIN_HOST.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: config_betas
            .map(|v| vec![("anthropic-beta".to_string(), v.to_string())])
            .unwrap_or_default(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn owned(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

fn header(config_betas: Option<&str>, client: &[&str], operator: &[&str]) -> Option<String> {
    let provider = provider(config_betas);
    let mut req = ChatRequest {
        anthropic_beta: owned(client),
        ..Default::default()
    };
    req.routectl_internal.operator_betas = owned(operator);
    let rb = reqwest::Client::new().post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, &req, "test-token", None);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .get("anthropic-beta")
        .map(|v| v.to_str().expect("ascii header").to_string())
}

#[test]
fn header_merge_trims_skips_empties_and_keeps_first_source_position() {
    struct Row {
        name: &'static str,
        config: Option<&'static str>,
        client: &'static [&'static str],
        operator: &'static [&'static str],
        want: Option<String>,
    }
    let rows = [
        Row {
            name: "client entries are trimmed",
            config: None,
            client: &[" a-2099-01-01 ", "\tb-2099-01-01"],
            operator: &[],
            want: Some(format!("{A},{B}")),
        },
        Row {
            name: "empty and blank client entries are skipped",
            config: None,
            client: &["", A, "   ", B],
            operator: &[],
            want: Some(format!("{A},{B}")),
        },
        Row {
            name: "config list is split on commas, trimmed, blanks skipped",
            config: Some(" a-2099-01-01 , ,b-2099-01-01 ,"),
            client: &[],
            operator: &[],
            want: Some(format!("{A},{B}")),
        },
        Row {
            name: "operator pins are trimmed and blanks skipped",
            config: None,
            client: &[],
            operator: &[" a-2099-01-01", "", " "],
            want: Some(A.to_string()),
        },
        Row {
            name: "client repeat ships once, padded or not",
            config: None,
            client: &[A, B, " a-2099-01-01 "],
            operator: &[],
            want: Some(format!("{A},{B}")),
        },
        Row {
            name: "a flag in client and config keeps its client position",
            config: Some("c-2099-01-01,a-2099-01-01"),
            client: &[B, A],
            operator: &[],
            want: Some(format!("{B},{A},{C}")),
        },
        Row {
            name: "a flag in client and operator keeps its client position",
            config: None,
            client: &[B, A],
            operator: &[D, A],
            want: Some(format!("{B},{A},{D}")),
        },
        Row {
            name: "config before operator, duplicate operator pin dropped",
            config: Some("c-2099-01-01"),
            client: &[A],
            operator: &[C, D, A],
            want: Some(format!("{A},{C},{D}")),
        },
        Row {
            name: "only blanks everywhere sends no header",
            config: Some(" , "),
            client: &["", " "],
            operator: &[""],
            want: None,
        },
    ];

    for row in rows {
        assert_eq!(
            header(row.config, row.client, row.operator),
            row.want,
            "{}",
            row.name
        );
    }
}
