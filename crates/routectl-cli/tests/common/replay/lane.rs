//! Lane classification and the wire-conservation exception table.
//!
//! Answers the two questions a conservation check asks about a captured
//! fixture: what CLASS is the lane it rode, and which of the divergences
//! between its `ingress_request.json` and its `outgoing_request.json` are
//! EXPECTED routectl transforms rather than wire loss.
//!
//! # Class is derived, never looked up
//!
//! `class = FIDELITY` iff the ingress dialect equals the egress dialect,
//! else `TRANSLATION`. [`lane_class`] computes it from [`Dialect`]
//! equality. The only committed data is the egress-kind -> dialect map
//! ([`EgressLane::dialect`]), six rows; no class column exists anywhere in
//! this module, because a class table is a snapshot of the derivation and
//! rots the moment a dialect moves.
//!
//! # Diff orientation (load-bearing)
//!
//! The exception predicates below are written against ONE orientation:
//!
//! ```text
//! diff_all(actual = the OUTGOING body, expected = the INGRESS body, ..)
//! ```
//!
//! With the wire on the `actual` side, [`DivergenceKind`] reads
//! egress-relative and matches how a transform is described in prose:
//! [`DivergenceKind::Added`] means routectl ADDED the field on the way
//! out, [`DivergenceKind::Removed`] means routectl DROPPED it, and
//! [`DivergenceKind::Changed`] means routectl rewrote its value. Swapping
//! the arguments inverts Added and Removed and silently un-matches every
//! matcher here, so a caller passes the ingress body as `expected`.
//!
//! # Two kinds of exception, and why they cannot be one
//!
//! - [`Transform::Normalizer`] -- changes an array's LENGTH. Applied to
//!   the ingress body BEFORE the diff. It can never be a per-index
//!   whitelist entry: array pairing in `diff_all` is positional, so a
//!   removal from the MIDDLE of `.messages` shifts every later element and
//!   one explained transform reports a divergence at nearly every index.
//!   An entry broad enough to absorb that matches essentially the whole
//!   message array, which is most of a captured body by bytes -- a mute
//!   button, not an exception. A normalizer therefore NEVER excuses a
//!   divergence ([`Exception::matches`] is always false for one); it
//!   removes the divergence by realigning the inputs.
//! - [`Transform::Matcher`] -- changes a value in place and moves no
//!   positions. Stays a post-hoc predicate over the returned divergence
//!   set.
//!
//! Rule of thumb: length changes NORMALIZE, in-place value changes MATCH.
//!
//! # What a matcher cannot see, and the two hooks that cover it
//!
//! [`Transform::Matcher`] receives ONE [`Divergence`] and nothing else, so
//! two classes of constraint are unexpressible in it and live on the
//! [`Exception`] instead:
//!
//! - [`Exception::applies_to`] -- a per-FIXTURE eligibility gate, for a
//!   transform gated on something outside the body. `normalize_claude_sampling`
//!   runs only for routectl's own OAuth-bearer credential against
//!   `api.anthropic.com`; keyed on the lane alone, its entry excused a
//!   dropped `temperature` on every fixture of the lane, api-key captures
//!   included. `None` -- the state of every ungated entry -- means every
//!   fixture on the lane.
//! - [`Exception::max_per_fixture`] -- a per-fixture CARDINALITY bound, for
//!   a transform that writes a bounded number of identical shapes. Auto-cache
//!   placement emits at most two markers per request; a matcher cannot
//!   count, so the bound is enforced by [`unexplained_for_fixture`] and a
//!   third same-shaped addition stays unexplained.
//!
//! Both are `Option`, both default to the unconstrained behavior, and a
//! fixture-gated entry is consulted only through
//! [`unexplained_for_fixture`] -- [`unexplained`] is the weaker
//! fixture-less form and treats every entry as eligible.
//!
//! # What a NORMALIZER cannot see, and where its gate lives instead
//!
//! Neither hook above serves a normalizer: its seam takes a BODY rather
//! than a fixture, so an [`Exception::applies_to`] declared on one would be
//! silently ignored (`only_matchers_carry_a_fixture_gate` pins that).
//!
//! `system-turn-lift` still needs a gate, because whether the production
//! lift ran is a property of the PAIR and not of either body: under the
//! Forward system-turn policy the `role:"system"` turns ride the wire IN
//! PLACE, and removing them from the ingress side then MANUFACTURES a
//! misalignment the wire never carried -- one divergence per surviving
//! index, on exactly the fixtures where nothing was lifted. So the gate
//! lives at the CALL SITE: [`normalize_ingress_for_pair`] reads both
//! bodies' system-turn counts through [`system_turns_were_lifted`] and
//! consults the entry only when the pair evidences a lift.
//!
//! PARTIAL loss is deliberately not a lift. The production lift takes
//! every `Role::System` turn or none, so turns that are partly gone went
//! to some other transform or to real wire loss and must stay a finding.
//!
//! # Path-string ambiguity: the decision taken here
//!
//! `diff_all` joins object keys with `.`, so a literal key containing a
//! dot is indistinguishable from nesting: diffing `{"a.b": 1}` against
//! `{"a": {"b": 1}}` yields the colliding paths `["a", "a.b"]`. Tool
//! definitions pass `input_schema` through verbatim and JSON Schema
//! `properties` keys are caller-chosen strings where a dot is legal, so a
//! caller could in principle forge a path a predicate matches.
//!
//! DECISION: keep the joined-string form, and constrain every matcher on
//! VALUES as well as on path. Matching on structure was considered and is
//! not reachable from here -- the ambiguity is created when `diff_all`
//! joins the segments, and a segment vector recovered by re-splitting the
//! joined string cannot tell the two shapes apart either. Widening the
//! comparator's return type is out of this module's scope.
//!
//! The residual risk is therefore accepted with two mitigations stated
//! explicitly:
//!
//! 1. A path predicate alone must NOT be trusted against a
//!    caller-controlled key subtree (tool `input_schema` is the known
//!    one). Every entry in this table pairs its path predicate with a
//!    constraint on the divergence itself, so forging the path is not
//!    enough to be excused -- the forged key would also have to carry the
//!    exact shape the transform produces. `oauth-sampling-stripped` is the
//!    one entry whose constraint is on the KIND rather than on a value,
//!    and it is sound for the same reason: its two paths are exact
//!    top-level equalities that no nested key can reach, so there is no
//!    caller-controlled subtree to forge from. It additionally carries a
//!    per-fixture credential gate ([`Exception::applies_to`]), so the weak
//!    value constraint is not the only thing standing between it and a
//!    fixture the transform never ran on.
//! 2. No entry here targets a path inside a caller-controlled subtree. The
//!    live entries address `messages`, `system`, `temperature`, `top_p`,
//!    `model`, `thinking`, and the `cache_control` marker slots, all
//!    routectl-owned. The cache-marker entry's path predicate is written to
//!    EXCLUDE the one caller-controlled subtree that can carry a
//!    `cache_control`-shaped key (a tool's `input_schema`).
//!
//! Also inherited: `ignore_paths` cannot express wildcards
//! (`messages[*].role`), which is intended -- this table filters the
//! RETURNED divergence set rather than pruning the walk. And `diff_all`
//! orders results by traversal (lexicographic per level, depth-first), so
//! a test asserting an exact path vector orders by traversal, not by
//! `sort()`.
//!
//! # What is deliberately NOT in the table
//!
//! - **The structural-validation register (the FIDELITY-lane local-400
//!   set).** Those requests are rejected before egress, so they produce no
//!   ingress/outgoing pair at all and are structurally invisible to a
//!   conservation diff. Encoding them would be dead config, and the
//!   zero-match rule below would then correctly fail the run on it. A
//!   local 400 firing on a case the driver expected to succeed is the
//!   driver's own exit-code check, not the diff's.
//! - **The `cch` re-sign.** `resign_cch_in_place`
//!   (`crates/routectl-providers/src/claude_signing.rs`) rewrites five
//!   lowercase hex chars of one `cch=` token inside the `system` billing
//!   block AFTER the outgoing-body trace, so it separates the captured
//!   outgoing body from the true transmitted bytes -- not the ingress body
//!   from the outgoing one. It produces no ingress-vs-outgoing divergence
//!   and so has no entry here; it is a stated limit on what a conservation
//!   diff over this corpus proves, owned by the harness that makes the
//!   claim. The transform is length-preserving and a silent no-op when the
//!   token is absent.
//!
//! # Credential-lane scope of the table
//!
//! The `anthropic` -> `anthropic-api` lane carries captures from TWO
//! credential surfaces, and the table's entries were measured on both.
//! An entry whose transform fires on only ONE of them carries an
//! [`Exception::applies_to`] gate keyed on the captured outgoing
//! credential; an entry whose transform is credential-independent does not.
//!
//! The live-box corpus is the NON-cloak surface, and that scope is measured
//! rather than assumed: the Claude Code billing text survives in the
//! outgoing `system` of the overwhelming majority of those fixtures, and
//! `cloak_body` (`crates/routectl-providers/src/anthropic_api/client.rs`)
//! strips it on the OAuth own-anthropic lane -- so the cloak gate was false
//! for those captures. Corroborating: no such fixture carries `temperature`
//! on ingress while many have it ADDED, which is only possible if
//! `normalize_claude_sampling` never ran.
//!
//! The committed driver corpus is the OAuth cloak surface (its outgoing
//! headers carry an `authorization: Bearer`, its ingress an `x-api-key`).
//! On that surface two further transforms fire, and both have entries
//! below:
//!
//! - The billing strip DOES fire, from both the always-on normalize path
//!   and the cloak -- see `billing-system-block-stripped`. Because the
//!   normalize-path call is unconditional on the credential, that entry
//!   stays UNGATED: it fires on an api-key egress too.
//! - `normalize_claude_sampling`
//!   (`crates/routectl-providers/src/anthropic_api/extras.rs`) strips BOTH
//!   `temperature` and `top_p` from the final body, because the OAuth seat
//!   400s a request carrying either -- see `oauth-sampling-stripped`, which
//!   is gated to Bearer-credential captures for exactly that reason. On
//!   this surface the two sampling entries COMPOSE rather than conflict:
//!   `thinking-temperature-clamp` writes `1.0` during request assembly and
//!   this strip removes the key at the egress boundary, so a capture here
//!   reports a REMOVED `temperature` while a non-cloak capture reports an
//!   ADDED one. Two directions, two sites, two entries.
//!
//! The remaining cloak sub-transforms are named here so the next
//! contributor re-derives rather than rediscovers:
//!
//! - `cloak_oauth_egress`
//!   (`crates/routectl-providers/src/anthropic_api/cloak.rs`) and its
//!   sub-transforms. Two of them move positions and are therefore
//!   NORMALIZER-shaped by this module's own rule:
//!   `relocate_client_system` (`cloak/identity.rs`) moves the client
//!   system into the first user message, and `sort_custom_tools_by_name`
//!   (`cloak/tool_sort.rs`) reorders `tools[]`. Both are gated on a
//!   NON-Claude-Code client, so a genuine-client capture does not reach
//!   them.
//!
//! Those entries are deliberately NOT written yet: they would match zero
//! divergences on today's corpus, and the zero-match rule would correctly
//! fail them. They arrive with the capture that exercises them.
//!
//! # Fail closed
//!
//! An unrecognized ingress or egress token is an error, never a default
//! arm ([`LaneError`]). The egress map is an exhaustive match over
//! [`EgressLane`], so a recognized kind with no dialect row cannot compile.
//!
//! A fixture's `meta.provider_kind` is a THIRD vocabulary and does not
//! resolve through [`resolve_egress`] -- see
//! [`egress_lane_from_fixture_kind`], which owns that translation so a
//! consumer never hand-rolls a fourth spelling.
//!
//! # Per-exception match counter
//!
//! Each entry counts its own hits ([`Exception::matched_count`]). An
//! exception that matched zero divergences on a populated lane is an
//! untested claim, and a too-broad matcher is how a whitelist silently
//! becomes a mute button; the counter is what lets a caller fail the run
//! on either.
//!
//! CONSTRAINT ON THE CONSUMER: the counters live on process-global
//! statics, so every test and every corpus walk in one test binary shares
//! them and they only ever increase. A zero-match gate MUST therefore
//! snapshot [`Exception::matched_count`] before its own walk and assert on
//! the DELTA. Reading the global directly reports hits some other walk
//! contributed and passes a gate that should have failed.
//!
//! And every writer holds [`COUNTER_DELTA_LOCK`] -- not only the callers of
//! [`Exception::matches`]. [`Exception::normalize`] increments the same
//! counters, so a test that merely calls [`normalize_ingress_for_lane`]
//! races a concurrent delta reader and shows up as an unrelated test
//! failing intermittently.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use super::loader::Fixture;
use super::{Divergence, DivergenceKind};

// ---------------------------------------------------------------------
// Dialects and lane class
// ---------------------------------------------------------------------

