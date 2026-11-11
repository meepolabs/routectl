//! Canonical `req.tools` + `req.tool_choice` -> Responses
//! `tools` / `tool_choice` translation.
//!
//! `ToolDef::Custom` -> flat Responses shape `{type, name, description?,
//! parameters, strict?}`. `ToolDef::Other` passes through verbatim so
//! Anthropic builtins / future shapes ride the egress without code edits.
//!
//! Bedrock Mantle receives the function tools alone: a `ToolDef::Other` or a
//! namespace container may name a server or file it would reach under the
//! operator's identity, and neither the replayed `additional_tools` item nor
//! a container reaches that lane, so every function they declared is sent
//! flat instead.
//!
//! tool_choice mapping:
//!   - `"auto"` / `"required"` / `"none"` -> bare-string passthrough
//!   - named function (OpenAI object shape, Anthropic-shape, or any
//!     `{"name"}`-bearing object) -> flat Responses shape
//!     `{"type":"function","name":"X"}` (smoke 2026-05-12 confirmed the
//!     nested chat-completions shape is rejected with
//!     "Unknown parameter: 'tool_choice.function'").

use std::collections::BTreeSet;

use serde_json::{Value, json};

use routectl_core::schema::ResponsesNamespaces;
use routectl_core::wire_check::{EditKind, EditSite, WireTap};
use routectl_core::{ChatRequest, CustomTool, ToolDef, sanitize_for_log};

use super::AuthKind;
use super::extras::UnknownMemberWithheldTally;
use super::types::{ResponsesFunctionTag, ResponsesTool};
use crate::translation_drop_metrics::record_translation_drop;
use crate::wire_declare::declare;

/// Responses `input[]` item kind that declares tools inline. The ingress
/// merges its function declarations into `req.tools` AND preserves the item
/// for verbatim replay, so this egress must not re-declare them top-level.
const ADDITIONAL_TOOLS_ITEM: &str = "additional_tools";

/// Tool declarations carried by the replayed `additional_tools` input
/// items: the names of the declarations the ingress admitted as canonical
/// functions, plus the raw value of every declaration. A function name is
/// recorded only when `CustomTool::responses_function_name` accepts the raw
/// declaration -- the same test the ingress's normalization applies -- so a
/// malformed inline declaration never hides a valid same-name top-level tool.
struct InlineDeclarations<'a> {
    function_names: Vec<&'a str>,
    raw: Vec<&'a Value>,
}

impl<'a> InlineDeclarations<'a> {
    fn collect(req: &'a ChatRequest) -> Self {
        let mut decl = Self {
            function_names: Vec::new(),
            raw: Vec::new(),
        };
        let items = req.routectl_internal.responses_input_passthrough.iter();
        for p in items
            .filter(|p| p.item.get("type").and_then(Value::as_str) == Some(ADDITIONAL_TOOLS_ITEM))
        {
            for tool in p
                .item
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = CustomTool::responses_function_name(tool) {
                    decl.function_names.push(name);
                }
                decl.raw.push(tool);
            }
        }
        decl
    }

    fn declares(&self, td: &ToolDef) -> bool {
        match td {
            ToolDef::Custom(c) => self.function_names.contains(&c.name.as_str()),
            ToolDef::Other(v) => self.raw.contains(&v),
        }
    }
}

/// Which entries of `tools` are members the Responses ingress flattened out
/// of a namespace container. Their container is sent instead (spliced back
/// here when it came from the top-level `tools`, replayed inside the
/// `additional_tools` item otherwise), so a member is never declared on its
/// own. A named member is found by name: a flat tool of the same name was a
/// collision at ingress and was never flattened. An unnamed member is found
/// by its occurrence among value-identical entries, so a flat copy of it is
/// still declared.
fn flattened_members(namespaces: &ResponsesNamespaces, tools: &[ToolDef]) -> Vec<bool> {
    let mut seen: Vec<(&Value, usize)> = Vec::new();
    tools
        .iter()
        .map(|td| match td {
            ToolDef::Custom(c) => namespaces.members.contains_key(&c.name),
            ToolDef::Other(v) => match v.get("name").and_then(Value::as_str) {
                Some(name) => namespaces.members.contains_key(name),
                None if namespaces.unnamed_members.is_empty() => false,
                None => {
                    let occurrence = next_occurrence(&mut seen, v);
                    namespaces
                        .unnamed_members
                        .iter()
                        .any(|m| m.occurrence == occurrence && m.raw == *v)
                }
            },
        })
        .collect()
}

