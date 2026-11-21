//! Alias step shares: an alias group's requests split over the top-level
//! steps of the chain its label resolves to, from the group's per-model
//! request counts over the selected range and the request's pinned config.
//!
//! The ledger's `alias` column holds the client's wire name, so the chain is
//! found by the router's own resolver, never by reading the label as a key.
//! The split always accounts for every request of the group:
//! `sum(steps.requests) + off_chain + not_dispatched == requests`.

use std::collections::{BTreeMap, BTreeSet};

use routectl_router::{AliasStep, AliasStepKind};
use serde::Serialize;

use super::super::router_view::QueryPricer;

/// An alias group's requests, split by the chain step that served them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct AliasShares {
    /// Every top-level chain step in chain order, each with the requests
    /// served by a model it owns; empty when the label resolves to no chain.
    steps: Vec<StepShare>,
    /// Requests served by a model the chain does not name.
    off_chain: i64,
    /// Requests no model served.
    not_dispatched: i64,
}

/// One top-level chain step and the requests its models served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct StepShare {
    /// The step as written in the chain.
    step: String,
    kind: StepKind,
    requests: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum StepKind {
    Model,
    Alias,
}

impl From<AliasStepKind> for StepKind {
    fn from(kind: AliasStepKind) -> Self {
        match kind {
            AliasStepKind::Model => Self::Model,
            AliasStepKind::Alias => Self::Alias,
        }
    }
}

/// The step shares of the alias group `label` holding `requests` requests,
/// `by_model` of them served, against the chain `pricer`'s config resolves
/// `label` to. A chain nested past the router's depth bound has no steps, so
/// every served request of the group counts as off the chain.
pub(super) fn alias_shares(
    label: &str,
    requests: i64,
    by_model: &BTreeMap<String, i64>,
    pricer: &QueryPricer,
) -> AliasShares {
    let steps = pricer.alias_chain_steps(label).unwrap_or_else(|err| {
        tracing::debug!(
            alias = label,
            error = %err,
            "status query: alias chain did not expand; its served requests count off the chain"
        );
        Vec::new()
    });
    split(&steps, requests, by_model)
}

/// `requests` split over `steps`: each served model counts toward the first
/// step whose expansion names it, a served model no step names counts off
/// the chain, and the requests no model served are the remainder.
fn split(steps: &[AliasStep], requests: i64, by_model: &BTreeMap<String, i64>) -> AliasShares {
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    let shares: Vec<StepShare> = steps
        .iter()
        .map(|step| {
            let served = step
                .models
                .iter()
                .filter(|model| claimed.insert(model.as_str()))
                .filter_map(|model| by_model.get(model))
                .sum();
            StepShare {
                step: step.name.clone(),
                kind: step.kind.into(),
                requests: served,
            }
        })
        .collect();
    let served: i64 = by_model.values().sum();
    let on_chain: i64 = shares.iter().map(|share| share.requests).sum();
    AliasShares {
        steps: shares,
        off_chain: served - on_chain,
        not_dispatched: requests - served,
    }
}
