//! Resolves a use-time upstream rejection to the CANONICAL capability it
//! names -- the single closed-set resolver shared by learn capture,
//! act-side route-away/strip lookup (via the same canonical key namespace),
//! and probe same-capability settlement. Its output keys on the request's
//! capability namespace (the `derive_feature_keys` vocabulary), so a learned
//! negative and a later dispatch-time lookup meet on identical strings.
//!
//! Data-driven, keyed by provider `kind` (the `class_policy` pattern), and
//! deliberately outside `routectl-core` -- the failure classifier stays
//! body-parse free, and this is not a `Provider` trait method. Each arm also
//! attributes a detection PHASE: the wire-token arms are `FailurePhase::F1`
//! (the upstream named a parameter or field path), the feature-naming arm is
//! `FailurePhase::F2` (the upstream named the feature in prose). Arms:
//!
//! - SELF-IDENTIFYING: the classifier already lifted the rejection to
//!   [`FailureClass::FeatureUnsupported`]. For openai-compat, the class
//!   carries an `error.code` TOKEN (`unsupported_parameter`, ...), which is
//!   NOT a capability key; the field that actually names the offending
//!   capability is `error.param` -- a WIRE param name (`response_format`,
//!   ...), which is not itself the request-capability key either. So this
//!   arm maps `/error/param` onto the canonical request-capability namespace
//!   through a CLOSED set: an explicit translation table
//!   ([`OPENAI_PARAM_TRANSLATIONS`], e.g. `response_format` ->
//!   `structured_output`) for wire params that name a capability under a
//!   different key than the request side, plus a passthrough for a param
//!   that -- after `strip_date_suffix` + `normalize_capability_key` -- is
//!   ALREADY a well-known capability key (a typed built-in tool an upstream
//!   rejects by `type`). A param outside that closed set, or an absent
//!   `param`, yields `None` -- the loop never learns a capability the request
//!   side cannot look up under the same key -- EXCEPT the small closed set of
//!   paramless rejections that still name a correct target-level route-away
//!   (a geo/region block), for which the code token itself is the canonical
//!   key. For other providers (Bedrock, as its token table grows) the class
//!   carries a field path and is normalized directly. One observation is
//!   trustworthy.
//! - INFERRED: a generic [`FailureClass::BadRequest`] whose free-text
//!   `error.message` names a capability only in prose. Matched by
//!   whole-phrase equality (case-insensitive) against a small per-provider
//!   table of phrases grounded in real captured / documented 400 envelopes.
//!   Precision over recall: an unverified phrase is omitted, and a
//!   near-miss or embedded phrase never matches.
//! - FEATURE-NAMING (F2): a [`FailureClass::BadRequest`] whose free-text
//!   `error.message` names the offending FEATURE explicitly. Matched by the
//!   anchored-template pipeline (the Bedrock-validation discipline reused for
//!   the nested `/error/message` shape) against per-provider CLOSED tables
//!   that ship EMPTY -- so F2 never fires on real traffic until a captured
//!   envelope grounds a pattern, mirroring the wire-token tables' precision.
//!
//! Every other class, provider, or malformed body yields `None` -- the
//! resolver never manufactures a false positive.

use routectl_core::capability::{
    FailurePhase, REASONING_REPLAY, STRUCTURED_OUTPUT, SignalTier, WELL_KNOWN_CAPABILITY_KEYS,
    normalize_capability_key,
};
use routectl_core::error::Error;
use routectl_core::failure_class::{ClassifiedFailure, FailureClass};

use crate::feature_keys::strip_date_suffix;

/// The openai-compat provider `kind` string. For this family the
/// `FeatureUnsupported` class carries an `error.code` token rather than a
/// capability, so the resolver reads `/error/param` instead.
const OPENAI_COMPAT_KIND: &str = "openai-compat";

/// The native AWS Bedrock provider `kind` string. On a generic `BadRequest`
/// this family reports the rejected field in a FLAT `{"__type","message"}`
/// envelope, not the nested `/error/message` shape the inferred arm reads --
/// so it gets its own top-level reader and an anchored-template extraction
/// path rather than the whole-phrase table.
const BEDROCK_KIND: &str = "bedrock";

/// The native OpenAI Responses provider `kind` string. Its lanes differ in
/// which reasoning artifacts they accept on replay, so its inferred table
/// carries the replay-rejection phrase.
const OPENAI_RESPONSES_KIND: &str = "openai-responses";

/// Closed-set translation of an openai-compat `error.param` WIRE param name
/// onto the canonical request-capability key the request side derives and
/// the act side looks up. openai names the offending field by its wire key
/// (`response_format` for constrained decoding), which the request side keys
/// under a DIFFERENT canonical name (`structured_output`); without this
/// mapping a learned negative keyed on the raw wire param would never meet
/// the request-derived key, so the loop would never route away.
///
/// EXTENSION POINT: add a `(wire_param, canonical_key)` row here when a new
/// provider body surface must map onto an existing canonical capability. A
/// param whose canonicalized form is ALREADY a well-known capability key
/// (a typed built-in tool an upstream rejects by `type`) passes through
/// without a row; every other param yields `None` (no-learn), keeping the
/// set closed.
const OPENAI_PARAM_TRANSLATIONS: &[(&str, &str)] = &[("response_format", STRUCTURED_OUTPUT)];

/// openai-compat `error.code` tokens whose rejection carries no
/// `/error/param` yet still names a correct route-away: a geo/region block
/// applies to the whole account, not a single request field. For these the
/// code token itself is the canonical key. Every other paramless openai
/// rejection yields `None` (no-learn).
///
/// DORMANT at capture: this token is a target-level geo signal, not a
/// request-derived feature, so `derive_feature_keys` never produces it and
/// the request-membership gate in `observe_for_learning` blocks the learn.
/// A target-level route-away for geo-blocks needs its own mechanism (out of
/// scope here); the resolver arm stays so that mechanism can reuse it.
const OPENAI_PARAMLESS_ROUTE_AWAY: &[&str] = &["unsupported_country_region_territory"];

/// A learned capability key. Feature keys are open-namespace strings
/// shared with the catalog prior and the alias-chain pre-filter.
type FeatureKey = String;

/// Capability key for assistant-message prefill. Open-namespace key (not
/// one of the well-known `routectl_core::capability` consts): the registry
/// namespace is open, so a capability the upstream names by prose is a
/// first-class key.
///
/// DORMANT until an act-side derivation for this capability exists. The
/// resolver emits `prefill` for a matched phrase, but `derive_feature_keys`
/// has no rule that produces it, so the capture-side request-membership gate
/// (`request_features.contains(key)` in `observe_for_learning`) blocks the
/// learn: learn-side output is not yet a subset of the act-side vocabulary.
/// The table and matcher stay wired so the day an act-side derivation lands,
/// the loop closes with no matcher change.
const PREFILL: &str = "prefill";

/// One inferred-rejection phrase: a free-text `error.message` equal to
/// `phrase` (case-insensitive, trimmed) names `capability`.
struct InferredPhrase {
    /// The verbatim upstream `error.message` phrase.
    phrase: &'static str,
    /// The capability key the phrase names.
    capability: &'static str,
}

