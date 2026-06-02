//! Replay-driven ingress contract test.
//!
//! Walks every fixture under `tests/fixtures/captured/`, mounts the
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
//! Openai-responses ingress replay is deferred from phase one:
//! `OpenAiResponsesProvider::complete()` always sets `stream:true`
//! and consumes SSE, so a wiremock returning a captured
//! post-extraction JSON body breaks the eventsource parser. Replay
//! of that egress arrives once the capture rig writes a raw-SSE
//! variant (or the test driver wraps the JSON in a synthetic event
//! stream).
//!
//! Phase one also bypasses fixtures whose model needs router-side
//! enrichment (adaptive thinking, DeepSeek `history_reasoning`) that
//! the bare ingress -> egress path does not yet replay -- see the
//! "Phase 1 corpus scope" section in `docs/REPLAY-FIXTURES.md`.
//!
//! Zero exercisable fixtures is acceptable: the captured/ corpus is
//! per-contributor and gitignored, so a fresh checkout (or one with
//! only out-of-scope captures) passes silently with a single info log.

mod common;

use std::sync::Arc;

use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::IngressAdapter;
use routectl_core::{ChatRequest, Provider, StaticToken};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::replay::{
    assert_json_equal_structural, captured_root, discover_fixtures, headers_from_pairs,
    phase1_skip_reason, Fixture, FixtureOutcome,
};

/// Description of which path + content-type the egress provider hits
/// upstream. Wiremock matches on these to serve the captured response.
struct EgressMount {
    method_str: &'static str,
    path_str: &'static str,
}

/// Map a provider kind to its upstream wiremock route. Returns
/// `Ok(None)` for kinds that are recognized but out of phase-one
/// scope (bedrock variants, openai-responses); an unknown kind is
/// treated as a fixture authoring bug and surfaces as an `Err`.
///
/// NOTE: this match must stay in lockstep with
/// `build_provider_for_kind` below and
/// `replay_egress.rs::normalize_for_kind` -- when adding or renaming
/// a kind, edit all three.
fn mount_for_kind(kind: &str) -> Result<Option<EgressMount>, String> {
    match kind {
        "anthropic" => Ok(Some(EgressMount {
            method_str: "POST",
            path_str: "/v1/messages",
        })),
        "openai-compat" => Ok(Some(EgressMount {
            method_str: "POST",
            path_str: "/chat/completions",
        })),
        // openai-responses ingress replay is deferred from phase one;
        // see the module doc for the SSE-shape rationale.
        "openai-responses" => Ok(None),
        // Bedrock egress replay is out of scope for phase one.
        "bedrock-invoke" | "bedrock-converse" => Ok(None),
        other => Err(format!("unknown provider_kind `{other}`")),
    }
}

/// Build the egress provider for a given kind, pointed at the
/// wiremock server. Returns `Ok(None)` for kinds that are recognized
/// but out of phase-one scope; an unknown kind surfaces as an `Err`
/// and fails the test.
///
/// NOTE: keep in lockstep with `mount_for_kind` and
/// `replay_egress.rs::normalize_for_kind`.
fn build_provider_for_kind(
    kind: &str,
    base_url: String,
) -> Result<Option<Box<dyn Provider>>, String> {
    match kind {
        "anthropic" => Ok(Some(Box::new(AnthropicApiProvider::new(
            AnthropicApiConfig {
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
                max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            },
        )))),
        "openai-compat" => Ok(Some(Box::new(OpenAiCompatProvider::new(
            OpenAiCompatConfig {
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
            },
        )))),
        // FUTURE: replace with provider construction when SSE-aware ingress replay lands (see mount_for_kind comment).
        "openai-responses" => Ok(None),
        "bedrock-invoke" | "bedrock-converse" => Ok(None),
        other => Err(format!("unknown provider_kind `{other}`")),
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

    let Some(mount) = mount_for_kind(&fixture.meta.provider_kind)? else {
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
    let Some(provider) = build_provider_for_kind(&fixture.meta.provider_kind, server.uri())? else {
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
    if let Some(reason) = phase1_skip_reason(fixture) {
        return Ok(FixtureOutcome::Skipped(reason));
    }
    if fixture.meta.stream {
        return Ok(FixtureOutcome::Skipped(
            "stream fixture; stream-body capture deferred".into(),
        ));
    }
    run_non_stream_fixture(fixture).await
}

#[tokio::test]
async fn ingress_replay_all() {
    let root = captured_root();
    if !root.exists() {
        eprintln!(
            "[replay_ingress] captured/ root `{}` not present; nothing to assert.",
            root.display(),
        );
        return;
    }
    let fixtures = match discover_fixtures(&root) {
        Ok(f) => f,
        Err(e) => panic!("failed to discover fixtures under {}: {e}", root.display()),
    };
    if fixtures.is_empty() {
        eprintln!("[replay_ingress] 0 fixtures in captured/; nothing to assert.");
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut asserted = 0usize;
    let mut skipped = 0usize;
    for fixture in &fixtures {
        match run_fixture(fixture).await {
            Ok(FixtureOutcome::Asserted) => asserted += 1,
            Ok(FixtureOutcome::Skipped(reason)) => {
                eprintln!(
                    "[replay_ingress] skipping fixture `{}`: {reason}",
                    fixture.name,
                );
                skipped += 1;
            }
            Err(msg) => failures.push(format!("fixture `{}`: {msg}", fixture.name)),
        }
    }

    eprintln!(
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
