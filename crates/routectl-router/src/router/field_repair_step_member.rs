//! The generic arm of the reactive field repair: a member the client sent in
//! a node carrier (message, content part, custom tool, system block) or as a
//! top-level `provider_extras` key, refused by name.
//!
//! The closed table cannot name such a member: its name is the client's. So
//! this arm acts only on a path that is EXACTLY the position of a member a
//! carrier holds, segment for segment, and removes that key from every
//! carrier at that position and nothing else. A parent, a child, or a typed
//! field at the same position names no carried member, so it removes
//! nothing; typed fields never sit in a carrier, so no typed field is
//! reachable. Verbatim nodes (`ContentPart::Other`, `ToolDef::Other`) and
//! sidecars are not carriers.
//!
//! A member step settles through the plan like a closed-table step: it is
//! pending until the repaired retry succeeds or is refused for a different
//! field, and only then commits `field:<path>` through its own guard.

use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::SignalTier;
use routectl_core::wire_check::{EditKind, EditSite};
use routectl_core::{ChatRequest, ContentPart, MessageContent, SystemContent, ToolDef};
use serde_json::{Map, Value};

use crate::field_verdict::{FieldAdmission, FieldRepairGuard, FieldVerdictKey};

use super::super::super::DispatchMeta;
use super::super::super::edit_declare::declare;
use super::super::super::rejection_parser::{FieldIdentity, RawPath};
use super::super::super::repair_budget::RepairBudget;
use super::super::FieldRepairHandle;
use super::{FieldRepairPlan, SettledStep};

/// Where a node carrier sits in a canonical request, as the index-free
/// segments of the wire path its members are emitted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Carrier {
    /// The client's top-level `provider_extras` keys.
    TopLevel,
    /// `Message.extras`.
    Message,
    /// The extras of a typed content part.
    Part,
    /// `CustomTool.extras`.
    Tool,
    /// `SystemBlock.extras`.
    SystemBlock,
}

impl Carrier {
    const ALL: [Self; 5] = [
        Self::TopLevel,
        Self::Message,
        Self::Part,
        Self::Tool,
        Self::SystemBlock,
    ];

    const fn position(self) -> &'static [&'static str] {
        match self {
            Self::TopLevel => &[],
            Self::Message => &["messages"],
            Self::Part => &["messages", "content"],
            Self::Tool => &["tools"],
            Self::SystemBlock => &["system"],
        }
    }

    /// The wire pointer pattern a removal of `key` from every carrier of
    /// this kind is declared under.
    fn pointer(self, key: &str) -> String {
        match self {
            Self::TopLevel => format!("/{key}"),
            Self::Message => format!("/messages/*/{key}"),
            Self::Part => format!("/messages/*/content/*/{key}"),
            Self::Tool => format!("/tools/*/{key}"),
            Self::SystemBlock => format!("/system/*/{key}"),
        }
    }
}

/// Whether the member `key` of a carrier at `position` is exactly the member
/// `path` names: the same segments, in order, and nothing more or less.
fn names_member(path: &[&str], position: &[&str], key: &str) -> bool {
    let member: Vec<&str> = position.iter().copied().chain([key]).collect();
    member.as_slice() == path
}

/// Whether `key` in a carrier of `kind` is a member the client sent
/// unmodeled. A top-level extras key spelled as a canonical request field is
/// one routectl writes itself, never a client unknown.
fn is_client_member(kind: Carrier, key: &str) -> bool {
    kind != Carrier::TopLevel || !routectl_core::is_canonical_request_key(key)
}

/// The carrier maps of `kind` in `req`.
fn carriers(req: &ChatRequest, kind: Carrier) -> Vec<&Map<String, Value>> {
    match kind {
        Carrier::TopLevel => req
            .provider_extras
            .as_ref()
            .and_then(Value::as_object)
            .into_iter()
            .collect(),
        Carrier::Message => req.messages.iter().map(|message| &message.extras).collect(),
        Carrier::Part => req
            .messages
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::Parts(parts) => Some(parts),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                ContentPart::Known(known) => Some(part_extras(known)),
                ContentPart::Other { .. } => None,
            })
            .collect(),
        Carrier::Tool => req
            .tools
            .iter()
            .flatten()
            .filter_map(|tool| match tool {
                ToolDef::Custom(tool) => Some(&tool.extras),
                ToolDef::Other(_) => None,
            })
            .collect(),
        Carrier::SystemBlock => match &req.system {
            Some(SystemContent::Blocks(blocks)) => {
                blocks.iter().map(|block| &block.extras).collect()
            }
            _ => Vec::new(),
        },
    }
}

