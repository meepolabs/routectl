//! The capability matrix keyed by learned lane: rows, nicknames, provider
//! kind, per-cell layer / action / timestamps, the replay tally, and the
//! typed read outcome that keeps an unreadable slice from rendering as an
//! empty registry.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_router::{
    CATALOG_VERSION, CapabilityMatrixPanel, LearnedRegistryEntry, MatrixAvailability, MatrixCell,
    MatrixLane, ModelEntry, PoolEntry, ProviderEntry,
};
use routectl_usage::{CapabilityEvent, insert_capability_event, open};
use rusqlite::params;
use tempfile::TempDir;

use super::gather::{build_capability_inputs, gather_capability_matrix};
use super::matrix::build_capability_matrix_panel;
use super::*;
use crate::commands::doctor_panels::render_capability_matrix_panel;

const OPENAI_COMPAT: &str = "openai-compat";

/// Two nicknames on one upstream of provider `p` (they share lane `p#up`),
/// a third nickname on another upstream, and a pooled model over members
/// `m1` / `m2` (one lane per member).
fn lane_config() -> Config {
    let mut config = Config::default();
    for name in ["p", "m1", "m2"] {
        config.providers.insert(
            name.to_string(),
            ProviderEntry::openai_compat("https://example.invalid", "env://KEY"),
        );
    }
    config.pools.insert(
        "pool".to_string(),
        PoolEntry::new(vec!["m1".to_string(), "m2".to_string()]),
    );
    config
        .models
        .insert("alpha".to_string(), ModelEntry::new("p", "up"));
    config
        .models
        .insert("beta".to_string(), ModelEntry::new("p", "up"));
    config
        .models
        .insert("gamma".to_string(), ModelEntry::new("p", "other"));
    config
        .models
        .insert("pooled".to_string(), ModelEntry::new("pool", "seat-up"));
    config
}

fn context(config: Config, source: CapabilityMatrixSource) -> DoctorContext {
    let capability = build_capability_inputs(
        &config,
        None,
        Some(routectl_router::CatalogOverlay::default()),
    );
    DoctorContext {
        config,
        raw_config: None,
        config_load_error: None,
        probes: Vec::new(),
        seats: Vec::new(),
        auth_store_error: None,
        secret_checks: Vec::new(),
        orphan_secrets: Vec::new(),
        orphan_seats: Vec::new(),
        probe_results: Vec::new(),
        would_trim: None,
        now_unix: 1_000,
        binary_version: "test",
        capability,
        capability_matrix: source,
        freshness: FreshnessInputs {
            catalog_version: CATALOG_VERSION,
            snapshot_date: routectl_router::CATALOG_SNAPSHOT_DATE,
            overlay_verified_at: None,
            staleness_hint_days: 14,
            today_epoch_day: 20_000,
            last_import: None,
            import_result: None,
        },
        pricing: None,
        knobs: None,
    }
}

fn negative(
    lane: &str,
    cap: &str,
    phase: FailurePhase,
    at: Instant,
    expires: Instant,
) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        state_key: lane.to_string(),
        feature_key: cap.to_string(),
        verdict: Verdict::LearnedBroken(phase),
        signal_tier: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: at,
        last_seen: at,
        expires_at: expires,
        evidence_class: None,
        phase,
        source: EvidenceSource::Live,
    }
}

fn ago(now: Instant, by: Duration) -> Instant {
    now.checked_sub(by)
        .expect("the monotonic clock reaches back this far")
}

fn available(
    entries: Vec<LearnedRegistryEntry>,
    now: Instant,
    now_ms: i64,
) -> CapabilityMatrixSource {
    CapabilityMatrixSource::Available {
        entries,
        now,
        now_ms,
        replay: MatrixReplaySummary::default(),
    }
}

fn lane<'a>(panel: &'a CapabilityMatrixPanel, key: &str) -> &'a MatrixLane {
    panel
        .lanes
        .iter()
        .find(|l| l.lane == key)
        .unwrap_or_else(|| {
            panic!(
                "lane {key} missing from {:?}",
                panel.lanes.iter().map(|l| &l.lane).collect::<Vec<_>>()
            )
        })
}

fn cell<'a>(panel: &'a CapabilityMatrixPanel, key: &str, cap: &str) -> &'a MatrixCell {
    let ci = panel
        .columns
        .iter()
        .position(|c| c == cap)
        .unwrap_or_else(|| panic!("column {cap} missing"));
    &lane(panel, key).cells[ci]
}

