//! Replay-driven egress contract test.
//!
//! Walks every fixture under `tests/fixtures/canon/`, drives the
//! captured ingress request through the matching egress provider's
//! `normalize_request`, and asserts the upstream-bound JSON body
//! matches the on-disk `outgoing_request.json` structurally.
//!
//! Phase one scope: anthropic ingress only. Egress providers covered
//! are `anthropic` (the api.anthropic.com client; the in-code
//! `PROVIDER_KIND` constant is `"anthropic"`), `openai-compat`, and
//! `openai-responses`. Bedrock is out of scope.
//!
//! Fixtures with out-of-scope provider kinds (bedrock variants) are
//! logged and bypassed; an unrecognized or misspelled `provider_kind`
//! is treated as a fixture authoring error and fails the test.
//!
//! Phase one also bypasses fixtures whose model needs router-side
//! enrichment (adaptive thinking, DeepSeek `history_reasoning`) that
//! the bare ingress -> egress path does not yet replay -- see the
//! "Phase 1 corpus scope" section in `docs/REPLAY-FIXTURES.md`.
//!
//! Zero fixtures is acceptable: when `canon/` holds no scenario
//! directories the test passes silently with a single info log so it
//! can land before the seed corpus is committed.

mod common;

use std::sync::Arc;

use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::IngressAdapter;
use routectl_core::{ChatRequest, Provider, StaticToken};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_providers::openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider};

use common::replay::{
    assert_json_equal_structural, canon_root, discover_fixtures, headers_from_pairs,
    phase1_skip_reason, Fixture, FixtureOutcome,
};

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
        max_thinking_entry_bytes: AnthropicApiConfig::DEFAULT_MAX_THINKING_ENTRY_BYTES,
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

// NOTE: `OpenAiResponsesConfig::new` hard-codes
// `auth_kind = AuthKind::ChatgptOauth` and `account_id = None`. Replay
// fixtures captured from a non-OAuth Responses session may diverge on
// store/account_id-gated body shape. The phase-one corpus is scoped
// to ChatgptOauth-captured fixtures; if that ever changes, pin
// `auth_kind` in `meta.json` and branch the builder here.
fn openai_responses_provider() -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig::new(
        "openai-responses-replay",
        "test-key",
    ))
}

/// Drive `normalize_request` for the egress matched by
/// `meta.provider_kind`. Returns `Ok(None)` when the provider kind is
/// recognized-but-skipped (bedrock variants); the caller emits a skip
/// log. An unknown kind is treated as a fixture authoring bug.
///
/// NOTE: this match must stay in lockstep with
/// `replay_ingress.rs::mount_for_kind` and
/// `replay_ingress.rs::build_provider_for_kind` -- when adding or
/// renaming a kind, edit all three.
fn normalize_for_kind(
    kind: &str,
    canonical: &ChatRequest,
) -> Result<Option<serde_json::Value>, String> {
    match kind {
        // The in-code constant in `routectl_providers::anthropic_api`
        // is `"anthropic"`; the capture rig writes that verbatim, so
        // the test side aligns to the source of truth.
        "anthropic" => anthropic_api_provider()
            .normalize_request(canonical)
            .map(Some)
            .map_err(|e| format!("anthropic normalize_request failed: {e}")),
        "openai-compat" => openai_compat_provider()
            .normalize_request(canonical)
            .map(Some)
            .map_err(|e| format!("openai-compat normalize_request failed: {e}")),
        "openai-responses" => openai_responses_provider()
            .normalize_request(canonical)
            .map(Some)
            .map_err(|e| format!("openai-responses normalize_request failed: {e}")),
        // Bedrock egress replay is out of scope for phase one.
        "bedrock-invoke" | "bedrock-converse" => Ok(None),
        other => Err(format!("unknown provider_kind `{other}`")),
    }
}

/// Per-provider `ignore_paths` for the structural body comparator.
/// Each ignored key is a body field the egress flips AFTER
/// `normalize_request` returns and BEFORE the trace-line capture, so
/// the captured `outgoing_request.json` and the bare-`normalize_request`
/// output diverge there by design:
///
/// - `stream`: anthropic-api `complete()` strips it, `stream()`
///   inserts `true`; openai-compat `complete()` forces `false`,
///   `stream()` forces `true`; openai-responses always forces `true`.
/// - `anthropic_beta`: anthropic-api `complete()` and `stream()` both
///   strip the body field (betas travel on the HTTP header instead).
/// - `stream_options`: openai-compat `stream()` auto-injects
///   `stream_options.include_usage = true`.
fn ignore_paths_for_kind(kind: &str, stream: bool) -> Vec<&'static str> {
    let mut paths = vec!["stream"];
    match kind {
        "anthropic" => paths.push("anthropic_beta"),
        "openai-compat" if stream => paths.push("stream_options"),
        _ => {}
    }
    paths
}

/// Run the egress assertion for one fixture. Skips return with a
/// reason; a real diff returns an `Err`.
fn run_egress_assertion(fixture: &Fixture) -> Result<FixtureOutcome, String> {
    if let Some(reason) = phase1_skip_reason(fixture) {
        return Ok(FixtureOutcome::Skipped(reason));
    }

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

    let ignore = ignore_paths_for_kind(&fixture.meta.provider_kind, fixture.meta.stream);
    assert_json_equal_structural(&actual_body, &fixture.outgoing_request, &ignore)
        .map_err(|e| format!("outgoing_request body mismatch: {e}"))?;

    Ok(FixtureOutcome::Asserted)
}

#[test]
fn egress_replay_all() {
    let root = canon_root();
    if !root.exists() {
        eprintln!(
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
        eprintln!("[replay_egress] 0 fixtures in canon/; nothing to assert.");
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut asserted = 0usize;
    let mut skipped = 0usize;
    for fixture in &fixtures {
        match run_egress_assertion(fixture) {
            Ok(FixtureOutcome::Asserted) => asserted += 1,
            Ok(FixtureOutcome::Skipped(reason)) => {
                eprintln!(
                    "[replay_egress] skipping fixture `{}`: {reason}",
                    fixture.name,
                );
                skipped += 1;
            }
            Err(msg) => failures.push(format!("fixture `{}`: {msg}", fixture.name)),
        }
    }

    eprintln!(
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
