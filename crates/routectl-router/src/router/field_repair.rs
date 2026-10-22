//! Reactive L0 envelope-field repair: the closed path-to-surface table, the
//! request-side drop, and the admission that binds a repair to the two-phase
//! field guard.
//!
//! An upstream that rejects a request by naming a WIRE FIELD PATH states
//! either that the lane does not accept that field, or that it does not
//! accept the VALUE sent in it. The row's [`FieldTransform`] decides the
//! repair: a DROP row removes the mapped field, a REWRITE row replaces its
//! value with the one member of the row's closed accepted set the rejection
//! names, and either re-dispatches the same target once. A rewrite never
//! substitutes a value the rejection did not name, and a rejection naming
//! none or several repairs nothing. What differs is what the repair may
//! teach: a refused FIELD, or a rewrite to the upstream's own named value,
//! commits a verdict. A refused value on a drop row is repaired for this
//! attempt alone and recorded as a lossy repair, because a learned drop
//! would strip every valid value of that field from every later request on
//! the lane.
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
//! # Where the attribution comes from
//!
//! The per-dialect rejection parser reads the error's carried parse and
//! names a row of this table, or nothing. The coverage of the arms
//! downstream of it can also inject a resolution through the test-only seam
//! in [`provisional`]; an injection replaces the parser for the thread it is
//! set on and stands for a refused field.

use std::time::Instant;

use routectl_core::Error;
use routectl_core::capability::SignalTier;
use routectl_core::failure_class::FailureClass;
use routectl_core::wire_check::{EditKind, EditSite, IngressDialect};
use routectl_core::{ChatRequest, sanitize_for_log};

use crate::field_verdict::{FieldRepairGuard, FieldVerdictKey};
use crate::field_verdict_value::{ValueSlot, VerdictValue};

use super::edit_declare::{declare_field_edit, fidelity_dialect};
use super::rejection_parser::{
    NamedRejection, OPENAI_UNSUPPORTED_CODE, RejectionKind, RejectionValue, parse_rejection,
};
use super::repair_budget::RepairBudget;

use super::{CapabilityClearedEvent, CapabilityLearnEvent, DispatchMeta, DispatchTarget, Router};

/// `anthropic-api`, as `ProviderEntry::kind_str` spells it: one of the kinds
/// field repair admits ([`field_repair_admits_kind`]).
pub(super) const ANTHROPIC_API_KIND: &str = routectl_core::provider_kind::config::ANTHROPIC_API;

/// Action token for the field-repair WARN: the L0 repair dropped the mapped
/// envelope field and re-dispatched the same target once. Closed set.
pub(super) const FIELD_ACTION_DROP_REPAIR: &str = "field_drop_repair";

/// Action token for the field-repair WARN when the upstream refused the
/// VALUE sent in the field rather than the field: the drop served this
/// attempt only, and no verdict was learned from it. Closed set.
pub(super) const FIELD_ACTION_LOSSY_DROP_REPAIR: &str = "field_lossy_drop_repair";

/// Action token for the field-repair WARN when the repair rewrote the field to
/// the one accepted value the rejection named. Closed set.
pub(super) const FIELD_ACTION_REWRITE_REPAIR: &str = "field_rewrite_repair";

/// Action token for the field-repair WARN when the repair lowered a token
/// limit to the ceiling the rejection named. Closed set.
pub(super) const FIELD_ACTION_CEILING_REPAIR: &str = "field_ceiling_repair";

/// Action token for the field-repair WARN when the repair lowered the token
/// limit routectl filled in for a client that sent none, so the served
/// request carries nothing the client asked for differently. Closed set.
pub(super) const FIELD_ACTION_CEILING_FILL_REPAIR: &str = "field_ceiling_fill_repair";

