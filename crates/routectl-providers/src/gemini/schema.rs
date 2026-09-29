//! JSON Schema -> Gemini OpenAPI-subset cleaning for the shared `Schema` proto.
//!
//! Gemini's `functionDeclarations[].parameters` and
//! `generationConfig.responseSchema` are the SAME proto and accept an
//! OpenAPI 3.0 Schema subset, not raw JSON Schema. A caller tool or
//! structured-output schema authored for OpenAI/Anthropic can carry
//! constructs Gemini rejects with a 400 or silently mis-parses. This module
//! normalizes a caller schema into the subset before emit:
//!
//!   - `oneOf` -> `anyOf` (Gemini has no `oneOf`).
//!   - Resolve an intra-document `$ref` (`#/$defs/X`, `#/definitions/X`) by
//!     inlining its cleaned target so nested-model shape survives; peer
//!     proxies do the same. Cyclic/self-referential refs terminate with a
//!     bounded empty schema, and an unresolvable ref degrades to a drop.
//!   - `allOf` (Gemini has no such keyword) is MERGED into its parent: each
//!     branch is cleaned, then folded in order over the parent's own keys.
//!     `properties` union by name (a shared name must clean to equal values),
//!     `required` is an ordered union, the annotations `title`,
//!     `description`, `default`, `example` and `examples` keep the first
//!     writer (the parent), and every other key must be equal across sources.
//!     A branch of `true` is a no-op. Any disagreement, a `false` or
//!     non-object branch, or a non-array `allOf` abandons the merge: the
//!     `allOf` contents are dropped, the parent's own keys are kept, and the
//!     loss is reported.
//!   - Strip keywords Gemini's Schema proto rejects: `$schema`,
//!     `additionalProperties`, `$defs`/`definitions`, `not`, `const`,
//!     `patternProperties`. The `$defs`/`definitions` containers never reach
//!     the wire; only their inlined ref targets do.
//!   - `required` is filtered to names that are keys of the final
//!     `properties` (Gemini rejects a dangling reference); an emptied
//!     `required` is omitted, and removing a name is reported as a loss.
//!   - Nullable: `type: [T, "null"]` -> `type: T` + `nullable: true`;
//!     an explicit `nullable` is preserved.
//!   - A multi-concrete-member `type` union (`["string","integer"]`) is
//!     lowered to an `anyOf` of single-`type` branches (Gemini has no
//!     multi-type `type`); a `"null"` member still lifts to `nullable`.
//!   - `format` is allowlisted to Gemini-supported values, otherwise dropped.
//!   - Numeric/boolean `enum` entries coerced to strings (Gemini's enum
//!     is a repeated string).
//!   - `type` uppercased to Gemini's TYPE enum (STRING, INTEGER, ...).
//!
//! Recurses only through genuinely schema-valued keywords (`properties`,
//! `items`, `prefixItems`, `allOf`, `anyOf`/`oneOf` branches); literal-valued
//! keywords (`default`, `example`, `title`, ...) are cloned verbatim so a
//! `type` field inside a VALUE is never misread as a schema type keyword.
//!
//! Ceilings are request-scoped: one [`SchemaBudget`] is threaded through every
//! schema a request carries (each tool's parameters and the response schema),
//! so for one request the cleaner's total retained output is bounded by
//! [`MAX_SCHEMA_BYTES`] and [`MAX_SCHEMA_NODES`] across ALL schemas in that
//! request, and its CPU is bounded by the input size plus that budget: work an
//! `allOf` fold repeats at each nesting level (moving keys, `required` members
//! and property names into the parent) is charged at every level it happens.
//! The scans that are not charged per level -- filtering a `required` list
//! against `properties`, and comparing values a fold already holds -- cost a
//! small constant number of passes per level over text that was charged when
//! emitted, at most [`MAX_SCHEMA_DEPTH`] levels; a filter that removes a name
//! briefly holds a second copy of the kept names while it rebuilds the list.
//! [`MAX_SCHEMA_DEPTH`] bounds the schema levels of any one schema. Nodes are
//! schema-valued positions (objects, boolean schemas and any other value
//! standing where a schema is expected). A `$ref` is re-cleaned at every ref
//! site, so a small document can fan out into an enormous schema, either in
//! nodes or by re-cloning one large literal; every literal is charged BEFORE
//! it is cloned, at each site it is emitted, so memory stays bounded by the
//! ceiling rather than by the input's fan-out. One cost model prices every
//! emitted value: at least [`VALUE_OVERHEAD`] each (so empty strings and
//! empty arrays are not free), plus the string length, plus
//! [`VALUE_OVERHEAD`] and the key length per object entry and
//! [`OBJECT_OVERHEAD`] per object. A `type` union lowered to `anyOf` charges
//! each generated branch the same way, and is refused outright when it names
//! more distinct types than Gemini's TYPE enum has members. A `type` name is
//! charged at its uppercased length, which can exceed its input length for
//! non-ASCII text. An `enum` is
//! charged at its coerced size, the string form of each number and boolean.
//! A `$ref` pointer is charged its length at every visit, and one longer than
//! [`MAX_REF_POINTER_BYTES`] is treated as unresolvable without being decoded.
//! Exceeding any ceiling returns [`SchemaTooLarge`] rather than truncating,
//! and no partial schema escapes.
//!
//! Pure function of its input -- no logging, no mutation. Several of the
//! strips above LOSE a caller-stated constraint rather than renormalizing
//! it, so [`clean_schema_reporting`] returns that fact for the egress's
//! per-request tally to log and count; nothing in here logs.

use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Deepest schema nesting the cleaner will walk. Literally written schemas
/// stay far below it (the JSON parse limit is 128 levels and a schema level
/// costs at least two); only a `$ref` chain can reach it.
pub(super) const MAX_SCHEMA_DEPTH: usize = 64;

/// Most schema-valued positions one request will clean. Depth alone cannot
/// bound the `$ref` fan-out, where each ref site re-cleans its target.
pub(super) const MAX_SCHEMA_NODES: usize = 10_000;

/// Most bytes one request will emit, priced by the model in the module doc.
/// Nodes bound objects, not the size of a description or default that a
/// `$ref` re-emits at every site. 8 MiB is far above what a hand-written tool
/// schema needs yet small enough that the allocation one request can force
/// stays a few multiples of it.
pub(super) const MAX_SCHEMA_BYTES: usize = 8 * 1024 * 1024;

/// Longest `$ref` pointer the cleaner will look up. A def name past this is
/// not a legitimate one, and the bound keeps the per-visit hashing and
/// decoding of a pointer from scaling with caller-chosen text.
const MAX_REF_POINTER_BYTES: usize = 1024;

/// Minimum bytes charged for any emitted JSON value, whatever it holds. Set
/// at `size_of::<Value>()` (32 bytes on 64-bit targets) so the heap-free
/// values -- an empty string, an empty array, a number -- still cost the slot
/// they occupy in their parent.
const VALUE_OVERHEAD: usize = 32;

/// Flat bytes charged per emitted object for its map allocation, on top of
/// [`VALUE_OVERHEAD`] per entry, so an empty object is not free.
const OBJECT_OVERHEAD: usize = 64;

/// Bytes charged for one emitted object, before its entries.
const OBJECT_COST: usize = VALUE_OVERHEAD + OBJECT_OVERHEAD;

const _: () = assert!(VALUE_OVERHEAD >= size_of::<Value>());

/// Number of members in Gemini's TYPE enum (STRING, NUMBER, INTEGER,
/// BOOLEAN, ARRAY, OBJECT, NULL). A `type` union naming more distinct
/// concrete types than this cannot be a legitimate schema.
const GEMINI_TYPE_COUNT: usize = 7;

/// Annotation keywords that carry no constraint; when several `allOf` sources
/// state one, the first writer (the parent, then branches in order) wins
/// instead of the disagreement abandoning the merge.
const FIRST_WRITER_KEYWORDS: &[&str] = &["title", "description", "default", "example", "examples"];

/// A caller schema exceeded a cleaning ceiling. Carries only which ceiling
/// and its value -- never schema text, property names, or ref pointers -- so
/// it is safe to log and to return to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SchemaTooLarge {
    pub limit: &'static str,
    pub max: usize,
}

impl SchemaTooLarge {
    const fn depth() -> Self {
        Self {
            limit: "depth",
            max: MAX_SCHEMA_DEPTH,
        }
    }

    const fn nodes() -> Self {
        Self {
            limit: "nodes",
            max: MAX_SCHEMA_NODES,
        }
    }

    const fn bytes() -> Self {
        Self {
            limit: "bytes",
            max: MAX_SCHEMA_BYTES,
        }
    }
}

impl fmt::Display for SchemaTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "schema exceeds the {} limit of {}", self.limit, self.max)
    }
}

/// Normalize a caller JSON Schema into the Gemini OpenAPI subset, discarding
/// the drop report [`clean_schema_reporting`] returns. Test-only: every
/// production call site must consume the report so the loss is counted.
#[cfg(test)]
fn clean_schema(schema: &Value) -> Value {
    clean_schema_alone(schema)
        .expect("test schema is within the cleaning ceilings")
        .0
}

/// [`clean_schema_reporting`] against a budget of its own, for tests that
/// exercise one schema in isolation.
#[cfg(test)]
pub(super) fn clean_schema_alone(schema: &Value) -> Result<(Value, bool), SchemaTooLarge> {
    clean_schema_reporting(schema, &mut SchemaBudget::default())
}

/// Normalize a caller JSON Schema into the Gemini OpenAPI subset, reporting
/// whether anything the caller STATED was lost rather than merely
/// renormalized.
///
/// The top-level schema is treated as the ref document root: its
/// `$defs`/`definitions` back any intra-document `$ref` encountered at any
/// depth. Those defs are collected once here, borrowed rather than copied, and
/// threaded through the recursion so nested refs resolve against the document
/// root.
///
/// `budget` is charged for everything this call emits and is shared by every
/// schema of one request, so the ceilings hold for the request as a whole.
///
/// The second element is `true` when at least one constraint keyword was
/// stripped, an unsupported `format` was dropped, a `$ref` degraded to an
/// empty schema, an `allOf` could not be merged, or a `required` entry was
/// removed -- i.e. the model will not see a restriction the caller wrote.
/// Returned rather than logged so this module stays a pure function of its
/// input; the egress's per-request tally owns the WARN and the counter.
///
/// # Errors
///
/// [`SchemaTooLarge`] when the schema exceeds [`MAX_SCHEMA_DEPTH`], or when
/// `budget` runs past [`MAX_SCHEMA_NODES`] or [`MAX_SCHEMA_BYTES`].
pub(super) fn clean_schema_reporting(
    schema: &Value,
    budget: &mut SchemaBudget,
) -> Result<(Value, bool), SchemaTooLarge> {
    let defs = collect_defs(schema);
    let mut walk = Walk {
        defs: &defs,
        visited: HashSet::new(),
        dropped: false,
        budget,
    };
    let cleaned = clean_value(schema, &mut walk, 0)?;
    Ok((cleaned, walk.dropped))
}

