//! The managed-key predicate every extras merge consults.
//!
//! An egress assembles its upstream body from the canonical request, then
//! layers client `provider_extras` or operator extras over it. A key the
//! predicate names is one routectl writes itself, so the merge drops the
//! extras value rather than let it replace the assembled one.
//!
//! Every dialect composes the shared canonical list
//! (`routectl_core::is_canonical_request_key`) with its own wire-layer keys.
//!
//! Keys match case-sensitively: the Anthropic and OpenAI upstreams key their
//! request fields by exact name. Gemini's ProtoJSON parser also accepts proto
//! field names (`generation_config` for `generationConfig`); only the
//! lowerCamelCase spelling is named here.
//!
//! Which client extras reach the merge at all is decided first, by the
//! request's ingress dialect (`client_extras_for`): a swept key is spelled
//! in its ingress's vocabulary, so it forwards only to the egress that speaks
//! that vocabulary.

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
use std::borrow::Cow;
use std::collections::BTreeSet;

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
use serde_json::Map;
use serde_json::Value;

#[cfg(feature = "anthropic-api")]
use crate::anthropic_api::PARALLEL_TOOL_CALLS_KEY;
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
use routectl_core::is_canonical_request_key;
#[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
use routectl_core::wire_check::WireTap;
use routectl_core::wire_check::{EditKind, EditSite, EgressKind, forwards_unknowns};
use routectl_core::{
    ChatRequest, ContentPart, Message, MessageContent, RequestProvenance, SystemContent, ToolDef,
};

/// The extras merge whose managed set a key is checked against.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// The Anthropic Messages body, built by the anthropic-api egress and
    /// reused by Bedrock Invoke for client `provider_extras`.
    #[cfg(feature = "anthropic-api")]
    AnthropicApi,
    /// Bedrock Invoke operator `additional_model_request_fields`: the
    /// Anthropic Messages set plus the Invoke seam's own body field.
    #[cfg(feature = "bedrock")]
    BedrockInvokeOperator,
    /// The Bedrock Converse `additionalModelRequestFields` bag, client and
    /// operator sources alike.
    #[cfg(feature = "bedrock")]
    BedrockConverse,
    /// The openai-compat Chat Completions body.
    #[cfg(feature = "openai-compat")]
    OpenAiCompat,
    /// The Gemini `generateContent` body.
    #[cfg(feature = "gemini")]
    Gemini,
}

/// True when `key` is a top-level field routectl writes for `dialect`, so an
/// extras value under that key must not reach the wire.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
pub fn is_managed_key(dialect: Dialect, key: &str) -> bool {
    match dialect {
        #[cfg(feature = "anthropic-api")]
        Dialect::AnthropicApi => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    // Composed from the client object or `req.reasoning`.
                    "thinking"
                        // Consumed into `tool_choice.disable_parallel_tool_use`;
                        // not a valid top-level Anthropic field.
                        | PARALLEL_TOOL_CALLS_KEY
                )
        }
        #[cfg(feature = "bedrock")]
        Dialect::BedrockInvokeOperator => {
            is_managed_key(Dialect::AnthropicApi, key)
                // Written by the Invoke seam before the operator merge, which
                // inserts over the assembled body.
                || matches!(key, "anthropic_version")
        }
        #[cfg(feature = "bedrock")]
        Dialect::BedrockConverse => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    // Bag keys routectl writes from canonical fields.
                    "thinking"
                        | "output_config"
                        // Converse top-level body fields; never valid in the bag.
                        | "inferenceConfig"
                        | "toolConfig"
                        | "additionalModelResponseFieldPaths"
                )
        }
        #[cfg(feature = "openai-compat")]
        Dialect::OpenAiCompat => is_canonical_request_key(key),
        #[cfg(feature = "gemini")]
        Dialect::Gemini => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    "contents" | "systemInstruction" | "toolConfig" | "generationConfig"
                )
        }
    }
}

/// The client extras one egress may merge for a request, split by where the
/// request came from.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
#[derive(Debug)]
pub struct ClientExtras<'a> {
    /// The bag to hand the egress's merge. Borrowed whole when the request
    /// speaks the egress's dialect; otherwise rebuilt from the keys the egress
    /// handles itself plus the operator's own `payload_extras`.
    pub forwardable: Option<Cow<'a, Value>>,
    /// Top-level keys withheld because a client of another ingress dialect
    /// supplied them. Sorted, so a diagnostic naming them is deterministic.
    pub foreign_keys: Vec<&'a str>,
}