#[test]
fn rows_are_lanes_carrying_the_nicknames_that_map_to_them() {
    let panel = build_capability_matrix_panel(&context(
        lane_config(),
        available(Vec::new(), Instant::now(), 0),
    ));

    let keys: Vec<&str> = panel.lanes.iter().map(|l| l.lane.as_str()).collect();
    assert_eq!(keys, ["m1#seat-up", "m2#seat-up", "p#other", "p#up"]);
    assert_eq!(lane(&panel, "p#up").nicknames, ["alpha", "beta"]);
    assert_eq!(lane(&panel, "p#other").nicknames, ["gamma"]);
    assert_eq!(lane(&panel, "m1#seat-up").nicknames, ["pooled"]);
    assert_eq!(lane(&panel, "m2#seat-up").nicknames, ["pooled"]);
    for l in &panel.lanes {
        assert_eq!(l.provider_kind, OPENAI_COMPAT, "lane {}", l.lane);
        assert!(l.routed, "a configured lane is routed: {}", l.lane);
    }
}

#[test]
fn a_lane_keyed_entry_lands_on_its_lane_row() {
    // The regression: a nickname-keyed join never matched a lane-keyed entry,
    // so every learned cell rendered unknown on a configured row and the
    // entry surfaced again as an unrouted ghost.
    let now = Instant::now();
    let entry = negative(
        "p#up",
        "web_search",
        FailurePhase::F2,
        now,
        now + Duration::from_hours(1),
    );
    let panel =
        build_capability_matrix_panel(&context(lane_config(), available(vec![entry], now, 0)));

    let learned = cell(&panel, "p#up", "web_search");
    assert_eq!(learned.verdict, "broken");
    assert_eq!(learned.layer, Some("learned"));
    assert!(
        panel.lanes.iter().all(|l| l.routed),
        "a learned entry on a configured lane must not also surface as an unrouted row"
    );
}

#[test]
fn an_entry_on_a_lane_no_model_maps_is_unrouted_with_its_kind() {
    let now = Instant::now();
    let entry = negative("m1#retired", "web_search", FailurePhase::F2, now, now);
    let legacy = negative("old-nickname", "web_search", FailurePhase::F2, now, now);
    let panel = build_capability_matrix_panel(&context(
        lane_config(),
        available(vec![entry, legacy], now, 0),
    ));

    let retired = lane(&panel, "m1#retired");
    assert!(!retired.routed);
    assert!(retired.nicknames.is_empty());
    assert_eq!(
        retired.provider_kind, OPENAI_COMPAT,
        "the provider entry is still configured"
    );
    let legacy = lane(&panel, "old-nickname");
    assert!(!legacy.routed);
    assert_eq!(
        legacy.provider_kind, "",
        "a legacy key names no provider entry"
    );
}

#[test]
fn a_learned_cell_carries_action_and_epoch_timestamps() {
    let now = Instant::now();
    let now_ms = 1_800_000_000_000;
    let first = ago(now, Duration::from_mins(1));
    let mut entry = negative(
        "p#up",
        "web_search",
        FailurePhase::F1,
        ago(now, Duration::from_secs(10)),
        now + Duration::from_secs(100),
    );
    entry.first_seen = first;
    let panel =
        build_capability_matrix_panel(&context(lane_config(), available(vec![entry], now, now_ms)));

    let c = cell(&panel, "p#up", "web_search");
    assert_eq!(
        c.action, "route_away",
        "web_search is essential, so an F1 negative routes away"
    );
    assert_eq!(c.first_seen_ms, Some(now_ms - 60_000));
    assert_eq!(c.last_seen_ms, Some(now_ms - 10_000));
    assert_eq!(c.expires_at_ms, Some(now_ms + 100_000));
    assert_eq!(c.age_ms, Some(10_000));
}

#[test]
fn a_lapsed_negative_reprobes_and_a_positive_has_no_expiry() {
    let now = Instant::now();
    let lapsed = negative(
        "p#up",
        "web_search",
        FailurePhase::F1,
        now,
        ago(now, Duration::from_secs(1)),
    );
    let mut positive = negative("p#up", "thinking", FailurePhase::F3, now, now);
    positive.verdict = Verdict::VerifiedWorking;
    let panel = build_capability_matrix_panel(&context(
        lane_config(),
        available(vec![lapsed, positive], now, 0),
    ));

    assert_eq!(cell(&panel, "p#up", "web_search").action, "reprobe");
    let verified = cell(&panel, "p#up", "thinking");
    assert_eq!(verified.action, "allow");
    assert_eq!(verified.expires_at_ms, None, "a positive never decays");
    assert!(verified.first_seen_ms.is_some());
}

#[test]
fn a_droppable_f1_negative_strips_unless_essential() {
    let now = Instant::now();
    let entry = || {
        negative(
            "p#up",
            "reasoning_replay",
            FailurePhase::F1,
            now,
            now + Duration::from_mins(1),
        )
    };

    let panel =
        build_capability_matrix_panel(&context(lane_config(), available(vec![entry()], now, 0)));
    assert_eq!(cell(&panel, "p#up", "reasoning_replay").action, "strip");

    let mut essential = lane_config();
    essential.capability.essential = vec!["reasoning_replay".to_string()];
    let panel =
        build_capability_matrix_panel(&context(essential, available(vec![entry()], now, 0)));
    assert_eq!(
        cell(&panel, "p#up", "reasoning_replay").action,
        "route_away"
    );
}

