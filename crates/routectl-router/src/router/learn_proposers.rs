//! The four learner proposers the engine arbitrates between. Each is a thin
//! reading of one existing resolver over a [`RejectionInput`]; none mutates,
//! draws budget, or touches a registry. The order they are consulted in lives
//! in [`super::learn_engine::ARBITRATION_ORDER`], not here.

use routectl_core::sanitize_for_log;

use crate::capability_matcher::resolve_requested_capability;

use super::Router;
use super::class_observe::upstream_facts;
use super::field_repair::rejected_field;
use super::learn_engine::{Proposal, Proposer, RejectionInput, RejectionSite};
use super::rejection_parser::rejected_beta_flags;
use super::replay_repair::replay_rejection_body_free;

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
            input.target.egress_kind,
            RejectionSite::sent_effort_of(input.site.as_ref()),
        )?;
        Some(Proposal::FieldRow {
            row: named.row,
            kind: named.kind,
            tier: named.tier,
            value: named.value,
        })
    }
}

/// Proposes withholding the `anthropic-beta` flags a beta rejection names,
/// when every named flag is removable from this attempt.
///
/// A flag is removable only when the client sent it (read from the client's
/// request, not the attempt's operator-merged set), the attempt did not
/// already withhold it, and the attempt's beta assembly lists it in neither
/// the floor nor the body-implied set: the egress re-adds both after any
/// withhold, so a retry without them would ship the same flag again. An
/// attempt with no recorded assembly has nothing removable. A
/// forwarded-credential target ships the client's set untouched, so a
/// withhold there changes nothing. One unremovable flag refuses the whole
/// proposal: a retry still carrying it draws the same rejection.
#[derive(Clone, Copy, Debug)]
pub(super) struct BetaProposer;

impl Proposer for BetaProposer {
    fn propose(&self, input: &RejectionInput<'_>) -> Option<Proposal> {
        let provider_kind = input.target.provider_kind?;
        let facts = upstream_facts(input.err);
        let flags = rejected_beta_flags(
            provider_kind,
            facts.status?,
            facts.upstream_type,
            input.err.parsed_body(),
        )?;
        let refusal = if input.target.use_forwarded_credential {
            Some("forwarded credential")
        } else if let Some(assembly) = input.beta_assembly {
            flags.iter().find_map(|flag| {
                let listed = |set: &[String]| set.iter().any(|f| f == flag);
                if !input.client_betas.iter().any(|sent| sent.trim() == flag) {
                    Some("not a client flag")
                } else if listed(input.withheld_betas) {
                    Some("already withheld")
                } else if listed(assembly.floor()) {
                    Some("beta floor")
                } else if listed(assembly.body_implied()) {
                    Some("body-implied")
                } else {
                    None
                }
            })
        } else {
            Some("no beta assembly recorded")
        };
        if let Some(reason) = refusal {
            tracing::debug!(
                state_key = %sanitize_for_log(&input.target.state_key),
                count = flags.len(),
                reason,
                "named beta rejection not repairable",
            );
            return None;
        }
        Some(Proposal::Beta { flags })
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
