use routectl_core::wire_check::{
    EgressKind, IngressDialect, Lane, Leg, PathClass, RejectionProvenance, WirePath,
};

use super::{
    CheckOutcome, FINDINGS_RING_CAPACITY, FieldRepairOutcome, MAX_TRACKED_VERSIONS_PER_LANE,
    WireCheckState,
};
use crate::config::{Config, WireCheckConfig};

fn config(first_n: u32, sample_one_in: u32) -> WireCheckConfig {
    let config: Config = toml::from_str(&format!(
        "version = 5\n[wire_check]\nfirst_n = {first_n}\nsample_one_in = {sample_one_in}\n"
    ))
    .expect("wire_check config parses");
    config.wire_check
}

fn anthropic_lane() -> Lane {
    Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi)
}

fn chat_lane() -> Lane {
    Lane::of(IngressDialect::Chat, EgressKind::OpenaiCompat)
}

/// A path where each all-digit segment is an array index and every other
/// segment is an object key.
fn path(segments: &[&str]) -> WirePath {
    segments.iter().fold(WirePath::root(), |path, segment| {
        match segment.parse::<usize>() {
            Ok(index) => path.index(index),
            Err(_) => path.child(segment),
        }
    })
}

/// Indices (0-based) of the calls `admit` accepted out of `calls` attempts.
fn admitted_indices(
    state: &WireCheckState,
    lane: Lane,
    version: Option<&str>,
    config: &WireCheckConfig,
    calls: usize,
) -> Vec<usize> {
    (0..calls)
        .filter(|_| state.admit(lane, version, config))
        .collect()
}

#[test]
fn sampler_admits_first_n_then_exactly_one_in_m_per_lane_and_version() {
    let state = WireCheckState::default();
    let config = config(3, 5);

    let admitted = admitted_indices(&state, anthropic_lane(), Some("2.1.0"), &config, 3 + 5 * 4);

    assert_eq!(admitted, vec![0, 1, 2, 7, 12, 17, 22]);
}

#[test]
fn each_version_and_each_lane_gets_its_own_first_n() {
    let state = WireCheckState::default();
    let config = config(2, 1000);
    for _ in 0..10 {
        state.admit(anthropic_lane(), Some("1.0.0"), &config);
    }

    let rows = [
        ("new version, same lane", anthropic_lane(), Some("1.0.1")),
        ("no version, same lane", anthropic_lane(), None),
        ("same version, other lane", chat_lane(), Some("1.0.0")),
    ];
    for (name, lane, version) in rows {
        assert_eq!(
            admitted_indices(&state, lane, version, &config, 10),
            vec![0, 1],
            "{name}"
        );
    }
}

#[test]
fn a_version_past_the_per_lane_cap_falls_to_one_in_m() {
    let state = WireCheckState::default();
    let config = config(4, 10);
    let tracked: Vec<String> = (0..MAX_TRACKED_VERSIONS_PER_LANE)
        .map(|n| format!("1.0.{n}"))
        .collect();
    for version in &tracked {
        state.admit(anthropic_lane(), Some(version), &config);
    }

    let overflow = admitted_indices(&state, anthropic_lane(), Some("9.9.9"), &config, 20);

    assert_eq!(overflow, vec![9, 19], "no first-N window past the cap");
    assert_eq!(
        admitted_indices(&state, anthropic_lane(), Some("1.0.0"), &config, 3),
        vec![0, 1, 2],
        "a version tracked before the cap keeps its own first-N window"
    );
}

