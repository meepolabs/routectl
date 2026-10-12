//! A rejection naming a path one of routectl's own declared edits put on the
//! wire, and the revert that answers it.
//!
//! The settlement matches the rejected path against the attempt's request
//! edits ([`MatchedEdit`]). With exactly one match the engine undoes that edit
//! on the attempt and re-sends it, when the edit's site is one whose client
//! value the client's canonical request still holds ([`revert`]). No learner
//! reads such a rejection, so nothing is learned and nothing is persisted: a
//! rejected routectl edit is a routectl bug, not a fact about the upstream.
//! Two matches name no single edit to undo, and every other site is not
//! undone.
//!
//! An undone edit must leave the request's slot holding exactly what the
//! client sent at the rejected location, or nothing when the client sent
//! nothing. The revert checks that against the client's wire body before it
//! applies, so a canonical slot that does not correspond to the wire
//! location the rejection names is left alone.

use routectl_core::warn_dedup::{CappedWarnSet, WarnDecision};
use routectl_core::wire_check::{DeclaredEdit, EditSite, Lane, RejectionProvenance, WirePath};
use routectl_core::{CacheControl, ChatRequest, SystemContent, ToolDef, sanitize_for_log};
use serde_json::Value;

use super::learn_engine::RejectionSite;
use super::repair_budget::RepairBudget;
use super::wire_rejection::locate;
use super::{DispatchMeta, Router};
use crate::wire_check_state::FieldRepairOutcome;

/// The attempt's request edits whose pattern matches a rejected path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum MatchedEdit {
    /// No declared edit explains the path.
    #[default]
    None,
    /// Exactly one declared edit explains it.
    One(DeclaredEdit),
    /// Several different declared edits explain it, so none is undone.
    Conflict,
}

impl MatchedEdit {
    /// The declared edits among `edits` matching `path`. The same edit
    /// declared twice is one edit.
    pub(super) fn among(edits: &[DeclaredEdit], path: &WirePath) -> Self {
        let mut matching = edits.iter().filter(|edit| edit.matches(path));
        let Some(first) = matching.next() else {
            return Self::None;
        };
        if matching.all(|other| other == first) {
            Self::One(first.clone())
        } else {
            Self::Conflict
        }
    }
}

/// Whether a rejected path may be attributed to a declared edit at all: a
/// path the body that went out is not known to lack, whose value routectl
/// added or changed. A translation lane never qualifies: its provenance is
/// not applicable, since the client's and the egress's pointers do not
/// correspond there. Any other rejection proceeds as it would with no
/// declarations.
pub(super) const fn attributable(provenance: RejectionProvenance, in_egress: Option<bool>) -> bool {
    !matches!(in_egress, Some(false))
        && matches!(
            provenance,
            RejectionProvenance::RoutectlAdded | RejectionProvenance::ClientChanged
        )
}

/// Which routectl edit a rejected path is laid to, as the
/// `routectl_edit_rejected` warning names it. Closed labels only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum EditAttribution {
    /// One declared edit, made at this site.
    Declared(EditSite),
    /// Several declared edits.
    Conflict,
    /// A path routectl added that no declaration explains.
    Undeclared,
}

impl EditAttribution {
    /// The attribution of a settled rejection, or `None` when the path is
    /// not laid to routectl at all.
    pub(super) const fn of(matched: &MatchedEdit, provenance: RejectionProvenance) -> Option<Self> {
        match matched {
            MatchedEdit::One(edit) => Some(Self::Declared(edit.site())),
            MatchedEdit::Conflict => Some(Self::Conflict),
            MatchedEdit::None => match provenance {
                RejectionProvenance::RoutectlAdded => Some(Self::Undeclared),
                _ => None,
            },
        }
    }

    /// Static label for the warning's `edit_site` field.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Declared(site) => site.label(),
            Self::Conflict => "conflict",
            Self::Undeclared => "undeclared",
        }
    }
}

/// Distinct `routectl_edit_rejected` warnings remembered before new ones
/// stop: rejected paths are client-influenced, so the set needs a bound.
const EDIT_REJECTED_WARN_CAP: usize = 1024;