/// The count of `v` seen before this call, then counts this one.
fn next_occurrence<'a>(seen: &mut Vec<(&'a Value, usize)>, v: &'a Value) -> usize {
    match seen.iter_mut().find(|(seen_value, _)| *seen_value == v) {
        Some((_, count)) => {
            *count += 1;
            *count - 1
        }
        None => {
            seen.push((v, 1));
            0
        }
    }
}

/// Put each top-level namespace container back at its inbound index. The
/// ingress recorded the index in the client's `tools` array; with nothing
/// else removed the remaining tools are exactly that array's other
/// entries, so ascending inserts rebuild its order. An index past the end
/// (a tool removed on the way) appends.
fn splice_containers(out: &mut Vec<ResponsesTool>, namespaces: &ResponsesNamespaces) {
    for container in &namespaces.containers {
        let at = container.index.min(out.len());
        out.insert(at, ResponsesTool::Other(container.raw.clone()));
    }
}

/// The flat Responses shape of a canonical function tool.
fn function_tool(c: &CustomTool) -> ResponsesTool {
    // Flat Responses shape: {type, name, description?, parameters, strict?}
    // The chat-completions nested shape ({type, function:{name,...}}) is
    // rejected by the chatgpt-oauth backend with
    // "Missing required parameter: 'tools[0].name'" (smoke 2026-05-12).
    ResponsesTool::Function {
        kind: ResponsesFunctionTag::Function,
        name: c.name.clone(),
        description: c.description.clone(),
        parameters: c.input_schema.clone(),
        strict: c.strict,
    }
}

/// Translate `req.tools` into the Responses `tools` array for the lane
/// `auth_kind`. Returns an empty Vec when no tools are configured -- the
/// parent `ResponsesRequest` skips serializing the field when empty.
pub(super) fn translate_tools(
    req: &ChatRequest,
    auth_kind: AuthKind,
    tap: Option<&WireTap>,
    withheld: &mut UnknownMemberWithheldTally,
) -> Vec<ResponsesTool> {
    if auth_kind == AuthKind::BedrockMantle {
        return translate_mantle_tools(req, tap, withheld);
    }
    translate_first_party_tools(req)
}

/// Tools an `additional_tools` input item already declares are omitted:
/// that item is replayed verbatim in `input[]`, and declaring a tool twice
/// is rejected upstream. For the same reason a member flattened out of a
/// namespace container is omitted and its container sent as written.
fn translate_first_party_tools(req: &ChatRequest) -> Vec<ResponsesTool> {
    let namespaces = &req.routectl_internal.responses_namespaces;
    let tools = req.tools.as_deref().unwrap_or_default();
    let inline = InlineDeclarations::collect(req);
    let mut out: Vec<ResponsesTool> = Vec::with_capacity(tools.len());
    let flattened = flattened_members(namespaces, tools);
    let declared = tools
        .iter()
        .zip(flattened)
        .filter(|(td, is_member)| !is_member && !inline.declares(td))
        .map(|(td, _)| td);
    for td in declared {
        match td {
            ToolDef::Custom(c) => out.push(function_tool(c)),
            ToolDef::Other(v) => {
                // Forward-compat passthrough. Anthropic builtins and
                // future shapes ride here unchanged so the Responses
                // server can surface its own error if it doesn't
                // accept them.
                //
                // EXCEPT `cache_control`: an Anthropic-only prompt-cache
                // marker has no Responses wire slot, and a tools-position
                // marker IS a counted cache breakpoint, so leaving it on
                // the forwarded value made the lane's
                // `cache_control_unsupported` counter report a removal that
                // never happened while the field still reached the upstream.
                // Stripping here is what makes that count true. Mirrors the
                // per-part strip the openai-compat content lift performs.
                let mut forwarded = v.clone();
                if let Some(obj) = forwarded.as_object_mut() {
                    obj.remove("cache_control");
                }
                out.push(ResponsesTool::Other(forwarded));
            }
        }
    }
    splice_containers(&mut out, namespaces);
    out
}

