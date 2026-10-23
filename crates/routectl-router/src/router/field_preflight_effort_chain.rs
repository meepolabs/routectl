// The effort rows' chain through learned levels: a pre-flight that rewrote a
// request's effort to a learned level reads that level's own row next, and
// keeps going while each written level's verdict acts. `include!`d into
// field_preflight.rs; the types and imports it uses live there, so do not add
// `use` lines here.

impl<'a> FieldPreflightPlan<'a> {
    /// Take one row's decision into this plan: its record, the canary it
    /// claimed, and its accounting.
    ///
    /// Every acting row's accounting is RETAINED, not just the first: each
    /// entry belongs to a different identity, and a dropped one would report
    /// zero requests in flight for an identity this request is in fact
    /// modifying.
    fn absorb(&mut self, records: &mut Vec<FieldPreflight>, decided: RowDecision<'a>) {
        records.push(decided.record);
        if let Some(canary) = decided.canary {
            debug_assert!(
                self.canary.is_none(),
                "a row may only claim the canary slot while it is free",
            );
            self.canary = Some(canary);
        }
        self.accounting.extend(decided.accounting);
    }
}

impl Router {
    /// Follow the effort chain from the level a ranked row just wrote onto
    /// `planned`: decide the row of the level the request now emits, and while
    /// that row acts, the row of the level it wrote, and so on.
    ///
    /// Each hop is a full row decision through [`Router::plan_one_row`], so a
    /// verdict that is not acting (decayed, not acknowledged, suspended, below
    /// quorum) refuses its hop and ends the chain with the level before it on
    /// the wire. `sent` is the rank bitmask of the levels this request has
    /// emitted so far; a hop never writes one of them, which ends a chain two
    /// verdicts would otherwise close into a loop. At most one hop per rank
    /// member is taken however the store reads.
    fn follow_effort_chain<'a>(
        &'a self,
        planned: &mut ChatRequest,
        target: &DispatchTarget,
        table: FieldRepairTable,
        surface: DispatchSurface,
        mut sent: u8,
    ) -> Vec<RowDecision<'a>> {
        let mut hops = Vec::new();
        for _ in 0..routectl_providers::effort::VALID_EFFORT_TOKENS.len() {
            let Some(row) = table
                .lane_rows()
                .find(|row| is_ranked(row) && row.surface.present_in(planned))
                .copied()
            else {
                break;
            };
            sent |= ranked_level_bit(&row);
            let decided = self.plan_one_row(planned, target, row, surface, false, sent);
            let acted = decided.record.acted;
            hops.push(decided);
            if !acted {
                break;
            }
        }
        hops
    }
}

/// The rank bit of the level a ranked row names, or `0` for any other row.
fn ranked_level_bit(row: &FieldRepairRow) -> u8 {
    match row.surface {
        FieldSurface::Effort(level) if is_ranked(row) => level_bit(level),
        _ => 0,
    }
}

/// Whether `row` is ranked and `value` is a level in the rank bitmask `sent`
/// of the levels this request already emitted.
fn emitted_before(row: &FieldRepairRow, value: &str, sent: u8) -> bool {
    is_ranked(row) && level_bit(value) & sent != 0
}

/// The rank bit of the effort level `level`, or `0` outside the rank.
fn level_bit(level: &str) -> u8 {
    super::learn_engine::EffortRank::of_token(level).map_or(0, |rank| rank.bit())
}

/// Whether `req` already holds the pre-flight declaration a rewrite of
/// `surface` in `wire` would add: a chained effort rewrites one wire field
/// several times, and the field is declared once.
fn preflight_already_declared(
    req: &ChatRequest,
    wire: Option<IngressDialect>,
    surface: FieldSurface,
    kind: EditKind,
) -> bool {
    let pointer = wire.and_then(|wire| super::edit_declare::field_wire_pointer(surface, wire));
    pointer.is_some_and(|pointer| {
        req.routectl_internal.declared_edits.iter().any(|edit| {
            edit.site() == EditSite::Preflight && edit.kind() == kind && edit.path() == pointer
        })
    })
}