/// Node and byte spend of one request's schemas, shared by every
/// [`clean_schema_reporting`] call the request makes.
#[derive(Debug, Default)]
pub(super) struct SchemaBudget {
    nodes: usize,
    bytes: usize,
}

impl SchemaBudget {
    /// Charge one schema-valued position against the node ceiling.
    const fn charge_node(&mut self) -> Result<(), SchemaTooLarge> {
        self.nodes += 1;
        if self.nodes > MAX_SCHEMA_NODES {
            return Err(SchemaTooLarge::nodes());
        }
        Ok(())
    }

    /// Charge `count` emitted bytes against the byte ceiling.
    const fn charge_bytes(&mut self, count: usize) -> Result<(), SchemaTooLarge> {
        self.bytes = self.bytes.saturating_add(count);
        if self.bytes > MAX_SCHEMA_BYTES {
            return Err(SchemaTooLarge::bytes());
        }
        Ok(())
    }

    /// Charge an object key that is about to be copied into the output.
    const fn charge_key(&mut self, key: &str) -> Result<(), SchemaTooLarge> {
        self.charge_bytes(entry_cost(key))
    }

    /// Charge a caller value that is about to be cloned into the output. The
    /// measurement stops at the remaining budget, so an over-ceiling value is
    /// refused before any of it is copied.
    fn charge_literal(&mut self, value: &Value) -> Result<(), SchemaTooLarge> {
        let remaining = MAX_SCHEMA_BYTES.saturating_sub(self.bytes);
        let size = literal_size(value, remaining).ok_or_else(SchemaTooLarge::bytes)?;
        self.charge_bytes(size)
    }

    /// Charge a `type` value at the size it will have once its names are
    /// uppercased, before anything is uppercased: case mapping can grow
    /// non-ASCII text.
    fn charge_type(&mut self, value: &Value) -> Result<(), SchemaTooLarge> {
        match value {
            Value::String(name) => self.charge_bytes(VALUE_OVERHEAD + uppercased_len(name)),
            Value::Array(members) => {
                self.charge_bytes(VALUE_OVERHEAD)?;
                for member in members {
                    match member {
                        Value::String(name) => {
                            self.charge_bytes(VALUE_OVERHEAD + uppercased_len(name))?;
                        }
                        other => self.charge_literal(other)?,
                    }
                }
                Ok(())
            }
            other => self.charge_literal(other),
        }
    }

    /// Charge an `enum` at the size it will have once its numbers and booleans
    /// are coerced to strings, one entry at a time so an over-ceiling list is
    /// refused after O(budget) work and before anything is materialized.
    fn charge_enum(&mut self, value: &Value) -> Result<(), SchemaTooLarge> {
        let Value::Array(items) = value else {
            return self.charge_literal(value);
        };
        self.charge_bytes(VALUE_OVERHEAD)?;
        for item in items {
            match item {
                Value::Number(number) => {
                    self.charge_bytes(VALUE_OVERHEAD + display_len(number))?;
                }
                Value::Bool(flag) => {
                    self.charge_bytes(VALUE_OVERHEAD + display_len(flag))?;
                }
                other => self.charge_literal(other)?,
            }
        }
        Ok(())
    }
}

/// Byte length of `text` after `str::to_uppercase`, counted without building it.
fn uppercased_len(text: &str) -> usize {
    text.chars()
        .flat_map(char::to_uppercase)
        .map(char::len_utf8)
        .sum()
}

/// Length of `value`'s `Display` text, counted without building it.
fn display_len(value: &impl fmt::Display) -> usize {
    struct Counter(usize);
    impl fmt::Write for Counter {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.0 += text.len();
            Ok(())
        }
    }
    let mut counter = Counter(0);
    // Counter::write_str never fails, so neither does the formatting.
    let _ = fmt::Write::write_fmt(&mut counter, format_args!("{value}"));
    counter.0
}

/// Ref-resolution table: the root's defs keyed by their JSON-pointer ref,
/// borrowing the def bodies from the caller's document.
type Defs<'a> = HashMap<String, &'a Value>;

/// Mutable state of one cleaning call.
struct Walk<'a> {
    defs: &'a Defs<'a>,
    /// Refs on the current resolution path, for cycle protection.
    visited: HashSet<&'a str>,
    dropped: bool,
    budget: &'a mut SchemaBudget,
}

impl Walk<'_> {
    /// Account for one schema object at `depth` (the parent's level; the root
    /// call passes 0) and return its own level.
    fn enter(&mut self, depth: usize) -> Result<usize, SchemaTooLarge> {
        let level = depth + 1;
        if level > MAX_SCHEMA_DEPTH {
            return Err(SchemaTooLarge::depth());
        }
        self.budget.charge_node()?;
        self.budget.charge_bytes(OBJECT_COST)?;
        Ok(level)
    }
}

/// Bytes charged for one object entry's key.
const fn entry_cost(key: &str) -> usize {
    VALUE_OVERHEAD + key.len()
}

/// Bytes charged for `value` itself, excluding what it contains.
const fn own_cost(value: &Value) -> usize {
    match value {
        Value::String(text) => VALUE_OVERHEAD + text.len(),
        Value::Object(_) => OBJECT_COST,
        _ => VALUE_OVERHEAD,
    }
}

/// A container whose children have not all been visited yet.
enum Frame<'a> {
    Items(std::slice::Iter<'a, Value>),
    Entries(serde_json::map::Iter<'a>),
}

impl<'a> Frame<'a> {
    fn of(value: &'a Value) -> Option<Self> {
        match value {
            Value::Array(items) => Some(Self::Items(items.iter())),
            Value::Object(entries) => Some(Self::Entries(entries.iter())),
            _ => None,
        }
    }

    /// Next child and the bytes charged for the entry key that leads to it.
    fn advance(&mut self) -> Option<(usize, &'a Value)> {
        match self {
            Self::Items(items) => items.next().map(|item| (0, item)),
            Self::Entries(entries) => entries.next().map(|(key, value)| (entry_cost(key), value)),
        }
    }
}

/// Emitted size of `value` under the module's cost model, or `None` as soon
/// as it exceeds `limit`. Children are visited lazily, one at a time, so an
/// over-ceiling array or object is refused after O(`limit`) work rather than
/// after enqueueing all of it; the stack is explicit, so a deeply nested
/// literal cannot overflow the call stack.
fn literal_size(value: &Value, limit: usize) -> Option<usize> {
    let mut total = own_cost(value);
    if total > limit {
        return None;
    }
    let mut stack: Vec<Frame<'_>> = Frame::of(value).into_iter().collect();
    while let Some(frame) = stack.last_mut() {
        let Some((key_cost, child)) = frame.advance() else {
            stack.pop();
            continue;
        };
        total = total
            .saturating_add(key_cost)
            .saturating_add(own_cost(child));
        if total > limit {
            return None;
        }
        stack.extend(Frame::of(child));
    }
    Some(total)
}

/// Collect the root `$defs`/`definitions` object schemas into a lookup keyed
/// by their JSON-pointer ref (`#/$defs/Name`, `#/definitions/Name`). Only the
/// key is built; each def body stays where the caller put it.
fn collect_defs(schema: &Value) -> Defs<'_> {
    let mut defs = HashMap::new();
    if let Value::Object(map) = schema {
        for container in ["$defs", "definitions"] {
            if let Some(Value::Object(entries)) = map.get(container) {
                for (name, target) in entries {
                    defs.insert(format!("#/{container}/{name}"), target);
                }
            }
        }
    }
    defs
}

fn clean_value(schema: &Value, walk: &mut Walk<'_>, depth: usize) -> Result<Value, SchemaTooLarge> {
    match schema {
        Value::Object(map) => clean_object(map, walk, depth),
        Value::Array(items) => {
            walk.budget.charge_bytes(VALUE_OVERHEAD)?;
            items
                .iter()
                .map(|item| {
                    // An array directly inside an array is not a schema level, so
                    // it never passes through `Walk::enter`; count it here so a
                    // hand-built value cannot recurse past the depth ceiling.
                    let below = if item.is_array() { depth + 1 } else { depth };
                    if below > MAX_SCHEMA_DEPTH {
                        return Err(SchemaTooLarge::depth());
                    }
                    clean_value(item, walk, below)
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        // A boolean schema, or any other non-object value standing where a
        // schema is expected, is still a node and still a clone.
        other => {
            walk.budget.charge_node()?;
            walk.budget.charge_literal(other)?;
            Ok(other.clone())
        }
    }
}

/// Resolve an intra-document `$ref` to its cleaned target, with path-based
/// cycle protection. A revisited pointer (cycle), a missing target or a
/// pointer longer than [`MAX_REF_POINTER_BYTES`] yields `None`, degrading the
/// ref site to a bounded terminal at the call site. Each lookup is charged the
/// pointer's text, since it is decoded and hashed at every visit.
fn resolve_ref(
    pointer: &str,
    walk: &mut Walk<'_>,
    depth: usize,
) -> Result<Option<Value>, SchemaTooLarge> {
    if pointer.len() > MAX_REF_POINTER_BYTES {
        return Ok(None);
    }
    walk.budget.charge_bytes(entry_cost(pointer))?;
    let decoded = decode_ref_pointer(pointer);
    let defs = walk.defs;
    let Some((key, target)) = defs.get_key_value(decoded.as_ref()) else {
        return Ok(None);
    };
    if !walk.visited.insert(key.as_str()) {
        return Ok(None);
    }
    let cleaned = clean_value(target, walk, depth);
    walk.visited.remove(key.as_str());
    cleaned.map(Some)
}

/// Decode JSON Pointer escapes (RFC 6901) in a `$ref` so an escaped def name
/// matches the raw key `collect_defs` stored: `~1` -> `/` and `~0` -> `~`,
/// in one left-to-right pass so a literal `~1` written as `~01` is not
/// mangled. Container segments (`$defs`/`definitions`) carry no escapable
/// characters, so decoding the whole pointer reconstructs the raw-joined key.
/// A pointer with no `~` is borrowed, not copied.
fn decode_ref_pointer(pointer: &str) -> Cow<'_, str> {
    if !pointer.contains('~') {
        return Cow::Borrowed(pointer);
    }
    let mut decoded = String::with_capacity(pointer.len());
    let mut chars = pointer.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('~', Some('1')) => {
                decoded.push('/');
                chars.next();
            }
            ('~', Some('0')) => {
                decoded.push('~');
                chars.next();
            }
            _ => decoded.push(c),
        }
    }
    Cow::Owned(decoded)
}

fn clean_object(
    map: &Map<String, Value>,
    walk: &mut Walk<'_>,
    depth: usize,
) -> Result<Value, SchemaTooLarge> {
    let mut cleaned = clean_object_unpruned(map, walk, depth)?;
    if prune_required(&mut cleaned) {
        walk.dropped = true;
    }
    Ok(Value::Object(cleaned))
}