/// Apply `remove` to every carrier map of `kind` in `req`. Clones the shared
/// message list only when `kind` lives in it.
fn for_each_carrier_mut(
    req: &mut ChatRequest,
    kind: Carrier,
    mut remove: impl FnMut(&mut Map<String, Value>),
) {
    match kind {
        Carrier::TopLevel => {
            if let Some(extras) = req.provider_extras.as_mut().and_then(Value::as_object_mut) {
                remove(extras);
            }
        }
        Carrier::Message => {
            for message in Arc::make_mut(&mut req.messages) {
                remove(&mut message.extras);
            }
        }
        Carrier::Part => {
            for message in Arc::make_mut(&mut req.messages) {
                if let MessageContent::Parts(parts) = &mut message.content {
                    for part in parts {
                        if let ContentPart::Known(known) = part {
                            remove(part_extras_mut(known));
                        }
                    }
                }
            }
        }
        Carrier::Tool => {
            for tool in req.tools.iter_mut().flatten() {
                if let ToolDef::Custom(tool) = tool {
                    remove(&mut tool.extras);
                }
            }
        }
        Carrier::SystemBlock => {
            if let Some(SystemContent::Blocks(blocks)) = &mut req.system {
                for block in blocks {
                    remove(&mut block.extras);
                }
            }
        }
    }
}

const fn part_extras(part: &routectl_core::KnownContentPart) -> &Map<String, Value> {
    use routectl_core::KnownContentPart as Part;
    match part {
        Part::Text { extras, .. }
        | Part::Image { extras, .. }
        | Part::ImageUrl { extras, .. }
        | Part::File { extras, .. }
        | Part::Document { extras, .. }
        | Part::ToolUse { extras, .. }
        | Part::ToolResult { extras, .. }
        | Part::Thinking { extras, .. }
        | Part::RedactedThinking { extras, .. } => extras,
    }
}

const fn part_extras_mut(part: &mut routectl_core::KnownContentPart) -> &mut Map<String, Value> {
    use routectl_core::KnownContentPart as Part;
    match part {
        Part::Text { extras, .. }
        | Part::Image { extras, .. }
        | Part::ImageUrl { extras, .. }
        | Part::File { extras, .. }
        | Part::Document { extras, .. }
        | Part::ToolUse { extras, .. }
        | Part::ToolResult { extras, .. }
        | Part::Thinking { extras, .. }
        | Part::RedactedThinking { extras, .. } => extras,
    }
}

/// The carrier kind holding the member `path` names in `req`, or `None`
/// when no carrier holds exactly that member.
fn carried_member(req: &ChatRequest, path: &[&str]) -> Option<Carrier> {
    Carrier::ALL.into_iter().find(|&kind| {
        carriers(req, kind).into_iter().any(|map| {
            map.keys()
                .any(|key| is_client_member(kind, key) && names_member(path, kind.position(), key))
        })
    })
}

/// Remove the member `path` names from every carrier of `kind` in `req`,
/// returning how many carriers held it.
fn remove_member(req: &mut ChatRequest, kind: Carrier, path: &[&str]) -> usize {
    let mut removed = 0;
    for_each_carrier_mut(req, kind, |map| {
        let before = map.len();
        map.retain(|key, _| !names_member(path, kind.position(), key));
        removed += before - map.len();
    });
    removed
}

/// The carrier kind whose members sit at exactly `segments`: a carrier
/// position plus one client member key that the capability strip does not
/// own. A structural answer, so it holds for a persisted path whatever a
/// request carries.
fn member_carrier(segments: &[&str]) -> Option<Carrier> {
    let (key, position) = segments.split_last()?;
    if crate::capability_strip::strippable_keys().any(|owned| owned == *key) {
        return None;
    }
    Carrier::ALL
        .into_iter()
        .find(|&kind| kind.position() == position && is_client_member(kind, key))
}

/// Whether the index-free dotted `path` names a carried client member the
/// generic arm can remove: the paths its verdicts are keyed on.
pub(in crate::router) fn is_carried_member_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    member_carrier(&segments).is_some()
}

/// The index-free dotted paths of every client member `req` carries in a
/// carrier, each once, in sorted order.
pub(in crate::router) fn carried_member_paths(req: &ChatRequest) -> Vec<String> {
    let mut paths = std::collections::BTreeSet::new();
    for kind in Carrier::ALL {
        for map in carriers(req, kind) {
            for key in map.keys().filter(|key| is_client_member(kind, key)) {
                let segments: Vec<&str> = kind
                    .position()
                    .iter()
                    .copied()
                    .chain([key.as_str()])
                    .collect();
                if member_carrier(&segments).is_some() {
                    paths.insert(segments.join("."));
                }
            }
        }
    }
    paths.into_iter().collect()
}

/// A clone of `req` without the member `path` names in any carrier at that
/// exact position, with the wire pointer pattern the removal is declared
/// under. `None` when no carrier held it, or it is still carried after.
pub(in crate::router) fn without_carried_member(
    req: &ChatRequest,
    path: &str,
) -> Option<(ChatRequest, String)> {
    let segments: Vec<&str> = path.split('.').collect();
    let kind = member_carrier(&segments)?;
    let mut scratch = req.clone();
    if remove_member(&mut scratch, kind, &segments) == 0
        || carried_member(&scratch, &segments).is_some()
    {
        return None;
    }
    Some((scratch, kind.pointer(segments.last()?)))
}