/// The Bedrock Mantle `tools` array: every canonical function tool, flat,
/// in canonical order, including those a namespace container or an
/// `additional_tools` item declared. Every `ToolDef::Other` and every
/// top-level namespace container is withheld and the request recorded on
/// `withheld`.
/// TRANSLATION-DROP: lane=openai-responses class=unknown_member_withheld test=mantle_withholds_unmodeled_tools_and_namespace_containers_and_counts_once
fn translate_mantle_tools(
    req: &ChatRequest,
    tap: Option<&WireTap>,
    withheld: &mut UnknownMemberWithheldTally,
) -> Vec<ResponsesTool> {
    let tools = req.tools.as_deref().unwrap_or_default();
    let unmodeled = tools
        .iter()
        .filter(|td| matches!(td, ToolDef::Other(_)))
        .count();
    let containers = req.routectl_internal.responses_namespaces.containers.len();
    if unmodeled + containers > 0 {
        withheld.record();
        tracing::debug!(
            unmodeled,
            containers,
            "openai-responses egress: omitting unmodeled tools and namespace containers on \
             bedrock-mantle"
        );
    }
    if let Some(tap) = tap {
        declare_mantle_tool_edits(tap, req, tools);
    }
    tools
        .iter()
        .filter_map(|td| match td {
            ToolDef::Custom(c) => Some(function_tool(c)),
            ToolDef::Other(_) => None,
        })
        .collect()
}

/// Declare how the Mantle `tools` array differs from the client's: a Remove
/// at the client index of each withheld container and `ToolDef::Other`, and
/// an Add at the wire index of each function the client declared inside a
/// container or an `additional_tools` item. A tool from the client's own
/// `tools` array takes the next client index no container holds, the rule
/// [`splice_containers`] rebuilds that array by.
fn declare_mantle_tool_edits(tap: &WireTap, req: &ChatRequest, tools: &[ToolDef]) {
    let namespaces = &req.routectl_internal.responses_namespaces;
    let inline = InlineDeclarations::collect(req);
    let container_at: BTreeSet<usize> = namespaces.containers.iter().map(|c| c.index).collect();
    for index in &container_at {
        declare_tool(tap, *index, EditKind::Remove);
    }
    let mut client_slots = (0..).filter(|index| !container_at.contains(index));
    let mut wire_index = 0;
    for (td, is_member) in tools.iter().zip(flattened_members(namespaces, tools)) {
        let client_index = (!is_member && !inline.declares(td))
            .then(|| client_slots.next())
            .flatten();
        match (td, client_index) {
            (ToolDef::Custom(_), None) => declare_tool(tap, wire_index, EditKind::Add),
            (ToolDef::Other(_), Some(index)) => declare_tool(tap, index, EditKind::Remove),
            (ToolDef::Custom(_) | ToolDef::Other(_), _) => {}
        }
        if matches!(td, ToolDef::Custom(_)) {
            wire_index += 1;
        }
    }
}

fn declare_tool(tap: &WireTap, index: usize, kind: EditKind) {
    declare(
        tap,
        EditSite::AuthSurfaceGate,
        format!("/tools/{index}"),
        kind,
    );
}

/// Per-REQUEST tally for the tool_choice family's drops on this lane.
///
/// Each field is a per-request FLAG, not an occurrence count: a request
/// carries exactly one `tool_choice`, and the two classes are the two
/// distinct operator problems a malformed one presents. The denominator is
/// NOT touched here -- `request::translate` owns the single
/// `record_translation_lane_seen` site for this lane, and a second would
/// understate the rate for the whole lane.
#[derive(Default)]
#[must_use = "a tally records nothing until flush() runs"]
struct ToolChoiceDropTally {
    shape_unrepresentable: bool,
    name_missing: bool,
}

impl ToolChoiceDropTally {
    /// Record that the caller's tool_choice named a mode the Responses
    /// `tool_choice` field has no spelling for.
    const fn record_shape_unrepresentable(&mut self) {
        self.shape_unrepresentable = true;
    }

    /// Record that the caller asked for a specific function but supplied no
    /// usable name for it.
    const fn record_name_missing(&mut self) {
        self.name_missing = true;
    }

    fn flush(self) {
        if self.shape_unrepresentable {
            record_translation_drop(super::LANE, "tool_choice_shape_unrepresentable");
        }
        if self.name_missing {
            record_translation_drop(super::LANE, "tool_choice_name_missing");
        }
    }
}