#[test]
fn a_released_charge_is_given_back_to_the_cohort_that_took_it() {
    let first_n_one = config(1, 1000);
    let tracked: Vec<String> = (0..MAX_TRACKED_VERSIONS_PER_LANE)
        .map(|n| format!("1.0.{n}"))
        .collect();
    let fill = |state: &WireCheckState| {
        for version in &tracked {
            state.admit(anthropic_lane(), Some(version), &first_n_one);
        }
    };

    let tracked_state = WireCheckState::default();
    assert!(tracked_state.admit(anthropic_lane(), Some("2.0.0"), &first_n_one));
    tracked_state.release(anthropic_lane(), Some("2.0.0"));
    assert!(
        tracked_state.admit(anthropic_lane(), Some("2.0.0"), &first_n_one),
        "a tracked version gets its first-N slot back"
    );

    let overflow_state = WireCheckState::default();
    fill(&overflow_state);
    let overflow_config = config(0, 2);
    assert!(!overflow_state.admit(anthropic_lane(), Some("9.9.9"), &overflow_config));
    overflow_state.release(anthropic_lane(), Some("9.9.9"));
    assert!(
        !overflow_state.admit(anthropic_lane(), Some("9.9.9"), &overflow_config),
        "an overflow version gives its charge back to the overflow counter"
    );
    assert_eq!(
        admitted_indices(
            &overflow_state,
            anthropic_lane(),
            Some("1.0.0"),
            &first_n_one,
            1
        ),
        Vec::<usize>::new(),
        "and never to a tracked version's first-N window"
    );
}

#[test]
fn a_release_never_underflows_or_starts_tracking_a_version() {
    let state = WireCheckState::default();
    let config = config(1, 1000);

    state.release(anthropic_lane(), Some("never-admitted"));
    state.release(anthropic_lane(), None);

    assert!(
        state.admit(anthropic_lane(), Some("never-admitted"), &config),
        "a release with no matching charge changes nothing"
    );
    state.release(anthropic_lane(), Some("never-admitted"));
    state.release(anthropic_lane(), Some("never-admitted"));
    assert!(state.admit(anthropic_lane(), Some("never-admitted"), &config));
    assert!(
        !state.admit(anthropic_lane(), Some("never-admitted"), &config),
        "the count saturated at zero rather than wrapping"
    );
}

#[test]
fn first_n_zero_admits_only_every_mth_success() {
    let state = WireCheckState::default();
    let config = config(0, 4);

    let admitted = admitted_indices(&state, chat_lane(), None, &config, 12);

    assert_eq!(admitted, vec![3, 7, 11]);
}

#[test]
fn only_a_finished_untruncated_comparison_with_no_findings_is_clean() {
    let lost = path(&["metadata", "user_id"]);
    type Row<'a> = (&'a str, &'a [(&'a WirePath, PathClass)], bool, CheckOutcome);
    let rows: [Row<'_>; 4] = [
        ("no findings", &[], false, CheckOutcome::Clean),
        (
            "a finding",
            &[(&lost, PathClass::Missing)],
            false,
            CheckOutcome::Loss,
        ),
        ("truncated, no findings", &[], true, CheckOutcome::Budget),
        (
            "truncated with a finding",
            &[(&lost, PathClass::Missing)],
            true,
            CheckOutcome::Budget,
        ),
    ];
    for (name, findings, truncated, expected) in rows {
        let state = WireCheckState::default();

        let outcome = state.record_compared(
            anthropic_lane(),
            Leg::Request,
            findings.iter().copied(),
            truncated,
        );

        assert_eq!(outcome, expected, "{name}");
        let checks = state.snapshot(&WireCheckConfig::default()).checks;
        assert_eq!(checks.len(), 1, "{name}");
        assert_eq!(checks[0].outcome, expected, "{name}");
        assert_eq!(checks[0].reading.count, 1, "{name}");
    }
}

#[test]
fn an_incomplete_check_is_counted_incomplete_not_clean() {
    let state = WireCheckState::default();

    state.record_incomplete(anthropic_lane());

    let checks = state.snapshot(&WireCheckConfig::default()).checks;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].outcome, CheckOutcome::Incomplete);
    assert_eq!(checks[0].lane, anthropic_lane());
}

