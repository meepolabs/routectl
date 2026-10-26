//! Unit coverage for the replay harness's two selection decisions:
//! which fixtures the residual enrichment filter skips, and which ingress
//! adapter parses its captured inbound body.
//!
//! Both are driven off hand-built fixtures rather than the captured
//! corpus: the corpus is per-contributor and gitignored, so a test that
//! needed it would silently assert nothing on a fresh checkout.

mod common;

use std::fs;
use std::path::Path;

use routectl_cli::ingress::IngressAdapter;
use routectl_core::{Provider, StaticToken};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use serde_json::{Value, json};
use tempfile::tempdir;

use common::replay::{
    ENRICHMENT_DEPENDENT_MODELS, FIXTURE_SCHEMA_VERSION, Fixture, bounded_body_diff, diff_all,
    divergence_count, diverges_only_in_messages, enrichment_skip_reason, ingress_for_kind,
    load_fixture, parse_fixture_canonical, system_turn_lift_skip_reason,
};

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

/// A thinking-enabled Anthropic Messages request. `max_tokens` sits well
/// above Anthropic's 1024 thinking floor so the legacy `Enabled` shape is
/// reachable.
fn thinking_ingress_body(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 8192,
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "output_config": {"effort": "high"},
        "messages": [{"role": "user", "content": "hello"}],
    })
}

fn write_fixture_dir(root: &Path, name: &str, ingress_kind: &str, model: &str) -> Fixture {
    let dir = root.join(name);
    fs::create_dir(&dir).expect("create fixture dir");
    let meta = json!({
        "schema_version": FIXTURE_SCHEMA_VERSION,
        "provider_kind": "anthropic",
        "lane": "anthropic-api",
        "ingress_kind": ingress_kind,
        "case_id": "thinking-enabled",
        "config_sha": "abc123",
        "client": {"name": "claude-code", "version": "2.1.167", "connection_mode": "base-url"},
        "stream": false,
        "model": model,
        "routectl_version": "0.8.0",
    });
    let headers = json!([["content-type", "application/json"]]);
    fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    fs::write(
        dir.join("ingress_request.json"),
        serde_json::to_vec(&thinking_ingress_body(model)).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("ingress_request.headers.json"),
        serde_json::to_vec(&headers).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("outgoing_request.json"),
        serde_json::to_vec(&json!({"model": model})).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("outgoing_request.headers.json"),
        serde_json::to_vec(&headers).unwrap(),
    )
    .unwrap();
    load_fixture(&dir).expect("hand-built fixture loads")
}

fn anthropic_egress() -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-replay".into(),
        auth: std::sync::Arc::new(StaticToken::new("test-key")),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        mantle: None,
    })
}

/// The `thinking.type` the Anthropic egress puts on the wire for this
/// fixture.
fn wire_thinking_type(fixture: &Fixture) -> String {
    let canonical = parse_fixture_canonical(fixture)
        .expect("ingress dispatch resolves")
        .expect("fixture pins an ingress_kind");
    let body = anthropic_egress()
        .normalize_request(&canonical)
        .expect("normalize_request succeeds");
    body.get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("<absent>")
        .to_string()
}

// ---------------------------------------------------------------------------
// Enrichment: the closed skip
// ---------------------------------------------------------------------------

/// A pre-4.7 Anthropic model replays on the legacy shape it was sent.
#[test]
fn pre_adaptive_anthropic_model_replays_under_the_legacy_wire_shape() {
    // Arrange
    let tmp = tempdir().unwrap();
    let fixture = write_fixture_dir(tmp.path(), "sonnet", "anthropic", "claude-sonnet-4-5");

    // Act
    let shape = wire_thinking_type(&fixture);

    // Assert
    assert!(enrichment_skip_reason(&fixture).is_none());
    assert_eq!(
        shape, "enabled",
        "a non-adaptive model must keep the legacy thinking shape"
    );
}

/// The narrowed predicate still catches what it must: the DeepSeek family
/// depends on a `history_reasoning` value no fixture field pins, and the
/// skip reason names that missing overlay.
#[test]
fn deepseek_model_still_skips_naming_the_missing_history_reasoning_overlay() {
    // Arrange
    let tmp = tempdir().unwrap();
    let fixture = write_fixture_dir(tmp.path(), "ds", "anthropic", "deepseek-v4");

    // Act
    let skip = enrichment_skip_reason(&fixture);

    // Assert
    let reason = skip.expect("deepseek must still skip");
    assert!(
        reason.contains("history_reasoning"),
        "skip reason must name the missing overlay: {reason}"
    );
    assert!(
        reason.contains("deepseek-v4"),
        "skip reason must name the model: {reason}"
    );
}

