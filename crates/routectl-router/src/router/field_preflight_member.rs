// Learned drops of carried client members: a `field:` verdict no closed-table
// row names, minted by the reactive generic arm, applied before dispatch on the
// lane that learned it. `include!`d into field_preflight.rs; the types and
// imports it uses live there, so do not add `use` lines here.

/// One carried member the request sends whose verdict acts on the target's
/// lane: its index-free dotted path and the identity that verdict is keyed on.
struct MemberVerdict {
    path: String,
    key: FieldVerdictKey,
}

impl Router {
    /// The carried members `req` sends whose learned drop acts on `target`'s
    /// lane, in path order.
    ///
    /// Only on a fidelity lane: a carried member reaches the wire only where the
    /// client's dialect is the egress's, which is the only lane its verdict
    /// can have been learned on. A member whose verdict is absent or not acting
    /// produces no decision, so a request's other unknowns add no records.
    fn acting_member_verdicts(
        &self,
        req: &ChatRequest,
        target: &DispatchTarget,
        lane: Option<&AttributableLane<'_>>,
    ) -> Vec<MemberVerdict> {
        let (Some(provider_kind), Some(_), Some(_), Some(learned_lane)) = (
            self.preflight_lane_admits(target),
            lane,
            fidelity_dialect(target, req),
            target.learned_lane.as_ref(),
        ) else {
            return Vec::new();
        };
        let table = self.field_repair_table();
        let generation = self.registry_generation();
        let now = Instant::now();
        super::field_repair::carried_member_paths(req)
            .into_iter()
            .filter_map(|path| {
                let key = FieldVerdictKey::new(learned_lane, &path, provider_kind)?;
                super::field_repair::carried_member_path_of_capability_key(
                    table,
                    key.capability_key(),
                )?;
                self.field_verdicts()
                    .is_negative_acting_in_generation(&key, generation, now)
                    .then_some(MemberVerdict { path, key })
            })
            .collect()
    }

    /// Decide one learned member drop and, when every envelope gate clears,
    /// remove the member from every carrier at its exact position on `planned`.
    ///
    /// Never a canary: the canary's repaired retry drops a closed-table surface
    /// and has no member to restore, so a member verdict re-verifies through
    /// decay alone.
    fn plan_one_member<'a>(
        &'a self,
        planned: &mut ChatRequest,
        target: &DispatchTarget,
        member: MemberVerdict,
    ) -> RowDecision<'a> {
        let member_path = sanitize_for_log(&member.path);
        let refuse = |reason| {
            let mut record = unchanged_record(
                target,
                Some(super::field_repair::FIELD_PATH_CARRIED_MEMBER),
                Some(TransformClass::Envelope),
                reason,
            );
            record.member_path = Some(member_path.clone());
            RowDecision {
                record,
                canary: None,
                accounting: None,
            }
        };
        let Some(provider_kind) = self.preflight_lane_admits(target) else {
            return refuse(FIELD_PREFLIGHT_UNSUPPORTED_LANE);
        };
        let gates = GateInputs {
            target,
            key: &member.key,
            provider_kind,
            class: TransformClass::Envelope,
            generation: self.registry_generation(),
            now: Instant::now(),
        };
        let authorization = match self.preflight_authorization_for(gates) {
            Ok(authorization) => authorization,
            Err(reason) => return refuse(reason),
        };
        let Some((adopted, pointer)) =
            super::field_repair::without_carried_member(planned, &member.path)
        else {
            return refuse(FIELD_PREFLIGHT_AMBIGUOUS_MUTATION);
        };
        let Some(accounting) = self
            .field_verdicts()
            .canaries()
            .begin_modified_request(&member.key, authorization.incarnation)
        else {
            return refuse(FIELD_PREFLIGHT_NOT_ELIGIBLE);
        };
        *planned = adopted;
        super::edit_declare::declare(planned, EditSite::Preflight, pointer, EditKind::Remove);
        self.metrics.incr_field_preflight_action();
        let mut record = unchanged_record(
            target,
            Some(super::field_repair::FIELD_PATH_CARRIED_MEMBER),
            Some(TransformClass::Envelope),
            FIELD_PREFLIGHT_ACTION_DROP,
        );
        record.acted = true;
        record.authorization = Some(authorization_record(authorization));
        record.member_path = Some(member_path);
        RowDecision {
            record,
            canary: None,
            accounting: Some(accounting),
        }
    }
}
