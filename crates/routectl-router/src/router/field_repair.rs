//! Reactive L0 envelope-field repair: the closed path-to-surface table, the
//! request-side drop, and the admission that binds a repair to the two-phase
//! field guard.
//!
//! An upstream that rejects a request by naming a WIRE FIELD PATH states
//! that the lane does not accept that field. The L0 repair is the whole of
//! the response: DROP the mapped field from the attempt request and
//! re-dispatch the same target once. There is no rewrite, no substituted
//! value, and no second class -- an L0 repair either removes one field or
//! does nothing at all.
//!
//! # Why the table is closed, and what "closed" buys
//!
//! A path an upstream names is an arbitrary string. Acting on it generically
//! would mean deriving a mutation from untrusted input, on the dispatch
//! path, against a permanent verdict namespace. So the mapping from path to
//! request-side surface is a CLOSED table of code-authored rows: a path with
//! no row mutates nothing and performs no repair, whatever the rejection
//! said. Each row names one concrete request-side surface whose removal is
//! known to remove that wire field, so the repair's effect is reviewable
//! rather than inferred.
//!
//! # Why nothing resolves in production yet
//!
//! The repair needs a path, and the only producer of one is a rejection
//! parser that is deliberately NOT part of this stage: the capability
//! matcher's feature-naming tables ship empty by their own stated
//! discipline (no entry without a real captured envelope), and the envelope
//! that was captured names a wire field path plus an enum rather than a
//! feature in prose -- the shape those tables cannot parse. So
//! [`rejected_field_path`] resolves NOTHING on real traffic, by
//! construction, and every arm downstream of it is inert in production.
//!
//! That is the intended Stage 1 state, not a gap to close here: the
//! namespace, the guard, the budget, the settlement and the persistence are
//! what this stage lands and what a captured envelope later switches on. The
//! coverage that keeps them honest injects a PROVISIONAL resolution through
//! the test-only seam in [`provisional`], and a control asserts the
//! production resolver stays inert for every rejection shape a test can
//! build.

use std::time::Instant;

use routectl_core::Error;
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatRequest, sanitize_for_log};

use crate::field_verdict::{FieldRepairGuard, FieldVerdictKey};

use super::repair_budget::RepairBudget;

use super::{CapabilityClearedEvent, CapabilityLearnEvent, DispatchMeta, DispatchTarget, Router};

/// The one provider kind Stage 1 acts on. Matches `ProviderEntry::kind_str`,
/// so it round-trips with the `kind = "..."` discriminant in the operator's
/// provider table.
///
/// Every other kind is excluded here rather than downstream, which is what
/// keeps the Bedrock exclusion a property of this arm instead of a property
/// of the shared capability-key normalizer: that lane's normalization
/// rewrites a dotted key, and the field namespace is permanent, so a lane
/// whose keys would be rewritten must never mint one. The verdict key
/// refuses such a lane independently ([`FieldVerdictKey::new`]) -- two
/// independent refusals for one irreversible mistake.
pub(super) const ANTHROPIC_API_KIND: &str = "anthropic-api";

/// Action token for the field-repair WARN: the L0 repair dropped the mapped
/// envelope field and re-dispatched the same target once. Closed set.
pub(super) const FIELD_ACTION_DROP_REPAIR: &str = "field_drop_repair";

/// Reason token: the attempt drew an upstream rejection naming this field
/// path. Closed set.
pub(super) const FIELD_REASON_UPSTREAM_REJECTION: &str = "upstream_field_rejection";

/// The request-side surface whose removal drops one wire field.
///
/// A variant is a code-authored fact about how a canonical request produces
/// a wire field, so a repair's effect can be reviewed at the surface rather
/// than inferred from a path string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldSurface {
    /// The Anthropic `thinking.display` wire field.
    ///
    /// TWO canonical carriers produce it and both must go, or the drop is a
    /// false claim: the egress reads the verbatim ingress carrier FIRST and
    /// falls back to deriving the value from the canonical `exclude`
    /// boolean, so clearing only the carrier leaves a request that still
    /// emits the field with a different value -- the same rejection, plus a
    /// wasted repair.
    ///
    /// Neither carrier gates whether thinking is requested at all (that
    /// reads `enabled` / `max_tokens` / `effort`), so dropping both removes
    /// the rejected field and nothing else.
    AnthropicThinkingDisplay,
    /// Test-only: reports itself PRESENT and removes NOTHING.
    ///
    /// The real surface's presence check and drop read the same two carriers, so
    /// they cannot disagree -- which leaves `apply`'s fail-closed branch
    /// unreachable, and an unreachable safety branch is exactly the kind that
    /// rots into a silent charge for a repair the wire never saw. This variant
    /// manufactures the divergence so that branch can be exercised, in RELEASE
    /// where the debug assertion is compiled out and only the `return None`
    /// remains.
    ///
    /// Gated to match its only consumer: in a DEBUG test build the release test
    /// is compiled out, so an ungated variant here would be dead code.
    #[cfg(all(test, not(debug_assertions)))]
    DivergentForTests,
    /// Test-only surface that MUTATES and then reports failure.
    ///
    /// Distinct from `DivergentForTests`, which removes nothing: a surface that
    /// removes nothing cannot distinguish a transform applied to a scratch
    /// clone from one applied in place, so it cannot pin the pre-flight
    /// planner's byte-safety contract at all (measured -- reverting the scratch
    /// clone left every assertion green against the divergent variant). This
    /// one drops one of the two carriers and then returns `false`, which is the
    /// actual hazard: an in-place transform on that path hands its caller a
    /// half-mutated body while reporting it did nothing.
    ///
    /// Gated to `test` alone rather than `not(debug_assertions)` too, because
    /// the byte-safety property it exists to pin holds in every build.
    #[cfg(test)]
    PartialDropForTests,
    /// Test-only PREFIX-IMPACTING surface: a system prompt whose text is the
    /// exact sentinel [`TEST_PREFIX_SENTINEL`].
    ///
    /// The closed table ships no prefix-impacting row today, so the content
    /// gates that class is subject to -- the confirmation quorum and the
    /// target opt-in -- would have no row to act on and every one of their
    /// branches would be unreachable. An unreachable gate is one whose
    /// removal no test would notice, which on this surface means a
    /// prefix-impacting rewrite could later ship with the gates silently
    /// inert.
    ///
    /// Dropping the system prompt is a genuine cache-prefix rewrite rather
    /// than a stand-in for one, so a decision made about it exercises the
    /// same classification a real content row will carry. It is also
    /// DISJOINT from the envelope surface's two carriers, which is what lets
    /// one request carry both classes at once and pin that each is gated on
    /// its own terms -- and, on the reactive side, that a rejection naming
    /// the LATER row is attributed to that row rather than starved by the
    /// earlier one.
    ///
    /// Presence is an EXACT match on that sentinel, not "the request has a
    /// system prompt". A broad predicate would make every unrelated fixture
    /// anywhere in the test suite that happens to set `system` carry a
    /// closed-table row -- so those fixtures would silently acquire pre-flight
    /// decisions, feature keys, and reactive candidates that production cannot
    /// produce, and a test asserting "one decision" would be asserting about a
    /// row it never meant to involve. The sentinel confines this row to the
    /// tests that deliberately opt into it.
    #[cfg(test)]
    PrefixImpactingSystemForTests,
    /// Test-only surface that reports a SUCCESSFUL removal while its own
    /// presence predicate still reports the field present.
    ///
    /// The third distinct shape, and the only one that reaches the pre-flight
    /// planner's POST-CONDITION branch: `PartialDropForTests` fails the
    /// `drop_from` check and never gets that far, and the grounded surface
    /// passes both. Without this variant the post-condition is unreachable, and
    /// an unreachable safety check is one whose removal no test would notice.
    ///
    /// It removes nothing and returns `true`, which is the hazard in question:
    /// a transform claiming success over a field the request still carries
    /// would have the planner report `field_preflight_drop` for a body that
    /// still emits the field to the upstream.
    #[cfg(test)]
    ClaimsRemovalForTests,
}