/// Whether a repair recorded under `action` served the attempt on a request
/// carrying something other than what the client sent: a drop of a field
/// whose VALUE was refused, a rewrite to the accepted value, or a lowered
/// client token limit. A drop of a field the lane does not take, or a lowered
/// fill routectl itself supplied, changes nothing the client asked for. The
/// lossy counter and the wire finding's repair outcome both read this.
pub(super) fn repair_action_is_lossy(action: &str) -> bool {
    matches!(
        action,
        FIELD_ACTION_LOSSY_DROP_REPAIR | FIELD_ACTION_REWRITE_REPAIR | FIELD_ACTION_CEILING_REPAIR
    )
}

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
    /// The Anthropic `thinking.type` wire field, PRESENT when the type an
    /// Anthropic-shape egress would emit is exactly the held refused type.
    ///
    /// Presence reads the COMPOSED type
    /// (`routectl_providers::anthropic_api::emitted_thinking_type`, with the
    /// operator's `supports_adaptive_thinking` the egress itself is handed), not
    /// the final body: a later body pass can still strip `thinking` (a forced
    /// tool choice, a context-management cache miss). A client
    /// `output_config.effort` of "none" does not change the type: the egress
    /// forwards that effort as sent beside whatever type ships. That gap is
    /// sound for the uses this surface has. A rejection naming
    /// `thinking.type.X` is evidence that X was on the wire, so a stripped body
    /// never draws the rejection a repair acts on. The read also stages and
    /// verifies the rewrite target, and none of the late passes can turn a
    /// rewritten type back into the refused one: they only remove thinking,
    /// never re-enable or retype it.
    ///
    /// The rewrite replaces the native carrier with `{type: <target>}`
    /// (materializing one when the request carried none): an adaptive target
    /// keeps only `display`, which has no budget field to carry over, and any
    /// other target is written bare. An adaptive target with no positive effort
    /// on the request also gets the code-owned companion effort
    /// [`COMPANION_EFFORT`], written to the canonical effort and to a client
    /// `output_config.effort` the egress would otherwise re-merge over it.
    /// Never dropped: a request with no thinking at all thinks at the model's
    /// default effort.
    AnthropicThinkingType(&'static str),
    /// A canonical sampling parameter every egress emits from one request
    /// field when it is set; see [`SamplingParam`].
    SamplingParam(SamplingParam),
    /// The wire `max_tokens`, PRESENT when the client sent one or the egress
    /// fills it (`routectl_internal.fills_max_tokens`) from the model's
    /// ceiling, else its baseline. The client's value wins on the wire, so it
    /// is the emitted value whenever it is set.
    /// Never dropped and never probed; a ceiling repair lowers the emitted
    /// value (see [`Self::clamp_to`]).
    MaxTokens,
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
    /// Test-only ENVELOPE surface for a field the upstream refuses outright:
    /// the `user` field, when it holds exactly [`TEST_UNSUPPORTED_SENTINEL`].
    ///
    /// The one grounded row's captured rejection refuses a VALUE, so without
    /// this the path on which a parsed rejection commits a verdict has no row
    /// to act on. Sentinel-gated for the same reason as the prefix-impacting
    /// surface, and disjoint from both other carriers.
    #[cfg(test)]
    UnsupportedFieldForTests,
    /// Test-only surface for a REWRITE row: the [`TEST_REWRITE_KWARG`] key of
    /// `chat_template_kwargs`. A key no other fixture sets, so the row stays
    /// confined to the tests that opt into it.
    #[cfg(any(test, feature = "test-utils"))]
    RewriteFieldForTests,
    /// Test-only REWRITE surface whose write never reads back: present while
    /// the [`TEST_NON_TAKING_KWARG`] key of `chat_template_kwargs` is set, and
    /// its write lands on a different key. The grounded surfaces always read
    /// back what they write, so without this the rewrite post-condition is
    /// unreachable.
    #[cfg(test)]
    NonTakingRewriteForTests,
    /// Test-only surface for a CEILING row: `max_tokens` holding exactly
    /// [`TEST_CEILING_SENTINEL`].
    #[cfg(test)]
    CeilingFieldForTests,
    /// Test-only surface for a DROP row a free token count validates: the
    /// [`TEST_RECHECK_KWARG`] key of `chat_template_kwargs`. The grounded row
    /// does not validate on a count, so without this the re-check trigger has
    /// no row to fire on. Also compiled under `test-utils`, so the probe tests
    /// of other crates can drive a re-check end to end through a Router that
    /// installs it ([`FieldRepairTable::with_synthetic_rows`]).
    #[cfg(any(test, feature = "test-utils"))]
    RecheckFieldForTests,
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
            Self::AnthropicThinkingType(refused) => {
                emitted_thinking_type(req).as_deref() == Some(*refused)
            }
            Self::SamplingParam(param) => param.is_set(req),
            Self::MaxTokens => emitted_max_tokens(req).is_some(),
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
            #[cfg(test)]
            Self::UnsupportedFieldForTests => {
                req.user.as_deref() == Some(TEST_UNSUPPORTED_SENTINEL)
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::RewriteFieldForTests => req
                .chat_template_kwargs
                .as_ref()
                .is_some_and(|kwargs| kwargs.get(TEST_REWRITE_KWARG).is_some()),
            #[cfg(test)]
            Self::NonTakingRewriteForTests => {
                Self::test_kwarg(req, TEST_NON_TAKING_KWARG).is_some()
            }
            #[cfg(test)]
            Self::CeilingFieldForTests => req.max_tokens == Some(TEST_CEILING_SENTINEL),
            #[cfg(any(test, feature = "test-utils"))]
            Self::RecheckFieldForTests => Self::test_kwarg(req, TEST_RECHECK_KWARG).is_some(),
        }
    }

    /// The string value of `chat_template_kwargs[kwarg]`, for the test-only
    /// kwarg surfaces.
    #[cfg(any(test, feature = "test-utils"))]
    fn test_kwarg<'r>(req: &'r ChatRequest, kwarg: &str) -> Option<&'r str> {
        req.chat_template_kwargs
            .as_ref()
            .and_then(|kwargs| kwargs.get(kwarg))
            .and_then(serde_json::Value::as_str)
    }

    /// Set `chat_template_kwargs[kwarg]` to `value`, for the test-only kwarg
    /// surfaces.
    #[cfg(any(test, feature = "test-utils"))]
    fn set_test_kwarg(req: &mut ChatRequest, kwarg: &str, value: &str) {
        let kwargs = req
            .chat_template_kwargs
            .get_or_insert_with(|| serde_json::json!({}));
        if let Some(object) = kwargs.as_object_mut() {
            object.insert(kwarg.to_string(), value.into());
        }
    }

    /// The VALUE `req` carries for this surface, for the probe payload.
    ///
    /// Read through the surface itself rather than by a caller reaching into
    /// the request, so the probe asks about the same carrier the repair
    /// would drop. Bounded: the carrier is a short display token, and the
    /// derived boolean renders as its canonical spelling.
    fn value_in(&self, req: &ChatRequest) -> Option<String> {
        match self {
            Self::AnthropicThinkingDisplay => req
                .routectl_internal
                .anthropic_thinking_display
                .clone()
                .or_else(|| {
                    req.reasoning
                        .as_ref()
                        .and_then(|r| r.exclude)
                        .map(|exclude| if exclude { "omitted" } else { "summarized" }.to_string())
                }),
            Self::AnthropicThinkingType(_) => emitted_thinking_type(req)
                .map(|emitted| format!("{THINKING_TYPE_VALUE_PREFIX}{emitted}")),
            // No probe payload: no token count validates a sampling parameter
            // or an output token limit.
            Self::SamplingParam(_) | Self::MaxTokens => None,
            #[cfg(all(test, not(debug_assertions)))]
            Self::DivergentForTests => None,
            // No probe payload: the test-only surfaces are never rows of
            // `FIELD_REPAIRS`, the only table a probe payload is built from,
            // so no probe is ever built from one.
            #[cfg(test)]
            Self::PartialDropForTests
            | Self::PrefixImpactingSystemForTests
            | Self::ClaimsRemovalForTests
            | Self::UnsupportedFieldForTests
            | Self::CeilingFieldForTests => None,
            #[cfg(any(test, feature = "test-utils"))]
            Self::RewriteFieldForTests => {
                Self::test_kwarg(req, TEST_REWRITE_KWARG).map(str::to_string)
            }
            #[cfg(test)]
            Self::NonTakingRewriteForTests => {
                Self::test_kwarg(req, TEST_NON_TAKING_KWARG).map(str::to_string)
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::RecheckFieldForTests => {
                Self::test_kwarg(req, TEST_RECHECK_KWARG).map(str::to_string)
            }
        }
    }

    /// Set this surface's carrier on `req` to `value`, for the probe body.
    ///
    /// The inverse of [`Self::drop_from`] over the same carrier, so a probe
    /// asks about exactly the field a repair would remove.
    fn set_in(&self, req: &mut ChatRequest, value: &str) {
        match self {
            Self::AnthropicThinkingDisplay => {
                req.routectl_internal.anthropic_thinking_display = Some(value.to_string());
                // The carrier alone does NOT reach the wire. The Anthropic
                // normalizer's `build_thinking` returns early when
                // `req.reasoning` is absent, and again when the caller asked
                // for no thinking, so a body carrying only the carrier
                // serializes with no `thinking` object at all -- the probe
                // would ask a plain token count and read its success as
                // evidence about a field it never sent.
                //
                // So the minimal canonical state the normalizer requires is
                // set alongside: reasoning present, thinking explicitly
                // active, and a `max_tokens` for the budget window the legacy
                // shape clamps into. Only filled where absent, so a caller
                // that already expressed a reasoning state keeps it.
                let reasoning = req.reasoning.get_or_insert_with(Default::default);
                if reasoning.enabled.is_none()
                    && reasoning.effort.is_none()
                    && reasoning.max_tokens.is_none()
                {
                    reasoning.enabled = Some(true);
                }
                if req.max_tokens.is_none() {
                    req.max_tokens = Some(PROBE_THINKING_MAX_TOKENS);
                }
            }
            Self::AnthropicThinkingType(_) => write_thinking_type(req, value),
            Self::SamplingParam(_) | Self::MaxTokens => {}
            #[cfg(all(test, not(debug_assertions)))]
            Self::DivergentForTests => {}
            #[cfg(test)]
            Self::PartialDropForTests
            | Self::PrefixImpactingSystemForTests
            | Self::ClaimsRemovalForTests
            | Self::UnsupportedFieldForTests
            | Self::CeilingFieldForTests => {}
            #[cfg(any(test, feature = "test-utils"))]
            Self::RewriteFieldForTests => Self::set_test_kwarg(req, TEST_REWRITE_KWARG, value),
            #[cfg(test)]
            Self::NonTakingRewriteForTests => {
                Self::set_test_kwarg(req, TEST_NON_TAKING_SHADOW_KWARG, value);
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::RecheckFieldForTests => Self::set_test_kwarg(req, TEST_RECHECK_KWARG, value),
        }
    }

    /// True when `req` already carries exactly `value` on this surface.
    pub(super) fn carries(&self, req: &ChatRequest, value: &str) -> bool {
        self.value_in(req).as_deref() == Some(value)
    }

    /// Rewrite this surface's carrier on `req` to `value`, adopting the change
    /// only when the surface then reads back exactly `value`. `false` leaves
    /// `req` untouched, and is also the answer when `req` already carries
    /// `value`: the upstream refused the value it would re-send, so the
    /// retry would repeat the rejection rather than repair it.
    ///
    /// Staged on a clone because [`Self::set_in`] cannot report failure, and a
    /// rewrite that did not take must not leave a half-written body behind.
    pub(super) fn rewrite_in(&self, req: &mut ChatRequest, value: &str) -> bool {
        if self.carries(req, value) {
            return false;
        }
        let mut scratch = req.clone();
        self.set_in(&mut scratch, value);
        if self.value_in(&scratch).as_deref() != Some(value) {
            return false;
        }
        if !self.rewrite_is_complete(&scratch, value) {
            return false;
        }
        *req = scratch;
        true
    }

    /// What a rewrite to `value` needs beyond reading `value` back. An adaptive
    /// thinking type is only half a shape: without a positive effort the
    /// upstream either refuses it or thinks at its default, so a rewrite that
    /// left none would re-send a shape the repair did not mean to send.
    fn rewrite_is_complete(&self, rewritten: &ChatRequest, value: &str) -> bool {
        match self {
            Self::AnthropicThinkingType(_) => {
                thinking_target(value) != Some(ADAPTIVE_THINKING_TYPE)
                    || carries_positive_effort(rewritten)
            }
            _ => true,
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
            Self::AnthropicThinkingType(_) => false,
            Self::SamplingParam(param) => param.take(req),
            Self::MaxTokens => false,
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
            #[cfg(test)]
            Self::UnsupportedFieldForTests => {
                if !Self::UnsupportedFieldForTests.present_in(req) {
                    return false;
                }
                req.user.take().is_some()
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::RewriteFieldForTests => req
                .chat_template_kwargs
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|object| object.remove(TEST_REWRITE_KWARG))
                .is_some(),
            #[cfg(test)]
            Self::NonTakingRewriteForTests => false,
            #[cfg(test)]
            Self::CeilingFieldForTests => {
                if !Self::CeilingFieldForTests.present_in(req) {
                    return false;
                }
                req.max_tokens.take().is_some()
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::RecheckFieldForTests => req
                .chat_template_kwargs
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|object| object.remove(TEST_RECHECK_KWARG))
                .is_some(),
        }
    }

    /// Lower this surface's token limit on `req` to `ceiling`, returning
    /// whether it changed. Only a nonzero ceiling strictly below the value the
    /// request emits is applied: a ceiling at or above it names a limit the
    /// request already meets, so the retry would repeat the rejection, and a
    /// clamp never raises a value.
    ///
    /// The value is written where the emitted one came from: the client's
    /// `max_tokens` when it sent one, otherwise the ceiling slot the egress
    /// fills from, so a client that sent none still sends none and the egress
    /// keeps declaring its own default. With no client value [`Self::clamp_lowers`]
    /// admits only an egress that fills.
    pub(super) const fn clamp_to(&self, req: &mut ChatRequest, ceiling: u32) -> bool {
        if !self.clamp_lowers(req, ceiling) {
            return false;
        }
        if req.max_tokens.is_some() {
            req.max_tokens = Some(ceiling);
        } else {
            req.routectl_internal.max_output_tokens = ceiling;
        }
        true
    }

    /// Whether [`Self::clamp_to`] with `ceiling` would lower the token limit
    /// `req` emits on this surface. The one statement of that rule, so a
    /// caller deciding ahead of the clamp cannot disagree with it.
    pub(super) const fn clamp_lowers(&self, req: &ChatRequest, ceiling: u32) -> bool {
        let clampable = match self {
            Self::MaxTokens => true,
            #[cfg(test)]
            Self::CeilingFieldForTests => matches!(req.max_tokens, Some(TEST_CEILING_SENTINEL)),
            _ => false,
        };
        match emitted_max_tokens(req) {
            Some(emitted) => clampable && 0 < ceiling && ceiling < emitted,
            None => false,
        }
    }

    /// Whether [`Self::clamp_to`] on `req` writes the ceiling slot the egress
    /// fills from rather than a value the client sent. Such a clamp changes no client field, and
    /// the egress already declares the wire field as its own default.
    pub(super) const fn clamp_writes_fill(&self, req: &ChatRequest) -> bool {
        matches!(self, Self::MaxTokens) && req.max_tokens.is_none()
    }
}

/// The `max_tokens` the request's egress sends: the client's value; else,
/// on an egress that fills it, the model's nonzero ceiling or the egress
/// baseline; else none.
const fn emitted_max_tokens(req: &ChatRequest) -> Option<u32> {
    let internal = &req.routectl_internal;
    match (
        req.max_tokens,
        internal.fills_max_tokens,
        internal.max_output_tokens,
    ) {
        (Some(client), _, _) => Some(client),
        (None, false, _) => None,
        (None, true, 0) => Some(routectl_providers::anthropic_api::DEFAULT_MAX_OUTPUT_TOKENS),
        (None, true, ceiling) => Some(ceiling),
    }
}

/// A sampling parameter a [`FieldSurface::SamplingParam`] row drops: one
/// canonical request field, PRESENT when set. Every egress emits it from that
/// field alone (the thinking clamp only rewrites a value the client sent), so
/// taking it removes it from the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SamplingParam {
    Temperature,
    TopP,
    PresencePenalty,
    FrequencyPenalty,
}

impl SamplingParam {
    const fn is_set(self, req: &ChatRequest) -> bool {
        match self {
            Self::Temperature => req.temperature.is_some(),
            Self::TopP => req.top_p.is_some(),
            Self::PresencePenalty => req.presence_penalty.is_some(),
            Self::FrequencyPenalty => req.frequency_penalty.is_some(),
        }
    }

    const fn take(self, req: &mut ChatRequest) -> bool {
        match self {
            Self::Temperature => req.temperature.take().is_some(),
            Self::TopP => req.top_p.take().is_some(),
            Self::PresencePenalty => req.presence_penalty.take().is_some(),
            Self::FrequencyPenalty => req.frequency_penalty.take().is_some(),
        }
    }

