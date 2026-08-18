//! Repair of the ONE field Anthropic requires on every object in an
//! `output_config.format.schema`: `additionalProperties: false`.
//!
//! Measured twice against the live wire (2026-08-11): a structured-output
//! schema whose ROOT or any NESTED object omits `additionalProperties` is
//! rejected with HTTP 400 `For 'object' type, 'additionalProperties' must be
//! explicitly set to false`, while the same schema carrying the key is
//! accepted. Most hand-written JSON schemas do not set it on every nested
//! object, so this is the conventional caller shape, not an edge case.
//!
//! This is a surgical mandatory-field repair, NOT a JSON Schema processor.
//! It supplies a value whose only accepted value is `false`, so it can
//! contradict no caller intent; it never removes or rewrites anything the
//! caller wrote. The distinction matters: STRIPPING a caller's constraint is
//! silent data loss, which this egress does not do.
//!
//! Rules, in order of how easy each is to get wrong:
//!
//! - **Inject only where the key is ABSENT.** A present `false` is left as
//!   is. A present NON-`false` value (`true`, or a schema-valued
//!   `additionalProperties`) is forwarded VERBATIM with one bounded WARN.
//!   Dropping or overwriting it would be stripping. Anthropic will reject
//!   such a body naming the field, which is a truthful upstream verdict; a
//!   locally-enforced "only false is acceptable" rule would instead become a
//!   FALSE REJECTION the day Anthropic accepts anything else.
//! - **Object detection is by SHAPE**, not by `type` alone: `type ==
//!   "object"`, or the presence of `properties`, or the presence of
//!   `additionalProperties`. The dangerous direction is a false POSITIVE
//!   (injecting into a non-object), not a miss -- a miss yields the same 400
//!   the caller gets today, never a fabricated success.
//! - **Descent is a fixed, documented set of positions**, everything else
//!   OPAQUE: `properties.*`, `patternProperties.*`, `$defs.*`,
//!   `definitions.*`, `anyOf.*`, `oneOf.*`, `allOf.*`, `not`, `if`, `then`,
//!   `else`, `items` (single schema or array). Deliberately NOT a
//!   keyword-to-container lookup table: such a table is a permanent
//!   obligation that rots against a large spec and drifts from what
//!   Anthropic actually accepts.
//! - **Instance-valued keywords are never entered**: `const`, `enum`,
//!   `examples`, `default`. An object sitting there is caller DATA, and
//!   inserting a schema keyword into it would corrupt the request. They are
//!   opaque by virtue of not being in the descent set; the test suite pins
//!   that they stay byte-identical.
//! - **`$ref` is never dereferenced.** It is a string, not a container, and
//!   its targets are reached as `$defs`/`definitions` entries directly. A
//!   `serde_json::Value` tree has no in-memory cycles, so declining to
//!   follow references dissolves the `$ref`-cycle problem rather than
//!   guarding it.
//! - **The walk is iterative and bounded, at PUSH time.** An incoming body
//!   can reach 32 MiB, so recursion is not an option and neither is an
//!   unbounded stack. The node budget is therefore spent as children are
//!   PUSHED, not as frames are popped: a single node can hold millions of
//!   `anyOf` branches, so a pop-time check would let the work stack and the
//!   path arena grow proportional to the BODY before the limit could fire --
//!   which is the same unbounded sink the limit exists to prevent. A limit
//!   breach returns a clean error, never a panic.
//! - **Path rendering is paid for only where it is used, and is itself
//!   bounded.** At most `MAX_LOGGED_DIAGNOSTIC_ITEMS` paths are ever
//!   retained, so nothing is rendered once the WARN sample is full; and a
//!   render stops at the length the log field would keep anyway, because a
//!   path segment is a caller-chosen name that can be megabytes long. Both
//!   halves are needed: a deduplicating sample fed equal-prefix names never
//!   fills, so the per-render bound is what keeps the total cost linear in
//!   the node count instead of quadratic in the body.
//!
//! Consulted `crate::gemini::schema` for the container set and its
//! schema-valued-vs-literal discrimination, which it already encodes. Its
//! `$ref` resolver and `visited` cycle guard are deliberately NOT mirrored
//! here: with no dereferencing there is nothing to guard.