/// The HTTP ingress, if any, whose clients spell their extras in an egress's
/// vocabulary.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchingIngress {
    /// The egress sends an ingress dialect's own wire shape, so requests of
    /// that dialect speak its vocabulary ([`forwards_unknowns`]). A
    /// managed-key spelling from a client of another dialect is kept for the
    /// egress's merge to refuse, exactly as it refuses one from that ingress.
    #[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
    SameDialect(EgressKind),
    /// Requests of this provenance speak the egress's vocabulary although no
    /// ingress sends the egress's wire shape (the Bedrock Converse bag takes
    /// Anthropic-spelled fields). A managed-key spelling from a client of
    /// another dialect is kept for the egress's merge to refuse, exactly as
    /// it refuses one from this ingress.
    #[cfg_attr(not(feature = "bedrock"), allow(dead_code))]
    Http(RequestProvenance),
    /// No HTTP ingress speaks the egress's dialect; only a library caller
    /// does. An HTTP client's key that shares a managed key's spelling was
    /// addressed to another vendor, so it is foreign like any other key
    /// rather than refused as an override.
    #[cfg(feature = "gemini")]
    LibraryOnly,
}

/// Split `req.provider_extras` for an egress whose vocabulary is spoken by
/// the `matching` ingress.
///
/// A library caller built its extras itself and a matching ingress swept keys
/// spelled for this egress, so both forward unchanged. Every other provenance,
/// including any variant added later, keeps only:
///
/// - keys the egress handles itself whatever their source: `handled_by_egress`
///   (a key it translates into another field, or withholds under its own
///   count) and, when an HTTP ingress matches, those its managed-key
///   predicate refuses. The egress's own handling of them stays identical
///   across provenances.
/// - the operator's `payload_extras`, with the operator's value, so a client
///   contribution merged under an operator key does not ride along.
///
/// Any other key is foreign and lands in `foreign_keys`.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
pub fn client_extras_for<'a>(
    req: &'a ChatRequest,
    egress: Dialect,
    matching: MatchingIngress,
    handled_by_egress: &[&str],
) -> ClientExtras<'a> {
    let dispatched = req.provider_extras.as_ref();
    if speaks_egress_dialect(req.routectl_internal.provenance, matching) {
        return ClientExtras {
            forwardable: dispatched.map(Cow::Borrowed),
            foreign_keys: Vec::new(),
        };
    }
    let Some(dispatched) = dispatched.and_then(Value::as_object) else {
        return ClientExtras {
            forwardable: None,
            foreign_keys: Vec::new(),
        };
    };
    let operator = operator_payload_extras(req);
    let mut kept = Map::new();
    let mut foreign_keys = Vec::new();
    for (key, value) in dispatched {
        let refused_as_managed = match matching {
            #[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
            MatchingIngress::SameDialect(_) => is_managed_key(egress, key),
            MatchingIngress::Http(_) => is_managed_key(egress, key),
            #[cfg(feature = "gemini")]
            MatchingIngress::LibraryOnly => false,
        };
        if refused_as_managed || handled_by_egress.contains(&key.as_str()) {
            kept.insert(key.clone(), value.clone());
            continue;
        }
        let operator_value = operator.and_then(|o| o.get(key));
        if operator_value != Some(value) {
            foreign_keys.push(key.as_str());
        }
        if let Some(v) = operator_value {
            kept.insert(key.clone(), v.clone());
        }
    }
    foreign_keys.sort_unstable();
    ClientExtras {
        forwardable: Some(Cow::Owned(Value::Object(kept))),
        foreign_keys,
    }
}

/// One top-level extras entry an egress reads into a field it composes
/// itself, rather than merging the entry whole.
#[cfg(feature = "bedrock")]
#[derive(Debug)]
pub struct ReadableExtra<'a> {
    /// The value the egress may read: the merged one when the request speaks
    /// the egress's dialect, otherwise only the operator's `payload_extras`
    /// value, if any.
    pub value: Option<&'a Value>,
    /// True when a client of another ingress dialect supplied a value the
    /// egress must not read.
    pub foreign: bool,
}