impl FieldSurface {
    /// Whether this surface currently contributes its wire field to `req`.
    ///
    /// Checked BEFORE a repair is admitted: a rejection naming a field the
    /// attempt does not carry is not repairable by dropping it, and a
    /// request that mutates nothing must not claim a guard slot, spend
    /// budget, or learn.
    pub(super) fn present_in(&self, req: &ChatRequest) -> bool {
        match self {
            Self::AnthropicThinkingDisplay => {
                req.routectl_internal.anthropic_thinking_display.is_some()
                    || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
            }
            #[cfg(all(test, not(debug_assertions)))]
            Self::DivergentForTests => true,
            #[cfg(test)]
            Self::PartialDropForTests => req.routectl_internal.anthropic_thinking_display.is_some(),
            #[cfg(test)]
            Self::PrefixImpactingSystemForTests => {
                matches!(
                    req.system.as_ref(),
                    Some(routectl_core::SystemContent::Text(text)) if text == TEST_PREFIX_SENTINEL
                )
            }
            // Always present: that is the point -- a successful drop still
            // leaves this surface readable.
            #[cfg(test)]
            Self::ClaimsRemovalForTests => true,
        }
    }

    /// Drop this surface from `req`, returning whether anything was removed.
    pub(super) fn drop_from(&self, req: &mut ChatRequest) -> bool {
        match self {
            Self::AnthropicThinkingDisplay => {
                let carrier = req
                    .routectl_internal
                    .anthropic_thinking_display
                    .take()
                    .is_some();
                let derived = req
                    .reasoning
                    .as_mut()
                    .and_then(|r| r.exclude.take())
                    .is_some();
                carrier || derived
            }
            #[cfg(all(test, not(debug_assertions)))]
            Self::DivergentForTests => false,
            #[cfg(test)]
            Self::PartialDropForTests => {
                // Mutates, then reports failure -- the hazard the scratch
                // clone exists to contain.
                req.routectl_internal.anthropic_thinking_display.take();
                false
            }
            #[cfg(test)]
            Self::PrefixImpactingSystemForTests => {
                // Guarded by the SAME exact-sentinel predicate the presence
                // check uses, so this surface can never remove a system prompt
                // some unrelated fixture happened to set.
                if !Self::PrefixImpactingSystemForTests.present_in(req) {
                    return false;
                }
                req.system.take().is_some()
            }
            // Removes nothing and claims success -- the hazard the
            // post-condition re-check exists to catch.
            #[cfg(test)]
            Self::ClaimsRemovalForTests => true,
        }
    }
}

/// What a transform's removal costs the request beyond the field itself.
///
/// The taxonomy is TRANSFORM-KEYED: a row carries its own class, and the
/// class is what decides which gates a pre-flight application of that row
/// must clear. Nothing infers a class from a path string, so a new row
/// states its cost explicitly rather than inheriting one by resemblance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TransformClass {
    /// Removal touches request ENVELOPE metadata only: nothing the upstream
    /// hashes into a cache prefix, and nothing the model reads as content.
    Envelope,
    /// Removal rewrites content the upstream hashes into its cache prefix,
    /// so every applied request pays a full prefix recompute and a wrong
    /// verdict degrades every later request on that lane silently.
    ///
    /// Not constructed by a GROUNDED row in this build: the closed table
    /// ships one envelope row, and the first prefix-impacting row lands with
    /// the captured evidence that grounds it. The class, its gates, and
    /// their enforcement ship now rather than alongside that row, so the
    /// row's arrival is a table edit rather than a new gate -- the scoped
    /// allowance is what that costs, and the change adding the row removes
    /// it. The variant IS constructed in a test build, by the test-only row
    /// that keeps every branch of those gates reachable.
    #[cfg_attr(not(test), allow(dead_code))]
    PrefixImpacting,
}

impl TransformClass {
    /// Stable, closed-set token for a decision record and its diagnostic.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Envelope => "envelope",
            Self::PrefixImpacting => "prefix_impacting",
        }
    }

    /// Confirmed repair cycles an acting verdict needs before a pre-flight
    /// application of this class may run.
    ///
    /// Both values are code constants co-located in [`crate::config`] beside
    /// the canary cadence, never operator knobs: a quorum tuned per deployment
    /// turns a safety parameter into a support surface. Their ORDER is enforced
    /// beside their definitions by anonymous `const _: () = assert!(...)` items,
    /// which rustc evaluates whenever it compiles the crate -- so nothing here
    /// references them, and nothing needs to. Those assertions catch an edit that
    /// VIOLATES the order, not every edit: an order-preserving retune compiles,
    /// and the exact values are pinned by
    /// `the_three_quorum_values_are_exactly_one_one_and_two`.
    ///
    /// What a test CAN pin, and what
    /// `each_class_consumes_the_co_located_quorum_constant_for_its_own_tier`
    /// does, is the WIRING: that each arm reads its own tier's constant. That is
    /// a consumer mistake the compile-time check cannot see, because swapping the
    /// arms leaves both literals -- and so the relation between them -- intact.
    pub(super) const fn required_quorum(self) -> u32 {
        match self {
            Self::Envelope => crate::config::ENVELOPE_QUORUM,
            Self::PrefixImpacting => crate::config::PREFIX_QUORUM,
        }
    }

    /// Whether this class additionally requires an explicit `[fidelity]`
    /// target opt-in. Prefix-impacting repair stays dormant until the
    /// operator names the target; envelope repair never needs one.
    pub(super) const fn requires_target_opt_in(self) -> bool {
        matches!(self, Self::PrefixImpacting)
    }
}

/// One row of the closed path-to-surface table: the qualified dotted path,
/// the request-side surface whose removal drops it, and the class that
/// decides which gates a pre-flight application must clear.
#[derive(Clone, Copy, Debug)]
pub(super) struct FieldRepairRow {
    pub(super) path: &'static str,
    pub(super) surface: FieldSurface,
    pub(super) class: TransformClass,
}

/// The CLOSED path-to-surface table: one row per qualified dotted path this
/// build knows how to repair.
///
/// One grounded row today. The path is the one a real captured rejection
/// envelope named, and its surface is the canonical carrier pair that
/// produces that wire field. A path outside this table resolves to no row,
/// so it mutates nothing and repairs nothing -- the table is the whole of
/// what an L0 repair can act on.
const FIELD_REPAIRS: &[FieldRepairRow] = &[FieldRepairRow {
    path: "thinking.enabled.display",
    surface: FieldSurface::AnthropicThinkingDisplay,
    class: TransformClass::Envelope,
}];

/// The qualified dotted path of the test-only prefix-impacting row. Outside
/// the grounded table, so it can never collide with a real rejection's path.
#[cfg(test)]
pub(in crate::router) const TEST_PREFIX_IMPACTING_PATH: &str = "system.prompt.for.tests";

/// The EXACT system-prompt text the test-only prefix-impacting row's surface
/// recognizes.
///
/// A sentinel rather than "any system prompt" so this row stays confined to the
/// tests that opt into it: a broad predicate would give every fixture in the
/// suite that sets `system` a closed-table row it never asked for, and the
/// decisions, feature keys, and reactive candidates that follow from one.
/// Deliberately not a plausible prompt -- a fixture cannot match it by accident.
#[cfg(test)]
pub(in crate::router) const TEST_PREFIX_SENTINEL: &str =
    "routectl-test-only-prefix-impacting-surface-sentinel";

/// Rows a TEST build adds to the closed table.
///
/// The grounded table ships no prefix-impacting row, so without this the
/// content gates that class is subject to would have no row to act on and
/// every branch of them would be unreachable -- and an unreachable gate is
/// one whose removal no test would notice. Kept as its own table rather than
/// a `cfg`'d element inside [`FIELD_REPAIRS`] so the grounded table's SIZE
/// stays assertable: a new permanent row must still be a deliberate edit
/// that updates the guard on that count.
#[cfg(test)]
const TEST_FIELD_REPAIRS: &[FieldRepairRow] = &[FieldRepairRow {
    path: TEST_PREFIX_IMPACTING_PATH,
    surface: FieldSurface::PrefixImpactingSystemForTests,
    class: TransformClass::PrefixImpacting,
}];

/// The whole closed table this build scans, grounded rows first.
#[cfg(not(test))]
pub(super) const fn closed_table() -> &'static [FieldRepairRow] {
    FIELD_REPAIRS
}