use serde_json::{Map, Value};

use routectl_core::{Error, Result, sanitize_for_log};

use crate::bounded_diagnostics::BoundedLogSample;

/// The single key this module repairs.
const ADDITIONAL_PROPERTIES: &str = "additionalProperties";

/// Maximum schema nesting levels the walk descends before failing the
/// request. An ASSUMPTION, not a measured provider limit: no real
/// structured-output schema nests anywhere near this deep, and a body can
/// reach 32 MiB, so the bound exists to keep a pathological schema from
/// turning the walk into an unbounded memory sink.
const MAX_SCHEMA_DEPTH: usize = 256;

/// Maximum schema nodes the walk visits before failing the request. Same
/// standing as [`MAX_SCHEMA_DEPTH`]: an assumption, generous against real
/// schemas, present so the cost of the walk is bounded by something other
/// than the body size limit.
const MAX_SCHEMA_NODES: usize = 100_000;

/// Characters of caller-chosen segment text one rendered path may carry.
/// Matches the cap `routectl_core::sanitize_for_log` applies, so the bound
/// costs the diagnostic nothing the log record would have kept anyway -- the
/// point of enforcing it DURING the render is that a segment is a
/// caller-chosen property name that can itself be megabytes long.
const RENDERED_PATH_CHARS: usize = 256;

/// Structured event name for the not-`false` forward, so an operator can
/// filter the diagnostic without matching on message text.
const ADDITIONAL_PROPERTIES_FORWARD_EVENT: &str = "output_schema_additional_properties_not_false";

/// Positions whose VALUE is a map from caller-chosen names to schemas.
const SCHEMA_MAP_POSITIONS: [&str; 4] = ["properties", "patternProperties", "$defs", "definitions"];

/// Positions whose VALUE is an array of schemas.
const SCHEMA_ARRAY_POSITIONS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

/// Positions whose VALUE is a single schema.
const SCHEMA_POSITIONS: [&str; 4] = ["not", "if", "then", "else"];

/// The `items` position: a single schema, or an array of positional schemas.
const ITEMS: &str = "items";

/// What the repair walk observed but did NOT change, so the caller can emit
/// one diagnostic per request.
///
/// Only the not-`false` forwards are recorded. The injections themselves are
/// routine repair of a mandatory field and carry no operator decision, so
/// counting them would produce a metric nobody acts on.
#[derive(Default)]
pub(super) struct AdditionalPropertiesRepair {
    /// Schema paths carrying a present, non-`false` `additionalProperties`.
    /// Bounded at collection time: a schema can hold arbitrarily many, and
    /// an uncapped sample turns one WARN into an unbounded log record.
    forwarded: BoundedLogSample<String>,
}

impl AdditionalPropertiesRepair {
    /// Union of two repair passes' forwards, for a seam that re-runs the
    /// repair after rewriting `output_config` and must still emit ONE WARN.
    /// The path sample stays bounded: the fold pushes through the same cap.
    pub(super) fn merged(mut self, other: Self) -> Self {
        self.forwarded.absorb(other.forwarded);
        self
    }

    /// Emit the aggregated WARN, if any non-`false` value was forwarded.
    /// Called exactly once per request by the assembly that ran the repair.
    pub(super) fn warn(&self, provider: &str) {
        if self.forwarded.is_empty() {
            return;
        }
        // Paths only. The schema and its VALUES are caller data and never
        // reach a log record -- a path names where the operator should look
        // without reproducing what the caller sent.
        tracing::warn!(
            provider = provider,
            event = ADDITIONAL_PROPERTIES_FORWARD_EVENT,
            paths = ?self.forwarded.items(),
            paths_truncated = self.forwarded.truncated(),
            "output_config.format.schema carries an `additionalProperties` \
             value other than `false` at the named paths; forwarded verbatim. \
             Anthropic requires the key to be explicitly `false` on every \
             object and rejects any other value, so the upstream will reject \
             the request naming the field. routectl does not overwrite it: \
             replacing a value the caller set would silently discard \
             requested semantics."
        );
    }
}