/// Paired control for the narrowing: `opus-4` as a bare substring used to
/// catch every Anthropic Opus-4 capture. The residual set must no longer
/// mention it, and a 4.5-generation Opus capture must not skip.
#[test]
fn the_residual_set_no_longer_catches_the_whole_opus_4_family() {
    // Arrange
    let tmp = tempdir().unwrap();
    let fixture = write_fixture_dir(tmp.path(), "opus45", "anthropic", "claude-opus-4-5");

    // Act
    let skip = enrichment_skip_reason(&fixture);

    // Assert
    assert!(
        !ENRICHMENT_DEPENDENT_MODELS.contains(&"opus-4"),
        "the residual set must not carry the broad opus-4 substring"
    );
    assert!(
        skip.is_none(),
        "a pre-adaptive Opus capture must replay, got: {skip:?}"
    );
}

/// A fixture with no `meta.model` at all carries no enrichment signal and
/// no reason to skip -- the filter keys on the model, so absence is not a
/// match.
#[test]
fn fixture_without_a_pinned_model_does_not_skip() {
    // Arrange
    let tmp = tempdir().unwrap();
    let mut fixture = write_fixture_dir(tmp.path(), "src", "anthropic", "claude-sonnet-4-5");
    fixture.meta.model = None;

    // Act
    let skip = enrichment_skip_reason(&fixture);

    // Assert
    assert!(skip.is_none(), "absent model must not match: {skip:?}");
}

// ---------------------------------------------------------------------------
// Ingress dispatch
// ---------------------------------------------------------------------------

/// Each token the capture rig can write resolves to the adapter whose
/// `id()` produced it -- the lookup is an identity round-trip, so a
/// vocabulary drift on either side shows up here.
#[test]
fn each_known_ingress_kind_resolves_to_the_adapter_that_named_it() {
    for token in ["anthropic", "openai", "openai-responses"] {
        // Act
        let resolved = ingress_for_kind(token)
            .unwrap_or_else(|e| panic!("known token `{token}` must resolve: {e}"))
            .unwrap_or_else(|| panic!("known token `{token}` must not be treated as unpinned"));

        // Assert
        assert_eq!(
            resolved.id(),
            token,
            "ingress_kind `{token}` resolved to adapter `{}`",
            resolved.id()
        );
    }
}

/// Fail closed on a value outside the vocabulary, naming it, rather than
/// falling back to a default dialect.
#[test]
fn unknown_ingress_kind_errors_naming_the_value() {
    // Act
    let err = match ingress_for_kind("gemini") {
        Err(e) => e,
        Ok(resolved) => panic!(
            "unknown token must fail closed, resolved to `{:?}`",
            resolved.map(IngressAdapter::id)
        ),
    };

    // Assert
    assert!(
        err.contains("gemini"),
        "error must name the offending value: {err}"
    );
}

/// The empty value is the meta contract's "unpinned", NOT an unknown
/// token: it yields `Ok(None)` so the caller skips the individual fixture
/// with a reason, and is never silently defaulted to `anthropic`.
#[test]
fn empty_ingress_kind_is_unpinned_rather_than_an_error_or_a_default() {
    // Act
    let resolved = ingress_for_kind("").expect("empty is unpinned, not an error");

    // Assert
    assert!(
        resolved.is_none(),
        "empty ingress_kind must not resolve to any adapter"
    );
}

/// End-to-end on the empty case: a fixture that pins no ingress dialect
/// produces no canonical request, so the driver skips it.
#[test]
fn fixture_with_empty_ingress_kind_yields_no_canonical_request() {
    // Arrange
    let tmp = tempdir().unwrap();
    let unpinned = write_fixture_dir(tmp.path(), "unpinned", "", "claude-sonnet-4-5");
    let pinned = write_fixture_dir(tmp.path(), "pinned", "anthropic", "claude-sonnet-4-5");

    // Act
    let unpinned_result = parse_fixture_canonical(&unpinned).expect("empty is not an error");
    let pinned_result = parse_fixture_canonical(&pinned).expect("anthropic resolves");

    // Assert
    assert!(unpinned_result.is_none());
    assert!(
        pinned_result.is_some(),
        "control: a pinned fixture must produce a canonical request"
    );
}