/// A client-facing or upstream-facing wire dialect. Equality of the two
/// ends is the whole definition of [`LaneClass::Fidelity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Anthropic Messages JSON.
    AnthropicMessages,
    /// OpenAI Chat Completions JSON.
    OpenaiChat,
    /// OpenAI Responses JSON.
    OpenaiResponses,
    /// AWS Bedrock's vendor-neutral Converse envelope.
    BedrockConverse,
    /// Google Gemini generateContent JSON.
    Gemini,
}

/// Whether a lane translates dialects or preserves one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneClass {
    /// Ingress and egress speak the same dialect.
    Fidelity,
    /// The lane crosses a dialect boundary.
    Translation,
}

/// The ingress tokens that own a client-facing dialect, as
/// `IngressAdapter::id()` emits them
/// (`crates/routectl-cli/src/ingress/`). Shared helpers in that directory
/// (`mod.rs`, `session_key.rs`, `token_estimate.rs`) own no dialect and are
/// absent by construction.
pub const INGRESS_IDS: &[&str] = &["anthropic", "openai", "openai-responses"];

/// The provider-kind tokens `ProviderEntry::kind_str()` emits
/// (`crates/routectl-router/src/config/schema.rs`). `bedrock` carries no
/// shape in the token, which is why [`resolve_egress`] takes `api_shape`
/// alongside it.
///
/// Derived from `kind_str` specifically, and NOT from
/// `CacheCapability::for_provider_kind` in the same file: that match names
/// only a subset and serves the rest through a catch-all, so it cannot
/// enumerate the kinds.
pub const EGRESS_KINDS: &[&str] = &[
    "openai-compat",
    "anthropic-api",
    "bedrock",
    "openai-responses",
    "gemini",
];

/// The `api_shape` tokens `BedrockApiShape::provider_kind_str()` derives
/// its lane labels from
/// (`crates/routectl-providers/src/bedrock/mod.rs`).
pub const BEDROCK_API_SHAPES: &[&str] = &["invoke", "converse"];

/// One egress lane: the `kind_str()` enumeration with `bedrock` resolved
/// through its `api_shape`. Six variants, and [`EgressLane::dialect`] is
/// the six-row map -- the only committed lane data in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressLane {
    /// `openai-compat`.
    OpenaiCompat,
    /// `anthropic-api`.
    AnthropicApi,
    /// `bedrock` with `api_shape = "invoke"`.
    BedrockInvoke,
    /// `bedrock` with `api_shape = "converse"`.
    BedrockConverse,
    /// `openai-responses`.
    OpenaiResponses,
    /// `gemini`.
    Gemini,
}

impl EgressLane {
    /// Every lane, in `kind_str()` order with bedrock expanded. The
    /// enumeration a caller iterates instead of re-listing the variants.
    pub const ALL: [Self; 6] = [
        Self::OpenaiCompat,
        Self::AnthropicApi,
        Self::BedrockInvoke,
        Self::BedrockConverse,
        Self::OpenaiResponses,
        Self::Gemini,
    ];

    /// The lane's token. Matches `kind_str()` for the five unsplit kinds
    /// and `BedrockApiShape::provider_kind_str()` for the two bedrock
    /// shapes, so a lane label is greppable against either source.
    ///
    /// NOTE the vocabulary trap: this is the `kind_str()` spelling, in
    /// which the Anthropic Messages egress is `anthropic-api`. The
    /// in-crate `PROVIDER_KIND` constant in `routectl-providers` spells the
    /// same provider `anthropic`, and a fixture's `meta.provider_kind`
    /// retains THAT older spelling. Neither is wrong; they are different
    /// vocabularies and must not be reconciled.
    pub const fn token(self) -> &'static str {
        match self {
            Self::OpenaiCompat => "openai-compat",
            Self::AnthropicApi => "anthropic-api",
            Self::BedrockInvoke => "bedrock-invoke",
            Self::BedrockConverse => "bedrock-converse",
            Self::OpenaiResponses => "openai-responses",
            Self::Gemini => "gemini",
        }
    }

    /// THE egress-kind -> dialect map. Exhaustive by construction: a new
    /// [`EgressLane`] variant fails to compile until it declares a
    /// dialect, so no kind can fall through to a default.
    ///
    /// `BedrockInvoke` shares [`Dialect::AnthropicMessages`] with
    /// `AnthropicApi` rather than owning a dialect of its own, and that is
    /// the deliberate reading of the code: `bedrock::invoke::normalize_request`
    /// builds its body by calling
    /// `anthropic_api::request::normalize_deferring_format_key_warn` and
    /// then patching Bedrock-specific fields onto the result, and its
    /// response path calls `anthropic_api::response::normalize` directly.
    /// The Invoke shape is the vendor-native passthrough; `Converse` is the
    /// vendor-neutral envelope AWS translates internally, hence a distinct
    /// dialect. Additive Bedrock-only fields on the Invoke body are wire
    /// differences within one dialect, which is what the exception table --
    /// not the class -- exists to adjudicate.
    pub const fn dialect(self) -> Dialect {
        match self {
            Self::OpenaiCompat => Dialect::OpenaiChat,
            Self::AnthropicApi | Self::BedrockInvoke => Dialect::AnthropicMessages,
            Self::BedrockConverse => Dialect::BedrockConverse,
            Self::OpenaiResponses => Dialect::OpenaiResponses,
            Self::Gemini => Dialect::Gemini,
        }
    }
}

/// Why a lane could not be resolved. Every variant names the offending
/// value so a failure points at the fixture field that carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneError {
    /// The ingress token is not one of [`INGRESS_IDS`].
    UnknownIngress(String),
    /// The egress kind is not one of [`EGRESS_KINDS`].
    UnknownEgressKind(String),
    /// `bedrock` was named without the `api_shape` that splits it.
    MissingApiShape,
    /// `bedrock` named an `api_shape` outside [`BEDROCK_API_SHAPES`].
    UnknownApiShape(String),
    /// A kind that carries no shape was given one, which means the caller
    /// and this map disagree about the kind's identity.
    UnexpectedApiShape {
        /// The egress kind that carries no shape.
        kind: String,
        /// The shape the caller supplied anyway.
        api_shape: String,
    },
}

impl std::fmt::Display for LaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownIngress(token) => write!(
                f,
                "unknown ingress dialect `{token}`; known: {}",
                INGRESS_IDS.join(", ")
            ),
            Self::UnknownEgressKind(kind) => write!(
                f,
                "unknown egress provider_kind `{kind}`; known: {}",
                EGRESS_KINDS.join(", ")
            ),
            Self::MissingApiShape => write!(
                f,
                "egress kind `bedrock` needs an api_shape to resolve a lane; known: {}",
                BEDROCK_API_SHAPES.join(", ")
            ),
            Self::UnknownApiShape(shape) => write!(
                f,
                "unknown bedrock api_shape `{shape}`; known: {}",
                BEDROCK_API_SHAPES.join(", ")
            ),
            Self::UnexpectedApiShape { kind, api_shape } => write!(
                f,
                "egress kind `{kind}` carries no api_shape, but `{api_shape}` was supplied"
            ),
        }
    }
}

impl std::error::Error for LaneError {}

/// Resolve an ingress token to its dialect. Fails closed.
pub fn ingress_dialect(token: &str) -> Result<Dialect, LaneError> {
    match token {
        "anthropic" => Ok(Dialect::AnthropicMessages),
        "openai" => Ok(Dialect::OpenaiChat),
        "openai-responses" => Ok(Dialect::OpenaiResponses),
        other => Err(LaneError::UnknownIngress(other.to_string())),
    }
}

/// Resolve a `kind_str()` token plus an optional `api_shape` to an egress
/// lane. Fails closed on an unknown kind, on `bedrock` without a shape,
/// on an unknown shape, and on a shape supplied for a kind that has none.
pub fn resolve_egress(kind: &str, api_shape: Option<&str>) -> Result<EgressLane, LaneError> {
    if kind == "bedrock" {
        return match api_shape {
            None => Err(LaneError::MissingApiShape),
            Some("invoke") => Ok(EgressLane::BedrockInvoke),
            Some("converse") => Ok(EgressLane::BedrockConverse),
            Some(other) => Err(LaneError::UnknownApiShape(other.to_string())),
        };
    }
    let lane = match kind {
        "openai-compat" => EgressLane::OpenaiCompat,
        "anthropic-api" => EgressLane::AnthropicApi,
        "openai-responses" => EgressLane::OpenaiResponses,
        "gemini" => EgressLane::Gemini,
        other => return Err(LaneError::UnknownEgressKind(other.to_string())),
    };
    match api_shape {
        None => Ok(lane),
        Some(shape) => Err(LaneError::UnexpectedApiShape {
            kind: kind.to_string(),
            api_shape: shape.to_string(),
        }),
    }
}

/// Resolve an egress lane from its own token (the `bedrock-invoke` /
/// `bedrock-converse` spelling), for a caller that already holds a lane
/// label rather than a kind plus a shape.
pub fn egress_lane_from_token(token: &str) -> Result<EgressLane, LaneError> {
    EgressLane::ALL
        .into_iter()
        .find(|lane| lane.token() == token)
        .ok_or_else(|| LaneError::UnknownEgressKind(token.to_string()))
}

/// The fixture-side spelling of the Anthropic Messages egress. The capture
/// rig records whatever the provider passed to `trace_outgoing_body`, and
/// the anthropic-api provider passes its in-crate `PROVIDER_KIND`, which is
/// `"anthropic"` -- not the `kind_str()` token `"anthropic-api"`.
const FIXTURE_KIND_ANTHROPIC: &str = "anthropic";

/// Resolve an egress lane from a fixture's `meta.provider_kind`.
///
/// THE one place the fixture vocabulary is translated. Three spellings of
/// the same enumeration exist in the tree and each is correct in its own
/// context:
///
/// 1. `kind_str()` (`crates/routectl-router/src/config/schema.rs`) -- the
///    config-facing token, in which this egress is `anthropic-api`. That is
///    what [`resolve_egress`] and [`EgressLane::token`] speak.
/// 2. The per-provider in-crate `PROVIDER_KIND` constant, in which the same
///    egress is `anthropic`. Every other provider's constant already equals
///    its lane token.
/// 3. A fixture's `meta.provider_kind`, which is (2) verbatim: the capture
///    rig scrapes the `provider_kind=` tracing field, and each provider
///    emits it from its own constant -- except Bedrock, which emits
///    `BedrockApiShape::provider_kind_str()` and so already lands on the
///    split `bedrock-invoke` / `bedrock-converse` lane tokens.
///
/// So exactly ONE row needs translating and the rest pass through. Owning
/// it here is deliberate: `resolve_egress("anthropic", None)` errors by
/// design (it is not a `kind_str()` token), and without this function every
/// consumer holding a `FixtureMeta` would hand-roll the mapping -- which is
/// how the vocabularies drifted apart in the first place.
///
/// The bare `bedrock` kind is NOT accepted here: a fixture never carries it
/// (the rig records the shape-split token), so accepting it would invent a
/// shape the capture did not state.
pub fn egress_lane_from_fixture_kind(meta_provider_kind: &str) -> Result<EgressLane, LaneError> {
    if meta_provider_kind == FIXTURE_KIND_ANTHROPIC {
        return Ok(EgressLane::AnthropicApi);
    }
    egress_lane_from_token(meta_provider_kind)
}

/// Classify a lane from two ends already resolved to dialects. The
/// equality rule itself, shared by [`lane_class`] and any caller that
/// resolved its egress through [`egress_lane_from_fixture_kind`] instead of
/// from a `kind_str()` token.
pub fn class_for_dialects(ingress: Dialect, egress: Dialect) -> LaneClass {
    if ingress == egress {
        LaneClass::Fidelity
    } else {
        LaneClass::Translation
    }
}

/// Classify a lane from its two ends: FIDELITY iff the dialects are
/// equal. There is no lookup table behind this and no default arm -- an
/// unrecognized token on either end is an error.
pub fn lane_class(
    ingress_token: &str,
    egress_kind: &str,
    api_shape: Option<&str>,
) -> Result<LaneClass, LaneError> {
    let ingress = ingress_dialect(ingress_token)?;
    let egress = resolve_egress(egress_kind, api_shape)?.dialect();
    Ok(class_for_dialects(ingress, egress))
}

/// Identity of one lane: the ingress token plus the egress lane token.
/// What an exception is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneKey {
    /// `IngressAdapter::id()` spelling.
    pub ingress: &'static str,
    /// [`EgressLane::token`] spelling.
    pub egress: &'static str,
}

/// The `anthropic` -> `anthropic-api` lane: same dialect on both ends, so
/// FIDELITY by derivation, and the lane the live exception entries below
/// were measured on.
pub const ANTHROPIC_FIDELITY_LANE: LaneKey = LaneKey {
    ingress: "anthropic",
    egress: "anthropic-api",
};

// ---------------------------------------------------------------------
// Exception table
// ---------------------------------------------------------------------