/// Inject `additionalProperties: false` into every object in the ASSEMBLED
/// body's `output_config.format.schema` that omits the key.
///
/// Reading the assembled body is load-bearing, not belt-and-braces.
/// `output_config` is deliberately not a routectl-managed key (see
/// `extras::is_routectl_managed_key`), so `merge_provider_extras` forwards a
/// caller's whole `output_config` verbatim and
/// `request::set_output_config_format`'s `or_insert` then leaves it
/// untouched. A caller-supplied `output_config.format` therefore never
/// passes through `response_format_to_anthropic_format` at all, which makes
/// a converter-side repair a NO-OP for exactly the callers that need it.
/// Same posture, and same reason, as
/// `request::drop_unrepresentable_output_format_keys`.
///
/// A body with no `output_config.format.schema`, or a non-object schema, is
/// left alone and reports nothing.
pub(super) fn inject_additional_properties_false(
    provider: &str,
    body: &mut Map<String, Value>,
) -> Result<AdditionalPropertiesRepair> {
    let Some(schema) = body
        .get_mut("output_config")
        .and_then(Value::as_object_mut)
        .and_then(|oc| oc.get_mut("format"))
        .and_then(Value::as_object_mut)
        .and_then(|format| format.get_mut("schema"))
    else {
        return Ok(AdditionalPropertiesRepair::default());
    };
    repair_schema(provider, schema)
}

/// The bounded iterative walk. Split from the body-shaped entry point above
/// so the traversal can be exercised on a bare schema.
///
/// Child nodes are reached by MATCHING the `&mut Value` rather than calling
/// `as_object_mut` / `as_array_mut`: a method call reborrows for the duration
/// of the local, which cannot outlive one loop iteration, while a match moves
/// the reference and so yields children that live as long as the root borrow
/// the work stack is typed against.
fn repair_schema(provider: &str, root: &mut Value) -> Result<AdditionalPropertiesRepair> {
    let mut repair = AdditionalPropertiesRepair::default();
    let mut pending = Pending::default();
    let root_path = pending.paths.push(None, "schema");
    if !pending.push(root, root_path, 0) {
        return Err(node_limit_error(provider));
    }

    while let Some(frame) = pending.stack.pop() {
        if frame.depth > MAX_SCHEMA_DEPTH {
            return Err(depth_limit_error(provider));
        }
        let map = match frame.node {
            Value::Object(map) => map,
            _ => continue,
        };

        if is_object_shaped(map) {
            match map.get(ADDITIONAL_PROPERTIES) {
                None => {
                    map.insert(ADDITIONAL_PROPERTIES.to_string(), Value::Bool(false));
                }
                Some(Value::Bool(false)) => {}
                // Rendered LAZILY: a path costs O(depth x segment length) and
                // a schema can carry one non-`false` value per node, so
                // rendering every one of them to feed a sample that keeps at
                // most `MAX_LOGGED_DIAGNOSTIC_ITEMS` is work the request never
                // uses. Sanitized because the segments are caller-chosen
                // property names, `patternProperties` regexes and `$defs`
                // keys: without the cap one WARN field carries as many bytes
                // as the caller cares to nest.
                Some(_) => {
                    let paths = &pending.paths;
                    repair
                        .forwarded
                        .push_distinct_lazily(|| sanitize_for_log(&paths.render(frame.path)));
                }
            }
        }

        if !push_children(map, frame.path, frame.depth, &mut pending) {
            return Err(node_limit_error(provider));
        }
    }

    Ok(repair)
}