/// A fixture carrying an out-of-vocabulary `ingress_kind` fails the
/// driver rather than being replayed through a guessed dialect.
#[test]
fn fixture_with_unknown_ingress_kind_fails_the_parse_naming_the_value() {
    // Arrange
    let tmp = tempdir().unwrap();
    let fixture = write_fixture_dir(tmp.path(), "bogus", "bedrock-ingress", "claude-sonnet-4-5");

    // Act
    let err = parse_fixture_canonical(&fixture).expect_err("unknown kind must fail closed");

    // Assert
    assert!(
        err.contains("bedrock-ingress"),
        "error must name the offending value: {err}"
    );
}

// ---------------------------------------------------------------------------
// Bounded failure reporting
// ---------------------------------------------------------------------------

/// A realistic captured prompt: long, and carrying the kinds of content a
/// real corpus holds (third-party source, an address). Fake values, but
/// shaped like the ones that leaked.
fn long_sensitive_body() -> String {
    let mut s = String::from("export let count = 0; contact: nobody@example.invalid; ");
    while s.len() < 4096 {
        s.push_str("function handler(evt) { return evt.detail; } ");
    }
    s
}

/// The leak this closes: a body mismatch used to render both sides in
/// full, dumping an entire captured prompt into the test log. The summary
/// must stay small and must not carry the body text.
#[test]
fn body_diff_summary_omits_the_prompt_text_and_stays_bounded() {
    // Arrange
    let secret = long_sensitive_body();
    let actual = json!({"messages": [{"role": "user", "content": secret.clone()}]});
    let expected = json!({"messages": [{"role": "user", "content": "something else"}]});

    // Act
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies must report");

    // Assert
    assert!(
        !summary.contains("export let count"),
        "summary leaked prompt text: {summary}"
    );
    assert!(
        !summary.contains("nobody@example.invalid"),
        "summary leaked an address from the prompt: {summary}"
    );
    assert!(
        !summary.contains(&secret),
        "summary contained the whole captured body"
    );
    assert!(
        summary.len() < 512,
        "summary must stay bounded, got {} bytes: {summary}",
        summary.len()
    );
}

/// Positive control for the assertion above: the bound must not have been
/// achieved by reporting nothing useful. The path and the kind -- the
/// actual diagnostic -- survive in full.
#[test]
fn body_diff_summary_keeps_the_full_path_and_kind() {
    // Arrange
    let actual = json!({"messages": [{"role": "user", "content": long_sensitive_body()}]});
    let expected = json!({"messages": [{"role": "user", "content": "other"}]});

    // Act
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies report");

    // Assert
    assert!(
        summary.contains("messages[0].content"),
        "summary must keep the full divergence path: {summary}"
    );
    assert!(
        summary.contains("value mismatch"),
        "summary must keep the divergence kind: {summary}"
    );
    assert!(
        summary.contains("len="),
        "summary must characterize the value by size: {summary}"
    );
}

/// Identical bodies produce no summary at all -- the reporter must not
/// manufacture a failure, or every fixture would "fail" bounded.
#[test]
fn body_diff_returns_none_for_structurally_equal_bodies() {
    // Arrange
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "x"}]});

    // Act + Assert
    assert!(bounded_body_diff(&body, &body.clone(), &[]).is_none());
}

/// A short scalar is still shown: bounding must not blind the common case
/// where the value IS the diagnostic (an alias-vs-upstream model string).
#[test]
fn body_diff_shows_short_scalar_values_in_full() {
    // Arrange
    let actual = json!({"model": "sonnet"});
    let expected = json!({"model": "claude-sonnet-4-5"});

    // Act
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies report");

    // Assert
    assert!(
        summary.contains("sonnet") && summary.contains("claude-sonnet-4-5"),
        "a short scalar must remain visible: {summary}"
    );
}

