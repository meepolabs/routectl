//! Replay-driven ingress contract test.
//!
//! Walks every fixture under `tests/fixtures/captured/`, mounts the
//! captured upstream response in a wiremock server, drives the
//! matching egress provider's `complete()` against it, runs the
//! resulting canonical `ChatResponse` through `AnthropicIngress::
//! render_response`, and asserts the rendered JSON body matches the
//! captured `egress_response` structurally.
//!
//! Current scope: the response render is anthropic-only, non-stream
//! fixtures only. The REQUEST leg parses through the adapter named by
//! `meta.ingress_kind`; the render leg still goes through
//! `AnthropicIngress`, so a fixture captured on another ingress dialect
//! compares an Anthropic-shape render against its own dialect's captured
//! response and is expected to diverge until the render leg dispatches
//! too. Stream fixtures captured today have empty
//! `egress_response`/`upstream_response` slots (the capture rig does
//! not write stream bodies yet); this test skips them with an info
//! log so the rest of the corpus still runs. Bedrock egress is also
//! out of scope.
//!
//! Openai-responses ingress replay is deferred from the current scope:
//! `OpenAiResponsesProvider::complete()` always sets `stream:true`
//! and consumes SSE, so a wiremock returning a captured
//! post-extraction JSON body breaks the eventsource parser. Replay
//! of that egress arrives once the capture rig writes a raw-SSE
//! variant (or the test driver wraps the JSON in a synthetic event
//! stream).
//!
//! Per-model router enrichment is reconstructed for each fixture (see
//! `common::replay::replay_resolved_model`); the residual set of models
//! whose enrichment cannot be reconstructed is bypassed -- see the
//! "Corpus scope" section in `docs/REPLAY-FIXTURES.md`.
//!
//! Zero exercisable fixtures is acceptable: the captured/ corpus is
//! per-contributor and gitignored, so a fresh checkout (or one with
//! only out-of-scope captures) passes silently with a single info log.

mod common;

use std::sync::Arc;

use routectl_cli::ingress::IngressAdapter;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_core::{Provider, StaticToken};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::replay::{
    Fixture, FixtureOutcome, bounded_body_diff, discover_fixtures, enrichment_skip_reason,
    local_root, parse_enriched_canonical, unpinned_ingress_skip_reason,
};

/// Description of which path + content-type the egress provider hits
/// upstream. Wiremock matches on these to serve the captured response.
struct EgressMount {
    method_str: &'static str,
    path_str: &'static str,
}

/// Map a provider kind to its upstream wiremock route. Returns
/// `Ok(None)` for kinds that are recognized but out of current
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
        // openai-responses ingress replay is deferred; see the module doc for the SSE-shape rationale.
        "openai-responses" => Ok(None),
        // Bedrock egress replay is out of current scope.
        "bedrock-invoke" | "bedrock-converse" => Ok(None),
        other => Err(format!("unknown provider_kind `{other}`")),
    }
}

/// Build the egress provider for a given kind, pointed at the
/// wiremock server. Returns `Ok(None)` for kinds that are recognized
/// but out of current scope; an unknown kind surfaces as an `Err`
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
                session_id: None,
                cloak: CloakConfig::default(),
                use_forwarded_bearer: false,

                mantle: None,
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
                mantle: None,
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

/// Drive one non-stream fixture end-to-end and compare the rendered
/// ingress response against the captured `egress_response.json`.
async fn run_non_stream_fixture(fixture: &Fixture) -> Result<FixtureOutcome, String> {
    if fixture.upstream_response_bytes.is_empty() {
        return Ok(FixtureOutcome::Skipped(
            "no captured upstream_response; ingress side cannot be exercised".into(),
        ));
    }
    if fixture.egress_response_bytes.is_empty() {
        return Ok(FixtureOutcome::Skipped(
            "no captured egress_response; nothing to compare against".into(),
        ));
    }

    // Parse before mounting: an unpinned or unknown ingress dialect
    // decides the fixture's fate without a wiremock server ever starting.
    let Some(canonical) = parse_enriched_canonical(fixture)? else {
        return Ok(FixtureOutcome::Skipped(unpinned_ingress_skip_reason()));
    };

    let Some(mount) = mount_for_kind(&fixture.meta.provider_kind)? else {
        return Ok(FixtureOutcome::Skipped(format!(
            "provider_kind `{}` out of current scope",
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
            "provider_kind `{}` has no current-scope builder",
            fixture.meta.provider_kind,
        )));
    };

    let response = provider
        .complete(canonical)
        .await
        .map_err(|e| format!("provider.complete failed: {e}"))?;
    let rendered_bytes = AnthropicIngress
        .render_response(response)
        .map_err(|e| format!("AnthropicIngress.render_response failed: {e}"))?;
    let rendered: Value = serde_json::from_slice(&rendered_bytes)
        .map_err(|e| format!("rendered ingress response parse failed: {e}"))?;

    let expected: Value = serde_json::from_slice(&fixture.egress_response_bytes)
        .map_err(|e| format!("egress_response.json parse failed: {e}"))?;
    if let Some(summary) = bounded_body_diff(&rendered, &expected, &[]) {
        return Err(format!("rendered ingress response mismatch: {summary}"));
    }
    Ok(FixtureOutcome::Asserted)
}

/// Drive one fixture, dispatching to the stream or non-stream path
/// based on `meta.stream`. Stream fixtures are skipped pending the
/// capture rig writing stream bodies.
async fn run_fixture(fixture: &Fixture) -> Result<FixtureOutcome, String> {
    if let Some(reason) = enrichment_skip_reason(fixture) {
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
    // The LIVE-BOX root, named explicitly: this driver is report-only
    // and must never gate, because these bodies are real prompts.
    let root = local_root();
    if !root.exists() {
        eprintln!(
            "[replay_ingress] local captured/ root `{}` not present; nothing to assert.",
            root.display(),
        );
        return;
    }
    let fixtures = match discover_fixtures(&root) {
        Ok(corpus) => corpus.fixtures,
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

    assert!(
        failures.is_empty(),
        "{} ingress replay failure(s):\n  - {}",
        failures.len(),
        failures.join("\n  - "),
    )
}
