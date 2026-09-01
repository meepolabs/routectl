//! Lift `req.tool_choice` from Anthropic wire shape to OpenAI wire shape.
//!
//! Anthropic accepts:
//!   "auto" | "none" | "required"         (bare strings, shared with OpenAI)
//!   {"type":"auto"} | {"type":"any"} | {"type":"none"}
//!   {"type":"tool", "name":"X"}
//!
//! OpenAI accepts:
//!   "auto" | "none" | "required"         (bare strings)
//!   {"type":"function", "function":{"name":"X"}}
//!
//! Mapping:
//!   bare strings                     -> passthrough
//!   {"type":"function",...}          -> passthrough (already OpenAI)
//!   {"type":"tool","name":"X"}       -> {"type":"function","function":{"name":"X"}}
//!   {"type":"auto"}                  -> "auto"
//!   {"type":"any"}                   -> "required"
//!   {"type":"none"}                  -> "none"
//!   anything else                    -> warn + drop

use serde_json::Value;
use tracing::warn;

use routectl_core::{ChatRequest, Result};

use super::reject_or_drop_unrepresentable;

pub fn lift(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    let tc = if let Some(v) = req.tool_choice.as_ref() {
        v
    } else {
        obj.remove("tool_choice");
        return Ok(());
    };

    let mut dropped_shape = false;
    let lifted = map_tool_choice(id, tc, &mut dropped_shape);
    match lifted {
        Some(v) => {
            obj.insert("tool_choice".to_string(), v);
        }
        None => {
            obj.remove("tool_choice");
        }
    }

    // A forcing tool_choice with no tools to force is unrepresentable:
    // OpenAI hosts 400 on `tool_choice:"required"` (or a named function)
    // when `tools` is empty or absent. `tools` is read from the WIRE
    // (`obj["tools"]`), which is the post-tools-lift state because the
    // tools step runs before tool_choice in LIFT_STEPS. In lenient mode
    // we drop the forcing tool_choice and warn; strict rejects.
    //
    // Cross-dialect translation lane: a canonical forcing selector whose
    // tools were themselves dropped upstream in this same lift pass (the
    // Anthropic-builtin case in `tools.rs`). Drop rather than forward --
    // forwarding a selector naming tools the wire body no longer carries
    // is a guaranteed upstream 400, so the request survives shorn of the
    // selector instead of failing outright. Baked seed verdict: it stands
    // until this lane's own wire evidence contradicts it, and is not
    // eligible for deletion until then.
    // TRANSLATION-DROP: lane=openai-compat class=forcing_tool_choice_without_tools test=forcing_tool_choice_without_tools_drops_and_warns
    let mut dropped_forcing = false;
    if wire_tools_empty(obj) && tool_choice_is_forcing(obj.get("tool_choice")) {
        dropped_forcing = true;
        reject_or_drop_unrepresentable(
            id,
            strict,
            "tool_choice",
            "forcing tool_choice with no tools to force",
        )?;
        obj.remove("tool_choice");
    }

    // Bump the per-request counters at the lift's single exit rather than
    // at each arm: one request loses at most one tool_choice, but keeping
    // both classes' increments here makes the once-per-request property
    // structural rather than incidental. Strict mode never arrives (the
    // drop arms returned Err), so nothing lost is nothing counted.
    if dropped_shape {
        crate::translation_drop_metrics::record_translation_drop(
            "openai-compat",
            "tool_choice_shape_unrepresentable",
        );
    }
    if dropped_forcing {
        crate::translation_drop_metrics::record_translation_drop(
            "openai-compat",
            "forcing_tool_choice_without_tools",
        );
    }

    Ok(())
}

/// True when the wire body carries no usable `tools` array (absent or
/// empty). The tools lift runs first and removes the key when no tool
/// survives, so an empty/absent `obj["tools"]` is the authoritative
/// "nothing to force" signal.
fn wire_tools_empty(obj: &serde_json::Map<String, Value>) -> bool {
    match obj.get("tools") {
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
        None => true,
    }
}

