//! Schema-driven validation of dotted `config.toml` paths.
//!
//! [`validate_config_path`] walks the derived `Config` JSON schema
//! (`schemars::schema_for!(Config)`) segment by segment so an editing
//! command can reject an unknown key BEFORE mutating the TOML document,
//! naming the offending segment and the valid sibling keys at that level.
//! The schema is the registry: a struct rename updates the accepted paths
//! with no separate list to maintain.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use serde_json::Value;

use crate::config::Config;

/// Shape of the node a validated path lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathShape {
    /// A leaf value (string / number / bool / enum / union).
    Scalar,
    /// A table or map node that holds further keys.
    Table,
}

/// Why a dotted path failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The path was empty.
    Empty,
    /// A leading, trailing, or doubled dot produced an empty segment.
    EmptySegment,
    /// The path carried a quoted segment (out of scope: quoted keys that
    /// may contain literal dots are not supported).
    QuotedKey,
    /// A segment did not name a known key at its level.
    UnknownSegment {
        /// The segment that did not match.
        segment: String,
        /// Valid sibling keys at that level, sorted.
        siblings: Vec<String>,
    },
    /// A segment tried to descend into a scalar leaf.
    NotATable {
        /// The segment that could not be descended into.
        segment: String,
    },
    /// A segment addressed an array (indexed or whole-array edits are
    /// unsupported).
    ArrayTarget {
        /// The array-typed segment.
        segment: String,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "config path is empty"),
            Self::EmptySegment => {
                write!(
                    f,
                    "config path has an empty segment (leading, trailing, or doubled dot)"
                )
            }
            Self::QuotedKey => {
                write!(f, "quoted config keys are unsupported")
            }
            Self::UnknownSegment { segment, siblings } => {
                if siblings.is_empty() {
                    write!(f, "unknown config key `{segment}`")
                } else {
                    write!(
                        f,
                        "unknown config key `{segment}`; valid keys here: {}",
                        siblings.join(", ")
                    )
                }
            }
            Self::NotATable { segment } => {
                write!(f, "cannot descend into `{segment}`: parent is not a table")
            }
            Self::ArrayTarget { segment } => {
                write!(
                    f,
                    "cannot edit array `{segment}`: indexed and whole-array edits are unsupported"
                )
            }
        }
    }
}

impl std::error::Error for PathError {}

/// The derived `Config` schema as a JSON value, computed once. Independent
/// of `Date`/env: only structure is walked, never the path defaults that
/// carry the generating machine's home directory.
fn config_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::to_value(schemars::schema_for!(Config))
            .expect("config schema serializes to JSON")
    })
}

/// Validates a dotted config path against the `Config` schema.
///
/// On success returns the leaf shape (scalar vs table). On failure returns
/// a [`PathError`] naming the offending segment and, for an unknown key,
/// the valid siblings at that level.
pub fn validate_config_path(dotted: &str) -> Result<PathShape, PathError> {
    if dotted.is_empty() {
        return Err(PathError::Empty);
    }
    if dotted.contains('"') || dotted.contains('\'') {
        return Err(PathError::QuotedKey);
    }
    let segments: Vec<&str> = dotted.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(PathError::EmptySegment);
    }

    let schema = config_schema();
    let mut node = schema;
    for seg in &segments {
        let resolved = deref(schema, node);
        if is_array(resolved) {
            return Err(PathError::ArrayTarget {
                segment: (*seg).to_string(),
            });
        }
        node = step(schema, resolved, seg)?;
    }

    let final_node = deref(schema, node);
    if is_array(final_node) {
        let last = segments.last().expect("non-empty path");
        return Err(PathError::ArrayTarget {
            segment: (*last).to_string(),
        });
    }
    Ok(classify(schema, final_node))
}

/// Descends one segment. Named properties (across `oneOf`/`anyOf` object
/// variants) are matched first; a map node then accepts any key. Returns
/// the child schema to walk next, or an error naming the failing segment.
fn step<'a>(schema: &'a Value, node: &'a Value, seg: &str) -> Result<&'a Value, PathError> {
    let variants = object_variants(schema, node);

    let mut sibling_keys = BTreeSet::new();
    for v in &variants {
        if let Some(props) = v.get("properties").and_then(Value::as_object) {
            for key in props.keys() {
                sibling_keys.insert(key.clone());
            }
            if let Some(child) = props.get(seg) {
                return Ok(child);
            }
        }
    }

    for v in &variants {
        if let Some(child) = map_value_schema(v) {
            return Ok(child);
        }
    }

    if sibling_keys.is_empty() {
        Err(PathError::NotATable {
            segment: seg.to_string(),
        })
    } else {
        Err(PathError::UnknownSegment {
            segment: seg.to_string(),
            siblings: sibling_keys.into_iter().collect(),
        })
    }
}

/// Classifies a resolved (non-array) node as a scalar leaf or a table.
fn classify(schema: &Value, node: &Value) -> PathShape {
    for v in object_variants(schema, node) {
        let has_props = v
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| !p.is_empty());
        if has_props || map_value_schema(v).is_some() {
            return PathShape::Table;
        }
    }
    PathShape::Scalar
}