/// Push every child schema of `map` reachable through the fixed descent set.
/// A position whose value has the wrong JSON shape for its keyword (an
/// `anyOf` that is not an array, a `properties` that is not an object) is
/// skipped rather than guessed at: it is a caller error the upstream will
/// report accurately.
///
/// Returns `false` as soon as the node budget is exhausted, so the caller can
/// fail the request BEFORE the pending set and the path arena have grown past
/// the node limit.
fn push_children<'a>(
    map: &'a mut Map<String, Value>,
    path: usize,
    depth: usize,
    pending: &mut Pending<'a>,
) -> bool {
    for (key, value) in map {
        let key = key.as_str();
        if SCHEMA_MAP_POSITIONS.contains(&key) {
            let container = pending.paths.push(Some(path), key);
            if let Value::Object(entries) = value {
                for (name, child) in entries {
                    let child_path = pending.paths.push(Some(container), name);
                    if !pending.push(child, child_path, depth + 1) {
                        return false;
                    }
                }
            }
        } else if SCHEMA_ARRAY_POSITIONS.contains(&key) {
            let container = pending.paths.push(Some(path), key);
            if !push_branches(value, container, depth, pending) {
                return false;
            }
        } else if SCHEMA_POSITIONS.contains(&key) {
            let child_path = pending.paths.push(Some(path), key);
            if !pending.push(value, child_path, depth + 1) {
                return false;
            }
        } else if key == ITEMS {
            // `items` is either a single schema (the common case) or an array
            // of positional schemas. Both are schema positions, so both are
            // descended.
            let container = pending.paths.push(Some(path), key);
            if value.is_array() {
                if !push_branches(value, container, depth, pending) {
                    return false;
                }
            } else if !pending.push(value, container, depth + 1) {
                return false;
            }
        }
        // Everything else is OPAQUE, which is what keeps instance-valued
        // keywords (`const`, `enum`, `examples`, `default`) and a
        // schema-valued `additionalProperties` unentered and unmodified.
    }
    true
}

/// Push each element of an array-valued schema position. Returns `false` on
/// an exhausted budget, per [`push_children`].
fn push_branches<'a>(
    value: &'a mut Value,
    container: usize,
    depth: usize,
    pending: &mut Pending<'a>,
) -> bool {
    let Value::Array(branches) = value else {
        return true;
    };
    for (index, branch) in branches.iter_mut().enumerate() {
        let branch_path = pending.paths.push(Some(container), &index.to_string());
        if !pending.push(branch, branch_path, depth + 1) {
            return false;
        }
    }
    true
}

/// Whether a schema object is confidently object-shaped and therefore needs
/// the mandatory key.
///
/// `type` is matched as the exact string `"object"`. A multi-member `type`
/// union (`["object", "null"]`) is not read as object-shaped on the `type`
/// key alone -- one carrying `properties` is still caught by that arm, and
/// declining to widen the match keeps the false-positive direction closed on
/// a shape whose upstream treatment is unmeasured.
fn is_object_shaped(map: &Map<String, Value>) -> bool {
    map.get("type").and_then(Value::as_str) == Some("object")
        || map.contains_key("properties")
        || map.contains_key(ADDITIONAL_PROPERTIES)
}

/// A limit breach is a clean request-normalization error. Neither message
/// echoes any part of the schema.
fn depth_limit_error(provider: &str) -> Error {
    Error::normalize_request(
        provider,
        format!(
            "output_config.format.schema nests deeper than the \
             {MAX_SCHEMA_DEPTH}-level limit of the additionalProperties repair \
             walk; flatten the schema or split the request"
        ),
    )
}

fn node_limit_error(provider: &str) -> Error {
    Error::normalize_request(
        provider,
        format!(
            "output_config.format.schema holds more than {MAX_SCHEMA_NODES} \
             schema nodes, the limit of the additionalProperties repair walk; \
             split the request"
        ),
    )
}

/// One pending node of the walk.
struct Frame<'a> {
    node: &'a mut Value,
    /// Index into the [`PathArena`], not a rendered path.
    path: usize,
    depth: usize,
}

/// The remaining [`MAX_SCHEMA_NODES`] allowance, spent as nodes enter the
/// pending set.
///
/// Counting at PUSH rather than at pop is what makes the limit bound the
/// walk's memory: one schema node can carry millions of children, so a
/// pop-time counter admits the whole fan-out into the work stack and the
/// path arena before it can fire.
struct NodeBudget {
    remaining: usize,
}

impl NodeBudget {
    const fn new() -> Self {
        Self {
            remaining: MAX_SCHEMA_NODES,
        }
    }

    /// Spend one node's allowance. `false` means the schema holds more nodes
    /// than the limit permits and the walk must fail.
    const fn claim(&mut self) -> bool {
        match self.remaining.checked_sub(1) {
            Some(left) => {
                self.remaining = left;
                true
            }
            None => false,
        }
    }
}

/// The walk's mutable state: what is still to visit, the interned paths, and
/// the node allowance those two share. Bundled into one value because they
/// are only ever correct together -- a frame entering the stack must also
/// spend a node's budget, and letting a caller push one without the other is
/// precisely the defect that let the pending set grow with the body.
#[derive(Default)]
struct Pending<'a> {
    stack: Vec<Frame<'a>>,
    paths: PathArena,
    budget: NodeBudget,
}

