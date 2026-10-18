//! One request per reachable refusal reason, through the real ingress, router
//! and egress against a mock upstream: the client gets a 400, the upstream is
//! never called, and `rc_request_refused_local_total` holds exactly one count,
//! under the refusal's lane and reason.
//!
//! Each row builds its own router, so the full counter set after the request
//! is the delta that request caused.
//!
//! `translation-accounting` has no row: it is a self-check of the
//! anthropic-api translation that a well-formed translation never trips, so
//! no request reaches it; its unit test pins it. `schema-depth` has no row:
//! the schema repair walk is bounded deeper than the JSON parse limit every
//! ingress applies, so a request nested deep enough is refused as
//! `unparseable` first; the Converse output-schema test pins it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::{Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use routectl_core::StaticToken;
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_router::config::CredentialSource;
use routectl_router::{
    AliasValue, Config, ModelEntry, ProviderEntry, ResolvedModel, RetryPolicy, Router,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::handlers::ingress_handle::ingress_handle;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;
use crate::server::AppState;

const ALIAS: &str = "a";
const NICKNAME: &str = "m";
const PROVIDER: &str = "p";
const UPSTREAM: &str = "upstream";

#[derive(Clone, Copy, Debug)]
enum Ingress {
    Anthropic,
    Chat,
    Responses,
}

/// The one target a row's alias names.
#[derive(Clone, Copy)]
enum Target {
    /// A provider entry built through the production factory, aimed at the
    /// mock upstream; `strict` sets `[server] strict_translation`.
    Built {
        entry: fn(&str) -> ProviderEntry,
        strict: bool,
    },
    /// An own-OAuth anthropic-api seat on api.anthropic.com, the lane the
    /// identity cloak runs on. Installed directly: an OAuth secret ref
    /// needs a stored login the factory would resolve.
    CloakSeat,
}

/// One refusal reason and the request that reaches it.
struct Row {
    reason: &'static str,
    ingress: Ingress,
    target: Target,
    body: Vec<u8>,
    lane: &'static str,
}

const fn built(entry: fn(&str) -> ProviderEntry) -> Target {
    Target::Built {
        entry,
        strict: false,
    }
}

fn anthropic_api(base: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")).with_base_url(base)
}

fn forwarded_anthropic_api(_base: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api("")
        .with_base_url("https://api.anthropic.com")
        .with_credential_source(CredentialSource::Forwarded)
}

fn openai_compat(base: &str) -> ProviderEntry {
    ProviderEntry::openai_compat(base, crate::test_secret::file_ref("k"))
}

fn openai_responses(base: &str) -> ProviderEntry {
    ProviderEntry::openai_responses(crate::test_secret::file_ref("k"))
        .with_openai_responses_auth_kind(routectl_providers::openai_responses::AuthKind::ApiKey)
        .with_openai_responses_base_url(base)
}

fn gemini(base: &str) -> ProviderEntry {
    let mut entry = ProviderEntry::gemini(crate::test_secret::file_ref("k"));
    if let ProviderEntry::Gemini { base_url, .. } = &mut entry {
        *base_url = base.to_string();
    }
    entry
}

fn json_body(body: &Value) -> Vec<u8> {
    serde_json::to_vec(body).expect("row body serializes")
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// A text block carrying a cache breakpoint with `ttl`.
fn cached_text(ttl: &str) -> Value {
    json!({"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": ttl}})
}

/// A chat transcript whose tool result answers no call.
fn mispaired_chat_messages() -> Value {
    json!([user("q"), {"role": "tool", "tool_call_id": "call_missing", "content": "r"}])
}

/// An `anyOf` with `n` empty branches: `n + 1` schema nodes.
fn wide_schema(n: usize) -> Value {
    json!({"anyOf": vec![json!({}); n]})
}

/// An object schema with `n` properties.
fn many_property_schema(n: usize) -> Value {
    let properties: serde_json::Map<String, Value> = (0..n)
        .map(|i| (format!("p{i}"), json!({"type": "string"})))
        .collect();
    json!({"type": "object", "properties": properties})
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            reason: "unparseable",
            ingress: Ingress::Anthropic,
            target: built(anthropic_api),
            body: b"{ not valid json".to_vec(),
            lane: "anthropic->local",
        },
        Row {
            reason: "field-type",
            ingress: Ingress::Chat,
            target: built(openai_compat),
            body: json_body(&json!({
                "model": ALIAS, "systemPrompt": {"text": "s"}, "messages": [user("q")]
            })),
            lane: "chat->local",
        },
        Row {
            reason: "previous-response-id",
            ingress: Ingress::Responses,
            target: built(openai_responses),
            body: json_body(&json!({
                "model": ALIAS, "input": "q", "previous_response_id": "resp_1"
            })),
            lane: "responses->local",
        },
        Row {
            reason: "beta-header-cap",
            ingress: Ingress::Anthropic,
            target: built(anthropic_api),
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16, "messages": [user("q")],
                "anthropic_beta": (0..65).map(|i| format!("beta-{i}")).collect::<Vec<_>>()
            })),
            lane: "anthropic->local",
        },
        Row {
            reason: "cache-breakpoint-cap",
            ingress: Ingress::Anthropic,
            target: built(anthropic_api),
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [{"role": "user", "content": vec![cached_text("5m"); 5]}]
            })),
            lane: "anthropic->local",
        },
        Row {
            reason: "cache-ttl-order",
            ingress: Ingress::Anthropic,
            target: built(anthropic_api),
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [{"role": "user",
                              "content": [cached_text("5m"), cached_text("1h")]}]
            })),
            lane: "anthropic->local",
        },
        Row {
            reason: "tool-call-id-missing",
            ingress: Ingress::Chat,
            target: built(anthropic_api),
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [user("q"), {"role": "tool", "tool_call_id": "", "content": "r"}]
            })),
            lane: "chat->anthropic-api",
        },
        Row {
            reason: "schema-size",
            ingress: Ingress::Anthropic,
            target: built(anthropic_api),
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16, "messages": [user("q")],
                "output_config": {"format": {"type": "json_schema",
                                             "schema": wide_schema(100_000)}}
            })),
            lane: "anthropic->anthropic-api",
        },
        Row {
            reason: "tool-pairing",
            ingress: Ingress::Chat,
            target: built(gemini),
            body: json_body(&json!({"model": ALIAS, "messages": mispaired_chat_messages()})),
            lane: "chat->gemini",
        },
        Row {
            reason: "unrepresentable",
            ingress: Ingress::Chat,
            target: built(openai_responses),
            body: json_body(&json!({
                "model": ALIAS,
                "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": "https://example.com/a.png"}
                ]}]
            })),
            lane: "chat->openai-responses",
        },
        Row {
            reason: "strict-translation",
            ingress: Ingress::Anthropic,
            target: Target::Built {
                entry: openai_compat,
                strict: true,
            },
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16,
                "messages": [{"role": "user", "content": [
                    {"type": "document", "source": {"type": "text", "media_type": "text/plain",
                                                    "data": "d"}}
                ]}]
            })),
            lane: "anthropic->openai-compat",
        },
        Row {
            reason: "forwarded-bearer-missing",
            ingress: Ingress::Anthropic,
            target: built(forwarded_anthropic_api),
            body: json_body(&json!({"model": ALIAS, "max_tokens": 16, "messages": [user("q")]})),
            lane: "anthropic->anthropic-api",
        },
        Row {
            reason: "cloak-relocation",
            ingress: Ingress::Anthropic,
            target: Target::CloakSeat,
            body: json_body(&json!({
                "model": ALIAS, "max_tokens": 16, "system": "client rules",
                "messages": [{"role": "system", "content": "turn rules"}]
            })),
            lane: "anthropic->anthropic-api",
        },
        Row {
            reason: "gemini-seed",
            ingress: Ingress::Chat,
            target: built(gemini),
            body: json_body(&json!({
                "model": ALIAS, "messages": [user("q")],
                "tools": [{"type": "function", "function": {
                    "name": "lookup", "parameters": many_property_schema(10_001)
                }}]
            })),
            lane: "chat->gemini",
        },
    ]
}