/// On an allowlisted identifier path the length cap is the only gate: an
/// over-cap value must report length only with NO prefix, while a short
/// identifier on the same path (control) still renders verbatim, so the
/// elision follows the cap rather than the path. A raw `Divergence`'s
/// `Debug` is held to the same cap, while `render_verbose` (control) still
/// renders the value in full.
#[test]
fn body_diff_elides_an_over_cap_identifier_with_no_prefix_and_keeps_a_short_one() {
    // Arrange: the over-cap value's distinguishing lead is inside the
    // first 48 chars, so any prefix-style cap would echo it.
    let over_cap = format!("LEADING-MARKER-{}", "y".repeat(200));
    let over = json!({"model": over_cap});
    let short = json!({"model": "sonnet"});
    let other = json!({"model": "claude-sonnet-4-5"});

    // Act
    let elided = bounded_body_diff(&over, &other, &[]).expect("differing bodies report");
    let control = bounded_body_diff(&short, &other, &[]).expect("differing bodies report");
    let divergence = diff_all(&over, &other, &[]).remove(0);
    let debugged = format!("{divergence:?}");
    let verbose = divergence.render_verbose();

    // Assert
    assert!(elided.contains("model"), "path preserved: {elided}");
    assert!(
        elided.contains("value mismatch"),
        "kind preserved: {elided}"
    );
    assert!(
        elided.contains("elided"),
        "over-cap value not marked: {elided}"
    );
    assert!(
        !elided.contains("LEADING-MARKER"),
        "a prefix of the over-cap value leaked: {elided}"
    );
    assert!(
        control.contains("\"sonnet\"") && control.contains("claude-sonnet-4-5"),
        "a short identifier must render in full: {control}"
    );
    assert!(
        debugged.contains("elided") && !debugged.contains("LEADING-MARKER"),
        "Debug must summarize an over-cap value, not print it: {debugged}"
    );
    assert!(
        debugged.len() < 200,
        "Debug must stay bounded on a large value ({} bytes): {debugged}",
        debugged.len()
    );
    assert!(
        verbose.contains(&over_cap),
        "control: render_verbose is the full-value path and must print it"
    );
}

/// A large divergence set reports its true count while enumerating only
/// the leading few, so a body-wide misalignment cannot produce an
/// unbounded message.
#[test]
fn body_diff_caps_the_number_of_enumerated_divergences_but_reports_the_count() {
    // Arrange: 40 differing sibling keys.
    let mut a = serde_json::Map::new();
    let mut e = serde_json::Map::new();
    for i in 0..40 {
        a.insert(format!("k{i}"), json!(format!("actual-{i}")));
        e.insert(format!("k{i}"), json!(format!("expected-{i}")));
    }

    // Act
    let summary = bounded_body_diff(&Value::Object(a), &Value::Object(e), &[])
        .expect("differing bodies report");

    // Assert
    assert!(
        summary.contains("40 divergence(s)"),
        "true count must be reported: {summary}"
    );
    assert!(
        summary.contains("first 5 shown"),
        "the cap must be disclosed: {summary}"
    );
    assert!(summary.len() < 1024, "capped summary stayed bounded");
}

// ---------------------------------------------------------------------------
// System-turn lift skip
// ---------------------------------------------------------------------------

/// A fixture whose only divergences sit inside `messages[]` is the
/// system-turn lift's positional shift, which has no normalizer yet.
#[test]
fn divergences_confined_to_messages_are_recognized_as_the_lift() {
    // Arrange: a removed middle turn shifts every later index.
    let actual = json!({"model": "m", "messages": [{"role": "user", "content": "a"}]});
    let expected = json!({
        "model": "m",
        "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "a"}],
    });

    // Act + Assert
    assert!(diverges_only_in_messages(&actual, &expected, &[]));
}

/// Paired control, and the load-bearing one: a divergence OUTSIDE
/// `messages[]` must NOT be classified as the lift, even when the fixture
/// is also lift-affected -- otherwise the skip would swallow real
/// regressions like the `model` alias rewrite.
#[test]
fn a_divergence_outside_messages_is_not_classified_as_the_lift() {
    // Arrange: same lift shift PLUS a model mismatch.
    let actual = json!({"model": "sonnet", "messages": [{"role": "user", "content": "a"}]});
    let expected = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "a"}],
    });

    // Act + Assert
    assert!(
        !diverges_only_in_messages(&actual, &expected, &[]),
        "a non-messages divergence must keep the fixture failing"
    );
}