/// Whether `req` carries any member the generic arm could be asked to
/// remove: a client top-level extras key or a non-empty node carrier.
pub(in crate::router) fn carries_unmodeled_members(req: &ChatRequest) -> bool {
    Carrier::ALL
        .into_iter()
        .any(|kind| carriers(req, kind).into_iter().any(|map| !map.is_empty()))
}

/// One applied member removal awaiting settlement.
pub(super) struct MemberStep<'a> {
    path: RawPath,
    /// The index-free dotted path, the verdict key's path and the repair
    /// record's member path.
    dotted: String,
    tier: SignalTier,
    reject_status: u16,
    handle: Option<FieldRepairHandle>,
    guard: Option<FieldRepairGuard<'a>>,
}

impl MemberStep<'_> {
    pub(super) fn identity(&self) -> FieldIdentity {
        FieldIdentity::folded(self.path.segments())
    }

    /// Persist this step's verdict, or release its guard when it holds none.
    /// A member step answers a refused member, never a refused value, so it
    /// always teaches when it can settle.
    pub(super) fn commit(self, request_features: &[String], now: Instant) -> SettledStep {
        let learned = self.guard.and_then(|guard| {
            guard.commit(
                self.tier,
                None,
                self.reject_status,
                request_features.to_vec(),
                now,
            )
        });
        SettledStep {
            handle: self.handle,
            learned,
        }
    }
}

/// The guard a member step settles through, read before the mutation.
enum MemberClaim<'a> {
    Held(FieldRepairGuard<'a>),
    /// A non-settling plan, or a sibling request holds the slot: repair the
    /// attempt and settle nothing.
    Unheld,
}

impl<'a> FieldRepairPlan<'a> {
    /// Remove the client member `path` names from every carrier at exactly
    /// that position on `attempt_req`, for a rejection already attributed to
    /// a member the client sent unchanged on a fidelity lane.
    ///
    /// `Some(status)`: the member was removed, one repair was charged, the
    /// removal was declared, and a step awaits settlement. `None`: nothing
    /// changed and nothing was charged -- the plan is not on a fidelity lane,
    /// no carrier holds exactly that member, this target already repaired
    /// it, the allowance is spent, or the verdict cannot be keyed or acts.
    pub(in crate::router) fn apply_member(
        &mut self,
        path: &RawPath,
        tier: SignalTier,
        attempt_req: &mut ChatRequest,
        meta: &mut DispatchMeta,
        budget: &mut RepairBudget,
        reject_status: u16,
    ) -> Option<u16> {
        self.wire?;
        let segments: Vec<&str> = path.segments().collect();
        let identity = FieldIdentity::folded(segments.iter().copied());
        if self.committed.contains(&identity)
            || self.members.iter().any(|step| step.identity() == identity)
        {
            return None;
        }
        let kind = carried_member(attempt_req, &segments)?;
        let key = *segments.last()?;
        let dotted = segments.join(".");
        if !budget.check_or_mark_refusal() {
            return None;
        }
        let claim = self.claim_member(&dotted)?;
        let removed = remove_member(attempt_req, kind, &segments);
        debug_assert!(removed > 0, "a carried member is removed from its carrier");
        let charged = budget.draw();
        debug_assert!(charged, "check_or_mark_refusal held before the mutation");
        declare(
            attempt_req,
            EditSite::FieldRepair,
            kind.pointer(key),
            EditKind::Remove,
        );
        for candidate in &mut self.candidates {
            drop(candidate.guard.take());
        }
        super::super::super::replay_repair::restamp_calibration_estimate(attempt_req, meta);
        self.members.push(MemberStep {
            path: path.clone(),
            dotted,
            tier,
            reject_status,
            handle: None,
            guard: match claim {
                MemberClaim::Held(guard) => Some(guard),
                MemberClaim::Unheld => None,
            },
        });
        self.applied = true;
        Some(reject_status)
    }

    /// The guard a member step on `dotted` settles through. A settling plan
    /// that cannot key the member, or on which a verdict for it already
    /// acts, repairs nothing.
    fn claim_member(&self, dotted: &str) -> Option<MemberClaim<'a>> {
        let Some(admission) = self.reclaim.as_ref() else {
            return Some(MemberClaim::Unheld);
        };
        let key = FieldVerdictKey::new(&admission.lane, dotted, admission.provider_kind)?;
        match admission.registry.admit_provisional(
            &key,
            &admission.base_url,
            admission.generation,
            Instant::now(),
        ) {
            FieldAdmission::Admitted(guard) => Some(MemberClaim::Held(guard)),
            FieldAdmission::Contended => Some(MemberClaim::Unheld),
            FieldAdmission::Refused => None,
        }
    }

    /// The latest member step's dotted path, for its repair record.
    pub(in crate::router) fn latest_member_path(&self) -> Option<String> {
        self.members.last().map(|step| step.dotted.clone())
    }

    /// Bind the latest member step to the request record `handle` names.
    pub(in crate::router) fn stamp_latest_member(&mut self, handle: FieldRepairHandle) {
        if let Some(step) = self.members.last_mut() {
            step.handle = Some(handle);
        }
    }
}
