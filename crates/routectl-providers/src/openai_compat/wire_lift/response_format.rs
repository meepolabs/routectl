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

use routectl_core::{ChatRequest, Result};

pub fn lift(
    _id: &str,
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

    let lifted = match translate_format(format) {
        Some(v) => v,
        None => return Ok(()),
    };
    obj.insert("response_format".into(), lifted);
    // Strip the Anthropic-shape leftover regardless. (It currently
    // can't reach the wire today since the egress's extras merge has
    // a managed-key list, but defense in depth.)
    obj.remove("output_config");
    Ok(())
}

fn translate_format(format: &Value) -> Option<Value> {
    let obj = format.as_object()?;
    let kind = obj.get("type").and_then(|v| v.as_str())?;
    match kind {
        "json_schema" => {
            let schema = obj.get("schema").cloned()?;
            Some(serde_json::json!({
                "type": "json_schema",
                "json_schema": {"schema": schema, "strict": true}
            }))
        }
        "json_object" => Some(serde_json::json!({"type": "json_object"})),
        _ => None,
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
            }],
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
        assert_eq!(rf["json_schema"]["schema"]["type"], "object");
        assert_eq!(
            rf["json_schema"]["schema"]["properties"]["x"]["type"],
            "integer"
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
