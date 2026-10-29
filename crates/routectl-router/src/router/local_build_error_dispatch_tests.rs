//! A request the egress cannot build (a client beta flag no HTTP header
//! can carry) fails locally with `Error::Internal` before anything reaches
//! the upstream, and fails the request closed: no breaker debit, no
//! same-provider retry, no fallback. Each row runs beside a sendable flag on
//! the same harness, whose refused connection is a status-0 network error
//! that debits, retries the same provider on a completion and falls back.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::{ChatRequest, Error, Message, MessageContent, Provider, Result, Role};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};

use super::scripted_provider_test_support::ScriptedProvider;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::Router;

const ALIAS: &str = "a";
/// A discard-port address nothing listens on, so a request that gets past
/// the build fails as a refused connection.
const CLOSED_PORT: &str = "http://127.0.0.1:9";
const UNSENDABLE_BETA: &str = "client-flag\nx";
const SENDABLE_BETA: &str = "client-flag";

/// Two attempts per target, so a same-provider retry is admitted, and a
/// breaker threshold no row reaches, so a debit never re-gates the retry to
/// CircuitOpen and the failure count shows each debited attempt.
fn router(fallback: &Arc<ScriptedProvider>) -> Router {
    let toml_text = format!(
        "version = 5\n[retry]\nmax_attempts = 2\ninitial_backoff_ms = 0\njitter_ms = 0\n\
         [providers.p0]\nkind = \"anthropic-api\"\nbase_url = \"{CLOSED_PORT}\"\n\
         api_key_ref = \"literal:k\"\ncircuit_failures = 5\n\
         [providers.p1]\nkind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\ncircuit_failures = 5\n\
         [aliases]\n{ALIAS} = [\"m0\", \"m1\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let primary: Arc<dyn Provider> = Arc::new(AnthropicApiProvider::new(AnthropicApiConfig {
        base_url: CLOSED_PORT.into(),
        ..AnthropicApiConfig::new("p0", "k")
    }));
    let fallback = Arc::clone(fallback) as Arc<dyn Provider>;
    let mut router = Router::new(Arc::new(config));
    let models = [("m0", "p0", primary), ("m1", "p1", fallback)]
        .into_iter()
        .map(|(nickname, provider_id, provider)| {
            let model = ResolvedModel::new(nickname, provider_id, provider, "upstream");
            (nickname.to_string(), Arc::new(model))
        })
        .collect::<BTreeMap<_, _>>();
    router.install_resolved_models(models);
    router
}

fn request(beta: &str) -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        max_tokens: Some(16),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        anthropic_beta: vec![beta.into()],
        ..Default::default()
    }
}

fn served(_: &ChatRequest) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Surface {
    Complete,
    Stream,
}

async fn dispatch(router: &Router, surface: Surface, beta: &str) -> Result<()> {
    match surface {
        Surface::Complete => router.complete(request(beta)).await.map(|_| ()),
        Surface::Stream => router.stream(request(beta)).await.map(|_| ()),
    }
}

fn primary_failures(router: &Router) -> u32 {
    router
        .state
        .get("m0")
        .expect("primary state slot exists")
        .lock()
        .consecutive_failures_for_tests()
}

struct Row {
    name: &'static str,
    surface: Surface,
    beta: &'static str,
    /// Debited primary attempts: one per attempt made on a network error.
    primary_debits: u32,
    fallback_calls: usize,
}

#[tokio::test]
async fn an_unbuildable_request_neither_debits_retries_nor_falls_back() {
    let rows = [
        Row {
            name: "complete: unbuildable request",
            surface: Surface::Complete,
            beta: UNSENDABLE_BETA,
            primary_debits: 0,
            fallback_calls: 0,
        },
        Row {
            name: "complete: refused connection (control)",
            surface: Surface::Complete,
            beta: SENDABLE_BETA,
            primary_debits: 2,
            fallback_calls: 1,
        },
        Row {
            name: "stream: unbuildable request",
            surface: Surface::Stream,
            beta: UNSENDABLE_BETA,
            primary_debits: 0,
            fallback_calls: 0,
        },
        Row {
            name: "stream: refused connection (control)",
            surface: Surface::Stream,
            beta: SENDABLE_BETA,
            primary_debits: 1,
            fallback_calls: 1,
        },
    ];
    for row in rows {
        // Arrange
        let fallback = ScriptedProvider::new(None, served);
        let router = router(&fallback);

        // Act
        let result = dispatch(&router, row.surface, row.beta).await;

        // Assert
        let name = row.name;
        assert_eq!(
            primary_failures(&router),
            row.primary_debits,
            "{name}: debits"
        );
        assert_eq!(
            fallback.calls(),
            row.fallback_calls,
            "{name}: fallback calls"
        );
        if row.fallback_calls > 0 {
            assert!(result.is_ok(), "{name}: served by the fallback");
        } else {
            let err = result.expect_err("an unbuildable request fails");
            assert!(matches!(err, Error::Internal(_)), "{name}: got {err:?}");
            assert!(
                router.gate_status_for_tests("m0").last_outcome.is_none(),
                "{name}: no outcome settled"
            );
        }
    }
}