/// Anthropic Messages API inferred-rejection phrases. Small by design;
/// each phrase is grounded in a real captured / documented 400 envelope
/// (sources cited in the module tests). Unverified capabilities wait.
const ANTHROPIC_INFERRED: &[InferredPhrase] = &[InferredPhrase {
    phrase: "Prefilling assistant messages is not supported for this model.",
    capability: PREFILL,
}];

/// OpenAI Responses inferred-rejection phrases. The content-validating lane
/// family checks the encrypted-content PREFIX and rejects a blob minted by
/// the id-validating family with this message; it names the replay of a
/// prior artifact, not the ability to reason, so it resolves to
/// [`REASONING_REPLAY`] and never to `thinking`.
///
/// The phrase is the VERBATIM captured `error.message`, matched
/// whole-phrase like every other inferred row: a shortened prefix would
/// never equal the real message and the row would be dead on real traffic.
const OPENAI_RESPONSES_INFERRED: &[InferredPhrase] = &[InferredPhrase {
    phrase: "encrypted content missing recognized prefix (expected `rsn_` or `smry_`)",
    capability: REASONING_REPLAY,
}];

/// Anchored-template extractions for a provider's F2 FEATURE-NAMING rejection
/// message: a `BadRequest` whose free-text `error.message` names the
/// offending FEATURE explicitly (self-identifying) rather than a wire token.
/// Each entry is a `(prefix, suffix)` literal template bracketing exactly ONE
/// extracted token; the whole trimmed message must equal `prefix + token +
/// suffix`, so wording drift, an extra sentence, or a missing anchor all fail
/// closed -- the [`BEDROCK_VALIDATION_TEMPLATES`] discipline reused for the
/// nested `/error/message` shape.
///
/// Ships EMPTY (precision over recall): no entry without a real captured
/// envelope, mirroring the wire-token tables' discipline exactly. Provisional
/// shapes live in test data only, so F2 never fires on real traffic until a
/// pattern is grounded.
const ANTHROPIC_FEATURE_NAMING_TEMPLATES: &[(&str, &str)] = &[];

/// Closed-set translation of a normalized F2 feature-naming token onto the
/// canonical request-capability key the request side derives and the act side
/// looks up (the [`OPENAI_PARAM_TRANSLATIONS`] pattern). A token outside this
/// set yields `None` (no-learn), keeping the set closed.
///
/// Ships EMPTY alongside the template table.
const ANTHROPIC_FEATURE_NAMING_TRANSLATIONS: &[(&str, &str)] = &[];

/// Resolve a classified rejection to the CANONICAL capability it names, the
/// signal tier of that evidence, and the DETECTION PHASE that attributed it,
/// or `None` when the rejection names no capability this resolver can
/// attribute. The single shared resolver: its output keys on the
/// request-capability namespace so learn capture, the act-side lookup, and
/// probe settlement all meet on identical strings.
///
/// Every wire-token arm attributes [`FailurePhase::F1`] (the upstream named a
/// wire parameter or field path). The feature-naming arm attributes
/// [`FailurePhase::F2`] (the upstream named the offending feature in prose);
/// its per-provider tables ship EMPTY, so F2 never fires on real traffic
/// until a captured envelope grounds a pattern.
pub fn resolve_requested_capability(
    provider_kind: &str,
    err: &Error,
    cf: &ClassifiedFailure,
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    match &cf.class {
        FailureClass::FeatureUnsupported { capability } => {
            resolve_self_identifying(provider_kind, err, capability)
        }
        FailureClass::BadRequest if provider_kind == BEDROCK_KIND => match_bedrock_validation(err),
        // F2 self-identifying feature-naming outranks F1 inferred prose, so it
        // is tried first; with the shipped-empty tables it always falls
        // through to the inferred arm on real traffic.
        FailureClass::BadRequest => {
            match_feature_naming(provider_kind, err).or_else(|| match_inferred(provider_kind, err))
        }
        _ => None,
    }
}

/// The self-identifying arm. For openai-compat the class token is an
/// `error.code`, not a capability, so the real capability is read from
/// `/error/param`; other providers carry a field path in the token and are
/// normalized directly.
fn resolve_self_identifying(
    provider_kind: &str,
    err: &Error,
    upstream_token: &str,
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    if provider_kind == OPENAI_COMPAT_KIND {
        return resolve_openai_param(err, upstream_token);
    }
    Some((
        normalize_capability_key(upstream_token, provider_kind),
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
    ))
}

/// openai-compat: map `/error/param` onto the canonical request-capability
/// namespace through the closed set (an explicit translation row, then a
/// well-known tool-type passthrough), so a learned key lands where the
/// request side derives it. A missing param yields `None` unless the code
/// token is a paramless route-away.
fn resolve_openai_param(
    err: &Error,
    upstream_token: &str,
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    if let Some(param) = upstream_error_field(err, "param") {
        return resolve_openai_param_surface(param.trim());
    }
    if OPENAI_PARAMLESS_ROUTE_AWAY.contains(&upstream_token) {
        return Some((
            upstream_token.to_string(),
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
        ));
    }
    None
}

/// Resolve a raw openai `error.param` surface against the closed set: an
/// explicit translation row first, then a well-known tool-type passthrough
/// (canonicalized via `strip_date_suffix` + `normalize_capability_key`),
/// else `None`. The set is closed so an arbitrary rejected wire param never
/// learns a key the act side cannot look up.
fn resolve_openai_param_surface(param: &str) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    // Share one date-stripped input across both closed-set paths so a dated
    // variant of a translated surface (`response_format_20250305`) still
    // resolves rather than silently falling through to no-learn.
    let base = strip_date_suffix(param);
    if let Some((_, canonical)) = OPENAI_PARAM_TRANSLATIONS
        .iter()
        .find(|(surface, _)| *surface == base)
    {
        return Some((
            (*canonical).to_string(),
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
        ));
    }
    let canonical = normalize_capability_key(base, OPENAI_COMPAT_KIND);
    if WELL_KNOWN_CAPABILITY_KEYS.contains(&canonical.as_str()) {
        return Some((canonical, SignalTier::SelfIdentifying, FailurePhase::F1));
    }
    None
}

/// Whole-phrase match of the upstream `error.message` against the
/// provider's inferred table.
fn match_inferred(
    provider_kind: &str,
    err: &Error,
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    let table = inferred_table_for(provider_kind)?;
    let message = upstream_error_field(err, "message")?;
    let needle = message.trim();
    let matched = table
        .iter()
        .find(|entry| entry.phrase.eq_ignore_ascii_case(needle))?;
    Some((
        matched.capability.to_string(),
        SignalTier::Inferred,
        FailurePhase::F1,
    ))
}

/// The inferred phrase table for a provider `kind`, or `None` when the
/// provider has no inferred matcher in this slice.
fn inferred_table_for(provider_kind: &str) -> Option<&'static [InferredPhrase]> {
    match provider_kind {
        "anthropic-api" => Some(ANTHROPIC_INFERRED),
        OPENAI_RESPONSES_KIND => Some(OPENAI_RESPONSES_INFERRED),
        _ => None,
    }
}

