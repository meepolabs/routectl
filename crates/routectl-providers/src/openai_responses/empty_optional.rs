//! Removal of empty optional arguments from the function calls the codex lane
//! returns.
//!
//! The model behind the ChatGPT-subscription surface fills an optional
//! argument it has no value for with `""`. A client that validates the call
//! against the schema it declared rejects that value (a page range of `""`,
//! say), and the call fails although leaving the argument out was valid. A
//! top-level argument is removed only when its value is exactly `""`, the
//! tool's schema names it in `properties`, and the schema does not list it in
//! `required`. A tool whose schema has no `properties` object is never
//! touched, and arguments with nothing to remove pass through byte for byte.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{Map, Value};

use routectl_core::ToolDef;

/// The argument schema of one declared tool, borrowed from the request.
struct ArgShape<'a> {
    properties: &'a Map<String, Value>,
    required: Vec<&'a str>,
}

impl ArgShape<'_> {
    fn is_empty_optional(&self, key: &str, value: &Value) -> bool {
        value.as_str() == Some("")
            && self.properties.contains_key(key)
            && !self.required.contains(&key)
    }
}

/// The empty optional arguments removed over one response, per tool name.
pub struct EmptyOptionalFilter<'a> {
    shapes: HashMap<&'a str, ArgShape<'a>>,
    removed: Vec<(String, usize)>,
}

impl<'a> EmptyOptionalFilter<'a> {
    /// Index the request's declared tools. Only canonical function tools
    /// carry an argument schema; a tool whose schema has no `properties`
    /// object is left out, so its calls are never filtered.
    pub fn from_tools(tools: &'a [ToolDef]) -> Self {
        let shapes = tools
            .iter()
            .filter_map(|tool| match tool {
                ToolDef::Custom(c) => Some((c.name.as_str(), arg_shape(&c.input_schema)?)),
                ToolDef::Other(_) => None,
            })
            .collect();
        Self {
            shapes,
            removed: Vec::new(),
        }
    }

    /// `arguments` of a call to `name` without its empty optional top-level
    /// arguments. Borrowed unchanged when there is nothing to remove, when
    /// the tool is unknown or schemaless, or when the arguments are not a
    /// JSON object.
    pub fn apply<'s>(&mut self, name: &str, arguments: &'s str) -> Cow<'s, str> {
        let Some(shape) = self.shapes.get(name) else {
            return Cow::Borrowed(arguments);
        };
        let Ok(Value::Object(mut args)) = serde_json::from_str::<Value>(arguments) else {
            return Cow::Borrowed(arguments);
        };
        let before = args.len();
        args.retain(|key, value| !shape.is_empty_optional(key, value));
        let removed = before - args.len();
        if removed == 0 {
            return Cow::Borrowed(arguments);
        }
        let Ok(filtered) = serde_json::to_string(&Value::Object(args)) else {
            return Cow::Borrowed(arguments);
        };
        self.removed.push((name.to_owned(), removed));
        Cow::Owned(filtered)
    }

    /// The tool names and per-call removed-argument counts, in call order.
    pub fn removed(&self) -> &[(String, usize)] {
        &self.removed
    }
}

fn arg_shape(schema: &Value) -> Option<ArgShape<'_>> {
    let properties = schema.get("properties")?.as_object()?;
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    Some(ArgShape {
        properties,
        required,
    })
}

#[cfg(test)]
#[path = "empty_optional_tests.rs"]
mod tests;
