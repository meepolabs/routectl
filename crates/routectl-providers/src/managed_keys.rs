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

#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
use serde_json::{Map, Value};

use routectl_core::is_canonical_request_key;
#[cfg(any(feature = "anthropic-api", feature = "openai-compat"))]
use routectl_core::wire_check::{EditKind, EditSite, WireTap};
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
use routectl_core::{ChatRequest, RequestProvenance};

/// The extras merge whose managed set a key is checked against.
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
                        | "parallel_tool_calls"
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
    /// Requests of this provenance speak the egress's dialect. A managed-key
    /// spelling from a client of another dialect is kept for the egress's
    /// merge to refuse, exactly as it refuses one from this ingress.
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

/// Whether a request of `provenance` carries extras spelled for an egress
/// whose vocabulary the `matching` ingress speaks.
#[cfg(any(
    feature = "anthropic-api",
    feature = "openai-compat",
    feature = "gemini"
))]
fn speaks_egress_dialect(provenance: RequestProvenance, matching: MatchingIngress) -> bool {
    provenance == RequestProvenance::Library || matching == MatchingIngress::Http(provenance)
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
#[cfg(feature = "anthropic-api")]
#[must_use = "a foreign-dialect withhold counts nothing until its lane flushes it"]
#[derive(Debug, Default)]
pub struct ForeignDialectExtraTally {
    keys: Vec<String>,
}

#[cfg(feature = "anthropic-api")]
impl ForeignDialectExtraTally {
    /// Note the request's withheld keys, if any.
    pub fn record(&mut self, foreign_keys: &[&str]) {
        self.keys
            .extend(foreign_keys.iter().map(|k| (*k).to_string()));
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

#[cfg(test)]
#[path = "managed_keys_tests.rs"]
mod tests;