    /// The parameter's pointer on a client wire that spells it at the root.
    pub(super) const fn wire_pointer(self) -> &'static str {
        match self {
            Self::Temperature => "/temperature",
            Self::TopP => "/top_p",
            Self::PresencePenalty => "/presence_penalty",
            Self::FrequencyPenalty => "/frequency_penalty",
        }
    }
}

/// Prefix of every `thinking.type` row's path and accepted value: the dotted
/// path an upstream names for one thinking type.
const THINKING_TYPE_VALUE_PREFIX: &str = "thinking.type.";

/// The thinking type that takes no budget and pairs with an effort.
const ADAPTIVE_THINKING_TYPE: &str = "adaptive";

/// The effort a `thinking.type` rewrite to adaptive supplies when the request
/// carries no positive effort. The lowest grounded level that keeps adaptive
/// thinking accepted; never persisted and never learned, so it is not part of
/// the verdict.
pub(super) const COMPANION_EFFORT: &str = "low";

/// The thinking type an Anthropic-shape egress composes for `req`, with the
/// operator knob that egress is handed.
fn emitted_thinking_type(req: &ChatRequest) -> Option<String> {
    routectl_providers::anthropic_api::emitted_thinking_type(
        req,
        req.routectl_internal.supports_adaptive_thinking,
    )
}

/// The thinking type a `thinking.type.<type>` row value names.
fn thinking_target(value: &str) -> Option<&str> {
    value
        .strip_prefix(THINKING_TYPE_VALUE_PREFIX)
        .filter(|target| !target.is_empty())
}

/// The client's `output_config.effort`, as the forward-compat extras carry it.
fn extras_effort(req: &ChatRequest) -> Option<&serde_json::Value> {
    req.provider_extras
        .as_ref()?
        .get("output_config")?
        .get("effort")
}

/// Whether the body `req` composes carries a positive effort. The egress
/// merges the client's `output_config` over the one it derives from the
/// canonical effort, so a client `output_config.effort` decides when present
/// and the canonical effort decides otherwise.
pub(super) fn carries_positive_effort(req: &ChatRequest) -> bool {
    let effective = match extras_effort(req) {
        Some(sent) => sent.as_str(),
        None => req.reasoning.as_ref().and_then(|r| r.effort.as_deref()),
    };
    effective.is_some_and(|effort| !routectl_core::ReasoningConfig::is_off_effort(effort))
}

/// Replace the native thinking carrier with `{type: <target>}` for the row
/// value `value`; see [`FieldSurface::AnthropicThinkingType`]. A value naming
/// no type writes nothing, which the caller's read-back refuses.
fn write_thinking_type(req: &mut ChatRequest, value: &str) {
    let Some(target) = thinking_target(value) else {
        return;
    };
    let Some(object) = routectl_core::ThinkingObject::from_client(serde_json::json!({
        "type": target,
    })) else {
        return;
    };
    if target == ADAPTIVE_THINKING_TYPE {
        keep_display_on_the_carrier(req);
        if !carries_positive_effort(req) {
            write_companion_effort(req);
        }
    } else {
        req.routectl_internal.anthropic_thinking_display = None;
    }
    req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::Sent(object);
}

/// A forwarded carrier emits `display` from its own carrier only, so a
/// display a translated request expressed through the canonical `exclude`
/// moves onto that carrier before the carrier replaces the translation.
fn keep_display_on_the_carrier(req: &mut ChatRequest) {
    if req.routectl_internal.anthropic_thinking_display.is_some() {
        return;
    }
    let derived = req
        .reasoning
        .as_ref()
        .and_then(|r| r.exclude)
        .map(|exclude| if exclude { "omitted" } else { "summarized" });
    req.routectl_internal.anthropic_thinking_display = derived.map(str::to_string);
}

/// Write [`COMPANION_EFFORT`] to the canonical effort and over a client
/// `output_config.effort`. Both, because the egress derives the wire effort
/// from the canonical one and then merges the client's `output_config` over
/// it, forwarding a client "none" as sent: left there, a non-positive effort
/// would ship beside the repaired adaptive type, a pairing the companion
/// replaces so the retry is one the upstream accepts.
fn write_companion_effort(req: &mut ChatRequest) {
    req.reasoning.get_or_insert_with(Default::default).effort = Some(COMPANION_EFFORT.to_string());
    if let Some(config) = req
        .provider_extras
        .as_mut()
        .and_then(|extras| extras.get_mut("output_config"))
        .and_then(serde_json::Value::as_object_mut)
        && config.contains_key("effort")
    {
        config.insert("effort".to_string(), COMPANION_EFFORT.into());
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
    /// ships envelope rows only, and the first prefix-impacting row lands with
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

/// What a repair does to a row's field once a rejection names it.
///
/// [`TransformClass`] says what a transform COSTS the request; this says what
/// it DOES. Closed: a new kind of repair is a new variant, never a value
/// derived from upstream text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FieldTransform {
    /// Remove the field.
    Drop,
    /// Replace the field's value with the ONE member of `accepted` the
    /// rejection names. A rejection naming none, or several, repairs nothing:
    /// the substituted value is always the upstream's own answer.
    Rewrite { accepted: &'static [&'static str] },
    /// Lower a token limit to the ceiling the rejection names, only when that
    /// ceiling is below the value sent. Repaired for the attempt, and a learned
    /// ceiling also lowers a later request's limit before dispatch; it never
    /// raises one.
    Ceiling,
}

impl FieldTransform {
    /// The closed set a persisted value for a row with this transform is
    /// decoded against, or `None` for a transform that carries no value.
    const fn value_slot(self) -> Option<ValueSlot> {
        match self {
            Self::Drop => None,
            Self::Rewrite { accepted } => Some(ValueSlot::Shape(accepted)),
            Self::Ceiling => Some(ValueSlot::Ceiling),
        }
    }
}

/// One row of the closed path-to-surface table: the qualified dotted path,
/// the request-side surface carrying it, what a repair does to it, the class
/// that decides which gates a pre-flight application must clear, and whether
/// a free token count validates the field.
#[derive(Clone, Copy, Debug)]
pub(super) struct FieldRepairRow {
    pub(super) path: &'static str,
    pub(super) surface: FieldSurface,
    pub(super) transform: FieldTransform,
    pub(super) class: TransformClass,
    /// Whether the upstream's token-count endpoint rejects this field the way
    /// a completion does, so a free count can re-check a verdict on it. Only
    /// such a row is ever probed.
    pub(super) count_tokens_validates: bool,
}

/// The CLOSED path-to-surface table: one row per qualified dotted path this
/// build knows how to repair.
///
/// Each path is one a real captured rejection envelope named, and its surface
/// is the request-side carrier that produces that wire field. A path outside
/// this table resolves to no row, so it mutates nothing and repairs nothing
/// -- the table is the whole of what an L0 repair can act on.
///
/// `count_tokens_validates` is false on every row: no captured token count
/// has rejected any of these fields, so a count accepting one proves nothing. The
/// count endpoint also carries no `output_config`, so it cannot validate an
/// adaptive thinking type and the effort it pairs with.
const FIELD_REPAIRS: &[FieldRepairRow] = &[
    FieldRepairRow {
        path: "thinking.enabled.display",
        surface: FieldSurface::AnthropicThinkingDisplay,
        transform: FieldTransform::Drop,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "thinking.type.enabled",
        surface: FieldSurface::AnthropicThinkingType("enabled"),
        transform: FieldTransform::Rewrite {
            accepted: &["thinking.type.adaptive"],
        },
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "thinking.type.disabled",
        surface: FieldSurface::AnthropicThinkingType("disabled"),
        transform: FieldTransform::Rewrite {
            accepted: &["thinking.type.adaptive", "thinking.type.between_tools"],
        },
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "temperature",
        surface: FieldSurface::SamplingParam(SamplingParam::Temperature),
        transform: FieldTransform::Drop,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "top_p",
        surface: FieldSurface::SamplingParam(SamplingParam::TopP),
        transform: FieldTransform::Drop,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "presence_penalty",
        surface: FieldSurface::SamplingParam(SamplingParam::PresencePenalty),
        transform: FieldTransform::Drop,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "frequency_penalty",
        surface: FieldSurface::SamplingParam(SamplingParam::FrequencyPenalty),
        transform: FieldTransform::Drop,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: "max_tokens",
        surface: FieldSurface::MaxTokens,
        transform: FieldTransform::Ceiling,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
];

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

/// The qualified dotted path of the test-only refused-field row. Outside the
/// grounded table and every committed rejection fixture's paths.
#[cfg(test)]
pub(in crate::router) const TEST_UNSUPPORTED_PATH: &str = "routectl_test_unsupported_field";

/// The EXACT `user` value the test-only refused-field surface recognizes.
#[cfg(test)]
pub(in crate::router) const TEST_UNSUPPORTED_SENTINEL: &str =
    "routectl-test-only-unsupported-field-sentinel";

/// The qualified dotted path of the test-only rewrite row.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub const TEST_REWRITE_PATH: &str = "routectl_test_rewrite_field";

/// The `chat_template_kwargs` key the test-only rewrite surface reads and
/// writes.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub const TEST_REWRITE_KWARG: &str = "routectl_test_rewrite_kwarg";

/// The test-only rewrite row's closed accepted set. Two members, so a
/// rejection can name both.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub const TEST_REWRITE_ACCEPTED: &[&str] =
    &["routectl-test-accepted-a", "routectl-test-accepted-b"];

/// The qualified dotted path of the test-only rewrite row whose write never
/// reads back.
#[cfg(test)]
pub(in crate::router) const TEST_NON_TAKING_PATH: &str = "routectl_test_non_taking";

/// The `chat_template_kwargs` key the non-taking rewrite surface reads.
#[cfg(test)]
pub(in crate::router) const TEST_NON_TAKING_KWARG: &str = "routectl_test_non_taking_kwarg";

/// The key the non-taking rewrite surface writes, which its read never sees.
#[cfg(test)]
pub(in crate::router) const TEST_NON_TAKING_SHADOW_KWARG: &str =
    "routectl_test_non_taking_shadow_kwarg";

/// The non-taking rewrite row's closed accepted set.
#[cfg(test)]
pub(in crate::router) const TEST_NON_TAKING_ACCEPTED: &[&str] = &["routectl-test-non-taking"];

/// The qualified dotted path of the test-only row a free count validates.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub const TEST_RECHECK_PATH: &str = "routectl_test_recheck_field";

/// The `chat_template_kwargs` key the test-only re-check surface reads and
/// writes.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub const TEST_RECHECK_KWARG: &str = "routectl_test_recheck_kwarg";

/// The qualified dotted path of the test-only ceiling row.
#[cfg(test)]
pub(in crate::router) const TEST_CEILING_PATH: &str = "routectl_test_ceiling_field";

/// The EXACT `max_tokens` the test-only ceiling surface recognizes.
#[cfg(test)]
pub(in crate::router) const TEST_CEILING_SENTINEL: u32 = 987_653;

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
const TEST_FIELD_REPAIRS: &[FieldRepairRow] = &[
    FieldRepairRow {
        path: TEST_PREFIX_IMPACTING_PATH,
        surface: FieldSurface::PrefixImpactingSystemForTests,
        transform: FieldTransform::Drop,
        class: TransformClass::PrefixImpacting,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: TEST_UNSUPPORTED_PATH,
        surface: FieldSurface::UnsupportedFieldForTests,
        transform: FieldTransform::Drop,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: TEST_CEILING_PATH,
        surface: FieldSurface::CeilingFieldForTests,
        transform: FieldTransform::Ceiling,
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
    FieldRepairRow {
        path: TEST_NON_TAKING_PATH,
        surface: FieldSurface::NonTakingRewriteForTests,
        transform: FieldTransform::Rewrite {
            accepted: TEST_NON_TAKING_ACCEPTED,
        },
        class: TransformClass::Envelope,
        count_tokens_validates: false,
    },
];

/// The test-only rewrite row, which [`FieldRepairTable::with_synthetic_rows`]
/// appends: no grounded row writes a value, so this is the only row a valued
/// verdict can be learned, persisted, or restated through in another crate's
/// tests.
#[cfg(any(test, feature = "test-utils"))]
const TEST_REWRITE_FIELD_REPAIRS: &[FieldRepairRow] = &[FieldRepairRow {
    path: TEST_REWRITE_PATH,
    surface: FieldSurface::RewriteFieldForTests,
    transform: FieldTransform::Rewrite {
        accepted: TEST_REWRITE_ACCEPTED,
    },
    class: TransformClass::Envelope,
    count_tokens_validates: true,
}];

/// The test-only row a free token count validates, which
/// [`FieldRepairTable::with_synthetic_rows`] appends: no grounded row is
/// count-validated, so this is the only row a re-check probe can be built from
/// in any test, in this crate or another.
#[cfg(any(test, feature = "test-utils"))]
const TEST_RECHECK_FIELD_REPAIRS: &[FieldRepairRow] = &[FieldRepairRow {
    path: TEST_RECHECK_PATH,
    surface: FieldSurface::RecheckFieldForTests,
    transform: FieldTransform::Drop,
    class: TransformClass::Envelope,
    count_tokens_validates: true,
}];

/// The grounded rows alone, without the rows a test build appends.
#[cfg(test)]
pub(super) const fn grounded_table() -> &'static [FieldRepairRow] {
    FIELD_REPAIRS
}

/// The rows of [`FieldRepairTable::default`], for unit tests that read the
/// table without a router.
#[cfg(test)]
pub(super) fn closed_table() -> &'static [FieldRepairRow] {
    FieldRepairTable::default().rows()
}