/// How an exception is applied. See the module docs -- the distinction is
/// load-bearing and the two are not interchangeable.
pub enum Transform {
    /// Length-changing. Rewrites the INGRESS body before the diff. Takes
    /// the ingress body and returns a new one; never mutates in place.
    Normalizer(fn(&Value) -> Value),
    /// In-place value change. Decides whether one returned divergence is
    /// this transform, given that its path already matched.
    Matcher(fn(&Divergence) -> bool),
}

impl std::fmt::Debug for Transform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Normalizer(_) => "Normalizer",
            Self::Matcher(_) => "Matcher",
        })
    }
}

/// Which of the two application seams an exception uses. Derived from
/// [`Transform`]; never stored, so the two cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionKind {
    /// Applied to the ingress body pre-diff.
    Normalizer,
    /// Matched over the returned divergence set post-diff.
    Matcher,
}

/// One explained routectl transform on one lane.
#[derive(Debug)]
pub struct Exception {
    /// The lane this transform is claimed for. An exception is never
    /// consulted on another lane: reachability is per-lane, so a transform
    /// measured on one lane proves nothing on another.
    pub lane: LaneKey,
    /// Stable id, used in failure output.
    pub id: &'static str,
    /// Why this divergence is expected, and what was read to confirm it.
    pub reason: &'static str,
    /// The function in the tree that performs the transform. Verified by
    /// SYMBOL, not by line number, and always together with
    /// [`Exception::site_path`] -- a bare symbol is not a unique citation.
    /// `strip_thinking_when_tool_choice_forces_use` is defined in BOTH the
    /// anthropic-api and the Bedrock-Converse egress, so a symbol-only check
    /// would keep passing after the cited one was deleted, satisfied by an
    /// unrelated lane's homonym.
    pub site_symbol: &'static str,
    /// The file the symbol must be defined in, relative to the workspace
    /// root. Disambiguates a homonym and pins the citation to one lane's
    /// code.
    pub site_path: &'static str,
    /// Which divergence paths this exception concerns. For a matcher this
    /// is half the match (the value constraint in [`Transform::Matcher`]
    /// is the other half); for a normalizer it declares the subtree the
    /// rewrite touches.
    pub path_predicate: fn(&str) -> bool,
    /// Which FIXTURES this entry may be consulted for at all. `None` = every
    /// fixture on the lane, which is what an entry whose production
    /// transform runs for every request on the lane declares.
    ///
    /// Some transforms are gated on something a [`Divergence`] cannot see --
    /// the credential the request egressed on, for instance. A
    /// [`Transform::Matcher`] receives only the divergence, so a
    /// lane-keyed-only entry for such a transform excuses the shape on
    /// EVERY fixture of the lane, including one where the transform could
    /// not have run and the loss is real. This hook is where that gate
    /// lives: the predicate reads the fixture's captured evidence and says
    /// whether the transform was reachable for it.
    ///
    /// Only a MATCHER may carry one today, and
    /// `only_matchers_carry_a_fixture_gate` pins that: the normalizer seam
    /// ([`normalize_ingress_for_lane`]) takes a body rather than a fixture,
    /// so a gate declared on a normalizer would be silently ignored.
    pub applies_to: Option<fn(&Fixture) -> bool>,
    /// Most divergences ONE fixture may have explained by this entry.
    /// `None` = unbounded.
    ///
    /// A [`Transform::Matcher`] sees one divergence at a time and so cannot
    /// count; a production transform that writes at most N markers per
    /// request needs the bound expressed here or the entry admits an
    /// unbounded spray of the same shape. Enforced in [`unexplained`],
    /// which is called once per fixture -- matches past the bound stay
    /// unexplained.
    pub max_per_fixture: Option<usize>,
    /// How the transform is applied.
    pub transform: Transform,
    /// How many divergences this entry has matched (matcher), or how many
    /// bodies it actually rewrote (normalizer).
    matched: AtomicUsize,
}

impl Exception {
    /// Which seam this entry uses.
    pub const fn kind(&self) -> ExceptionKind {
        match self.transform {
            Transform::Normalizer(_) => ExceptionKind::Normalizer,
            Transform::Matcher(_) => ExceptionKind::Matcher,
        }
    }

    /// Whether this entry may be consulted for `fixture` at all. An entry
    /// with no [`Exception::applies_to`] hook applies to every fixture on
    /// its lane.
    pub fn eligible_for(&self, fixture: &Fixture) -> bool {
        self.applies_to.is_none_or(|gate| gate(fixture))
    }

    /// Whether this entry explains `divergence`, counting the hit.
    ///
    /// A normalizer ALWAYS returns false: it does not excuse a divergence,
    /// it removes one by realigning the inputs. Letting a length-changing
    /// transform answer here is exactly the per-index whitelist the module
    /// docs rule out.
    pub fn matches(&self, divergence: &Divergence) -> bool {
        let Transform::Matcher(predicate) = self.transform else {
            return false;
        };
        if !(self.path_predicate)(&divergence.path) || !predicate(divergence) {
            return false;
        }
        self.matched.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Apply this entry's pre-diff rewrite to an ingress body, counting an
    /// application only when it actually changed something. `None` for a
    /// matcher, which has no pre-diff seam.
    pub fn normalize(&self, ingress: &Value) -> Option<Value> {
        let Transform::Normalizer(rewrite) = self.transform else {
            return None;
        };
        let out = rewrite(ingress);
        if &out != ingress {
            self.matched.fetch_add(1, Ordering::Relaxed);
        }
        Some(out)
    }

    /// Hits so far. Zero on a populated lane means the claim is untested.
    pub fn matched_count(&self) -> usize {
        self.matched.load(Ordering::Relaxed)
    }
}

/// The id of the entry whose ingress-side rewrite mirrors the system-turn
/// lift. Named once so the entry and the gate that selects it cannot
/// drift apart.
const SYSTEM_TURN_LIFT_ID: &str = "system-turn-lift";

/// How many `role: "system"` turns `.messages` carries. Zero for a body
/// with no `messages` array, which is the same stance
/// [`without_system_turns`] takes on that shape.
pub fn in_band_system_turns(body: &Value) -> usize {
    body.get("messages")
        .and_then(Value::as_array)
        .map_or(0, |messages| {
            messages
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
                .count()
        })
}

/// Whether a fixture pair evidences the system-turn lift: the ingress
/// carried in-band system turns and the wire carried none.
///
/// Two non-lifts this must keep distinguishing, because the ingress-side
/// rewrite would MANUFACTURE a misalignment that was never on the wire in
/// either:
///
/// - EQUAL nonzero counts -- the Forward system-turn policy, where the
///   turns ride the wire in place. Nothing was lifted, so nothing may be
///   removed from the comparison's ingress side.
/// - PARTIAL loss -- some turns gone, some still on the wire. The lift
///   takes every `Role::System` turn or none, so a partial loss is some
///   OTHER transform (or real wire loss) and has to stay a finding.
///   Treating it as a lift is how this normalizer would absorb the class
///   the harness exists to surface.
pub fn system_turns_were_lifted(ingress: &Value, outgoing: &Value) -> bool {
    in_band_system_turns(ingress) > 0 && in_band_system_turns(outgoing) == 0
}

/// Drop every `role: "system"` turn from `.messages`, the ingress-side
/// mirror of the system-turn lift. Immutable: returns a new body.
///
/// A body with no `messages` array rides through unchanged -- this runs
/// over whatever a fixture happens to hold, and a shape mismatch is the
/// diff's finding to report, not this function's to panic on.
fn without_system_turns(body: &Value) -> Value {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return body.clone();
    };
    let kept: Vec<Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .cloned()
        .collect();
    let mut out = body.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("messages".to_string(), Value::Array(kept));
    }
    out
}

/// Whether `path` addresses `.messages` or any element beneath it.
fn is_messages_path(path: &str) -> bool {
    path == "messages" || path.starts_with("messages[")
}

/// Whether `path` addresses `.system` or any element beneath it.
fn is_system_path(path: &str) -> bool {
    path == "system" || path.starts_with("system[")
}

/// The marker that identifies a Claude Code billing/attribution system
/// block. The SAME rule the production strips use
/// (`system_filter::is_billing_attribution_block` and
/// `cloak::billing::block_is_billing`): a leading occurrence in the block's
/// `text` after trimming leading whitespace. Never a position in the array,
/// so a client that sends the block second is treated identically.
const BILLING_BLOCK_MARKER: &str = "x-anthropic-billing-header:";

/// Whether a `system[]` element is the billing/attribution block.
fn system_block_is_billing(block: &Value) -> bool {
    block
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim_start().starts_with(BILLING_BLOCK_MARKER))
}

/// Drop the billing/attribution block from `.system`, the ingress-side
/// mirror of the egress strip. Immutable: returns a new body.
///
/// Passes the body through unchanged when `system` is not an array, when no
/// element carries the marker, and when EVERY element does: a system array
/// that is nothing but the billing block collapses to an ABSENT `system`
/// key upstream, and modelling that here would explain a whole-field drop
/// this entry has never measured.
fn without_billing_system_block(body: &Value) -> Value {
    let Some(blocks) = body.get("system").and_then(Value::as_array) else {
        return body.clone();
    };
    let kept: Vec<Value> = blocks
        .iter()
        .filter(|block| !system_block_is_billing(block))
        .cloned()
        .collect();
    if kept.len() == blocks.len() || kept.is_empty() {
        return body.clone();
    }
    let mut out = body.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("system".to_string(), Value::Array(kept));
    }
    out
}

/// The marker value auto-cache placement writes, as it serializes.
///
/// `CacheControl::ephemeral_5m()` (`routectl-core`, `cache_control.rs`) is
/// the ONE value both placement sites assign -- the top-level terminal
/// field and `place_front_marker`'s front slot -- and its `ttl` is
/// `Some("5m")`, which serializes as a present key (the field skips
/// serialization only when `None`).
const AUTO_CACHE_TYPE: &str = "ephemeral";
/// The `ttl` [`AUTO_CACHE_TYPE`] rides with. Pinned, not open: see
/// [`is_auto_cache_marker`].
const AUTO_CACHE_TTL: &str = "5m";

/// Whether `value` is EXACTLY the marker auto-cache placement emits.
///
/// The TTL is pinned deliberately, and the earlier open-TTL reading was
/// wrong: `CacheControl::ephemeral_1h` exists in `routectl-core` but no
/// auto-placement site constructs it, so admitting `1h` excused a shape
/// production cannot emit -- a caller marker mistaken for an injection, or
/// a future placement change nobody had to review. An omitted `ttl` is
/// likewise not the emitted shape: `ephemeral_5m()` carries `Some("5m")`.
/// When a premium tier really is auto-emitted, widening this constant is
/// the review moment that change deserves.
fn is_auto_cache_marker(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.get("type").and_then(Value::as_str) == Some(AUTO_CACHE_TYPE)
        && obj.get("ttl").and_then(Value::as_str) == Some(AUTO_CACHE_TTL)
}

/// Most auto-cache markers one request can gain: the front marker plus the
/// top-level terminal one.
///
/// `apply_auto_cache_placement`
/// (`crates/routectl-router/src/router/dispatch.rs`) writes exactly two
/// slots -- one `place_front_marker` call against a single `FrontSlot`, and
/// one assignment to the top-level `cache_control` -- so a third
/// same-shaped addition on one fixture is not this transform.
const MAX_AUTO_CACHE_MARKERS: usize = 2;

/// Whether `path` is one of the THREE places routectl's auto-cache
/// placement writes a marker, in `diff_all`'s path grammar: the top-level
/// `cache_control` field (the terminal marker), or the `cache_control` leaf
/// of an indexed `system[]` / `tools[]` element (the front marker, whose
/// `FrontSlot` resolves to the last wire-eligible system block else the
/// last custom tool).
///
/// A `cache_control` leaf anywhere else -- inside `messages[]`, inside a
/// tool's caller-authored `input_schema` -- is NOT admitted: auto-placement
/// never writes there, so such a key came from the caller and appears on
/// BOTH sides of the diff rather than as an addition.
fn is_auto_cache_marker_path(path: &str) -> bool {
    if path == "cache_control" {
        return true;
    }
    let Some(parent) = path.strip_suffix(".cache_control") else {
        return false;
    };
    is_indexed_element_of(parent, "system") || is_indexed_element_of(parent, "tools")
}