/// The dedup set of `routectl_edit_rejected` warnings, one per lane, path
/// and attribution.
pub(super) type EditRejectedWarned = CappedWarnSet<(Lane, WirePath, EditAttribution)>;

/// An empty [`EditRejectedWarned`] at its cap.
pub(super) fn edit_rejected_warned() -> EditRejectedWarned {
    CappedWarnSet::new(EDIT_REJECTED_WARN_CAP)
}

impl Router {
    /// Warn, once per lane, path and attribution, that the upstream rejected
    /// a path routectl put on the wire. The path renders masked and
    /// sanitized; every other field is a closed label.
    pub(super) fn warn_edit_rejected(
        &self,
        lane: Lane,
        path: &WirePath,
        attribution: EditAttribution,
    ) {
        match self
            .edit_rejected_warned
            .admit((lane, path.clone(), attribution))
        {
            WarnDecision::Emit => tracing::warn!(
                lane = lane.label(),
                path = %sanitize_for_log(path.as_str()),
                edit_site = attribution.label(),
                "routectl_edit_rejected"
            ),
            WarnDecision::CapReached { notice: true } => tracing::warn!(
                cap = self.edit_rejected_warned.cap(),
                "routectl_edit_rejected dedup is full; new paths are no longer warned"
            ),
            WarnDecision::AlreadyWarned | WarnDecision::CapReached { notice: false } => {}
        }
    }
}

/// `attempt_req` with `edit` undone at `locator` (the rejected location,
/// unmasked), or `None` when the site is not revertible, the slot cannot be
/// read, the request's slot does not hold what the body that went out held
/// at `locator` (`sent`, so the slot is not the one the rejection names),
/// undoing changes nothing, or the undone slot would not hold exactly what
/// the client's wire body holds at `locator`.
///
/// Revertible: operator payload extras (the client's canonical extras value
/// is restored, or the key removed) and cache placement (the placed marker
/// is removed). Every other site is not.
pub(super) fn revert(
    edit: &DeclaredEdit,
    locator: &[String],
    sent: &SentSlot,
    client_req: &ChatRequest,
    attempt_req: &ChatRequest,
) -> Option<ChatRequest> {
    let mut candidate = attempt_req.clone();
    let slot = match edit.site() {
        EditSite::OperatorPayloadExtras => {
            let client_value = extras_slot(client_req, locator)?;
            write_extra(&mut candidate, locator, client_value)?;
            extras_slot
        }
        EditSite::CachePlacement => {
            *cache_marker_mut(&mut candidate, locator)? = None;
            cache_slot
        }
        _ => return None,
    };
    let before = slot(attempt_req, locator)?;
    let after = slot(&candidate, locator)?;
    let client_wire = client_wire_value(client_req, locator)?;
    let corresponds = before.is_some() && sent.0.as_ref() == Some(&before);
    (corresponds && before != after && after == client_wire).then_some(candidate)
}

/// What the body that went out held at a rejected location: `Some(None)`
/// when it held nothing there, `None` when it could not be read. Body
/// content, so `Debug` prints only whether a value was read.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct SentSlot(Slot);

impl SentSlot {
    /// The value at `locator` in `sent`.
    pub(super) fn at(sent: &Value, locator: &[String]) -> Self {
        Self(Some(locate(sent, locator).cloned()))
    }
}

impl std::fmt::Debug for SentSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let read = match &self.0 {
            None => "unread",
            Some(None) => "absent",
            Some(Some(_)) => "present",
        };
        write!(f, "SentSlot({read})")
    }
}

/// What a slot holds: `Some(None)` when it is empty, `None` when the request
/// has no slot of that shape at the location.
type Slot = Option<Option<Value>>;

/// The value at `locator` in `req`'s provider extras, every segment an
/// object key.
fn extras_slot(req: &ChatRequest, locator: &[String]) -> Slot {
    let Some(mut node) = req.provider_extras.as_ref() else {
        return Some(None);
    };
    for key in locator {
        match node.as_object()?.get(key) {
            Some(child) => node = child,
            None => return Some(None),
        }
    }
    Some(Some(node.clone()))
}

