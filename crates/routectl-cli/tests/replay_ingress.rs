//! Replay-driven ingress contract test.
//!
//! Walks every fixture under `tests/fixtures/canon/`, mounts the
//! captured upstream response in a wiremock server, drives the
//! matching egress provider's `complete()` against it, runs the
//! resulting canonical `ChatResponse` through `AnthropicIngress::
//! render_response`, and asserts the rendered JSON body matches the
//! captured `egress_response` structurally.
//!
//! Phase one scope: anthropic ingress only, non-stream fixtures only.
//! Stream fixtures captured today have empty
//! `egress_response`/`upstream_response` slots (the capture rig does
//! not write stream bodies yet); this test skips them with an info
//! log so the rest of the corpus still runs. Bedrock egress is also
//! out of scope.
//!
//! Zero exercisable fixtures is acceptable: when `canon/` has none of
//! the supported (non-stream + recognized provider) fixtures the test
//! passes silently with a single info log so it can land before the
//! seed corpus is committed.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::IngressAdapter;
use routectl_core::{ChatRequest, Provider, StaticToken};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_providers::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::replay::{assert_json_equal_structural, discover_fixtures, Fixture};

/// Path (relative to the workspace root) to the hand-curated fixture
/// corpus. Mirrors `replay_egress.rs::canon_root`.
fn canon_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/canon")
}

/// Build a `HeaderMap` from the loader's `Vec<(String, String)>`. See
/// the matching helper in `replay_egress.rs` for rationale.
fn headers_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in pairs {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        out.insert(name, value);
    }
    out
}

/// Outcome of one fixture's run. `Skipped` carries a human-readable
/// reason; `Asserted` means the fixture was exercised end-to-end.
enum FixtureOutcome {
    Asserted,
    Skipped(String),
}

/// Description of which path + content-type the egress provider hits
/// upstream. Wiremock matches on these to serve the captured response.
struct EgressMount {
    method_str: &'static str,
    path_str: &'static str,
}

fn mount_for_kind(kind: &str) -> Option<EgressMount> {
    match kind {
        "anthropic-api" => Some(EgressMount {
            method_str: "POST",
            path_str: "/v1/messages",
        }),
        "openai-compat" => Some(EgressMount {
            method_str: "POST",
            path_str: "/chat/completions",
        }),
        "openai-responses" => Some(EgressMount {
            method_str: "POST",
            path_str: "/responses",
        }),
        _ => None,
    }
}

/// Build the egress provider for a given kind, pointed at the
/// wiremock server. Returns `None` for kinds that are recognized but
/// out of phase-one scope (bedrock variants).
fn build_provider_for_kind(kind: &str, base_url: String) -> Option<Box<dyn Provider>> {
    match kind {
        "anthropic-api" => Some(Box::new(AnthropicApiProvider::new(AnthropicApiConfig {
            id: "anthropic-replay".into(),
            auth: Arc::new(StaticToken::new("test-key")),
            base_url,
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            allowed_betas: Vec::new(),
            forward_client_headers: Vec::new(),
            context_management: false,
        }))),
        "openai-compat" => Some(Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
            id: "openai-compat-replay".into(),
            base_url,
            api_key: "test-key".into(),
            header_extras: Vec::new(),
            payload_extras: None,
            reasoning_dialect: ReasoningDialect::OpenAi,
            history_reasoning: HistoryReasoning::Auto,
            user_agent: None,
            strict_translation: false,
            disable_stream_include_usage: false,
        }))),
        "openai-responses" => {
            let mut cfg = OpenAiResponsesConfig::new("openai-responses-replay", "test-key");
            cfg.base_url = base_url;
            Some(Box::new(OpenAiResponsesProvider::new(cfg)))
        }
        _ => None,
    }
}

/// Mount a wiremock handler returning the captured upstream body on
/// `<method> <path>` and return the running mock server.
async fn mount_upstream(mount: &EgressMount, body: Vec<u8>, content_type: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method(mount.method_str))
        .and(path(mount.path_str))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", content_type)
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;
    server
}