/// Whether `path` is exactly `<array_key>[<digits>]`.
fn is_indexed_element_of(path: &str, array_key: &str) -> bool {
    let Some(index) = path
        .strip_prefix(array_key)
        .and_then(|rest| rest.strip_prefix('['))
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `expected` is `actual` plus a NON-EMPTY bracketed suffix, e.g.
/// `claude-opus-4-8[1m]` against `claude-opus-4-8`.
///
/// Any bracket content qualifies, not only a context-window marker: no
/// other suffix vocabulary exists on this field today, and pinning the
/// literal `[1m]` would make the caller stale the day a second window
/// ships. What the check does enforce is the ALIAS RELATIONSHIP -- the wire
/// value must be an exact prefix of the ingress value, and the remainder
/// must be a complete bracket pair with something inside it.
fn is_bracketed_alias_of(expected: &str, actual: &str) -> bool {
    let Some(suffix) = expected.strip_prefix(actual) else {
        return false;
    };
    suffix.len() > 2 && suffix.starts_with('[') && suffix.ends_with(']')
}

/// The header name whose value carries the OAuth-bearer credential.
const AUTHORIZATION_HEADER: &str = "authorization";
/// The credential scheme an OAuth-bearer egress presents. Case-insensitive
/// per RFC 7235.
const BEARER_SCHEME: &str = "bearer ";

/// Whether the fixture's OUTGOING request egressed on a bearer credential.
///
/// The evidence survives scrubbing by design: the fixture scrub replaces
/// the credential VALUE and keeps the scheme, so a captured OAuth egress
/// reads `authorization: Bearer [REDACTED]` while an api-key egress carries
/// `x-api-key` and no `authorization` at all. Keyed on the outgoing side,
/// never the ingress: the client's own credential says nothing about which
/// credential routectl re-signed the request with.
///
/// This is a NECESSARY condition for the cloak lane rather than the whole
/// of it -- `is_cloak_lane`
/// (`crates/routectl-providers/src/anthropic_api/client.rs`) additionally
/// requires the `api.anthropic.com` host and a non-forwarded leg, neither
/// of which a scrubbed capture pins (the outgoing `host` header is not
/// recorded, and forwarding is not a fixture field). What it does exclude
/// is the case the hole was about: an api-key fixture, where the transform
/// provably did not run.
fn outgoing_credential_is_bearer(fixture: &Fixture) -> bool {
    fixture
        .outgoing_request_headers
        .iter()
        .any(|(name, value)| {
            name.eq_ignore_ascii_case(AUTHORIZATION_HEADER)
                && value.len() >= BEARER_SCHEME.len()
                && value[..BEARER_SCHEME.len()].eq_ignore_ascii_case(BEARER_SCHEME)
        })
}

/// The exception entries on [`ANTHROPIC_FIDELITY_LANE`].
///
/// Every `reason` below was re-confirmed by reading the named symbol in
/// current code; `site_symbol` records which one.
static ANTHROPIC_FIDELITY_EXCEPTIONS: [Exception; 7] = [
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "system-turn-lift",
        reason: "Ingress `role: \"system\"` turns are lifted out of `messages[]` into the \
                 wire `system` field, so `.messages` shrinks and every later element shifts \
                 index. Confirmed at `lift_legacy_system_stripped` \
                 (crates/routectl-providers/src/anthropic_api/system.rs), which joins the \
                 Role::System texts (dropping billing/attribution blocks per message) and is \
                 reached only from the `req.system`-ABSENT fallback in the anthropic-api \
                 request assembly; `lift_legacy_system` is the unfiltered sibling the \
                 Bedrock Converse egress reuses. Length-changing, hence a NORMALIZER: \
                 whitelisting it per index would have to cover essentially the whole message \
                 array. Reachability caveat, re-derive before trusting it: the lift runs \
                 under the Lift system-turn policy only -- under Forward the turns ride the \
                 wire in place and removing them from the ingress side would INTRODUCE a \
                 misalignment. That is why the rewrite is selected PER PAIR by \
                 `system_turns_were_lifted` rather than per lane: it is consulted only when \
                 the ingress carried in-band system turns and the wire carried none.",
        site_symbol: "lift_legacy_system_stripped",
        site_path: "crates/routectl-providers/src/anthropic_api/system.rs",
        path_predicate: is_messages_path,
        applies_to: None,
        max_per_fixture: None,
        transform: Transform::Normalizer(without_system_turns),
        matched: AtomicUsize::new(0),
    },
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "thinking-temperature-clamp",
        reason: "`.temperature` appears on the wire as `1.0` without the client sending it. \
                 Confirmed at `clamp_sampling_for_thinking` \
                 (crates/routectl-providers/src/anthropic_api/request.rs), which returns \
                 `Some(1.0)` for a thinking config of Enabled or Adaptive regardless of the \
                 caller's value (and drops `top_p` whenever a temperature is in play, since \
                 Anthropic rejects both together). NOT `reconcile_sampling_params`: that \
                 function early-returns while active thinking survives on the body and so \
                 cannot be the source. In-place value change, hence a MATCHER.",
        site_symbol: "clamp_sampling_for_thinking",
        site_path: "crates/routectl-providers/src/anthropic_api/request.rs",
        path_predicate: |path| path == "temperature",
        applies_to: None,
        max_per_fixture: None,
        transform: Transform::Matcher(|divergence| {
            divergence.kind == DivergenceKind::Added
                && divergence.actual.as_ref().and_then(Value::as_f64) == Some(1.0)
        }),
        matched: AtomicUsize::new(0),
    },
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "model-alias-suffix-resolved",
        reason: "`.model` on the wire is the operator-configured upstream id while the \
                 ingress body carried the client's alias, which may end in a bracketed \
                 context-window suffix (e.g. `claude-opus-4-8[1m]` -> `claude-opus-4-8`). \
                 Confirmed at the `attempt_req.model = target.upstream` assignment inside \
                 `complete_inner` (crates/routectl-router/src/router/dispatch.rs), which runs \
                 for every non-forwarded target; `stream_inner` does the same on the streaming \
                 path. NOTE the citation is weaker than the other three: `complete_inner` is a \
                 long dispatch method rather than a single-purpose transform, so its symbol \
                 surviving proves only that dispatch still exists, not that this assignment \
                 does. There is no narrower symbol to name -- no function strips the bracketed \
                 suffix, because the suffix is simply part of an alias that resolves to a bare \
                 upstream id. The predicate is what carries the precision here: it admits only \
                 an ingress value equal to the wire value plus a bracketed suffix, so a model \
                 divergence of another shape stays a finding. A bracketed suffix other than a \
                 context-window marker would also be admitted; no other suffix vocabulary \
                 exists on this field today, and narrowing to a literal `[1m]` would make the \
                 entry stale the day a second window ships. A forwarded target keeps the \
                 client's model verbatim and produces no divergence here at all.",
        site_symbol: "complete_inner",
        site_path: "crates/routectl-router/src/router/dispatch.rs",
        path_predicate: |path| path == "model",
        applies_to: None,
        max_per_fixture: None,
        transform: Transform::Matcher(|divergence| {
            divergence.kind == DivergenceKind::Changed
                && match (
                    divergence.actual.as_ref().and_then(Value::as_str),
                    divergence.expected.as_ref().and_then(Value::as_str),
                ) {
                    (Some(wire), Some(ingress)) => is_bracketed_alias_of(ingress, wire),
                    _ => false,
                }
        }),
        matched: AtomicUsize::new(0),
    },
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "disabled-thinking-dropped",
        reason: "The ingress sent `thinking: {\"type\": \"disabled\"}` and the wire body \
                 carries no `thinking` key at all; `max_tokens` is untouched. Confirmed at \
                 `strip_thinking_when_tool_choice_forces_use` \
                 (crates/routectl-providers/src/anthropic_api/extras.rs), which removes the \
                 whole `thinking` key when `tool_choice.type` is `any` or `tool` because \
                 Anthropic forbids the combination; the context-management cache-miss \
                 soft-fail in the same assembly drops it wholesale for the same reason. Both \
                 test only for the key's presence, so a `disabled` config is dropped along \
                 with an active one. Narrowed to the `disabled` form deliberately: that \
                 config spends no reasoning budget, so its loss carries no content, whereas \
                 an ACTIVE thinking config vanishing from the wire is a real fidelity loss \
                 and must keep surfacing. The divergence path is `thinking`, not \
                 `thinking.type` -- a wholesale key drop is reported at the key itself.",
        site_symbol: "strip_thinking_when_tool_choice_forces_use",
        site_path: "crates/routectl-providers/src/anthropic_api/extras.rs",
        path_predicate: |path| path == "thinking",
        applies_to: None,
        max_per_fixture: None,
        transform: Transform::Matcher(|divergence| {
            divergence.kind == DivergenceKind::Removed
                && divergence.expected.as_ref().and_then(|v| v.get("type"))
                    == Some(&Value::String("disabled".to_string()))
        }),
        matched: AtomicUsize::new(0),
    },
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "billing-system-block-stripped",
        reason: "The ingress `system[]` carries the Claude Code billing/attribution block (a \
                 block whose `text` starts with the `x-anthropic-billing-header:` marker after \
                 trimming leading whitespace) and the wire `system[]` does not, so the array \
                 shrinks and every later block shifts index. Confirmed at \
                 `strip_billing_attribution` \
                 (crates/routectl-providers/src/system_filter.rs), which drops the marked \
                 block from the canonical `SystemContent` and is called unconditionally from \
                 the anthropic-api request assembly (`request.rs`) -- an anthropic-api \
                 provider can be pointed at a third-party host where the OAuth cloak never \
                 fires, so the strip runs on the always-on normalize path. The OAuth cloak \
                 repeats it on the assembled JSON body at `strip_billing_block` \
                 (crates/routectl-providers/src/anthropic_api/cloak/billing.rs), keyed on the \
                 SAME leading marker, so either path produces this shape. Identified by the \
                 marker, never by position: both production predicates test the block's own \
                 text, so a client that sends the block second is stripped identically. \
                 Length-changing, hence a NORMALIZER: the shift reports a divergence at every \
                 later system index plus a membership divergence at the tail, and an entry \
                 broad enough to whitelist that would cover the whole system array -- which \
                 on this lane is most of the prompt. It carries NO `applies_to` fixture gate, \
                 deliberately and re-verified: the `request.rs` call is on the always-run \
                 normalize path, unconditional on the credential, so the strip fires for an \
                 api-key egress exactly as for an OAuth one and scoping it to a Bearer capture \
                 would leave a real api-key fixture unadjudicable. Deliberately NOT the non-CC system \
                 reduction: `relocate_client_system` \
                 (crates/routectl-providers/src/anthropic_api/cloak/identity.rs) also reshapes \
                 `system[]`, but only for a non-Claude-Code client, and it MOVES content into \
                 the first user message rather than dropping the billing block. This entry \
                 models only the billing removal, so a relocation keeps surfacing.",
        site_symbol: "strip_billing_attribution",
        site_path: "crates/routectl-providers/src/system_filter.rs",
        path_predicate: is_system_path,
        applies_to: None,
        max_per_fixture: None,
        transform: Transform::Normalizer(without_billing_system_block),
        matched: AtomicUsize::new(0),
    },
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "auto-cache-breakpoint-injected",
        reason: "An ephemeral `cache_control` marker appears on the wire that the client never \
                 sent: the top-level field (the TERMINAL marker) and/or the `cache_control` \
                 leaf of the last wire-eligible `system[]` block, else the last custom tool \
                 (the FRONT marker). Confirmed at `apply_auto_cache_placement` \
                 (crates/routectl-router/src/router/dispatch.rs), which assigns \
                 `CacheControl::ephemeral_5m()` to the top-level field and places the front \
                 marker through `place_front_marker` in the same file, both on a throwaway \
                 per-attempt clone that is committed only once the full breakpoint sequence \
                 validates. Injection is withheld entirely when the caller supplied any \
                 breakpoint of its own, which is why this shows up as an ADDITION and never as \
                 a rewrite of a caller marker. The value constraint is the EXACT emitted shape, \
                 `type == \"ephemeral\"` AND `ttl == \"5m\"`: both placement sites assign \
                 `CacheControl::ephemeral_5m()`, whose `ttl` is `Some(\"5m\")` and therefore \
                 serializes as a present key, so an omitted TTL and the `1h` premium tier -- \
                 `CacheControl::ephemeral_1h` exists in routectl-core but no auto-placement \
                 site constructs it -- are shapes production cannot emit and must keep \
                 surfacing. Widening the pin when a premium tier really is auto-emitted is the \
                 review moment that change deserves. The path predicate admits ONLY the three \
                 places placement writes -- the top-level \
                 field and a `cache_control` leaf directly under an indexed `system[]` / \
                 `tools[]` element -- so a `cache_control` key appearing anywhere else (inside \
                 `messages[]`, inside a tool's caller-authored `input_schema`) stays a \
                 finding. CARDINALITY is bounded per fixture rather than left to the matcher: \
                 placement emits at most one front marker plus the top-level one, so \
                 `max_per_fixture` caps the entry at two and a third same-shaped addition \
                 stays unexplained -- a matcher sees one divergence at a time and cannot count. \
                 In-place addition that moves no positions, hence a MATCHER.",
        site_symbol: "apply_auto_cache_placement",
        site_path: "crates/routectl-router/src/router/dispatch.rs",
        path_predicate: is_auto_cache_marker_path,
        applies_to: None,
        max_per_fixture: Some(MAX_AUTO_CACHE_MARKERS),
        transform: Transform::Matcher(|divergence| {
            divergence.kind == DivergenceKind::Added
                && divergence.actual.as_ref().is_some_and(is_auto_cache_marker)
        }),
        matched: AtomicUsize::new(0),
    },
    Exception {
        lane: ANTHROPIC_FIDELITY_LANE,
        id: "oauth-sampling-stripped",
        reason: "The ingress carried a top-level `temperature` and/or `top_p` and the wire body \
                 carries neither. Confirmed at `normalize_claude_sampling` \
                 (crates/routectl-providers/src/anthropic_api/extras.rs), which removes BOTH \
                 keys from the FINAL outbound body and emits one contents-free WARN naming the \
                 keys it dropped: Anthropic's OAuth seat on api.anthropic.com 400s a \
                 `/v1/messages` body carrying either. It is the last word on the JSON -- called \
                 after `cloak_body` and before the outgoing-body trace -- so no later pass \
                 re-introduces a stripped key. Gated on the CREDENTIAL LANE, not on the cloak \
                 state: `is_cloak_lane` \
                 (crates/routectl-providers/src/anthropic_api/client.rs) is OauthBearer auth \
                 plus an api.anthropic.com host plus a non-forwarded leg, and the off-lane \
                 preservation is pinned by the provider's own tests. That gate is invisible to \
                 a divergence, so this entry carries an `applies_to` FIXTURE gate \
                 (`outgoing_credential_is_bearer`) and is consulted only for a capture whose \
                 OUTGOING headers present a `Bearer` credential -- the scheme survives \
                 scrubbing while the value does not. Without it the entry excused a dropped \
                 `temperature` on every fixture of the lane, including an api-key or forwarded \
                 capture where the strip provably never ran and the loss is real. The gate is \
                 NECESSARY rather than sufficient: a scrubbed capture pins no outgoing host and \
                 no forwarding flag, so those two conditions stay unverifiable here. Both keys \
                 are admitted deliberately -- the production \
                 strip removes the pair, so a predicate naming `temperature` alone would leave \
                 the same hole open for the first fixture that sends `top_p`. The removed VALUE \
                 is deliberately NOT pinned: the strip is unconditional on this lane and drops \
                 whatever the caller, the thinking clamp, or a `provider_extras` smuggle put \
                 there, so pinning a value would fail to explain the identical transform on the \
                 next capture that happens to send a different one. Distinct from \
                 `thinking-temperature-clamp` and deliberately a separate entry: that one is an \
                 ADDED `1.0` -- a value the wire GAINS from the clamp at request-assembly time \
                 -- while this is a REMOVED key, a value the wire LOSES at the egress boundary. \
                 Different direction, different site, and on this lane the two compose (the \
                 clamp writes `1.0`, then this strips it), which is why one entry could not \
                 cover both. In-place key removal that moves no positions, hence a MATCHER.",
        site_symbol: "normalize_claude_sampling",
        site_path: "crates/routectl-providers/src/anthropic_api/extras.rs",
        path_predicate: |path| path == "temperature" || path == "top_p",
        applies_to: Some(outgoing_credential_is_bearer),
        max_per_fixture: None,
        transform: Transform::Matcher(|divergence| divergence.kind == DivergenceKind::Removed),
        matched: AtomicUsize::new(0),
    },
];