#[test]
fn a_loss_bumps_its_lane_and_class_counter_and_enters_the_ring_with_its_leg() {
    let state = WireCheckState::default();
    let lost = path(&["stop_sequences"]);

    state.record_compared(
        chat_lane(),
        Leg::Response,
        [(&lost, PathClass::Changed)],
        false,
    );

    let snapshot = state.snapshot(&WireCheckConfig::default());
    assert_eq!(snapshot.loss.len(), 1);
    assert_eq!(snapshot.loss[0].lane, chat_lane());
    assert_eq!(snapshot.loss[0].path_class, PathClass::Changed);
    assert_eq!(snapshot.loss[0].reading.count, 1);
    let finding = &snapshot.findings[0];
    assert_eq!(finding.leg, Leg::Response);
    assert_eq!(finding.path, lost);
    assert_eq!(finding.class, Some(PathClass::Changed));
    assert_eq!(finding.field_repair_outcome, None);
}

#[test]
fn the_ring_holds_at_most_its_capacity_keeping_the_newest() {
    let state = WireCheckState::default();
    let total = FINDINGS_RING_CAPACITY + 72;

    for n in 0..total {
        state.record_rejection(
            anthropic_lane(),
            Leg::Request,
            &path(&["tools", &n.to_string()]),
            RejectionProvenance::ClientUnchanged,
            FieldRepairOutcome::Repaired,
        );
    }

    let findings = state.snapshot(&WireCheckConfig::default()).findings;
    assert_eq!(findings.len(), FINDINGS_RING_CAPACITY);
    assert_eq!(findings[0].path, path(&["tools", "72"]), "oldest kept");
    assert_eq!(
        findings[FINDINGS_RING_CAPACITY - 1].path,
        path(&["tools", &(total - 1).to_string()]),
        "newest kept last"
    );
}

fn record_unrepaired_rejections(state: &WireCheckState, count: usize) {
    for n in 0..count {
        state.record_rejection(
            anthropic_lane(),
            Leg::Request,
            &path(&["tools", &n.to_string()]),
            RejectionProvenance::ClientUnchanged,
            FieldRepairOutcome::Unrepaired,
        );
    }
}

#[test]
fn a_flood_of_rejections_never_evicts_a_loss_finding() {
    let state = WireCheckState::default();
    let lost = path(&["stop_sequences"]);
    state.record_compared(
        chat_lane(),
        Leg::Request,
        [(&lost, PathClass::Missing)],
        false,
    );

    record_unrepaired_rejections(&state, FINDINGS_RING_CAPACITY + 72);

    let findings = state.snapshot(&WireCheckConfig::default()).findings;
    let losses: Vec<&WirePath> = findings
        .iter()
        .filter(|f| f.class.is_some())
        .map(|f| &f.path)
        .collect();
    assert_eq!(losses, [&lost]);
    assert_eq!(findings.len(), FINDINGS_RING_CAPACITY + 1);
    assert_eq!(findings[0].path, lost, "findings stay in recording order");
}

#[test]
fn a_flood_of_rejection_warn_keys_never_silences_a_new_loss() {
    let state = WireCheckState::default();
    let lost = path(&["stop_sequences"]);

    let events = routectl_testkit::capture_events(|| {
        record_unrepaired_rejections(&state, 1100);
        state.record_compared(
            chat_lane(),
            Leg::Request,
            [(&lost, PathClass::Missing)],
            false,
        );
    });

    let loss_warns: Vec<_> = wire_check_warns(&events)
        .into_iter()
        .filter(|e| e.field("path_class").is_some())
        .collect();
    assert_eq!(loss_warns.len(), 1);
    assert_eq!(loss_warns[0].field("path"), Some("/stop_sequences"));
}