/// Structurally equal bodies are not "lift-affected" -- otherwise a
/// passing fixture would be reclassified as skipped.
#[test]
fn equal_bodies_are_not_classified_as_the_lift() {
    let body = json!({"messages": [{"role": "user", "content": "a"}]});
    assert!(!diverges_only_in_messages(&body, &body.clone(), &[]));
}

/// The skip reason must name the missing prerequisite, so the skip reads
/// as a blocked assertion rather than a pass.
#[test]
fn lift_skip_reason_names_the_missing_normalizer_and_the_count() {
    // Act
    let reason = system_turn_lift_skip_reason(7);

    // Assert
    assert!(reason.contains("normalizer"), "must name it: {reason}");
    assert!(
        reason.contains("system-turn lift"),
        "must name the transform"
    );
    assert!(reason.contains('7'), "must carry the count: {reason}");
}

/// The gap the first cap attempt had: truncating to a prefix still
/// disclosed the leading bytes of a prompt, which is exactly the system
/// preamble. A long prompt value must report LENGTH ONLY -- no prefix.
#[test]
fn body_diff_never_emits_a_prefix_of_a_long_prompt_value() {
    // Arrange: the distinguishing text sits in the first 48 chars, so a
    // prefix-style cap would have leaked it.
    let secret = format!("SECRET-PREAMBLE-DO-NOT-LOG {}", long_sensitive_body());
    let actual = json!({"messages": [{"role": "user", "content": secret}]});
    let expected = json!({"messages": [{"role": "user", "content": "x"}]});

    // Act
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies report");

    // Assert
    assert!(
        !summary.contains("SECRET-PREAMBLE"),
        "a prefix of the prompt leaked: {summary}"
    );
    assert!(
        summary.contains("elided"),
        "an over-cap value must be marked elided: {summary}"
    );
}

/// A SHORT prompt value is still content: length is the wrong gate on a
/// prose-bearing path, so it is elided regardless of size. Paired with
/// `body_diff_shows_short_scalar_values_in_full`, which proves a short
/// value on an ALLOWLISTED path (`model`) does still print -- together
/// they pin that the redaction follows the leaf field, not just length.
#[test]
fn body_diff_elides_a_short_value_on_a_prompt_bearing_path() {
    // Arrange
    let actual = json!({"messages": [{"role": "user", "content": "hi bob@x.invalid"}]});
    let expected = json!({"messages": [{"role": "user", "content": "other"}]});

    // Act
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies report");

    // Assert
    assert!(
        !summary.contains("bob@x.invalid"),
        "a short prompt value still leaked: {summary}"
    );
    assert!(summary.contains("messages[0].content"), "path preserved");
}

/// The allowlist must fail CLOSED. An unenumerated leaf field (`note`
/// here) carries unknown content, and a captured body sweeps
/// forward-compat keys nobody listed, so anything off the allowlist is
/// elided rather than printed.
#[test]
fn body_diff_never_renders_container_contents() {
    // Arrange
    let actual = json!({"metadata": {"note": "sensitive-inner-value"}});
    let expected = json!({"metadata": {"other": 1}});

    // Act
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies report");

    // Assert
    assert!(
        !summary.contains("sensitive-inner-value"),
        "container contents leaked: {summary}"
    );
}

/// The skip reason reports a real count, so it must come from the same
/// walk the classifier used rather than a recomputed guess.
#[test]
fn divergence_count_matches_the_number_of_reported_divergences() {
    // Arrange: three differing sibling keys.
    let actual = json!({"a": 1, "b": 2, "c": 3});
    let expected = json!({"a": 9, "b": 8, "c": 7});

    // Act
    let count = divergence_count(&actual, &expected, &[]);
    let summary = bounded_body_diff(&actual, &expected, &[]).expect("differing bodies report");

    // Assert
    assert_eq!(count, 3);
    assert!(
        summary.contains("3 divergence(s)"),
        "the reported count must agree with divergence_count: {summary}"
    );
}

/// An ignored path must not inflate the count the skip reason quotes.
#[test]
fn divergence_count_honors_ignore_paths() {
    // Arrange
    let actual = json!({"a": 1, "stream": true});
    let expected = json!({"a": 9, "stream": false});

    // Act + Assert
    assert_eq!(divergence_count(&actual, &expected, &["stream"]), 1);
    assert_eq!(divergence_count(&actual, &expected, &[]), 2);
}