fn single_target_config(entry: ProviderEntry, strict: bool) -> Config {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    let mut config = Config {
        providers: BTreeMap::from([(PROVIDER.to_string(), entry)]),
        models: BTreeMap::from([(NICKNAME.to_string(), ModelEntry::new(PROVIDER, UPSTREAM))]),
        aliases: BTreeMap::from([(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()))]),
        retry,
        ..Default::default()
    };
    config.server.strict_translation = strict;
    config
}

async fn router_for(target: Target, base: &str) -> Router {
    match target {
        Target::Built { entry, strict } => {
            let config = single_target_config(entry(base), strict);
            let secrets: Arc<dyn routectl_auth::SecretStore> =
                Arc::new(routectl_auth::MemoryStore::new());
            crate::server::build_router_from_config(Arc::new(config), secrets)
                .await
                .expect("build router")
        }
        Target::CloakSeat => {
            let entry = ProviderEntry::anthropic_api(crate::test_secret::file_ref("k"))
                .with_auth_kind(AuthKind::OauthBearer);
            let mut router = Router::new(Arc::new(single_target_config(entry, false)));
            let mut cfg =
                AnthropicApiConfig::new_with_auth(PROVIDER, Arc::new(StaticToken::new("t")));
            cfg.auth_kind = AuthKind::OauthBearer;
            let provider = Arc::new(AnthropicApiProvider::new(cfg));
            router.install_resolved_models(BTreeMap::from([(
                NICKNAME.to_string(),
                Arc::new(ResolvedModel::new(NICKNAME, PROVIDER, provider, UPSTREAM)),
            )]));
            router
        }
    }
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers
}