#[test]
fn a_local_refusal_counts_per_lane_and_reason_sorted_by_both() {
    use routectl_core::RefusalReason;

    let state = WireCheckState::default();
    let local = Lane::of(IngressDialect::Responses, EgressKind::Local);
    let egress = Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi);

    state.record_local_refusal(local, RefusalReason::PreviousResponseId);
    state.record_local_refusal(local, RefusalReason::PreviousResponseId);
    state.record_local_refusal(local, RefusalReason::FieldType);
    state.record_local_refusal(egress, RefusalReason::ToolPairing);

    let refusals: Vec<_> = state
        .snapshot(&WireCheckConfig::default())
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
        .collect();
    assert_eq!(
        refusals,
        [
            ("anthropic->anthropic-api", "tool-pairing", 1),
            ("responses->local", "field-type", 1),
            ("responses->local", "previous-response-id", 2),
        ]
    );
}

#[test]
fn counter_window_includes_its_edge_and_excludes_past_it() {
    let reading = super::CounterReading {
        count: 1,
        last_seen_unix_secs: 1_000,
    };

    assert!(reading.seen_within(60, 1_060));
    assert!(!reading.seen_within(60, 1_061));
    assert!(
        reading.seen_within(60, 900),
        "a clock step back stays in window"
    );
}

const BODY_SENTINEL: &str = "planted body sentinel 7f3a";

fn wire_check_warns(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.target == "routectl_router::wire_check" && e.level == tracing::Level::WARN)
        .collect()
}

#[test]
fn a_repeated_loss_warns_once_while_its_counter_reaches_the_call_count() {
    let state = WireCheckState::default();
    let lost = path(&["messages", "0", BODY_SENTINEL]);
    let calls = 5;

    let events = routectl_testkit::capture_events(|| {
        for _ in 0..calls {
            state.record_compared(
                anthropic_lane(),
                Leg::Request,
                [(&lost, PathClass::Missing)],
                false,
            );
        }
    });

    let warns = wire_check_warns(&events);
    assert_eq!(warns.len(), 1);
    let warn = warns[0];
    assert_eq!(warn.field("lane"), Some("anthropic->anthropic-api"));
    assert_eq!(warn.field("leg"), Some("request"));
    assert_eq!(warn.field("path"), Some("/messages/0/*"));
    assert_eq!(warn.field("path_class"), Some("missing"));
    for (name, value) in &warn.fields {
        assert!(!value.contains(BODY_SENTINEL), "field {name} = {value}");
    }
    assert!(!warn.message.contains(BODY_SENTINEL));
    let snapshot = state.snapshot(&WireCheckConfig::default());
    assert_eq!(snapshot.loss[0].reading.count, calls);
    assert_eq!(snapshot.findings.len(), calls as usize);
}

#[test]
fn a_different_lane_path_or_class_warns_separately() {
    let state = WireCheckState::default();
    let a = path(&["temperature"]);
    let b = path(&["top_p"]);

    let events = routectl_testkit::capture_events(|| {
        for (lane, p, class) in [
            (anthropic_lane(), &a, PathClass::Missing),
            (anthropic_lane(), &a, PathClass::Missing),
            (chat_lane(), &a, PathClass::Missing),
            (anthropic_lane(), &b, PathClass::Missing),
            (anthropic_lane(), &a, PathClass::Changed),
        ] {
            state.record_compared(lane, Leg::Request, [(p, class)], false);
        }
    });

    assert_eq!(wire_check_warns(&events).len(), 4);
}

#[test]
fn only_an_unrepaired_rejection_warns_and_only_once_per_path() {
    let state = WireCheckState::default();
    let rejected = path(&["thinking"]);

    let events = routectl_testkit::capture_events(|| {
        for outcome in [
            FieldRepairOutcome::Repaired,
            FieldRepairOutcome::RepairedLossy,
            FieldRepairOutcome::Unrepaired,
            FieldRepairOutcome::Unrepaired,
        ] {
            state.record_rejection(
                anthropic_lane(),
                Leg::Request,
                &rejected,
                RejectionProvenance::RoutectlAdded,
                outcome,
            );
        }
    });

    let warns = wire_check_warns(&events);
    assert_eq!(warns.len(), 1);
    assert_eq!(warns[0].field("field_repair_outcome"), Some("unrepaired"));
    assert_eq!(warns[0].field("provenance"), Some("routectl_added"));
    assert_eq!(warns[0].field("path"), Some("/thinking"));
    assert_eq!(
        state.snapshot(&WireCheckConfig::default()).findings.len(),
        4
    );
}