/// Read the `key` entry of `req.provider_extras` under the same dialect rule
/// as [`client_extras_for`].
#[cfg(feature = "bedrock")]
pub fn readable_extra<'a>(
    req: &'a ChatRequest,
    matching: RequestProvenance,
    key: &str,
) -> ReadableExtra<'a> {
    let dispatched = req.provider_extras.as_ref().and_then(|e| e.get(key));
    if speaks_egress_dialect(
        req.routectl_internal.provenance,
        MatchingIngress::Http(matching),
    ) {
        return ReadableExtra {
            value: dispatched,
            foreign: false,
        };
    }
    let Some(dispatched) = dispatched else {
        return ReadableExtra {
            value: None,
            foreign: false,
        };
    };
    let operator = req
        .routectl_internal
        .operator_payload_extras
        .as_deref()
        .and_then(|o| o.get(key));
    ReadableExtra {
        value: operator,
        foreign: operator != Some(dispatched),
    }
}

/// Declare each withheld foreign-dialect key as removed from the top level
/// of the body (the Converse bag counts as its own top level). A key that is
/// not token-shaped is left undeclared, so it still surfaces as a finding.
#[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
pub fn declare_foreign_keys(tap: Option<&WireTap>, foreign_keys: &[&str]) {
    let Some(tap) = tap else {
        return;
    };
    for key in foreign_keys {
        if let Some(path) = crate::wire_declare::member_pointer("", key) {
            crate::wire_declare::declare(tap, EditSite::ManagedExtrasStrip, path, EditKind::Remove);
        }
    }
}

/// Name of the cache marker routectl places and strips itself; a client's
/// message-level value is reserved, never carried.
pub const MESSAGE_CACHE_MARKER: &str = "cache_control";

/// One member a client sent on a carrier node (a message, a typed content
/// part, a custom tool, a system block): the client-coordinate pattern of the
/// node, every index a wildcard, and the member's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CarriedMember<'a> {
    /// Pattern of the node the member sat on, as the client wrote it.
    pub parent: &'static str,
    /// The member's name.
    pub key: &'a str,
}

/// Every member `req`'s carrier nodes hold that a wire of another dialect
/// cannot carry, one entry per (node pattern, name): the members a
/// same-dialect egress would emit, plus a message's own `cache_control`,
/// which no egress forwards. Empty for a carrier-free request.
pub fn carried_members(req: &ChatRequest) -> BTreeSet<CarriedMember<'_>> {
    let (message, part) = message_patterns(req.routectl_internal.provenance);
    let mut members = BTreeSet::new();
    for msg in req.messages.iter() {
        insert_members(&mut members, message, message_members(msg));
        let MessageContent::Parts(parts) = &msg.content else {
            continue;
        };
        for known in parts.iter().filter_map(|p| match p {
            ContentPart::Known(known) => Some(known),
            ContentPart::Other { .. } => None,
        }) {
            insert_members(&mut members, part, known.forwardable_extras());
        }
    }
    for tool in req.tools.iter().flatten() {
        if let ToolDef::Custom(custom) = tool {
            insert_members(&mut members, "/tools/*", custom.forwardable_extras());
        }
    }
    if let Some(SystemContent::Blocks(blocks)) = &req.system {
        for block in blocks {
            insert_members(&mut members, "/system/*", block.forwardable_extras());
        }
    }
    members
}

fn insert_members<'a>(
    members: &mut BTreeSet<CarriedMember<'a>>,
    parent: &'static str,
    carried: impl Iterator<Item = (&'a String, &'a Value)>,
) {
    members.extend(carried.map(|(key, _)| CarriedMember { parent, key }));
}

/// A message's carried members plus its reserved cache marker.
fn message_members(msg: &Message) -> impl Iterator<Item = (&String, &Value)> {
    msg.forwardable_extras().chain(
        msg.extras
            .iter()
            .filter(|(key, _)| key.as_str() == MESSAGE_CACHE_MARKER),
    )
}

/// Client-coordinate patterns of a message and of one of its content parts:
/// a Responses client writes its turns as `input` items.
const fn message_patterns(provenance: RequestProvenance) -> (&'static str, &'static str) {
    match provenance {
        RequestProvenance::ResponsesIngress => ("/input/*", "/input/*/content/*"),
        _ => ("/messages/*", "/messages/*/content/*"),
    }
}