/// The closed table one router scans: which rows a rejection can name, a
/// request can carry, and a persisted verdict can resolve to.
///
/// A value rather than a global so the synthetic rows a test needs reach only
/// the router that asked for them. Outside a unit-test build the default is
/// the grounded rows alone, whatever features are enabled; the synthetic
/// rewrite and re-check rows arrive only through
/// `Router::with_synthetic_field_rows_for_tests`.
#[derive(Clone, Copy, Debug)]
pub struct FieldRepairTable {
    rows: &'static [FieldRepairRow],
}

impl Default for FieldRepairTable {
    /// The grounded rows. A unit-test build also appends every test row, so
    /// the in-crate suites reach each branch without opting in per router.
    fn default() -> Self {
        #[cfg(test)]
        {
            Self::with_synthetic_rows()
        }
        #[cfg(not(test))]
        {
            Self {
                rows: FIELD_REPAIRS,
            }
        }
    }
}

impl FieldRepairTable {
    /// The grounded rows, then [`TEST_FIELD_REPAIRS`] (unit tests only), then
    /// [`TEST_REWRITE_FIELD_REPAIRS`] and [`TEST_RECHECK_FIELD_REPAIRS`], in
    /// that order, so a grounded row keeps its production precedence and the
    /// added rows sit behind it.
    ///
    /// The divergent surface is deliberately NOT a row here: its presence
    /// predicate answers `true` unconditionally, so a row for it would make
    /// every request carry it. It reaches `apply`'s fail-closed branch through
    /// [`FieldRepairPlan::apply_candidate`] instead.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn with_synthetic_rows() -> Self {
        use std::sync::LazyLock;

        static ROWS: LazyLock<Vec<FieldRepairRow>> = LazyLock::new(|| {
            let rows = FIELD_REPAIRS.iter();
            #[cfg(test)]
            let rows = rows.chain(TEST_FIELD_REPAIRS);
            rows.chain(TEST_REWRITE_FIELD_REPAIRS)
                .chain(TEST_RECHECK_FIELD_REPAIRS)
                .copied()
                .collect()
        });
        Self {
            rows: LazyLock::force(&ROWS).as_slice(),
        }
    }

    /// The grounded rows alone, which a unit-test build's default is not.
    #[cfg(test)]
    pub(crate) const fn grounded_only() -> Self {
        Self {
            rows: FIELD_REPAIRS,
        }
    }

    #[cfg(test)]
    pub(crate) const fn row_count(self) -> usize {
        self.rows.len()
    }

    pub(super) const fn rows(self) -> &'static [FieldRepairRow] {
        self.rows
    }

    /// The row whose path is exactly `path`.
    pub(super) fn row_for_path(self, path: &str) -> Option<&'static FieldRepairRow> {
        self.rows.iter().find(|row| row.path == path)
    }

    /// The row a `field:`-namespaced capability key names.
    fn row_for_capability_key(self, key: &str) -> Option<&'static FieldRepairRow> {
        self.rows.iter().find(|row| {
            crate::field_capability::field_capability_key(row.path).as_deref() == Some(key)
        })
    }
}

/// `max_tokens` a probe body carries so the legacy Anthropic thinking shape
/// has a budget window to clamp into.
///
/// Small on purpose: `count_tokens` bills nothing and never generates, so
/// this only has to clear Anthropic's `[1024, max_tokens-1]` budget window.
const PROBE_THINKING_MAX_TOKENS: u32 = 2048;

/// Apply a probe payload's closed-table field and value onto `req`.
///
/// Resolved against the closed table, so a payload naming a path outside it
/// sets nothing and no probe can assemble an arbitrary envelope field. Which
/// rows a probe may ASK about is decided where payloads are captured
/// ([`recheck_payloads`]), the only production source of one.
pub(super) fn apply_probe_payload(
    table: FieldRepairTable,
    req: &mut ChatRequest,
    payload: &crate::probe_scheduler::ProbePayload,
) {
    let Some(row) = table.row_for_path(payload.field_path()) else {
        return;
    };
    row.surface.set_in(req, payload.field_value());
}

/// The count-validated closed-table rows `req` carries, each with the VALUE it
/// carries for that row -- the bounded capture a re-check probe is built from.
///
/// One accessor rather than a path list plus a separate value lookup: the
/// two must describe the same carrier, and splitting them is how a probe
/// comes to name one field while sending another's value.
pub(super) fn recheck_payloads(
    table: FieldRepairTable,
    req: &ChatRequest,
) -> impl Iterator<Item = (&'static str, String)> + use<'_> {
    table
        .rows()
        .iter()
        .filter(|row| row.count_tokens_validates)
        .filter_map(|row| row.surface.value_in(req).map(|value| (row.path, value)))
}

/// The provider kinds whose rejections a field stage may act on: each has a
/// rejection dialect in the parser and an egress the attributability decision
/// can classify. Every other kind is refused by [`attributable_field_lane`]
/// and by the lane gates that call [`field_repair_admits_kind`].
///
/// Absent on purpose: `openai-responses` (no rejection dialect in the parser,
/// so a lane on it could only ever mint from evidence nothing localized) and
/// `gemini`.
const FIELD_REPAIR_KINDS: &[&str] = &[ANTHROPIC_API_KIND, BEDROCK_KIND, OPENAI_COMPAT_KIND];

/// `bedrock`, as `ProviderEntry::kind_str` spells it.
const BEDROCK_KIND: &str = routectl_core::provider_kind::config::BEDROCK;

/// `openai-compat`, as `ProviderEntry::kind_str` spells it.
const OPENAI_COMPAT_KIND: &str = routectl_core::provider_kind::config::OPENAI_COMPAT;

/// Whether a field stage may act on a lane of `provider_kind` at all.
pub(super) fn field_repair_admits_kind(provider_kind: &str) -> bool {
    FIELD_REPAIR_KINDS.contains(&provider_kind)
}

/// What the attributability decision admitted: the entry's kind and the base
/// URL it egresses to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttributableLane<'a> {
    pub(super) provider_kind: &'static str,
    pub(super) base_url: std::borrow::Cow<'a, str>,
}

/// THE single attributability decision every field stage makes: the reactive
/// `plan_field_carry`, the pre-flight planner, the fidelity status read,
/// probe activation, and the probe's pre-dial recheck (the two probe stages
/// further narrow the answer to a kind and upstream with a free token count:
/// `anthropic-api`, and `bedrock` on an Anthropic-family model -- see
/// `count_tokens::seat_can_count_tokens`).
///
/// Answers `Some` only when ALL of:
///
/// - the caller's own forwarded-credential fact is false. A target
///   authenticating with the CLIENT's bearer must not have its rejections
///   attributed to a routectl-owned seat: one client's rejection would mint a
///   permanent verdict steering every other client;
/// - the operator's OWN `[providers]` entry for `provider_name` exists;
/// - that entry's kind is one of [`FIELD_REPAIR_KINDS`];
/// - the entry's effective egress is classifiable from its configured fields
///   (`ProviderEntry::classifiable_egress_base_url`). The Mantle sub-lanes
///   are excluded here: their configured base URL is not where they egress;
/// - that base URL does not name a LOOPBACK destination.
///
/// The forwarded fact is a PARAMETER rather than a per-site check because its
/// two sources differ -- the dispatch arms read a target's own
/// `use_forwarded_credential`, the probe stages have no target and read the
/// entry's `forwarded_base_url()` -- while the DECISION they feed must be one
/// decision. Passing the fact in is what lets the refusal set live here
/// whole: a stage that is narrower than another is exactly the asymmetry that
/// mints a verdict from evidence the acting arm had already rejected.
///
/// Read from the operator's entry rather than from a dispatch target because
/// the suppression predicate keys on the configured base URL, which no target
/// carries.
pub(super) fn attributable_field_lane<'a>(
    config: &'a crate::config::Config,
    provider_name: &str,
    use_forwarded_credential: bool,
) -> Option<AttributableLane<'a>> {
    if use_forwarded_credential {
        return None;
    }
    let entry = config.providers.get(provider_name)?;
    let provider_kind = entry.kind_str();
    if !field_repair_admits_kind(provider_kind) {
        return None;
    }
    let base_url = entry.classifiable_egress_base_url()?;
    if crate::field_verdict::loopback_target_suppresses_minting(&base_url) {
        return None;
    }
    Some(AttributableLane {
        provider_kind,
        base_url,
    })
}

