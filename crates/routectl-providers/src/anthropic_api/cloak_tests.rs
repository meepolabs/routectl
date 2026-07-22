use super::*;

use serde_json::json;

fn identity() -> ClaudeCodeIdentity {
    ClaudeCodeIdentity {
        session_id: "sess-123".into(),
        device_id: "a".repeat(64),
        account_uuid: "11111111-2222-3333-4444-555555555555".into(),
    }
}

// -- billing strip -----------------------------------------------------

#[test]
fn strip_removes_billing_block_keeps_others() {
    // Arrange
    let mut body = json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1; cch=abcde;"},
            {"type": "text", "text": "you are helpful"},
        ]
    });

    // Act
    strip_billing_block(&mut body);

    // Assert
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["text"], "you are helpful");
}

#[test]
fn strip_leaves_array_without_billing_unchanged() {
    // Arrange
    let mut body = json!({
        "system": [{"type": "text", "text": "you are helpful"}]
    });
    let before = body.clone();

    // Act
    strip_billing_block(&mut body);

    // Assert
    assert_eq!(body, before);
}

#[test]
fn strip_removes_pure_string_billing_system() {
    // Arrange
    let mut body = json!({"system": "x-anthropic-billing-header: v=1"});

    // Act
    strip_billing_block(&mut body);

    // Assert
    assert!(
        body.get("system").is_none(),
        "pure-billing string system must be removed"
    );
}