/// Test build: the grounded rows plus [`TEST_FIELD_REPAIRS`], in that order, so
/// a grounded row keeps its production precedence and the added row sits behind
/// it.
///
/// The divergent surface is deliberately NOT a row here: its presence predicate
/// answers `true` unconditionally, so a row for it would make every request in
/// a test build carry it. It reaches `apply`'s fail-closed branch through
/// [`FieldRepairPlan::apply_candidate`] instead.
#[cfg(test)]
pub(super) fn closed_table() -> &'static [FieldRepairRow] {
    use std::sync::LazyLock;

    static ROWS: LazyLock<Vec<FieldRepairRow>> = LazyLock::new(|| {
        FIELD_REPAIRS
            .iter()
            .chain(TEST_FIELD_REPAIRS)
            .copied()
            .collect()
    });
    &ROWS
}

/// The closed table's row for `path` -- the table's OWN path literal plus its
/// surface -- or `None` for a path this build has no grounded repair for.
///
/// THE single lookup, and the whole of the table's refusal: a repair fires only
/// when a rejection's path equals a row's, so an unknown path resolves to no
/// row and therefore to no mutation. Returning the table's literal rather than
/// the caller's string is what keeps upstream bytes out of every downstream
/// consumer, including the operator-facing WARN.
pub(super) fn closed_table_row(path: &str) -> Option<(&'static str, FieldSurface)> {
    closed_table()
        .iter()
        .find_map(|row| (row.path == path).then_some((row.path, row.surface)))
}

/// EVERY closed-table row whose surface `req` carries, in table order.
///
/// The scan shared by the pre-flight planner (which considers every present
/// row, each gated on its own terms) and the reactive admission (which
/// repairs one row per attempt, but must be able to attribute a rejection to
/// ANY present row rather than only the first -- see
/// [`Router::plan_field_carry`]).
///
/// Table order is the ENUMERATION order and nothing else: each row's
/// decision is computed from that row's own class, verdict, and gates, so
/// reordering the table reorders the records without changing any of them.
pub(super) fn present_rows(req: &ChatRequest) -> impl Iterator<Item = FieldRepairRow> + '_ {
    closed_table()
        .iter()
        .filter(|row| row.surface.present_in(req))
        .copied()
}

/// The request-side field-verdict feature keys `req` currently grounds.
///
/// Widens the chain's feature-key vocabulary with the `field:<path>` keys the
/// closed table can mint today, so an acting field verdict participates in
/// the existing stable partition
/// ([`super::feature_filter::filter_chain_by_features`]) the same way a
/// catalog capability does, through the same table and the same
/// carrier-presence check a repair itself uses -- no second detector, no
/// duplicated carrier logic. A request grounding no closed-table surface
/// yields an empty vector, leaving every caller's derived key list exactly
/// as it was before this function existed.
pub(super) fn grounded_field_feature_keys(req: &ChatRequest) -> Vec<String> {
    present_rows(req)
        .filter_map(|row| crate::field_capability::field_capability_key(row.path))
        .collect()
}

/// The full act-side feature-key vocabulary for `req`: the pure catalog keys
/// [`crate::feature_keys::derive_feature_keys`] derives, plus the one
/// grounded field key [`grounded_field_feature_keys`] can mint for it.
///
/// This is the widened vocabulary every routing, learn, observe, and count
/// call site that must treat a catalog capability and a field verdict alike
/// uses. The diagnostic-only feature-naming-drift path is deliberately NOT
/// one of those call sites: a field verdict is never a feature-naming-table
/// candidate, so that diagnostic stays on the bare catalog derivation.
pub(super) fn request_feature_keys(req: &ChatRequest) -> Vec<String> {
    crate::feature_keys::derive_feature_keys(
        req.tools.as_deref().unwrap_or(&[]),
        req.provider_extras.as_ref(),
        req.response_format.as_ref(),
    )
    .into_iter()
    .chain(grounded_field_feature_keys(req))
    .collect()
}

/// The CLOSED-TABLE path an upstream rejection named, or `None` when this
/// build can attribute none -- which is EVERY real rejection in this stage.
///
/// Returns the table's own `&'static str`, never a string built from upstream
/// text: a repair can only ever act on a table row, so resolving through the
/// table here means no caller downstream can hold a path the table does not
/// know. That is also what keeps upstream bytes out of the operator-facing
/// WARN, which renders this literal.
///
/// The class is read FIRST, so no parser is consulted for a class that cannot
/// carry a field rejection -- the future parser's input set is the same as
/// today's.
fn rejected_field_path(
    class: &FailureClass,
    err: &Error,
    provider_kind: &str,
) -> Option<&'static str> {
    if !class_can_name_a_field(class) {
        return None;
    }
    closed_table_row(&parse_rejected_field_path(err, provider_kind)?).map(|(path, _)| path)
}

/// Parse the qualified dotted field path a rejection envelope names.
///
/// PRODUCTION IMPLEMENTATION, and it resolves nothing by construction: the
/// envelope-to-path parser is out of this stage (see the module docs), so there
/// is no producer of a path on real traffic. The arguments are taken and named
/// because this signature is the seam a grounded parser lands behind -- no
/// caller changes when it does.
#[cfg(not(test))]
#[expect(
    clippy::missing_const_for_fn,
    reason = "const only because the body is a placeholder; a grounded parser reads the \
              error body and allocates, so marking it const now would have to be undone by \
              the change that fills it in"
)]
fn parse_rejected_field_path(err: &Error, provider_kind: &str) -> Option<String> {
    let _ = (err, provider_kind);
    None
}

/// Test build: the provisional resolution stands in for the absent parser, so
/// every arm downstream of it is reachable. Substituted at exactly this seam
/// and nowhere else, which is why the production body above stays the single
/// thing a grounded parser has to fill in -- and why the control beside it can
/// assert production inertness against the same rejection shapes.
#[cfg(test)]
fn parse_rejected_field_path(err: &Error, provider_kind: &str) -> Option<String> {
    let _ = (err, provider_kind);
    provisional::injected_path()
}

/// Whether a failure class can carry a field-naming rejection at all.
///
/// A field rejection is a caller-shaped 4xx: the upstream read the envelope
/// and refused one field in it. Every other class -- auth, rate limiting,
/// transport, overload, server error -- says nothing about the envelope's
/// shape, so a repair driven by one would drop a field over an unrelated
/// fault and then learn a permanent negative from it.
const fn class_can_name_a_field(class: &FailureClass) -> bool {
    matches!(class, FailureClass::BadRequest)
}

/// Whether a dispatch walk can SETTLE an envelope-field verdict.
///
/// A settlement mutates the shared learned registry and returns an event row
/// its caller drains to the capability-event ledger. A walk whose caller keeps
/// no such row cannot settle: the mutation would persist while the row was
/// dropped, which is the state a warm rebuild resurrects a verdict from. So
/// the capability is a REQUIRED, explicit argument at the admission -- a new
/// call site has to state which it is, and the non-settling answer makes the
/// whole arm inert rather than trusting a caller to drop the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldSettlementMode {
    /// The caller drains `DispatchMeta`'s learned / cleared rows to the
    /// ledger, so this walk may persist and clear verdicts.
    Settling,
    /// The caller discards the dispatch metadata, so this walk must not touch
    /// resident state: it computes and may repair for the answer it returns,
    /// and learns nothing either way.
    NonSettling,
}

/// The admitted L0 repair for one attempt: EVERY closed-table row the attempt
/// carries, each with the two-phase guard whose settlement the dispatch arm
/// owes, plus which row (if any) the arm actually repaired.
///
/// One candidate per present row rather than only the first, because the
/// rejection decides WHICH row is repaired and the upstream can name any of
/// them: an admission holding only the first present row starves a rejection
/// naming a later one -- the arm would find the plan's row unnamed, leave the
/// rejection on its ordinary terminal path, and the repairable field would
/// never be repaired on that lane. The one-repair-per-attempt ceiling is
/// unchanged: exactly one candidate can be applied, and applying it RELEASES
/// every other candidate's guard (see [`Self::apply`]).
///
/// Holding this is the PROVISIONAL phase -- nothing is persisted while it
/// lives. The dispatch arm settles it exactly once: [`Self::commit`] when
/// the repaired retry succeeded, [`Self::settle_success`] when the
/// unrepaired attempt succeeded outright, and release-by-drop on every other
/// exit (a repeat rejection, an unrelated error, a fallback hop, a client
/// disconnect), which learns nothing and strands no slot.
pub(super) struct FieldRepairPlan<'a> {
    candidates: Vec<FieldRepairCandidate<'a>>,
    /// Index into `candidates` of the row a successful [`Self::apply`] dropped.
    /// `None` until one succeeds, which is what makes the one-repair ceiling a
    /// property of the plan rather than of a caller's flag alone.
    repaired: Option<usize>,
}