#[test]
fn a_model_scoped_override_resolves_through_the_lane_nicknames() {
    let mut config = lane_config();
    config.capability.overrides.insert(
        "p:beta".to_string(),
        toml::from_str("unsupported = [\"web_search\"]").expect("override entry parses"),
    );
    let panel =
        build_capability_matrix_panel(&context(config, available(Vec::new(), Instant::now(), 0)));

    let overridden = cell(&panel, "p#up", "web_search");
    assert_eq!(overridden.verdict, "forced_unsupported");
    assert_eq!(overridden.layer, Some("override"));
    assert_eq!(
        overridden.action, "mixed",
        "only beta carries the override, so alpha and beta disagree"
    );
    let actions: Vec<(&str, &str)> = overridden
        .nickname_actions
        .iter()
        .map(|n| (n.nickname.as_str(), n.action))
        .collect();
    assert_eq!(actions, [("alpha", "none"), ("beta", "drop")]);
    assert_eq!(cell(&panel, "p#other", "web_search").verdict, "unknown");
}

#[test]
fn nicknames_on_one_lane_with_different_overrides_render_a_mixed_cell() {
    // alpha force-supports web_search while beta routes it away: one lane,
    // two actions, so no single action describes the cell.
    let mut config = lane_config();
    config.capability.overrides.insert(
        "p:alpha".to_string(),
        toml::from_str("force_supported = [\"web_search\"]").expect("override entry parses"),
    );
    config.capability.overrides.insert(
        "p:beta".to_string(),
        toml::from_str("unsupported = [\"web_search\"]").expect("override entry parses"),
    );
    let panel =
        build_capability_matrix_panel(&context(config, available(Vec::new(), Instant::now(), 0)));

    let mixed = cell(&panel, "p#up", "web_search");
    assert_eq!(mixed.action, "mixed");
    let actions: Vec<(&str, &str)> = mixed
        .nickname_actions
        .iter()
        .map(|n| (n.nickname.as_str(), n.action))
        .collect();
    assert_eq!(actions, [("alpha", "allow"), ("beta", "drop")]);

    let json = serde_json::to_value(&panel).expect("panel serializes");
    let lane = json["lanes"]
        .as_array()
        .and_then(|lanes| lanes.iter().find(|l| l["lane"] == "p#up"))
        .expect("p#up lane in json");
    let column = panel
        .columns
        .iter()
        .position(|c| c == "web_search")
        .expect("web_search column");
    assert_eq!(lane["cells"][column]["action"], "mixed");
    assert_eq!(
        lane["cells"][column]["nickname_actions"][1],
        serde_json::json!({ "nickname": "beta", "action": "drop" })
    );
    let human = render_capability_matrix_panel(&panel);
    assert!(
        human.contains("mixed(alpha=allow,beta=drop)"),
        "the human grid names each nickname's action: {human}"
    );
}

#[test]
fn nicknames_that_agree_on_a_lane_carry_no_per_nickname_actions() {
    let mut config = lane_config();
    for nickname in ["alpha", "beta"] {
        config.capability.overrides.insert(
            format!("p:{nickname}"),
            toml::from_str("unsupported = [\"web_search\"]").expect("override entry parses"),
        );
    }
    let panel =
        build_capability_matrix_panel(&context(config, available(Vec::new(), Instant::now(), 0)));

    let agreed = cell(&panel, "p#up", "web_search");
    assert_eq!(agreed.action, "drop");
    assert!(agreed.nickname_actions.is_empty());
}

#[test]
fn the_human_lane_label_is_the_exact_purge_argument() {
    let now = Instant::now();
    let entry = negative(
        "p#up",
        "web_search",
        FailurePhase::F2,
        now,
        now + Duration::from_mins(1),
    );
    let panel =
        build_capability_matrix_panel(&context(lane_config(), available(vec![entry], now, 0)));
    let human = render_capability_matrix_panel(&panel);

    let row = human
        .lines()
        .find(|line| line.trim_start().starts_with("p#up "))
        .unwrap_or_else(|| panic!("no p#up row: {human}"));
    let label = row.split_whitespace().next().expect("lane column");
    assert_eq!(
        routectl_router::StateKey::parse(label).map(|k| k.as_lane_key().to_string()),
        Some("p#up".to_string()),
        "the printed lane parses as the lane `capability purge` accepts"
    );
    assert!(
        row.contains("alpha,beta"),
        "the row names its nicknames: {row}"
    );
    assert!(row.contains(OPENAI_COMPAT), "the row names its kind: {row}");
    assert!(
        row.contains("broken[live]->route_away"),
        "the cell names its action: {row}"
    );
}