/// The transform class of the closed-table row a `field:`-namespaced capability
/// key names, or `None` for a key this build's table carries no row for.
pub(super) fn transform_class_of_capability_key(
    table: FieldRepairTable,
    key: &str,
) -> Option<TransformClass> {
    table.row_for_capability_key(key).map(|row| row.class)
}

/// Whether a negative on `feature_key` for the lane `state_key` of
/// `provider_kind` is recovered ONLY by the free token-count re-check.
///
/// True for a closed-table row a free count validates, on a kind and upstream
/// that has such a count. Such a negative does not decay into the generic
/// re-probe at its deadline: an ordinary successful completion that carried the
/// field cannot clear it, and only the queued re-check, acknowledged durable,
/// retracts it -- until [`crate::RECHECK_BACKSTOP`] past the deadline, when
/// `negative_lapsed` lets it lapse like any other.
/// Every other key -- a non-validated row, a lane with no free count to ask, a
/// catalog capability, a beta flag -- keeps its decay.
pub fn negative_recovers_only_by_recheck(
    table: FieldRepairTable,
    state_key: &str,
    feature_key: &str,
    provider_kind: &str,
) -> bool {
    let validated = table
        .row_for_capability_key(feature_key)
        .is_some_and(|row| row.count_tokens_validates);
    validated
        && crate::state_key::StateKey::parse(state_key).is_some_and(|lane| {
            super::count_tokens::seat_can_count_tokens(Some(provider_kind), lane.upstream())
        })
}

/// The value slot of the closed-table row a `field:`-namespaced capability key
/// names, or `None` when no row of this build's table names it or the row's
/// transform carries no value. What decides whether a persisted value token is
/// read at all: every other key's token is ignored.
pub fn value_slot_for_capability_key(table: FieldRepairTable, key: &str) -> Option<ValueSlot> {
    table
        .row_for_capability_key(key)
        .and_then(|row| row.transform.value_slot())
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
pub(super) fn present_rows(
    table: FieldRepairTable,
    req: &ChatRequest,
) -> impl Iterator<Item = FieldRepairRow> + '_ {
    table
        .rows()
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
pub(super) fn grounded_field_feature_keys(
    table: FieldRepairTable,
    req: &ChatRequest,
) -> Vec<String> {
    present_rows(table, req)
        .filter_map(|row| crate::field_capability::field_capability_key(row.path))
        .collect()
}

/// The full act-side feature-key vocabulary for `req`: the pure catalog keys
/// [`crate::feature_keys::derive_feature_keys`] derives,
/// [`crate::feature_keys::FORCED_TOOL_CHOICE`] when `tool_choice` forces a
/// tool call, plus the one grounded field key [`grounded_field_feature_keys`]
/// can mint for it.
///
/// This is the widened vocabulary every routing, learn, observe, and count
/// call site that must treat a catalog capability and a field verdict alike
/// uses. The diagnostic-only feature-naming-drift path is deliberately NOT
/// one of those call sites: a field verdict is never a feature-naming-table
/// candidate, so that diagnostic stays on the bare catalog derivation.
pub(super) fn request_feature_keys(table: FieldRepairTable, req: &ChatRequest) -> Vec<String> {
    crate::feature_keys::derive_feature_keys(
        req.tools.as_deref().unwrap_or(&[]),
        req.provider_extras.as_ref(),
        req.response_format.as_ref(),
    )
    .into_iter()
    .chain(
        crate::feature_keys::tool_choice_force(req.tool_choice.as_ref())
            .map(|_| crate::feature_keys::FORCED_TOOL_CHOICE.to_string()),
    )
    .chain(grounded_field_feature_keys(table, req))
    .collect()
}

/// The closed-table row an upstream rejection named and what it refused, or
/// `None` when the class cannot carry a field rejection or the parser named
/// no row.
///
/// The row is the table's own `&'static` entry, never a string built from
/// upstream text: a repair can only ever act on a table row, so no caller
/// downstream can hold a path the table does not know. That is also what
/// keeps upstream bytes out of the operator-facing WARN, which renders the
/// row's literal.
///
/// The class is read FIRST, so the parser is never consulted for a class
/// that cannot carry a field rejection.
pub(super) fn rejected_field(
    table: FieldRepairTable,
    class: &FailureClass,
    err: &Error,
    provider_kind: &str,
) -> Option<NamedRejection> {
    if !class_can_name_a_field(class, provider_kind) {
        return None;
    }
    #[cfg(test)]
    if let provisional::Injected::Resolution(named) = provisional::injected() {
        return named;
    }
    let status = super::class_observe::upstream_facts(err).status?;
    parse_rejection(table, provider_kind, status, err.parsed_body())
}

/// Whether a failure class, on a lane of `provider_kind`, can carry a
/// field-naming rejection at all.
///
/// A field rejection is a caller-shaped 4xx: the upstream read the envelope
/// and refused one field in it. Every other class -- auth, rate limiting,
/// transport, overload, server error -- says nothing about the envelope's
/// shape, so a repair driven by one would drop a field over an unrelated
/// fault and then learn a permanent negative from it.
///
/// `BadRequest` on every kind, plus exactly one lifted feature-unsupported
/// token: the openai-compat `unsupported_parameter` code, which the
/// classifier lifts out of `BadRequest` and which is precisely the code that
/// refuses a named parameter. The other lifted openai tokens refuse a value or
/// a region rather than a field, and no other kind lifts this one. A subset of
/// the generic learner's deterministic-class gate
/// ([`super::capability_learn::f2_class_is_deterministic`]); the field arm runs
/// first and the two learners never claim the same path (pinned by the
/// disjointness test over the closed table).
fn class_can_name_a_field(class: &FailureClass, provider_kind: &str) -> bool {
    match class {
        FailureClass::BadRequest => true,
        FailureClass::FeatureUnsupported { capability } => {
            provider_kind == OPENAI_COMPAT_KIND && capability == OPENAI_UNSUPPORTED_CODE
        }
        _ => false,
    }
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
/// every other candidate's guard (see [`Self::apply_named`]).
///
/// Holding this is the PROVISIONAL phase -- nothing is persisted while it
/// lives. The dispatch arm settles it exactly once: [`Self::commit`] when
/// the repaired retry succeeded, [`Self::settle_success`] when the
/// unrepaired attempt succeeded outright, and release-by-drop on every other
/// exit (a repeat rejection, an unrelated error, a fallback hop, a client
/// disconnect), which learns nothing and strands no slot.
pub(super) struct FieldRepairPlan<'a> {
    /// The table the candidates were admitted from, and so the only rows a
    /// rejection may name for this plan.
    #[cfg(test)]
    table: FieldRepairTable,
    candidates: Vec<FieldRepairCandidate<'a>>,
    /// Index into `candidates` of the row a successful [`Self::apply_named`] dropped.
    /// `None` until one succeeds, which is what makes the one-repair ceiling a
    /// property of the plan rather than of a caller's flag alone.
    repaired: Option<usize>,
    /// What the rejection that drove the repair refused. A refused FIELD may
    /// commit; a refused value commits only through a rewrite.
    repaired_kind: Option<RejectionKind>,
    /// The edit a successful [`Self::apply_named`] made, or `None` until one
    /// succeeds. A rewrite commits whatever the rejection refused: its value
    /// is the upstream's own named answer, not a guess.
    applied: Option<FieldEdit>,
    /// How confidently that rejection was attributed to the row. The commit
    /// records it, so an inferred attribution persists without acting until an
    /// independent request corroborates it.
    repaired_tier: Option<SignalTier>,
    /// Whether the applied ceiling lowered routectl's fill rather than a
    /// client value.
    fill_clamped: bool,
    /// The wire dialect the attempt shares with its client, for declaring the
    /// repair's edit; `None` on a translation lane.
    wire: Option<IngressDialect>,
}

/// One admitted row: the closed-table path, surface and transform, plus the
/// guard whose settlement this row owes.
struct FieldRepairCandidate<'a> {
    path: &'static str,
    surface: FieldSurface,
    transform: FieldTransform,
    /// `Some` only for a SETTLING walk. `None` is the non-settling plan: it
    /// repairs for the answer its caller returns and can settle nothing, so it
    /// holds no single-flight claim (leaving it available to a sibling request
    /// that CAN settle) and consults no resident verdict. Both settlements are
    /// no-ops on it -- the type carries the capability rather than every
    /// settlement site re-checking a mode flag.
    guard: Option<FieldRepairGuard<'a>>,
}

/// The concrete edit one repair makes: a row's transform resolved against the
/// rejection that named it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldEdit {
    Drop,
    Rewrite(&'static str),
    Ceiling(u32),
}

impl FieldEdit {
    /// The edit `transform` makes for `named`, or `None` when it makes none:
    /// a rewrite whose rejection named no shape, and a ceiling whose rejection
    /// named no ceiling.
    const fn for_rejection(transform: FieldTransform, named: &NamedRejection) -> Option<Self> {
        match (transform, named.value) {
            (FieldTransform::Drop, _) => Some(Self::Drop),
            (FieldTransform::Rewrite { .. }, Some(RejectionValue::Shape(value))) => {
                Some(Self::Rewrite(value))
            }
            (FieldTransform::Ceiling, Some(RejectionValue::Ceiling(ceiling))) => {
                Some(Self::Ceiling(ceiling))
            }
            (FieldTransform::Rewrite { .. }, Some(RejectionValue::Ceiling(_)) | None)
            | (FieldTransform::Ceiling, Some(RejectionValue::Shape(_)) | None) => None,
        }
    }