/// One admitted row: the closed-table path and surface, plus the guard whose
/// settlement this row owes.
struct FieldRepairCandidate<'a> {
    path: &'static str,
    surface: FieldSurface,
    /// `Some` only for a SETTLING walk. `None` is the non-settling plan: it
    /// repairs for the answer its caller returns and can settle nothing, so it
    /// holds no single-flight claim (leaving it available to a sibling request
    /// that CAN settle) and consults no resident verdict. Both settlements are
    /// no-ops on it -- the type carries the capability rather than every
    /// settlement site re-checking a mode flag.
    guard: Option<FieldRepairGuard<'a>>,
}

impl FieldRepairPlan<'_> {
    /// The closed-table path a successful [`Self::apply`] dropped, or `None`
    /// when this plan has not repaired anything. A code-authored literal,
    /// never upstream text, so it is safe in an operator-facing field.
    pub(super) fn repaired_path(&self) -> Option<&'static str> {
        self.repaired.map(|idx| self.candidates[idx].path)
    }

    /// Perform the repair for whichever admitted row `err` names: drop that
    /// row's field from the attempt request, re-stamp the calibration estimate,
    /// CHARGE the shared per-request budget, and RELEASE every other
    /// candidate's guard -- all or nothing.
    ///
    /// The rejection is matched against every admitted row rather than against
    /// one chosen at admission time: the upstream names the field, so the row
    /// it names is the row to repair. Matching is on the SURFACE (see
    /// [`Router::rejection_names_field_surface`]).
    ///
    /// The budget draw lives here, inside the successful branch, rather than in
    /// the caller's condition chain. The drop can legitimately remove nothing
    /// (the request changed under the arm), and a draw in the condition would
    /// then have charged a request that did not repair -- a charge nothing
    /// observable can see, since the arm falls through to the ordinary error
    /// path either way. Ordering the two the other way means the budget and
    /// the mutation succeed or fail together.
    ///
    /// `Some(status)` -- repaired: the named row's field is gone, the estimate
    /// describes the new payload, one repair was charged, and the caller must
    /// re-dispatch and later settle this plan. `None` -- nothing happened: no
    /// mutation, no charge, no guard released, and the caller must NOT mark the
    /// attempt repaired or select the commit settlement.
    pub(super) fn apply(
        &mut self,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        native_class: &FailureClass,
        rejection: &Error,
        provider_kind: &str,
    ) -> Option<u16> {
        // A plan that has already repaired must not repair again: the ceiling
        // is one drop per attempt, and a second would mutate a body the arm
        // already re-dispatched.
        if self.repaired.is_some() {
            return None;
        }
        // WHICH row: the one the upstream named. Refusing here is
        // pre-mutation, so an unnamed rejection leaves the request and every
        // guard exactly as the caller handed them over.
        let idx = self.candidates.iter().position(|candidate| {
            Router::rejection_names_field_surface(
                candidate.surface,
                native_class,
                rejection,
                provider_kind,
            )
        })?;
        self.apply_candidate(idx, attempt_req, meta, budget, rejection)
    }

    /// The EFFECT half of [`Self::apply`], for the candidate at `idx`: the
    /// presence re-check, the allowance check, the drop, the charge, the guard
    /// releases, and the estimate re-stamp.
    ///
    /// Split from the rejection-to-candidate SELECTION above so the fail-closed
    /// branch below is reachable in a release test build: the divergent surface
    /// that manufactures the presence/drop disagreement reports itself present
    /// unconditionally, so giving it a closed-table row would make every request
    /// in a test build carry it. This is the entry point that test drives.
    #[cfg_attr(not(test), allow(clippy::needless_pass_by_ref_mut))]
    fn apply_candidate(
        &mut self,
        idx: usize,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
    ) -> Option<u16> {
        let surface = self.candidates[idx].surface;
        // Presence is re-read HERE rather than trusted from admission: the
        // request has been through a dispatch since, and a drop that removes
        // nothing must not consume the allowance.
        if !surface.present_in(attempt_req) {
            return None;
        }
        // Availability is CHECKED, not spent: an exhausted allowance must leave
        // the request untouched, so this precedes the mutation, while the charge
        // below follows it. Splitting the read from the draw is what lets the
        // order be "check, mutate, then charge" without a charge that a failed
        // mutation would have to unwind.
        if !budget.can_draw() {
            return None;
        }
        if !surface.drop_from(attempt_req) {
            // FAIL CLOSED. `present_in` held immediately above and nothing
            // between touches the request, so reaching here means the two have
            // diverged -- a bug in this module. The debug assertion names it in
            // development, but the RELEASE behavior must not depend on that
            // assertion being compiled: absorbing the inconsistency silently
            // would report a repair the wire never saw, charge the allowance for
            // it, and let the caller select the commit settlement. So the
            // release path returns None, having mutated nothing and charged
            // nothing. This is the LAST point at which `None` is a correct
            // answer: it is still pre-mutation, so refusing here leaves the
            // request exactly as the caller handed it over.
            debug_assert!(
                false,
                "a present surface must remove something; presence and drop have diverged",
            );
            return None;
        }
        // Charged UNCONDITIONALLY now, and this is the one place that must not
        // have a refusal branch. The field is already gone from `attempt_req`,
        // so returning `None` here would report "no repair" for a request whose
        // body this call already changed: the caller would leave the arm without
        // re-dispatching, and the mutated attempt would sit in a walk that
        // believes it never repaired -- a silently altered request, which is
        // strictly worse than either repairing or refusing. `can_draw` above is
        // what makes the refusal impossible, so the invariant is asserted rather
        // than branched on, and NO path below the mutation returns `None`.
        let charged = budget.draw();
        debug_assert!(
            charged,
            "can_draw held before the mutation, so the draw after it cannot fail",
        );
        self.repaired = Some(idx);
        // Every OTHER candidate's guard is released now rather than held to
        // settlement. This attempt has spent its one repair on `idx`, so no
        // outcome it reaches can settle another row: the arm's commit speaks
        // for the repaired row alone. Releasing frees each unused
        // single-flight slot for a sibling request that CAN settle that row,
        // instead of pinning it for the rest of this walk.
        for (other, candidate) in self.candidates.iter_mut().enumerate() {
            if other != idx {
                drop(candidate.guard.take());
            }
        }
        super::dispatch::restamp_calibration_estimate(attempt_req, meta);
        Some(
            super::class_observe::upstream_facts(rejection)
                .status
                .unwrap_or(0),
        )
    }

    /// A guardless plan over the divergent test surface: the shape that reaches
    /// `apply`'s fail-closed branch. Test-only, and deliberately not
    /// constructible any other way.
    #[cfg(all(test, not(debug_assertions)))]
    pub(super) fn divergent_for_tests() -> Self {
        Self {
            candidates: vec![FieldRepairCandidate {
                // Outside the closed table deliberately: the surface is not a
                // row (see `closed_table`), and this path exists only so the
                // candidate is well formed.
                path: "divergent.surface.for.tests",
                surface: FieldSurface::DivergentForTests,
                guard: None,
            }],
            repaired: None,
        }
    }

    /// Drive [`Self::apply_candidate`] for this plan's only candidate, skipping
    /// the rejection-to-candidate selection.
    ///
    /// Test-only, and the ONLY way the fail-closed branch is reachable: the
    /// divergent surface has no closed-table row, so no rejection can select it.
    /// The production effect path is called directly, so a mutation to the
    /// refusal, the charge, or the mutation ordering is observable here.
    #[cfg(all(test, not(debug_assertions)))]
    pub(super) fn apply_sole_candidate_for_tests(
        &mut self,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
    ) -> Option<u16> {
        assert_eq!(
            self.candidates.len(),
            1,
            "the divergent fixture holds exactly one candidate",
        );
        self.apply_candidate(0, attempt_req, meta, budget, rejection)
    }

    /// Phase two after a successful repaired retry: persist the REPAIRED row's
    /// verdict and return the emission row for the capability-event sink, or
    /// `None` for a non-settling plan (which persists nothing and emits
    /// nothing) and for a plan that repaired nothing.
    ///
    /// Only the repaired row commits. The rejection named that row, and the
    /// retry that succeeded is evidence about that row alone -- every other
    /// candidate's guard was already released by [`Self::apply`].
    ///
    /// `request_features` is taken BY VALUE: the guard stores it on the event
    /// row, so a borrow here would only be cloned one frame deeper.
    pub(super) fn commit(
        mut self,
        upstream_status: u16,
        request_features: Vec<String>,
        now: Instant,
    ) -> Option<CapabilityLearnEvent> {
        let idx = self.repaired?;
        self.candidates[idx]
            .guard
            .take()?
            .commit(upstream_status, request_features, now)
    }

    /// Phase two after the UNREPAIRED attempt succeeded: every carried field is
    /// accepted, so drop any resident verdict for each admitted row and return
    /// the cleared rows so the ledger records the clears and a warm rebuild
    /// cannot resurrect them.
    ///
    /// One event per row that actually had a resident entry removed; empty for
    /// an attempt whose rows had none, and for every non-settling plan. A
    /// non-settling walk must NOT clear: the removal would be invisible to the
    /// ledger, so the next warm rebuild would resurrect a verdict this process
    /// had already dropped.
    pub(super) fn settle_success(mut self) -> Vec<CapabilityClearedEvent> {
        self.candidates
            .iter_mut()
            .filter_map(|candidate| candidate.guard.take()?.clear())
            .collect()
    }
}