/// Translate `req.tool_choice` to the Responses-shape value. Returns
/// None when canonical has no tool_choice. Bare-string shapes
/// (`auto`/`required`/`none`) pass through verbatim; OpenAI / Anthropic
/// named-function shapes collapse to flat Responses shape
/// `{"type":"function","name":"X"}`.
///
/// Owns the tally's whole lifetime: this is the one function every
/// request's tool_choice passes through exactly once, and nothing below it
/// is fallible, so the flush cannot be skipped by an early `?` the way a
/// flush placed further out could be.
pub(super) fn translate_tool_choice(tc: Option<&Value>) -> Option<Value> {
    let mut tally = ToolChoiceDropTally::default();
    let out = translate_tool_choice_tallied(tc, &mut tally);
    tally.flush();
    out
}

fn translate_tool_choice_tallied(
    tc: Option<&Value>,
    tally: &mut ToolChoiceDropTally,
) -> Option<Value> {
    let tc = tc?;
    match tc {
        Value::String(s) => translate_tool_choice_string(s, tally),
        Value::Object(map) => translate_tool_choice_object(map, tally),
        _ => None,
    }
}

fn translate_tool_choice_string(s: &str, tally: &mut ToolChoiceDropTally) -> Option<Value> {
    match s {
        "auto" | "required" | "none" => Some(Value::String(s.to_string())),
        // A bare-string mode token outside `auto` / `required` / `none` (a
        // future spelling, or a client typo). The Responses `tool_choice`
        // field admits those three strings and the named-function object and
        // nothing else, so the upstream rejects any other token -- there is
        // no shape that carries it, and guessing at the nearest mode would
        // let the model call tools the caller may have meant to forbid.
        // Lane: openai-responses, construction-time translation. Baked seed
        // verdict: it stands until this lane's own wire evidence contradicts
        // it, and is not eligible for deletion until then.
        // TRANSLATION-DROP: lane=openai-responses class=tool_choice_shape_unrepresentable test=responses_unknown_bare_string_tool_choice_drops_and_counts_once
        other => {
            tally.record_shape_unrepresentable();
            tracing::warn!(
                tool_choice = %sanitize_for_log(other),
                "unknown bare-string tool_choice; dropping on Responses egress"
            );
            None
        }
    }
}

/// Object shapes recognized:
///   - OpenAI: `{"type":"function","function":{"name":"X"}}`
///   - Anthropic: `{"type":"tool","name":"X"}` (and `{"type":"auto"|"any"}`)
///   - Generic: any object with a `name` (or nested `function.name`)
///     string -> emit named-function shape.
fn translate_tool_choice_object(
    map: &serde_json::Map<String, Value>,
    tally: &mut ToolChoiceDropTally,
) -> Option<Value> {
    // Anthropic-shape `{"type":"auto"|"any"}` -> string equivalents.
    match map.get("type").and_then(|v| v.as_str()) {
        Some("auto") => return Some(Value::String("auto".into())),
        Some("any" | "required") => return Some(Value::String("required".into())),
        Some("none") => return Some(Value::String("none".into())),
        // Not one of the mode spellings, so this object is a named-function
        // shape (or malformed). Fall through to the name extraction below
        // rather than returning: nothing is decided or discarded here.
        // TRANSLATION-DROP: structural -- falls through to named-function extraction; no branch is terminal
        _ => {}
    }

    // The caller asked to force a specific function and named none this
    // egress can use. Split by WHICH loss it is, matching the two classes the
    // Converse egress uses for the same family: an object carrying no name
    // member at all is an unreadable shape (no mode token, no name -- nothing
    // to translate), while one carrying an empty name is a forcing request
    // whose target is missing. The Responses named-function shape requires
    // the name and the upstream rejects the object without it; substituting
    // `"required"` would force a DIFFERENT tool than the caller meant. Lane:
    // openai-responses, construction-time translation. Baked seed verdict: it
    // stands until this lane's own wire evidence contradicts it, and is not
    // eligible for deletion until then.
    // TRANSLATION-DROP: lane=openai-responses class=tool_choice_shape_unrepresentable test=responses_unreadable_tool_choice_object_drops_and_counts_once
    let Some(name) = extract_tool_name(map) else {
        tally.record_shape_unrepresentable();
        tracing::warn!("unknown tool_choice object shape; dropping on Responses egress");
        return None;
    };
    // TRANSLATION-DROP: lane=openai-responses class=tool_choice_name_missing test=responses_named_tool_choice_without_a_name_drops_and_counts_once
    if name.is_empty() {
        tally.record_name_missing();
        tracing::warn!("tool_choice missing or invalid name; dropping field on Responses egress");
        return None;
    }
    // Flat Responses shape: {"type":"function","name":"X"}
    // The chat-completions nested shape ({"type":"function","function":{"name":"X"}})
    // is rejected by the chatgpt-oauth backend with
    // "Unknown parameter: 'tool_choice.function'" (smoke 2026-05-12).
    Some(json!({
        "type": "function",
        "name": name
    }))
}

