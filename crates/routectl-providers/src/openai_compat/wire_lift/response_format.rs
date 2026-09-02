//! Lift Anthropic-shape `output_config.format` (carried via
//! `req.provider_extras`) into the OpenAI top-level `response_format`
//! field.
//!
//! Anthropic carries structured-output config like:
//!   {"output_config": {"format": {"type":"json_schema",
//!                                  "schema": {...}}}}
//! or
//!   {"output_config": {"format": {"type":"json_object"}}}
//!
//! OpenAI top-level shape:
//!   {"response_format": {"type":"json_schema",
//!                         "json_schema": {"schema": {...}, "strict": true}}}
//! or
//!   {"response_format": {"type":"json_object"}}
//!
//! If `obj["response_format"]` already exists at lift time (e.g. set by
//! the dialect or by a previous extras merge), DO NOT overwrite -- the
//! caller wins. The provider_extras merge runs LATER in the pipeline
//! (after wire_lift), but operator default_extras are folded in via
//! the same path so we preserve the same precedence here.
//!
//! Also strips `output_config` from obj so the Anthropic-shape field
//! never reaches strict OpenAI hosts (NIM 400s on unknown top-level
//! fields). Note: provider_extras merge runs AFTER this lift, but
//! `output_config` is gated through `req.provider_extras` which is
//! shallow-merged; if a careless config carries `output_config` in
//! `default_extras` or `provider_extras`, the managed-key allow-list
//! does not currently include it. The lift-side strip below is the
//! only guarantee we get on the egress wire.

use serde_json::{Map, Value};
use tracing::warn;

use routectl_core::{ChatRequest, Result};

pub fn lift(
    id: &str,
    obj: &mut Map<String, Value>,
    req: &ChatRequest,
    _strict: bool,
) -> Result<()> {
    // If response_format is already set, caller wins.
    if obj.contains_key("response_format") {
        // Still strip a stray output_config to keep the wire clean.
        obj.remove("output_config");
        return Ok(());
    }

    // TRANSLATION-DROP: structural -- no Anthropic-shape output_config.format was
    // supplied, so there is nothing to translate and nothing to lose.
    let format = match req
        .provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("output_config"))
        .and_then(|oc| oc.as_object())
        .and_then(|o| o.get("format"))
    {
        Some(f) => f,
        None => return Ok(()),
    };

    let lifted = match translate_format(id, format) {
        Some(v) => v,
        // The structured-output request the client sent is gone from the
        // outgoing body: the model will answer in free-form prose while the
        // client parses for JSON. `translate_format` owns the warn and the
        // counter for whichever of its exits fired; the `output_config`
        // strip below runs on this path too, so the Anthropic-shape leftover
        // never reaches a strict host as a consolation copy.
        None => {
            obj.remove("output_config");
            return Ok(());
        }
    };
    obj.insert("response_format".into(), lifted);
    // Strip the Anthropic-shape leftover regardless. (It currently
    // can't reach the wire today since the egress's extras merge has
    // a managed-key list, but defense in depth.)
    obj.remove("output_config");
    Ok(())
}