impl Router {
    /// Claim the envelope-field repair slot for EVERY closed-table field the
    /// attempt carries toward `target`, or `None` when this attempt can settle
    /// none.
    ///
    /// Admitted BEFORE dispatch and WITHOUT touching the request, mirroring
    /// the reasoning-replay carry admission. Pre-dispatch admission is what
    /// makes both settlements reachable: the identity of a field the request
    /// carries is derivable from the closed table alone, so a request whose
    /// field is ACCEPTED can clear a resident verdict while a request whose
    /// field is REJECTED can commit one. An admission driven off the rejection
    /// instead could only ever commit, and a verdict that can be minted but
    /// never cleared decays instead of being disproved.
    ///
    /// Every present row is admitted, not only the first: the REJECTION decides
    /// which row is repaired, and the upstream can name any carried field. An
    /// admission holding only the first present row would leave a rejection
    /// naming a later row unrepairable on that lane forever. The
    /// one-repair-per-attempt ceiling is enforced by
    /// [`FieldRepairPlan::apply`], which applies exactly one candidate and
    /// releases every other candidate's guard.
    ///
    /// This is NOT a pre-flight rewrite: nothing here reads a resident verdict
    /// in order to modify the outgoing request. An acting verdict REFUSES the
    /// slot, so the request dispatches exactly as it would have and simply
    /// does not repair -- inert rather than wrong. Acting on a verdict before
    /// dispatch is a separate, later concern.
    ///
    /// Every refusal is a refusal to ACT, checked before any state is claimed:
    ///
    /// - the caller cannot SETTLE (`mode`): a walk whose settlement rows have
    ///   nowhere to go must not claim a slot it cannot resolve;
    /// - the learned-capability kill switch is off;
    /// - the target is not on the one lane this stage acts on;
    /// - the target authenticates with a FORWARDED client credential;
    /// - the attempt carries no field in the closed table, so no L0 drop could
    ///   change what goes upstream;
    /// - the target's base URL names a local destination, or the lane would
    ///   rewrite the minted key, so no verdict may be minted for it;
    /// - for a given row, an acting verdict is resident or a concurrent request
    ///   holds that row's single-flight slot -- which drops that ROW from the
    ///   plan, leaving its siblings admitted.
    ///
    /// `None` when NO row survived, so a caller's `Some` always names at least
    /// one repairable candidate.
    ///
    /// The returned plan holds the guards. A caller that drops it without
    /// settling releases every slot and learns nothing, so an arm whose later
    /// conditions decline leaves no residue.
    pub(super) fn plan_field_carry<'a>(
        &'a self,
        target: &DispatchTarget,
        attempt_req: &ChatRequest,
        mode: FieldSettlementMode,
        now: Instant,
    ) -> Option<FieldRepairPlan<'a>> {
        if !self.config.capability.enabled {
            return None;
        }
        let provider_kind = target.provider_kind?;
        if provider_kind != ANTHROPIC_API_KIND {
            return None;
        }
        // A forwarded-credential target authenticates with the CLIENT's own
        // bearer, which makes its rejection unattributable to this seat in the
        // way a verdict claims: routectl owns no credential there, the upstream
        // behind it is whatever that bearer reaches, and the same configured
        // entry serves every client that presents one. Minting a permanent,
        // routing-affecting negative from one client's rejection would steer
        // every other client's traffic. Refused BEFORE the body is read, the
        // budget is drawn, or a guard is claimed, so such a target is untouched
        // rather than repaired-then-unlearned.
        if target.use_forwarded_credential {
            return None;
        }
        // Read from the operator's own provider entry rather than from the
        // dispatch target: the suppression predicate keys on the configured
        // base URL, and the target carries none. The accessor answers only
        // for the lane this stage acts on, so every other kind fails closed
        // here as well as at the kind check above.
        let base_url = self
            .config
            .providers
            .get(&target.provider_name)
            .and_then(crate::config::ProviderEntry::anthropic_api_base_url)?;
        // Loopback suppression is checked HERE, above the settlement-mode
        // branch, so it applies to EVERY walk. The guard's own admission also
        // refuses a local target, but only a settling walk reaches that
        // admission -- leaving the check there alone let a result-only walk
        // mutate the body, spend the shared allowance, and re-dispatch a
        // target whose rejection this stage has decided it cannot attribute at
        // all. A suppressed target is one routectl must not act on, not merely
        // one it must not learn from, so the refusal belongs to the plan rather
        // than to the settlement.
        if crate::field_verdict::loopback_target_suppresses_minting(base_url) {
            return None;
        }
        // The identities come from the CLOSED TABLE plus the request, never
        // from upstream text: every row whose surface this attempt actually
        // carries, in table source order.
        let candidates: Vec<FieldRepairCandidate<'a>> = present_rows(attempt_req)
            .filter_map(|row| {
                // A NON-SETTLING walk admits the row guardless: it repairs for
                // the answer it returns and can persist nothing, so it claims
                // no single-flight slot (leaving it to a sibling that can
                // settle), reads no resident verdict, and is structurally
                // unable to mint or clear -- the guard it would need does not
                // exist on its candidate.
                if mode == FieldSettlementMode::NonSettling {
                    return Some(FieldRepairCandidate {
                        path: row.path,
                        surface: row.surface,
                        guard: None,
                    });
                }
                let key = FieldVerdictKey::new(&target.state_key, row.path, provider_kind)?;
                let guard = self.field_verdicts().admit_provisional(
                    &key,
                    base_url,
                    self.registry_generation(),
                    now,
                )?;
                Some(FieldRepairCandidate {
                    path: row.path,
                    surface: row.surface,
                    guard: Some(guard),
                })
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(FieldRepairPlan {
            candidates,
            repaired: None,
        })
    }

    /// Whether the rejection `err`, natively classified as `native_class`,
    /// names a closed-table row whose SURFACE is `surface`.
    ///
    /// THE single implementation of "did the upstream name this mutation", shared
    /// by the reactive plan's own candidate selection ([`FieldRepairPlan::apply`])
    /// and the canary's repaired retry (see [`super::field_preflight`]). Two
    /// copies would be two answers to one question, and the one that drifted
    /// would either repair over an unrelated fault or decline a rejection it was
    /// built to handle.
    ///
    /// The NATIVE class is the input, never the operator-remapped one. A
    /// `[class_overrides]` entry expresses how the operator wants a status
    /// ROUTED; it is not a claim about what the upstream said. Reading the
    /// remapped class would let an override turn a 429 or a 503 into an
    /// envelope-field repair -- dropping a field over a rate limit, and then
    /// minting a permanent verdict from it -- so the repair decision reads what
    /// the upstream actually returned and the routing decision keeps reading
    /// the override.
    ///
    /// Compared on the surface rather than the path string: the surface is what
    /// the drop acts on, and two rows sharing one surface would be the same
    /// mutation under two identities.
    ///
    /// False for every real rejection in this stage: no envelope resolves to a
    /// path (see [`rejected_field_path`]), so an admitted carry settles as an
    /// ordinary success or releases, and no repair fires.
    pub(super) fn rejection_names_field_surface(
        surface: FieldSurface,
        native_class: &FailureClass,
        err: &Error,
        provider_kind: &str,
    ) -> bool {
        let Some(named) = rejected_field_path(native_class, err, provider_kind) else {
            return false;
        };
        closed_table_row(named).is_some_and(|(_, row_surface)| row_surface == surface)
    }

    /// Record that the field repair fired for this request, for the single
    /// aggregated WARN emitted at request resolution, and count it.
    ///
    /// The WARN is one line per request; the counter is what answers "how
    /// often does this arm fire" without depending on a log level, so both
    /// are driven from here and cannot disagree about whether a repair
    /// happened.
    pub(super) fn note_field_repair(
        &self,
        meta: &mut DispatchMeta,
        state_key: &str,
        path: &'static str,
    ) {
        meta.field_repair = Some(super::FieldRepair {
            action: FIELD_ACTION_DROP_REPAIR,
            state_key: sanitize_for_log(state_key),
            field_path: path,
            reason: FIELD_REASON_UPSTREAM_REJECTION,
            repair_attempted: true,
            repair_succeeded: false,
            learned: false,
        });
        self.metrics.incr_field_repair_attempted();
    }

    /// Record that the repaired retry succeeded, and whether that outcome
    /// persisted a verdict. Both booleans move together because a committed
    /// verdict is exactly a succeeded repair -- recording them at separate
    /// sites is how a summary comes to claim one without the other.
    pub(super) fn note_field_repair_succeeded(&self, meta: &mut DispatchMeta, learned: bool) {
        if let Some(record) = meta.field_repair.as_mut() {
            record.repair_succeeded = true;
            record.learned = learned;
        }
        self.metrics.incr_field_repair_succeeded();
        if learned {
            self.metrics.incr_field_verdicts_learned();
        }
    }
}

