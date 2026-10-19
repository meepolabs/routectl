//! Each body edit the anthropic-api assembly and cloak make is declared on
//! the attempt's wire tap at the site that makes it, with the pointer the
//! edit shows at in the sent body. One row per site: with the transform
//! firing the tap holds that site's declaration; with it idle, none of that
//! site's.

use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, Leg, WireTap};
use routectl_core::{
    AnthropicThinking, ChatRequest, ContentPart, KnownContentPart, Message, ThinkingObject,
};
use serde_json::{Value, json};

use super::super::cloak::{ClaudeCodeIdentity, CloakConfig, ToolRename, cloak_oauth_egress};
use super::super::request;

fn tapped(mut req: ChatRequest) -> (ChatRequest, WireTap) {
    let tap = WireTap::admitted();
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

/// An assistant turn carrying a text block then one `tool_use` block per
/// id, answered by one `tool_result` block per id.
fn tool_use_exchange(ids: &[&str]) -> ChatRequest {
    let uses: Vec<Value> = ids
        .iter()
        .map(|id| json!({"type": "tool_use", "id": id, "name": "f", "input": {}}))
        .collect();
    let results: Vec<Value> = ids
        .iter()
        .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": "ok"}))
        .collect();
    let mut parts = vec![json!({"type": "text", "text": "calling"})];
    parts.extend(uses);
    let mut req = with_assistant_parts(Value::Array(parts));
    let mut messages: Vec<Message> = req.messages.to_vec();
    if let Some(last) = messages.last_mut() {
        *last = serde_json::from_value(json!({"role": "user", "content": results}))
            .expect("tool result turn");
    }
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
        firing: || tool_use_exchange(&["call_a1", "call.a:1"]),
        idle: || tool_use_exchange(&["call_a1", "call_b2"]),
        expected: &[(EditKind::Rewrite, "/messages/1/content/2/id")],
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
    AssemblyRow {
        name: "empty forwarded system turn dropped",
        site: EditSite::DialectFieldStrip,
        firing: || {
            request_from(json!({"model": "claude-x", "max_tokens": 16,
                "system": "rules",
                "messages": [user_turn(), {"role": "system", "content": ""},
                             {"role": "assistant", "content": "ok"}, user_turn()]}))
        },
        idle: || {
            request_from(json!({"model": "claude-x", "max_tokens": 16,
                "system": "rules",
                "messages": [user_turn(), {"role": "assistant", "content": "ok"}, user_turn()]}))
        },
        expected: &[(EditKind::Remove, "/messages/1")],
    },
    AssemblyRow {
        name: "same-role turns joined across a dropped system turn",
        site: EditSite::DialectFieldStrip,
        firing: || {
            request_from(json!({"model": "claude-x", "max_tokens": 16,
                "system": "rules",
                "messages": [user_turn(), {"role": "system", "content": ""}, user_turn()]}))
        },
        idle: || {
            request_from(json!({"model": "claude-x", "max_tokens": 16,
                "system": "rules",
                "messages": [user_turn(), {"role": "system", "content": "keep"}, user_turn()]}))
        },
        expected: &[
            (EditKind::Remove, "/messages/1"),
            (EditKind::Rewrite, "/messages/0/content"),
            (EditKind::Remove, "/messages/2"),
        ],
    },
    AssemblyRow {
        name: "billing-only forwarded system turn dropped",
        site: EditSite::ClientFingerprintStrip,
        firing: || {
            request_from(json!({"model": "claude-x", "max_tokens": 16,
                "system": "rules",
                "messages": [user_turn(),
                    {"role": "system",
                     "content": "x-anthropic-billing-header: cc_version=1; cch=abcde;"},
                    {"role": "assistant", "content": "ok"}, user_turn()]}))
        },
        idle: || {
            request_from(json!({"model": "claude-x", "max_tokens": 16,
                "system": "rules",
                "messages": [user_turn(), {"role": "system", "content": "keep"},
                             {"role": "assistant", "content": "ok"}, user_turn()]}))
        },
        expected: &[(EditKind::Remove, "/messages/1")],
    },
    AssemblyRow {
        name: "output schema additionalProperties injected at each object's depth",
        site: EditSite::RequiredFieldDefault,
        firing: || {
            base(json!({"provider_extras": {"output_config": {"format": {
                "type": "json_schema", "schema": {
                "type": "object",
                "properties": {"inner": {"type": "object", "properties": {}}}}}}}}))
        },
        idle: || {
            base(json!({"provider_extras": {"output_config": {"format": {
                "type": "json_schema", "schema": {
                "type": "object", "additionalProperties": false,
                "properties": {"inner": {"type": "object", "properties": {},
                                         "additionalProperties": false}}}}}}}))
        },
        expected: &[
            (
                EditKind::Add,
                "/output_config/format/schema/additionalProperties",
            ),
            (
                EditKind::Add,
                "/output_config/format/schema/properties/*/additionalProperties",
            ),
        ],
    },
    AssemblyRow {
        name: "omitted max_tokens filled with the default ceiling",
        site: EditSite::RequiredFieldDefault,
        firing: || request_from(json!({"model": "claude-x", "messages": [user_turn()]})),
        idle: || base(json!({})),
        expected: &[(EditKind::Add, "/max_tokens")],
    },
];

#[test]
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

/// A request carrying the client's own `thinking` object, as the Anthropic
/// ingress captures it.
fn forwarding(thinking: Value, max_thinking_budget: u32) -> ChatRequest {
    let mut req = base(json!({"max_tokens": 16000}));
    req.routectl_internal.anthropic_thinking =
        AnthropicThinking::Sent(ThinkingObject::from_client(thinking).expect("object"));
    req.routectl_internal.max_thinking_budget = max_thinking_budget;
    req
}

struct ForwardRow {
    name: &'static str,
    adaptive: bool,
    thinking: Value,
    max_thinking_budget: u32,
    sent: Value,
    override_edits: &'static [(EditKind, &'static str)],
    cap_edits: &'static [(EditKind, &'static str)],
}

