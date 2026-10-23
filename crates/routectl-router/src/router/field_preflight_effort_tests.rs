// Pre-flight over an EFFORT row: a resident verdict that refused one level
// rewrites the request's effort to the level the verdict stored, in the
// carriers that held it and nowhere else, and follows the verdict of each
// level it writes while that verdict acts.
// An `include!`d FRAGMENT of `field_preflight_tests.rs`.

/// A ledger of persisted effort verdicts on seat `m0`'s lane, one per
/// `(refused, stored)` pair: the row of the refused level, its value column
/// carrying `shape:<stored>`.
struct EffortVerdictLedger {
    verdicts: &'static [(&'static str, &'static str)],
}

impl crate::capability_rebuild::CapabilityLedgerReader for EffortVerdictLedger {
    fn tombstone(&self) -> Option<crate::capability_rebuild::ReplayTombstone> {
        Some(crate::capability_rebuild::ReplayTombstone::new(0, 0, 0))
    }

    fn read_events(&self) -> Vec<crate::capability_rebuild::CapabilityEventRow> {
        (1..)
            .zip(self.verdicts)
            .map(|(rowid, &(refused, stored))| effort_event(rowid, refused, stored))
            .collect()
    }
}

/// One persisted effort verdict: `refused` answered with `shape:<stored>`.
fn effort_event(
    rowid: i64,
    refused: &str,
    stored: &str,
) -> crate::capability_rebuild::CapabilityEventRow {
    let capability =
        crate::field_capability::field_capability_key(&format!("output_config.effort.{refused}"))
            .expect("an effort path is a qualified path");
    crate::capability_rebuild::CapabilityEventRow::new(
        rowid,
        Instant::now(),
        "broken".to_string(),
        Some("f1".to_string()),
        "live".to_string(),
        Some("self-identifying".to_string()),
        None,
        capability,
        crate::router::probe_test_support::seat_chain_lane("m0")
            .as_lane_key()
            .to_string(),
        ANTHROPIC.to_string(),
        0,
        0,
    )
    .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION))
    .with_upstream_token(Some(format!("shape:{stored}")))
}