/// Emit the single aggregated envelope-field repair WARN for a resolved
/// request. Fires exactly ONCE when the repair fired anywhere in the walk,
/// and not at all otherwise. Content-free by construction: closed-set
/// tokens, the closed table's own path literal, and booleans -- never the
/// upstream body, the rejected field's VALUE, or the session key. The request
/// span already supplies `request_id` correlation across the hops.
pub(super) fn emit_field_repair(meta: &DispatchMeta) {
    let Some(record) = meta.field_repair.as_ref() else {
        return;
    };
    tracing::warn!(
        action = record.action,
        state_key = %record.state_key,
        field_path = record.field_path,
        reason = record.reason,
        repair_attempted = record.repair_attempted,
        repair_succeeded = record.repair_succeeded,
        learned = record.learned,
        "{FIELD_REPAIR_EVENT}",
    );
}

/// Message/event name of the single aggregated field-repair WARN. Stable,
/// greppable, closed-set.
const FIELD_REPAIR_EVENT: &str = "envelope_field_repaired";

/// Test-only injection of a PROVISIONAL field resolution.
///
/// The production resolver is inert by construction (see the module docs),
/// so behavioral coverage of every arm downstream of it -- the drop, the
/// shared budget, the two-phase settlement, the persistence, the WARN --
/// needs a resolution from somewhere. This is that seam, and it is the ONLY
/// one: no production path reads it, and the corresponding control asserts
/// the production resolver stays inert for the same rejection shapes.
///
/// Thread-local because a `#[tokio::test]` drives its future to completion on
/// the test's own thread, so an injection set in a test body is visible
/// inside the dispatch it drives and invisible to every sibling test running
/// concurrently. That property is asserted rather than assumed
/// (`the_injected_resolution_is_visible_inside_a_dispatch`).
#[cfg(test)]
pub(in crate::router) mod provisional {
    use std::cell::RefCell;

