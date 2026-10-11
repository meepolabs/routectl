//! Each body edit the anthropic-api assembly and cloak make is declared on
//! the attempt's wire tap at the site that makes it, with the pointer the
//! edit shows at in the sent body. One row per site: with the transform
//! firing the tap holds that site's declaration; with it idle, none of that
//! site's.

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, Leg, WireTap};
use routectl_core::{ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Role};
use serde_json::{Value, json};

use super::super::cloak::{ClaudeCodeIdentity, CloakConfig, ToolRename, cloak_oauth_egress};
use super::super::request;

fn tapped(mut req: ChatRequest) -> (ChatRequest, WireTap) {
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    (req, tap)
}

fn request_from(body: Value) -> ChatRequest {
    serde_json::from_value(body).expect("fixture is a canonical request")
}

fn user_turn() -> Value {
    json!({"role": "user", "content": "hi"})
}

fn base(extra: Value) -> ChatRequest {
    let mut body = json!({"model": "claude-x", "max_tokens": 4096, "messages": [user_turn()]});
    if let (Some(obj), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        obj.extend(extra.clone());
    }
    request_from(body)
}

/// Assemble the Anthropic body through the shared normalizer with a tap.
fn assemble(req: ChatRequest) -> (Value, Vec<DeclaredEdit>) {
    let (req, tap) = tapped(req);
    let body = request::normalize("wire-edits-test", &req, true, false, None, true, true)
        .expect("fixture assembles");
    (body, tap.take_request().edits(Leg::Request).to_vec())
}

fn edits_of(edits: &[DeclaredEdit], site: EditSite) -> Vec<(EditKind, String)> {
    edits
        .iter()
        .filter(|edit| edit.site() == site)
        .map(|edit| (edit.kind(), edit.path().to_string()))
        .collect()
}

fn adaptive(effort: &str) -> Value {
    json!({"reasoning": {"effort": effort}})
}

fn thinking_part(signature: Option<&str>) -> Value {
    let mut part = json!({"type": "thinking", "thinking": "mull"});
    if let Some(sig) = signature
        && let Some(obj) = part.as_object_mut()
    {
        obj.insert("signature".into(), json!(sig));
    }
    part
}

fn with_assistant_parts(parts: Value) -> ChatRequest {
    let mut req = base(json!({}));
    let assistant: Message = serde_json::from_value(json!({
        "role": "assistant",
        "content": parts,
    }))
    .expect("assistant turn");
    let mut messages: Vec<Message> = req.messages.to_vec();
    messages.push(assistant);
    messages.push(serde_json::from_value(user_turn()).expect("user turn"));
    req.messages = messages.into();
    req
}

fn tool_exchange(id: &str) -> ChatRequest {
    let mut req = base(json!({}));
    let call: Message = serde_json::from_value(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{"id": id, "type": "function", "function": {"name": "f", "arguments": "{}"}}],
    }))
    .expect("assistant tool call");
    let result = Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text("ok".into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: Some(id.into()),
        tool_calls: None,
    };
    let mut messages: Vec<Message> = req.messages.to_vec();
    messages.push(call);
    messages.push(result);
    req.messages = messages.into();
    req
}

struct AssemblyRow {
    name: &'static str,
    site: EditSite,
    firing: fn() -> ChatRequest,
    idle: fn() -> ChatRequest,
    expected: &'static [(EditKind, &'static str)],
}