/// THE lock that serializes every reader of a per-exception counter DELTA.
///
/// The counters are process-global statics that only ever increase, so a
/// zero-match gate has to read the delta across its own walk -- and that
/// delta is attributable only while nothing else is incrementing them.
/// Cargo runs the tests of one binary on several threads, so both a
/// conservation walk AND a unit test that calls `Exception::matches`
/// directly must hold THIS lock, not one lock each: two locks serialize
/// each side against itself and leave the two sides racing, which reads as
/// a doubled delta and passes or fails by timing.
pub static COUNTER_DELTA_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Every exception entry, across all lanes.
pub fn all_exceptions() -> &'static [Exception] {
    &ANTHROPIC_FIDELITY_EXCEPTIONS
}

/// The entries claimed for `lane`.
pub fn exceptions_for_lane(lane: &LaneKey) -> Vec<&'static Exception> {
    all_exceptions()
        .iter()
        .filter(|entry| &entry.lane == lane)
        .collect()
}

/// Apply every normalizer registered for `lane` to an ingress body, in
/// table order, and return the body to pass as `expected`. Each
/// application that changed something counts against its entry.
///
/// For a caller that holds only ONE body. A corpus walk holds both and
/// uses [`normalize_ingress_for_pair`], which additionally decides whether
/// the system-turn lift actually happened for this capture -- the same
/// weaker-form / fixture-aware-form split as [`unexplained`] vs
/// [`unexplained_for_fixture`], and for the same reason: this form applies
/// `system-turn-lift`'s rewrite unconditionally, which on a pair whose
/// turns rode the wire in place MANUFACTURES a misalignment that was never
/// transmitted.
pub fn normalize_ingress_for_lane(lane: &LaneKey, ingress: &Value) -> Value {
    fold_normalizers(&exceptions_for_lane(lane), ingress)
}

/// Apply the normalizers registered for `lane` that this fixture PAIR
/// evidences, and return the body to pass as `expected`.
///
/// `system-turn-lift` is length-changing on `.messages` and is the one
/// entry whose applicability a single body cannot settle: whether the
/// production lift ran is a property of the PAIR (see
/// [`system_turns_were_lifted`]). Under the Forward system-turn policy the
/// turns ride the wire in place, and rewriting the ingress side then
/// injects a misalignment the wire never carried -- inflating a handful of
/// real differences into one per surviving index. So the entry is consulted
/// only when the pair evidences a lift; where it does fire, its rewrite and
/// its hit counting are exactly as before.
pub fn normalize_ingress_for_pair(lane: &LaneKey, ingress: &Value, outgoing: &Value) -> Value {
    let lifted = system_turns_were_lifted(ingress, outgoing);
    let eligible: Vec<&'static Exception> = exceptions_for_lane(lane)
        .into_iter()
        .filter(|entry| lifted || entry.id != SYSTEM_TURN_LIFT_ID)
        .collect();
    fold_normalizers(&eligible, ingress)
}

/// The shared fold: apply each entry's pre-diff rewrite in table order.
/// Order matters -- the two normalizers touch different subtrees today,
/// but a future pair that touch one must compose deterministically.
fn fold_normalizers(entries: &[&'static Exception], ingress: &Value) -> Value {
    entries.iter().fold(ingress.clone(), |body, entry| {
        entry.normalize(&body).unwrap_or(body)
    })
}

/// The divergences on `lane` that no exception explains, with every entry
/// on the lane considered eligible.
///
/// For a caller that holds NO fixture. A caller that does hold one uses
/// [`unexplained_for_fixture`], which additionally consults each entry's
/// [`Exception::applies_to`] gate -- a fixture-gated entry is
/// unconditionally eligible here, so this form is strictly the weaker
/// adjudication and is never the one a corpus walk wants.
pub fn unexplained<'a>(lane: &LaneKey, divergences: &'a [Divergence]) -> Vec<&'a Divergence> {
    residual(&exceptions_for_lane(lane), divergences)
}

/// The divergences on `lane` that no exception explains FOR THIS FIXTURE.
///
/// The fixture-aware form, and the one a corpus walk uses: an entry whose
/// [`Exception::applies_to`] gate rejects the fixture is not consulted at
/// all, so a transform that could not have run for this capture cannot
/// excuse the shape it would have produced.
pub fn unexplained_for_fixture<'a>(
    lane: &LaneKey,
    fixture: &Fixture,
    divergences: &'a [Divergence],
) -> Vec<&'a Divergence> {
    let eligible: Vec<&'static Exception> = exceptions_for_lane(lane)
        .into_iter()
        .filter(|entry| entry.eligible_for(fixture))
        .collect();
    residual(&eligible, divergences)
}

/// The shared adjudication: which of `divergences` no entry of `entries`
/// explains, enforcing each entry's [`Exception::max_per_fixture`] bound
/// across this ONE call.
///
/// Counting happens inside [`Exception::matches`], so the bound is checked
/// BEFORE the entry is consulted: a match past the bound must neither
/// excuse the divergence nor inflate the entry's hit count, or a
/// too-broad matcher reads as a well-exercised one.
fn residual<'a>(
    entries: &[&'static Exception],
    divergences: &'a [Divergence],
) -> Vec<&'a Divergence> {
    let mut used = vec![0usize; entries.len()];
    divergences
        .iter()
        .filter(|divergence| {
            !entries.iter().enumerate().any(|(idx, entry)| {
                if entry.max_per_fixture.is_some_and(|max| used[idx] >= max) {
                    return false;
                }
                let matched = entry.matches(divergence);
                if matched {
                    used[idx] += 1;
                }
                matched
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// Symbol resolution
// ---------------------------------------------------------------------

/// Why a `site_symbol` / `site_path` citation failed to resolve. Each
/// variant is a distinct drift mode, so a failure says what to do next
/// rather than only that something is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolError {
    /// The workspace root could not be located, or it holds no `.rs` files
    /// at all. Distinct from every other variant on purpose: an empty
    /// search corpus makes a "symbol is absent" answer VACUOUSLY true, so
    /// it must never be reported as a clean negative.
    SearchTreeUnavailable(String),
    /// The cited file does not exist or could not be read.
    SitePathUnreadable(String),
    /// The file exists but defines no such function.
    SymbolNotInSitePath {
        /// The symbol that was looked for.
        symbol: String,
        /// The file it was expected in.
        site_path: String,
    },
    /// The file defines the symbol more than once, so the citation does not
    /// name one site.
    SymbolAmbiguousInSitePath {
        /// The symbol that was looked for.
        symbol: String,
        /// The file it was found in.
        site_path: String,
        /// How many definitions were found.
        count: usize,
    },
}

impl std::fmt::Display for SymbolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SearchTreeUnavailable(detail) => write!(
                f,
                "cannot search the workspace for symbol citations: {detail}. \
                 A negative result would be vacuous, so this is a hard failure"
            ),
            Self::SitePathUnreadable(path) => {
                write!(f, "cited site path `{path}` cannot be read")
            }
            Self::SymbolNotInSitePath { symbol, site_path } => write!(
                f,
                "`{site_path}` defines no `fn {symbol}`; the transform was renamed, \
                 moved, or deleted -- re-derive the citation"
            ),
            Self::SymbolAmbiguousInSitePath {
                symbol,
                site_path,
                count,
            } => write!(
                f,
                "`{site_path}` defines `fn {symbol}` {count} times; the citation \
                 does not name a single site"
            ),
        }
    }
}

impl std::error::Error for SymbolError {}

/// Resolve a `(symbol, site_path)` citation: the named file must define the
/// named function EXACTLY once.
///
/// Pinning the file is what makes the check falsifiable against DRIFT. A
/// search-the-whole-tree-for-the-symbol version passes on a homonym --
/// `strip_thinking_when_tool_choice_forces_use` is defined in both the
/// anthropic-api and the Bedrock-Converse egress -- so deleting the cited
/// one would leave the check green, satisfied by an unrelated lane's
/// function. It also passes on a symbol that survives only as a test
/// helper, since the workspace tree includes test modules (this very file
/// among them).
///
/// WHAT THIS DOES AND DOES NOT PROVE. It proves that a function of this
/// name is defined in the cited file, which catches the realistic drift:
/// the transform renamed or moved with its call sites updated (an outright
/// deletion is caught earlier, by the compiler, at those call sites). It
/// does NOT prove the cited file belongs to the lane the entry claims --
/// repointing an entry's `site_path` at another lane's file that happens to
/// define the same symbol still resolves. That is a deliberate rewrite
/// rather than a drift mode, so it is accepted rather than closed;
/// asserting the cited path's lane against the entry's own `lane.egress`
/// would be more machinery than the risk earns.
///
/// Failing when the search tree is unavailable is deliberate: an empty
/// corpus makes "not found" true for free, which is exactly how a paired
/// negative control passes without proving anything.
pub fn resolve_site_symbol(symbol: &str, site_path: &str) -> Result<(), SymbolError> {
    let root = workspace_root()?;
    let full = root.join(site_path);
    let text = std::fs::read_to_string(&full)
        .map_err(|_| SymbolError::SitePathUnreadable(site_path.to_string()))?;
    match count_fn_definitions(&text, symbol) {
        1 => Ok(()),
        0 => Err(SymbolError::SymbolNotInSitePath {
            symbol: symbol.to_string(),
            site_path: site_path.to_string(),
        }),
        count => Err(SymbolError::SymbolAmbiguousInSitePath {
            symbol: symbol.to_string(),
            site_path: site_path.to_string(),
            count,
        }),
    }
}

/// How many times `text` defines `fn <symbol>`. Counts both the plain and
/// the generic form (`fn f(` / `fn f<`).
fn count_fn_definitions(text: &str, symbol: &str) -> usize {
    [format!("fn {symbol}("), format!("fn {symbol}<")]
        .iter()
        .map(|needle| text.matches(needle.as_str()).count())
        .sum()
}