/// Clean one schema object without filtering its `required` list. An `allOf`
/// branch is cleaned through this so a branch that states `required` for
/// properties its siblings declare is judged against the merged object, not
/// against itself.
fn clean_object_unpruned(
    map: &Map<String, Value>,
    walk: &mut Walk<'_>,
    depth: usize,
) -> Result<Map<String, Value>, SchemaTooLarge> {
    let depth = walk.enter(depth)?;
    let mut out = Map::new();
    // A resolvable `$ref` inlines its cleaned target as the base schema;
    // sibling keywords (rare, per JSON Schema 2020-12) override it.
    let mut inlined: Option<Map<String, Value>> = None;
    let mut all_of: Option<&Value> = None;
    for (key, value) in map {
        match key.as_str() {
            // Resolve intra-document refs by inlining; drop the `$ref`
            // itself. An unresolvable or cyclic ref leaves `inlined` unset,
            // degrading to the prior drop/empty behavior with no panic.
            //
            // Cross-dialect translation lane: a caller schema authored for
            // OpenAI/Anthropic (pydantic, zod) reaching Gemini's OpenAPI-subset
            // `Schema` proto. Drop rather than forward -- a `$ref` Gemini's
            // proto has no keyword for would 400 the whole request, whereas the
            // unresolvable case degrades to an unconstrained schema that still
            // reaches the model. Only the DEGRADED case is a content loss; an
            // inlined ref preserves the shape, so only the former is tallied.
            // Baked seed verdict: deletion-blocked pending this lane's own wire
            // evidence.
            // TRANSLATION-DROP: lane=gemini class=schema_keyword_unsupported test=unresolvable_ref_reports_a_drop
            "$ref" => {
                if let Value::String(pointer) = value
                    && let Some(Value::Object(target)) = resolve_ref(pointer, walk, depth)?
                {
                    inlined = Some(target);
                } else {
                    walk.dropped = true;
                }
            }
            // Keywords Gemini's Schema proto rejects: drop them rather than
            // pass through and 400. `$defs`/`definitions` only back `$ref`
            // and are inlined at ref sites, so the containers are dead weight
            // on the wire. `not`/`const`/`patternProperties` have no Gemini
            // equivalent; dropping loses the constraint (const loses the pin)
            // but avoids a hard 400 on common pydantic/zod schemas.
            //
            // Cross-dialect translation lane: same as `$ref` above. Two shapes
            // share this arm and only one loses anything. `$schema` and the
            // `$defs`/`definitions` containers are STRUCTURAL -- metadata and a
            // ref sidecar whose contents already reach the wire inlined at each
            // ref site -- so they are not tallied. `additionalProperties`,
            // `not`, `const` and `patternProperties` ARE caller-stated
            // constraints with no Gemini keyword to carry them, so the model
            // never learns them: tallied. Baked seed verdict, deletion-blocked
            // pending this lane's own wire evidence.
            // TRANSLATION-DROP: lane=gemini class=schema_keyword_unsupported test=unsupported_keywords_report_a_drop
            "$schema" | "$defs" | "definitions" => {}
            "additionalProperties" | "not" | "const" | "patternProperties" => {
                walk.dropped = true;
            }
            "allOf" => all_of = Some(value),
            "oneOf" | "anyOf" => {
                walk.budget.charge_key("anyOf")?;
                out.insert("anyOf".to_string(), clean_value(value, walk, depth)?);
            }
            "type" => {
                walk.budget.charge_key("type")?;
                walk.budget.charge_type(value)?;
                insert_type(&mut out, value, walk)?;
            }
            "enum" => {
                walk.budget.charge_key("enum")?;
                walk.budget.charge_enum(value)?;
                out.insert("enum".to_string(), coerce_enum(value));
            }
            // Gemini accepts `format` only for a small closed set; any other
            // value (uri/email/uuid/date/...) is dropped, not passed through.
            //
            // Cross-dialect translation lane: a caller `format` outside
            // Gemini's closed set. Drop rather than forward -- Gemini's Schema
            // proto rejects an unknown `format` outright, so forwarding turns a
            // lost hint into a failed request; the caller's validation
            // expectation is what is lost. Baked seed verdict, deletion-blocked
            // pending this lane's own wire evidence.
            // TRANSLATION-DROP: lane=gemini class=schema_keyword_unsupported test=unsupported_format_reports_a_drop
            "format" => {
                if value.as_str().is_some_and(is_supported_format) {
                    walk.budget.charge_key("format")?;
                    walk.budget.charge_literal(value)?;
                    out.insert("format".to_string(), value.clone());
                } else {
                    walk.dropped = true;
                }
            }
            // Name-keyed schema map: child keys are user-chosen property
            // names, not keywords, so recurse per-value without keyword
            // interpretation.
            "properties" => {
                walk.budget.charge_key(key)?;
                out.insert(key.clone(), clean_schema_map(value, walk, depth)?);
            }
            // Genuinely schema-valued keywords: recurse as schemas.
            "items" | "prefixItems" => {
                walk.budget.charge_key(key)?;
                out.insert(key.clone(), clean_value(value, walk, depth)?);
            }
            // Literal-valued keywords (`default`, `example`, `examples`,
            // `title`, `description`) and any unrecognized keyword carry data
            // VALUES, not nested schemas -- recursing would misread a value's
            // `type` field as a schema type keyword and corrupt it. Clone
            // verbatim.
            _ => {
                walk.budget.charge_key(key)?;
                walk.budget.charge_literal(value)?;
                out.insert(key.clone(), value.clone());
            }
        }
    }
    let mut merged = inlined.unwrap_or_default();
    merged.extend(out);
    match all_of {
        Some(all_of) => apply_all_of(merged, all_of, walk, depth),
        None => Ok(merged),
    }
}