/// Follows a `$ref` to its `$defs` entry. Non-ref nodes pass through.
fn deref<'a>(schema: &'a Value, node: &'a Value) -> &'a Value {
    if let Some(reference) = node.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.rsplit('/').next()
        && let Some(target) = schema.pointer(&format!("/$defs/{name}"))
    {
        return deref(schema, target);
    }
    node
}

/// The object-like schemas to consider for a node: the node itself plus any
/// `oneOf`/`anyOf` arms, each dereferenced, skipping `null` arms.
fn object_variants<'a>(schema: &'a Value, node: &'a Value) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for key in ["oneOf", "anyOf"] {
        if let Some(arms) = node.get(key).and_then(Value::as_array) {
            for arm in arms {
                let resolved = deref(schema, arm);
                if is_null_type(resolved) {
                    continue;
                }
                out.push(resolved);
            }
        }
    }
    if out.is_empty() {
        out.push(node);
    }
    out
}

/// The value schema of a map node, if `additionalProperties` carries a
/// schema. A `false` (deny-unknown-fields struct) is not a map.
fn map_value_schema(node: &Value) -> Option<&Value> {
    node.get("additionalProperties").filter(|v| v.is_object())
}

/// Whether a node's `type` is (or includes) `array`.
fn is_array(node: &Value) -> bool {
    type_matches(node, "array")
}

/// Whether a node's `type` is exactly `null` (a nullable union arm).
fn is_null_type(node: &Value) -> bool {
    match node.get("type") {
        Some(Value::String(s)) => s == "null",
        Some(Value::Array(items)) => items.len() == 1 && items[0].as_str() == Some("null"),
        _ => false,
    }
}

fn type_matches(node: &Value, want: &str) -> bool {
    match node.get("type") {
        Some(Value::String(s)) => s == want,
        Some(Value::Array(items)) => items.iter().any(|i| i.as_str() == Some(want)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_leaf_path_validates() {
        assert_eq!(validate_config_path("server.host"), Ok(PathShape::Scalar));
    }

    #[test]
    fn class_policy_leaf_validates_as_scalar() {
        assert_eq!(
            validate_config_path("retry.classes.server-error.retry"),
            Ok(PathShape::Scalar)
        );
    }

    #[test]
    fn map_key_path_validates_as_table() {
        assert_eq!(
            validate_config_path("retry.classes.server-error"),
            Ok(PathShape::Table)
        );
        assert_eq!(
            validate_config_path("providers.anthropic"),
            Ok(PathShape::Table)
        );
    }

    #[test]
    fn map_value_field_validates() {
        assert_eq!(
            validate_config_path("providers.anthropic.base_url"),
            Ok(PathShape::Scalar)
        );
    }

    #[test]
    fn numeric_class_override_key_validates() {
        assert_eq!(
            validate_config_path("providers.anthropic.class_overrides.400"),
            Ok(PathShape::Scalar)
        );
    }

    #[test]
    fn unknown_segment_lists_sorted_siblings() {
        let err = validate_config_path("server.nope").unwrap_err();
        match err {
            PathError::UnknownSegment { segment, siblings } => {
                assert_eq!(segment, "nope");
                assert!(siblings.contains(&"host".to_string()));
                let mut sorted = siblings.clone();
                sorted.sort();
                assert_eq!(siblings, sorted, "siblings must be sorted");
            }
            other => panic!("expected UnknownSegment, got {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_segment_reports_siblings() {
        let err = validate_config_path("bogus").unwrap_err();
        match err {
            PathError::UnknownSegment { segment, siblings } => {
                assert_eq!(segment, "bogus");
                assert!(siblings.contains(&"retry".to_string()));
            }
            other => panic!("expected UnknownSegment, got {other:?}"),
        }
    }

    #[test]
    fn descending_into_scalar_is_rejected() {
        let err = validate_config_path("server.host.deeper").unwrap_err();
        assert_eq!(
            err,
            PathError::NotATable {
                segment: "deeper".to_string()
            }
        );
    }

    #[test]
    fn array_target_is_rejected() {
        let err = validate_config_path("bedrock.allowed_betas").unwrap_err();
        assert!(matches!(err, PathError::ArrayTarget { .. }));
    }

    #[test]
    fn indexing_into_array_is_rejected() {
        let err = validate_config_path("bedrock.allowed_betas.0").unwrap_err();
        assert!(matches!(err, PathError::ArrayTarget { .. }));
    }

    #[test]
    fn empty_and_malformed_paths_rejected() {
        assert_eq!(validate_config_path(""), Err(PathError::Empty));
        assert_eq!(
            validate_config_path("server."),
            Err(PathError::EmptySegment)
        );
        assert_eq!(
            validate_config_path(".server"),
            Err(PathError::EmptySegment)
        );
        assert_eq!(
            validate_config_path("server..host"),
            Err(PathError::EmptySegment)
        );
    }

    #[test]
    fn quoted_segments_rejected() {
        assert_eq!(
            validate_config_path("aliases.\"claude-opus-4.5\""),
            Err(PathError::QuotedKey)
        );
    }

    #[test]
    fn error_display_names_segment_and_siblings() {
        let err = validate_config_path("server.nope").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"));
        assert!(msg.contains("host"));
    }
}