/// The F2 feature-naming arm: a `BadRequest` whose free-text `error.message`
/// names the offending FEATURE explicitly. Reads the nested `/error/message`,
/// then runs the provider's anchored-template pipeline. With the shipped-empty
/// tables this always yields `None`, so F2 never fires on real traffic.
fn match_feature_naming(
    provider_kind: &str,
    err: &Error,
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    let (templates, translations) = feature_naming_tables_for(provider_kind)?;
    let message = upstream_error_field(err, "message")?;
    extract_feature_naming_capability(&message, provider_kind, templates, translations)
}

/// The `(templates, translations)` feature-naming tables for a provider
/// `kind`, or `None` when the provider has no feature-naming matcher in this
/// slice. Both tables ship EMPTY, so a present provider still yields no match.
type PatternTable = &'static [(&'static str, &'static str)];

fn feature_naming_tables_for(provider_kind: &str) -> Option<(PatternTable, PatternTable)> {
    match provider_kind {
        "anthropic-api" => Some((
            ANTHROPIC_FEATURE_NAMING_TEMPLATES,
            ANTHROPIC_FEATURE_NAMING_TRANSLATIONS,
        )),
        _ => None,
    }
}

/// True when `provider_kind` carries a feature-naming (F2) table at all
/// (currently or empty). The learn-side drift observer uses it to fire ONLY
/// for a provider whose F2 table exists yet matched nothing -- a deterministic
/// feature-carrying rejection that a grounded pattern could have attributed --
/// rather than for every unresolved rejection on every provider.
pub fn has_feature_naming_table(provider_kind: &str) -> bool {
    feature_naming_tables_for(provider_kind).is_some()
}

/// Run the feature-naming anchored-template pipeline over a trimmed `message`
/// (the [`extract_bedrock_capability`] precedent): the first template whose
/// anchors bracket the message extracts its single token; the token must be
/// token-shaped ASCII ([`is_safe_param_token`]) or the match fails closed; the
/// normalized token must resolve through the closed `translations` set to a
/// canonical capability. The upstream named the feature explicitly, so a match
/// is self-identifying evidence at [`FailurePhase::F2`]. Split from the table
/// consts so tests can drive the engine with provisional shapes without
/// touching the shipped-empty production tables.
fn extract_feature_naming_capability(
    message: &str,
    provider_kind: &str,
    templates: &[(&str, &str)],
    translations: &[(&str, &str)],
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    let needle = message.trim();
    let token = templates
        .iter()
        .find_map(|&(prefix, suffix)| extract_anchored_token(needle, prefix, suffix))?;
    if !is_safe_param_token(token) {
        return None;
    }
    let normalized = normalize_capability_key(token, provider_kind);
    let capability = translations
        .iter()
        .find_map(|&(surface, canonical)| (normalized == surface).then_some(canonical))?;
    Some((
        capability.to_string(),
        SignalTier::SelfIdentifying,
        FailurePhase::F2,
    ))
}

/// Ceiling on the upstream error body we JSON-parse to extract a field.
/// Guards BOTH the self-identifying `error.param` read and the inferred
/// `error.message` read: a malicious upstream must not be able to force
/// repeated large-JSON parses on the routing path; a body over this cap is
/// not parsed and yields `None`. Bound to the shared core constant the
/// request-fault producers cap their stored body at, so the producer and
/// this consumer cannot drift and truncate a real envelope into unparseable
/// JSON.
const MAX_ERROR_BODY_BYTES: usize = routectl_core::MAX_ERROR_BODY_BYTES;

/// Extract `error.<field>` (a string) from an [`Error::Upstream`] body.
/// Shared by the self-identifying `param` read and the inferred `message`
/// read. Any non-upstream error variant, an over-cap body, a non-JSON body,
/// a missing field, or a non-string value yields `None`.
fn upstream_error_field(err: &Error, field: &str) -> Option<String> {
    let Error::Upstream { body, .. } = err else {
        return None;
    };
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get("error")?.get(field)?.as_str().map(str::to_string)
}

/// Upper bound on a param token surfaced verbatim in an operator log. A
/// canonical capability param (`web_search`, `response_format`, ...) is far
/// shorter; this cap only bounds an adversarial or buggy upstream.
const MAX_PARAM_TOKEN_LEN: usize = 64;

/// True if `param` is safe to surface verbatim in an operator log: a
/// non-empty, bounded, single-token ASCII string with no whitespace or
/// control bytes (`is_ascii_graphic` is the printable ASCII range excluding
/// space). Any canonical capability param the closed-set resolver accepts is
/// token-shaped by construction, so this gate re-admits every legitimate
/// value while dropping log-forging content (newlines, control bytes) and
/// oversized blobs.
fn is_safe_param_token(param: &str) -> bool {
    !param.is_empty()
        && param.len() <= MAX_PARAM_TOKEN_LEN
        && param.bytes().all(|b| b.is_ascii_graphic())
}

/// The upstream `error.param` string, for observability enrichment on the
/// learn event -- emitted ONLY when [`is_safe_param_token`] holds. Shares the
/// same over-cap / non-JSON / missing-field guard as the resolver's read;
/// carries the capability the upstream named, never a request body / message
/// / prompt. An unsafe (oversized, whitespace/control-laden, or empty) param
/// yields `None` so the log boundary never trusts the raw upstream field.
pub fn upstream_param(err: &Error) -> Option<String> {
    upstream_error_field(err, "param").filter(|param| is_safe_param_token(param))
}

/// The `__type` discriminant an AWS Bedrock request-validation 400 carries,
/// namespace-stripped as the lift lands it on
/// [`Error::Upstream::upstream_type`]. Used only for drift observability at
/// the learn site (a rejection whose lifted type is this yet matched no
/// template is visible wording drift), never to attribute a capability.
const BEDROCK_VALIDATION_EXCEPTION_TYPE: &str = "ValidationException";

/// Anchored-template extractions for a Bedrock `ValidationException`
/// message. Each entry is a `(prefix, suffix)` literal template with
/// exactly ONE extracted token between the anchors; the whole (trimmed)
/// message must equal `prefix + token + suffix`, so wording drift, an extra
/// sentence, or a missing anchor all fail closed. The extracted token must
/// pass [`is_safe_param_token`], normalize via [`normalize_capability_key`],
/// and hit [`BEDROCK_TOKEN_TRANSLATIONS`].
///
/// Grounded byte-for-byte in captured bedrock-runtime InvokeModel 400
/// envelopes:
/// - a rejected tool type, single-quoted
///   (`tool type '<type>' is not supported for this model`);
/// - a rejected request field, pydantic-style prefix
///   (`<field>: Extra inputs are not permitted`).
///
/// A message matching a template but whose extracted token has no
/// [`BEDROCK_TOKEN_TRANSLATIONS`] row (the field-name case, absent a closed
/// row) stays dormant: the seam extracts, but the closed set yields no
/// capability, so nothing is learned.
const BEDROCK_VALIDATION_TEMPLATES: &[(&str, &str)] = &[
    ("tool type '", "' is not supported for this model"),
    ("", ": Extra inputs are not permitted"),
];