// ---------------------------------------------------------------------------
// The typed read outcome and the replay tally, through a real ledger.
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits i64")
}

fn ledger_config(db: &std::path::Path) -> Config {
    let mut config = lane_config();
    config.usage.db_path = db.to_path_buf();
    config
}

fn event(lane: &str, kind: Option<&str>, vocab: Option<i64>) -> CapabilityEvent {
    CapabilityEvent {
        ts: now_ms(),
        lane_key: lane.to_string(),
        capability: "web_search".to_string(),
        verdict: "broken".to_string(),
        phase: "f1".to_string(),
        source: "live".to_string(),
        tier: "self-identifying".to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: i64::from(CATALOG_VERSION),
        overlay_revision: 0,
        provider_kind: kind.map(str::to_string),
        vocab_version: vocab,
    }
}

/// A ledger whose boundary classifies as a matching replay, holding the
/// given post-boundary events.
fn seeded_ledger(events: &[CapabilityEvent]) -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("usage.db");
    let db = open(&path).expect("open ledger");
    insert_capability_event(
        db.conn(),
        &CapabilityEvent::tombstone(now_ms(), i64::from(CATALOG_VERSION), 0),
    )
    .expect("tombstone");
    for e in events {
        insert_capability_event(db.conn(), e).expect("event");
    }
    drop(db);
    (tmp, path)
}

#[test]
fn a_failed_slice_read_renders_unavailable_while_a_clean_empty_slice_renders_empty() {
    // Positive control: a matched boundary with ZERO post-boundary rows is the
    // honest empty -- the same fixture shape, minus the poisoned row.
    let (_clean_dir, clean) = seeded_ledger(&[]);
    let clean_source = gather_capability_matrix(&ledger_config(&clean), false, 0);
    let clean_panel = build_capability_matrix_panel(&context(ledger_config(&clean), clean_source));
    assert_eq!(clean_panel.availability, MatrixAvailability::Empty);
    assert!(
        clean_panel.replay.is_some(),
        "an empty that was read still carries its tally"
    );

    // A matched boundary whose post-boundary slice cannot be decoded: the
    // boundary classifies, the slice read fails.
    let (_dir, poisoned) = seeded_ledger(&[]);
    rusqlite::Connection::open(&poisoned)
        .expect("reopen ledger")
        .execute(
            "INSERT INTO capability_events (ts, verdict) VALUES (?1, 'broken')",
            params!["not-a-timestamp"],
        )
        .expect("plant undecodable row");

    let source = gather_capability_matrix(&ledger_config(&poisoned), false, 0);
    let panel = build_capability_matrix_panel(&context(ledger_config(&poisoned), source));
    assert_eq!(
        panel.availability,
        MatrixAvailability::Unavailable {
            code: "query_failed"
        },
        "a slice that could not be read is unavailable, never an empty registry"
    );
    assert!(
        panel.replay.is_none(),
        "no replay ran, so no tally is reported"
    );
    let human = render_capability_matrix_panel(&panel);
    assert!(
        human.contains("learned registry unavailable (query_failed)")
            && !human.contains("learned registry empty"),
        "the human render names the failure, not an empty registry: {human}"
    );
}

#[test]
fn the_replay_tally_counts_replayed_and_each_skip_reason() {
    let current = Some(routectl_router::CURRENT_VOCAB_VERSION);
    let (_dir, path) = seeded_ledger(&[
        event("p#up", Some(OPENAI_COMPAT), current),
        event("m1#seat-up", Some(OPENAI_COMPAT), current),
        event("p#up", Some("anthropic-api"), current),
        event("gone#up", Some(OPENAI_COMPAT), current),
        event("p#up", Some(OPENAI_COMPAT), Some(i64::MAX)),
    ]);

    let source = gather_capability_matrix(&ledger_config(&path), false, 0);
    let panel = build_capability_matrix_panel(&context(ledger_config(&path), source));

    assert_eq!(panel.availability, MatrixAvailability::Available);
    let replay = panel.replay.expect("the replay ran");
    assert_eq!(replay.loaded_rows, 5);
    assert_eq!(replay.replayed, 2);
    assert_eq!(replay.skipped_owner, 2, "a kind change and a removed entry");
    assert_eq!(replay.skipped_vocab, 1, "an unknown vocabulary version");
    assert_eq!(cell(&panel, "p#up", "web_search").verdict, "broken");
    assert_eq!(cell(&panel, "m1#seat-up", "web_search").verdict, "broken");

    let human = render_capability_matrix_panel(&panel);
    assert!(
        human.contains("replay: 5 rows read, 2 replayed; skipped: vocab=1 owner=2"),
        "the human render carries the tally: {human}"
    );
}
