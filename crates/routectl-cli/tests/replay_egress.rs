//! Replay-driven egress contract test.
//!
//! Walks every fixture under `tests/fixtures/canon/`, drives the
//! captured ingress request through the matching egress provider's
//! `normalize_request`, and asserts the upstream-bound JSON body
//! matches the on-disk `outgoing_request.json` structurally.
//!
//! Phase one scope: anthropic ingress only. Egress providers covered
//! are `anthropic-api`, `openai-compat`, and `openai-responses`.
//! Bedrock is out of scope. Fixtures with unrecognized or skipped
//! provider kinds are logged and bypassed without failing the test.
//!
//! Zero fixtures is acceptable: when `canon/` holds no scenario
//! directories the test passes silently with a single info log so it
//! can land before the seed corpus is committed.

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

use common::replay::{assert_json_equal_structural, discover_fixtures, Fixture};

/// Path (relative to the workspace root) to the hand-curated fixture
/// corpus. `discover_fixtures` returns an empty vector when the
/// directory contains only `.gitkeep` / `README.md`, which keeps this
/// test passing before the seed corpus lands.
fn canon_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/canon")
}

/// Build a `HeaderMap` from the `(name, value)` pairs persisted in a
/// fixture's `*.headers.json`. Pairs whose name or value cannot be
/// converted into the `http` types are skipped silently -- a fixture
/// produced by the capture rig stores ASCII-clean headers, and this
/// helper is a defensive bridge between the loader's
/// `Vec<(String, String)>` and the ingress's `&HeaderMap`.
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

fn anthropic_api_provider() -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-replay".into(),
        auth: Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
    })
}

fn openai_compat_provider() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "openai-compat-replay".into(),
        base_url: "https://api.openai.com/v1".into(),
        api_key: "test-key".into(),
        header_extras: Vec::new(),
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        disable_stream_include_usage: false,
    })
}

fn openai_responses_provider() -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
        "openai-responses-replay",
        "test-key",
    ))
}

/// Outcome of a per-fixture run. `Skipped` carries a human-readable
/// reason so the test driver can surface it as an info log rather than
/// a failure.
enum FixtureOutcome {
    Asserted,
    Skipped(String),
}

/// Drive `normalize_request` for the egress matched by
/// `meta.provider_kind`. Returns `Ok(None)` when the provider kind is
/// recognized-but-skipped (bedrock variants); the caller emits a skip
/// log. An unknown kind is treated as a fixture authoring bug.
fn normalize_for_kind(
    kind: &str,
    canonical: &ChatRequest,
) -> Result<Option<serde_json::Value>, String> {
    match kind {
        "anthropic-api" => anthropic_api_provider()
            .normalize_request(canonical)
            .map(Some)
            .map_err(|e| format!("anthropic-api normalize_request failed: {e}")),
        "openai-compat" => openai_compat_provider()
            .normalize_request(canonical)
            .map(Some)
            .map_err(|e| format!("openai-compat normalize_request failed: {e}")),
        "openai-responses" => openai_responses_provider()
            .normalize_request(canonical)
            .map(Some)
            .map_err(|e| format!("openai-responses normalize_request failed: {e}")),
        // Bedrock egress replay is out of scope for phase one.
        "bedrock" | "bedrock-invoke" | "bedrock-converse" => Ok(None),
        other => Err(format!("unknown provider_kind `{other}`")),
    }
}

/// Run the egress assertion for one fixture. Skips return with a
/// reason; a real diff returns an `Err`.
fn run_egress_assertion(fixture: &Fixture) -> Result<FixtureOutcome, String> {
    let headers = headers_from_pairs(&fixture.ingress_request_headers);
    let canonical = AnthropicIngress
        .parse_request(&headers, fixture.ingress_request.clone())
        .map_err(|e| format!("anthropic ingress parse_request failed: {e}"))?;

    let Some(actual_body) = normalize_for_kind(&fixture.meta.provider_kind, &canonical)? else {
        return Ok(FixtureOutcome::Skipped(format!(
            "provider_kind `{}` out of phase-one scope",
            fixture.meta.provider_kind,
        )));
    };

    assert_json_equal_structural(&actual_body, &fixture.outgoing_request, &[])
        .map_err(|e| format!("outgoing_request body mismatch: {e}"))?;

    Ok(FixtureOutcome::Asserted)
}

#[test]
fn egress_replay_all() {
    let root = canon_root();
    if !root.exists() {
        println!(
            "[replay_egress] canon/ root `{}` not present; nothing to assert.",
            root.display(),
        );
        return;
    }
    let fixtures = match discover_fixtures(&root) {
        Ok(f) => f,
        Err(e) => panic!("failed to discover fixtures under {}: {e}", root.display()),
    };
    if fixtures.is_empty() {
        println!("[replay_egress] 0 fixtures in canon/; nothing to assert.");
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut asserted = 0usize;
    let mut skipped = 0usize;
    for fixture in &fixtures {
        match run_egress_assertion(fixture) {
            Ok(FixtureOutcome::Asserted) => asserted += 1,
            Ok(FixtureOutcome::Skipped(reason)) => {
                println!(
                    "[replay_egress] skipping fixture `{}`: {reason}",
                    fixture.name,
                );
                skipped += 1;
            }
            Err(msg) => failures.push(format!("fixture `{}`: {msg}", fixture.name)),
        }
    }

    println!(
        "[replay_egress] {} fixture(s): {} asserted, {} skipped, {} failed",
        fixtures.len(),
        asserted,
        skipped,
        failures.len(),
    );

    if !failures.is_empty() {
        panic!(
            "{} egress replay failure(s):\n  - {}",
            failures.len(),
            failures.join("\n  - "),
        );
    }
}