/// Set (or, for `None`, remove) the provider-extras value at `locator`.
/// `None` when a parent on the path is missing or not an object.
fn write_extra(req: &mut ChatRequest, locator: &[String], value: Option<Value>) -> Option<()> {
    let (last, parents) = locator.split_last()?;
    let mut node = req.provider_extras.as_mut()?;
    for key in parents {
        node = node.as_object_mut()?.get_mut(key)?;
    }
    let map = node.as_object_mut()?;
    match value {
        Some(value) => {
            map.insert(last.clone(), value);
        }
        None => {
            map.remove(last);
        }
    }
    Some(())
}

/// The serialized cache marker at `locator`.
fn cache_slot(req: &ChatRequest, locator: &[String]) -> Slot {
    let marker = cache_marker(req, locator)?;
    marker.as_ref().map(serde_json::to_value).transpose().ok()
}

/// The cache-marker slot a placement pointer names: the top-level marker, a
/// system block's, or a custom tool's.
fn cache_marker<'r>(req: &'r ChatRequest, locator: &[String]) -> Option<&'r Option<CacheControl>> {
    match marker_location(locator)? {
        MarkerLocation::TopLevel => Some(&req.cache_control),
        MarkerLocation::SystemBlock(index) => match req.system.as_ref()? {
            SystemContent::Blocks(blocks) => Some(&blocks.get(index)?.cache_control),
            SystemContent::Text(_) => None,
        },
        MarkerLocation::Tool(index) => match req.tools.as_ref()?.get(index)? {
            ToolDef::Custom(tool) => Some(&tool.cache_control),
            _ => None,
        },
    }
}

fn cache_marker_mut<'r>(
    req: &'r mut ChatRequest,
    locator: &[String],
) -> Option<&'r mut Option<CacheControl>> {
    match marker_location(locator)? {
        MarkerLocation::TopLevel => Some(&mut req.cache_control),
        MarkerLocation::SystemBlock(index) => match req.system.as_mut()? {
            SystemContent::Blocks(blocks) => Some(&mut blocks.get_mut(index)?.cache_control),
            SystemContent::Text(_) => None,
        },
        MarkerLocation::Tool(index) => match req.tools.as_mut()?.get_mut(index)? {
            ToolDef::Custom(tool) => Some(&mut tool.cache_control),
            _ => None,
        },
    }
}

enum MarkerLocation {
    TopLevel,
    SystemBlock(usize),
    Tool(usize),
}

fn marker_location(locator: &[String]) -> Option<MarkerLocation> {
    let segments: Vec<&str> = locator.iter().map(String::as_str).collect();
    match segments.as_slice() {
        [CACHE_MARKER] => Some(MarkerLocation::TopLevel),
        ["system", index, CACHE_MARKER] => Some(MarkerLocation::SystemBlock(index.parse().ok()?)),
        ["tools", index, CACHE_MARKER] => Some(MarkerLocation::Tool(index.parse().ok()?)),
        _ => None,
    }
}

const CACHE_MARKER: &str = "cache_control";

/// What the client's own body holds at `locator`; `Some(None)` when it holds
/// nothing there, `None` when there is no client body to read.
fn client_wire_value(client_req: &ChatRequest, locator: &[String]) -> Slot {
    let ingress = client_req.routectl_internal.ingress_wire.as_ref()?;
    let body = serde_json::from_slice::<Value>(&ingress.bytes).ok()?;
    Some(locate(&body, locator).cloned())
}

/// What became of a rejection laid to one or more declared edits. Held on
/// [`DispatchMeta`] by the rejection's position in its pending list, and read
/// once when the rejections are recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevertState {
    /// Nothing was reverted: a conflict, a site that is not revertible, a
    /// revert that would not hold, or a spent budget.
    Declined,
    /// The edit was undone and the attempt re-sent; it has not succeeded.
    Reverted,
    /// The re-sent attempt succeeded.
    Succeeded,
}