impl Default for NodeBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Pending<'a> {
    /// Admit one node to the walk. `false` means the node limit is reached
    /// and the walk must fail before this node is queued.
    fn push(&mut self, node: &'a mut Value, path: usize, depth: usize) -> bool {
        if !self.budget.claim() {
            return false;
        }
        self.stack.push(Frame { node, path, depth });
        true
    }
}

/// Interned path segments, so a walk over up to [`MAX_SCHEMA_NODES`] nodes
/// holds ONE short segment per node rather than a fully-rendered path per
/// pending frame. Paths are rendered only for the handful that reach the
/// bounded WARN sample -- see the lazy push in [`repair_schema`], which is
/// what keeps rendering off the per-node path.
#[derive(Default)]
struct PathArena {
    segments: Vec<(Option<usize>, String)>,
}

impl PathArena {
    fn push(&mut self, parent: Option<usize>, segment: &str) -> usize {
        #[cfg(test)]
        probe::record_segment();
        self.segments.push((parent, segment.to_string()));
        self.segments.len() - 1
    }

    /// Render a dotted path from the arena root to `index`, stopping at
    /// [`RENDERED_PATH_CHARS`].
    ///
    /// The cap is enforced WHILE rendering rather than afterwards. A segment
    /// is a caller-chosen property name and can itself be megabytes long, so
    /// materializing the whole path and then trimming it is the expensive
    /// half of the work: an equal-prefix set of long names also renders to
    /// one identical capped string, which keeps the deduplicating sample
    /// unfilled and so keeps this path live for every node in the schema.
    fn render(&self, index: usize) -> String {
        #[cfg(test)]
        probe::record_render();
        let mut parts: Vec<&str> = Vec::new();
        let mut cursor = Some(index);
        while let Some(i) = cursor {
            let (parent, segment) = &self.segments[i];
            parts.push(segment);
            cursor = *parent;
        }
        let mut out = String::with_capacity(RENDERED_PATH_CHARS);
        let mut remaining = RENDERED_PATH_CHARS;
        for (position, segment) in parts.iter().rev().enumerate() {
            if remaining == 0 {
                break;
            }
            if position > 0 {
                out.push('.');
                remaining -= 1;
            }
            for c in segment.chars().take(remaining) {
                out.push(c);
                remaining -= 1;
            }
        }
        #[cfg(test)]
        probe::record_rendered_chars(out.chars().count());
        out
    }
}

/// Test-only tallies of what the walk SPENDS: how many paths it rendered, how
/// many characters those renders materialized, and how many arena segments it
/// interned. All three are the cost the bounds exist to contain, and none is
/// observable from the walk's return value, so the suite asserts on these
/// counts. Wall-clock assertions are not an option -- they are the
/// timing-flake class this repo keeps out of the default test path.
#[cfg(test)]
mod probe {
    use std::cell::Cell;

    thread_local! {
        static RENDERS: Cell<usize> = const { Cell::new(0) };
        static RENDERED_CHARS: Cell<usize> = const { Cell::new(0) };
        static SEGMENTS: Cell<usize> = const { Cell::new(0) };
    }

    /// Each `#[test]` owns its thread, so the thread-local tallies need no
    /// cross-test coordination; a test resets before it measures.
    pub(super) fn reset() {
        RENDERS.with(|c| c.set(0));
        RENDERED_CHARS.with(|c| c.set(0));
        SEGMENTS.with(|c| c.set(0));
    }

    pub(super) fn record_render() {
        RENDERS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn record_rendered_chars(chars: usize) {
        RENDERED_CHARS.with(|c| c.set(c.get() + chars));
    }

    pub(super) fn record_segment() {
        SEGMENTS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn renders() -> usize {
        RENDERS.with(Cell::get)
    }

    pub(super) fn rendered_chars() -> usize {
        RENDERED_CHARS.with(Cell::get)
    }

    pub(super) fn segments() -> usize {
        SEGMENTS.with(Cell::get)
    }
}

#[cfg(test)]
#[path = "output_schema_tests.rs"]
mod tests;
