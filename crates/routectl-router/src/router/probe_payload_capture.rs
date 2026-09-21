//! What an admitted request contributes to a probe: the bounded payload
//! captured from it, and the activation seam that turns that capture into
//! queued work.
//!
//! Split from the lifecycle because the two answer different questions. The
//! lifecycle owns WHEN work runs and how it settles; this owns WHAT a probe
//! carries -- which closed-table field, under which beta context, classified
//! which way -- and the refusals that arise while deciding it.
//!
//! Everything here is `&self` bookkeeping: it dials nothing and awaits
//! nothing, so the admitted request that triggers it is never delayed.

use super::Router;
use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::validator_plan;

impl Router {
    /// Activate probe lanes for a request the dispatch walk has proven to
    /// be admitted real traffic against `target`, reading the PER-TARGET
    /// request (post-overlay, post-strip) rather than the ingress one.
    ///
    /// The per-target request is the body that will actually go upstream.
    /// Reading the ingress request instead would probe a capability an
    /// overlay added but the target never sends, or miss one the overlay
    /// introduced -- in both directions the probed identity would describe
    /// bytes no upstream ever saw.
    pub(super) fn on_admitted_request(
        &self,
        target: &super::DispatchTarget,
        attempt_req: &routectl_core::ChatRequest,
    ) {
        self.activate_probe_lanes_for_admitted_request(
            &target.state_key,
            &target.provider_name,
            target.provider_kind,
            target.use_forwarded_credential,
            attempt_req,
        );
    }

    /// Activate every probe lane an ADMITTED REAL REQUEST grounds on one
    /// resolved dispatch target. THE production entry point, and the only
    /// path by which a lane ever enters the scheduler.
    ///
    /// The refusals MIRROR `Router::plan_field_carry`'s exactly, because a
    /// probe produces evidence for the verdicts that arm mints: a target
    /// the arm may not act on must not be probed. In refusal order:
    ///
    /// - the learned-capability kill switch is off;
    /// - a kind other than the one lane this stage acts on;
    /// - a target authenticating with a FORWARDED client credential, whose
    ///   rejections are not attributable to a routectl-owned seat;
    /// - a provider entry that yields NO attributable Anthropic base URL.
    ///   This is what excludes the Bedrock Mantle shape, whose entry reads
    ///   `anthropic-api` while egressing through Mantle: the accessor
    ///   answers `None` for it, so the refusal comes from the same
    ///   accessor the reactive arm uses rather than a second base-url read
    ///   that could disagree;
    /// - a base URL naming a LOOPBACK destination, through the same
    ///   suppression predicate;
    /// - a request grounding no closed-table capability, which has no
    ///   identity to probe.
    ///
    /// Identities come from the existing normalized [`FieldVerdictKey`]
    /// constructor over the closed table's own path literals, so no
    /// permanent field key is assembled here.
    pub(crate) fn activate_probe_lanes_for_admitted_request(
        &self,
        state_key: &str,
        provider_name: &str,
        provider_kind: Option<&str>,
        use_forwarded_credential: bool,
        attempt_req: &routectl_core::ChatRequest,
    ) {
        let Some(kind) = provider_kind else {
            return;
        };
        if kind != super::field_repair::ANTHROPIC_API_KIND {
            return;
        }
        // THE shared attributability decision, fed the forwarded fact the
        // dispatch walk derived for THIS target. The pre-dial recheck calls the
        // same decision through `probe_entry_is_attributable`, which reads the
        // forwarded fact off the entry instead -- it has no target by then, and
        // a reload may have replaced the entry since.
        if super::field_repair::attributable_anthropic_base_url(
            &self.config,
            provider_name,
            use_forwarded_credential,
        )
        .is_none()
        {
            return;
        }
        // `count_tokens` is available on this lane, so the plan leads with that
        // free validator. The expected-rejection class does NOT follow it: it
        // has no grounded template, so `validator_plan` omits it entirely and
        // the plan's only other entry is the paid class, which this stage never
        // dials.
        let plan = validator_plan(true);
        // The admitted request's CLAUDE-CODE classification, read once: the
        // egress's `is_non_cc` decision gates the beta floor, so the probe must
        // land on the same side of it as the request it probes for. A bit, not
        // the session capture -- see `ProbePayload`.
        let had_cc_session = routectl_core::identity::anthropic::has_claude_code_session(
            &attempt_req.routectl_internal.claude_code_headers,
        );
        for (path, field_value) in super::field_repair::grounded_closed_table_payloads(attempt_req)
        {
            let Some(key) = FieldVerdictKey::new(state_key, path, kind) else {
                continue;
            };
            // The two beta sources stay SEPARATE all the way through: the
            // egress subjects the client set to `allowed_betas` and exempts the
            // operator set, so a union reapplied to either carrier would send a
            // header the admitted request did not. Any bound or validity breach
            // refuses the payload -- counted and diagnosed rather than silently
            // skipped, since an un-probed lane otherwise looks like one that was
            // never admitted.
            let Some(payload) = crate::probe_scheduler::ProbePayload::new(
                path,
                field_value,
                &attempt_req.anthropic_beta,
                &attempt_req.routectl_internal.operator_betas,
                had_cc_session,
            ) else {
                self.probe_scheduler.note_payload_refusal();
                self.warn_probe_payload_refused_once();
                continue;
            };
            self.activate_probe_plan(&key, plan.clone(), payload);
        }
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