/// Closed-set translation of a normalized Bedrock validation token onto the
/// canonical request-capability key the request side derives and the act
/// side looks up (the [`OPENAI_PARAM_TRANSLATIONS`] pattern). A token
/// outside this set yields `None` (no-learn), keeping the set closed.
///
/// A rejected tool type maps onto the identically-named tool-type key
/// `derive_feature_keys` emits for a `tools[]` entry of that type, so a
/// learned key lands where the request side derives it and the membership
/// gate admits the observation. A rejected wire field name has no row: it
/// is not a `derive_feature_keys`-producible key, so it stays dormant.
const BEDROCK_TOKEN_TRANSLATIONS: &[(&str, &str)] = &[("advisor", "advisor")];

/// The Bedrock `BadRequest` arm: gate on the lifted `ValidationException`
/// discriminator, then read the flat validation message and run the
/// anchored-template extraction pipeline. The discriminator gate fires
/// BEFORE the message read: the captured must-not-learn rejections (a bad
/// model id, an unknown beta flag) share the exact flat-envelope shape of
/// the learnable one, so shape alone cannot discriminate -- only the lifted
/// type may unlock a match. A rejection without the lifted discriminator
/// yields `None` (a visible, recoverable non-learn) rather than risking a
/// silent false attribution from a shape fallback.
fn match_bedrock_validation(err: &Error) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    if !is_bedrock_validation_exception(err) {
        return None;
    }
    let message = bedrock_validation_message(err)?;
    extract_bedrock_capability(
        &message,
        BEDROCK_VALIDATION_TEMPLATES,
        BEDROCK_TOKEN_TRANSLATIONS,
    )
}

/// Run the anchored-template pipeline over a trimmed validation `message`:
/// the first template whose anchors bracket the message extracts its single
/// token; the token must be token-shaped ASCII ([`is_safe_param_token`]) or
/// the match fails closed; the normalized token must resolve through the
/// closed `translations` set to a canonical capability. Split from the table
/// consts so tests can drive the engine with provisional shapes without
/// touching the shipped-empty production tables. A Bedrock validation
/// rejection names a wire field, so a match is [`FailurePhase::F1`].
fn extract_bedrock_capability(
    message: &str,
    templates: &[(&str, &str)],
    translations: &[(&str, &str)],
) -> Option<(FeatureKey, SignalTier, FailurePhase)> {
    let needle = message.trim();
    let token = templates
        .iter()
        .find_map(|&(prefix, suffix)| extract_anchored_token(needle, prefix, suffix))?;
    if !is_safe_param_token(token) {
        return None;
    }
    let normalized = normalize_capability_key(token, BEDROCK_KIND);
    let capability = translations
        .iter()
        .find_map(|&(surface, canonical)| (normalized == surface).then_some(canonical))?;
    Some((
        capability.to_string(),
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
    ))
}

/// Extract the single token an anchored template brackets: the whole message
/// must start with `prefix` and end with `suffix`, and the token is exactly
/// what remains between them. A message that does not carry both anchors
/// yields `None` (no fuzzy `contains`).
fn extract_anchored_token<'a>(message: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    message.strip_prefix(prefix)?.strip_suffix(suffix)
}

/// Read a top-level string field from a FLAT AWS error envelope
/// (`{"__type","message"}`) on an [`Error::Upstream`] body, bounded by the
/// module's [`MAX_ERROR_BODY_BYTES`] cap. Distinct from
/// [`upstream_error_field`], which reads the NESTED `/error/<field>` shape
/// and returns `None` on this flat body. Any non-upstream variant, an
/// over-cap body, a non-JSON body, a missing field, or a non-string value
/// yields `None`.
fn bedrock_flat_field(err: &Error, field: &str) -> Option<String> {
    let Error::Upstream { body, .. } = err else {
        return None;
    };
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get(field)?.as_str().map(str::to_string)
}

/// The flat Bedrock validation `message`, or `None` when the body is not a
/// flat envelope (e.g. the nested `/error/message` shape).
fn bedrock_validation_message(err: &Error) -> Option<String> {
    bedrock_flat_field(err, "message")
}

/// True when `err` carries a Bedrock `ValidationException`, read from the
/// lifted, namespace-stripped [`Error::Upstream::upstream_type`] rather than
/// re-parsed from the raw body. The native lane emits the namespaced wire
/// form (`com.amazon.coral.validate#ValidationException`) in the body but
/// lifts the bare `ValidationException` onto `upstream_type`; matching the
/// lifted token is what makes this predicate fire on real production input
/// (a raw-body match on the bare name is dead against the namespaced shape).
/// The learn site uses it to bump a drift counter when the matcher
/// attributed no capability yet the rejection WAS a validation fault -- so
/// wording drift is visible instead of silently reintroducing repeat 400s.
pub fn is_bedrock_validation_exception(err: &Error) -> bool {
    matches!(
        err,
        Error::Upstream {
            upstream_type: Some(t),
            ..
        } if t == BEDROCK_VALIDATION_EXCEPTION_TYPE
    )
}

#[cfg(test)]
mod tests {
    use super::resolve_requested_capability;
    use super::upstream_param;
    use super::{
        BEDROCK_VALIDATION_EXCEPTION_TYPE, MAX_ERROR_BODY_BYTES, MAX_PARAM_TOKEN_LEN,
        bedrock_validation_message, extract_bedrock_capability, extract_feature_naming_capability,
        is_bedrock_validation_exception,
    };
    use routectl_core::capability::{FailurePhase, SignalTier};
    use routectl_core::error::Error;
    use routectl_core::failure_class::{ClassifiedFailure, FailureClass, MatchedBy, classify};

    /// The verbatim Anthropic Messages API 400 body for a prefill
    /// rejection.
    const PREFILL_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Prefilling assistant messages is not supported for this model."}}"#;

    fn upstream(status: u16, body: &str, ty: Option<&str>, code: Option<&str>) -> Error {
        Error::upstream_full(
            "p",
            status,
            body,
            None,
            ty.map(str::to_string),
            code.map(str::to_string),
        )
    }

    fn cf(class: FailureClass) -> ClassifiedFailure {
        ClassifiedFailure {
            class,
            matched_by: MatchedBy::Status,
        }
    }

