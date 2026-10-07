//! Unit tests for the named beta-rejection parser, repair gate, and the
//! withheld-set filter's floor exemption. The envelope strings are the captured bedrock-runtime 400 messages,
//! byte for byte.

use super::*;
use serde_json::json;

/// Converse, one flag.
pub(super) const CONVERSE_ONE_FLAG: &str = "The model returned the following errors: Unexpected value(s) `advanced-tool-use-2025-11-20` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
/// Invoke, three rejected flags among four sent; the accepted one is omitted.
pub(super) const INVOKE_MIXED: &str = "Unexpected value(s) `advanced-tool-use-2025-11-20`, `prompt-caching-scope-2026-01-05`, `zz-probe-2099-01-01` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
/// Invoke, a flag no Anthropic release ever issued.
pub(super) const INVOKE_NEVER_ISSUED: &str = "Unexpected value(s) `zz-probe-2099-01-01` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
/// Invoke, the three flags a stock agent client sent with no allowlist.
const INVOKE_CLIENT_TRIO: &str = "Unexpected value(s) `advanced-tool-use-2025-11-20`, `advisor-tool-2026-03-01`, `prompt-caching-scope-2026-01-05` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
/// The older-model answer to the same flags: it names nothing.
pub(super) const NON_NAMING: &str = "invalid beta flag";

const PROVIDER: &str = "bedrock-beta-repair-unit";
const CARRIER: &str = "bedrock-invoke";

fn strings(flags: &[&str]) -> Vec<String> {
    flags.iter().map(|f| (*f).to_string()).collect()
}

fn validation_400(message: &str) -> Error {
    Error::upstream_full(
        PROVIDER,
        400,
        json!({ "message": message }).to_string(),
        None,
        Some("ValidationException".into()),
        None,
    )
}