/// The revert record of one rejection.
#[derive(Clone, Debug)]
pub struct WireRevert {
    rejection: usize,
    state: RevertState,
}

impl WireRevert {
    /// A rejection at position `rejection` that a declared edit explains,
    /// before any revert ran.
    pub(super) const fn declined(rejection: usize) -> Self {
        Self {
            rejection,
            state: RevertState::Declined,
        }
    }
}

/// The wire-check outcome of the rejection at position `rejection` when one
/// or more declared edits explained it: repaired only when its reverted
/// attempt succeeded. `None` when no declared edit explained it.
pub(super) fn revert_outcome(
    rejection: usize,
    reverts: &[WireRevert],
) -> Option<FieldRepairOutcome> {
    reverts
        .iter()
        .find(|revert| revert.rejection == rejection)
        .map(|revert| match revert.state {
            RevertState::Succeeded => FieldRepairOutcome::Repaired,
            RevertState::Declined | RevertState::Reverted => FieldRepairOutcome::Unrepaired,
        })
}

fn set_state(meta: &mut DispatchMeta, rejection: usize, state: RevertState) {
    for revert in &mut meta.wire_reverts {
        if revert.rejection == rejection {
            revert.state = state;
        }
    }
}

/// One chain target's revert: whether it spent its one revert, and which
/// rejection the attempt in flight re-sends after it.
#[derive(Debug, Default)]
pub(super) struct TargetRevert {
    attempted: bool,
    awaiting: Option<usize>,
}

impl TargetRevert {
    /// Undo `edit` on `attempt_req` for the rejection `site` settled, drawing
    /// one repair from `budget` last. `false` when this target already
    /// reverted, the revert does not hold, or the budget is spent; nothing is
    /// changed or charged then.
    pub(super) fn try_apply(
        &mut self,
        edit: &DeclaredEdit,
        site: &RejectionSite,
        client_req: &ChatRequest,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
    ) -> bool {
        if self.attempted {
            return false;
        }
        let Some(locator) = site.locator.as_ref() else {
            return false;
        };
        let rejection = site.pending;
        let Some(mut reverted) = revert(
            edit,
            locator.as_slice(),
            &site.sent,
            client_req,
            attempt_req,
        ) else {
            return false;
        };
        if !budget.draw() {
            return false;
        }
        // Remove one occurrence only: operator extras declare the same masked
        // pattern once per leaf, and the other leaves are still on the retry.
        let declared = &mut reverted.routectl_internal.declared_edits;
        if let Some(position) = declared.iter().position(|declared| declared == edit) {
            declared.remove(position);
        }
        *attempt_req = reverted;
        self.attempted = true;
        self.awaiting = Some(rejection);
        set_state(meta, rejection, RevertState::Reverted);
        true
    }

    /// The attempt in flight failed, so a revert it re-sent did not succeed.
    pub(super) const fn on_failure(&mut self) {
        self.awaiting = None;
    }

    /// The attempt in flight succeeded, and with it a revert it re-sent.
    pub(super) fn on_success(&mut self, meta: &mut DispatchMeta) {
        if let Some(rejection) = self.awaiting.take() {
            set_state(meta, rejection, RevertState::Succeeded);
        }
    }
}

/// Answer a rejection the settlement laid to declared edits: undo the one
/// edit, or, for several, nothing. `None` when no declared edit explains the
/// rejection, which leaves it to the learners; otherwise whether the edit
/// was undone on `attempt_req`.
pub(super) fn answer_declared_edit(
    revert: &mut TargetRevert,
    site: Option<&RejectionSite>,
    client_req: &ChatRequest,
    attempt_req: &mut ChatRequest,
    meta: &mut DispatchMeta,
    budget: &mut RepairBudget,
) -> Option<bool> {
    let site = site?;
    match &site.matched_edit {
        MatchedEdit::None => None,
        MatchedEdit::Conflict => Some(false),
        MatchedEdit::One(edit) => {
            Some(revert.try_apply(edit, site, client_req, attempt_req, meta, budget))
        }
    }
}

#[cfg(test)]
#[path = "learn_revert_tests.rs"]
mod learn_revert_tests;