    fn apply_to(self, surface: FieldSurface, req: &mut ChatRequest) -> bool {
        match self {
            Self::Drop => surface.drop_from(req),
            Self::Rewrite(value) => surface.rewrite_in(req, value),
            Self::Ceiling(ceiling) => surface.clamp_to(req, ceiling),
        }
    }

    const fn declared_kind(self) -> EditKind {
        match self {
            Self::Drop => EditKind::Remove,
            Self::Rewrite(_) | Self::Ceiling(_) => EditKind::Rewrite,
        }
    }

    /// The value a verdict committed for this edit applies: none for a drop.
    const fn verdict_value(self) -> Option<VerdictValue> {
        match self {
            Self::Drop => None,
            Self::Rewrite(value) => Some(VerdictValue::Shape(value)),
            Self::Ceiling(ceiling) => Some(VerdictValue::Ceiling(ceiling)),
        }
    }
}

impl FieldRepairPlan<'_> {
    /// The closed-table path a successful [`Self::apply_named`] dropped, or `None`
    /// when this plan has not repaired anything. A code-authored literal,
    /// never upstream text, so it is safe in an operator-facing field.
    pub(super) fn repaired_path(&self) -> Option<&'static str> {
        self.repaired.map(|idx| self.candidates[idx].path)
    }

    /// The value the applied repair wrote, or `None` for a drop.
    fn written_value(&self) -> Option<VerdictValue> {
        self.applied.and_then(FieldEdit::verdict_value)
    }

    /// The closed-set WARN action token for the repair this plan applied.
    fn repair_action(&self) -> &'static str {
        match self.applied {
            Some(FieldEdit::Rewrite(_)) => FIELD_ACTION_REWRITE_REPAIR,
            Some(FieldEdit::Ceiling(_)) if self.fill_clamped => FIELD_ACTION_CEILING_FILL_REPAIR,
            Some(FieldEdit::Ceiling(_)) => FIELD_ACTION_CEILING_REPAIR,
            Some(FieldEdit::Drop) | None
                if self.repaired_kind == Some(RejectionKind::InvalidValue) =>
            {
                FIELD_ACTION_LOSSY_DROP_REPAIR
            }
            Some(FieldEdit::Drop) | None => FIELD_ACTION_DROP_REPAIR,
        }
    }

    /// Whether the applied repair may commit a verdict once its retry succeeds.
    fn repair_teaches(&self) -> bool {
        self.written_value().is_some()
            || self.repaired_kind == Some(RejectionKind::UnsupportedField)
    }

    /// Perform the repair for whichever admitted row `err` names: apply that
    /// row's transform to the attempt request, re-stamp the calibration estimate,
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
    #[cfg(test)]
    pub(super) fn apply(
        &mut self,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        native_class: &FailureClass,
        rejection: &Error,
        provider_kind: &str,
    ) -> Option<u16> {
        let named = rejected_field(self.table, native_class, rejection, provider_kind)?;
        self.apply_named(named, attempt_req, meta, budget, rejection)
    }

    /// Repair for a rejection already resolved to its closed-table row: the
    /// admitted candidate whose SURFACE that row names.
    pub(super) fn apply_named(
        &mut self,
        named: NamedRejection,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        rejection: &Error,
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
        let idx = self
            .candidates
            .iter()
            .position(|candidate| candidate.surface == named.row.surface)?;
        let edit = FieldEdit::for_rejection(self.candidates[idx].transform, &named)?;
        let status = self.apply_candidate(idx, edit, attempt_req, meta, budget, rejection)?;
        self.repaired_kind = Some(named.kind);
        self.repaired_tier = Some(named.tier);
        self.applied = Some(edit);
        Some(status)
    }

    /// The EFFECT half of [`Self::apply_named`], for the candidate at `idx`: the
    /// presence re-check, the allowance check, the edit, the charge, the guard
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
        edit: FieldEdit,
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
        let fill_clamped =
            matches!(edit, FieldEdit::Ceiling(_)) && surface.clamp_writes_fill(attempt_req);
        if !edit.apply_to(surface, attempt_req) {
            // FAIL CLOSED. A rewrite may refuse by design: the value is
            // already carried, or the rewritten body failed its read-back. A
            // ceiling may too: the request already meets the named limit. A
            // drop cannot: `present_in` held immediately above and nothing
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
                edit != FieldEdit::Drop,
                "a present surface must take a drop; presence and the drop have diverged",
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
        // A clamped fill is still the egress's declared default; a second
        // declaration at the same pointer would read as a conflict to the next
        // rejection there and keep it from the proposers.
        if !fill_clamped {
            declare_field_edit(
                attempt_req,
                self.wire,
                EditSite::FieldRepair,
                surface,
                edit.declared_kind(),
            );
        }
        self.fill_clamped = fill_clamped;
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
            table: FieldRepairTable::default(),
            candidates: vec![FieldRepairCandidate {
                // Outside the closed table deliberately: the surface is not a
                // row (see `FieldRepairTable`), and this path exists only so the
                // candidate is well formed.
                path: "divergent.surface.for.tests",
                surface: FieldSurface::DivergentForTests,
                transform: FieldTransform::Drop,
                guard: None,
            }],
            repaired: None,
            repaired_kind: None,
            applied: None,
            repaired_tier: None,
            fill_clamped: false,
            wire: None,
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
        self.apply_candidate(0, FieldEdit::Drop, attempt_req, meta, budget, rejection)
    }

    /// Phase two after a successful repaired retry: persist the REPAIRED row's
    /// verdict and return the emission row for the capability-event sink, or
    /// `None` for a non-settling plan (which persists nothing and emits
    /// nothing) and for a plan that repaired nothing.
    ///
    /// Only the repaired row commits. The rejection named that row, and the
    /// retry that succeeded is evidence about that row alone -- every other
    /// candidate's guard was already released by [`Self::apply_named`].
    ///
    /// And only when the rejection refused the FIELD, the repair rewrote it to
    /// the one accepted value the rejection named, or it lowered a token limit
    /// to the ceiling the rejection named. A refused value repaired
    /// by a DROP says the lane accepts the field, so a verdict dropping it
    /// would strip every valid value from every later request on the lane;
    /// such a plan releases its guard here and persists nothing.
    ///
    /// The commit carries the attribution tier of the rejection that drove the
    /// repair: a self-identifying verdict acts at once, an inferred one persists
    /// pending and acts only once a later request corroborates it. A rewrite
    /// or ceiling commit also carries the value it wrote, so the persisted
    /// verdict names what it rewrites or clamps to.
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
        let tier = self.repaired_tier?;
        if !self.repair_teaches() {
            return None;
        }
        let value = self.written_value();
        self.candidates[idx].guard.take()?.commit(
            tier,
            value,
            upstream_status,
            request_features,
            now,
        )
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
    /// [`FieldRepairPlan::apply_named`], which applies exactly one candidate and
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
    /// - the target's provider kind is not one field repair acts on;
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
        if !field_repair_admits_kind(provider_kind) {
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
        // base URL, and the target carries none. The decision answers only
        // for the kinds this stage acts on, so every other kind fails closed
        // here as well as at the kind check above.
        //
        // Loopback suppression is folded into the SAME shared read, so it
        // applies to EVERY walk. The guard's own admission also refuses a
        // local target, but only a settling walk reaches that admission --
        // leaving the check there alone let a result-only walk mutate the
        // body, spend the shared allowance, and re-dispatch a target whose
        // rejection this stage has decided it cannot attribute at all. A
        // suppressed target is one routectl must not act on, not merely one
        // it must not learn from, so the refusal belongs to the plan rather
        // than to the settlement.
        // The forwarded fact comes from the TARGET here -- the chain derived it
        // for this dispatch -- and the rest of the refusal set lives in the
        // shared decision.
        let lane = attributable_field_lane(
            &self.config,
            &target.provider_name,
            target.use_forwarded_credential,
        )?;
        // A reload may have re-kinded the entry under a target expanded from
        // the old one; the verdict key carries the target's kind, so the two
        // must agree before anything is keyed on it.
        if lane.provider_kind != provider_kind {
            return None;
        }
        // The identities come from the CLOSED TABLE plus the request, never
        // from upstream text: every row whose surface this attempt actually
        // carries, in table source order.
        let table = self.field_repair_table();
        let candidates: Vec<FieldRepairCandidate<'a>> = present_rows(table, attempt_req)
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
                        transform: row.transform,
                        guard: None,
                    });
                }
                let key =
                    FieldVerdictKey::new(target.learned_lane.as_ref()?, row.path, provider_kind)?;
                let guard = self.field_verdicts().admit_provisional(
                    &key,
                    &lane.base_url,
                    self.registry_generation(),
                    now,
                )?;
                Some(FieldRepairCandidate {
                    path: row.path,
                    surface: row.surface,
                    transform: row.transform,
                    guard: Some(guard),
                })
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(FieldRepairPlan {
            #[cfg(test)]
            table,
            candidates,
            repaired: None,
            repaired_kind: None,
            applied: None,
            repaired_tier: None,
            fill_clamped: false,
            wire: fidelity_dialect(target, attempt_req),
        })
    }

    /// What the rejection `err`, natively classified as `native_class`,
    /// refused at a closed-table row whose SURFACE is `surface`, or `None`
    /// when it names no such row.
    ///
    /// THE single implementation of "did the upstream name this mutation", shared
    /// by the reactive plan's own candidate selection ([`FieldRepairPlan::apply_named`])
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
    pub(super) fn rejection_names_field_surface(
        table: FieldRepairTable,
        surface: FieldSurface,
        native_class: &FailureClass,
        err: &Error,
        provider_kind: &str,
    ) -> Option<NamedRejection> {
        rejected_field(table, native_class, err, provider_kind)
            .filter(|named| named.row.surface == surface)
    }

    /// Whether a rejection eligible to name a field localized NO field path from
    /// its envelope.
    ///
    /// The am-I-flying-blind predicate, read through the SAME resolver the repair's
    /// own candidate selection uses.
    pub(super) fn rejection_localizes_no_field(
        table: FieldRepairTable,
        native_class: &FailureClass,
        err: &Error,
        provider_kind: &str,
    ) -> bool {
        class_can_name_a_field(native_class, provider_kind)
            && rejected_field(table, native_class, err, provider_kind).is_none()
    }

    /// Record that the field repair fired for this request, for the
    /// field-repair WARN emitted at request resolution, and count it.
    ///
    /// The WARN reports each repair; the counter is what answers "how often
    /// does this arm fire" without depending on a log level, so both are driven
    /// from here and cannot disagree about whether a repair happened.
    ///
    /// A repair answering a refused VALUE is recorded under its own action
    /// token and counted as lossy: it served the attempt by dropping a field
    /// the client set and the lane accepts, and an operator needs to tell that
    /// apart from a drop of a field the lane does not take at all.
    ///
    /// Returns the handle the walk settles THIS record through, so a later
    /// repair elsewhere in the walk cannot take its outcome.
    pub(super) fn note_field_repair(
        &self,
        meta: &mut DispatchMeta,
        state_key: &str,
        plan: &FieldRepairPlan<'_>,
    ) -> FieldRepairHandle {
        let field_path = plan
            .repaired_path()
            .expect("a reported repair names the row it dropped");
        self.note_field_repair_at(meta, state_key, field_path, plan.repair_action())
    }

    /// Record a field repair by the path it repaired and its action token:
    /// the shared recorder behind [`Self::note_field_repair`] and the canary's
    /// refused-value repair, so both report through one summary and one set
    /// of counters, and the lossy counter reads [`repair_action_is_lossy`].
    pub(super) fn note_field_repair_at(
        &self,
        meta: &mut DispatchMeta,
        state_key: &str,
        field_path: &'static str,
        action: &'static str,
    ) -> FieldRepairHandle {
        let handle = FieldRepairHandle(meta.field_repairs.len());
        meta.field_repairs.push(super::FieldRepair {
            action,
            state_key: sanitize_for_log(state_key),
            field_path,
            reason: FIELD_REASON_UPSTREAM_REJECTION,
            repair_attempted: true,
            repair_succeeded: false,
            learned: false,
        });
        self.metrics.incr_field_repair_attempted();
        if repair_action_is_lossy(action) {
            self.metrics.incr_field_repair_lossy();
        }
        handle
    }

    /// Record that the repaired retry behind `repair` succeeded, and whether
    /// that outcome persisted a verdict. Both booleans move together because a
    /// committed verdict is exactly a succeeded repair -- recording them at
    /// separate sites is how a summary comes to claim one without the other.
    pub(super) fn note_field_repair_succeeded(
        &self,
        meta: &mut DispatchMeta,
        repair: FieldRepairHandle,
        learned: bool,
    ) {
        if let Some(record) = meta.field_repairs.get_mut(repair.0) {
            record.repair_succeeded = true;
            record.learned = learned;
        }
        self.metrics.incr_field_repair_succeeded();
        if learned {
            self.metrics.incr_field_verdicts_learned();
        }
    }
}