    thread_local! {
        static INJECTED_PATH: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// The injected path, if this thread has one.
    pub(super) fn injected_path() -> Option<String> {
        INJECTED_PATH.with(|slot| slot.borrow().clone())
    }

    /// Make every field rejection on this thread resolve to `path` until the
    /// returned guard drops.
    ///
    /// Visible to the whole `router` module rather than to `field_repair`
    /// alone: the canary settlement's coverage lives in a sibling sidecar and
    /// needs the SAME seam, because a second injection point would be a second
    /// thing a grounded parser has to displace.
    pub(in crate::router) fn inject(path: &str) -> Injection {
        INJECTED_PATH.with(|slot| *slot.borrow_mut() = Some(path.to_string()));
        Injection
    }

    /// Clears the thread's injection on drop, so one test cannot leak a
    /// resolution into another test that reuses its thread.
    pub(in crate::router) struct Injection;

    impl Drop for Injection {
        fn drop(&mut self) {
            INJECTED_PATH.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
#[path = "field_repair_tests.rs"]
mod field_repair_tests;

#[cfg(test)]
mod table_tests {
    use super::*;

    /// This module's own source, for the production-body guard. Scanned whole:
    /// the production parser sits under `cfg(not(test))`, so it is present in
    /// the TEXT while absent from this binary, and reading the text is the only
    /// way to assert anything about it.
    const SOURCE: &str = include_str!("field_repair.rs");

    /// The body of the `cfg(not(test))` parser seam: the text between its
    /// signature's opening brace and the closing brace at column zero.
    ///
    /// # Panics
    ///
    /// If the `cfg(not(test))` attribute, the signature that follows it, or the
    /// closing brace is absent. Failing closed is the point -- a locator that
    /// returned an empty body on a missed needle would make its caller pass
    /// vacuously, which is exactly the shape a production-inertness guard must
    /// not have.
    fn production_parser_body(source: &str) -> &str {
        const GATE: &str = "#[cfg(not(test))]";
        const SIGNATURE: &str = "fn parse_rejected_field_path(";
        const OPEN: &str = ") -> Option<String> {";
        let gate_at = source
            .find(GATE)
            .expect("the production parser must sit behind a cfg(not(test)) gate");
        let after_gate = &source[gate_at..];
        let sig_at = after_gate
            .find(SIGNATURE)
            .expect("the cfg(not(test)) gate must be followed by the parser signature");
        let after_sig = &after_gate[sig_at..];
        let open = after_sig
            .find(OPEN)
            .expect("the parser signature must be followed by its body");
        let body = &after_sig[open + OPEN.len()..];
        let end = body
            .find("\n}")
            .expect("the parser body must close at column zero");
        &body[..end]
    }

    /// The body of `FieldRepairPlan::apply_candidate` -- the EFFECT half of the
    /// repair, from its signature's opening brace to the closing brace at its
    /// own indentation.
    ///
    /// Anchored on the function NAME rather than on the return type alone: the
    /// selection half (`apply`) shares that return type, and a bare type anchor
    /// would read the wrong body -- one that contains no mutation, so the guard
    /// below would fail on its own premise rather than pass vacuously.
    ///
    /// # Panics
    ///
    /// If the signature or the closing brace is absent -- failing closed, for
    /// the same reason as the production-body locator: a guard reading an empty
    /// body would pass vacuously.
    fn apply_body(source: &str) -> &str {
        const SIGNATURE: &str = "fn apply_candidate(";
        const OPEN: &str = "    ) -> Option<u16> {";
        let sig_at = source
            .find(SIGNATURE)
            .expect("the effect half must be named `apply_candidate`");
        let after_sig = &source[sig_at..];
        let open_at = after_sig
            .find(OPEN)
            .expect("`apply_candidate` must carry its `-> Option<u16>` signature");
        let body = &after_sig[open_at + OPEN.len()..];
        let end = body
            .find("\n    }")
            .expect("`apply_candidate`'s body must close at its own indentation");
        &body[..end]
    }

    /// A body reduced to its CODE TOKENS: comments removed, every run of
    /// whitespace collapsed away entirely.
    ///
    /// Normalizing to an exact string is what makes the production-inertness
    /// guard a CONTRACT rather than a denylist. A denylist of forbidden
    /// substrings can only refuse the shapes someone thought of -- an arbitrary
    /// `helper()` call returning `Option<String>` passes every such needle while
    /// making the parser live. Comparing the whole normalized body against the
    /// one permitted spelling inverts that: anything not exactly "discard the
    /// two parameters, return None" fails, whatever it is called.
    ///
    /// Whitespace is dropped rather than collapsed to single spaces, and a
    /// comma directly before a closing bracket is dropped too, so the contract
    /// is insensitive to rustfmt's line breaking: reflowing the parameter
    /// discard onto several lines adds a trailing comma that is not part of the
    /// property being asserted. Nothing else is rewritten -- a dropped token
    /// would be a hole in the contract.
    fn normalized_code(body: &str) -> String {
        let mut out = String::with_capacity(body.len());
        for line in body.lines() {
            let code = match line.find("//") {
                Some(at) => &line[..at],
                None => line,
            };
            out.extend(code.chars().filter(|c| !c.is_whitespace()));
        }
        // Trailing commas inside brackets only: `,)` and `,]`.
        out.replace(",)", ")").replace(",]", "]")
    }

    use routectl_core::{ReasoningConfig, ReasoningDetail, ReasoningDetailKind};

    /// A bare request carrying `prompt` as its flat system prompt.
    fn req_with_system(prompt: &str) -> ChatRequest {
        ChatRequest {
            system: Some(routectl_core::SystemContent::Text(prompt.to_string())),
            ..Default::default()
        }
    }

    fn req_with(carrier: Option<&str>, exclude: Option<bool>) -> ChatRequest {
        let mut req = ChatRequest::default();
        req.routectl_internal.anthropic_thinking_display = carrier.map(str::to_string);
        if exclude.is_some() {
            req.reasoning = Some(ReasoningConfig {
                effort: None,
                max_tokens: None,
                exclude,
                enabled: Some(true),
            });
        }
        req
    }

    #[test]
    fn the_closed_table_carries_the_one_grounded_path_and_nothing_else() {
        // The table's SIZE is the property: a second row is a second
        // permanent commitment and must be a deliberate, reviewed edit
        // rather than something a passing suite absorbs silently.
        assert_eq!(
            FIELD_REPAIRS.len(),
            1,
            "the closed repair table ships exactly one grounded row; a new row is a new \
             permanent commitment and updates this guard deliberately",
        );
        assert_eq!(
            FIELD_REPAIRS[0].path, "thinking.enabled.display",
            "the grounded row's path is the qualified dotted path the captured rejection \
             named, byte for byte -- a permanent token cannot be respelled later",
        );
        assert_eq!(
            FIELD_REPAIRS[0].class,
            TransformClass::Envelope,
            "the one grounded row removes envelope metadata only -- it is NOT \
             prefix-impacting, and a row whose class changed would silently change \
             which gates its pre-flight application must clear",
        );
    }

    #[test]
    fn a_path_outside_the_closed_table_resolves_to_no_row() {
        // The whole of "unknown path mutates nothing": with no row there is
        // no surface, so no code path can decide what to drop.
        for unknown in [
            "thinking.display",
            "thinking.enabled",
            "thinking.enabled.display.extra",
            "",
            "reasoning.exclude",
        ] {
            assert!(
                closed_table_row(unknown).is_none(),
                "`{unknown}` is outside the closed table and must resolve to no row",
            );
        }
        // Positive control: the table is not simply empty.
        assert!(
            closed_table_row("thinking.enabled.display").is_some(),
            "control: the grounded path DOES resolve, so the negatives above are about the \
             paths and not about an empty table",
        );
    }

    #[test]
    fn the_surface_is_present_when_either_carrier_would_emit_the_field() {
        let surface = FieldSurface::AnthropicThinkingDisplay;
        assert!(
            surface.present_in(&req_with(Some("updates"), None)),
            "the verbatim ingress carrier alone emits the field",
        );
        assert!(
            surface.present_in(&req_with(None, Some(true))),
            "the canonical boolean alone emits the field through the egress derivation",
        );
        assert!(
            surface.present_in(&req_with(Some("updates"), Some(false))),
            "both carriers present is still present",
        );
        assert!(
            !surface.present_in(&req_with(None, None)),
            "neither carrier means the attempt does not emit the field, so there is nothing \
             an L0 drop could repair",
        );
    }

    #[test]
    fn the_drop_removes_both_carriers_and_reports_what_it_removed() {
        let surface = FieldSurface::AnthropicThinkingDisplay;

        // Arrange -- both carriers set, which is the case a
        // carrier-only drop would silently leave emitting the field.
        let mut req = req_with(Some("updates"), Some(true));

        // Act
        let dropped = surface.drop_from(&mut req);

        // Assert
        assert!(dropped, "removing either carrier is a removal");
        assert!(
            !surface.present_in(&req),
            "after the drop neither carrier remains, so the wire field is gone -- a drop that \
             cleared only the carrier would leave the derivation emitting it",
        );
        assert!(
            req.routectl_internal.anthropic_thinking_display.is_none(),
            "the verbatim carrier is cleared",
        );
        assert_eq!(
            req.reasoning.as_ref().and_then(|r| r.exclude),
            None,
            "the canonical boolean is cleared",
        );
        assert_eq!(
            req.reasoning.as_ref().and_then(|r| r.enabled),
            Some(true),
            "the drop removes the rejected field ONLY: whether thinking was requested is a \
             different field and must survive, or the repair silently disables the feature",
        );
    }

    #[test]
    fn the_test_prefix_surface_drop_refuses_a_non_sentinel_system_prompt() {
        // The DROP half of the sentinel scoping, tested directly on the surface
        // rather than through the planner. `present_in` and `drop_from` are two
        // functions, so narrowing only the former would leave the latter able to
        // take ANY request's system prompt -- and the planner never reaches
        // `drop_from` for a row whose presence check refused, so no
        // planner-level test can observe that. The internal re-check inside
        // `drop_from` is what closes it, and removing that re-check makes this
        // test RED while every planner test stays green.
        let surface = FieldSurface::PrefixImpactingSystemForTests;
        let mut req = req_with_system("you are a careful assistant");
        let before = serde_json::to_string(&req).expect("serializable");

        // Act -- ask the surface to drop a prompt it does not recognize.
        let dropped = surface.drop_from(&mut req);

        // Assert -- refused, and the prompt survives untouched.
        assert!(
            !dropped,
            "the test-only prefix surface must report NO removal for a system prompt \
             that is not its exact sentinel",
        );
        assert!(
            req.system.is_some(),
            "and must leave the prompt in place: a fixture elsewhere in the suite that \
             happens to set `system` must not have it silently removed",
        );
        assert_eq!(
            serde_json::to_string(&req).expect("serializable"),
            before,
            "byte-identical, since a no-op drop that moved any byte is a different \
             request upstream",
        );

        // Positive control: the SENTINEL prompt IS removed, so the refusals above
        // are about the predicate rather than about a surface that never drops.
        let mut sentinel = req_with_system(TEST_PREFIX_SENTINEL);
        assert!(
            surface.drop_from(&mut sentinel),
            "control: the sentinel prompt is dropped",
        );
        assert!(
            sentinel.system.is_none(),
            "control: and is gone from the request",
        );
    }

    #[test]
    fn dropping_an_absent_surface_reports_no_removal_and_mutates_nothing() {
        let surface = FieldSurface::AnthropicThinkingDisplay;
        let mut req = req_with(None, None);
        req.messages = vec![routectl_core::Message {
            role: routectl_core::Role::Assistant,
            content: routectl_core::MessageContent::Text("prior".into()),
            reasoning: None,
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Text,
                id: None,
                format: None,
                index: None,
                payload: serde_json::json!({"text": "t"}),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into();
        let before = serde_json::to_string(&req).expect("serializable");

        assert!(
            !surface.drop_from(&mut req),
            "an absent surface removes nothing",
        );
        assert_eq!(
            serde_json::to_string(&req).expect("serializable"),
            before,
            "a no-op drop must leave the request byte-identical: an attempt whose bytes moved \
             is a different request upstream, cache-affinity included",
        );
    }

    #[test]
    fn only_a_caller_shaped_rejection_can_name_a_field() {
        assert!(
            class_can_name_a_field(&FailureClass::BadRequest),
            "the envelope-refusal class is the one a field rejection arrives on",
        );
        for other in [
            FailureClass::RateLimited,
            FailureClass::Auth,
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::ServerError,
            FailureClass::Timeout,
            FailureClass::NetworkError,
            FailureClass::Overloaded,
            FailureClass::Unknown,
            FailureClass::FeatureUnsupported {
                capability: "reasoning_replay".into(),
            },
        ] {
            assert!(
                !class_can_name_a_field(&other),
                "{other:?} says nothing about the envelope's shape, so a repair driven by it \
                 would drop a field over an unrelated fault and learn from it",
            );
        }
    }

    #[test]
    fn the_cfg_test_resolver_declines_every_shape_with_no_injection_in_scope() {
        // Scope, stated because it is easy to overread: this exercises the
        // cfg(test) resolver. It shows the TEST build attributes no path
        // without an injection -- which is what makes every no-injection
        // control elsewhere meaningful -- and it says NOTHING about the
        // production body, which this binary does not contain. The production
        // claim is pinned by
        // `the_production_parser_body_cannot_resolve_a_path` below.
        let captured = r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display: Input should be 'summarized', 'omitted'"}}"#;
        for body in [
            captured,
            r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display"}}"#,
            r#"{"error":{"param":"thinking.enabled.display"}}"#,
            "thinking.enabled.display",
            "{}",
            "",
        ] {
            let err = Error::upstream("p", 400, body);
            assert!(
                rejected_field_path(&FailureClass::BadRequest, &err, ANTHROPIC_API_KIND).is_none(),
                "with no injection in scope, no field path may be attributed to `{body}`",
            );
        }
        // Positive control: the seam DOES resolve when injected, so the
        // negatives above are about the absent resolution and not about a
        // resolver that can never return a path.
        let _injection = provisional::inject("thinking.enabled.display");
        assert_eq!(
            rejected_field_path(
                &FailureClass::BadRequest,
                &Error::upstream("p", 400, captured),
                ANTHROPIC_API_KIND,
            ),
            Some("thinking.enabled.display"),
            "control: the injected seam resolves, so the negatives are not vacuous",
        );
    }

    #[test]
    fn no_path_below_the_body_mutation_can_return_none() {
        // The ordering invariant `apply` rests on, asserted over its own source
        // because it is a property of CONTROL FLOW rather than of any single
        // outcome: once the field has been dropped, every remaining path must
        // report the repair. A `None` after the mutation would hand the caller
        // "no repair" for a request this call already changed -- the caller then
        // leaves the arm without re-dispatching, and a silently altered attempt
        // rides on in a walk that believes it never repaired.
        //
        // No behavioral test can pin this: the only way to reach such a branch
        // is for the very invariant that makes it unreachable to be broken, so
        // the shape of the code is the evidence. Both halves are checked -- the
        // mutation is located, and nothing after it returns None.
        // Anchored at the point the mutation has COMMITTED, not at the call:
        // the `if !drop_from(..)` refusal is still pre-mutation (the drop
        // reported removing nothing), so its `return None` is correct and must
        // not be read as a violation. The region under test begins where that
        // refusal block ends.
        let body = apply_body(SOURCE);
        let mutation = "if !surface.drop_from(attempt_req) {";
        let at = body.find(mutation).unwrap_or_else(|| {
            panic!("`apply` must mutate through `{mutation}`; its body was: {body}")
        });
        let refusal_end = body[at..]
            .find("\n        }")
            .expect("the pre-mutation refusal block must close");
        let after = &body[at + refusal_end..];
        assert!(
            !after.contains("return None"),
            "no path below the body mutation may `return None`: the field is already gone, so \
             reporting no repair would leave a walk holding a silently altered request. Post- \
             mutation source was: {after}",
        );
        // The `?` operator is the same hazard by another spelling.
        assert!(
            !after.contains('?'),
            "and none may propagate a None with `?`, for the same reason. Post-mutation source \
             was: {after}",
        );
        // Positive control: `None` returns DO exist above the mutation (the
        // presence, allowance and divergence refusals), so the assertions above
        // are about position and not about a function that never refuses.
        assert!(
            body[..at + refusal_end].matches("return None").count() >= 3,
            "control: the pre-mutation refusals must still be there, or this guard is passing \
             because `apply` stopped refusing anything",
        );
    }

    #[test]
    fn the_production_parser_body_is_exactly_a_discard_and_none() {
        // THE production-inertness claim, and it cannot be made behaviorally:
        // the `cfg(not(test))` body is not compiled into this test binary, so no
        // call from here can reach it. Its own source is the evidence, read from
        // THIS file so the guard cannot drift from what ships.
        //
        // An EXACT contract, not a denylist. A list of forbidden substrings can
        // only refuse the shapes its author imagined; an arbitrary
        // `helper(err)?` returning `Option<String>` slips through every one of
        // them while making the parser live. So the whole normalized body is
        // compared against the single permitted spelling: discard both
        // parameters, return None. Anything else -- any call, any binding, any
        // second statement -- fails, whatever it is named.
        assert_eq!(
            normalized_code(production_parser_body(SOURCE)),
            "let_=(err,provider_kind);None",
            "the production parser body may ONLY discard its two parameters and return None. \
             This stage ships no grounded envelope parser, so any other body means the arm can \
             attribute a path on real traffic -- and the change that grounds a parser updates \
             this contract deliberately, together with the coverage that then becomes \
             reachable",
        );
    }

    #[test]
    fn the_body_contract_refuses_an_arbitrary_helper_call() {
        // The contract's own discrimination, measured rather than assumed: the
        // shapes a substring denylist would have admitted must all fail it. Each
        // of these is a body that RESOLVES a path, written so no obvious needle
        // ("Some(", "serde_json", "message") appears in it.
        let permitted = "let_=(err,provider_kind);None";
        for live in [
            "let _ = provider_kind;\n    helper(err)\n",
            "let _ = (err, provider_kind);\n    lookup()\n",
            "grounded(err, provider_kind)\n",
            "let _ = provider_kind;\n    err.field_path()\n",
            "let _ = (err, provider_kind);\n    None.or_else(fallback)\n",
        ] {
            assert_ne!(
                normalized_code(live),
                permitted,
                "a body that delegates to a helper must NOT satisfy the contract: `{live}`",
            );
        }
        // And the real shape does satisfy it, in both a formatted and a
        // reflowed spelling plus with a comment -- so the contract is about the
        // CODE and not about rustfmt's line breaking.
        for inert in [
            "let _ = (err, provider_kind);\n    None\n",
            "let _ = (\n        err,\n        provider_kind,\n    );\n    None\n",
            "// no grounded parser in this stage\n    let _ = (err, provider_kind);\n    None\n",
        ] {
            assert_eq!(
                normalized_code(inert),
                permitted,
                "the inert shape must satisfy the contract regardless of formatting or \
                 comments: `{inert}`",
            );
        }
    }

    #[test]
    fn the_production_body_locator_fails_closed() {
        // The guard above is only as good as its locator: a needle that stopped
        // matching would read an empty body and pass vacuously. So the locator
        // PANICS rather than returning nothing, asserted here per missing
        // needle so a partial match cannot pass either.
        for absent in [
            "fn unrelated() {}",
            // The gate without the signature.
            "#[cfg(not(test))]\nfn something_else(x: u8) -> u8 { x }\n",
            // The gate and signature without the returning-body opener.
            "#[cfg(not(test))]\nfn parse_rejected_field_path(e: &Error) -> bool { true }\n",
        ] {
            let outcome = std::panic::catch_unwind(|| production_parser_body(absent));
            assert!(
                outcome.is_err(),
                "a source without the full production-parser shape must panic the locator, not \
                 yield a body every assertion would pass against: `{absent}`",
            );
        }
        // And it finds a real one, so the panic case is not the only behavior.
        assert!(
            !production_parser_body(SOURCE).is_empty(),
            "the locator must find this module's own production body",
        );
    }

    #[test]
    fn an_injected_resolution_does_not_bypass_the_class_gate() {
        // The injection replaces the missing PARSER, not the arm's own
        // conditions: a class that cannot name a field still resolves to
        // nothing even with a resolution injected, so no test can
        // accidentally cover a path production could not reach.
        let _injection = provisional::inject("thinking.enabled.display");
        assert!(
            rejected_field_path(
                &FailureClass::RateLimited,
                &Error::upstream("p", 429, "{}"),
                ANTHROPIC_API_KIND,
            )
            .is_none(),
            "an injected resolution must not lift a class that cannot carry a field rejection",
        );
    }

    #[test]
    fn an_injection_clears_itself_so_it_cannot_leak_into_a_sibling_test() {
        {
            let _injection = provisional::inject("thinking.enabled.display");
            assert!(provisional::injected_path().is_some(), "injected in scope");
        }
        assert!(
            provisional::injected_path().is_none(),
            "the injection must clear on drop: the thread is reused across tests, so a leaked \
             resolution would make a sibling's production-inertness control vacuous",
        );
    }
}