fn request_with_betas(betas: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-opus-5-5".into(),
        anthropic_beta: strings(betas),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// parse_rejected_beta_envelope
// ---------------------------------------------------------------------------

#[test]
fn parses_the_converse_prefixed_envelope() {
    assert_eq!(
        parse_rejected_beta_envelope(CONVERSE_ONE_FLAG),
        Some(vec!["advanced-tool-use-2025-11-20"])
    );
}

#[test]
fn parses_every_flag_of_a_multi_flag_invoke_envelope_in_order() {
    assert_eq!(
        parse_rejected_beta_envelope(INVOKE_MIXED),
        Some(vec![
            "advanced-tool-use-2025-11-20",
            "prompt-caching-scope-2026-01-05",
            "zz-probe-2099-01-01",
        ])
    );
    assert_eq!(
        parse_rejected_beta_envelope(INVOKE_CLIENT_TRIO),
        Some(vec![
            "advanced-tool-use-2025-11-20",
            "advisor-tool-2026-03-01",
            "prompt-caching-scope-2026-01-05",
        ])
    );
}

#[test]
fn parses_a_never_issued_flag_like_any_other() {
    assert_eq!(
        parse_rejected_beta_envelope(INVOKE_NEVER_ISSUED),
        Some(vec!["zz-probe-2099-01-01"])
    );
}

#[test]
fn the_non_naming_envelope_yields_nothing() {
    assert_eq!(parse_rejected_beta_envelope(NON_NAMING), None);
}

#[test]
fn a_message_that_deviates_from_the_envelope_yields_nothing() {
    let near_misses = [
        // Tail cut after the header sentence.
        "Unexpected value(s) `zz-probe-2099-01-01` for the `anthropic-beta` header.".to_string(),
        // Surrounding whitespace: exact, not trimmed.
        format!(" {INVOKE_NEVER_ISSUED}"),
        format!("{INVOKE_NEVER_ISSUED}\n"),
        // Text before the envelope that is not the Converse prefix.
        format!("Error: {INVOKE_NEVER_ISSUED}"),
        // The Converse prefix applied twice.
        format!("The model returned the following errors: {CONVERSE_ONE_FLAG}"),
        // A different header named.
        INVOKE_NEVER_ISSUED.replace("`anthropic-beta`", "`anthropic-version`"),
        // Case drift in the head.
        INVOKE_NEVER_ISSUED.replace("Unexpected value(s)", "unexpected value(s)"),
        // A trailing sentence appended.
        format!("{INVOKE_NEVER_ISSUED} Request id: 1."),
    ];
    for message in &near_misses {
        assert_eq!(
            parse_rejected_beta_envelope(message),
            None,
            "near-miss must not parse: {message:?}"
        );
    }
}

#[test]
fn a_malformed_flag_list_yields_nothing() {
    let lists = [
        "",
        "``",
        "zz-probe-2099-01-01",
        "`a`,`b`",
        "`a`, ",
        ", `a`",
        "`a`,  `b`",
        "`a`b`",
        "`a` `b`",
        "`two words`",
        "`line\nbreak`",
        "`caf\u{e9}`",
    ];
    for list in lists {
        let message = format!("{ENVELOPE_HEAD}{list}{ENVELOPE_TAIL}");
        assert_eq!(
            parse_rejected_beta_envelope(&message),
            None,
            "malformed list must not parse: {list:?}"
        );
    }
}

#[test]
fn a_flag_over_the_token_length_cap_yields_nothing() {
    let at_cap = "a".repeat(routectl_core::MAX_SAFE_TOKEN_LEN);
    let over_cap = "a".repeat(routectl_core::MAX_SAFE_TOKEN_LEN + 1);

    let accepted = format!("{ENVELOPE_HEAD}`{at_cap}`{ENVELOPE_TAIL}");
    let refused = format!("{ENVELOPE_HEAD}`{at_cap}`, `{over_cap}`{ENVELOPE_TAIL}");

    assert_eq!(
        parse_rejected_beta_envelope(&accepted),
        Some(vec![at_cap.as_str()])
    );
    assert_eq!(parse_rejected_beta_envelope(&refused), None);
}

// ---------------------------------------------------------------------------
// repairable_rejected_betas
// ---------------------------------------------------------------------------

#[test]
fn a_named_client_flag_is_repairable() {
    let err = validation_400(INVOKE_NEVER_ISSUED);
    let client = strings(&["context-management-2025-06-27", "zz-probe-2099-01-01"]);

    let flags = repairable_rejected_betas(PROVIDER, CARRIER, &err, &client, &[]);

    assert_eq!(flags, Some(strings(&["zz-probe-2099-01-01"])));
}

#[test]
fn a_named_flag_the_client_did_not_send_is_not_repairable() {
    let err = validation_400(INVOKE_MIXED);
    // The client sent two of the three named flags.
    let client = strings(&[
        "advanced-tool-use-2025-11-20",
        "prompt-caching-scope-2026-01-05",
    ]);

    let flags = repairable_rejected_betas(PROVIDER, CARRIER, &err, &client, &[]);

    assert_eq!(flags, None);
}

#[test]
fn a_named_operator_floor_flag_is_not_repairable() {
    let err = validation_400(INVOKE_NEVER_ISSUED);
    let client = strings(&["zz-probe-2099-01-01"]);
    let floor = strings(&["zz-probe-2099-01-01"]);

    let flags = repairable_rejected_betas(PROVIDER, CARRIER, &err, &client, &floor);

    assert_eq!(flags, None);
}

#[test]
fn a_rejection_without_the_validation_discriminator_is_not_repairable() {
    let client = strings(&["zz-probe-2099-01-01"]);
    let body = json!({ "message": INVOKE_NEVER_ISSUED }).to_string();
    let errors = [
        Error::upstream_full(PROVIDER, 400, body.clone(), None, None, None),
        Error::upstream_full(
            PROVIDER,
            400,
            body.clone(),
            None,
            Some("ThrottlingException".into()),
            None,
        ),
        Error::upstream_full(
            PROVIDER,
            500,
            body,
            None,
            Some("ValidationException".into()),
            None,
        ),
        Error::Config(INVOKE_NEVER_ISSUED.into()),
    ];
    for err in &errors {
        assert_eq!(
            repairable_rejected_betas(PROVIDER, CARRIER, err, &client, &[]),
            None,
            "must not repair {err:?}"
        );
    }
}

#[test]
fn a_namespaced_validation_discriminator_is_repairable() {
    let err = Error::upstream_full(
        PROVIDER,
        400,
        json!({ "message": INVOKE_NEVER_ISSUED }).to_string(),
        None,
        Some("com.amazon.coral.validate#ValidationException".into()),
        None,
    );
    let client = strings(&["zz-probe-2099-01-01"]);

    let flags = repairable_rejected_betas(PROVIDER, CARRIER, &err, &client, &[]);

    assert_eq!(flags, Some(strings(&["zz-probe-2099-01-01"])));
}

#[test]
fn a_body_that_is_not_the_flat_envelope_is_not_repairable() {
    let client = strings(&["zz-probe-2099-01-01"]);
    let bodies = [
        INVOKE_NEVER_ISSUED.to_string(),
        json!({ "error": { "message": INVOKE_NEVER_ISSUED } }).to_string(),
        json!({ "message": [INVOKE_NEVER_ISSUED] }).to_string(),
    ];
    for body in bodies {
        let err = Error::upstream_full(
            PROVIDER,
            400,
            body.clone(),
            None,
            Some("ValidationException".into()),
            None,
        );
        assert_eq!(
            repairable_rejected_betas(PROVIDER, CARRIER, &err, &client, &[]),
            None,
            "must not repair body {body:?}"
        );
    }
}

#[test]
fn a_flag_named_twice_is_stripped_once() {
    let message = format!("{ENVELOPE_HEAD}`zz-a`, `zz-a`, `zz-b`{ENVELOPE_TAIL}");
    let err = validation_400(&message);
    let client = strings(&["zz-a", "zz-b"]);

    let flags = repairable_rejected_betas(PROVIDER, CARRIER, &err, &client, &[]);

    assert_eq!(flags, Some(strings(&["zz-a", "zz-b"])));
}

// ---------------------------------------------------------------------------
// without_client_betas
// ---------------------------------------------------------------------------

#[test]
fn stripping_removes_exactly_the_named_flags_and_keeps_order() {
    let req = request_with_betas(&["keep-1", "zz-a", "keep-2", "zz-b"]);

    let stripped = without_client_betas(req, &strings(&["zz-a", "zz-b"]));

    assert_eq!(stripped.anthropic_beta, strings(&["keep-1", "keep-2"]));
    assert_eq!(stripped.model, "anthropic.claude-opus-5-5");
}

// ---------------------------------------------------------------------------
// Withheld set vs the operator floor
// ---------------------------------------------------------------------------

/// The bag's `anthropic_beta` after the shared filter in pass-through mode.
fn filtered(client: &[&str], withheld: &[&str], floor: &[&str]) -> (Option<Value>, bool) {
    let mut bag = serde_json::Map::new();
    bag.insert("anthropic_beta".into(), json!(client));
    let dropped = super::super::betas::filter_bedrock_betas(
        PROVIDER,
        &mut bag,
        &[],
        &strings(floor),
        &strings(withheld),
        &[],
    );
    (bag.remove("anthropic_beta"), dropped)
}

#[test]
fn a_withheld_flag_is_dropped_and_signalled() {
    let out = filtered(&["keep-1", "zz-a", "keep-2"], &["zz-a"], &[]);

    assert_eq!(out, (Some(json!(["keep-1", "keep-2"])), true));
}

#[test]
fn a_withheld_flag_the_operator_floor_asserts_is_kept() {
    let out = filtered(&["keep-1", "zz-a"], &["zz-a"], &["zz-a"]);

    assert_eq!(out, (Some(json!(["keep-1", "zz-a"])), false));
}