/// An Anthropic client's request at `effort`, as the ingress parses it: the
/// `output_config` in the extras, its effort lifted onto the canonical one,
/// and `thinking` as the client sent it (`None`: no thinking field).
fn effort_req(effort: &str, thinking: Option<serde_json::Value>) -> ChatRequest {
    let mut req = req_without_field(ALIAS);
    req.max_tokens = None;
    req.provider_extras = Some(json!({"output_config": {"effort": effort}}));
    match thinking {
        Some(thinking) => {
            req.reasoning = Some(ReasoningConfig {
                effort: Some(effort.into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            });
            req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::Sent(
                routectl_core::ThinkingObject::from_client(thinking).expect("an object"),
            );
        }
        None => {
            req.reasoning = Some(ReasoningConfig {
                effort: Some(effort.into()),
                ..ReasoningConfig::default()
            });
            req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
        }
    }
    req
}

/// The Messages body the Anthropic egress assembles for `req`.
fn messages_body(req: &ChatRequest) -> serde_json::Value {
    routectl_providers::anthropic_api::AnthropicApiProvider::new(
        routectl_providers::anthropic_api::AnthropicApiConfig::new("effort-seat", "k"),
    )
    .normalize_request(req)
    .expect("the messages body assembles")
}

/// A router holding a resident, pre-flight-eligible `xhigh -> high` verdict.
fn router_with_effort_verdict() -> Router {
    router_with_effort_verdicts(&[("xhigh", "high")])
}

/// A router holding one resident, pre-flight-eligible verdict per
/// `(refused, stored)` pair.
fn router_with_effort_verdicts(verdicts: &'static [(&'static str, &'static str)]) -> Router {
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    let summary = router.rebuild_learned_from_ledger(&EffortVerdictLedger { verdicts });
    assert_eq!(
        summary.replayed_negative,
        verdicts.len(),
        "premise: every verdict replayed"
    );
    assert_eq!(summary.degraded_value, 0, "premise: every value decoded");
    router
}

/// The one decision the planner made on the `xhigh` effort row.
fn effort_decision(records: &[FieldPreflight]) -> &FieldPreflight {
    let mut decisions = records
        .iter()
        .filter(|record| record.field_path == Some("output_config.effort.xhigh"));
    let decision = decisions
        .next()
        .expect("a decision on the xhigh effort row");
    assert!(
        decisions.next().is_none(),
        "one decision on the row: {records:?}"
    );
    decision
}

/// A resident `xhigh -> high` verdict rewrites an adaptive request's effort to
/// `high` on the wire, and the thinking object ships byte for byte as before.
///
/// Mutation check: make the `RankedRewrite` arm of `preflight_edit` refuse
/// (`FIELD_PREFLIGHT_NO_STORED_VALUE`) -> red.
#[test]
fn a_resident_effort_verdict_rewrites_the_egress_effort_and_keeps_thinking() {
    let router = router_with_effort_verdict();
    let req = effort_req("xhigh", Some(json!({"type": "adaptive"})));
    let before = messages_body(&req);

    let (planned, records) = plan_all(&router, &req, "m0");
    let after = messages_body(&planned);

    let decision = effort_decision(&records);
    assert!(decision.acted, "{decision:?}");
    assert_eq!(decision.reason, super::FIELD_PREFLIGHT_ACTION_REWRITE);
    assert_eq!(before["output_config"]["effort"], json!("xhigh"), "premise");
    assert_eq!(after["output_config"]["effort"], json!("high"));
    assert_eq!(
        serde_json::to_vec(&after["thinking"]).expect("serialize"),
        serde_json::to_vec(&before["thinking"]).expect("serialize"),
        "the thinking object is byte-identical",
    );
    assert_eq!(after["thinking"]["type"], json!("adaptive"), "premise");
}

/// An effort-only request gains neither a thinking object nor a canonical
/// reasoning it did not carry: the rewrite moves the effort alone.
///
/// Mutation check: create `reasoning` in `write_effort` -> red.
#[test]
fn a_preflight_effort_rewrite_on_an_effort_only_request_adds_no_thinking() {
    let router = router_with_effort_verdict();
    let mut req = effort_req("xhigh", None);
    req.reasoning = None;
    let before = messages_body(&req);

    let (planned, records) = plan_all(&router, &req, "m0");
    let after = messages_body(&planned);

    assert!(effort_decision(&records).acted);
    assert!(planned.reasoning.is_none(), "no reasoning is created");
    assert_eq!(
        planned.routectl_internal.anthropic_thinking,
        routectl_core::AnthropicThinking::SentNone,
    );
    assert!(
        before.get("thinking").is_none(),
        "premise: no thinking sent"
    );
    assert!(after.get("thinking").is_none(), "no thinking is created");
    assert_eq!(after["output_config"]["effort"], json!("high"));
}

/// A request at another level than the refused one carries no row the
/// verdict names, so nothing is rewritten.
#[test]
fn an_effort_verdict_leaves_a_request_at_another_level_alone() {
    let router = router_with_effort_verdict();
    let req = effort_req("medium", None);

    let (planned, records) = plan_all(&router, &req, "m0");

    assert!(records.iter().all(|record| !record.acted), "{records:?}");
    assert_eq!(planned.provider_extras, req.provider_extras);
}

/// The effort surface's wire pointer is the field each client dialect spells
/// it at, and none on a Chat client, whose effort is not on that path.
#[test]
fn the_effort_surface_points_at_each_dialects_effort_field() {
    use routectl_core::wire_check::IngressDialect;
    let surface = super::super::field_repair::FieldSurface::Effort("xhigh");
    let pointer = |wire| super::super::edit_declare::field_wire_pointer(surface, wire);

    assert_eq!(
        pointer(IngressDialect::Anthropic),
        Some("/output_config/effort")
    );
    assert_eq!(
        pointer(IngressDialect::Responses),
        Some("/reasoning/effort")
    );
    assert_eq!(pointer(IngressDialect::Chat), None);
}

/// Lapse the resident verdict of the refused effort `level` on seat `m0` into
/// a re-probe, through the registry's own expiry seam: resident, not acting.
fn lapse_effort_verdict(router: &Router, level: &str) {
    let capability =
        crate::field_capability::field_capability_key(&format!("output_config.effort.{level}"))
            .expect("an effort path is a qualified path");
    let expired = router.learned_capabilities.expire_keyed_in_generation(
        router.registry_generation(),
        crate::router::probe_test_support::seat_chain_lane("m0").as_lane_key(),
        &capability,
        ANTHROPIC,
        Instant::now(),
    );
    assert!(
        matches!(
            expired,
            crate::learned_capability::GenerationOutcome::Applied { value: true, .. }
        ),
        "premise: the {level} verdict lapsed",
    );
}

/// A pre-flight that rewrote the effort reads the row of the level it wrote
/// and keeps going while that level's verdict acts: two acting verdicts take
/// `xhigh` straight to `medium`; a lapsed second verdict stops the chain at
/// `high`; two verdicts naming each other stop at the first written level,
/// since a hop never writes a level the request already emitted.
///
/// Mutation checks: take only the first hop in `follow_effort_chain` -> red
/// on `two hops`; make `emitted_before` answer false -> red
/// on `repeat` (the two verdicts alternate up to the hop bound).
#[test]
fn a_preflight_effort_rewrite_follows_each_acting_learned_level() {
    const REWRITTEN: &str = super::FIELD_PREFLIGHT_ACTION_REWRITE;
    struct Row {
        name: &'static str,
        verdicts: &'static [(&'static str, &'static str)],
        lapsed: Option<&'static str>,
        sent: &'static str,
        decisions: &'static [(&'static str, bool, &'static str)],
    }
    let rows = [
        Row {
            name: "two hops",
            verdicts: &[("xhigh", "high"), ("high", "medium")],
            lapsed: None,
            sent: "medium",
            decisions: &[
                ("output_config.effort.xhigh", true, REWRITTEN),
                ("output_config.effort.high", true, REWRITTEN),
                (
                    "output_config.effort.medium",
                    false,
                    FIELD_PREFLIGHT_NOT_ELIGIBLE,
                ),
            ],
        },
        Row {
            name: "stopped by a lapsed verdict",
            verdicts: &[("xhigh", "high"), ("high", "medium")],
            lapsed: Some("high"),
            sent: "high",
            decisions: &[
                ("output_config.effort.xhigh", true, REWRITTEN),
                (
                    "output_config.effort.high",
                    false,
                    FIELD_PREFLIGHT_NOT_ELIGIBLE,
                ),
            ],
        },
        Row {
            name: "repeat",
            verdicts: &[("xhigh", "high"), ("high", "xhigh")],
            lapsed: None,
            sent: "high",
            decisions: &[
                ("output_config.effort.xhigh", true, REWRITTEN),
                (
                    "output_config.effort.high",
                    false,
                    super::FIELD_PREFLIGHT_VALUE_ALREADY_SENT,
                ),
            ],
        },
        Row {
            name: "one hop",
            verdicts: &[("xhigh", "high")],
            lapsed: None,
            sent: "high",
            decisions: &[
                ("output_config.effort.xhigh", true, REWRITTEN),
                (
                    "output_config.effort.high",
                    false,
                    FIELD_PREFLIGHT_NOT_ELIGIBLE,
                ),
            ],
        },
    ];

    for row in rows {
        let router = router_with_effort_verdicts(row.verdicts);
        if let Some(level) = row.lapsed {
            lapse_effort_verdict(&router, level);
        }
        let req = effort_req("xhigh", Some(json!({"type": "adaptive"})));
        let before = messages_body(&req);

        let (planned, records) = plan_all(&router, &req, "m0");
        let after = messages_body(&planned);

        let decisions: Vec<_> = records
            .iter()
            .filter(|record| {
                record
                    .field_path
                    .is_some_and(|path| path.starts_with("output_config.effort."))
            })
            .map(|record| (record.field_path.unwrap_or(""), record.acted, record.reason))
            .collect();
        assert_eq!(decisions, row.decisions, "{}", row.name);
        assert_eq!(
            after["output_config"]["effort"],
            json!(row.sent),
            "{}",
            row.name
        );
        assert_eq!(
            planned.reasoning.as_ref().and_then(|r| r.effort.as_deref()),
            Some(row.sent)
        );
        assert_eq!(
            serde_json::to_vec(&after["thinking"]).expect("serialize"),
            serde_json::to_vec(&before["thinking"]).expect("serialize"),
            "{}: the thinking object is byte-identical",
            row.name,
        );
    }
}

/// A chained effort pre-flight declares the effort pointer once, as a
/// rewrite, however many learned levels it passed through.
///
/// Mutation check: drop the `preflight_already_declared` check in `adopt_row`
/// -> red (two identical declarations).
#[test]
fn a_chained_effort_preflight_declares_the_effort_pointer_once() {
    let router = router_with_effort_verdicts(&[("xhigh", "high"), ("high", "medium")]);
    let req = with_anthropic_ingress(effort_req("xhigh", Some(json!({"type": "adaptive"}))));

    let (planned, _records) = plan_all(&router, &req, "m0");

    assert_eq!(
        preflight_declarations(&planned),
        [(
            "/output_config/effort".to_owned(),
            routectl_core::wire_check::EditKind::Rewrite
        )]
    );
    assert_eq!(
        messages_body(&planned)["output_config"]["effort"],
        json!("medium")
    );
}