#[test]
fn each_forwarded_thinking_override_declares_only_what_it_changed() {
    let rows = [
        ForwardRow {
            name: "enabled on an adaptive model",
            adaptive: true,
            thinking: json!({"type": "enabled", "budget_tokens": 8192}),
            max_thinking_budget: 0,
            sent: json!({"type": "adaptive"}),
            override_edits: &[
                (EditKind::Rewrite, "/thinking/type"),
                (EditKind::Remove, "/thinking/budget_tokens"),
            ],
            cap_edits: &[],
        },
        ForwardRow {
            name: "budgetless enabled on an adaptive model",
            adaptive: true,
            thinking: json!({"type": "enabled"}),
            max_thinking_budget: 0,
            sent: json!({"type": "adaptive"}),
            override_edits: &[(EditKind::Rewrite, "/thinking/type")],
            cap_edits: &[],
        },
        ForwardRow {
            name: "adaptive on an adaptive model",
            adaptive: true,
            thinking: json!({"type": "adaptive"}),
            max_thinking_budget: 0,
            sent: json!({"type": "adaptive"}),
            override_edits: &[],
            cap_edits: &[],
        },
        ForwardRow {
            name: "budget above the operator cap",
            adaptive: false,
            thinking: json!({"type": "enabled", "budget_tokens": 8192}),
            max_thinking_budget: 4096,
            sent: json!({"type": "enabled", "budget_tokens": 4096}),
            override_edits: &[],
            cap_edits: &[(EditKind::Rewrite, "/thinking/budget_tokens")],
        },
        ForwardRow {
            name: "budget equal to the operator cap",
            adaptive: false,
            thinking: json!({"type": "enabled", "budget_tokens": 4096}),
            max_thinking_budget: 4096,
            sent: json!({"type": "enabled", "budget_tokens": 4096}),
            override_edits: &[],
            cap_edits: &[],
        },
        ForwardRow {
            name: "budget with no operator cap",
            adaptive: false,
            thinking: json!({"type": "enabled", "budget_tokens": 8192}),
            max_thinking_budget: 0,
            sent: json!({"type": "enabled", "budget_tokens": 8192}),
            override_edits: &[],
            cap_edits: &[],
        },
    ];
    for row in rows {
        let (req, tap) = tapped(forwarding(row.thinking, row.max_thinking_budget));

        let body = request::normalize(
            "wire-edits-test",
            &req,
            row.adaptive,
            false,
            None,
            true,
            true,
        )
        .expect("fixture assembles");
        let edits = tap.take_request().edits(Leg::Request).to_vec();

        assert_eq!(body["thinking"], row.sent, "{}: sent thinking", row.name);
        assert_eq!(
            edits_of(&edits, EditSite::AdaptiveThinkingOverride),
            owned(row.override_edits),
            "{}: adaptive override",
            row.name
        );
        assert_eq!(
            edits_of(&edits, EditSite::ThinkingBudgetCap),
            owned(row.cap_edits),
            "{}: budget cap",
            row.name
        );
    }
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
fn the_cloak_relocation_is_silent_for_a_genuine_client() {
    let body = json!({
        "system": [{"type": "text", "text": "rules"}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    });

    assert!(cloak_edits(body, false, &CloakConfig::default()).is_empty());
}

#[test]
fn the_cloak_user_id_mint_declares_only_a_minted_value() {
    let fresh = json!({"metadata": {}, "messages": [user_turn()]});
    let set = json!({"metadata": {"user_id": "caller"}, "messages": [user_turn()]});

    assert_eq!(
        cloak_edits(fresh, true, &CloakConfig::default()),
        owned(&[(EditKind::Add, "/metadata/*")])
    );
    assert!(cloak_edits(set, true, &CloakConfig::default()).is_empty());
}

#[test]
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

/// A key planted into the sent schema: the pointer of its parent object
/// and the key.
type Planted = Option<(&'static str, &'static str)>;

/// The client's `output_config` beside the one the assembly sent, with the
/// `extra` key planted in the sent copy, diffed under the assembly's
/// declarations.
fn schema_findings(extra: Planted) -> Vec<String> {
    use routectl_core::wire_check::{LaneClass, diff};

    let client = json!({"format": {"type": "json_schema", "schema": {
        "type": "object",
        "properties": {"inner": {"type": "object", "properties": {}}}}}});
    let (body, edits) = assemble(base(json!({"provider_extras": {"output_config": client}})));
    let mut sent = json!({"output_config": body["output_config"].clone()});
    if let Some((parent, key)) = extra
        && let Some(obj) = sent.pointer_mut(parent).and_then(Value::as_object_mut)
    {
        obj.insert(key.to_string(), json!(true));
    }
    let input = json!({"output_config": client});

    diff(&input, &sent, LaneClass::Fidelity, &edits)
        .findings
        .iter()
        .map(|finding| finding.path.as_str().to_string())
        .collect()
}

#[test]
fn only_the_injected_schema_keys_are_explained() {
    let schema = "/output_config/format/schema";
    let inner = "/output_config/format/schema/properties/inner";
    let rows: [(&str, Planted, Vec<&str>); 5] = [
        ("injections alone", None, vec![]),
        (
            "unknown key beside the root injection",
            Some((schema, "x_vendor")),
            vec!["/output_config/format/schema/*"],
        ),
        (
            "keyword beside the root injection",
            Some((schema, "minProperties")),
            vec!["/output_config/format/schema/minProperties"],
        ),
        (
            "keyword beside a nested injection",
            Some((inner, "unevaluatedProperties")),
            vec!["/output_config/format/schema/properties/*/unevaluatedProperties"],
        ),
        (
            "property literally named additionalProperties",
            Some((
                "/output_config/format/schema/properties/inner/properties",
                "additionalProperties",
            )),
            vec!["/output_config/format/schema/properties/*/properties/*"],
        ),
    ];

    for (name, extra, expected) in rows {
        assert_eq!(schema_findings(extra), expected, "row {name}");
    }
}