fn extract_tool_name(map: &serde_json::Map<String, Value>) -> Option<String> {
    // OpenAI shape: `{"type":"function","function":{"name":"X"}}`.
    if let Some(name) = map
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(name.to_string());
    }
    // Anthropic shape: `{"type":"tool","name":"X"}`. Falls back to any
    // top-level `name`.
    if let Some(name) = map.get("name").and_then(|n| n.as_str()) {
        return Some(name.to_string());
    }
    None
}

#[cfg(test)]
mod tool_choice_drop_tests {
    use super::translate_tool_choice;
    use serde_json::{Value, json};

    /// The `(openai-responses, class)` counter's current value, read through
    /// the public snapshot.
    fn drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-responses" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    fn shape_drop_count() -> u64 {
        drop_count("tool_choice_shape_unrepresentable")
    }

    fn name_drop_count() -> u64 {
        drop_count("tool_choice_name_missing")
    }

    /// The tool_choice value this egress would emit, serialized, alongside
    /// every captured event. `None` is rendered as JSON `null` so an absence
    /// assertion reads the emitted value rather than the typed Option.
    fn emitted(choice: Value) -> (Value, Vec<routectl_testkit::CapturedEvent>) {
        let mut out = Value::Null;
        let events = routectl_testkit::capture_events(|| {
            out = translate_tool_choice(Some(&choice)).unwrap_or(Value::Null);
        });
        (out, events)
    }

    /// NEGATIVE CONTROL: a bare-string mode token outside the three the
    /// Responses `tool_choice` field admits drops, warns with the sanitized
    /// token, and counts once. Before this the arm warned and counted nothing.
    #[test]
    fn responses_unknown_bare_string_tool_choice_drops_and_counts_once() {
        // Arrange
        let before = shape_drop_count();

        // Act
        let (out, events) = emitted(json!("marker_future_mode"));
        let after = shape_drop_count();

        // Assert 1 -- the WARN fired.
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("unknown bare-string tool_choice")
            }),
            "the drop must warn; got: {events:?}"
        );

        // Assert 2 -- nothing of the unusable token reached the emitted value.
        assert_eq!(out, Value::Null, "no tool_choice may be invented: {out}");

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// NEGATIVE CONTROL: an object carrying neither a mode token nor any name
    /// member is an unreadable shape, counted on the shape class.
    #[test]
    fn responses_unreadable_tool_choice_object_drops_and_counts_once() {
        // Arrange
        let before_shape = shape_drop_count();
        let before_name = name_drop_count();

        // Act
        let (out, events) = emitted(json!({"marker_unknown_key": "x"}));
        let after_shape = shape_drop_count();

        // Assert
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("unknown tool_choice object shape")
            }),
            "the drop must warn; got: {events:?}"
        );
        assert_eq!(out, Value::Null, "emitted: {out}");
        assert_eq!(after_shape - before_shape, 1);
        assert_eq!(
            name_drop_count(),
            before_name,
            "an object with no name member is a shape loss, not a missing-name one"
        );
    }

    /// NEGATIVE CONTROL: a named-function shape whose name is present but
    /// empty is a forcing request with no target -- counted on the name class,
    /// and NOT also as an unreadable shape.
    #[test]
    fn responses_named_tool_choice_without_a_name_drops_and_counts_once() {
        // Arrange
        let before_name = name_drop_count();
        let before_shape = shape_drop_count();

        // Act
        let (out, events) = emitted(json!({"type": "function", "function": {"name": ""}}));
        let after_name = name_drop_count();

        // Assert 1 -- exactly one WARN, the name one.
        assert_eq!(
            events
                .iter()
                .filter(|e| e.level == tracing::Level::WARN)
                .count(),
            1,
            "one lost tool_choice owes exactly one WARN; got: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("tool_choice missing or invalid name")),
            "the drop must warn about the name; got: {events:?}"
        );

        // Assert 2 -- nothing forced. Substituting `"required"` here would
        // force a DIFFERENT tool than the caller meant.
        assert_eq!(out, Value::Null, "emitted: {out}");

        assert_eq!(after_name - before_name, 1);
        assert_eq!(
            shape_drop_count(),
            before_shape,
            "a recognized named-function shape must not also count as unreadable"
        );
    }

    /// POSITIVE CONTROL for the fixtures above: every representable
    /// tool_choice spelling reaches the emitted value, warns not at all, and
    /// advances NEITHER counter. Without it the absence assertions above would
    /// pass against an egress that dropped every tool_choice.
    #[test]
    fn representable_tool_choices_survive_and_advance_no_counter() {
        for (choice, expected) in [
            (json!("auto"), json!("auto")),
            (json!("required"), json!("required")),
            (json!("none"), json!("none")),
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "get_weather"}),
                json!({"type": "function", "name": "get_weather"}),
            ),
            (
                json!({"type": "function", "function": {"name": "get_weather"}}),
                json!({"type": "function", "name": "get_weather"}),
            ),
        ] {
            // Arrange
            let before_name = name_drop_count();
            let before_shape = shape_drop_count();

            // Act
            let (out, events) = emitted(choice.clone());

            // Assert
            assert!(
                !events.iter().any(|e| e.level == tracing::Level::WARN),
                "{choice} is representable and must not warn; got: {events:?}"
            );
            assert_eq!(out, expected, "{choice} must reach the wire as {expected}");
            assert_eq!(name_drop_count(), before_name, "{choice} counted a drop");
            assert_eq!(shape_drop_count(), before_shape, "{choice} counted a drop");
        }
    }
}