/// True when the (post-map) wire tool_choice forces a tool call:
/// the bare string `"required"` or an OpenAI `{type:"function", ...}`
/// object. `"auto"` / `"none"` are not forcing. `map_tool_choice` nests
/// a named tool under `function`, so no forcing output carries a bare
/// top-level `name` key.
fn tool_choice_is_forcing(tc: Option<&Value>) -> bool {
    match tc {
        Some(Value::String(s)) => s == "required",
        Some(Value::Object(o)) => {
            matches!(o.get("type").and_then(|t| t.as_str()), Some("function"))
        }
        _ => false,
    }
}

/// Map one tool_choice value to OpenAI shape. Returns None when the
/// shape is unrecognized (caller drops the field). Sets `dropped_shape`
/// on any return that loses a selector the caller cannot re-derive, so
/// the caller's single exit owns the counter.
fn map_tool_choice(id: &str, tc: &Value, dropped_shape: &mut bool) -> Option<Value> {
    // Bare string: passthrough.
    if let Some(s) = tc.as_str() {
        return Some(Value::String(s.to_string()));
    }

    // Cross-dialect translation lane: a tool_choice value that is neither a
    // bare string nor an object carrying a string `type`. Drop rather than
    // forward -- OpenAI's tool_choice union accepts only those two shapes,
    // so an untagged value has no member to translate onto and forwarding it
    // 400s the whole request over one selector. Baked seed verdict: it
    // stands until this lane's own wire evidence contradicts it, and is not
    // eligible for deletion until then.
    // TRANSLATION-DROP: lane=openai-compat class=tool_choice_shape_unrepresentable test=untagged_tool_choice_drops_and_warns
    let Some(obj) = tc.as_object() else {
        *dropped_shape = true;
        warn!(
            provider = id,
            shape = "non-object",
            "openai-compat egress: unrecognized tool_choice shape dropped"
        );
        return None;
    };
    // Same lane, same class: an object with no string `type` discriminant
    // cannot be mapped onto any OpenAI tool_choice member either.
    // TRANSLATION-DROP: lane=openai-compat class=tool_choice_shape_unrepresentable test=tool_choice_object_without_type_drops_and_warns
    let Some(kind) = obj.get("type").and_then(|t| t.as_str()) else {
        *dropped_shape = true;
        warn!(
            provider = id,
            shape = "missing-type",
            "openai-compat egress: unrecognized tool_choice shape dropped"
        );
        return None;
    };

    match kind {
        // Already OpenAI function-name shape.
        "function" => Some(tc.clone()),

        // Anthropic specific-tool: rewrite to OpenAI function-name object.
        "tool" => {
            // Cross-dialect translation lane: an Anthropic
            // `{type:"tool"}` selector with no usable `name`. Drop rather
            // than forward -- the OpenAI member this maps onto is
            // `{type:"function", function:{name}}`, whose `name` is
            // mandatory, so there is nothing to construct and no way to
            // guess which tool the client meant. Baked seed verdict: it
            // stands until this lane's own wire evidence contradicts it,
            // and is not eligible for deletion until then.
            // TRANSLATION-DROP: lane=openai-compat class=tool_choice_shape_unrepresentable test=named_tool_choice_missing_name_drops_and_warns
            let name = if let Some(n) = obj.get("name").and_then(|n| n.as_str()) {
                n
            } else {
                *dropped_shape = true;
                warn!(
                    provider = id,
                    shape_type = "tool",
                    "openai-compat egress: tool_choice {{type:\"tool\"}} missing or invalid name; dropping field"
                );
                return None;
            };
            Some(serde_json::json!({
                "type": "function",
                "function": {"name": name}
            }))
        }

        // Anthropic auto -> OpenAI "auto".
        "auto" => Some(Value::String("auto".to_string())),

        // Anthropic any -> OpenAI "required".
        "any" => Some(Value::String("required".to_string())),

        // Anthropic none -> OpenAI "none".
        "none" => Some(Value::String("none".to_string())),

        // Cross-dialect translation lane: a tagged tool_choice shape from
        // neither dialect's known vocabulary (a future Anthropic tag, or a
        // client typo). Drop rather than forward -- routectl cannot know
        // which OpenAI union member an unknown tag was meant to become, and
        // forwarding an unrecognized `type` 400s a strict host. Baked seed
        // verdict: it stands until this lane's own wire evidence
        // contradicts it, and is not eligible for deletion until then.
        // TRANSLATION-DROP: lane=openai-compat class=tool_choice_shape_unrepresentable test=unknown_tool_choice_shape_drops_and_warns
        other => {
            *dropped_shape = true;
            warn!(
                provider = id,
                shape = other,
                "openai-compat egress: unrecognized tool_choice shape dropped"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{Message, MessageContent, Role};
    use serde_json::json;

    fn make_req(tool_choice: Option<Value>) -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            tool_choice,
            ..Default::default()
        }
    }

    fn run(tc: Option<Value>) -> Option<Value> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        lift("test", &mut obj, &req, false).unwrap();
        obj.get("tool_choice").cloned()
    }

    /// Variant of `run` that seeds the wire body with a non-empty `tools`
    /// array so a forcing tool_choice has something to force (the forcing-choice
    /// guard only fires when wire tools are empty/absent).
    fn run_with_tools(tc: Option<Value>) -> Option<Value> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );
        lift("test", &mut obj, &req, false).unwrap();
        obj.get("tool_choice").cloned()
    }

    /// Strict variant that seeds empty wire tools and surfaces the Result.
    fn run_strict_empty_tools(tc: Option<Value>) -> Result<Option<Value>> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        obj.insert("tools".into(), json!([]));
        lift("test", &mut obj, &req, true)?;
        Ok(obj.get("tool_choice").cloned())
    }

    #[test]
    fn bare_string_auto_passes_through() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!("auto"))), Some(json!("auto")));
    }

    #[test]
    fn bare_string_none_passes_through() {
        assert_eq!(run(Some(json!("none"))), Some(json!("none")));
    }

    #[test]
    fn bare_string_required_passes_through() {
        // `required` is forcing, so the forcing-choice guard would drop it unless
        // the wire carries tools. Seed tools so the choice survives.
        let req = make_req(Some(json!("required")));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );
        lift("test", &mut obj, &req, false).unwrap();

        assert_eq!(
            obj.get("tool_choice"),
            Some(&json!("required")),
            "forcing tool_choice must survive when tools are present"
        );
        assert!(
            obj.get("tools").is_some(),
            "seeded tools must still be on the wire"
        );
    }

    #[test]
    fn openai_function_object_passes_through() {
        // Arrange -- forcing function object; seed tools so the forcing-choice guard
        // does not drop it.
        let tc = json!({"type": "function", "function": {"name": "calculator"}});
        let req = make_req(Some(tc.clone()));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert -- passthrough verbatim and the guard did not fire.
        assert_eq!(obj.get("tool_choice"), Some(&tc));
        assert!(obj.get("tools").is_some(), "seeded tools must survive");
    }

    #[test]
    fn anthropic_tool_type_rewrites_to_openai_function() {
        // Arrange -- forcing named tool; seed tools so the forcing-choice guard does
        // not drop the rewritten choice.
        let tc = json!({"type": "tool", "name": "calculator"});
        let req = make_req(Some(tc));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();
        let result = obj.get("tool_choice").cloned().unwrap();

        // Assert
        assert_eq!(result["type"], "function");
        assert_eq!(result["function"]["name"], "calculator");
        assert!(obj.get("tools").is_some(), "seeded tools must survive");
    }

    #[test]
    fn anthropic_auto_object_rewrites_to_string() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!({"type": "auto"}))), Some(json!("auto")));
    }

    #[test]
    fn anthropic_any_rewrites_to_required() {
        // Arrange -- `any` maps to the forcing `required`; seed tools so the
        // forcing-choice guard does not drop it.
        let req = make_req(Some(json!({"type": "any"})));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert_eq!(obj.get("tool_choice"), Some(&json!("required")));
        assert!(obj.get("tools").is_some(), "seeded tools must survive");
    }

    #[test]
    fn anthropic_none_object_rewrites_to_string() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!({"type": "none"}))), Some(json!("none")));
    }

    #[test]
    #[serial_test::serial(openai_compat_tool_choice_shape_unrepresentable)]
    fn unknown_shape_is_dropped() {
        // Arrange -- a shape routectl has never seen.
        let tc = json!({"type": "custom_unknown_shape"});

        // Act + Assert -- field absent after lift
        assert_eq!(run(Some(tc)), None);
    }

    /// The `(openai-compat, class)` counter's current value, read back
    /// through the public snapshot.
    fn drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-compat" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    /// Run the lift over a body seeded with real wire tools and a real
    /// canonical tool_choice, returning the EMITTED WIRE BODY as the string
    /// an upstream would receive plus every captured event. Seeding tools
    /// keeps the forcing-choice guard out of the way so each fixture
    /// exercises exactly the arm it names, and gives the positive control a
    /// representable sibling riding in the same body.
    fn emitted_wire_with_tools(tc: Value) -> (String, Vec<routectl_testkit::CapturedEvent>) {
        let req = make_req(Some(tc));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "surviving_sibling_tool"}}]),
        );
        let events = routectl_testkit::capture_events(|| {
            lift("test", &mut obj, &req, false).expect("lenient lift must succeed");
        });
        let wire = serde_json::to_string(&Value::Object(obj)).expect("wire body serializes");
        (wire, events)
    }

    /// Assert the three bars for one unrepresentable tool_choice shape:
    /// the WARN fired, the offending value is absent from the emitted wire
    /// body, and the representable sibling (the tools array) survives in
    /// that same body. `marker` is a token unique to the fixture, so the
    /// absence assertion cannot pass by matching some unrelated key.
    fn assert_shape_drop(tc: Value, marker: &str) {
        // Act
        let before = drop_count("tool_choice_shape_unrepresentable");
        let (wire, events) = emitted_wire_with_tools(tc);
        let after = drop_count("tool_choice_shape_unrepresentable");

        // Assert 1 -- the drop warned.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("provider"), Some("test"));

        // Assert 2 -- neither the key nor the offending token reached the
        // emitted body.
        assert!(
            !wire.contains("tool_choice"),
            "the dropped selector's key must not reach the wire, got: {wire}"
        );
        assert!(
            !wire.contains(marker),
            "the dropped selector's payload must not reach the wire, got: {wire}"
        );

        // Assert 3 -- the representable sibling survived in that same body.
        assert!(
            wire.contains("surviving_sibling_tool"),
            "the representable tools sibling must survive, got: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// NEGATIVE CONTROL: a tagged-but-unknown selector shape.
    #[test]
    #[serial_test::serial(openai_compat_tool_choice_shape_unrepresentable)]
    fn unknown_tool_choice_shape_drops_and_warns() {
        assert_shape_drop(
            json!({"type": "marker_unknown_selector_tag"}),
            "marker_unknown_selector_tag",
        );
    }

    /// NEGATIVE CONTROL: a selector that is neither a string nor an object.
    /// This arm was a bare `?`-on-`Option` before, dropping with no log.
    #[test]
    #[serial_test::serial(openai_compat_tool_choice_shape_unrepresentable)]
    fn untagged_tool_choice_drops_and_warns() {
        assert_shape_drop(
            json!([{"marker": "marker_array_selector"}]),
            "marker_array_selector",
        );
    }

    /// NEGATIVE CONTROL: an object selector with no `type` discriminant.
    /// Also previously a bare `?`-on-`Option` with no log.
    #[test]
    #[serial_test::serial(openai_compat_tool_choice_shape_unrepresentable)]
    fn tool_choice_object_without_type_drops_and_warns() {
        assert_shape_drop(
            json!({"tool": "marker_typeless_selector"}),
            "marker_typeless_selector",
        );
    }

    /// NEGATIVE CONTROL: an Anthropic named-tool selector with no `name`.
    #[test]
    #[serial_test::serial(openai_compat_tool_choice_shape_unrepresentable)]
    fn named_tool_choice_missing_name_drops_and_warns() {
        // Arrange -- `name` present but not a string, so the arm's
        // "missing or invalid" path fires with a token to look for.
        assert_shape_drop(
            json!({"type": "tool", "name": {"nested": "marker_invalid_name"}}),
            "marker_invalid_name",
        );
    }

    /// POSITIVE CONTROL for all four fixtures above: a representable
    /// Anthropic named-tool selector must translate, reach the emitted wire
    /// body, and warn not at all. Without this, every absence assertion
    /// above would pass on a lift that dropped every selector.
    #[test]
    fn representable_named_tool_choice_survives_without_warning() {
        // Act
        let (wire, events) = emitted_wire_with_tools(json!({
            "type": "tool", "name": "marker_representable_tool"
        }));

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a representable selector must not warn at all, got: {events:?}"
        );
        assert!(
            wire.contains("marker_representable_tool") && wire.contains("tool_choice"),
            "the translated selector must reach the wire, got: {wire}"
        );
    }

    /// NEGATIVE CONTROL: a forcing selector whose tools are gone drops,
    /// warns, and leaves no selector on the emitted body -- while the
    /// unrelated keys of that body survive.
    #[test]
    #[serial_test::serial(openai_compat_forcing_tool_choice_without_tools)]
    fn forcing_tool_choice_without_tools_drops_and_warns() {
        // Arrange -- empty wire tools (the state `tools::lift` leaves when
        // every tool was itself unrepresentable) plus a forcing selector.
        let req = make_req(Some(json!({"type": "any"})));
        let mut obj = serde_json::Map::new();
        obj.insert("tools".into(), json!([]));
        obj.insert("model".into(), json!("marker_model_survives"));

        // Act
        let before = drop_count("forcing_tool_choice_without_tools");
        let events = routectl_testkit::capture_events(|| {
            lift("test", &mut obj, &req, false).expect("lenient lift must succeed");
        });
        let after = drop_count("forcing_tool_choice_without_tools");
        let wire = serde_json::to_string(&Value::Object(obj)).expect("wire body serializes");

        // Assert 1 -- the drop warned, naming the tool_choice context.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("context"), Some("tool_choice"));
        assert_eq!(
            warn.field("what"),
            Some("forcing tool_choice with no tools to force")
        );

        // Assert 2 -- no selector of any form reached the emitted body.
        assert!(
            !wire.contains("tool_choice") && !wire.contains("required"),
            "no forcing selector may reach the wire, got: {wire}"
        );

        // Assert 3 -- the rest of the body survived the drop.
        assert!(
            wire.contains("marker_model_survives"),
            "unrelated body keys must survive, got: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// POSITIVE CONTROL: the SAME forcing selector with real tools on the
    /// wire must survive and warn not at all -- proving the fixture above
    /// warns because of the missing tools, not because the selector was
    /// forcing.
    #[test]
    fn forcing_tool_choice_with_tools_survives_without_warning() {
        // Act
        let (wire, events) = emitted_wire_with_tools(json!({"type": "any"}));

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a forcing selector with tools present must not warn, got: {events:?}"
        );
        assert!(
            wire.contains("\"tool_choice\":\"required\""),
            "the forcing selector must survive when tools are present, got: {wire}"
        );
    }

    #[test]
    fn no_tool_choice_removes_key() {
        // Arrange + Act + Assert
        assert_eq!(run(None), None);
    }

    /// A forcing tool_choice ({type:"any"} -> "required") with
    /// no tools on the wire is unrepresentable; lenient mode drops it.
    #[test]
    #[serial_test::serial(openai_compat_forcing_tool_choice_without_tools)]
    fn forcing_tool_choice_without_tools_dropped_lenient() {
        // Arrange -- empty wire tools + a forcing Anthropic `any` choice.
        let req = make_req(Some(json!({"type": "any"})));
        let mut obj = serde_json::Map::new();
        obj.insert("tools".into(), json!([]));

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert -- tool_choice removed (cannot force with no tools).
        assert!(
            obj.get("tool_choice").is_none(),
            "forcing tool_choice with empty tools must be dropped"
        );
    }

    /// The same forcing-without-tools case errors under strict.
    #[test]
    #[serial_test::serial(openai_compat_forcing_tool_choice_without_tools)]
    fn forcing_tool_choice_without_tools_strict_errors() {
        // Act
        let res = run_strict_empty_tools(Some(json!({"type": "any"})));

        // Assert
        assert!(
            res.is_err(),
            "strict mode must reject forcing tc without tools"
        );
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("strict_translation"));
    }

    /// "auto" is not forcing -- it survives even with no tools.
    #[test]
    fn auto_tool_choice_without_tools_untouched() {
        // Arrange
        let req = make_req(Some(json!("auto")));
        let mut obj = serde_json::Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert_eq!(obj.get("tool_choice"), Some(&json!("auto")));
    }

    /// A forcing choice WITH tools present is untouched.
    #[test]
    fn forcing_tool_choice_with_tools_untouched() {
        // Arrange + Act + Assert
        assert_eq!(
            run_with_tools(Some(json!("required"))),
            Some(json!("required"))
        );
    }
}