/// Withhold `req`'s carried members from an `egress` whose wire does not
/// speak the request's dialect: nothing is emitted from a carrier there, so
/// each member is declared removed at its client pattern and the request is
/// noted in `tally`, which counts it once with any foreign top-level key. A
/// request whose members `egress` forwards is left alone. A name that is not
/// token-shaped is counted but left undeclared, so it still surfaces as a
/// finding.
pub fn withhold_carried_members(
    req: &ChatRequest,
    egress: EgressKind,
    tally: &mut ForeignDialectExtraTally,
) {
    if forwards_unknowns(req.routectl_internal.provenance, egress) {
        return;
    }
    let members = carried_members(req);
    if members.is_empty() {
        return;
    }
    tally.record_carried(&members);
    let Some(tap) = crate::wire_declare::tap(req) else {
        return;
    };
    for member in &members {
        if let Some(path) = crate::wire_declare::member_pointer(member.parent, member.key) {
            crate::wire_declare::declare(tap, EditSite::ManagedExtrasStrip, path, EditKind::Remove);
        }
    }
}

/// Declare the client's message-level cache marker removed under
/// `messages_pattern` (the client's messages array, every index a wildcard)
/// when any of `messages` carried one: the marker is reserved, so a
/// same-dialect emission never forwards it.
#[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
pub fn declare_message_cache_marker(
    tap: Option<&WireTap>,
    messages: &[Message],
    messages_pattern: &str,
) {
    let Some(tap) = tap else {
        return;
    };
    if messages
        .iter()
        .any(|msg| msg.extras.contains_key(MESSAGE_CACHE_MARKER))
    {
        crate::wire_declare::declare(
            tap,
            EditSite::CacheMarkerStrip,
            format!("{messages_pattern}/{MESSAGE_CACHE_MARKER}"),
            EditKind::Remove,
        );
    }
}

/// Whether a request of `provenance` carries extras spelled for an egress
/// whose vocabulary the `matching` ingress speaks.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn speaks_egress_dialect(provenance: RequestProvenance, matching: MatchingIngress) -> bool {
    match matching {
        #[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
        MatchingIngress::SameDialect(egress) => forwards_unknowns(provenance, egress),
        MatchingIngress::Http(matching) => {
            provenance == RequestProvenance::Library || provenance == matching
        }
        #[cfg(feature = "gemini")]
        MatchingIngress::LibraryOnly => provenance == RequestProvenance::Library,
    }
}

/// The operator's `payload_extras` object, if any. Callers consult it only
/// for keys the attempt's merged `provider_extras` still carries, so a
/// dispatch-time strip that removed a key is never undone by this layer.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn operator_payload_extras(req: &ChatRequest) -> Option<&Map<String, Value>> {
    req.routectl_internal
        .operator_payload_extras
        .as_deref()?
        .as_object()
}

/// Per-request record that client extras from another ingress dialect were
/// withheld. A flag, not a count: the `(lane, class)` counters are per
/// request, and the egress that owns the request's lane flushes it once.
#[must_use = "a foreign-dialect withhold counts nothing until its lane flushes it"]
#[derive(Debug, Default)]
pub struct ForeignDialectExtraTally {
    keys: Vec<String>,
}

impl ForeignDialectExtraTally {
    /// Note the request's withheld keys, if any.
    #[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
    pub fn record(&mut self, foreign_keys: &[&str]) {
        self.keys
            .extend(foreign_keys.iter().map(|k| (*k).to_string()));
    }

    /// Note the request's withheld carried members, by pattern and name.
    pub fn record_carried(&mut self, members: &BTreeSet<CarriedMember<'_>>) {
        self.keys
            .extend(members.iter().map(|m| format!("{}/{}", m.parent, m.key)));
    }

    /// Whether `key` was noted as withheld for this request.
    #[cfg(feature = "bedrock")]
    pub fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }

    /// Whether this request withheld any foreign-dialect client extra.
    pub const fn dropped(&self) -> bool {
        !self.keys.is_empty()
    }

    /// The withheld key names, comma-joined and sanitized for a log line.
    /// Values are never part of it: a client extra may carry a credential.
    pub fn keys_for_log(&self) -> String {
        routectl_core::sanitize_for_log(&self.keys.join(","))
    }
}

#[cfg(all(
    test,
    any(
        feature = "anthropic-api",
        feature = "openai-compat",
        feature = "gemini"
    )
))]
#[path = "managed_keys_tests.rs"]
mod tests;