#[cfg(test)]
mod inline_declaration_tests {
    use super::InlineDeclarations;
    use crate::alloc_probe::count_allocs;
    use routectl_core::{ChatRequest, CustomTool, ResponsesPassthroughItem};
    use serde_json::{Map, Value, json};

    /// A request replaying one `additional_tools` item that declares a
    /// single function whose schema carries `properties` entries.
    fn request_declaring_one_function(properties: usize) -> ChatRequest {
        let props: Map<String, Value> = (0..properties)
            .map(|i| (format!("field_{i}"), json!({"type": "string"})))
            .collect();
        let item = json!({
            "type": "additional_tools",
            "tools": [{
                "type": "function",
                "name": "shell",
                "parameters": {"type": "object", "properties": props}
            }]
        });
        let mut req = ChatRequest::default();
        req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
            modeled_prefix: 0,
            item,
        }];
        req
    }

    fn collect_counting(req: &ChatRequest) -> (usize, u64) {
        count_allocs(|| InlineDeclarations::collect(req).function_names.len())
    }

    #[test]
    fn collecting_inline_names_never_copies_the_declared_schema() {
        // Arrange
        let small = request_declaring_one_function(1);
        let large = request_declaring_one_function(2_000);

        // Act
        let (small_names, small_allocs) = collect_counting(&small);
        let (large_names, large_allocs) = collect_counting(&large);

        // Assert
        assert_eq!((small_names, large_names), (1, 1));
        assert_eq!(
            large_allocs, small_allocs,
            "collection must allocate the same for a 2000-property schema as for a 1-property one"
        );
    }

    #[test]
    fn normalizing_a_large_declaration_does_copy_its_schema() {
        // Arrange: negative control -- the probe observes a schema copy, so
        // the equality above would fail if collection normalized the tool.
        let large = request_declaring_one_function(2_000);
        let tool = &large.routectl_internal.responses_input_passthrough[0].item["tools"][0];

        // Act
        let (normalized, allocs) =
            count_allocs(|| CustomTool::from_responses_function(tool).is_some());

        // Assert
        assert!(normalized);
        assert!(
            allocs > 2_000,
            "a schema copy allocates per property; observed {allocs}"
        );
    }
}