/// Which of a request's repair records a walk settles: the position the
/// recorder pushed it at. Records are only ever appended, so a handle stays
/// valid for the whole request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FieldRepairHandle(usize);

/// Emit the envelope-field repair WARN for a resolved request: one line per
/// repair that fired, in the order they fired, and none otherwise. Content-free
/// by construction: closed-set tokens, the closed table's own path literal, and
/// booleans -- never the upstream body, the rejected field's VALUE, or the
/// session key. The request span already supplies `request_id` correlation
/// across the hops.
pub(super) fn emit_field_repair(meta: &DispatchMeta) {
    for record in &meta.field_repairs {
        tracing::warn!(
            action = record.action,
            state_key = %sanitize_for_log(&record.state_key),
            field_path = record.field_path,
            reason = record.reason,
            repair_attempted = record.repair_attempted,
            repair_succeeded = record.repair_succeeded,
            learned = record.learned,
            "{FIELD_REPAIR_EVENT}",
        );
    }
}

/// Message/event name of the field-repair WARN. Stable, greppable,
/// closed-set.
const FIELD_REPAIR_EVENT: &str = "envelope_field_repaired";

/// Test-only injection of a field resolution, standing in for the parser.
///
/// Arms downstream of the parser -- the drop, the shared budget, the two-phase
/// settlement, the persistence, the WARN -- are covered over the full closed
/// table, test rows included, and over rejection shapes no committed envelope
/// produces. This is the seam those tests resolve through; no production path
/// reads it.
///
/// Thread-local because a `#[tokio::test]` drives its future to completion on
/// the test's own thread, so an injection set in a test body is visible
/// inside the dispatch it drives and invisible to every sibling test running
/// concurrently. That property is asserted rather than assumed
/// (`the_injected_resolution_is_visible_inside_a_dispatch`).
#[cfg(test)]
pub(in crate::router) mod provisional {
    use std::cell::Cell;

    use routectl_core::capability::SignalTier;

    use super::super::rejection_parser::{NamedRejection, RejectionKind, RejectionValue};
    use super::closed_table;

    /// This thread's injection state.
    #[derive(Clone, Copy, Debug)]
    pub(super) enum Injected {
        /// No injection: the live parser answers.
        None,
        /// The parser is replaced by this answer. `None` is an injected path
        /// with no closed-table row: it resolves to nothing and must not fall
        /// through to the parser.
        Resolution(Option<NamedRejection>),
    }

    thread_local! {
        static INJECTED: Cell<Injected> = const { Cell::new(Injected::None) };
    }

    pub(super) fn injected() -> Injected {
        INJECTED.with(Cell::get)
    }

    /// Make every field rejection on this thread name `path` as a refused
    /// FIELD, attributed self-identifyingly, until the returned guard drops.
    ///
    /// Visible to the whole `router` module rather than to `field_repair`
    /// alone: the canary settlement's coverage lives in a sibling sidecar and
    /// needs the SAME seam.
    pub(in crate::router) fn inject(path: &str) -> Injection {
        inject_with_tier(path, SignalTier::SelfIdentifying)
    }

    /// [`inject`], with the attribution tier the resolution carries.
    pub(in crate::router) fn inject_with_tier(path: &str, tier: SignalTier) -> Injection {
        inject_resolution(path, tier, RejectionKind::UnsupportedField, None)
    }

    /// Make every field rejection on this thread name `path` as a refused
    /// VALUE carrying `value`, attributed self-identifyingly, until the
    /// returned guard drops. The value is injected as given, whatever the
    /// row's transform: what a mismatched pairing does is the repair's call.
    pub(in crate::router) fn inject_value(path: &str, value: RejectionValue) -> Injection {
        inject_resolution(
            path,
            SignalTier::SelfIdentifying,
            RejectionKind::InvalidValue,
            Some(value),
        )
    }

    fn inject_resolution(
        path: &str,
        tier: SignalTier,
        kind: RejectionKind,
        value: Option<RejectionValue>,
    ) -> Injection {
        let named = closed_table()
            .iter()
            .find(|row| row.path == path)
            .map(|row| NamedRejection {
                row,
                tier,
                kind,
                value,
            });
        INJECTED.with(|slot| slot.set(Injected::Resolution(named)));
        Injection
    }

    /// Clears the thread's injection on drop, so one test cannot leak a
    /// resolution into another test that reuses its thread.
    pub(in crate::router) struct Injection;

    impl Drop for Injection {
        fn drop(&mut self) {
            INJECTED.with(|slot| slot.set(Injected::None));
        }
    }
}

#[cfg(test)]
#[path = "field_repair_tests.rs"]
mod field_repair_tests;

#[cfg(test)]
#[path = "field_attribution_tests.rs"]
mod field_attribution_tests;

#[cfg(test)]
#[path = "field_learner_disjointness_tests.rs"]
mod field_learner_disjointness_tests;

#[cfg(test)]
mod table_tests {
    use super::*;