/// The workspace root, verified to exist and to actually contain the
/// `crates/` tree. Canonicalized so the `..` hops cannot silently resolve
/// to nothing.
pub fn workspace_root() -> Result<std::path::PathBuf, SymbolError> {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("..").join("..");
    let root = root.canonicalize().map_err(|e| {
        SymbolError::SearchTreeUnavailable(format!("{} does not resolve: {e}", root.display()))
    })?;
    let crates_dir = root.join("crates");
    if !crates_dir.is_dir() {
        return Err(SymbolError::SearchTreeUnavailable(format!(
            "{} is not a directory",
            crates_dir.display()
        )));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    // ---------- class derivation ----------

    #[test]
    fn lane_class_is_fidelity_when_the_two_ends_share_a_dialect() {
        for (ingress, kind, api_shape) in [
            ("anthropic", "anthropic-api", None),
            ("anthropic", "bedrock", Some("invoke")),
            ("openai", "openai-compat", None),
            ("openai-responses", "openai-responses", None),
        ] {
            let class = lane_class(ingress, kind, api_shape).expect("known lane");

            assert_eq!(
                class,
                LaneClass::Fidelity,
                "{ingress} -> {kind}/{api_shape:?} shares a dialect"
            );
        }
    }

    #[test]
    fn lane_class_is_translation_across_dialects() {
        for (ingress, kind, api_shape) in [
            ("anthropic", "openai-compat", None),
            ("anthropic", "gemini", None),
            ("anthropic", "bedrock", Some("converse")),
            ("openai", "anthropic-api", None),
            ("openai", "openai-responses", None),
            ("openai-responses", "openai-compat", None),
        ] {
            let class = lane_class(ingress, kind, api_shape).expect("known lane");

            assert_eq!(
                class,
                LaneClass::Translation,
                "{ingress} -> {kind}/{api_shape:?} crosses a dialect boundary"
            );
        }
    }

    #[test]
    fn the_egress_dialect_map_has_six_rows() {
        assert_eq!(EgressLane::ALL.len(), 6);

        let tokens: Vec<&str> = EgressLane::ALL.into_iter().map(EgressLane::token).collect();

        assert_eq!(
            tokens,
            vec![
                "openai-compat",
                "anthropic-api",
                "bedrock-invoke",
                "bedrock-converse",
                "openai-responses",
                "gemini",
            ]
        );
    }

    #[test]
    fn bedrock_splits_into_two_distinct_dialects_by_api_shape() {
        let invoke = resolve_egress("bedrock", Some("invoke")).expect("invoke resolves");
        let converse = resolve_egress("bedrock", Some("converse")).expect("converse resolves");

        assert_ne!(invoke, converse);
        assert_ne!(
            invoke.dialect(),
            converse.dialect(),
            "the two shapes are different dialects"
        );
        assert_eq!(invoke.dialect(), Dialect::AnthropicMessages);
        assert_eq!(converse.dialect(), Dialect::BedrockConverse);
    }

    // ---------- fail closed ----------

    #[test]
    fn an_unknown_ingress_token_errors_naming_the_value() {
        // Positive control: every token an ingress adapter emits resolves,
        // so the rejection below is a real boundary and not a broken map.
        for token in INGRESS_IDS {
            assert!(
                ingress_dialect(token).is_ok(),
                "known ingress `{token}` must resolve"
            );
        }

        let err = ingress_dialect("anthropic-api").expect_err("not an ingress token");

        assert_eq!(
            err,
            LaneError::UnknownIngress("anthropic-api".to_string()),
            "the error must carry the offending token"
        );
        assert!(err.to_string().contains("anthropic-api"), "got: {err}");
    }

    #[test]
    fn an_unknown_egress_kind_errors_naming_the_value() {
        // Positive control: every kind_str token resolves (bedrock through
        // its shape), so no recognized kind lacks a dialect row.
        for kind in EGRESS_KINDS {
            if *kind == "bedrock" {
                for shape in BEDROCK_API_SHAPES {
                    assert!(
                        resolve_egress(kind, Some(shape)).is_ok(),
                        "known lane `{kind}`/`{shape}` must resolve"
                    );
                }
            } else {
                assert!(
                    resolve_egress(kind, None).is_ok(),
                    "known kind `{kind}` must resolve"
                );
            }
        }

        let err = resolve_egress("anthropic", None).expect_err("PROVIDER_KIND is not a kind_str");

        assert_eq!(
            err,
            LaneError::UnknownEgressKind("anthropic".to_string()),
            "the error must carry the offending kind"
        );
        assert!(err.to_string().contains("anthropic"), "got: {err}");
    }

    #[test]
    fn egress_kinds_is_the_same_set_as_the_config_provider_kinds() {
        // Welds this module's hand-listed vocabulary to its source. Set
        // equality in BOTH directions, not a length or subset check:
        // a kind added to the config schema must gain a dialect row here,
        // and if `CONFIG_PROVIDER_KINDS` ever SHRINKS (it is cfg-gated per
        // provider feature, and a reduced-feature build is not a supported
        // routectl-cli target) this fails loudly instead of narrowing in
        // silence.
        let config: BTreeSet<&str> = routectl_router::CONFIG_PROVIDER_KINDS
            .iter()
            .copied()
            .collect();
        let harness: BTreeSet<&str> = EGRESS_KINDS.iter().copied().collect();

        let missing_here: Vec<&str> = config.difference(&harness).copied().collect();
        let missing_in_config: Vec<&str> = harness.difference(&config).copied().collect();

        assert!(
            missing_here.is_empty(),
            "provider kinds present in the config schema but absent from \
             EGRESS_KINDS: {missing_here:?} -- a kind an operator can write \
             as `kind = \"...\"` needs a dialect row in this module, or \
             every fixture on its lane fails as an unknown egress kind",
        );
        assert!(
            missing_in_config.is_empty(),
            "tokens present in EGRESS_KINDS but absent from the config \
             schema: {missing_in_config:?} -- this module must not invent \
             egress kinds no config can name",
        );
    }

    #[test]
    fn bedrock_without_an_api_shape_is_unresolvable() {
        let err = resolve_egress("bedrock", None).expect_err("bedrock needs a shape");

        assert_eq!(err, LaneError::MissingApiShape);
    }

    #[test]
    fn an_unknown_bedrock_api_shape_errors_naming_the_value() {
        let err = resolve_egress("bedrock", Some("invocation")).expect_err("unknown shape");

        assert_eq!(err, LaneError::UnknownApiShape("invocation".to_string()));
        assert!(err.to_string().contains("invocation"), "got: {err}");
    }

    #[test]
    fn a_shapeless_kind_given_an_api_shape_errors_naming_both() {
        let err =
            resolve_egress("anthropic-api", Some("invoke")).expect_err("no shape on the kind");

        assert_eq!(
            err,
            LaneError::UnexpectedApiShape {
                kind: "anthropic-api".to_string(),
                api_shape: "invoke".to_string(),
            }
        );
        let rendered = err.to_string();
        assert!(rendered.contains("anthropic-api"), "got: {rendered}");
        assert!(rendered.contains("invoke"), "got: {rendered}");
    }

    #[test]
    fn a_lane_token_round_trips_through_its_own_spelling() {
        for lane in EgressLane::ALL {
            assert_eq!(
                egress_lane_from_token(lane.token()).expect("own token resolves"),
                lane
            );
        }

        let err = egress_lane_from_token("bedrock").expect_err("the bare kind is not a lane token");

        assert_eq!(err, LaneError::UnknownEgressKind("bedrock".to_string()));
    }

    // ---------- table shape ----------

    #[test]
    fn every_exception_carries_a_lane_an_id_a_reason_and_a_site_symbol() {
        for entry in all_exceptions() {
            assert!(!entry.id.is_empty(), "an entry has no id");
            assert!(!entry.reason.is_empty(), "`{}` has no reason", entry.id);
            assert!(
                !entry.site_symbol.is_empty(),
                "`{}` has no site_symbol",
                entry.id
            );
            assert!(
                !entry.site_path.is_empty(),
                "`{}` has no site_path",
                entry.id
            );
            // The entry's OWN lane, not a hardcoded one: a future entry on
            // another lane must be validated against the lane it claims.
            assert!(
                lane_class(entry.lane.ingress, entry.lane.egress, None).is_ok(),
                "`{}` names an unresolvable lane ({} -> {})",
                entry.id,
                entry.lane.ingress,
                entry.lane.egress
            );
        }
    }

    #[test]
    fn the_anthropic_fidelity_lane_carries_two_normalizers_and_five_matchers() {
        let entries = exceptions_for_lane(&ANTHROPIC_FIDELITY_LANE);

        let kinds: Vec<(&str, ExceptionKind)> = entries.iter().map(|e| (e.id, e.kind())).collect();
        assert_eq!(
            kinds,
            vec![
                ("system-turn-lift", ExceptionKind::Normalizer),
                ("thinking-temperature-clamp", ExceptionKind::Matcher),
                ("model-alias-suffix-resolved", ExceptionKind::Matcher),
                ("disabled-thinking-dropped", ExceptionKind::Matcher),
                ("billing-system-block-stripped", ExceptionKind::Normalizer),
                ("auto-cache-breakpoint-injected", ExceptionKind::Matcher),
                ("oauth-sampling-stripped", ExceptionKind::Matcher),
            ],
            "the length-changing entries must be NORMALIZERS and the in-place \
             value changes MATCHERS",
        );
    }

    // ---------- symbol resolution ----------

    #[test]
    fn the_search_tree_is_actually_present() {
        // Guards every negative below. An unresolvable workspace root
        // yields an empty search corpus, which makes "this symbol is
        // absent" true for free -- the paired controls would then pass
        // while proving nothing.
        let root = workspace_root().expect("the workspace root must resolve");

        assert!(
            root.join("crates").is_dir(),
            "{} holds no crates/ tree",
            root.display()
        );
    }

    #[test]
    fn every_site_symbol_resolves_at_its_cited_path() {
        for entry in all_exceptions() {
            assert!(
                resolve_site_symbol(entry.site_symbol, entry.site_path).is_ok(),
                "`{}` cites `{}` in `{}`: {}",
                entry.id,
                entry.site_symbol,
                entry.site_path,
                resolve_site_symbol(entry.site_symbol, entry.site_path)
                    .expect_err("just asserted it fails")
            );
        }
    }

    #[test]
    fn the_resolver_rejects_a_symbol_absent_from_the_cited_file() {
        // Paired control: the check above must be able to fail. The file is
        // real (so this is not the unreadable-path branch) and the symbol
        // is not in it.
        let err = resolve_site_symbol(
            "lift_legacy_system_stripped_v2",
            "crates/routectl-providers/src/anthropic_api/system.rs",
        )
        .expect_err("a nonexistent symbol must not resolve");

        assert_eq!(
            err,
            SymbolError::SymbolNotInSitePath {
                symbol: "lift_legacy_system_stripped_v2".to_string(),
                site_path: "crates/routectl-providers/src/anthropic_api/system.rs".to_string(),
            }
        );
    }

    #[test]
    fn the_resolver_rejects_a_real_symbol_cited_against_the_wrong_file() {
        // The homonym trap, made concrete:
        // `strip_thinking_when_tool_choice_forces_use` is defined in BOTH
        // the anthropic-api and the Bedrock-Converse egress. A
        // search-the-whole-tree check cannot tell them apart, so deleting
        // the cited one would leave it green. Pinning the path is what
        // makes the citation falsifiable.
        let symbol = "strip_thinking_when_tool_choice_forces_use";
        let cited = "crates/routectl-providers/src/anthropic_api/extras.rs";
        let homonym = "crates/routectl-providers/src/bedrock/converse/extras.rs";

        // Positive control: the symbol really is defined in BOTH files, so
        // the rejection below is about the path pin and not about the
        // symbol being absent everywhere.
        assert!(resolve_site_symbol(symbol, cited).is_ok());
        assert!(resolve_site_symbol(symbol, homonym).is_ok());

        // A citation pointing at a THIRD file must fail even though the
        // symbol exists elsewhere in the tree.
        let err = resolve_site_symbol(symbol, "crates/routectl-providers/src/anthropic_api/mod.rs")
            .expect_err("the symbol is not defined in mod.rs");

        assert!(
            matches!(err, SymbolError::SymbolNotInSitePath { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn the_resolver_rejects_an_unreadable_site_path() {
        let err = resolve_site_symbol("whatever", "crates/does/not/exist.rs")
            .expect_err("a missing file must not resolve");

        assert_eq!(
            err,
            SymbolError::SitePathUnreadable("crates/does/not/exist.rs".to_string())
        );
    }

    #[test]
    fn a_test_only_definition_does_not_satisfy_a_citation() {
        // This very file defines `fn a_test_only_definition_...`, and the
        // workspace tree includes it. A whole-tree search would resolve a
        // symbol that survives only as a test helper; requiring the cited
        // production path does not.
        let err = resolve_site_symbol(
            "a_test_only_definition_does_not_satisfy_a_citation",
            "crates/routectl-providers/src/anthropic_api/extras.rs",
        )
        .expect_err("a test fn must not satisfy a production citation");

        assert!(
            matches!(err, SymbolError::SymbolNotInSitePath { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn the_definition_counter_distinguishes_absent_unique_and_duplicated() {
        // Backs the ambiguity arm of `resolve_site_symbol`: a citation is
        // only unique if the file defines the symbol exactly once. Driven
        // through the counter directly because no production file in the
        // tree currently defines one symbol twice, and manufacturing one
        // would be a worse test than pinning the arithmetic.
        assert_eq!(count_fn_definitions("fn other() {}", "target"), 0);
        assert_eq!(count_fn_definitions("pub fn target() {}", "target"), 1);
        assert_eq!(
            count_fn_definitions("fn target<T>(x: T) {}", "target"),
            1,
            "the generic form counts"
        );
        assert_eq!(
            count_fn_definitions("fn target() {}\n#[cfg(x)]\nfn target() {}", "target"),
            2,
            "two definitions must be detectable so the citation can be called ambiguous"
        );
        assert_eq!(
            count_fn_definitions("fn target_suffixed() {}", "target"),
            0,
            "a longer name that merely starts with the symbol is not a definition"
        );
    }

    #[test]
    fn a_fixture_provider_kind_resolves_including_the_older_anthropic_spelling() {
        // The vocabulary trap this bridge exists for: a fixture's
        // meta.provider_kind is the in-crate PROVIDER_KIND spelling, in
        // which the Anthropic Messages egress is `anthropic`.
        assert_eq!(
            egress_lane_from_fixture_kind("anthropic").expect("the fixture spelling resolves"),
            EgressLane::AnthropicApi
        );

        // Positive control on the same input through the config-facing
        // resolver: it correctly REJECTS it, which is why the bridge is
        // needed rather than a widened `resolve_egress`.
        assert_eq!(
            resolve_egress("anthropic", None).expect_err("not a kind_str token"),
            LaneError::UnknownEgressKind("anthropic".to_string())
        );

        // Every other provider writes its lane token verbatim.
        for (fixture_kind, expected) in [
            ("openai-compat", EgressLane::OpenaiCompat),
            ("openai-responses", EgressLane::OpenaiResponses),
            ("gemini", EgressLane::Gemini),
            ("bedrock-invoke", EgressLane::BedrockInvoke),
            ("bedrock-converse", EgressLane::BedrockConverse),
        ] {
            assert_eq!(
                egress_lane_from_fixture_kind(fixture_kind).expect("fixture kind resolves"),
                expected,
                "`{fixture_kind}` must resolve to its own lane"
            );
        }
    }

    #[test]
    fn the_fixture_bridge_still_fails_closed() {
        // The bare `bedrock` kind never appears in a fixture (the rig
        // records the shape-split token), so accepting it would invent a
        // shape the capture never stated.
        assert_eq!(
            egress_lane_from_fixture_kind("bedrock").expect_err("no shape stated"),
            LaneError::UnknownEgressKind("bedrock".to_string())
        );
        assert_eq!(
            egress_lane_from_fixture_kind("anthropic-messages").expect_err("invented spelling"),
            LaneError::UnknownEgressKind("anthropic-messages".to_string())
        );
    }

    #[test]
    fn a_fixture_kind_classifies_through_the_shared_equality_rule() {
        // What a consumer holding a FixtureMeta actually does: resolve the
        // egress through the bridge, then apply the same rule `lane_class`
        // applies. Same verdict as the kind_str path for the same lane.
        let egress = egress_lane_from_fixture_kind("anthropic").expect("resolves");
        let ingress = ingress_dialect("anthropic").expect("resolves");

        assert_eq!(
            class_for_dialects(ingress, egress.dialect()),
            LaneClass::Fidelity
        );
        assert_eq!(
            class_for_dialects(ingress, egress.dialect()),
            lane_class("anthropic", "anthropic-api", None).expect("resolves"),
            "the bridge must not produce a different class than the kind_str path"
        );
        assert_eq!(
            class_for_dialects(
                ingress,
                egress_lane_from_fixture_kind("gemini")
                    .expect("resolves")
                    .dialect()
            ),
            LaneClass::Translation
        );
    }

    // ---------- the normalizer ----------

    #[test]
    fn the_system_turn_normalizer_makes_a_length_changed_pair_diff_empty() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Mirrors the normalize-then-diff contract pinned in json_diff:
        // the lift removes turns from the MIDDLE of `.messages`, so every
        // later element shifts index and positional pairing collapses.
        let ingress = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "system", "content": "lifted"},
                {"role": "assistant", "content": "two"},
                {"role": "system", "content": "also lifted"},
                {"role": "user", "content": "three"},
            ],
        });
        let outgoing = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "user", "content": "three"},
            ],
        });

        // Positive control: diffed raw, one explained transform reports a
        // divergence at nearly every surviving index.
        let raw = super::super::diff_all(&outgoing, &ingress, &[]);
        assert_eq!(raw.len(), 6, "got: {raw:?}");

        let normalized = super::super::diff_all(
            &outgoing,
            &normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &ingress),
            &[],
        );

        assert!(normalized.is_empty(), "got: {normalized:?}");
    }

    #[test]
    fn the_normalizer_leaves_a_body_without_system_turns_untouched() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = json!({"messages": [{"role": "user", "content": "one"}]});

        let normalized = normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &body);

        assert_eq!(normalized, body);
    }

    #[test]
    fn the_normalizer_passes_through_a_body_with_no_messages_array() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = json!({"model": "m"});

        assert_eq!(
            normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &body),
            body
        );
    }

    #[test]
    fn a_normalizer_never_explains_a_divergence() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The rule the whole two-kind split exists to enforce: a
        // length-changing transform must not answer the post-diff
        // question, or it becomes a per-index whitelist over `.messages`.
        let entry = exceptions_for_lane(&ANTHROPIC_FIDELITY_LANE)[0];
        let divergence = Divergence {
            path: "messages[1]".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!({"role": "system", "content": "lifted"})),
        };

        assert!(
            (entry.path_predicate)(&divergence.path),
            "the path predicate does cover the subtree it rewrites"
        );
        assert!(
            !entry.matches(&divergence),
            "a normalizer must never excuse a divergence"
        );
    }

    // ---------- the billing-block normalizer ----------

    #[test]
    fn the_billing_normalizer_realigns_a_system_array_the_strip_shrank() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The billing block is the FIRST system element, so removing it
        // shifts every later block: positional pairing then reports a
        // changed text at each surviving index plus a membership
        // divergence at the tail.
        let ingress = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=1.2.3;"},
                {"type": "text", "text": "identity line"},
                {"type": "text", "text": "the long prompt"},
            ],
        });
        let outgoing = json!({
            "system": [
                {"type": "text", "text": "identity line"},
                {"type": "text", "text": "the long prompt"},
            ],
        });

        // Positive control: diffed raw, one explained transform reports
        // three divergences over a three-block array.
        let raw = super::super::diff_all(&outgoing, &ingress, &[]);
        assert_eq!(raw.len(), 3, "got: {raw:?}");

        let normalized = super::super::diff_all(
            &outgoing,
            &normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &ingress),
            &[],
        );

        assert!(normalized.is_empty(), "got: {normalized:?}");
    }

    #[test]
    fn the_billing_normalizer_is_marker_keyed_not_position_keyed() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The production predicates test the block's own text, so a client
        // that sends the billing block SECOND is stripped identically.
        let ingress = json!({
            "system": [
                {"type": "text", "text": "identity line"},
                {"type": "text", "text": "  \n x-anthropic-billing-header: cc_version=1.2.3;"},
            ],
        });

        let normalized = normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &ingress);

        assert_eq!(
            normalized["system"],
            json!([{"type": "text", "text": "identity line"}]),
            "leading whitespace before the marker still matches, at any index",
        );
    }

    #[test]
    fn the_billing_normalizer_erases_no_other_system_block() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The negative that proves the normalizer is billing-SPECIFIC
        // rather than a blanket explanation of system-array shrinkage: a
        // wire body missing a NON-billing block must stay a finding.
        let ingress = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=1.2.3;"},
                {"type": "text", "text": "identity line"},
                {"type": "text", "text": "the long prompt"},
            ],
        });
        // The identity line is gone from the wire too -- not a transform
        // this entry claims.
        let outgoing = json!({
            "system": [
                {"type": "text", "text": "the long prompt"},
            ],
        });

        let residual = super::super::diff_all(
            &outgoing,
            &normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &ingress),
            &[],
        );

        assert!(
            !residual.is_empty(),
            "dropping a non-billing block must survive the normalizer as a divergence",
        );

        // A body whose only marker-free blocks shrink is untouched by the
        // rewrite at all.
        let marker_free = json!({
            "system": [
                {"type": "text", "text": "identity line"},
                {"type": "text", "text": "the long prompt"},
            ],
        });
        assert_eq!(
            normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &marker_free),
            marker_free,
        );
    }

    #[test]
    fn the_billing_normalizer_leaves_an_all_billing_system_alone() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A system array that is NOTHING but the billing block collapses
        // to an ABSENT `system` key upstream. That whole-field drop is a
        // shape this entry has never measured, so the rewrite declines it
        // and the divergence keeps surfacing.
        let body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=1.2.3;"},
            ],
        });

        assert_eq!(
            normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &body),
            body
        );
    }

    #[test]
    fn the_billing_normalizer_passes_through_a_non_array_system() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A flat-string system rides through: the harness runs over
        // whatever a fixture holds, and a shape mismatch is the diff's
        // finding to report.
        for body in [
            json!({"system": "x-anthropic-billing-header: cc_version=1.2.3;"}),
            json!({"model": "m"}),
        ] {
            assert_eq!(
                normalize_ingress_for_lane(&ANTHROPIC_FIDELITY_LANE, &body),
                body
            );
        }
    }

    // ---------- the matchers ----------

    fn matcher(id: &str) -> &'static Exception {
        exceptions_for_lane(&ANTHROPIC_FIDELITY_LANE)
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("no exception `{id}`"))
    }

    #[test]
    fn the_temperature_clamp_matches_a_forced_one_point_zero_and_nothing_else() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("thinking-temperature-clamp");
        let clamped = Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(1.0)),
            expected: None,
        };

        assert!(entry.matches(&clamped));

        // Same path, a value the clamp never produces.
        assert!(!entry.matches(&Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(0.7)),
            expected: None,
        }));
        // Same path and value, but the client sent it too -- that is a
        // rewrite of the caller's sampling, not the clamp adding a field.
        assert!(!entry.matches(&Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!(1.0)),
            expected: Some(json!(0.2)),
        }));
        // An unrelated path carrying the same shape.
        assert!(!entry.matches(&Divergence {
            path: "top_p".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(1.0)),
            expected: None,
        }));
    }

    #[test]
    fn the_model_matcher_matches_a_bracketed_alias_and_nothing_else() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("model-alias-suffix-resolved");

        assert!(entry.matches(&Divergence {
            path: "model".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!("claude-opus-4-8")),
            expected: Some(json!("claude-opus-4-8[1m]")),
        }));

        // A different model entirely: a real routing divergence.
        assert!(!entry.matches(&Divergence {
            path: "model".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!("claude-haiku-4-5")),
            expected: Some(json!("claude-opus-4-8[1m]")),
        }));
        // Suffix on the wrong side: the wire gained the alias marker.
        assert!(!entry.matches(&Divergence {
            path: "model".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!("claude-opus-4-8[1m]")),
            expected: Some(json!("claude-opus-4-8")),
        }));
        // An empty bracket pair proves no alias relationship.
        assert!(!entry.matches(&Divergence {
            path: "model".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!("claude-opus-4-8")),
            expected: Some(json!("claude-opus-4-8[]")),
        }));
        // An unrelated path whose values happen to fit the shape.
        assert!(!entry.matches(&Divergence {
            path: "metadata.user_id".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!("u")),
            expected: Some(json!("u[1m]")),
        }));
    }

    #[test]
    fn the_thinking_matcher_matches_a_dropped_disabled_config_and_nothing_else() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("disabled-thinking-dropped");

        assert!(entry.matches(&Divergence {
            path: "thinking".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!({"type": "disabled"})),
        }));

        // An ACTIVE thinking config vanishing from the wire is a real
        // fidelity loss and must keep surfacing.
        assert!(!entry.matches(&Divergence {
            path: "thinking".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!({"type": "enabled", "budget_tokens": 4096})),
        }));
        // routectl ADDING a disabled config is the opposite transform.
        assert!(!entry.matches(&Divergence {
            path: "thinking".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!({"type": "disabled"})),
            expected: None,
        }));
        // An unrelated path carrying the same value shape.
        assert!(!entry.matches(&Divergence {
            path: "output_config".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!({"type": "disabled"})),
        }));
    }

    #[test]
    fn the_cache_matcher_matches_an_injected_ephemeral_marker_and_nothing_else() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("auto-cache-breakpoint-injected");

        // The three slots placement writes: the top-level terminal marker
        // and the front marker on a system block or a custom tool.
        for path in [
            "cache_control",
            "system[1].cache_control",
            "tools[3].cache_control",
        ] {
            assert!(
                entry.matches(&Divergence {
                    path: path.to_string(),
                    kind: DivergenceKind::Added,
                    actual: Some(json!({"type": "ephemeral", "ttl": "5m"})),
                    expected: None,
                }),
                "`{path}` is a placement slot"
            );
        }
        // A premium TTL is a shape auto-placement never constructs: only
        // `ephemeral_5m()` is assigned, so `1h` is a caller marker or an
        // unreviewed placement change and must keep surfacing.
        assert!(!entry.matches(&Divergence {
            path: "cache_control".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!({"type": "ephemeral", "ttl": "1h"})),
            expected: None,
        }));
        // Nor is an omitted TTL: `ephemeral_5m()` carries `Some("5m")`, and
        // the field skips serialization only when it is `None`.
        assert!(!entry.matches(&Divergence {
            path: "cache_control".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!({"type": "ephemeral"})),
            expected: None,
        }));

        // A cache-control kind routectl never injects: the type is what
        // carries the precision, so this must keep surfacing.
        assert!(!entry.matches(&Divergence {
            path: "cache_control".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!({"type": "permanent", "ttl": "5m"})),
            expected: None,
        }));
        // Same path, no type at all.
        assert!(!entry.matches(&Divergence {
            path: "cache_control".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!({"ttl": "5m"})),
            expected: None,
        }));
        // routectl REWRITING a caller's marker is a different transform:
        // injection is withheld entirely when the caller supplied one.
        assert!(!entry.matches(&Divergence {
            path: "cache_control".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!({"type": "ephemeral", "ttl": "5m"})),
            expected: Some(json!({"type": "ephemeral", "ttl": "1h"})),
        }));
        // The wire DROPPING a caller's marker is wire loss, not injection.
        assert!(!entry.matches(&Divergence {
            path: "system[1].cache_control".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!({"type": "ephemeral", "ttl": "5m"})),
        }));
        // Paths auto-placement never writes, including the one
        // caller-controlled subtree that can carry the key name.
        for path in [
            "messages[0].content[0].cache_control",
            "tools[0].input_schema.properties.cache_control",
            "system.cache_control",
            "system[].cache_control",
            "cache_control.ttl",
            "metadata.cache_control",
        ] {
            assert!(
                !entry.matches(&Divergence {
                    path: path.to_string(),
                    kind: DivergenceKind::Added,
                    actual: Some(json!({"type": "ephemeral", "ttl": "5m"})),
                    expected: None,
                }),
                "`{path}` is not a placement slot"
            );
        }
    }

    #[test]
    fn the_sampling_strip_matches_both_dropped_keys_and_nothing_else() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("oauth-sampling-stripped");

        // The production strip removes the PAIR, so both keys match. The
        // value is not part of the claim: whatever the caller sent, the
        // seat-driven strip drops it.
        for (path, sent) in [
            ("temperature", json!(1)),
            ("temperature", json!(0.2)),
            ("top_p", json!(0.9)),
        ] {
            assert!(
                entry.matches(&Divergence {
                    path: path.to_string(),
                    kind: DivergenceKind::Removed,
                    actual: None,
                    expected: Some(sent.clone()),
                }),
                "`{path}` carrying {sent} is what the strip removes"
            );
        }

        // A sampling key the strip leaves alone: the seat accepts it, so
        // its loss is real.
        assert!(!entry.matches(&Divergence {
            path: "top_k".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!(40)),
        }));
        // A REWRITE of the caller's value is not this transform -- the
        // strip removes the key outright.
        assert!(!entry.matches(&Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!(1.0)),
            expected: Some(json!(0.2)),
        }));
        // An ADDED temperature stays `thinking-temperature-clamp`'s
        // territory: that entry is a value the wire GAINS, this one a
        // value the wire LOSES.
        assert!(!entry.matches(&Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(1.0)),
            expected: None,
        }));
        // A nested key that merely ends in one of the two names.
        assert!(!entry.matches(&Divergence {
            path: "provider_extras.temperature".to_string(),
            kind: DivergenceKind::Removed,
            actual: None,
            expected: Some(json!(0.5)),
        }));
    }

    // ---------- the per-fixture eligibility gate ----------

    /// A minimal fixture carrying only the outgoing headers the gate reads.
    /// Bodies are irrelevant here: eligibility is decided before any
    /// divergence is looked at.
    fn fixture_with_outgoing_headers(headers: &[(&str, &str)]) -> super::super::loader::Fixture {
        use super::super::loader::{FIXTURE_SCHEMA_VERSION, FixtureClient, FixtureMeta};
        super::super::loader::Fixture {
            name: "synthetic".to_string(),
            ingress_request: json!({}),
            ingress_request_headers: Vec::new(),
            outgoing_request: json!({}),
            outgoing_request_headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect(),
            upstream_response_bytes: Vec::new(),
            upstream_response_headers: Vec::new(),
            egress_response_bytes: Vec::new(),
            egress_response_headers: Vec::new(),
            meta: FixtureMeta {
                schema_version: FIXTURE_SCHEMA_VERSION,
                provider_kind: "anthropic".to_string(),
                lane: String::new(),
                ingress_kind: "anthropic".to_string(),
                case_id: "synthetic".to_string(),
                config_sha: String::new(),
                wire_pattern: String::new(),
                client: FixtureClient::default(),
                stream: false,
                model: None,
                routectl_version: None,
            },
        }
    }

    #[test]
    fn the_sampling_entry_is_eligible_only_for_a_bearer_credential_capture() {
        let entry = matcher("oauth-sampling-stripped");

        // The committed capture's shape: the scrub keeps the scheme.
        assert!(entry.eligible_for(&fixture_with_outgoing_headers(&[
            ("anthropic-version", "2023-06-01"),
            ("authorization", "Bearer [REDACTED]"),
        ])));
        // Case-insensitive on both the name and the scheme, per RFC 7235.
        assert!(entry.eligible_for(&fixture_with_outgoing_headers(&[(
            "Authorization",
            "bEaReR [REDACTED]"
        )])));

        // An api-key egress: the OAuth-only strip could not have run.
        assert!(!entry.eligible_for(&fixture_with_outgoing_headers(&[
            ("anthropic-version", "2023-06-01"),
            ("x-api-key", "[REDACTED]"),
        ])));
        // No outgoing headers at all.
        assert!(!entry.eligible_for(&fixture_with_outgoing_headers(&[])));
        // A different scheme whose value merely mentions the word.
        assert!(!entry.eligible_for(&fixture_with_outgoing_headers(&[(
            "authorization",
            "Basic bearer-looking-value"
        )])));
        // The scheme as the whole value, with no credential after it: not
        // the `Bearer <token>` shape a capture presents.
        assert!(!entry.eligible_for(&fixture_with_outgoing_headers(&[(
            "authorization",
            "Bearer"
        )])));
        // The ingress credential says nothing about what routectl re-signed
        // the request with, so it must not satisfy the gate.
        let ingress_only = super::super::loader::Fixture {
            ingress_request_headers: vec![(
                "authorization".to_string(),
                "Bearer [REDACTED]".to_string(),
            )],
            ..fixture_with_outgoing_headers(&[("x-api-key", "[REDACTED]")])
        };
        assert!(!entry.eligible_for(&ingress_only));
    }

    #[test]
    fn an_ungated_entry_is_eligible_for_every_fixture_on_its_lane() {
        // The default the other six entries rely on: no `applies_to` hook
        // means the lane key alone decides, which is the behavior they were
        // measured under. The billing strip specifically: its production
        // call is on the always-run normalize path, so an api-key capture
        // must stay adjudicable.
        let api_key = fixture_with_outgoing_headers(&[("x-api-key", "[REDACTED]")]);
        for entry in exceptions_for_lane(&ANTHROPIC_FIDELITY_LANE)
            .into_iter()
            .filter(|e| e.id != "oauth-sampling-stripped")
        {
            assert!(
                entry.applies_to.is_none(),
                "`{}` gained a fixture gate; the seven-entry table's gating is \
                 asserted here so a new gate is a review moment",
                entry.id
            );
            assert!(entry.eligible_for(&api_key), "`{}`", entry.id);
        }
    }

    #[test]
    fn only_matchers_carry_a_fixture_gate() {
        // The normalizer seam takes a BODY, not a fixture, so a gate
        // declared on a normalizer would be silently ignored -- an
        // exception that reads as scoped while applying to everything.
        for entry in all_exceptions() {
            assert!(
                entry.applies_to.is_none() || entry.kind() == ExceptionKind::Matcher,
                "`{}` is a normalizer carrying an `applies_to` gate that nothing consults",
                entry.id,
            );
        }
    }

    #[test]
    fn the_cardinality_bound_stops_explaining_past_its_limit() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("auto-cache-breakpoint-injected");
        let marker = |path: &str| Divergence {
            path: path.to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!({"type": "ephemeral", "ttl": "5m"})),
            expected: None,
        };
        assert_eq!(entry.max_per_fixture, Some(2));

        // POSITIVE CONTROL: the two production slots are explained and
        // counted, so the third's rejection is the bound and not the shape.
        let before = entry.matched_count();
        let two = vec![marker("cache_control"), marker("system[0].cache_control")];
        assert!(unexplained(&ANTHROPIC_FIDELITY_LANE, &two).is_empty());
        assert_eq!(entry.matched_count() - before, 2);

        let before = entry.matched_count();
        let three = vec![
            marker("cache_control"),
            marker("system[0].cache_control"),
            marker("system[1].cache_control"),
        ];

        let residual = unexplained(&ANTHROPIC_FIDELITY_LANE, &three);

        assert_eq!(residual.len(), 1, "got: {residual:?}");
        assert_eq!(
            entry.matched_count() - before,
            2,
            "a match past the bound must not inflate the counter",
        );
    }

    // ---------- counters and adjudication ----------

    #[test]
    fn a_match_increments_the_entrys_own_counter() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("thinking-temperature-clamp");
        let before = entry.matched_count();

        assert!(entry.matches(&Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(1.0)),
            expected: None,
        }));

        assert!(
            entry.matched_count() > before,
            "a matched divergence must be counted so a zero-match entry is detectable"
        );
    }

    #[test]
    fn a_non_match_leaves_the_counter_alone() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("disabled-thinking-dropped");
        let before = entry.matched_count();

        assert!(!entry.matches(&Divergence {
            path: "max_tokens".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!(1)),
            expected: Some(json!(2)),
        }));

        assert_eq!(entry.matched_count(), before);
    }

    #[test]
    fn a_zero_match_gate_must_read_the_delta_not_the_global_counter() {
        // The counters are process-global statics shared by every test in
        // this binary, so a consumer that reads `matched_count()` directly
        // sees hits some other walk contributed. This demonstrates the
        // discipline the module docs require: snapshot, walk, subtract.
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = matcher("model-alias-suffix-resolved");
        let unrelated = vec![Divergence {
            path: "max_tokens".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!(1024)),
            expected: Some(json!(4096)),
        }];

        let before = entry.matched_count();
        let _ = unexplained(&ANTHROPIC_FIDELITY_LANE, &unrelated);
        let delta = entry.matched_count() - before;

        assert_eq!(
            delta, 0,
            "this walk exercised nothing for this entry, so its DELTA is zero \
             regardless of what earlier tests left on the global counter"
        );

        // Positive control: a walk that DOES exercise the entry moves the
        // delta, so the zero above is a real signal and not a constant.
        let matching = vec![Divergence {
            path: "model".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!("claude-opus-4-8")),
            expected: Some(json!("claude-opus-4-8[1m]")),
        }];

        let before = entry.matched_count();
        let residual = unexplained(&ANTHROPIC_FIDELITY_LANE, &matching);

        assert!(residual.is_empty(), "got: {residual:?}");
        assert_eq!(entry.matched_count() - before, 1);
    }

    #[test]
    fn unexplained_keeps_only_the_divergences_no_exception_covers() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let explained = Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(1.0)),
            expected: None,
        };
        let real_loss = Divergence {
            path: "max_tokens".to_string(),
            kind: DivergenceKind::Changed,
            actual: Some(json!(1024)),
            expected: Some(json!(4096)),
        };
        let divergences = vec![explained, real_loss.clone()];

        let residual = unexplained(&ANTHROPIC_FIDELITY_LANE, &divergences);

        assert_eq!(residual, vec![&real_loss], "got: {residual:?}");
    }

    #[test]
    fn a_lane_with_no_registered_exceptions_explains_nothing() {
        let _guard = super::COUNTER_DELTA_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Positive control for the lane keying: the same divergence the
        // anthropic lane explains is unexplained on a lane that never
        // claimed the transform.
        let gemini_lane = LaneKey {
            ingress: "anthropic",
            egress: "gemini",
        };
        let divergences = vec![Divergence {
            path: "temperature".to_string(),
            kind: DivergenceKind::Added,
            actual: Some(json!(1.0)),
            expected: None,
        }];

        assert!(exceptions_for_lane(&gemini_lane).is_empty());
        assert_eq!(unexplained(&gemini_lane, &divergences).len(), 1);
        assert!(unexplained(&ANTHROPIC_FIDELITY_LANE, &divergences).is_empty());
    }
}