/// Build the canonical `ChatRequest` from the fixture's captured
/// ingress request + headers. Phase-one ingress is anthropic-only.
fn parse_canonical(fixture: &Fixture) -> Result<ChatRequest, String> {
    let headers = headers_from_pairs(&fixture.ingress_request_headers);
    AnthropicIngress
        .parse_request(&headers, fixture.ingress_request.clone())
        .map_err(|e| format!("anthropic ingress parse_request failed: {e}"))
}

/// Drive one non-stream fixture end-to-end and compare the rendered
/// ingress response against the captured `egress_response.json`.
async fn run_non_stream_fixture(fixture: &Fixture) -> Result<FixtureOutcome, String> {
    if !fixture.meta.has_upstream_response {
        return Ok(FixtureOutcome::Skipped(
            "no captured upstream_response; ingress side cannot be exercised".into(),
        ));
    }
    if !fixture.meta.has_egress_response {
        return Ok(FixtureOutcome::Skipped(
            "no captured egress_response; nothing to compare against".into(),
        ));
    }

    let Some(mount) = mount_for_kind(&fixture.meta.provider_kind) else {
        return Ok(FixtureOutcome::Skipped(format!(
            "provider_kind `{}` out of phase-one scope",
            fixture.meta.provider_kind,
        )));
    };

    let server = mount_upstream(
        &mount,
        fixture.upstream_response_bytes.clone(),
        "application/json",
    )
    .await;
    let Some(provider) = build_provider_for_kind(&fixture.meta.provider_kind, server.uri()) else {
        return Ok(FixtureOutcome::Skipped(format!(
            "provider_kind `{}` lacks a phase-one builder",
            fixture.meta.provider_kind,
        )));
    };

    let canonical = parse_canonical(fixture)?;
    let response = provider
        .complete(canonical)
        .await
        .map_err(|e| format!("provider.complete failed: {e}"))?;
    let rendered = AnthropicIngress
        .render_response(response)
        .map_err(|e| format!("AnthropicIngress.render_response failed: {e}"))?;

    let expected: Value = serde_json::from_slice(&fixture.egress_response_bytes)
        .map_err(|e| format!("egress_response.json parse failed: {e}"))?;
    assert_json_equal_structural(&rendered, &expected, &[])
        .map_err(|e| format!("rendered ingress response mismatch: {e}"))?;
    Ok(FixtureOutcome::Asserted)
}

/// Drive one fixture, dispatching to the stream or non-stream path
/// based on `meta.stream`. Stream fixtures are skipped pending the
/// capture rig writing stream bodies (deferred from phase one).
async fn run_fixture(fixture: &Fixture) -> Result<FixtureOutcome, String> {
    if fixture.meta.stream {
        return Ok(FixtureOutcome::Skipped(
            "stream fixture; stream-body capture deferred".into(),
        ));
    }
    run_non_stream_fixture(fixture).await
}

#[tokio::test]
async fn ingress_replay_all() {
    let root = canon_root();
    if !root.exists() {
        println!(
            "[replay_ingress] canon/ root `{}` not present; nothing to assert.",
            root.display(),
        );
        return;
    }
    let fixtures = match discover_fixtures(&root) {
        Ok(f) => f,
        Err(e) => panic!("failed to discover fixtures under {}: {e}", root.display()),
    };
    if fixtures.is_empty() {
        println!("[replay_ingress] 0 fixtures in canon/; nothing to assert.");
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut asserted = 0usize;
    let mut skipped = 0usize;
    for fixture in &fixtures {
        match run_fixture(fixture).await {
            Ok(FixtureOutcome::Asserted) => asserted += 1,
            Ok(FixtureOutcome::Skipped(reason)) => {
                println!(
                    "[replay_ingress] skipping fixture `{}`: {reason}",
                    fixture.name,
                );
                skipped += 1;
            }
            Err(msg) => failures.push(format!("fixture `{}`: {msg}", fixture.name)),
        }
    }

    println!(
        "[replay_ingress] {} fixture(s): {} asserted, {} skipped, {} failed",
        fixtures.len(),
        asserted,
        skipped,
        failures.len(),
    );

    if !failures.is_empty() {
        panic!(
            "{} ingress replay failure(s):\n  - {}",
            failures.len(),
            failures.join("\n  - "),
        );
    }
}