const ASSEMBLY_ROWS: &[AssemblyRow] = &[
    AssemblyRow {
        name: "thinking strip when tool_choice forces tool use",
        site: EditSite::ThinkingStripForcedToolUse,
        firing: || {
            let mut req = base(json!({
                "reasoning": {"effort": "high"},
                "tools": [{"name": "f", "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "tool", "name": "f"},
            }));
            req.routectl_internal.supports_adaptive_thinking = true;
            req
        },
        idle: || {
            base(json!({
                "reasoning": {"effort": "high"},
                "tools": [{"name": "f", "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "auto"},
            }))
        },
        expected: &[(EditKind::Remove, "/thinking")],
    },
    AssemblyRow {
        name: "sampling reconcile restores the caller temperature after a thinking strip",
        site: EditSite::SamplingReconcile,
        firing: || {
            base(json!({
                "temperature": 0.3,
                "reasoning": {"effort": "high"},
                "tools": [{"name": "f", "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "any"},
            }))
        },
        idle: || {
            base(json!({
                "temperature": 0.3,
                "reasoning": {"effort": "high"},
            }))
        },
        expected: &[(EditKind::Rewrite, "/temperature")],
    },
    AssemblyRow {
        name: "thinking clamp pins temperature",
        site: EditSite::ThinkingSamplingClamp,
        firing: || base(json!({"temperature": 0.3, "reasoning": {"effort": "high"}})),
        idle: || base(json!({"temperature": 0.3})),
        expected: &[(EditKind::Rewrite, "/temperature")],
    },
    AssemblyRow {
        name: "effort injected beside adaptive thinking",
        site: EditSite::EffortInjection,
        firing: || {
            let mut req = base(adaptive("high"));
            req.routectl_internal.supports_adaptive_thinking = true;
            req
        },
        idle: || base(json!({})),
        expected: &[(EditKind::Add, "/output_config")],
    },
    AssemblyRow {
        name: "tool-use id rewritten to the wire charset",
        site: EditSite::ToolIdSanitize,
        firing: || tool_exchange("call.a:1"),
        idle: || tool_exchange("call_a1"),
        expected: &[
            (EditKind::Rewrite, "/messages/*/content/*/id"),
            (EditKind::Rewrite, "/messages/*/content/*/tool_use_id"),
        ],
    },
    AssemblyRow {
        name: "unsigned thinking block stripped",
        site: EditSite::SignatureStrip,
        firing: || {
            with_assistant_parts(json!([thinking_part(None), {"type": "text", "text": "a"}]))
        },
        idle: || with_assistant_parts(json!([{"type": "text", "text": "a"}])),
        expected: &[(EditKind::Remove, "/messages/1/content/0")],
    },
    AssemblyRow {
        name: "hosted tool withheld at its concrete index",
        site: EditSite::HostedToolWithhold,
        firing: || {
            base(json!({"tools": [
                {"name": "f", "input_schema": {"type": "object"}},
                {"type": "mcp", "server_label": "s", "server_url": "https://example.com"},
            ]}))
        },
        idle: || base(json!({"tools": [{"name": "f", "input_schema": {"type": "object"}}]})),
        expected: &[(EditKind::Remove, "/tools/1")],
    },
    AssemblyRow {
        name: "client billing block withheld from system",
        site: EditSite::ClientFingerprintStrip,
        firing: || {
            base(json!({"system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=1; cch=abcde;"},
                {"type": "text", "text": "rules"},
            ]}))
        },
        idle: || base(json!({"system": [{"type": "text", "text": "rules"}]})),
        expected: &[(EditKind::Remove, "/system/0")],
    },
];

#[test]
#[serial_test::serial(
    anthropic_api_client_fingerprint_stripped,
    anthropic_hosted_mcp_tool_withheld
)]
fn each_assembly_site_declares_its_edit_only_when_it_fires() {
    for row in ASSEMBLY_ROWS {
        let (_, fired) = assemble((row.firing)());
        let (_, idle) = assemble((row.idle)());

        let want: Vec<(EditKind, String)> = row
            .expected
            .iter()
            .map(|(kind, path)| (*kind, (*path).to_string()))
            .collect();
        assert_eq!(edits_of(&fired, row.site), want, "{}: firing", row.name);
        assert!(edits_of(&idle, row.site).is_empty(), "{}: idle", row.name);
    }
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn the_hosted_tool_withhold_never_declares_the_tools_container() {
    let req = base(json!({"tools": [
        {"type": "mcp", "server_label": "a", "server_url": "https://example.com"},
        {"name": "f", "input_schema": {"type": "object"}},
        {"type": "mcp", "server_label": "b", "server_url": "https://example.com"},
    ]}));

    let (_, edits) = assemble(req);

    assert_eq!(
        edits_of(&edits, EditSite::HostedToolWithhold),
        [
            (EditKind::Remove, "/tools/0".to_string()),
            (EditKind::Remove, "/tools/2".to_string()),
        ]
    );
}

#[test]
fn an_untapped_assembly_produces_the_same_body() {
    let req = base(json!({
        "temperature": 0.3,
        "reasoning": {"effort": "high"},
        "tools": [{"name": "f", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"},
    }));

    let untapped = request::normalize("wire-edits-test", &req, true, false, None, true, true)
        .expect("assembles");
    let (tapped_body, _) = assemble(req);

    assert_eq!(untapped, tapped_body);
}

fn identity() -> ClaudeCodeIdentity {
    ClaudeCodeIdentity {
        session_id: "sess".into(),
        device_id: "d".repeat(64),
        account_uuid: "11111111-2222-3333-4444-555555555555".into(),
    }
}

fn cloak(body: &mut Value, req: &ChatRequest, is_non_cc: bool, config: &CloakConfig) {
    cloak_oauth_egress(body, req, &identity(), is_non_cc, config).expect("cloak applies");
}

fn cloak_edits(mut body: Value, is_non_cc: bool, config: &CloakConfig) -> Vec<(EditKind, String)> {
    let (req, tap) = tapped(ChatRequest::default());
    cloak(&mut body, &req, is_non_cc, config);
    edits_of(tap.take_request().edits(Leg::Request), EditSite::Cloak)
}

fn owned(rows: &[(EditKind, &str)]) -> Vec<(EditKind, String)> {
    rows.iter()
        .map(|(kind, path)| (*kind, (*path).to_string()))
        .collect()
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_billing_strip_declares_the_concrete_block() {
    let firing = json!({
        "system": [{"type": "text", "text": "x-anthropic-billing-header: v=1;"}, {"type": "text", "text": "r"}],
        "messages": [user_turn()],
    });
    let idle = json!({"system": [{"type": "text", "text": "r"}], "messages": [user_turn()]});

    let fired = cloak_edits(firing, false, &CloakConfig::default());
    let quiet = cloak_edits(idle, false, &CloakConfig::default());

    assert_eq!(fired, owned(&[(EditKind::Remove, "/system/0")]));
    assert!(quiet.is_empty(), "{quiet:?}");
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_relocation_declares_the_system_swap_and_the_landing() {
    let body = json!({
        "system": [{"type": "text", "text": "rules"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    });

    let edits = cloak_edits(body, true, &CloakConfig::default());

    assert_eq!(
        edits,
        owned(&[
            (EditKind::Remove, "/system/0"),
            (EditKind::Add, "/system/0"),
            (EditKind::Add, "/messages/0/content/0"),
            (EditKind::Add, "/metadata"),
        ])
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_relocation_is_silent_for_a_genuine_client() {
    let body = json!({
        "system": [{"type": "text", "text": "rules"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    });

    assert!(cloak_edits(body, false, &CloakConfig::default()).is_empty());
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_user_id_mint_declares_only_a_minted_value() {
    let fresh = json!({"metadata": {}, "messages": [user_turn()]});
    let set = json!({"metadata": {"user_id": "caller"}, "messages": [user_turn()]});

    assert_eq!(
        cloak_edits(fresh, true, &CloakConfig::default()),
        owned(&[(EditKind::Add, "/metadata/user_id")])
    );
    assert!(cloak_edits(set, true, &CloakConfig::default()).is_empty());
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_tool_rename_declares_each_renamed_name() {
    let firing = json!({
        "tools": [{"name": "mcp__kept"}, {"name": "renamed"}],
        "tool_choice": {"type": "tool", "name": "renamed"},
        "messages": [user_turn()],
    });
    let idle = json!({"tools": [{"name": "mcp__kept"}], "messages": [user_turn()]});

    let fired = cloak_edits(firing, false, &CloakConfig::default());
    let quiet = cloak_edits(idle, false, &CloakConfig::default());

    assert_eq!(
        fired,
        owned(&[
            (EditKind::Rewrite, "/tools/1/name"),
            (EditKind::Rewrite, "/tool_choice/name"),
            (EditKind::Rewrite, "/messages/*/content/*/name"),
            (EditKind::Rewrite, "/messages/*/content/*/tool_name"),
            (
                EditKind::Rewrite,
                "/messages/*/content/*/content/*/tool_name"
            ),
        ])
    );
    assert!(quiet.is_empty(), "{quiet:?}");
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_operator_tool_rename_declares_its_rename() {
    let config = CloakConfig {
        tool_rename: vec![ToolRename {
            from: "mcp__a".into(),
            to: "mcp__b".into(),
        }],
        ..CloakConfig::default()
    };
    let body = json!({"tools": [{"name": "mcp__a"}], "messages": [user_turn()]});

    let edits = cloak_edits(body, false, &config);

    assert!(
        edits.contains(&(EditKind::Rewrite, "/tools/0/name".to_string())),
        "{edits:?}"
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_obfuscation_declares_the_rewritten_surfaces() {
    let config = CloakConfig {
        sensitive_words: vec!["secret".into()],
        ..CloakConfig::default()
    };
    let firing = json!({"messages": [{"role": "user", "content": "a secret"}]});
    let idle = json!({"messages": [{"role": "user", "content": "plain"}]});

    let fired = cloak_edits(firing, false, &config);
    let quiet = cloak_edits(idle, false, &config);

    assert!(
        fired.contains(&(EditKind::Rewrite, "/messages/*/content".to_string())),
        "{fired:?}"
    );
    assert!(quiet.is_empty(), "{quiet:?}");
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn the_cloak_tool_sort_declares_each_moved_position() {
    let firing = json!({
        "tools": [{"name": "mcp__b"}, {"name": "mcp__a"}, {"name": "mcp__c"}],
        "messages": [user_turn()],
    });
    let idle = json!({
        "tools": [{"name": "mcp__a"}, {"name": "mcp__b"}],
        "messages": [user_turn()],
    });

    let fired = cloak_edits(firing, true, &CloakConfig::default());
    let quiet = cloak_edits(idle, true, &CloakConfig::default());

    let relocations: Vec<_> = fired
        .into_iter()
        .filter(|(kind, _)| *kind == EditKind::Relocate)
        .collect();
    assert_eq!(
        relocations,
        owned(&[
            (EditKind::Relocate, "/tools/0"),
            (EditKind::Relocate, "/tools/1")
        ])
    );
    assert!(
        !quiet.iter().any(|(kind, _)| *kind == EditKind::Relocate),
        "{quiet:?}"
    );
}

#[test]
#[serial_test::serial(anthropic_api_cloak_policy_actions)]
fn an_untapped_cloak_declares_nothing_and_changes_nothing_extra() {
    let body = json!({
        "system": [{"type": "text", "text": "rules"}],
        "tools": [{"name": "b"}, {"name": "a"}],
        "messages": [{"role": "user", "content": "hi"}],
    });
    let mut untapped = body.clone();
    let mut with_tap = body;
    let (tapped_req, _tap) = tapped(ChatRequest::default());

    cloak(
        &mut untapped,
        &ChatRequest::default(),
        true,
        &CloakConfig::default(),
    );
    cloak(&mut with_tap, &tapped_req, true, &CloakConfig::default());

    let strip_user_id = |mut v: Value| {
        if let Some(metadata) = v.get_mut("metadata").and_then(Value::as_object_mut) {
            metadata.remove("user_id");
        }
        v
    };
    assert_eq!(strip_user_id(untapped), strip_user_id(with_tap));
}

#[test]
fn a_refused_pattern_is_dropped_without_failing() {
    let tap = WireTap::default();

    super::declare(Some(&tap), EditSite::Cloak, "/tools", EditKind::Remove);

    assert!(tap.take_request().edits(Leg::Request).is_empty());
}

#[test]
fn the_part_predicate_marks_only_unsigned_thinking() {
    let unsigned = ContentPart::Known(KnownContentPart::Thinking {
        thinking: "t".into(),
        signature: None,
    });
    let tap = WireTap::default();

    super::declare_unsigned_thinking_strip(Some(&tap), 3, &[unsigned], |_| true);

    assert_eq!(
        edits_of(
            tap.take_request().edits(Leg::Request),
            EditSite::SignatureStrip
        ),
        owned(&[(EditKind::Remove, "/messages/3/content/0")])
    );
}
