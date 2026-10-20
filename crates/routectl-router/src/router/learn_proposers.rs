//! The three learner proposers the engine arbitrates between. Each is a thin
//! reading of one existing resolver over a [`RejectionInput`]; none mutates,
//! draws budget, or touches a registry. The order they are consulted in lives
//! in [`super::learn_engine::ARBITRATION_ORDER`], not here.

use crate::capability_matcher::resolve_requested_capability;

use super::Router;
use super::dispatch::replay_rejection_body_free;
use super::field_repair::rejected_field;
use super::learn_engine::{Proposal, Proposer, RejectionInput};

/// Proposes the reasoning-replay strip when the attempt carried replay
/// artifacts and the EFFECTIVE (remapped) class is the proven replay
/// rejection.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReplayProposer;

impl Proposer for ReplayProposer {
    fn propose(&self, input: &RejectionInput<'_>) -> Option<Proposal> {
        (input.replay_carried && Router::is_replay_rejection_class(&input.class.class))
            .then_some(Proposal::ReplayStrip)
    }
}

/// Proposes an edit of the closed-table row the rejection names, read from
/// the NATIVE class through the field-rejection parser.
#[derive(Clone, Copy, Debug)]
pub(super) struct FieldRowProposer;

impl Proposer for FieldRowProposer {
    fn propose(&self, input: &RejectionInput<'_>) -> Option<Proposal> {
        let named = rejected_field(
            input.table,
            input.native_class,
            input.err,
            input.target.provider_kind.unwrap_or(""),
        )?;
        Some(Proposal::FieldRow {
            row: named.row,
            kind: named.kind,
            tier: named.tier,
            value: named.value,
        })
    }
}

/// Proposes a generic capability negative for the capability the resolver
/// attributes the rejection to.
///
/// Resolves against the error the generic learner is handed: a replay
/// rejection's body-free rebuild, so a body the walk has already discarded
/// cannot attribute a capability.
#[derive(Clone, Copy, Debug)]
pub(super) struct CapabilityProposer;

impl Proposer for CapabilityProposer {
    fn propose(&self, input: &RejectionInput<'_>) -> Option<Proposal> {
        let provider_kind = input.target.provider_kind?;
        let body_free =
            replay_rejection_body_free(input.err, &input.class.class, &input.target.provider_name);
        let err = body_free.as_ref().unwrap_or(input.err);
        resolve_requested_capability(provider_kind, err, input.class).map(Proposal::Capability)
    }
}