/// What one row's request left behind: the client status, the calls that
/// reached the upstream, every local-refusal key, and the error body.
type Outcome = (StatusCode, usize, Vec<(&'static str, &'static str, u64)>);

async fn send(row: &Row) -> (Outcome, String) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(
        router_for(row.target, &server.uri()).await,
    ));
    let state = AppState::for_test(Arc::clone(&swap));
    let body = Ok(Bytes::from(row.body.clone()));
    let resp = Box::pin(async {
        match row.ingress {
            Ingress::Anthropic => {
                ingress_handle(state, headers(), None, body, AnthropicIngress).await
            }
            Ingress::Chat => ingress_handle(state, headers(), None, body, OpenAiIngress).await,
            Ingress::Responses => {
                ingress_handle(state, headers(), None, body, ResponsesIngress).await
            }
        }
    })
    .await;
    let status = resp.status();
    let text = to_bytes(resp.into_body(), usize::MAX)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let calls = server.received_requests().await.map_or(0, |r| r.len());
    let refusals = swap
        .load()
        .wire_check_snapshot()
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
        .collect();
    ((status, calls, refusals), text)
}

#[tokio::test]
async fn each_reachable_refusal_reason_answers_400_and_counts_once_under_its_lane() {
    let rows = rows();
    let mut observed = Vec::new();
    let mut bodies = Vec::new();
    for row in &rows {
        let (outcome, text) = send(row).await;
        bodies.push(format!("{}: {text}", row.reason));
        observed.push((row.reason, outcome));
    }

    let expected: Vec<(&str, Outcome)> = rows
        .iter()
        .map(|row| {
            let keys = vec![(row.lane, row.reason, 1)];
            (row.reason, (StatusCode::BAD_REQUEST, 0, keys))
        })
        .collect();
    assert_eq!(observed, expected, "client bodies: {bodies:#?}");
}

#[test]
fn the_matrix_covers_every_reason_but_the_unreachable_two() {
    let unreachable = ["translation-accounting", "schema-depth"];
    let covered: BTreeSet<&str> = rows().iter().map(|row| row.reason).collect();

    let expected: BTreeSet<&str> = routectl_core::RefusalReason::ALL
        .iter()
        .map(|reason| reason.as_str())
        .filter(|token| !unreachable.contains(token))
        .collect();
    assert_eq!(covered, expected);
}

/// `items` nested `depth` times: one schema level per JSON level.
fn nested_items_schema(depth: usize) -> Value {
    (0..depth).fold(json!({}), |inner, _| json!({"items": inner}))
}

#[tokio::test]
async fn a_schema_deeper_than_the_repair_bound_is_refused_as_unparseable_first() {
    let row = Row {
        reason: "schema-depth",
        ingress: Ingress::Anthropic,
        target: built(anthropic_api),
        body: json_body(&json!({
            "model": ALIAS, "max_tokens": 16, "messages": [user("q")],
            "output_config": {"format": {"type": "json_schema",
                                         "schema": nested_items_schema(257)}}
        })),
        lane: "anthropic->local",
    };

    let (outcome, text) = send(&row).await;

    assert_eq!(
        outcome,
        (
            StatusCode::BAD_REQUEST,
            0,
            vec![("anthropic->local", "unparseable", 1)]
        ),
        "{text}"
    );
}