    fn anthropic_body(message: &str) -> String {
        serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message}
        })
        .to_string()
    }

    // --- Arm 1: self-identifying ---

    /// An openai-compat 400 whose `error.code` lifts to FeatureUnsupported
    /// and whose `error.param` names the offending capability.
    fn openai_unsupported_body(code: &str, param: &str) -> String {
        serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "code": code,
                "param": param,
                "message": "Unsupported parameter."
            }
        })
        .to_string()
    }

    #[test]
    fn openai_resolves_tool_type_param_via_passthrough() {
        // The classifier lifts the `error.code` token into FeatureUnsupported,
        // but the code token is NOT a capability -- the resolver must return
        // the canonicalized `/error/param` when it names a well-known
        // tool-type capability (a typed built-in an upstream rejects by type),
        // date suffix stripped.
        for (code, param, canonical) in [
            ("unsupported_parameter", "web_search_20250305", "web_search"),
            ("unsupported_value", "computer_use", "computer_use"),
        ] {
            // Arrange
            let body = openai_unsupported_body(code, param);
            let err = upstream(400, &body, Some("invalid_request_error"), Some(code));
            let classified = classify(&err, Some("openai-compat"));

            // Act
            let got = resolve_requested_capability("openai-compat", &err, &classified);

            // Assert
            assert_eq!(
                got,
                Some((
                    canonical.to_string(),
                    SignalTier::SelfIdentifying,
                    FailurePhase::F1
                )),
                "code {code} param {param}"
            );
        }
    }

    #[test]
    fn openai_translates_response_format_to_structured_output() {
        // openai names constrained decoding by its wire param
        // (`response_format`); the request side keys the same capability as
        // `structured_output`. The closed-set translation table maps the two
        // so a learned negative meets the request-derived key.
        let body = openai_unsupported_body("unsupported_parameter", "response_format");
        let err = upstream(
            400,
            &body,
            Some("invalid_request_error"),
            Some("unsupported_parameter"),
        );
        let classified = classify(&err, Some("openai-compat"));

        let got = resolve_requested_capability("openai-compat", &err, &classified);

        assert_eq!(
            got,
            Some((
                "structured_output".to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn openai_translates_dated_variant_of_a_translated_param() {
        // A dated variant of a translated wire surface is date-stripped
        // BEFORE the translation lookup, so it still resolves rather than
        // falling through the closed set to no-learn.
        let body = openai_unsupported_body("unsupported_parameter", "response_format_20250305");
        let err = upstream(
            400,
            &body,
            Some("invalid_request_error"),
            Some("unsupported_parameter"),
        );
        let classified = classify(&err, Some("openai-compat"));

        let got = resolve_requested_capability("openai-compat", &err, &classified);

        assert_eq!(
            got,
            Some((
                "structured_output".to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn openai_param_outside_the_closed_set_does_not_learn() {
        // A param that is neither a translation-table wire surface nor a
        // well-known tool-type key names a capability the request side cannot
        // look up under the same key -- the closed set yields None (no-learn),
        // so an arbitrary rejected body param never poisons routing.
        for param in ["reasoning", "temperature", "max_tokens", "seed", "stop"] {
            let body = openai_unsupported_body("unsupported_parameter", param);
            let err = upstream(
                400,
                &body,
                Some("invalid_request_error"),
                Some("unsupported_parameter"),
            );
            let classified = classify(&err, Some("openai-compat"));
            let got = resolve_requested_capability("openai-compat", &err, &classified);
            assert_eq!(got, None, "param {param}");
        }
    }

    #[test]
    fn openai_paramless_rejection_does_not_learn() {
        // A `unsupported_parameter` / `unsupported_value` rejection with no
        // `/error/param` names no capability -- the resolver must NOT fall
        // back to the code token (it is not a capability key).
        for code in ["unsupported_parameter", "unsupported_value"] {
            let err = upstream(400, "{}", Some("invalid_request_error"), Some(code));
            let classified = classify(&err, Some("openai-compat"));
            let got = resolve_requested_capability("openai-compat", &err, &classified);
            assert_eq!(got, None, "code {code}");
        }
    }

    #[test]
    fn openai_country_region_rejection_falls_back_to_the_code_token() {
        // A geo/region block carries no `/error/param` yet still names a
        // correct target-level route-away: the closed-set fallback keys on
        // the code token itself.
        let code = "unsupported_country_region_territory";
        let err = upstream(400, "{}", Some("invalid_request_error"), Some(code));
        let classified = classify(&err, Some("openai-compat"));
        let got = resolve_requested_capability("openai-compat", &err, &classified);
        assert_eq!(
            got,
            Some((
                code.to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn feature_unsupported_capability_is_normalized_for_bedrock() {
        // Non-openai providers carry a field path in the class token; the
        // resolver normalizes it directly (a Bedrock request-bag field path
        // reduces to the bag field). No `/error/param` read for this family.
        let class = FailureClass::FeatureUnsupported {
            capability: "additionalModelRequestFields.anthropic_beta".to_string(),
        };

        // Act
        let got =
            resolve_requested_capability("bedrock", &upstream(400, "{}", None, None), &cf(class));

        // Assert
        assert_eq!(
            got,
            Some((
                "anthropic_beta".to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn feature_unsupported_takes_precedence_and_ignores_body() {
        // A FeatureUnsupported class on a non-openai provider wins arm 1
        // regardless of body: the inferred body-parse never runs.
        let class = FailureClass::FeatureUnsupported {
            capability: "web_search".to_string(),
        };

        // Act -- body matches the inferred table, but arm 1 short-circuits.
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, PREFILL_BODY, None, None),
            &cf(class),
        );

        // Assert
        assert_eq!(
            got,
            Some((
                "web_search".to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F1
            ))
        );
    }

    // --- Arm 2: openai-responses inferred whole-phrase ---

    /// The verbatim OpenAI Responses 400 body a content-prefix-validating
    /// lane returns for a foreign replay artifact. Inline (the captured
    /// fixture corpus never ships) and byte-exact: it is the regression pin
    /// for the inferred row.
    const REPLAY_REJECTION_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

    fn replay_rejection() -> Error {
        upstream(
            400,
            REPLAY_REJECTION_BODY,
            Some("invalid_request_error"),
            Some("validation_error"),
        )
    }

    #[test]
    fn openai_responses_replay_rejection_maps_to_reasoning_replay() {
        // A lane whose replay validator rejects a foreign artifact names
        // the REPLAY capability, not the ability to reason.
        let err = replay_rejection();
        let classified = classify(&err, Some("openai-responses"));

        // Sanity: neither token is in a lift set, so the rejection arrives
        // as a plain BadRequest -- which is what routes it to this arm.
        assert_eq!(classified.class, FailureClass::BadRequest);

        // Act
        let got = resolve_requested_capability("openai-responses", &err, &classified);

        // Assert -- resolves to the replay key, never to `thinking`.
        assert_eq!(
            got,
            Some((
                "reasoning_replay".to_string(),
                SignalTier::Inferred,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn openai_responses_inferred_phrase_is_scoped_to_its_provider_kind() {
        // The same body from another provider must not resolve: the
        // inferred tables are keyed by provider kind.
        let err = replay_rejection();
        let classified = classify(&err, Some("anthropic-api"));

        // Act / Assert
        assert_eq!(
            resolve_requested_capability("anthropic-api", &err, &classified),
            None
        );
    }

    #[test]
    fn openai_responses_near_miss_phrase_does_not_match() {
        // Whole-phrase equality only: a truncated, embedded, or reworded
        // message fails closed rather than learning off a partial match.
        for message in [
            "encrypted content missing recognized prefix",
            "400: encrypted content missing recognized prefix (expected `rsn_` or `smry_`).",
            "encrypted content missing prefix (expected `rsn_` or `smry_`)",
        ] {
            let err = upstream(
                400,
                &anthropic_body(message),
                Some("invalid_request_error"),
                None,
            );
            let classified = classify(&err, Some("openai-responses"));
            assert_eq!(
                resolve_requested_capability("openai-responses", &err, &classified),
                None,
                "message {message}"
            );
        }
    }

    #[test]
    fn a_generic_openai_responses_bad_request_is_not_a_replay_rejection() {
        // An ordinary 400 on this provider must not resolve to the replay
        // key -- the row is one exact phrase, not a family of 400s.
        let err = upstream(
            400,
            &anthropic_body("Invalid value for 'temperature'."),
            Some("invalid_request_error"),
            None,
        );
        let classified = classify(&err, Some("openai-responses"));

        // Act / Assert
        assert_eq!(
            resolve_requested_capability("openai-responses", &err, &classified),
            None
        );
    }

    // --- Arm 2: Anthropic inferred whole-phrase ---

    #[test]
    fn anthropic_prefill_phrase_maps_to_prefill_inferred() {
        // Source: Anthropic Messages API errors doc, "Prefill not
        // supported" (platform.claude.com/docs/en/api/errors). The 400 body
        // carries the phrase in free-text error.message.
        let err = upstream(400, PREFILL_BODY, Some("invalid_request_error"), None);
        let classified = classify(&err, Some("anthropic-api"));

        // Sanity: a generic invalid_request_error stays BadRequest.
        assert_eq!(classified.class, FailureClass::BadRequest);

        // Act
        let got = resolve_requested_capability("anthropic-api", &err, &classified);

        // Assert
        assert_eq!(
            got,
            Some((
                "prefill".to_string(),
                SignalTier::Inferred,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn anthropic_prefill_phrase_match_is_case_insensitive() {
        // Arrange
        let body = anthropic_body("PREFILLING ASSISTANT MESSAGES IS NOT SUPPORTED FOR THIS MODEL.");

        // Act
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(
            got,
            Some((
                "prefill".to_string(),
                SignalTier::Inferred,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn anthropic_prefill_phrase_matches_ignoring_surrounding_whitespace() {
        // Arrange
        let body =
            anthropic_body("  Prefilling assistant messages is not supported for this model.  ");

        // Act
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(
            got,
            Some((
                "prefill".to_string(),
                SignalTier::Inferred,
                FailurePhase::F1
            ))
        );
    }

    #[test]
    fn near_miss_anthropic_phrase_does_not_match() {
        // A truncated variant is not the verified whole phrase -- no fuzzy
        // contains, so it must not learn.
        let body = anthropic_body("Prefilling assistant messages is not supported.");

        // Act
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(got, None);
    }

    #[test]
    fn phrase_embedded_in_larger_message_does_not_match() {
        // Whole-phrase means the message IS the phrase; a phrase buried in
        // a larger message is a different, unverified shape -> no match.
        let body = anthropic_body(
            "Error: Prefilling assistant messages is not supported for this model. Please adjust.",
        );

        // Act
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );

        // Assert
        assert_eq!(got, None);
    }

    // --- Arm 2: malformed / missing bodies ---

    #[test]
    fn garbage_body_yields_none() {
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, "not json at all {{{", None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn empty_body_yields_none() {
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, "", None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn body_without_error_message_field_yields_none() {
        for body in [
            r#"{"foo":"bar"}"#,
            r#"{"error":{"type":"invalid_request_error"}}"#,
            r#"{"error":"just a string, not an object"}"#,
        ] {
            let got = resolve_requested_capability(
                "anthropic-api",
                &upstream(400, body, None, None),
                &cf(FailureClass::BadRequest),
            );
            assert_eq!(got, None, "body {body}");
        }
    }

    #[test]
    fn non_string_error_message_yields_none() {
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, r#"{"error":{"message":42}}"#, None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn oversized_body_yields_none() {
        // A body over the parse ceiling is never JSON-parsed, so even a body
        // that would otherwise match the inferred phrase yields None: a
        // malicious upstream cannot force a large-JSON parse on this path.
        let phrase = "Prefilling assistant messages is not supported for this model.";
        let padding = "x".repeat(64 * 1024 + 1);
        let body = anthropic_body(&format!("{padding}{phrase}"));
        assert!(body.len() > 64 * 1024, "sanity: body exceeds the cap");

        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    // --- Provider + class gating ---

    #[test]
    fn matching_phrase_on_non_anthropic_kind_yields_none() {
        // The inferred table is keyed by provider_kind; only anthropic-api
        // has one in this slice.
        for kind in ["openai-compat", "bedrock", "gemini", "future-vendor"] {
            let got = resolve_requested_capability(
                kind,
                &upstream(400, PREFILL_BODY, None, None),
                &cf(FailureClass::BadRequest),
            );
            assert_eq!(got, None, "kind {kind}");
        }
    }

    #[test]
    fn matching_phrase_but_wrong_class_yields_none() {
        // Arm 2 only fires on BadRequest; any other class never body-parses.
        for class in [
            FailureClass::RateLimited,
            FailureClass::Auth,
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::ServerError,
            FailureClass::NetworkError,
            FailureClass::Overloaded,
            FailureClass::Timeout,
            FailureClass::Unknown,
        ] {
            let got = resolve_requested_capability(
                "anthropic-api",
                &upstream(400, PREFILL_BODY, None, None),
                &cf(class.clone()),
            );
            assert_eq!(got, None, "class {class:?}");
        }
    }

    #[test]
    fn non_upstream_error_with_bad_request_class_yields_none() {
        // Only Error::Upstream carries a body to parse.
        let got = resolve_requested_capability(
            "anthropic-api",
            &Error::Streaming("connection reset".into()),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }

    // --- upstream_param log-boundary sanitization ---

    #[test]
    fn upstream_param_emits_a_token_shaped_value() {
        let err = upstream(
            400,
            &openai_unsupported_body("unsupported_parameter", "web_search"),
            None,
            None,
        );
        assert_eq!(upstream_param(&err).as_deref(), Some("web_search"));
    }

    #[test]
    fn upstream_param_drops_oversized_value() {
        let param = "a".repeat(65);
        let err = upstream(
            400,
            &openai_unsupported_body("unsupported_parameter", &param),
            None,
            None,
        );
        assert_eq!(upstream_param(&err), None);
    }

    #[test]
    fn upstream_param_drops_control_and_whitespace_values() {
        for param in ["web search", "web_search\n", "\tinject", "line\r\nbreak"] {
            let err = upstream(
                400,
                &openai_unsupported_body("unsupported_parameter", param),
                None,
                None,
            );
            assert_eq!(
                upstream_param(&err),
                None,
                "param {param:?} must be dropped"
            );
        }
    }

    // --- Bedrock validation arm: flat reader + anchored-template engine ---

    /// Real captured bedrock-runtime InvokeModel 400 `ValidationException`
    /// envelopes: byte-exact bodies + the header-lifted discriminator. The
    /// production template + translation tables are grounded in these; the
    /// fixture drives them (and their near-miss variants) end-to-end.
    const BEDROCK_CAPTURE_FIXTURE: &str =
        include_str!("../tests/fixtures/bedrock_validation_capture.json");

    fn capture_fixture() -> serde_json::Value {
        serde_json::from_str(BEDROCK_CAPTURE_FIXTURE).expect("valid capture fixture json")
    }

    fn fixture_pairs<'a>(
        fixture: &'a serde_json::Value,
        array: &str,
        a: &str,
        b: &str,
    ) -> Vec<(&'a str, &'a str)> {
        fixture[array]
            .as_array()
            .expect("fixture array")
            .iter()
            .map(|row| {
                (
                    row[a].as_str().expect("fixture field a"),
                    row[b].as_str().expect("fixture field b"),
                )
            })
            .collect()
    }

    /// A flat AWS Bedrock `ValidationException` envelope carrying `message`.
    fn flat_validation_body(message: &str) -> String {
        serde_json::json!({ "__type": "ValidationException", "message": message }).to_string()
    }

    /// The lifted, namespace-stripped discriminator a native-lane
    /// `ValidationException` lands on `Error::Upstream.upstream_type`.
    fn lifted_type(fx: &serde_json::Value) -> String {
        fx["lifted_type"]
            .as_str()
            .expect("fixture lifted_type")
            .to_string()
    }

    /// Each captured canary, driven through the full resolver with a VALID
    /// lifted discriminator, resolves to its expected outcome: the
    /// advisor-tool rejection to the `advisor` capability at SelfIdentifying;
    /// the two must-not-learn controls (unknown beta flag, bad model id --
    /// same flat shape, same valid header) and the untranslated field-name
    /// rejection all to `None` (no-learn).
    #[test]
    fn bedrock_capture_canaries_resolve_to_expected_capability() {
        let fx = capture_fixture();
        let ty = lifted_type(&fx);
        let canaries = fx["canaries"].as_object().expect("canaries object");
        for (name, canary) in canaries {
            let body = canary["body"].as_str().expect("canary body");
            let err = upstream(400, body, Some(&ty), None);
            let got = resolve_requested_capability("bedrock", &err, &cf(FailureClass::BadRequest));
            match canary["expect_capability"].as_str() {
                Some(expected) => assert_eq!(
                    got,
                    Some((
                        expected.to_string(),
                        SignalTier::SelfIdentifying,
                        FailurePhase::F1
                    )),
                    "canary {name} must learn {expected}"
                ),
                None => assert_eq!(got, None, "canary {name} must not learn"),
            }
        }
    }

    /// The learnable advisor rejection carrying NO discriminator (absent or
    /// a wrong `upstream_type`) yields `None`: the arm gates on the lifted
    /// `ValidationException` before the message is ever read, so a stripped
    /// header cannot silently false-learn on the shared flat shape.
    #[test]
    fn bedrock_arm_requires_lifted_discriminator() {
        let fx = capture_fixture();
        let body = fx["canaries"]["advisor-tool"]["body"]
            .as_str()
            .expect("advisor body");
        for wrong_type in [None, Some("ThrottlingException")] {
            let err = upstream(400, body, wrong_type, None);
            assert_eq!(
                resolve_requested_capability("bedrock", &err, &cf(FailureClass::BadRequest)),
                None,
                "no lifted ValidationException -> no learn (type {wrong_type:?})"
            );
        }
    }

    /// Near-miss / drifted variants of each real template fail closed: a
    /// changed verb, a trailing sentence, a missing seam, and a
    /// whitespace-bearing (unsafe) extracted token all yield `None`.
    #[test]
    fn bedrock_near_miss_variants_yield_none() {
        let fx = capture_fixture();
        let ty = lifted_type(&fx);
        let near = fx["near_miss_messages"]
            .as_object()
            .expect("near_miss_messages object");
        for (name, message) in near {
            let body = flat_validation_body(message.as_str().expect("near-miss message"));
            let err = upstream(400, &body, Some(&ty), None);
            assert_eq!(
                resolve_requested_capability("bedrock", &err, &cf(FailureClass::BadRequest)),
                None,
                "near-miss {name} must not learn"
            );
        }
    }

    /// The templates + translations, driven directly, extract and translate
    /// the advisor token onto the canonical capability at SelfIdentifying;
    /// the field-name token extracts through the same engine but has no
    /// closed-set row, so it stays dormant.
    #[test]
    fn bedrock_engine_translates_advisor_and_leaves_field_dormant() {
        let fx = capture_fixture();
        let templates = fixture_pairs(&fx, "templates", "prefix", "suffix");
        let translations = fixture_pairs(&fx, "translations", "token", "capability");

        let advisor = "tool type 'advisor' is not supported for this model";
        assert_eq!(
            extract_bedrock_capability(advisor, &templates, &translations),
            Some((
                "advisor".to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F1
            ))
        );

        let field = "routectl_envelope_probe_field: Extra inputs are not permitted";
        assert_eq!(
            extract_bedrock_capability(field, &templates, &translations),
            None,
            "an extracted field name with no closed-set row stays dormant"
        );
    }

    #[test]
    fn bedrock_engine_fails_closed_on_oversized_token() {
        // A structurally-anchored match whose extracted token exceeds the
        // token-shape length cap fails closed rather than surfacing a blob.
        let fx = capture_fixture();
        let templates = fixture_pairs(&fx, "templates", "prefix", "suffix");
        let translations = fixture_pairs(&fx, "translations", "token", "capability");
        let (prefix, suffix) = templates[0];
        let oversized = format!("{prefix}{}{suffix}", "a".repeat(MAX_PARAM_TOKEN_LEN + 1));

        assert_eq!(
            extract_bedrock_capability(&oversized, &templates, &translations),
            None
        );
    }

    #[test]
    fn bedrock_flat_reader_reads_message_and_predicate_uses_lifted_type() {
        // The flat reader pulls `message` from the body; the
        // validation-exception predicate reads the lifted, namespace-stripped
        // `upstream_type` (what the native lane lands from the namespaced wire
        // form), not the raw body.
        let body = flat_validation_body("some rejection");
        let err = upstream(400, &body, Some(BEDROCK_VALIDATION_EXCEPTION_TYPE), None);
        assert_eq!(
            bedrock_validation_message(&err).as_deref(),
            Some("some rejection")
        );
        assert!(is_bedrock_validation_exception(&err));
    }

    #[test]
    fn bedrock_validation_predicate_matches_lifted_token_not_raw_body() {
        // The namespaced wire form in the body is NOT matched by the
        // predicate: only the lifted, stripped `upstream_type` fires it. This
        // is the drift fix -- a raw-body match on the bare name was dead on
        // real namespaced production input.
        let namespaced =
            r#"{"__type":"com.amazon.coral.validate#ValidationException","message":"x"}"#;
        assert!(
            !is_bedrock_validation_exception(&upstream(400, namespaced, None, None)),
            "no lifted type -> predicate stays false even though the body names it"
        );
        assert!(
            is_bedrock_validation_exception(&upstream(
                400,
                namespaced,
                Some("ValidationException"),
                None
            )),
            "the native-lane lift of the stripped token fires the predicate"
        );
    }

    #[test]
    fn bedrock_flat_reader_returns_none_on_nested_envelope() {
        // The nested `/error/message` shape (anthropic/openai) is NOT the
        // flat AWS envelope: the top-level reader finds no `message` /
        // `__type`, and the full resolver's bedrock arm yields None.
        let body = r#"{"error":{"type":"invalid_request_error","message":"nested"}}"#;
        let err = upstream(400, body, None, None);
        assert_eq!(bedrock_validation_message(&err), None);
        assert!(!is_bedrock_validation_exception(&err));
        assert_eq!(
            resolve_requested_capability("bedrock", &err, &cf(FailureClass::BadRequest)),
            None
        );
    }

    #[test]
    fn bedrock_over_cap_body_is_not_parsed() {
        // A flat envelope over the parse ceiling is never JSON-parsed by the
        // message reader.
        let padding = "x".repeat(MAX_ERROR_BODY_BYTES + 1);
        let body = flat_validation_body(&padding);
        assert!(
            body.len() > MAX_ERROR_BODY_BYTES,
            "sanity: body exceeds cap"
        );
        let err = upstream(400, &body, None, None);
        assert_eq!(bedrock_validation_message(&err), None);
    }

    #[test]
    fn bedrock_flat_reader_reads_message_over_log_excerpt() {
        // A validation message longer than the log excerpt cap but within the
        // matcher ceiling is read intact: the flat reader is bounded by
        // MAX_ERROR_BODY_BYTES, not the shorter log excerpt the producer once
        // capped the body at, so a verbose real envelope still reaches the
        // matcher.
        let long_message = "reject_".repeat(200);
        assert!(
            long_message.len() > routectl_core::MAX_LOG_BODY_EXCERPT,
            "sanity: message exceeds the log excerpt cap"
        );
        let body = flat_validation_body(&long_message);
        assert!(
            body.len() <= MAX_ERROR_BODY_BYTES,
            "sanity: body is within the parse ceiling"
        );
        let err = upstream(400, &body, Some(BEDROCK_VALIDATION_EXCEPTION_TYPE), None);
        assert_eq!(
            bedrock_validation_message(&err).as_deref(),
            Some(long_message.as_str())
        );
    }

    #[test]
    fn bedrock_non_upstream_error_yields_none() {
        // Only Error::Upstream carries a body to parse.
        let err = Error::Streaming("connection reset".into());
        assert_eq!(bedrock_validation_message(&err), None);
        assert!(!is_bedrock_validation_exception(&err));
        assert_eq!(
            resolve_requested_capability("bedrock", &err, &cf(FailureClass::BadRequest)),
            None
        );
    }

    #[test]
    fn flat_validation_on_non_bedrock_kind_yields_none() {
        // The flat-envelope + anchored-template path is bedrock-only; a flat
        // ValidationException on another kind takes that kind's arm (the
        // inferred whole-phrase table, which has no such phrase) -> None.
        let body =
            flat_validation_body("The parameter response_schema is not supported for this model.");
        for kind in ["openai-compat", "anthropic-api", "gemini"] {
            assert_eq!(
                resolve_requested_capability(
                    kind,
                    &upstream(400, &body, None, None),
                    &cf(FailureClass::BadRequest),
                ),
                None,
                "kind {kind}"
            );
        }
    }

    // --- Arm 3: F2 feature-naming anchored-template engine ---

    /// Provisional synthetic shapes for the F2 feature-naming engine. The
    /// PRODUCTION tables ship empty; these drive the engine in isolation and
    /// are replaced by sanitized captured envelopes once real capture lands.
    const FEATURE_NAMING_PROVISIONAL_FIXTURE: &str =
        include_str!("../tests/fixtures/feature_naming_provisional.json");

    fn feature_naming_fixture() -> serde_json::Value {
        serde_json::from_str(FEATURE_NAMING_PROVISIONAL_FIXTURE)
            .expect("valid feature-naming provisional fixture json")
    }

    #[test]
    fn feature_naming_provisional_template_matches_and_translates() {
        // The engine, driven with a provisional template + translation,
        // extracts the single anchored token, normalizes it, and maps it
        // through the closed set to a canonical capability at SelfIdentifying
        // tier and the F2 phase.
        let fx = feature_naming_fixture();
        let templates = fixture_pairs(&fx, "templates", "prefix", "suffix");
        let translations = fixture_pairs(&fx, "translations", "token", "capability");
        let message = fx["matched"]["message"].as_str().expect("matched message");
        let expected = fx["matched"]["capability"]
            .as_str()
            .expect("matched capability");

        let got =
            extract_feature_naming_capability(message, "anthropic-api", &templates, &translations);

        assert_eq!(
            got,
            Some((
                expected.to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F2
            ))
        );
    }

    #[test]
    fn feature_naming_engine_fails_closed_on_drift_and_untranslated_tokens() {
        // Wording drift, an extra sentence, a missing anchor, an untranslated
        // (closed-set-miss) token, and a whitespace-bearing (unsafe) token all
        // yield None -- no fuzzy match, no verdict.
        let fx = feature_naming_fixture();
        let templates = fixture_pairs(&fx, "templates", "prefix", "suffix");
        let translations = fixture_pairs(&fx, "translations", "token", "capability");
        for key in [
            "wording_drift",
            "extra_sentence",
            "missing_seam",
            "untranslated_token",
            "whitespace_token",
        ] {
            let message = fx["fail_closed_messages"][key]
                .as_str()
                .expect("fail-closed message");
            assert_eq!(
                extract_feature_naming_capability(
                    message,
                    "anthropic-api",
                    &templates,
                    &translations
                ),
                None,
                "case {key}"
            );
        }
    }

    #[test]
    fn feature_naming_engine_fails_closed_on_oversized_token() {
        // A structurally-anchored match whose extracted token exceeds the
        // token-shape length cap fails closed rather than surfacing a blob.
        let fx = feature_naming_fixture();
        let templates = fixture_pairs(&fx, "templates", "prefix", "suffix");
        let translations = fixture_pairs(&fx, "translations", "token", "capability");
        let (prefix, suffix) = templates[0];
        let oversized = format!("{prefix}{}{suffix}", "a".repeat(MAX_PARAM_TOKEN_LEN + 1));

        assert_eq!(
            extract_feature_naming_capability(
                &oversized,
                "anthropic-api",
                &templates,
                &translations
            ),
            None
        );
    }

    #[test]
    fn feature_naming_empty_production_tables_yield_none() {
        // The shipped-empty feature-naming template + translation tables mean
        // every provider BadRequest -- even a well-formed message whose wording
        // a provisional template would match -- yields None through the full
        // resolver, so F2 never fires on real traffic.
        let fx = feature_naming_fixture();
        let message = fx["matched"]["message"].as_str().expect("matched message");
        let body = anthropic_body(message);
        let got = resolve_requested_capability(
            "anthropic-api",
            &upstream(400, &body, None, None),
            &cf(FailureClass::BadRequest),
        );
        assert_eq!(got, None);
    }
}