#[test]
fn strip_runs_even_with_genuine_cc_marker_present() {
    // Arrange: the interactive identity marker is present, but the
    // billing block must still be stripped.
    let mut body = json!({
        "system": [
            {"type": "text", "text": INTERACTIVE_IDENTITY_LINE},
            {"type": "text", "text": "x-anthropic-billing-header: v=1; cch=abcde;"},
        ]
    });

    // Act
    strip_billing_block(&mut body);

    // Assert
    let arr = body["system"].as_array().expect("system is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
}

// -- system relocation (non-CC) ----------------------------------------

fn reminder_text(inner: &str) -> String {
    format!("{SYSTEM_REMINDER_OPEN}\n{inner}\n{SYSTEM_REMINDER_CLOSE}")
}

#[test]
fn relocate_string_system_sets_identity_only_and_moves_to_first_user() {
    // Arrange: non-CC body, client system as a String, one user message.
    let mut body = json!({
        "system": "client system prompt",
        "messages": [{"role": "user", "content": "hello"}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: system is identity-only; first user content[0] is the
    // reminder wrapping the original string, content[1] the original text.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    let content = body["messages"][0]["content"]
        .as_array()
        .expect("content promoted to array");
    assert_eq!(content[0]["text"], reminder_text("client system prompt"));
    assert_eq!(content[1]["text"], "hello");
}

#[test]
fn relocate_array_system_joins_blocks_into_one_reminder() {
    // Arrange: client system as an array of two text blocks.
    let mut body = json!({
        "system": [
            {"type": "text", "text": "first block"},
            {"type": "text", "text": "second block"},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: identity-only system; reminder joins both blocks' text.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    let content = body["messages"][0]["content"]
        .as_array()
        .expect("content promoted to array");
    assert_eq!(
        content[0]["text"],
        reminder_text("first block\n\nsecond block")
    );
    assert_eq!(content[1]["text"], "hi");
}

#[test]
fn strict_mode_drops_client_system_and_leaves_user_message_unchanged() {
    // Arrange: strict mode set, client system present, a user message.
    let mut body = json!({
        "system": "client system prompt",
        "messages": [{"role": "user", "content": "hello"}]
    });

    // Act
    relocate_client_system(&mut body, true);

    // Assert: identity-only system; user message untouched, no reminder.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert_eq!(body["messages"][0]["content"], "hello");
}

#[test]
fn relocate_preserves_cache_control_on_reminder_block() {
    // Arrange: a client system block carrying a cache_control breakpoint.
    let mut body = json!({
        "system": [
            {"type": "text", "text": "cached prompt", "cache_control": {"type": "ephemeral"}},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: the relocated reminder carries the cache_control.
    let reminder = &body["messages"][0]["content"][0];
    assert_eq!(reminder["cache_control"]["type"], "ephemeral");
}

#[test]
fn relocate_no_panic_when_no_user_message_present() {
    // Arrange: only an assistant message -- nowhere to relocate into.
    let mut body = json!({
        "system": "client system prompt",
        "messages": [{"role": "assistant", "content": "prior"}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: identity-only system; client body dropped; messages intact.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert_eq!(body["messages"][0]["content"], "prior");
}

#[test]
fn relocate_no_panic_when_messages_empty() {
    // Arrange: empty messages array.
    let mut body = json!({
        "system": "client system prompt",
        "messages": []
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: identity-only system; no reminder anywhere.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert!(body["messages"].as_array().unwrap().is_empty());
}

#[test]
fn relocate_identity_only_system_leaves_messages_untouched() {
    // Arrange: client system is already exactly the identity line.
    let mut body = json!({
        "system": INTERACTIVE_IDENTITY_LINE,
        "messages": [{"role": "user", "content": "hello"}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: identity-only system; no reminder added; message intact.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert_eq!(body["messages"][0]["content"], "hello");
}

#[test]
fn relocate_excludes_identity_line_from_reminder() {
    // Arrange: client system = [identity line, real body].
    let mut body = json!({
        "system": [
            {"type": "text", "text": INTERACTIVE_IDENTITY_LINE},
            {"type": "text", "text": "real body"},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: identity-only system; only the real body relocated (the
    // identity line is not duplicated into the reminder).
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    let reminder = &body["messages"][0]["content"][0];
    assert_eq!(reminder["text"], reminder_text("real body"));
}

#[test]
fn relocated_identity_carries_no_cache_control() {
    // Arrange: a plain client system, no cache_control.
    let mut body = json!({
        "system": [{"type": "text", "text": "custom"}],
        "messages": [{"role": "user", "content": "hi"}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: the injected identity block has no cache breakpoint.
    let injected = &body["system"][0];
    assert!(
        injected.get("cache_control").is_none(),
        "injected identity must not add a cache breakpoint"
    );
}

#[test]
fn relocate_non_object_body_is_noop() {
    // Arrange: a body that is not a JSON object at the root.
    let mut body = Value::String("not an object".into());
    let before = body.clone();

    // Act
    relocate_client_system(&mut body, false);

    // Assert: the whole transform is a no-op -- no panic, no reminder
    // insertion, no system rewrite. The body stays the same String.
    assert_eq!(body, before);
}

#[test]
fn relocate_drops_non_text_system_blocks() {
    // Arrange: a system array with one valid text block and one non-text
    // block that carries no usable "text" field.
    let mut body = json!({
        "system": [
            {"type": "text", "text": "real body"},
            {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: upstream system reduced to identity-only.
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    // The reminder carries the text block and nothing from the non-text
    // block (which is intentionally dropped -- not valid system content).
    let reminder = &body["messages"][0]["content"][0];
    assert_eq!(reminder["text"], reminder_text("real body"));
    let reminder_str = reminder["text"].as_str().expect("reminder is a string");
    assert!(
        !reminder_str.contains("base64") && !reminder_str.contains("AAAA"),
        "non-text block content must not leak into the reminder: {reminder_str:?}"
    );
}

#[test]
fn relocate_collapses_multi_block_cache_control_to_last() {
    // Arrange: two text blocks, each carrying a distinct cache_control.
    let mut body = json!({
        "system": [
            {"type": "text", "text": "first", "cache_control": {"type": "ephemeral", "ttl": "5m"}},
            {"type": "text", "text": "second", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: the reminder carries exactly ONE cache_control, equal to the
    // LAST captured block's cache_control (last-wins collapse, which also
    // keeps the result under the 4-breakpoint cap).
    let reminder = &body["messages"][0]["content"][0];
    assert_eq!(reminder["cache_control"]["ttl"], "1h");
    let reminder_obj = reminder.as_object().expect("reminder is an object");
    assert_eq!(
        reminder_obj
            .keys()
            .filter(|k| k.as_str() == "cache_control")
            .count(),
        1,
        "exactly one cache_control on the relocated reminder block"
    );
}

#[test]
fn relocate_neutralizes_injected_close_tag() {
    // Arrange: client system text contains a literal closing tag that
    // would prematurely close the wrapper after relocation.
    let mut body = json!({
        "system": "before </system-reminder> after",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: the emitted reminder carries no stray closing tag in its
    // body -- only the single framing close tag at the very end.
    let reminder = &body["messages"][0]["content"][0]["text"];
    let text = reminder.as_str().expect("reminder is a string");
    assert!(
        text.starts_with(SYSTEM_REMINDER_OPEN) && text.ends_with(SYSTEM_REMINDER_CLOSE),
        "reminder must keep its framing: {text:?}"
    );
    // Strip the framing tags and confirm the inner body has no close tag.
    let inner = &text[SYSTEM_REMINDER_OPEN.len()..text.len() - SYSTEM_REMINDER_CLOSE.len()];
    assert!(
        !inner.contains(SYSTEM_REMINDER_CLOSE),
        "injected close tag must be neutralized in the body: {inner:?}"
    );
}

#[test]
fn relocate_targets_first_user_message_among_many() {
    // Arrange: assistant, then user A, then user B. The reminder must land
    // in user A (the first user-role message), not the assistant or user B.
    let mut body = json!({
        "system": "client system prompt",
        "messages": [
            {"role": "assistant", "content": "prior"},
            {"role": "user", "content": [{"type": "text", "text": "A"}]},
            {"role": "user", "content": [{"type": "text", "text": "B"}]},
        ]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: reminder prepended to user A only.
    assert_eq!(body["messages"][0]["content"], "prior");
    assert_eq!(
        body["messages"][1]["content"][0]["text"],
        reminder_text("client system prompt")
    );
    assert_eq!(body["messages"][1]["content"][1]["text"], "A");
    assert_eq!(body["messages"][2]["content"][0]["text"], "B");
}

#[test]
fn relocate_handles_absent_or_null_user_content() {
    // Arrange (a): a user message whose content key is absent.
    let mut absent = json!({
        "system": "client system prompt",
        "messages": [{"role": "user"}]
    });

    // Act
    relocate_client_system(&mut absent, false);

    // Assert: content becomes an array holding only the reminder.
    let content = absent["messages"][0]["content"]
        .as_array()
        .expect("content set to array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], reminder_text("client system prompt"));

    // Arrange (b): a user message whose content is explicitly null.
    let mut null_content = json!({
        "system": "client system prompt",
        "messages": [{"role": "user", "content": Value::Null}]
    });

    // Act
    relocate_client_system(&mut null_content, false);

    // Assert: same -- content becomes an array holding only the reminder.
    let content = null_content["messages"][0]["content"]
        .as_array()
        .expect("content set to array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], reminder_text("client system prompt"));
}

#[test]
fn relocate_handles_whitespace_only_system() {
    // Arrange: system is a whitespace-only string.
    let mut body = json!({
        "system": "   ",
        "messages": [{"role": "user", "content": "hello"}]
    });

    // Act
    relocate_client_system(&mut body, false);

    // Assert: system is reduced to identity-only (sensible, no panic).
    let sys = body["system"].as_array().expect("system is array");
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0]["text"], INTERACTIVE_IDENTITY_LINE);
    // Current behavior pinned: whitespace is not a recognized identity
    // line, so it IS relocated -- the reminder wraps the whitespace.
    let content = body["messages"][0]["content"]
        .as_array()
        .expect("content promoted to array");
    assert_eq!(content[0]["text"], reminder_text("   "));
    assert_eq!(content[1]["text"], "hello");
}

// -- metadata mint -----------------------------------------------------

#[test]
fn metadata_user_id_minted_when_absent() {
    // Arrange
    let id = identity();
    let mut body = json!({"model": "claude"});

    // Act
    mint_metadata_user_id(&mut body, &id);

    // Assert
    let user_id = body["metadata"]["user_id"]
        .as_str()
        .expect("user_id is a string");
    let parsed: Value = serde_json::from_str(user_id).expect("user_id parses as JSON");
    assert_eq!(parsed["device_id"], id.device_id);
    assert_eq!(parsed["account_uuid"], id.account_uuid);
    assert_eq!(parsed["session_id"], id.session_id);
    // device_id is 64 hex chars.
    let device = parsed["device_id"].as_str().unwrap();
    assert_eq!(device.len(), 64);
    assert!(device.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn metadata_user_id_key_order_is_corpus_shape() {
    // Arrange
    let id = identity();
    let mut body = json!({});

    // Act
    mint_metadata_user_id(&mut body, &id);

    // Assert: keys appear in device_id, account_uuid, session_id order.
    let user_id = body["metadata"]["user_id"].as_str().unwrap();
    let dev = user_id.find("device_id").unwrap();
    let acct = user_id.find("account_uuid").unwrap();
    let sess = user_id.find("session_id").unwrap();
    assert!(
        dev < acct && acct < sess,
        "key order must match corpus: {user_id}"
    );
}

#[test]
fn metadata_user_id_present_non_empty_untouched() {
    // Arrange
    let id = identity();
    let mut body = json!({"metadata": {"user_id": "client-supplied"}});

    // Act
    mint_metadata_user_id(&mut body, &id);

    // Assert
    assert_eq!(body["metadata"]["user_id"], "client-supplied");
}

#[test]
fn metadata_user_id_present_empty_is_minted() {
    // Arrange
    let id = identity();
    let mut body = json!({"metadata": {"user_id": ""}});

    // Act
    mint_metadata_user_id(&mut body, &id);

    // Assert
    assert_ne!(body["metadata"]["user_id"], "");
    let user_id = body["metadata"]["user_id"].as_str().unwrap();
    assert!(user_id.contains(&id.session_id));
}

#[test]
fn metadata_mint_preserves_other_metadata_keys() {
    // Arrange
    let id = identity();
    let mut body = json!({"metadata": {"other": "keep-me"}});

    // Act
    mint_metadata_user_id(&mut body, &id);

    // Assert
    assert_eq!(body["metadata"]["other"], "keep-me");
    assert!(body["metadata"]["user_id"].is_string());
}

// -- full cloak orchestration ------------------------------------------

#[test]
fn cloak_non_cc_strips_billing_stamps_identity_and_metadata() {
    // Arrange: non-CC body with billing + custom system and a user
    // message to relocate the client system into.
    let id = identity();
    let req = ChatRequest::default();
    let mut body = json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1"},
            {"type": "text", "text": "custom"},
        ],
        "messages": [{"role": "user", "content": "hello"}]
    });

    // Act
    cloak_oauth_egress(&mut body, &req, &id, true, &CloakConfig::default());

    // Assert: billing gone, system is identity-only, the client system is
    // relocated into the first user message, metadata minted.
    let arr = body["system"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert!(!arr.iter().any(|b| {
        b["text"]
            .as_str()
            .is_some_and(|t| t.starts_with(BILLING_PREFIX))
    }));
    let reminder = &body["messages"][0]["content"][0];
    assert_eq!(
        reminder["text"],
        format!("{SYSTEM_REMINDER_OPEN}\ncustom\n{SYSTEM_REMINDER_CLOSE}")
    );
    assert!(body["metadata"]["user_id"].is_string());
}

#[test]
fn cloak_genuine_cc_strips_billing_but_does_not_stamp() {
    // Arrange: genuine CC (is_non_cc = false). Billing must still be
    // stripped, but no identity block, no metadata, no reminder added.
    let id = identity();
    let req = ChatRequest::default();
    let mut body = json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1"},
            {"type": "text", "text": "custom"},
        ],
        "messages": [{"role": "user", "content": "hello"}]
    });

    // Act
    cloak_oauth_egress(&mut body, &req, &id, false, &CloakConfig::default());

    // Assert: billing stripped, but identity NOT stamped, metadata absent,
    // client system retained in `system`, and NO reminder anywhere.
    let arr = body["system"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["text"], "custom");
    assert!(body.get("metadata").is_none());
    assert_eq!(body["messages"][0]["content"], "hello");
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        !serialized.contains(SYSTEM_REMINDER_OPEN),
        "genuine CC must not gain a system-reminder block"
    );
}

#[test]
fn mint_session_id_prefers_credential_value() {
    let id = ClaudeCodeIdentity::mint(Some("cred-session"));
    assert_eq!(id.session_id, "cred-session");
}

#[test]
fn mint_session_id_falls_back_to_fresh_uuid() {
    let id = ClaudeCodeIdentity::mint(None);
    assert!(
        uuid::Uuid::parse_str(&id.session_id).is_ok(),
        "minted session id must be a valid uuid; got {}",
        id.session_id
    );
}

#[test]
fn mint_device_id_is_64_lowercase_hex() {
    let id = ClaudeCodeIdentity::mint(None);
    assert_eq!(id.device_id.len(), 64);
    assert!(
        id.device_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}

#[test]
fn mint_account_uuid_is_dashed_uuid() {
    let id = ClaudeCodeIdentity::mint(None);
    assert!(
        uuid::Uuid::parse_str(&id.account_uuid).is_ok(),
        "account_uuid must be a valid dashed uuid; got {}",
        id.account_uuid
    );
    assert!(id.account_uuid.contains('-'));
}

// -- tool-name normalization to mcp__ (forward) ------------------------

#[test]
fn doubles_single_underscore_mcp_prefix_only() {
    // Arrange: internal separators must be untouched.
    let mut body = json!({
        "tools": [{"name": "mcp_linear_get_issue"}]
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert: prefix doubled, internal underscores preserved.
    assert_eq!(body["tools"][0]["name"], "mcp__linear_get_issue");
    assert_eq!(
        reverse.get("mcp__linear_get_issue").map(String::as_str),
        Some("mcp_linear_get_issue")
    );
}

#[test]
fn renames_across_tool_choice() {
    // Arrange
    let mut body = json!({
        "tool_choice": {"type": "tool", "name": "mcp_foo"}
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body["tool_choice"]["name"], "mcp__foo");
    assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
}

#[test]
fn tool_choice_auto_is_untouched() {
    // Arrange: tool_choice without type=="tool" has no name to rename.
    let mut body = json!({"tool_choice": {"type": "auto"}});
    let before = body.clone();

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body, before);
    assert!(reverse.is_empty());
}

#[test]
fn renames_tool_use_in_message_history() {
    // Arrange
    let mut body = json!({
        "messages": [{
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "mcp_foo", "input": {}}]
        }]
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body["messages"][0]["content"][0]["name"], "mcp__foo");
    assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
}

#[test]
fn renames_tool_reference_in_message_history() {
    // Arrange
    let mut body = json!({
        "messages": [{
            "role": "user",
            "content": [{"type": "tool_reference", "tool_name": "mcp_foo"}]
        }]
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body["messages"][0]["content"][0]["tool_name"], "mcp__foo");
    assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
}

#[test]
fn renames_nested_tool_reference_inside_tool_result() {
    // Arrange
    let mut body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": [{"type": "tool_reference", "tool_name": "mcp_foo"}]
            }]
        }]
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(
        body["messages"][0]["content"][0]["content"][0]["tool_name"],
        "mcp__foo"
    );
    assert_eq!(reverse.get("mcp__foo").map(String::as_str), Some("mcp_foo"));
}

#[test]
fn idempotent_double_underscore_untouched_no_reverse_entry() {
    // Arrange: an already-mcp__ name records nothing and is unchanged.
    let mut body = json!({"tools": [{"name": "mcp__foo"}]});
    let before = body.clone();

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body, before);
    assert!(
        reverse.is_empty(),
        "an already-normalized name must record no reverse entry"
    );
}

#[test]
fn idempotent_applying_twice_is_byte_identical() {
    // Arrange
    let mut body = json!({"tools": [{"name": "mcp_foo"}]});

    // Act: first pass renames, second is a no-op.
    normalize_tool_names_to_mcp(&mut body);
    let once = body.clone();
    let reverse2 = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body, once);
    assert!(reverse2.is_empty());
}

#[test]
fn builtin_tool_with_type_is_skipped() {
    // Arrange: a native builtin (non-empty "type") is left unchanged
    // even if its name happens to carry the mcp_ prefix.
    let mut body = json!({
        "tools": [{"type": "web_search_20250305", "name": "mcp_should_not_rename"}]
    });
    let before = body.clone();

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body, before);
    assert!(reverse.is_empty());
}

#[test]
fn collision_guard_skips_when_renamed_form_already_exists() {
    // Arrange: renaming mcp_foo would collide with an existing mcp__foo.
    let mut body = json!({
        "tools": [
            {"name": "mcp_foo"},
            {"name": "mcp__foo"}
        ]
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert: the colliding rename is skipped; both names preserved.
    assert_eq!(body["tools"][0]["name"], "mcp_foo");
    assert_eq!(body["tools"][1]["name"], "mcp__foo");
    assert!(
        reverse.is_empty(),
        "collision must skip the rename and record no reverse entry"
    );
}

#[test]
fn bare_name_is_prefixed_with_mcp_double() {
    // Arrange: a bare snake_case tool name (the hermes-style set).
    let mut body = json!({"tools": [{"name": "read_file"}]});

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert: prefixed, reverse restores the bare original.
    assert_eq!(body["tools"][0]["name"], "mcp__read_file");
    assert_eq!(
        reverse.get("mcp__read_file").map(String::as_str),
        Some("read_file")
    );
}

#[test]
fn titlecase_bare_name_is_prefixed_with_mcp_double() {
    // Arrange: the bare path applies to anything non-mcp__, including
    // TitleCase names like Bash.
    let mut body = json!({"tools": [{"name": "Bash"}]});

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert
    assert_eq!(body["tools"][0]["name"], "mcp__Bash");
    assert_eq!(reverse.get("mcp__Bash").map(String::as_str), Some("Bash"));
}

#[test]
fn every_non_mcp_double_name_is_cloaked_across_all_surfaces() {
    // Arrange: a mixed set of bare, TitleCase, single-mcp_, and
    // already-mcp__ names spread across every renamed surface.
    let mut body = json!({
        "tools": [{"name": "Bash"}, {"name": "glob"}, {"name": "read_file"}],
        "tool_choice": {"type": "tool", "name": "Bash"},
        "messages": [{
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "terminal", "input": {}}]
        }]
    });

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert: every bare name became mcp__-prefixed on every surface.
    assert_eq!(body["tools"][0]["name"], "mcp__Bash");
    assert_eq!(body["tools"][1]["name"], "mcp__glob");
    assert_eq!(body["tools"][2]["name"], "mcp__read_file");
    assert_eq!(body["tool_choice"]["name"], "mcp__Bash");
    assert_eq!(body["messages"][0]["content"][0]["name"], "mcp__terminal");
    // Reverse map has one entry per distinct renamed name.
    assert_eq!(reverse.get("mcp__Bash").map(String::as_str), Some("Bash"));
    assert_eq!(reverse.get("mcp__glob").map(String::as_str), Some("glob"));
    assert_eq!(
        reverse.get("mcp__read_file").map(String::as_str),
        Some("read_file")
    );
    assert_eq!(
        reverse.get("mcp__terminal").map(String::as_str),
        Some("terminal")
    );
}

#[test]
fn full_hermes_tool_set_round_trips() {
    // Arrange: a representative subset of the real hermes tool set (all
    // bare snake_case), the empirical trigger for the billing 400.
    let names = [
        "browser_back",
        "read_file",
        "terminal",
        "write_file",
        "list_dir",
        "search",
    ];
    let tools: Vec<Value> = names.iter().map(|n| json!({"name": n})).collect();
    let mut body = json!({"tools": tools});

    // Act
    let reverse = normalize_tool_names_to_mcp(&mut body);

    // Assert: every tool is now mcp__-prefixed, one reverse entry each,
    // and the reverse fully restores the originals.
    let out = body["tools"].as_array().unwrap();
    assert_eq!(out.len(), names.len());
    for (i, n) in names.iter().enumerate() {
        let renamed = format!("mcp__{n}");
        assert_eq!(out[i]["name"], renamed);
        assert_eq!(reverse.get(&renamed).map(String::as_str), Some(*n));
    }
    assert_eq!(reverse.len(), names.len());
}

#[test]
fn cloak_oauth_egress_returns_reverse_map() {
    // Arrange
    let id = identity();
    let req = ChatRequest::default();
    let mut body = json!({"tools": [{"name": "mcp_foo"}]});

    // Act
    let result = cloak_oauth_egress(&mut body, &req, &id, true, &CloakConfig::default());

    // Assert
    assert_eq!(body["tools"][0]["name"], "mcp__foo");
    assert_eq!(
        result.tool_reverse.get("mcp__foo").map(String::as_str),
        Some("mcp_foo")
    );
}

#[test]
fn normalize_is_deterministic_same_input_byte_identical() {
    // Arrange: a body exercising every renamed surface.
    let template = json!({
        "tools": [{"name": "mcp_foo"}, {"name": "Bash"}],
        "tool_choice": {"type": "tool", "name": "mcp_bar"},
        "messages": [{
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "t1", "name": "mcp_baz", "input": {}},
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "tool_reference", "tool_name": "mcp_qux"}
                ]}
            ]
        }]
    });

    // Act: normalize two independent clones.
    let mut a = template.clone();
    let mut b = template;
    normalize_tool_names_to_mcp(&mut a);
    normalize_tool_names_to_mcp(&mut b);

    // Assert: byte-identical serialized output.
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}

// -- CloakConfig defaults ----------------------------------------------

#[test]
fn cloak_config_default_is_auto_false_empty_empty() {
    // Arrange / Act
    let cfg = CloakConfig::default();

    // Assert
    assert_eq!(cfg.mode, CloakMode::Auto);
    assert!(!cfg.strict_mode);
    assert!(cfg.tool_rename.is_empty());
    assert!(cfg.sensitive_words.is_empty());
}

#[test]
fn cloak_mode_default_is_auto() {
    assert_eq!(CloakMode::default(), CloakMode::Auto);
}

// -- regression guard: default config == base cloak transforms ---------

/// With a DEFAULT (empty) CloakConfig, the non-CC post-cloak body must be
/// byte-identical to the base cloak transforms: billing strip, system
/// relocation (identity-only system + client body moved to the first user
/// message), metadata mint, and the broadened tool-name normalization.
/// This is the hard regression guard for the opt-in surface; the base is
/// the NEW relocate behavior, not the old keep-behind-identity.
#[test]
fn default_config_byte_identical_to_base_transforms() {
    // Arrange: a body exercising billing strip + system relocation +
    // tool-name normalization (mcp_ subcase AND a bare name). A user
    // message is present so the client system has somewhere to relocate.
    let id = identity();
    let req = ChatRequest::default();
    let template = json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1"},
            {"type": "text", "text": "custom system prompt"},
        ],
        "tools": [{"name": "mcp_linear_get_issue"}, {"name": "Bash"}],
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello"}]
        }]
    });

    // Act: one body through the cloak with a default config; a second
    // body through the SAME base transforms applied directly (strip +
    // relocate + metadata + tool-name normalization).
    let mut via_config = template.clone();
    cloak_oauth_egress(&mut via_config, &req, &id, true, &CloakConfig::default());

    let mut via_base = template;
    strip_billing_block(&mut via_base);
    relocate_client_system(&mut via_base, false);
    mint_metadata_user_id(&mut via_base, &id);
    let _ = normalize_tool_names_to_mcp(&mut via_base);

    // Assert: byte-identical serialized output.
    assert_eq!(
        serde_json::to_string(&via_config).unwrap(),
        serde_json::to_string(&via_base).unwrap()
    );
    // The NEW base behavior: identity-only system, client body relocated.
    assert_eq!(via_config["system"][0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert_eq!(
        via_config["messages"][0]["content"][0]["text"],
        format!("{SYSTEM_REMINDER_OPEN}\ncustom system prompt\n{SYSTEM_REMINDER_CLOSE}")
    );
    // And the BROADENED normalization applied: the mcp_ subcase doubled
    // its prefix AND the bare name gained the mcp__ prefix.
    assert_eq!(via_config["tools"][0]["name"], "mcp__linear_get_issue");
    assert_eq!(via_config["tools"][1]["name"], "mcp__Bash");
}

/// Companion guard for the GENUINE-CC path (is_non_cc=false): with a
/// DEFAULT config, the post-cloak body must be byte-identical to the base
/// CC sequence -- strip_billing_block + normalize_tool_names_to_mcp ONLY,
/// with NO identity block prepended and NO metadata user_id minted.
#[test]
fn default_config_byte_identical_to_base_transforms_genuine_cc() {
    // Arrange: same representative body as the non-CC guard.
    let id = identity();
    let req = ChatRequest::default();
    let template = json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: v=1"},
            {"type": "text", "text": "custom system prompt"},
        ],
        "tools": [{"name": "mcp_linear_get_issue"}, {"name": "Bash"}],
        "messages": [{
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "mcp_foo", "input": {}}]
        }]
    });

    // Act: default config with is_non_cc=false vs. the base CC sequence
    // (billing strip + tool-name normalize only -- no identity, no
    // user_id mint).
    let mut via_config = template.clone();
    cloak_oauth_egress(&mut via_config, &req, &id, false, &CloakConfig::default());

    let mut via_base = template;
    strip_billing_block(&mut via_base);
    let _ = normalize_tool_names_to_mcp(&mut via_base);

    // Assert: byte-identical serialized output.
    assert_eq!(
        serde_json::to_string(&via_config).unwrap(),
        serde_json::to_string(&via_base).unwrap()
    );
    // The broadened normalization still applies on the CC path...
    assert_eq!(via_config["tools"][0]["name"], "mcp__linear_get_issue");
    assert_eq!(via_config["tools"][1]["name"], "mcp__Bash");
    // ...but NO identity block was prepended (system[0] is still the
    // client's billing-stripped first block, not the interactive line)
    // and NO metadata was minted.
    assert_ne!(via_config["system"][0]["text"], INTERACTIVE_IDENTITY_LINE);
    assert!(via_config.get("metadata").is_none());
}

// -- tool_rename --------------------------------------------------------

#[test]
fn tool_rename_applies_to_tools_and_tool_use_and_records_reverse() {
    // Arrange: an operator rename across tools + tool_use. Because the
    // tool-name normalization runs FIRST, a bare `foo` is already
    // `mcp__foo` on the wire by the time the operator pass runs, so the
    // rename keys on the normalized name `mcp__foo`.
    let id = identity();
    let req = ChatRequest::default();
    let mut body = json!({
        "tools": [{"name": "foo"}],
        "messages": [{
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "foo", "input": {}}]
        }]
    });
    let cfg = CloakConfig {
        tool_rename: vec![ToolRename {
            from: "mcp__foo".into(),
            to: "bar".into(),
        }],
        ..CloakConfig::default()
    };

    // Act
    let result = cloak_oauth_egress(&mut body, &req, &id, false, &cfg);

    // Assert: forward rename applied on both surfaces.
    assert_eq!(body["tools"][0]["name"], "bar");
    assert_eq!(body["messages"][0]["content"][0]["name"], "bar");
    // Both reverse hops recorded: the normalization mcp__foo->foo and the
    // operator rename bar->mcp__foo.
    assert_eq!(
        result.tool_reverse.get("mcp__foo").map(String::as_str),
        Some("foo")
    );
    assert_eq!(
        result.tool_reverse.get("bar").map(String::as_str),
        Some("mcp__foo")
    );
}

/// Ordering: tool-name normalization runs FIRST, then operator
/// tool_rename. A rename targeting the post-normalization name `mcp__x`
/// must match (because the wire name is already `mcp__x` by the time the
/// operator pass runs). A rename keyed on the pre-normalization `mcp_x`
/// must NOT match (the normalization pass already changed it).
#[test]
fn tool_rename_runs_after_tool_name_normalization() {
    let id = identity();
    let req = ChatRequest::default();

    // (a) rename keyed on the normalized name mcp__x -> renamed.
    let mut body_a = json!({"tools": [{"name": "mcp_x"}]});
    let cfg_a = CloakConfig {
        tool_rename: vec![ToolRename {
            from: "mcp__x".into(),
            to: "renamed".into(),
        }],
        ..CloakConfig::default()
    };
    let res_a = cloak_oauth_egress(&mut body_a, &req, &id, false, &cfg_a);
    assert_eq!(
        body_a["tools"][0]["name"], "renamed",
        "rename keyed on the normalized name must match"
    );
    // Both reverse hops are recorded: mcp__x->mcp_x (from the
    // normalization pass) and renamed->mcp__x (from the operator pass).
    assert_eq!(
        res_a.tool_reverse.get("mcp__x").map(String::as_str),
        Some("mcp_x")
    );
    assert_eq!(
        res_a.tool_reverse.get("renamed").map(String::as_str),
        Some("mcp__x")
    );

    // (b) rename keyed on the PRE-normalization name mcp_x must NOT match
    // (the normalization pass already rewrote it to mcp__x first).
    let mut body_b = json!({"tools": [{"name": "mcp_x"}]});
    let cfg_b = CloakConfig {
        tool_rename: vec![ToolRename {
            from: "mcp_x".into(),
            to: "should_not_apply".into(),
        }],
        ..CloakConfig::default()
    };
    cloak_oauth_egress(&mut body_b, &req, &id, false, &cfg_b);
    assert_eq!(
        body_b["tools"][0]["name"], "mcp__x",
        "rename keyed on the pre-normalization name must NOT match"
    );
}

#[test]
fn tool_rename_empty_is_noop() {
    let mut body = json!({"tools": [{"name": "foo"}]});
    let mut reverse: HashMap<String, String> = HashMap::new();
    apply_tool_rename(&mut body, &[], &mut reverse);
    assert_eq!(body["tools"][0]["name"], "foo");
    assert!(reverse.is_empty());
}

// -- sensitive_words ----------------------------------------------------

#[test]
fn sensitive_words_obfuscates_system_and_message_text() {
    // Arrange
    let mut body = json!({
        "system": "the secret password is here",
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "another secret"}]
        }]
    });

    // Act
    obfuscate_sensitive_words(&mut body, &["secret".to_string()]);

    // Assert: a zero-width space lands after the first char of "secret".
    let zws = ZERO_WIDTH_SPACE;
    let expect = format!("s{zws}ecret");
    assert!(
        body["system"].as_str().unwrap().contains(&expect),
        "system text must be obfuscated: {:?}",
        body["system"]
    );
    assert!(
        body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&expect),
        "message text must be obfuscated"
    );
}

#[test]
fn sensitive_words_empty_list_is_byte_identical() {
    // Arrange
    let mut with_words = json!({
        "system": "the secret password",
        "messages": [{"role": "user", "content": "secret stuff"}]
    });
    let without = with_words.clone();

    // Act: empty list must be a byte-identical no-op.
    obfuscate_sensitive_words(&mut with_words, &[]);

    // Assert
    assert_eq!(
        serde_json::to_string(&with_words).unwrap(),
        serde_json::to_string(&without).unwrap()
    );
}

#[test]
fn sensitive_words_case_insensitive_longest_first() {
    // Arrange: "secretkey" (longer) must win over "secret" at the same
    // anchor, and matching is case-insensitive.
    let mut body = json!({"system": "my SECRETKEY value"});
    let cfg_words = vec!["secret".to_string(), "secretkey".to_string()];

    // Act
    obfuscate_sensitive_words(&mut body, &cfg_words);

    // Assert: the obfuscation marks after the first char of the WHOLE
    // longest match, preserving the original casing of the remaining
    // chars ("SECRETKEY" -> "S<zws>ECRETKEY").
    let zws = ZERO_WIDTH_SPACE;
    let out = body["system"].as_str().unwrap();
    assert!(
        out.contains(&format!("S{zws}ECRETKEY")),
        "longest case-insensitive match must be obfuscated whole: {out:?}"
    );
}

#[test]
fn sensitive_words_obfuscation_carries_no_reverse() {
    // The full egress with sensitive_words set records NO extra reverse
    // entries for the obfuscation (zero-width space is invisible). The
    // tool name is already mcp__-shaped, so the tool-name normalization
    // adds no reverse entry either -- isolating the obfuscation pass.
    let id = identity();
    let req = ChatRequest::default();
    let mut body = json!({"system": "secret", "tools": [{"name": "mcp__bash"}]});
    let cfg = CloakConfig {
        sensitive_words: vec!["secret".to_string()],
        ..CloakConfig::default()
    };
    let result = cloak_oauth_egress(&mut body, &req, &id, false, &cfg);
    assert!(
        result.tool_reverse.is_empty(),
        "sensitive-word obfuscation must not add reverse entries"
    );
}