/// Translate one Anthropic-shape `output_config.format` object into the
/// OpenAI `response_format` shape, or `None` when it has no equivalent.
///
/// Both `None` paths lose the client's structured-output request outright,
/// so each warns and counts here rather than at the caller: the caller
/// cannot tell the two apart, and they are different operator problems
/// (an unknown format tag vs. a malformed json_schema entry).
fn translate_format(id: &str, format: &Value) -> Option<Value> {
    // TRANSLATION-DROP: structural -- a non-object format, or one with no string
    // `type`, is not a format specification this egress can be said to have
    // dropped a translation of; the unknown-tag arm below owns the real case.
    let obj = format.as_object()?;
    let kind = obj.get("type").and_then(|v| v.as_str())?;
    match kind {
        "json_schema" => {
            // Cross-dialect translation lane: an Anthropic
            // `output_config.format` tagged `json_schema` but carrying no
            // `schema` member. Drop rather than forward -- OpenAI's
            // `response_format.json_schema` requires `schema`, so there is
            // no valid shape to emit, and emitting the envelope without it
            // 400s the host on a field routectl invented. Baked seed
            // verdict: it stands until this lane's own wire evidence
            // contradicts it, and is not eligible for deletion until then.
            // TRANSLATION-DROP: lane=openai-compat class=response_format_schema_missing test=json_schema_without_schema_drops_and_warns
            let Some(schema) = obj.get("schema").cloned() else {
                warn!(
                    provider = id,
                    format_type = kind,
                    "openai-compat egress: dropping json_schema output format with no `schema` \
                     member; OpenAI response_format.json_schema requires one"
                );
                crate::translation_drop_metrics::record_translation_drop(
                    "openai-compat",
                    "response_format_schema_missing",
                );
                return None;
            };
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("response");
            let mut json_schema = Map::new();
            json_schema.insert("name".into(), Value::from(name));
            json_schema.insert("schema".into(), schema);
            // Emit strict only when the source explicitly requests it;
            // omit otherwise (absent beats explicit false on strict
            // hosts that reject an Anthropic-shape schema).
            if obj.get("strict").and_then(Value::as_bool) == Some(true) {
                json_schema.insert("strict".into(), Value::Bool(true));
            }
            Some(serde_json::json!({
                "type": "json_schema",
                "json_schema": json_schema
            }))
        }
        "json_object" => Some(serde_json::json!({"type": "json_object"})),
        // Cross-dialect translation lane: an `output_config.format` tag
        // from neither dialect's known vocabulary (a future Anthropic
        // format, or a client typo). Drop rather than forward -- OpenAI's
        // `response_format` union admits only the tags handled above, so
        // routectl cannot know which member an unknown tag was meant to
        // become and inventing one would silently constrain the model's
        // output shape. Baked seed verdict: it stands until this lane's own
        // wire evidence contradicts it, and it is not eligible for deletion
        // until then.
        // TRANSLATION-DROP: lane=openai-compat class=response_format_type_unrepresentable test=unknown_format_type_drops_and_warns
        other => {
            warn!(
                provider = id,
                format_type = other,
                "openai-compat egress: dropping unrecognized output format type; it has no \
                 OpenAI response_format equivalent"
            );
            crate::translation_drop_metrics::record_translation_drop(
                "openai-compat",
                "response_format_type_unrepresentable",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    fn req_with_extras(extras: Option<Value>) -> ChatRequest {
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
            provider_extras: extras,
            ..Default::default()
        }
    }

    #[test]
    fn json_schema_format_lifts_to_openai_response_format() {
        // Arrange
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "strict": true,
                    "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
                }
            }
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        let rf = &obj["response_format"];
        assert_eq!(rf["type"], "json_schema");
        assert_eq!(rf["json_schema"]["strict"], true);
        assert_eq!(
            rf["json_schema"]["name"], "response",
            "missing source name must default to \"response\""
        );
        assert_eq!(rf["json_schema"]["schema"]["type"], "object");
        assert_eq!(
            rf["json_schema"]["schema"]["properties"]["x"]["type"],
            "integer"
        );
    }

    #[test]
    fn json_schema_strict_absent_emits_no_strict_key() {
        // Arrange -- source format omits strict; absent must beat a
        // hosted default that would reject an Anthropic-shape schema.
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
                }
            }
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(
            obj["response_format"]["json_schema"]
                .get("strict")
                .is_none(),
            "strict must be omitted when the source does not request it"
        );
    }

    #[test]
    fn json_schema_strict_false_emits_no_strict_key() {
        // Arrange -- explicit false must also omit the key, not emit false.
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "strict": false,
                    "schema": {"type": "object"}
                }
            }
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(
            obj["response_format"]["json_schema"]
                .get("strict")
                .is_none(),
            "explicit strict:false must omit the key entirely"
        );
    }

    #[test]
    fn json_schema_strict_non_bool_emits_no_strict_key() {
        // Arrange -- a malformed (non-bool) strict is ignored.
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "strict": "yes",
                    "schema": {"type": "object"}
                }
            }
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(
            obj["response_format"]["json_schema"]
                .get("strict")
                .is_none(),
            "a non-bool strict must be ignored"
        );
    }

    #[test]
    fn json_schema_format_carries_source_name() {
        // Arrange -- the source format supplies an explicit name.
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "weather_report",
                    "schema": {"type": "object", "properties": {"x": {"type": "integer"}}}
                }
            }
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert -- the supplied name is carried through verbatim.
        assert_eq!(
            obj["response_format"]["json_schema"]["name"],
            "weather_report"
        );
    }

    #[test]
    fn json_object_format_passes_through() {
        // Arrange
        let extras = json!({
            "output_config": {"format": {"type": "json_object"}}
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert_eq!(obj["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn existing_response_format_is_not_overwritten() {
        // Arrange -- caller supplied response_format already; lift must
        // not clobber.
        let extras = json!({
            "output_config": {"format": {"type": "json_object"}}
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();
        obj.insert(
            "response_format".into(),
            json!({"type": "text", "marker": "caller-wins"}),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert -- the original is untouched
        assert_eq!(obj["response_format"]["marker"], "caller-wins");
    }

    #[test]
    fn no_output_config_is_no_op() {
        // Arrange
        let req = req_with_extras(None);
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(obj.get("response_format").is_none());
    }

    #[test]
    #[serial_test::serial(openai_compat_response_format_type_unrepresentable)]
    fn unknown_format_type_is_no_op() {
        // Arrange -- an unrecognized format type; lift must not invent a shape.
        let extras = json!({
            "output_config": {"format": {"type": "future_format_xyz"}}
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(obj.get("response_format").is_none());
    }

    /// The `(openai-compat, class)` counter's current value, read back
    /// through the public snapshot.
    fn drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "openai-compat" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    /// Run the lift over a body carrying an unrelated key, and return the
    /// EMITTED WIRE BODY as the string an upstream would receive plus every
    /// captured event. The unrelated key is the positive control's
    /// survivor: this lift writes only top-level keys, so a representable
    /// sibling of the dropped format is another surviving top-level field.
    fn emitted_wire(extras: Value) -> (String, Vec<routectl_testkit::CapturedEvent>) {
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();
        obj.insert("model".into(), json!("marker_model_survives"));
        let events = routectl_testkit::capture_events(|| {
            lift("test", &mut obj, &req, false).expect("lenient lift must succeed");
        });
        let wire = serde_json::to_string(&Value::Object(obj)).expect("wire body serializes");
        (wire, events)
    }

    /// NEGATIVE CONTROL: an unrecognized `output_config.format.type` drops,
    /// warns with structured fields naming the tag, and leaves neither a
    /// `response_format` nor the Anthropic-shape leftover on the emitted
    /// wire body. Before this task the arm returned `None` with no log at
    /// all -- the client's structured-output request vanished silently.
    #[test]
    #[serial_test::serial(openai_compat_response_format_type_unrepresentable)]
    fn unknown_format_type_drops_and_warns() {
        // Arrange
        let extras = json!({
            "output_config": {"format": {"type": "marker_future_format_tag"}}
        });

        // Act
        let before = drop_count("response_format_type_unrepresentable");
        let (wire, events) = emitted_wire(extras);
        let after = drop_count("response_format_type_unrepresentable");

        // Assert 1 -- the WARN fired, naming the unrecognized tag.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("provider"), Some("test"));
        assert_eq!(warn.field("format_type"), Some("marker_future_format_tag"));

        // Assert 2 -- nothing of the request reached the emitted body: no
        // invented response_format, and no Anthropic-shape leftover riding
        // along as a consolation copy for a strict host to 400 on.
        assert!(
            !wire.contains("response_format")
                && !wire.contains("output_config")
                && !wire.contains("marker_future_format_tag"),
            "no trace of the dropped format may reach the wire, got: {wire}"
        );

        // Assert 3 -- the rest of the body survived.
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

    /// NEGATIVE CONTROL: a `json_schema` format with no `schema` member
    /// drops, warns, and leaves no partial envelope on the emitted body.
    /// This is the `?`-on-`Option` exit that matched none of the audit's
    /// candidate-arm grep patterns and dropped with no log at all.
    #[test]
    // Sole holder of this guard name today: exactly one fixture in the crate
    // reaches this arm, so the guard excludes nothing yet. It is here so the
    // NEXT test that constructs this shape shares a name rather than silently
    // making the delta below flaky.
    #[serial_test::serial(openai_compat_response_format_schema_missing)]
    fn json_schema_without_schema_drops_and_warns() {
        // Arrange -- a json_schema entry carrying only a name.
        let extras = json!({
            "output_config": {
                "format": {"type": "json_schema", "name": "marker_orphan_schema_name"}
            }
        });

        // Act
        let before = drop_count("response_format_schema_missing");
        let (wire, events) = emitted_wire(extras);
        let after = drop_count("response_format_schema_missing");

        // Assert 1 -- the WARN fired, distinguishable from the unknown-tag
        // warn by its own message.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("format_type"), Some("json_schema"));
        assert!(
            warn.message.contains("no `schema` member"),
            "the warn must name the missing member, got: {:?}",
            warn.message
        );

        // Assert 2 -- no partial response_format envelope, and no leftover.
        assert!(
            !wire.contains("response_format")
                && !wire.contains("output_config")
                && !wire.contains("marker_orphan_schema_name"),
            "no partial envelope may reach the wire, got: {wire}"
        );

        // Assert 3 -- the rest of the body survived.
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

    /// POSITIVE CONTROL for both fixtures above: a `json_schema` format
    /// that DOES carry a `schema` must translate, land on the emitted wire
    /// body, and warn not at all. Without this, the absence assertions
    /// above would pass on a lift that dropped every format.
    #[test]
    fn representable_json_schema_survives_without_warning() {
        // Arrange
        let extras = json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "marker_representable_schema",
                    "schema": {"type": "object", "properties": {}}
                }
            }
        });

        // Act
        let (wire, events) = emitted_wire(extras);

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "a representable format must not warn at all, got: {events:?}"
        );
        assert!(
            wire.contains("response_format") && wire.contains("marker_representable_schema"),
            "the translated format must reach the wire, got: {wire}"
        );
        assert!(
            !wire.contains("output_config"),
            "the Anthropic-shape leftover must still be stripped, got: {wire}"
        );
    }

    #[test]
    fn output_config_is_stripped_after_lift() {
        // Arrange -- a stray output_config in obj would 400 strict hosts.
        let extras = json!({
            "output_config": {"format": {"type": "json_object"}}
        });
        let req = req_with_extras(Some(extras));
        let mut obj = Map::new();
        // Simulate a careless dialect or merge that left it on the body.
        obj.insert(
            "output_config".into(),
            json!({"format": {"type": "json_object"}}),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert!(
            obj.get("output_config").is_none(),
            "output_config must be stripped from the wire"
        );
    }
}