#[test]
fn snapshot_reads_do_not_advance_the_sampler_or_change_the_registry() {
    let state = WireCheckState::default();
    let config = config(2, 1000);
    assert!(state.admit(anthropic_lane(), None, &config));
    state.record_incomplete(anthropic_lane());
    let before = state.snapshot(&config);

    for _ in 0..10 {
        state.snapshot(&config);
    }

    assert_eq!(state.snapshot(&config), before);
    assert!(
        state.admit(anthropic_lane(), None, &config),
        "the second success is still inside first_n"
    );
    assert!(!state.admit(anthropic_lane(), None, &config));
}

#[test]
fn closed_label_sets_are_complete_and_distinct() {
    let outcomes: Vec<&str> = CheckOutcome::ALL.iter().map(|o| o.label()).collect();
    assert_eq!(outcomes, ["clean", "loss", "budget", "incomplete"]);
    let repair_outcomes: Vec<&str> = FieldRepairOutcome::ALL.iter().map(|o| o.label()).collect();
    assert_eq!(
        repair_outcomes,
        ["repaired", "repaired_lossy", "unrepaired"]
    );
}

/// Each row is a client version that is not a safe token. It must share
/// the no-version cohort's first-N budget rather than open its own, and
/// must never be stored.
#[test]
fn an_unsafe_client_version_shares_the_no_version_cohort() {
    let oversized = "v".repeat(65);
    let rows = [
        ("space", "2.1 beta"),
        ("control byte", "2.1\u{7}"),
        ("65 bytes", oversized.as_str()),
    ];
    for (name, unsafe_version) in rows {
        let state = WireCheckState::default();
        let config = config(3, 1000);
        for _ in 0..3 {
            state.admit(anthropic_lane(), None, &config);
        }

        let admitted = admitted_indices(&state, anthropic_lane(), Some(unsafe_version), &config, 5);

        assert_eq!(
            admitted,
            Vec::<usize>::new(),
            "{name}: shares None's spent budget"
        );
        let shard = state.sampler[anthropic_lane().index()]
            .lock()
            .expect("sampler lock");
        assert!(
            shard.versions.iter().all(|(v, _)| v.is_none()),
            "{name}: the unsafe version was stored"
        );
    }
}

#[test]
fn concurrent_admits_on_two_lanes_count_exactly_per_lane() {
    let state = std::sync::Arc::new(WireCheckState::default());
    let config = config(4, 10);
    let threads = 16;
    let per_thread = 250;
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let state = std::sync::Arc::clone(&state);
            let lane = if t % 2 == 0 {
                anthropic_lane()
            } else {
                chat_lane()
            };
            std::thread::spawn(move || {
                let admitted = (0..per_thread)
                    .filter(|_| state.admit(lane, Some("1.0.0"), &config))
                    .count();
                (lane, admitted)
            })
        })
        .collect();

    let mut per_lane = std::collections::HashMap::new();
    for handle in handles {
        let (lane, admitted) = handle.join().expect("admit thread");
        *per_lane.entry(lane.label()).or_insert(0) += admitted;
    }

    let calls_per_lane = (threads / 2) * per_thread;
    let expected = 4 + (calls_per_lane - 4) / 10;
    assert_eq!(per_lane.get(anthropic_lane().label()), Some(&expected));
    assert_eq!(per_lane.get(chat_lane().label()), Some(&expected));
}
