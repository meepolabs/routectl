//! What an admitted request contributes to a probe: the bounded payload
//! captured from it, and the activation seam that turns that capture into
//! queued work.
//!
//! Split from the lifecycle because the two answer different questions. The
//! lifecycle owns WHEN work runs and how it settles; this owns WHAT a probe
//! carries -- which closed-table field, under which beta context -- and the
//! refusals that arise while deciding it.
//!
//! A probe is a RE-CHECK, never discovery: it runs only for a field the lane
//! already holds an acting negative on, whose row a free token count
//! validates, on a kind with such a count. A request merely carrying a field
//! asks nothing -- the reactive arm learns from real rejections.
//!
//! Everything here is `&self` bookkeeping: it dials nothing and awaits
//! nothing, so the admitted request that triggers it is never delayed.

use std::time::Instant;

use super::Router;
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::validator_plan;

/// The count-validated closed-table fields one attempt carried BEFORE the
/// pre-flight planner ran, each with its value.
///
/// Taken before pre-flight because an acting negative is exactly what makes
/// pre-flight drop the field from the attempt: read after it, the field a
/// re-check exists to ask about is already gone. Holds only the closed table's
/// own `(path, value)` pairs, never the request, and lives for one attempt.
#[derive(Debug, Default)]
pub(super) struct ProbeFieldCapture {
    fields: Vec<(&'static str, String)>,
}

impl ProbeFieldCapture {
    /// Capture `req`'s count-validated closed-table fields.
    pub(super) fn of(
        table: super::field_repair::FieldRepairTable,
        req: &routectl_core::ChatRequest,
    ) -> Self {
        Self {
            fields: super::field_repair::recheck_payloads(table, req).collect(),
        }
    }
}

impl Router {
    /// Activate probe lanes for a request the dispatch walk has proven to
    /// be admitted real traffic against `target`.
    ///
    /// `fields` is the attempt's pre-flight capture; `attempt_req` is the
    /// PER-TARGET request (post-overlay, post-strip) the beta context is read
    /// from, since that is the header the upstream actually receives.
    pub(super) fn on_admitted_request(
        &self,
        target: &super::DispatchTarget,
        fields: &ProbeFieldCapture,
        attempt_req: &routectl_core::ChatRequest,
    ) {
        let Some(lane) = target.learned_lane.as_ref() else {
            return;
        };
        self.activate_probe_lanes_for_admitted_request(
            lane,
            &target.provider_name,
            target.provider_kind,
            target.use_forwarded_credential,
            fields,
            attempt_req,
        );
    }

    /// Queue a re-check for every captured field this lane holds an ACTING
    /// negative on. THE production entry point, and the only path by which a
    /// lane ever enters the scheduler.
    ///
    /// In refusal order:
    ///
    /// - the lane is not one [`Router::probe_lane_kind`] admits: the shared
    ///   attributability decision (forwarded credential, missing entry,
    ///   unclassifiable or loopback egress, a kind field repair does not act
    ///   on) plus a kind with no free token count for this upstream. The
    ///   pre-dial recheck asks the same question;
    /// - a captured field whose identity holds no acting negative on this
    ///   lane: with nothing learned there is nothing to re-check;
    /// - a payload breaching a retention bound, counted and diagnosed.
    ///
    /// Only count-validated rows are ever captured, so a row whose count
    /// cannot see the field never reaches here.
    pub(super) fn activate_probe_lanes_for_admitted_request(
        &self,
        lane: &crate::state_key::StateKey,
        provider_name: &str,
        provider_kind: Option<&str>,
        use_forwarded_credential: bool,
        fields: &ProbeFieldCapture,
        attempt_req: &routectl_core::ChatRequest,
    ) {
        if fields.fields.is_empty() {
            return;
        }
        let Some(kind) =
            self.probe_lane_kind(provider_name, use_forwarded_credential, lane.upstream())
        else {
            return;
        };
        // A reload may have re-kinded the entry under a target expanded from
        // the old one; the identity carries the target's kind.
        if provider_kind != Some(kind) {
            return;
        }
        let generation = self.registry_generation();
        let now = Instant::now();
        for (path, field_value) in &fields.fields {
            let Some(key) = FieldVerdictKey::new(lane, path, kind) else {
                continue;
            };
            if !self
                .field_verdicts()
                .is_negative_acting_in_generation(&key, generation, now)
            {
                continue;
            }
            self.activate_recheck(key, path, field_value, attempt_req);
        }
    }

    /// Build the bounded payload for one acting identity and queue it on the
    /// free plan.
    fn activate_recheck(
        &self,
        key: FieldVerdictKey,
        path: &'static str,
        field_value: &str,
        attempt_req: &routectl_core::ChatRequest,
    ) {
        // The admitted request's CLAUDE-CODE classification, read once: the
        // egress's `is_non_cc` decision gates the beta floor, so the probe must
        // land on the same side of it as the request it probes for. A bit, not
        // the session capture -- see `ProbePayload`.
        let had_cc_session = routectl_core::identity::anthropic::has_claude_code_session(
            &attempt_req.routectl_internal.claude_code_headers,
        );
        // A flag withheld for this lane's client traffic never rides a
        // background probe either.
        let withheld = &attempt_req.routectl_internal.withheld_betas;
        let client_betas: Vec<String> = attempt_req
            .anthropic_beta
            .iter()
            .filter(|flag| !withheld.contains(flag))
            .cloned()
            .collect();
        // The two beta sources stay SEPARATE all the way through: the egress
        // composes the client set and the operator set at different points of
        // the header, so a union reapplied to either carrier would send a header
        // the admitted request did not. Any bound or validity breach refuses the
        // payload -- counted and diagnosed rather than silently skipped, since an
        // un-probed lane otherwise looks like one that was never admitted.
        let Some(payload) = crate::probe_scheduler::ProbePayload::new(
            path,
            field_value.to_string(),
            &client_betas,
            &attempt_req.routectl_internal.operator_betas,
            had_cc_session,
        ) else {
            self.probe_scheduler.note_payload_refusal();
            self.warn_probe_payload_refused_once();
            return;
        };
        self.activate_probe_plan(&key, validator_plan(true), payload);
    }

    /// Emit the beta-retention refusal WARN at most once per ROUTER
    /// INCARNATION, keeping the counter as the volume signal.
    ///
    /// Bounded for the same reason as the queue-full line: the condition is a
    /// property of a client's beta set, so a client that keeps sending one
    /// refused set would put a WARN on every one of its requests. Per
    /// incarnation because the latch is a field on `Router` -- a reload
    /// publishes a new one with the latch clear, and a refusal under a new
    /// configuration is new information.
    fn warn_probe_payload_refused_once(&self) {
        let mut warned = self.probe_payload_refused_warned.lock();
        if *warned {
            return;
        }
        *warned = true;
        drop(warned);
        // No beta token, field value, or identity reaches the line: the whole
        // point of the refusal is that the retained shape was unacceptable, and
        // logging it would put exactly that shape in the log. The counter
        // carries the volume.
        tracing::warn!(
            payload_refusals_total = self.probe_scheduler_snapshot().payload_refusals_total,
            "{}",
            crate::probe_scheduler::PROBE_PAYLOAD_REFUSED_EVENT,
        );
    }
}