    /// This module's own source, for the control-flow guard over `apply`.
    const SOURCE: &str = include_str!("field_repair.rs");

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
    /// If the signature or the closing brace is absent -- failing closed: a
    /// guard reading an empty body would pass vacuously.
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
    fn the_closed_table_carries_the_grounded_paths_and_nothing_else() {
        // The table's SIZE is the property: each row is a permanent
        // commitment and must be a deliberate, reviewed edit rather than
        // something a passing suite absorbs silently.
        assert_eq!(
            FIELD_REPAIRS.len(),
            8,
            "the closed repair table ships exactly eight grounded rows; a new row is a new \
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
        assert_eq!(
            FIELD_REPAIRS[0].transform,
            FieldTransform::Drop,
            "the grounded row drops its field; its captured rejection names two accepted \
             values, so it cannot be a rewrite",
        );
        assert!(
            !FIELD_REPAIRS[0].count_tokens_validates,
            "no captured token count rejects the display field",
        );
        let thinking_type = FIELD_REPAIRS[1];
        assert_eq!(thinking_type.path, "thinking.type.enabled");
        assert_eq!(
            thinking_type.surface,
            FieldSurface::AnthropicThinkingType("enabled")
        );
        assert_eq!(
            thinking_type.transform,
            FieldTransform::Rewrite {
                accepted: &["thinking.type.adaptive"],
            },
            "the captured rejection names exactly one replacement shape",
        );
        assert_eq!(thinking_type.class, TransformClass::Envelope);
        assert!(
            !thinking_type.count_tokens_validates,
            "no captured token count rejects a thinking type, and the count endpoint \
             carries no output_config to validate adaptive's effort against",
        );
        let thinking_off = FIELD_REPAIRS[2];
        assert_eq!(thinking_off.path, "thinking.type.disabled");
        assert_eq!(
            thinking_off.surface,
            FieldSurface::AnthropicThinkingType("disabled")
        );
        assert_eq!(
            thinking_off.transform,
            FieldTransform::Rewrite {
                accepted: &["thinking.type.adaptive", "thinking.type.between_tools"],
            },
            "each member is the replacement one committed capture names: adaptive on \
             anthropic-api, between_tools on bedrock",
        );
        assert_eq!(thinking_off.class, TransformClass::Envelope);
        assert!(!thinking_off.count_tokens_validates);
        let temperature = FIELD_REPAIRS[3];
        assert_eq!(temperature.path, "temperature");
        assert_eq!(
            temperature.surface,
            FieldSurface::SamplingParam(SamplingParam::Temperature)
        );
        assert_eq!(
            temperature.transform,
            FieldTransform::Drop,
            "both committed refusals refuse the field itself and name no replacement",
        );
        assert_eq!(temperature.class, TransformClass::Envelope);
        assert!(
            !temperature.count_tokens_validates,
            "the Converse count endpoint is unsupported on the models that refuse it",
        );
        for (path, param) in [
            ("top_p", SamplingParam::TopP),
            ("presence_penalty", SamplingParam::PresencePenalty),
            ("frequency_penalty", SamplingParam::FrequencyPenalty),
        ] {
            let row = FIELD_REPAIRS
                .iter()
                .find(|row| row.path == path)
                .unwrap_or_else(|| panic!("{path}: a grounded row"));
            assert_eq!(row.surface, FieldSurface::SamplingParam(param), "{path}");
            assert_eq!(row.transform, FieldTransform::Drop, "{path}");
            assert_eq!(row.class, TransformClass::Envelope, "{path}");
            assert!(!row.count_tokens_validates, "{path}");
        }
        let max_tokens = FIELD_REPAIRS[7];
        assert_eq!(max_tokens.path, "max_tokens");
        assert_eq!(max_tokens.surface, FieldSurface::MaxTokens);
        assert_eq!(
            max_tokens.transform,
            FieldTransform::Ceiling,
            "the committed refusal names the model's own output limit",
        );
        assert_eq!(max_tokens.class, TransformClass::Envelope);
        assert!(
            !max_tokens.count_tokens_validates,
            "a token count generates nothing, so it cannot refuse an output limit",
        );
    }

    #[test]
    fn the_test_table_carries_one_row_per_transform() {
        let transforms: Vec<FieldTransform> = TEST_FIELD_REPAIRS
            .iter()
            .chain(TEST_REWRITE_FIELD_REPAIRS)
            .map(|row| row.transform)
            .collect();
        assert!(transforms.contains(&FieldTransform::Drop));
        assert!(transforms.contains(&FieldTransform::Ceiling));
        assert!(transforms.contains(&FieldTransform::Rewrite {
            accepted: TEST_REWRITE_ACCEPTED,
        }));
    }

    #[test]
    fn a_ceiling_row_and_an_unfilled_rewrite_make_no_edit() {
        let named_row = |path: &str| {
            closed_table()
                .iter()
                .find(|row| row.path == path)
                .expect("a test row")
        };
        let named = |path: &str, value| NamedRejection {
            row: named_row(path),
            tier: SignalTier::SelfIdentifying,
            kind: RejectionKind::InvalidValue,
            value,
        };
        let ceiling = named(TEST_CEILING_PATH, None);
        let rewrite = named(TEST_REWRITE_PATH, None);
        let filled = named(
            TEST_REWRITE_PATH,
            Some(RejectionValue::Shape(TEST_REWRITE_ACCEPTED[0])),
        );

        assert_eq!(
            FieldEdit::for_rejection(FieldTransform::Ceiling, &ceiling),
            None
        );
        assert_eq!(
            FieldEdit::for_rejection(rewrite.row.transform, &rewrite),
            None
        );
        assert_eq!(
            FieldEdit::for_rejection(filled.row.transform, &filled),
            Some(FieldEdit::Rewrite(TEST_REWRITE_ACCEPTED[0])),
        );
    }

    #[test]
    fn a_rewrite_that_does_not_read_back_leaves_the_request_untouched() {
        let mut req = ChatRequest {
            chat_template_kwargs: Some(serde_json::json!("not an object")),
            ..Default::default()
        };
        let before = req.chat_template_kwargs.clone();

        let taken = FieldSurface::RewriteFieldForTests.rewrite_in(&mut req, "v");

        assert!(
            !taken,
            "a write the surface cannot read back did not happen"
        );
        assert_eq!(req.chat_template_kwargs, before);
    }

    /// An Anthropic invalid-request rejection whose message names `path`.
    fn rejection_naming(path: &str, reason: &str) -> Error {
        let body = serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": format!("{path}: {reason}")},
        });
        Error::upstream("p", 400, body.to_string())
    }

    #[test]
    fn a_path_outside_the_closed_table_resolves_to_no_row() {
        // The whole of "unknown path mutates nothing": the parser localizes
        // each of these, and with no row there is no surface, so no code path
        // can decide what to drop.
        for unknown in [
            "thinking.display",
            "thinking.enabled",
            "thinking.enabled.display.extra",
            "reasoning.exclude",
        ] {
            assert!(
                rejected_field(
                    crate::router::FieldRepairTable::default(),
                    &FailureClass::BadRequest,
                    &rejection_naming(unknown, "Extra inputs are not permitted"),
                    ANTHROPIC_API_KIND,
                )
                .is_none(),
                "`{unknown}` is outside the closed table and must resolve to no row",
            );
        }
        // Positive control: the same message shape on the grounded path DOES
        // resolve, so the negatives above are about the paths and not about an
        // empty table or a parser that never answers.
        assert!(
            rejected_field(
                crate::router::FieldRepairTable::default(),
                &FailureClass::BadRequest,
                &rejection_naming("thinking.enabled.display", "Extra inputs are not permitted"),
                ANTHROPIC_API_KIND,
            )
            .is_some(),
            "control: the grounded path resolves through the same parser",
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

    /// Mutation checks: reduce the gate to `BadRequest` alone -> red on the
    /// openai-compat `unsupported_parameter` row; admit every
    /// `FeatureUnsupported` -> red on the refused lifted tokens; drop the kind
    /// test -> red on the anthropic-api and bedrock `unsupported_parameter`
    /// rows.
    #[test]
    fn only_a_caller_shaped_rejection_can_name_a_field() {
        let unsupported = |token: &str| FailureClass::FeatureUnsupported {
            capability: token.into(),
        };
        for kind in FIELD_REPAIR_KINDS {
            assert!(
                class_can_name_a_field(&FailureClass::BadRequest, kind),
                "{kind}: the envelope-refusal class is the one a field rejection arrives on",
            );
        }
        assert!(
            class_can_name_a_field(&unsupported(OPENAI_UNSUPPORTED_CODE), OPENAI_COMPAT_KIND),
            "the openai code that refuses a named parameter lifts out of BadRequest",
        );
        let refused_everywhere = [
            FailureClass::RateLimited,
            FailureClass::Auth,
            FailureClass::ContentPolicy,
            FailureClass::ContextWindow,
            FailureClass::ServerError,
            FailureClass::Timeout,
            FailureClass::NetworkError,
            FailureClass::Overloaded,
            FailureClass::Unknown,
            unsupported("unsupported_value"),
            unsupported("unsupported_country_region_territory"),
            unsupported("reasoning_replay"),
        ];
        for kind in FIELD_REPAIR_KINDS {
            for other in &refused_everywhere {
                assert!(
                    !class_can_name_a_field(other, kind),
                    "{kind}: {other:?} does not refuse a named field, so a repair driven by it \
                     would drop a field over an unrelated fault and learn from it",
                );
            }
        }
        for kind in [ANTHROPIC_API_KIND, BEDROCK_KIND] {
            assert!(
                !class_can_name_a_field(&unsupported(OPENAI_UNSUPPORTED_CODE), kind),
                "{kind}: the lifted openai code is admitted on openai-compat only",
            );
        }
    }

    #[test]
    fn the_live_resolver_names_the_display_row_from_the_captured_rejection() {
        // No injection in scope: the carried parse of the captured envelope is
        // what the parser reads. The display rejection refuses the VALUE, which
        // is what keeps the seam from learning a drop out of it.
        let captured = rejection_naming(
            "thinking.enabled.display",
            "Input should be 'summarized', 'omitted'",
        );

        let named = rejected_field(
            crate::router::FieldRepairTable::default(),
            &FailureClass::BadRequest,
            &captured,
            ANTHROPIC_API_KIND,
        )
        .expect("the captured display rejection names its row");

        assert_eq!(named.row.path, "thinking.enabled.display");
        assert_eq!(named.row.surface, FieldSurface::AnthropicThinkingDisplay);
        assert_eq!(named.kind, RejectionKind::InvalidValue);
    }

    #[test]
    fn the_live_resolver_names_nothing_without_a_parse_or_a_parser() {
        let captured = rejection_naming(
            "thinking.enabled.display",
            "Input should be 'summarized', 'omitted'",
        );
        // Control: the shape below resolves on its own lane, so each refusal
        // is about the one input that changed.
        assert!(
            rejected_field(
                crate::router::FieldRepairTable::default(),
                &FailureClass::BadRequest,
                &captured,
                ANTHROPIC_API_KIND
            )
            .is_some(),
            "premise: the captured rejection resolves on anthropic-api",
        );
        assert!(
            rejected_field(
                crate::router::FieldRepairTable::default(),
                &FailureClass::BadRequest,
                &captured,
                "gemini"
            )
            .is_none(),
            "a lane kind with no parser names nothing",
        );
        for unparsed in [
            Error::upstream(
                "p",
                400,
                "thinking.enabled.display: Input should be 'summarized', 'omitted'",
            ),
            Error::upstream("p", 400, ""),
            Error::upstream("p", 400, "{}"),
            Error::normalize_request(
                "p",
                "thinking.enabled.display: Input should be 'summarized', 'omitted'",
                routectl_core::RefusalReason::Unrepresentable,
            ),
        ] {
            assert!(
                rejected_field(
                    crate::router::FieldRepairTable::default(),
                    &FailureClass::BadRequest,
                    &unparsed,
                    ANTHROPIC_API_KIND
                )
                .is_none(),
                "an error carrying no parsed envelope names nothing: {unparsed:?}",
            );
        }
    }

    #[test]
    fn an_injected_unknown_path_does_not_fall_through_to_the_parser() {
        // The injection replaces the parser for its thread, a non-resolving
        // injection included: a test that injects an unknown path is asserting
        // that path's refusal, and a fall-through would let the realistic body
        // beside it resolve instead.
        let captured = rejection_naming(
            "thinking.enabled.display",
            "Input should be 'summarized', 'omitted'",
        );
        let _injection = provisional::inject("thinking.display");

        assert!(
            rejected_field(
                crate::router::FieldRepairTable::default(),
                &FailureClass::BadRequest,
                &captured,
                ANTHROPIC_API_KIND
            )
            .is_none(),
            "an injected path with no row resolves to nothing, whatever the body says",
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
        let mutation = "if !edit.apply_to(surface, attempt_req) {";
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
    fn an_injected_resolution_does_not_bypass_the_class_gate() {
        // The injection replaces the PARSER, not the arm's own
        // conditions: a class that cannot name a field still resolves to
        // nothing even with a resolution injected, so no test can
        // accidentally cover a path production could not reach.
        let _injection = provisional::inject("thinking.enabled.display");
        assert!(
            rejected_field(
                crate::router::FieldRepairTable::default(),
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
            assert!(
                matches!(
                    provisional::injected(),
                    provisional::Injected::Resolution(_)
                ),
                "injected in scope",
            );
        }
        assert!(
            matches!(provisional::injected(), provisional::Injected::None),
            "the injection must clear on drop: the thread is reused across tests, so a leaked \
             resolution would override the live parser in a sibling test",
        );
    }
}