/// Fold an `allOf` into `base` (the parent's own keys over any inlined
/// `$ref`), falling back to `base` alone when the branches cannot be merged.
/// Every branch is cleaned first, so refs, nested `allOf`, strips and
/// nullable lifting resolve exactly as they do anywhere else.
fn apply_all_of(
    base: Map<String, Value>,
    all_of: &Value,
    walk: &mut Walk<'_>,
    depth: usize,
) -> Result<Map<String, Value>, SchemaTooLarge> {
    let cleaned = match all_of {
        Value::Array(branches) => Some(
            branches
                .iter()
                .map(|branch| match branch {
                    Value::Object(map) => {
                        clean_object_unpruned(map, walk, depth).map(Value::Object)
                    }
                    other => clean_value(other, walk, depth),
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => None,
    };
    // Cross-dialect translation lane: a pydantic/zod `allOf` reaching Gemini's
    // Schema proto, which has no such keyword. Merging keeps the branches'
    // constraints; the fallback below fires only when the sources disagree (or
    // a branch is not an object schema), where no single Gemini schema states
    // all of them. Drop the `allOf` contents rather than forward -- an `allOf`
    // on the wire 400s the request, while the parent's own keys still
    // constrain the model. The merge itself loses nothing, so only the
    // fallback is tallied. Baked seed verdict, deletion-blocked pending this
    // lane's own wire evidence.
    // TRANSLATION-DROP: lane=gemini class=schema_keyword_unsupported test=all_of_conflict_reports_a_drop
    let folded = match cleaned {
        Some(branches) => fold_all_of(base, branches, walk.budget)?,
        None => Err(base),
    };
    match folded {
        Ok(folded) => Ok(folded),
        Err(unmerged) => {
            walk.dropped = true;
            Ok(unmerged)
        }
    }
}

/// Why an `allOf` fold stopped: the sources disagree, or the request budget
/// ran out.
enum FoldStop {
    Conflict,
    TooLarge(SchemaTooLarge),
}

impl From<SchemaTooLarge> for FoldStop {
    fn from(err: SchemaTooLarge) -> Self {
        Self::TooLarge(err)
    }
}

/// A completed fold, or the parent handed back unchanged after a conflict.
type Folded = Result<Map<String, Value>, Map<String, Value>>;

/// Fold cleaned `allOf` branches over `base` in order, charging `budget` for
/// what each branch moves into the parent. On the first disagreement returns
/// the inner `Err` holding `base` exactly as it came in.
fn fold_all_of(
    base: Map<String, Value>,
    branches: Vec<Value>,
    budget: &mut SchemaBudget,
) -> Result<Folded, SchemaTooLarge> {
    let mut fold = AllOfFold::new(base, budget)?;
    for branch in branches {
        let merged = match branch {
            Value::Bool(true) => Ok(()),
            Value::Object(source) => fold.absorb(source),
            _ => Err(FoldStop::Conflict),
        };
        match merged {
            Ok(()) => {}
            Err(FoldStop::Conflict) => return Ok(Err(fold.undo())),
            Err(FoldStop::TooLarge(err)) => return Err(err),
        }
    }
    Ok(Ok(fold.folded))
}

/// An `allOf` merge in progress: the parent's keys with each branch moved in
/// (never cloned). Every mutation of a value the parent already held is an
/// append, and each is recorded, so [`AllOfFold::undo`] can hand the parent
/// back untouched without a copy of it.
struct AllOfFold<'b> {
    folded: Map<String, Value>,
    /// Membership of the `required` entries folded so far, built once.
    required: RequiredSet,
    /// Length of the parent's own `required` array, if it had one.
    base_required_len: Option<usize>,
    /// Keys a branch introduced.
    inserted: Vec<String>,
    /// Property names a branch added to the parent's own `properties`.
    added_properties: Vec<String>,
    budget: &'b mut SchemaBudget,
}

impl<'b> AllOfFold<'b> {
    fn new(base: Map<String, Value>, budget: &'b mut SchemaBudget) -> Result<Self, SchemaTooLarge> {
        let mut required = RequiredSet::default();
        let base_required_len = match base.get("required") {
            Some(Value::Array(entries)) => {
                required.extend(entries, budget)?;
                Some(entries.len())
            }
            _ => None,
        };
        Ok(Self {
            folded: base,
            required,
            base_required_len,
            inserted: Vec::new(),
            added_properties: Vec::new(),
            budget,
        })
    }

    /// Fold one cleaned branch in.
    fn absorb(&mut self, source: Map<String, Value>) -> Result<(), FoldStop> {
        for (key, incoming) in source {
            self.budget.charge_key(&key)?;
            let Some(existing) = self.folded.get_mut(&key) else {
                if key == "required"
                    && let Value::Array(entries) = &incoming
                {
                    self.required.extend(entries, self.budget)?;
                }
                self.inserted.push(key.clone());
                self.folded.insert(key, incoming);
                continue;
            };
            match key.as_str() {
                k if FIRST_WRITER_KEYWORDS.contains(&k) => {}
                "properties" => {
                    union_properties(existing, incoming, &mut self.added_properties, self.budget)?;
                }
                "required" => union_required(existing, incoming, &mut self.required, self.budget)?,
                _ if *existing == incoming => {}
                _ => return Err(FoldStop::Conflict),
            }
        }
        Ok(())
    }

    /// The parent's keys as they were before any branch was absorbed.
    fn undo(self) -> Map<String, Value> {
        let Self {
            mut folded,
            base_required_len,
            inserted,
            added_properties,
            ..
        } = self;
        for key in &inserted {
            folded.remove(key);
        }
        if let Some(Value::Object(properties)) = folded.get_mut("properties") {
            for name in &added_properties {
                properties.remove(name);
            }
        }
        if let (Some(len), Some(Value::Array(entries))) =
            (base_required_len, folded.get_mut("required"))
        {
            entries.truncate(len);
        }
        folded
    }
}

/// Union `incoming` `properties` into `existing` by name, recording and
/// charging each name added. A name present in both must hold equal cleaned
/// schemas; there is no recursive property merge.
fn union_properties(
    existing: &mut Value,
    incoming: Value,
    added: &mut Vec<String>,
    budget: &mut SchemaBudget,
) -> Result<(), FoldStop> {
    match (existing, incoming) {
        (Value::Object(have), Value::Object(add)) => {
            for (name, schema) in add {
                match have.get(&name) {
                    Some(held) if *held != schema => return Err(FoldStop::Conflict),
                    Some(_) => {}
                    None => {
                        budget.charge_key(&name)?;
                        added.push(name.clone());
                        have.insert(name, schema);
                    }
                }
            }
            Ok(())
        }
        (existing, incoming) => equal_or_conflict(existing, &incoming),
    }
}

fn equal_or_conflict(existing: &Value, incoming: &Value) -> Result<(), FoldStop> {
    if existing == incoming {
        Ok(())
    } else {
        Err(FoldStop::Conflict)
    }
}

/// Ordered union of two `required` arrays, deduplicating string names. `seen`
/// holds the membership of `existing` and is updated as entries are appended,
/// so a merge costs the size of `incoming` alone.
fn union_required(
    existing: &mut Value,
    incoming: Value,
    seen: &mut RequiredSet,
    budget: &mut SchemaBudget,
) -> Result<(), FoldStop> {
    match (existing, incoming) {
        (Value::Array(have), Value::Array(add)) => {
            for entry in add {
                if seen.insert(&entry, budget)? {
                    have.push(entry);
                }
            }
            Ok(())
        }
        (existing, incoming) => equal_or_conflict(existing, &incoming),
    }
}

/// Hash-backed membership of the string names in a `required` array. A
/// non-string entry names no property, so it is never a member: it is neither
/// hashed nor serialized, and passes through for [`prune_required`] to drop and
/// report. Every entry visited is charged, so a `required` list carried up
/// through nested `allOf` levels is billed at each level.
#[derive(Default)]
struct RequiredSet {
    names: HashSet<String>,
}

impl RequiredSet {
    fn extend(
        &mut self,
        entries: &[Value],
        budget: &mut SchemaBudget,
    ) -> Result<(), SchemaTooLarge> {
        for entry in entries {
            self.insert(entry, budget)?;
        }
        Ok(())
    }

    /// Record `entry`; `true` when it should be kept in the array (a name not
    /// already a member, or a non-string entry).
    fn insert(&mut self, entry: &Value, budget: &mut SchemaBudget) -> Result<bool, SchemaTooLarge> {
        let Some(name) = entry.as_str() else {
            budget.charge_bytes(VALUE_OVERHEAD)?;
            return Ok(true);
        };
        budget.charge_key(name)?;
        if self.names.contains(name) {
            return Ok(false);
        }
        self.names.insert(name.to_owned());
        #[cfg(test)]
        SET_INSERTIONS.with(|count| count.set(count.get() + 1));
        Ok(true)
    }
}

#[cfg(test)]
thread_local! {
    /// Members added to any [`RequiredSet`] on this thread.
    static SET_INSERTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Times [`prune_required`] built a replacement list on this thread.
    static PRUNE_REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Keep only the `required` entries that name a key of the final `properties`.
/// Gemini rejects a `required` naming a property the schema does not declare;
/// after a merge or an inline the two lists can disagree. An emptied list is
/// omitted. Returns whether any entry was removed. A `required` that is not
/// an array is not this filter's to interpret and is left untouched.
fn prune_required(map: &mut Map<String, Value>) -> bool {
    let Some(Value::Array(names)) = map.get("required") else {
        return false;
    };
    let declared = map.get("properties").and_then(Value::as_object);
    let is_declared = |name: &Value| {
        name.as_str()
            .is_some_and(|n| declared.is_some_and(|props| props.contains_key(n)))
    };
    let kept_count = names.iter().filter(|name| is_declared(name)).count();
    let removed = kept_count != names.len();
    let replacement = if kept_count == 0 {
        None
    } else if removed {
        #[cfg(test)]
        PRUNE_REBUILDS.with(|count| count.set(count.get() + 1));
        Some(
            names
                .iter()
                .filter(|name| is_declared(name))
                .cloned()
                .collect(),
        )
    } else {
        return false;
    };
    match replacement {
        Some(kept) => {
            map.insert("required".to_string(), Value::Array(kept));
        }
        None => {
            map.remove("required");
        }
    }
    // Removing a name means the model is no longer told the field is
    // mandatory.
    // TRANSLATION-DROP: lane=gemini class=schema_keyword_unsupported test=required_naming_a_missing_property_reports_a_drop
    removed
}

/// Recurse into a map whose keys are user-chosen names (property names)
/// rather than JSON-Schema keywords: clean each value as a schema, leave the
/// keys untouched.
fn clean_schema_map(
    value: &Value,
    walk: &mut Walk<'_>,
    depth: usize,
) -> Result<Value, SchemaTooLarge> {
    match value {
        Value::Object(entries) => entries
            .iter()
            .map(|(name, schema)| {
                walk.budget.charge_key(name)?;
                Ok((name.clone(), clean_value(schema, walk, depth)?))
            })
            .collect::<Result<Map<_, _>, SchemaTooLarge>>()
            .map(Value::Object),
        other => clean_value(other, walk, depth),
    }
}

/// Emit Gemini's `type` (uppercased) and lift a `"null"` union member to a
/// `nullable: true` flag. A union with multiple concrete members is lowered
/// to an `anyOf` of single-`type` branches, since Gemini's `type` is scalar;
/// members are deduplicated, and more distinct concrete members than
/// [`GEMINI_TYPE_COUNT`] are refused on the node ceiling, since no legitimate
/// schema names that many types. Each generated branch is charged before it
/// is built. A non-array, non-string `type` passes through.
fn insert_type(
    out: &mut Map<String, Value>,
    value: &Value,
    walk: &mut Walk<'_>,
) -> Result<(), SchemaTooLarge> {
    match value {
        Value::String(t) => {
            out.insert("type".to_string(), Value::String(t.to_uppercase()));
        }
        Value::Array(members) => {
            let mut has_null = false;
            let mut seen: HashSet<String> = HashSet::new();
            let mut concrete: Vec<String> = Vec::new();
            for member in members {
                match member.as_str() {
                    Some("null") => has_null = true,
                    Some(t) => {
                        let upper = t.to_uppercase();
                        if seen.insert(upper.clone()) {
                            if concrete.len() == GEMINI_TYPE_COUNT {
                                return Err(SchemaTooLarge::nodes());
                            }
                            concrete.push(upper);
                        }
                    }
                    // A non-string member of a `type` union is not a JSON
                    // Schema type name at all, so there is nothing to lower
                    // onto Gemini's scalar TYPE enum and nothing that could be
                    // forwarded. Skipping it loses no caller-stated type.
                    // TRANSLATION-DROP: structural -- a non-string `type` union member names no type; the union's real members all translate
                    None => {}
                }
            }
            match concrete.len() {
                // A `type` union with no concrete member (`["null"]` alone, or
                // only non-string members) states no type to emit. `"null"`
                // already lifted to `nullable` below, so omitting `type`
                // reproduces the caller's schema exactly -- Gemini's proto has
                // no NULL member of its TYPE enum to spell it with.
                // TRANSLATION-DROP: structural -- an all-null `type` union is fully expressed by the `nullable` flag set below
                0 => {}
                1 => {
                    out.insert("type".to_string(), Value::String(concrete.remove(0)));
                }
                _ => {
                    walk.budget.charge_bytes(VALUE_OVERHEAD)?;
                    let branches = concrete
                        .into_iter()
                        .map(|t| type_branch(t, walk))
                        .collect::<Result<Vec<_>, _>>()?;
                    out.insert("anyOf".to_string(), Value::Array(branches));
                }
            }
            if has_null {
                out.insert("nullable".to_string(), Value::Bool(true));
            }
        }
        other => {
            out.insert("type".to_string(), other.clone());
        }
    }
    Ok(())
}

/// One `{"type": T}` branch of a lowered `type` union, charged as a node, an
/// object, its single entry and its string before it is built.
fn type_branch(type_name: String, walk: &mut Walk<'_>) -> Result<Value, SchemaTooLarge> {
    walk.budget.charge_node()?;
    walk.budget.charge_bytes(OBJECT_COST)?;
    walk.budget.charge_key("type")?;
    walk.budget
        .charge_bytes(own_cost(&Value::String(String::new())) + type_name.len())?;
    let mut branch = Map::new();
    branch.insert("type".to_string(), Value::String(type_name));
    Ok(Value::Object(branch))
}

/// Gemini's Schema proto accepts `format` only for a small closed set per
/// type (STRING: `enum`, `date-time`; NUMBER: `float`, `double`; INTEGER:
/// `int32`, `int64`). Every other JSON-Schema format (uri/email/uuid/date/
/// password/...) is dropped rather than passed through, which would 400.
fn is_supported_format(f: &str) -> bool {
    matches!(
        f,
        "enum" | "date-time" | "float" | "double" | "int32" | "int64"
    )
}

/// Coerce numeric and boolean enum entries to their string form; Gemini's
/// enum is a repeated string and rejects non-string members.
fn coerce_enum(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| match item {
                    Value::Number(n) => Value::String(n.to_string()),
                    Value::Bool(b) => Value::String(b.to_string()),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_of_is_renamed_to_any_of() {
        // Arrange
        let schema = json!({
            "oneOf": [{"type": "string"}, {"type": "integer"}]
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("oneOf").is_none(), "oneOf must be removed");
        let any_of = cleaned.get("anyOf").expect("anyOf present");
        assert_eq!(any_of[0]["type"], "STRING");
        assert_eq!(any_of[1]["type"], "INTEGER");
    }

    #[test]
    fn schema_and_ref_and_additional_properties_are_stripped() {
        // A `$ref` with no matching `$defs`/`definitions` target is
        // unresolvable, so it degrades to a drop (no inline, no panic).
        // Arrange
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/definitions/Foo",
            "additionalProperties": false,
            "type": "object"
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("$ref").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn nullable_type_array_lifts_to_nullable_flag() {
        // Arrange
        let schema = json!({"type": ["string", "null"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["type"], "STRING");
        assert_eq!(cleaned["nullable"], true);
    }

    #[test]
    fn explicit_nullable_flag_is_preserved() {
        // Arrange
        let schema = json!({"type": "string", "nullable": true});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["type"], "STRING");
        assert_eq!(cleaned["nullable"], true);
    }

    #[test]
    fn numeric_and_boolean_enum_entries_coerced_to_strings() {
        // Arrange
        let schema = json!({"type": "integer", "enum": [1, 2, 3]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["enum"], json!(["1", "2", "3"]));
    }

    #[test]
    fn string_enum_entries_pass_through() {
        // Arrange
        let schema = json!({"type": "string", "enum": ["a", "b"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["enum"], json!(["a", "b"]));
    }

    #[test]
    fn type_is_uppercased() {
        // Arrange
        let schema = json!({"type": "boolean"});

        // Act + Assert
        assert_eq!(clean_schema(&schema)["type"], "BOOLEAN");
    }

    #[test]
    fn nested_object_properties_are_cleaned_recursively() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "inner": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "leaf": {"type": ["number", "null"]}
                    }
                }
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let inner = &cleaned["properties"]["inner"];
        assert!(inner.get("additionalProperties").is_none());
        assert_eq!(inner["type"], "OBJECT");
        let leaf = &inner["properties"]["leaf"];
        assert_eq!(leaf["type"], "NUMBER");
        assert_eq!(leaf["nullable"], true);
    }

    #[test]
    fn array_items_schema_is_cleaned_recursively() {
        // Arrange
        let schema = json!({
            "type": "array",
            "items": {"type": "string", "$schema": "x"}
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["type"], "ARRAY");
        assert_eq!(cleaned["items"]["type"], "STRING");
        assert!(cleaned["items"].get("$schema").is_none());
    }

    #[test]
    fn property_named_like_a_keyword_is_not_treated_as_a_keyword() {
        // A property literally named "type" must recurse as a schema, not
        // be uppercased as a type token.
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "type": {"type": "string"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["type"]["type"], "STRING");
    }

    #[test]
    fn defs_and_definitions_are_stripped() {
        // Arrange: with no `$ref` pointing at them, the `$defs`/`definitions`
        // containers are dead weight and never reach the wire; their child
        // keys are definition names, not keywords.
        let schema = json!({
            "type": "object",
            "$defs": {"Foo": {"type": "string"}},
            "definitions": {"Bar": {"type": "integer"}}
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("$defs").is_none());
        assert!(cleaned.get("definitions").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn ref_to_defs_inlines_cleaned_target() {
        // A property whose schema is `{"$ref": "#/$defs/X"}` emits X's
        // inlined, cleaned object shape -- not an empty `{}`.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "Address": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "city": {"type": "string"},
                        "zip": {"type": ["string", "null"]}
                    }
                }
            },
            "properties": {
                "home": {"$ref": "#/$defs/Address"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let home = &cleaned["properties"]["home"];
        assert!(home.get("$ref").is_none(), "$ref must be resolved away");
        assert_eq!(home["type"], "OBJECT");
        assert!(
            home.get("additionalProperties").is_none(),
            "inlined target must be cleaned too"
        );
        assert_eq!(home["properties"]["city"]["type"], "STRING");
        assert_eq!(home["properties"]["zip"]["type"], "STRING");
        assert_eq!(home["properties"]["zip"]["nullable"], true);
        // The container itself never reaches the wire.
        assert!(cleaned.get("$defs").is_none());
    }

    #[test]
    fn ref_to_definitions_inlines_cleaned_target() {
        // Arrange
        let schema = json!({
            "type": "object",
            "definitions": {
                "Tag": {"type": "string", "enum": [1, 2]}
            },
            "properties": {
                "tag": {"$ref": "#/definitions/Tag"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let tag = &cleaned["properties"]["tag"];
        assert!(tag.get("$ref").is_none());
        assert_eq!(tag["type"], "STRING");
        assert_eq!(tag["enum"], json!(["1", "2"]));
        assert!(cleaned.get("definitions").is_none());
    }

    #[test]
    fn self_referential_ref_terminates_safely() {
        // A def that references itself must not recurse forever; the cyclic
        // ref site degrades to a bounded empty schema.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "value": {"type": "string"},
                        "next": {"$ref": "#/$defs/Node"}
                    }
                }
            },
            "properties": {
                "root": {"$ref": "#/$defs/Node"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let root = &cleaned["properties"]["root"];
        assert_eq!(root["type"], "OBJECT");
        assert_eq!(root["properties"]["value"]["type"], "STRING");
        // The self-reference terminates to a bounded empty schema.
        assert_eq!(root["properties"]["next"], json!({}));
    }

    #[test]
    fn mutually_recursive_refs_terminate_safely() {
        // A -> B -> A cycle must terminate rather than blow the stack.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "A": {
                    "type": "object",
                    "properties": {"b": {"$ref": "#/$defs/B"}}
                },
                "B": {
                    "type": "object",
                    "properties": {"a": {"$ref": "#/$defs/A"}}
                }
            },
            "properties": {
                "start": {"$ref": "#/$defs/A"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let start = &cleaned["properties"]["start"];
        assert_eq!(start["type"], "OBJECT");
        let b = &start["properties"]["b"];
        assert_eq!(b["type"], "OBJECT");
        // The back-edge A (already on the resolution path) terminates empty.
        assert_eq!(b["properties"]["a"], json!({}));
    }

    #[test]
    fn diamond_ref_inlines_at_each_site() {
        // The same def referenced from two independent sites (not a cycle)
        // inlines at both -- path-based cycle detection must not confuse a
        // diamond for a loop.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "Leaf": {"type": "string"}
            },
            "properties": {
                "left": {"$ref": "#/$defs/Leaf"},
                "right": {"$ref": "#/$defs/Leaf"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["left"]["type"], "STRING");
        assert_eq!(cleaned["properties"]["right"]["type"], "STRING");
    }

    #[test]
    fn unresolvable_ref_degrades_to_empty() {
        // A `$ref` with no matching target drops without panicking.
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "missing": {"$ref": "#/$defs/DoesNotExist"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["missing"], json!({}));
    }

    #[test]
    fn ref_with_escaped_pointer_tokens_resolves() {
        // A def name containing `/` (escaped as `~1`) or `~` (escaped as `~0`)
        // must resolve per RFC 6901, not fail lookup and degrade to `{}`.
        // Arrange
        let schema = json!({
            "type": "object",
            "$defs": {
                "A/B": {"type": "string"},
                "C~D": {"type": "integer"}
            },
            "properties": {
                "slash": {"$ref": "#/$defs/A~1B"},
                "tilde": {"$ref": "#/$defs/C~0D"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert: both escaped refs inline their cleaned targets.
        assert_eq!(cleaned["properties"]["slash"]["type"], "STRING");
        assert_eq!(cleaned["properties"]["tilde"]["type"], "INTEGER");
    }

    #[test]
    fn pattern_properties_is_stripped() {
        // Gemini's Schema proto has no `patternProperties`; it is dropped
        // rather than passed through (a 400 otherwise).
        // Arrange
        let schema = json!({
            "type": "object",
            "patternProperties": {
                "^x": {"type": "string"}
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("patternProperties").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn object_valued_default_passes_through_byte_identical() {
        // A `default` VALUE that happens to contain a `type` field must NOT
        // be treated as a nested schema: its `type` stays lowercase.
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {
                "size": {
                    "type": "string",
                    "default": {"type": "small", "count": 3}
                }
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(
            cleaned["properties"]["size"]["default"],
            json!({"type": "small", "count": 3}),
            "a default value must pass through verbatim, uncorrupted"
        );
    }

    #[test]
    fn literal_valued_keywords_pass_through_verbatim() {
        // `example`/`examples`/`title`/`description` carry data, not schemas,
        // so a `type` field inside them is never uppercased.
        // Arrange
        let schema = json!({
            "type": "string",
            "example": {"type": "x"},
            "examples": [{"type": "y"}],
            "title": "Type",
            "description": "the type field"
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned["example"], json!({"type": "x"}));
        assert_eq!(cleaned["examples"], json!([{"type": "y"}]));
        assert_eq!(cleaned["title"], "Type");
        assert_eq!(cleaned["description"], "the type field");
    }

    #[test]
    fn multi_concrete_type_union_lowers_to_any_of() {
        // Arrange
        let schema = json!({"type": ["string", "integer"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(
            cleaned.get("type").is_none(),
            "a multi-type union must not retain a scalar type"
        );
        let branches = cleaned.get("anyOf").expect("anyOf present");
        assert_eq!(branches[0]["type"], "STRING");
        assert_eq!(branches[1]["type"], "INTEGER");
        assert!(cleaned.get("nullable").is_none());
    }

    #[test]
    fn multi_concrete_type_union_with_null_sets_nullable() {
        // Arrange
        let schema = json!({"type": ["string", "integer", "null"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        let branches = cleaned.get("anyOf").expect("anyOf present");
        assert_eq!(branches[0]["type"], "STRING");
        assert_eq!(branches[1]["type"], "INTEGER");
        assert_eq!(cleaned["nullable"], true);
    }

    #[test]
    fn unsupported_keywords_are_stripped() {
        // Arrange
        let schema = json!({
            "type": "object",
            "not": {"type": "integer"},
            "const": {"type": "x"}
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("not").is_none());
        assert!(cleaned.get("const").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
    }

    #[test]
    fn supported_format_survives_and_unsupported_is_dropped() {
        // Arrange + Act + Assert
        let supported = json!({"type": "string", "format": "date-time"});
        assert_eq!(clean_schema(&supported)["format"], "date-time");

        let unsupported = json!({"type": "string", "format": "uri"});
        assert!(clean_schema(&unsupported).get("format").is_none());
    }

    #[test]
    fn combined_schema_applies_every_transform() {
        // Arrange: one schema exercising all constructs at once.
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": {
                    "oneOf": [
                        {"type": "string", "enum": ["ok", "err"]},
                        {"type": ["integer", "null"], "enum": [0, 1]}
                    ]
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        });

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["type"], "OBJECT");

        let status = &cleaned["properties"]["status"];
        assert!(status.get("oneOf").is_none());
        let branches = status.get("anyOf").expect("anyOf present");
        assert_eq!(branches[0]["type"], "STRING");
        assert_eq!(branches[0]["enum"], json!(["ok", "err"]));
        assert_eq!(branches[1]["type"], "INTEGER");
        assert_eq!(branches[1]["nullable"], true);
        assert_eq!(branches[1]["enum"], json!(["0", "1"]));

        let tags = &cleaned["properties"]["tags"];
        assert_eq!(tags["type"], "ARRAY");
        assert_eq!(tags["items"]["type"], "STRING");
    }

    fn clean_reporting(schema: &Value) -> (Value, bool) {
        clean_schema_alone(schema).expect("within the cleaning ceilings")
    }

    /// A root that reaches a leaf through `$ref` hops such that the deepest
    /// schema object sits at exactly `levels` schema levels.
    fn ref_chain(levels: usize) -> Value {
        assert!(levels >= 2, "a chain needs the root and one target");
        let last = levels - 2;
        let mut defs = Map::new();
        for i in 0..last {
            defs.insert(
                format!("D{i}"),
                json!({"$ref": format!("#/$defs/D{}", i + 1)}),
            );
        }
        defs.insert(format!("D{last}"), json!({"type": "string"}));
        json!({"$ref": "#/$defs/D0", "$defs": Value::Object(defs)})
    }

    #[test]
    fn all_of_single_ref_wrapper_merges_and_parent_annotation_wins() {
        // Arrange
        let schema = json!({
            "description": "from the parent",
            "allOf": [{"$ref": "#/$defs/Base"}],
            "$defs": {
                "Base": {
                    "title": "Base",
                    "description": "from the base",
                    "type": "object",
                    "properties": {"a": {"type": "string"}},
                    "required": ["a"]
                }
            }
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert!(cleaned.get("allOf").is_none());
        assert_eq!(cleaned["type"], "OBJECT");
        assert_eq!(cleaned["properties"]["a"]["type"], "STRING");
        assert_eq!(cleaned["required"], json!(["a"]));
        assert_eq!(cleaned["description"], "from the parent");
        assert_eq!(cleaned["title"], "Base");
        assert!(!dropped, "a clean merge loses nothing: {cleaned}");
    }

    #[test]
    fn all_of_disjoint_branches_union_properties_and_required() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {"own": {"type": "boolean"}},
            "required": ["own"],
            "allOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {
                    "type": "object",
                    "properties": {"b": {"type": "integer"}},
                    "required": ["b", "a"]
                }
            ]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["type"], "OBJECT");
        let props = cleaned["properties"].as_object().expect("properties");
        assert_eq!(props.len(), 3);
        assert_eq!(props["own"]["type"], "BOOLEAN");
        assert_eq!(props["a"]["type"], "STRING");
        assert_eq!(props["b"]["type"], "INTEGER");
        assert_eq!(cleaned["required"], json!(["own", "a", "b"]));
        assert!(!dropped);
    }

    #[test]
    fn all_of_branch_required_is_judged_against_the_merged_properties() {
        // Arrange -- the branch requires a property only its sibling declares.
        let schema = json!({
            "type": "object",
            "allOf": [
                {"properties": {"a": {"type": "string"}}},
                {"required": ["a"]}
            ]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["required"], json!(["a"]));
        assert!(!dropped);
    }

    #[test]
    fn all_of_conflicting_type_falls_back_to_the_parent_and_reports() {
        // Arrange
        let schema = json!({
            "type": "object",
            "description": "kept",
            "allOf": [{"type": "string"}]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned, json!({"type": "OBJECT", "description": "kept"}));
        assert!(dropped, "the abandoned allOf is a reported loss");
    }

    #[test]
    fn all_of_same_property_name_with_different_schemas_conflicts() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "allOf": [{"properties": {"a": {"type": "integer"}}}]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["a"]["type"], "STRING");
        assert!(dropped);
    }

    #[test]
    fn all_of_true_branch_is_identity_and_false_or_non_object_conflicts() {
        // Arrange + Act
        let (with_true, true_dropped) =
            clean_reporting(&json!({"type": "string", "allOf": [true]}));
        let (with_false, false_dropped) =
            clean_reporting(&json!({"type": "string", "allOf": [false]}));
        let (not_array, not_array_dropped) =
            clean_reporting(&json!({"type": "string", "allOf": {"type": "string"}}));

        // Assert
        assert_eq!(with_true, json!({"type": "STRING"}));
        assert!(!true_dropped);
        assert_eq!(with_false, json!({"type": "STRING"}));
        assert!(false_dropped);
        assert_eq!(not_array, json!({"type": "STRING"}));
        assert!(not_array_dropped);
    }

    #[test]
    fn nested_all_of_inside_a_branch_merges() {
        // Arrange
        let schema = json!({
            "type": "object",
            "allOf": [{
                "properties": {"b": {"type": "integer"}},
                "allOf": [{"properties": {"a": {"type": "string"}}, "required": ["a"]}]
            }]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["properties"]["a"]["type"], "STRING");
        assert_eq!(cleaned["properties"]["b"]["type"], "INTEGER");
        assert_eq!(cleaned["required"], json!(["a"]));
        assert!(!dropped);
    }

    #[test]
    fn required_naming_a_missing_property_is_filtered_and_reports() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a", "ghost"]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["required"], json!(["a"]));
        assert!(dropped, "the model is no longer told ghost is mandatory");
    }

    #[test]
    fn required_with_only_missing_names_is_omitted_and_non_string_entries_go() {
        // Arrange
        let no_properties = json!({"type": "object", "required": ["ghost"]});
        let non_string = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": [7, "a"]
        });

        // Act
        let (omitted, omitted_dropped) = clean_reporting(&no_properties);
        let (filtered, filtered_dropped) = clean_reporting(&non_string);

        // Assert
        assert!(omitted.get("required").is_none());
        assert!(omitted_dropped);
        assert_eq!(filtered["required"], json!(["a"]));
        assert!(filtered_dropped);
    }

    #[test]
    fn required_naming_declared_properties_is_untouched_and_reports_nothing() {
        // Arrange
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "integer"}},
            "required": ["b", "a"]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["required"], json!(["b", "a"]));
        assert!(!dropped);
    }

    #[test]
    fn ref_chain_at_the_depth_ceiling_cleans_and_one_level_deeper_is_refused() {
        // Arrange + Act
        let at_limit = clean_schema_alone(&ref_chain(MAX_SCHEMA_DEPTH));
        let over = clean_schema_alone(&ref_chain(MAX_SCHEMA_DEPTH + 1));

        // Assert
        let (cleaned, dropped) = at_limit.expect("64 levels is within the ceiling");
        assert_eq!(cleaned, json!({"type": "STRING"}));
        assert!(!dropped);
        assert_eq!(
            over,
            Err(SchemaTooLarge {
                limit: "depth",
                max: MAX_SCHEMA_DEPTH
            })
        );
    }

    #[test]
    fn nested_arrays_past_the_depth_ceiling_are_refused_not_recursed() {
        // Arrange -- built by hand: a parsed document stops at 128 levels.
        let mut value = json!("leaf");
        for _ in 0..(MAX_SCHEMA_DEPTH * 4) {
            value = Value::Array(vec![value]);
        }
        let schema = json!({"type": "array", "prefixItems": value});

        // Act
        let result = clean_schema_alone(&schema);

        // Assert
        assert_eq!(result.map(|_| ()), Err(SchemaTooLarge::depth()));
    }

    /// `levels` binary fan-out layers, every ref site of layer `i` pointing at
    /// layer `i - 1`: a small document that cleans to ~2^(levels + 2) nodes
    /// while staying far below the depth ceiling.
    fn fan_out_dag(levels: usize) -> Value {
        let mut defs = Map::new();
        defs.insert("L0".to_string(), json!({"type": "string"}));
        for i in 1..=levels {
            let below = format!("#/$defs/L{}", i - 1);
            defs.insert(
                format!("L{i}"),
                json!({"properties": {"l": {"$ref": below}, "r": {"$ref": below}}}),
            );
        }
        json!({"$ref": format!("#/$defs/L{levels}"), "$defs": Value::Object(defs)})
    }

    fn flat_schema(nodes: usize) -> Value {
        let properties: Map<String, Value> = (0..nodes - 1)
            .map(|i| (format!("p{i}"), json!({"type": "string"})))
            .collect();
        json!({"type": "object", "properties": Value::Object(properties)})
    }

    #[test]
    fn ref_fan_out_under_the_depth_ceiling_is_refused_on_the_node_ceiling() {
        // Arrange + Act
        let within = clean_schema_alone(&fan_out_dag(11));
        let over = clean_schema_alone(&fan_out_dag(12));

        // Assert
        assert!(within.is_ok(), "8190 nodes is within the ceiling");
        assert_eq!(
            over.map(|_| ()),
            Err(SchemaTooLarge {
                limit: "nodes",
                max: MAX_SCHEMA_NODES
            })
        );
    }

    #[test]
    fn node_ceiling_admits_exactly_the_maximum_and_refuses_one_more() {
        // Arrange + Act
        let at_limit = clean_schema_alone(&flat_schema(MAX_SCHEMA_NODES));
        let over = clean_schema_alone(&flat_schema(MAX_SCHEMA_NODES + 1));

        // Assert
        assert!(at_limit.is_ok());
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::nodes()));
    }

    #[test]
    fn boolean_schema_branches_are_charged_to_the_node_ceiling() {
        // Arrange -- the root plus `MAX_SCHEMA_NODES - 1` boolean branches is
        // exactly the ceiling; one more branch is over it.
        let branches = |n: usize| json!({"anyOf": vec![json!(true); n]});

        // Act
        let at_limit = clean_schema_alone(&branches(MAX_SCHEMA_NODES - 1));
        let over = clean_schema_alone(&branches(MAX_SCHEMA_NODES));

        // Assert
        assert!(at_limit.is_ok(), "the ceiling itself is admitted");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::nodes()));
    }

    #[test]
    fn boolean_property_and_items_schemas_are_charged_to_the_node_ceiling() {
        // Arrange
        let properties: Map<String, Value> = (0..MAX_SCHEMA_NODES)
            .map(|i| (format!("p{i}"), json!(true)))
            .collect();
        let wide_properties = json!({"properties": Value::Object(properties)});
        let wide_prefix_items = json!({"prefixItems": vec![json!(false); MAX_SCHEMA_NODES]});

        // Act + Assert
        for schema in [wide_properties, wide_prefix_items] {
            assert_eq!(
                clean_schema_alone(&schema).map(|_| ()),
                Err(SchemaTooLarge::nodes())
            );
        }
    }

    /// A root whose `sites` properties each `$ref` one def carrying a
    /// `literal_len`-byte description.
    fn shared_literal_schema(literal_len: usize, sites: usize) -> Value {
        let properties: Map<String, Value> = (0..sites)
            .map(|i| (format!("p{i}"), json!({"$ref": "#/$defs/Big"})))
            .collect();
        json!({
            "type": "object",
            "properties": Value::Object(properties),
            "$defs": {"Big": {"description": "x".repeat(literal_len)}}
        })
    }

    #[test]
    fn a_literal_inlined_at_many_ref_sites_is_refused_on_the_byte_ceiling() {
        // Arrange -- 2 MiB per site: 3 sites fit under 8 MiB, 5 do not.
        let literal = 2 * 1024 * 1024;

        // Act
        let under = clean_schema_alone(&shared_literal_schema(literal, 3));
        let over = clean_schema_alone(&shared_literal_schema(literal, 5));

        // Assert
        assert!(under.is_ok(), "3 x 2 MiB is under the byte ceiling");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn byte_ceiling_admits_exactly_the_maximum_and_refuses_one_more() {
        // Arrange -- the root object emits one key and one string; together
        // with the object itself they are charged exactly
        // `OBJECT_COST + VALUE_OVERHEAD + key.len() + VALUE_OVERHEAD + value.len()`.
        let key = "description";
        let fixed = OBJECT_COST + 2 * VALUE_OVERHEAD + key.len();
        let schema = |value_len: usize| json!({ key: "x".repeat(value_len) });

        // Act
        let at_limit = clean_schema_alone(&schema(MAX_SCHEMA_BYTES - fixed));
        let over = clean_schema_alone(&schema(MAX_SCHEMA_BYTES - fixed + 1));

        // Assert
        assert!(at_limit.is_ok(), "the ceiling itself is admitted");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn one_literal_larger_than_the_byte_ceiling_is_refused() {
        // Arrange
        let schema = json!({"type": "string", "description": "x".repeat(MAX_SCHEMA_BYTES + 1)});

        // Act
        let result = clean_schema_alone(&schema);

        // Assert
        assert_eq!(result.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn nested_literals_and_keys_are_charged_to_the_byte_ceiling() {
        // Arrange
        let nested_default = json!({
            "type": "string",
            "default": {"a": ["x".repeat(MAX_SCHEMA_BYTES / 2), "x".repeat(MAX_SCHEMA_BYTES / 2)]}
        });
        let long_name = json!({"properties": {"n".repeat(MAX_SCHEMA_BYTES): {"type": "string"}}});
        let long_enum = json!({"enum": ["x".repeat(MAX_SCHEMA_BYTES + 1)]});

        // Act + Assert
        for schema in [nested_default, long_name, long_enum] {
            assert_eq!(
                clean_schema_alone(&schema).map(|_| ()),
                Err(SchemaTooLarge::bytes())
            );
        }
    }

    /// A root whose `sites` properties each `$ref` one def whose `default` is
    /// the given literal.
    fn shared_default_schema(default: &Value, sites: usize) -> Value {
        let properties: Map<String, Value> = (0..sites)
            .map(|i| (format!("p{i}"), json!({"$ref": "#/$defs/Big"})))
            .collect();
        json!({
            "type": "object",
            "properties": Value::Object(properties),
            "$defs": {"Big": {"default": default}}
        })
    }

    #[test]
    fn empty_string_array_fanned_out_through_refs_is_refused_on_the_byte_ceiling() {
        // Arrange -- ~33 KB of accounted cost per site: 100 sites fit under
        // 8 MiB, 400 do not, although every string is empty.
        let default = json!(vec![""; 1000]);

        // Act
        let under = clean_schema_alone(&shared_default_schema(&default, 100));
        let over = clean_schema_alone(&shared_default_schema(&default, 400));

        // Assert
        assert!(under.is_ok(), "100 sites are under the byte ceiling");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn dense_one_key_objects_fanned_out_through_refs_are_refused_on_the_byte_ceiling() {
        // Arrange -- a thousand `{"k": null}` objects per site.
        let default = json!(vec![json!({"k": null}); 1000]);

        // Act
        let under = clean_schema_alone(&shared_default_schema(&default, 20));
        let over = clean_schema_alone(&shared_default_schema(&default, 100));

        // Assert
        assert!(under.is_ok(), "20 sites are under the byte ceiling");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn huge_empty_string_enum_is_refused_on_the_byte_ceiling() {
        // Arrange
        let schema = json!({"enum": vec![""; 1_000_000]});

        // Act
        let result = clean_schema_alone(&schema);

        // Assert
        assert_eq!(result.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn type_union_of_a_million_empty_strings_is_refused() {
        // Arrange
        let schema = json!({"type": vec![""; 1_000_000]});

        // Act
        let result = clean_schema_alone(&schema);

        // Assert
        assert_eq!(result.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn type_union_of_many_distinct_names_is_refused_on_the_node_ceiling() {
        // Arrange -- under the byte ceiling, so only the member count can
        // refuse it.
        let members: Vec<String> = (0..100_000).map(|i| format!("t{i}")).collect();
        let schema = json!({"type": members});

        // Act
        let result = clean_schema_alone(&schema);

        // Assert
        assert_eq!(result.map(|_| ()), Err(SchemaTooLarge::nodes()));
    }

    #[test]
    fn type_union_admits_as_many_distinct_members_as_gemini_has_types() {
        // Arrange
        let at_limit = json!({"type": ["a", "b", "c", "d", "e", "f", "g"]});
        let over = json!({"type": ["a", "b", "c", "d", "e", "f", "g", "h"]});
        let repeated = json!({"type": ["a", "b", "c", "d", "e", "f", "g", "G", "a"]});

        // Act
        let admitted = clean_schema_alone(&at_limit);
        let refused = clean_schema_alone(&over);
        let deduplicated = clean_schema_alone(&repeated);

        // Assert
        let (cleaned, _) = admitted.expect("seven distinct members are admitted");
        assert_eq!(cleaned["anyOf"].as_array().map(Vec::len), Some(7));
        assert_eq!(refused.map(|_| ()), Err(SchemaTooLarge::nodes()));
        let (cleaned, _) = deduplicated.expect("repeats do not count");
        assert_eq!(cleaned["anyOf"].as_array().map(Vec::len), Some(7));
    }

    #[test]
    fn type_union_members_that_repeat_collapse_to_one_type() {
        // Arrange
        let schema = json!({"type": ["string", "STRING", "null", "string"]});

        // Act
        let cleaned = clean_schema(&schema);

        // Assert
        assert_eq!(cleaned, json!({"type": "STRING", "nullable": true}));
    }

    #[test]
    fn legitimate_type_unions_lower_exactly() {
        // Arrange
        let nullable = json!({"type": ["string", "null"]});
        let union = json!({"type": ["string", "integer"]});

        // Act
        let nullable = clean_schema(&nullable);
        let union = clean_schema(&union);

        // Assert
        assert_eq!(nullable, json!({"type": "STRING", "nullable": true}));
        assert_eq!(
            union,
            json!({"anyOf": [{"type": "STRING"}, {"type": "INTEGER"}]})
        );
    }

    /// A root whose `sites` properties each `$ref` a def holding a seven-member
    /// `type` union.
    fn shared_type_union_schema(sites: usize) -> Value {
        let properties: Map<String, Value> = (0..sites)
            .map(|i| (format!("p{i}"), json!({"$ref": "#/$defs/U"})))
            .collect();
        json!({
            "properties": Value::Object(properties),
            "$defs": {"U": {"type": ["a", "b", "c", "d", "e", "f", "g"]}}
        })
    }

    #[test]
    fn type_union_branches_are_charged_to_the_node_ceiling() {
        // Arrange -- each site costs 2 schema objects plus 7 generated
        // branches: 1000 sites (9,001 nodes with the root) fit, 2000 do not.
        // Without charging the branches, 2000 sites would cost only 4,001.

        // Act
        let under = clean_schema_alone(&shared_type_union_schema(1000));
        let over = clean_schema_alone(&shared_type_union_schema(2000));

        // Assert
        assert!(under.is_ok(), "1000 sites are within the node ceiling");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::nodes()));
    }

    fn names(prefix: &str, count: usize) -> Vec<Value> {
        (0..count).map(|i| json!(format!("{prefix}{i}"))).collect()
    }

    #[test]
    fn required_union_of_large_disjoint_arrays_preserves_order() {
        // Arrange
        let mut have = Value::Array(names("a", 50_000));
        let mut add_names = names("b", 50_000);
        add_names.insert(1, json!("a7"));
        add_names.push(json!(3));
        add_names.push(json!(3));
        let add = Value::Array(add_names);

        // Act
        let mut budget = SchemaBudget::default();
        let mut seen = RequiredSet::default();
        seen.extend(
            match &have {
                Value::Array(entries) => entries,
                _ => unreachable!(),
            },
            &mut budget,
        )
        .expect("within budget");
        let merged = union_required(&mut have, add, &mut seen, &mut budget);

        // Assert -- names dedupe; non-string entries pass through unhashed for
        // the prune step to drop.
        assert!(merged.is_ok());
        let mut expected = names("a", 50_000);
        expected.extend(names("b", 50_000));
        expected.push(json!(3));
        expected.push(json!(3));
        assert_eq!(have, Value::Array(expected));
    }

    #[test]
    fn all_of_required_merge_of_large_arrays_completes_within_the_byte_ceiling() {
        // Arrange
        let schema = json!({
            "allOf": [
                {"required": names("a", 50_000)},
                {"required": names("b", 50_000)}
            ]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert -- no property declares them, so the pruned `required` is
        // omitted and the loss is reported.
        assert!(cleaned.get("required").is_none());
        assert!(dropped);
    }

    fn insertions_since_reset() -> usize {
        SET_INSERTIONS.with(std::cell::Cell::get)
    }

    fn reset_insertions() {
        SET_INSERTIONS.with(|count| count.set(0));
    }

    #[test]
    fn all_of_with_thousands_of_branches_builds_the_required_set_once() {
        // Arrange -- 500 declared properties, all required by the parent, and
        // 9,000 single-name branches restating them. A set rebuilt per branch
        // would insert 500 members 9,000 times.
        let properties: Map<String, Value> = (0..500)
            .map(|i| (format!("p{i}"), json!({"type": "string"})))
            .collect();
        let branches: Vec<Value> = (0..9_000)
            .map(|i| json!({"required": [format!("p{}", i % 500)]}))
            .collect();
        let schema = json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": names("p", 500),
            "allOf": branches
        });
        reset_insertions();

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(cleaned["required"], Value::Array(names("p", 500)));
        assert!(!dropped);
        assert_eq!(
            insertions_since_reset(),
            500,
            "membership is built once from the parent and only grows by new names"
        );
    }

    #[test]
    fn all_of_fold_over_a_large_required_adds_only_new_names_in_order() {
        // Arrange
        let base = json!({"required": names("a", 20_000)});
        let Value::Object(base) = base else {
            unreachable!()
        };
        let mut branches: Vec<Value> = (0..9_000)
            .map(|i| json!({"required": [format!("b{i}")]}))
            .collect();
        branches.insert(3, json!({"required": ["a5", "b1", "b1"]}));
        reset_insertions();

        // Act
        let folded = fold_all_of(base, branches, &mut SchemaBudget::default())
            .expect("within budget")
            .expect("branches agree");

        // Assert
        let mut expected = names("a", 20_000);
        expected.extend(names("b", 9_000));
        assert_eq!(folded["required"], Value::Array(expected));
        assert_eq!(insertions_since_reset(), 29_000);
    }

    #[test]
    fn a_failed_all_of_fold_hands_the_parent_back_unchanged() {
        // Arrange
        let base = json!({
            "type": "OBJECT",
            "properties": {"a": {"type": "STRING"}},
            "required": ["a"]
        });
        let Value::Object(base) = base else {
            unreachable!()
        };
        let branches = vec![
            json!({"properties": {"b": {"type": "INTEGER"}}, "required": ["b"], "format": "int32"}),
            json!({"type": "STRING"}),
        ];

        // Act
        let unmerged = fold_all_of(base.clone(), branches, &mut SchemaBudget::default())
            .expect("within budget")
            .expect_err("the second branch conflicts");

        // Assert
        assert_eq!(unmerged, base);
    }

    #[test]
    fn a_ref_pointer_over_the_length_cap_degrades_to_a_drop_without_resolving() {
        // Arrange -- the def exists, so only the cap can make the ref fail.
        let pointer_for = |name_len: usize| {
            let name = "n".repeat(name_len);
            let pointer = format!("#/$defs/{name}");
            let schema = json!({
                "properties": {"p": {"$ref": pointer}},
                "$defs": {name: {"type": "string"}}
            });
            (pointer.len(), schema)
        };
        let (at_cap_len, at_cap) = pointer_for(MAX_REF_POINTER_BYTES - "#/$defs/".len());
        let (over_len, over) = pointer_for(MAX_REF_POINTER_BYTES - "#/$defs/".len() + 1);
        assert_eq!(at_cap_len, MAX_REF_POINTER_BYTES);
        assert_eq!(over_len, MAX_REF_POINTER_BYTES + 1);

        // Act
        let (resolved, resolved_dropped) = clean_reporting(&at_cap);
        let (degraded, degraded_dropped) = clean_reporting(&over);

        // Assert
        assert_eq!(resolved["properties"]["p"], json!({"type": "STRING"}));
        assert!(!resolved_dropped);
        assert_eq!(degraded["properties"]["p"], json!({}));
        assert!(degraded_dropped, "an unresolved ref is a tallied drop");
    }

    #[test]
    fn a_ref_pointer_is_charged_at_every_visit() {
        // Arrange -- unresolvable ~1 KB pointers: one node per site, so only
        // the pointer text can reach the byte ceiling before the node ceiling.
        let name = "n".repeat(MAX_REF_POINTER_BYTES - "#/$defs/".len());
        let pointer = format!("#/$defs/{name}");
        let schema = |sites: usize| {
            let properties: Map<String, Value> = (0..sites)
                .map(|i| (format!("p{i}"), json!({"$ref": pointer})))
                .collect();
            json!({"properties": Value::Object(properties)})
        };

        // Act
        let under = clean_schema_alone(&schema(3_000));
        let over = clean_schema_alone(&schema(9_000));

        // Assert
        assert!(under.is_ok(), "3,000 sites are under the byte ceiling");
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn ref_pointer_escapes_decode_in_a_single_left_to_right_pass() {
        // Arrange + Act + Assert
        assert_eq!(decode_ref_pointer("#/$defs/plain"), "#/$defs/plain");
        assert_eq!(decode_ref_pointer("#/$defs/A~1B"), "#/$defs/A/B");
        assert_eq!(decode_ref_pointer("#/$defs/C~0D"), "#/$defs/C~D");
        assert_eq!(decode_ref_pointer("a~01b"), "a~1b");
        assert_eq!(decode_ref_pointer("~0~1~"), "~/~");
        assert_eq!(decode_ref_pointer("~2~"), "~2~");
    }

    #[test]
    fn enum_of_max_integers_is_refused_on_its_coerced_size() {
        // Arrange -- 200,000 x i64::MAX is 6.4 MB as numbers (32 bytes each)
        // but 10.2 MB once each is the 19-character string it becomes.
        let members = |count: usize| json!({"enum": vec![json!(i64::MAX); count]});

        // Act
        let under = clean_schema_alone(&members(100_000));
        let over = clean_schema_alone(&members(200_000));

        // Assert
        let (cleaned, _) = under.expect("100,000 coerced entries fit");
        assert_eq!(cleaned["enum"][99_999], i64::MAX.to_string());
        assert_eq!(over.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn enum_charge_equals_the_size_of_the_coerced_value() {
        // Arrange
        let value =
            json!([1, -2.5, true, false, "text", null, {"k": [1, "x"]}, i64::MIN, u64::MAX]);
        let mut budget = SchemaBudget::default();

        // Act
        budget.charge_enum(&value).expect("small");

        // Assert
        let coerced = coerce_enum(&value);
        assert_eq!(
            budget.bytes,
            literal_size(&coerced, usize::MAX).expect("unbounded")
        );
    }

    #[test]
    fn one_budget_spans_every_schema_of_a_request() {
        // Arrange -- three schemas of 4,000 nodes each, cleaned into one budget.
        let mut budget = SchemaBudget::default();
        let schema = flat_schema(4_000);

        // Act
        let first = clean_schema_reporting(&schema, &mut budget);
        let second = clean_schema_reporting(&schema, &mut budget);
        let third = clean_schema_reporting(&schema, &mut budget);

        // Assert
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(third.map(|_| ()), Err(SchemaTooLarge::nodes()));
    }

    #[test]
    fn schema_already_in_gemini_form_round_trips_byte_identical() {
        // Arrange
        let schema = json!({
            "type": "OBJECT",
            "title": "Search",
            "description": "Find things",
            "properties": {
                "query": {"type": "STRING", "description": "terms"},
                "limit": {"type": "INTEGER", "format": "int32", "nullable": true},
                "when": {"type": "STRING", "format": "date-time"},
                "kind": {"type": "STRING", "enum": ["a", "b"]},
                "tags": {"type": "ARRAY", "items": {"type": "STRING"}},
                "filter": {
                    "anyOf": [
                        {"type": "STRING"},
                        {"type": "OBJECT", "properties": {"k": {"type": "STRING"}}}
                    ]
                },
                "note": {"type": "STRING", "default": "x", "example": {"type": "y"}}
            },
            "required": ["query", "kind"]
        });

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert
        assert_eq!(
            serde_json::to_string(&cleaned).expect("serialize cleaned"),
            serde_json::to_string(&schema).expect("serialize input")
        );
        assert!(!dropped);
    }

    #[test]
    fn the_eight_keywords_that_never_reach_the_wire_stay_stripped() {
        // Arrange
        let keywords = [
            "$schema",
            "$defs",
            "definitions",
            "additionalProperties",
            "allOf",
            "not",
            "const",
            "patternProperties",
        ];

        // Act + Assert
        for keyword in keywords {
            let (cleaned, _) = clean_reporting(&json!({"type": "object", keyword: false}));
            assert!(cleaned.get(keyword).is_none(), "{keyword} must not survive");
        }
    }

    /// `depth` nested `allOf` wrappers around `inner`: the innermost schema
    /// sits at the depth ceiling.
    fn nest_all_of(inner: Value, wrappers: usize) -> Value {
        (0..wrappers).fold(inner, |acc, _| json!({"allOf": [acc]}))
    }

    #[test]
    fn nested_all_of_carrying_a_large_required_is_charged_at_every_level() {
        // Arrange -- 5,000 names cost about 200 KB to emit once; carried up
        // through 63 folds they cost about 12 MB, past the 8 MiB ceiling.
        let carrying = |count: usize| nest_all_of(json!({"required": names("name", count)}), 63);

        // Act
        let small = clean_schema_alone(&carrying(50));
        let large = clean_schema_alone(&carrying(5_000));

        // Assert
        assert!(small.is_ok(), "the same nesting with a short list is fine");
        assert_eq!(large.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn nested_all_of_carrying_many_keywords_is_charged_at_every_level() {
        // Arrange -- 5,000 unrecognized keywords cost about 350 KB to emit
        // once; moved up through 63 folds they cost about 12 MB.
        let keywords: Map<String, Value> = (0..5_000)
            .map(|i| (format!("x-key{i}"), json!(i)))
            .collect();
        let carrying = |wrappers: usize| nest_all_of(Value::Object(keywords.clone()), wrappers);

        // Act
        let shallow = clean_schema_alone(&carrying(2));
        let deep = clean_schema_alone(&carrying(63));

        // Assert
        assert!(shallow.is_ok(), "two levels stay far below the ceiling");
        assert_eq!(deep.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn nested_all_of_carrying_many_properties_is_charged_at_every_level() {
        // Arrange -- every level declares one property of its own, so each fold
        // unions the carried properties into a map that already exists.
        let properties: Map<String, Value> = (0..5_000)
            .map(|i| (format!("prop{i}"), json!({"type": "string"})))
            .collect();
        let carrying = |wrappers: usize| {
            (0..wrappers).fold(json!({"properties": Value::Object(properties.clone())}), |acc, i| {
                json!({"properties": {format!("own{i}"): {"type": "string"}}, "allOf": [acc]})
            })
        };

        // Act
        let shallow = clean_schema_alone(&carrying(2));
        let deep = clean_schema_alone(&carrying(62));

        // Assert
        assert!(shallow.is_ok(), "two levels stay far below the ceiling");
        assert_eq!(deep.map(|_| ()), Err(SchemaTooLarge::bytes()));
    }

    #[test]
    fn non_string_required_entries_are_never_hashed_or_serialized() {
        // Arrange
        let bulky = |i: usize| json!({"nested": [i, {"deep": [i]}]});
        let first: Vec<Value> = std::iter::once(json!("a"))
            .chain((0..500).map(bulky))
            .collect();
        let second: Vec<Value> = std::iter::once(json!("b"))
            .chain((0..500).map(bulky))
            .collect();
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
            "allOf": [{"required": first}, {"required": second}]
        });
        reset_insertions();

        // Act
        let (cleaned, dropped) = clean_reporting(&schema);

        // Assert -- only the two names entered the set; the non-string entries
        // were dropped by the prune step and reported.
        assert_eq!(insertions_since_reset(), 2);
        assert_eq!(cleaned["required"], json!(["a", "b"]));
        assert!(dropped);
    }

    #[test]
    fn prune_required_leaves_the_list_alone_when_nothing_is_removed() {
        // Arrange
        let Value::Object(mut map) = json!({
            "properties": {"a": {}, "b": {}},
            "required": ["a", "b"]
        }) else {
            unreachable!()
        };
        let before = map.clone();
        PRUNE_REBUILDS.with(|count| count.set(0));

        // Act
        let removed = prune_required(&mut map);

        // Assert
        assert!(!removed);
        assert_eq!(map, before);
        assert_eq!(PRUNE_REBUILDS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn prune_required_builds_a_new_list_only_when_it_removes_a_name() {
        // Arrange
        let Value::Object(mut map) = json!({
            "properties": {"a": {}},
            "required": ["a", "gone", 7]
        }) else {
            unreachable!()
        };
        PRUNE_REBUILDS.with(|count| count.set(0));

        // Act
        let removed = prune_required(&mut map);

        // Assert
        assert!(removed);
        assert_eq!(map["required"], json!(["a"]));
        assert_eq!(PRUNE_REBUILDS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn uppercased_length_matches_the_string_it_predicts() {
        // Arrange -- U+0390 grows from 2 bytes to 6, U+0149 from 2 to 3, and
        // the sharp s and the ligature change length in characters.
        let samples = [
            "string",
            "",
            "\u{390}",
            "\u{149}",
            "stra\u{df}e",
            "\u{fb03}x",
            "\u{1f600}",
        ];

        // Act + Assert
        for sample in samples {
            assert_eq!(
                uppercased_len(sample),
                sample.to_uppercase().len(),
                "{sample:?}"
            );
        }
    }

    #[test]
    fn a_type_name_is_charged_at_its_uppercased_length() {
        // Arrange -- 2M characters of U+0390 are 4 MB as written and 12 MB uppercased.
        let name = "\u{390}".repeat(2_000_000);
        let small = "\u{390}".repeat(10);
        let mut budget = SchemaBudget::default();

        // Act
        let refused = clean_schema_alone(&json!({"type": name}));
        budget.charge_type(&json!(small)).expect("small");

        // Assert
        assert_eq!(refused.map(|_| ()), Err(SchemaTooLarge::bytes()));
        assert_eq!(budget.bytes, VALUE_OVERHEAD + small.to_uppercase().len());
    }

    #[test]
    fn type_union_members_are_charged_at_their_uppercased_length() {
        // Arrange
        let member = "\u{390}".repeat(10);
        let mut budget = SchemaBudget::default();

        // Act
        budget.charge_type(&json!([member, "null"])).expect("small");

        // Assert
        assert_eq!(
            budget.bytes,
            VALUE_OVERHEAD
                + (VALUE_OVERHEAD + member.to_uppercase().len())
                + (VALUE_OVERHEAD + "NULL".len())
        );
    }
}
