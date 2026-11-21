//! The model-problem count the Doctor tab shows, pinned against the two
//! panels it joins on one frozen daemon state: the number of distinct
//! non-null `cause_key` values among `/status/models` rows (one per cause
//! line the Models view draws), each of which is the key of an active
//! `/status/attention` item built from the same targets and credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use routectl_core::EligibilityReason;
use routectl_core::failure_class::LastOutcome;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::{
    CircuitPhase, CredentialUnusableStatus, ProviderGateStatus, UnusableCause,
};
use routectl_router::{AccountIdentities, AccountReadings, Config, RejectionTotals, StateKey};
use serde_json::Value;

use super::fixture::models_panel_of;
use crate::commands::doctor::SeatFix;
use crate::handlers::status::attention::fixture::attention_panel_of;
use crate::handlers::status::attention::{AttentionSources, DriverReads};
use crate::handlers::status::panel_fixtures::FIXTURE_NOW_MS;
use crate::handlers::status::router_view::AccountCredential;
use crate::handlers::status::usage_health::LedgerHealth;
use crate::server::seat_driver::SeatDriverHealthSnapshot;

const SEAT_ENTRY: &str = "seat";
const FOLD_ENTRY: &str = "fold";
const LONE_ENTRY: &str = "lone";
const CALM_ENTRY: &str = "calm";
const FOLD_UPSTREAMS: [&str; 4] = ["up-a", "up-b", "up-c", "up-d"];

/// A logged-out login seat, a provider entry with four models (each its own
/// upstream), a second entry with one model and a third that answers.
fn config() -> Config {
    let api = |entry: &str| {
        format!(
            "[providers.{entry}]\nformat = \"openai-chat\"\n\
             base_url = \"https://{entry}.example.invalid/v1\"\n\
             credential = \"env://COUNT_{}_KEY\"\nauth = \"bearer\"\n",
            entry.to_uppercase()
        )
    };
    let mut text = format!(
        "version = {}\n\
         [providers.{SEAT_ENTRY}]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         credential = \"oauth://anthropic#a\"\nauth = \"bearer\"\n{}{}{}\
         [models.seat-m]\nprovider = \"{SEAT_ENTRY}\"\nupstream = \"seat-up\"\n\
         [models.lone-m]\nprovider = \"{LONE_ENTRY}\"\nupstream = \"lone-up\"\n\
         [models.calm-m]\nprovider = \"{CALM_ENTRY}\"\nupstream = \"calm-up\"\n",
        routectl_router::CURRENT_CONFIG_VERSION,
        api(FOLD_ENTRY),
        api(LONE_ENTRY),
        api(CALM_ENTRY),
    );
    for upstream in FOLD_UPSTREAMS {
        text.push_str(&format!(
            "[models.fold-{upstream}]\nprovider = \"{FOLD_ENTRY}\"\nupstream = \"{upstream}\"\n"
        ));
    }
    routectl_router::parse_config(&text).expect("the count fixture config parses")
}

fn account(config: &Config, entry: &str) -> String {
    AccountIdentities::account_of_entry(config, entry).expect("every entry names an account")
}

fn closed() -> ProviderGateStatus {
    ProviderGateStatus {
        reason: None,
        rpm_available: None,
        circuit: CircuitPhase::Closed,
        half_open_probe_in_flight: false,
        circuit_open_elapsed: None,
        last_outcome: Some(LastOutcome::Ok),
        last_outcome_elapsed: Some(Duration::from_mins(1)),
        credential_park: None,
        credential_unusable: None,
    }
}

fn open_after(outcome: LastOutcome) -> ProviderGateStatus {
    ProviderGateStatus {
        reason: Some(EligibilityReason::BreakerOpen),
        circuit: CircuitPhase::Open,
        circuit_open_elapsed: Some(Duration::from_mins(5)),
        last_outcome: Some(outcome),
        last_outcome_elapsed: Some(Duration::from_mins(5)),
        ..closed()
    }
}

fn target(
    config: &Config,
    nickname: &str,
    entry: &str,
    upstream: &str,
    gate: ProviderGateStatus,
) -> RouteTargetStatus {
    RouteTargetStatus {
        state_key: nickname.to_string(),
        nickname: nickname.to_string(),
        provider_name: entry.to_string(),
        upstream: upstream.to_string(),
        seat_label: None,
        account: Some(account(config, entry)),
        login_label: None,
        learned_lane: StateKey::parse(&format!("{entry}#{upstream}")),
        gate,
    }
}

/// The seat's target, refused because its login is unusable.
fn account_blocked(config: &Config) -> Vec<RouteTargetStatus> {
    let gate = ProviderGateStatus {
        reason: Some(EligibilityReason::CredentialUnusable),
        credential_unusable: Some(CredentialUnusableStatus {
            cause: UnusableCause::NotLoggedIn,
            since_elapsed: Duration::from_mins(30),
        }),
        ..closed()
    };
    vec![RouteTargetStatus {
        seat_label: Some("a".to_string()),
        login_label: Some("a".to_string()),
        ..target(config, "seat-m", SEAT_ENTRY, "seat-up", gate)
    }]
}

/// The fold entry's four targets, all open after a server error.
fn provider_fold(config: &Config) -> Vec<RouteTargetStatus> {
    FOLD_UPSTREAMS
        .iter()
        .map(|upstream| {
            target(
                config,
                &format!("fold-{upstream}"),
                FOLD_ENTRY,
                upstream,
                open_after(LastOutcome::Http5xx),
            )
        })
        .collect()
}

/// The lone entry's one target, open after a transport error.
fn single_open_route(config: &Config) -> Vec<RouteTargetStatus> {
    vec![target(
        config,
        "lone-m",
        LONE_ENTRY,
        "lone-up",
        open_after(LastOutcome::TransportError),
    )]
}

fn calm(config: &Config) -> Vec<RouteTargetStatus> {
    vec![target(config, "calm-m", CALM_ENTRY, "calm-up", closed())]
}

/// The seat logged out, every API-key entry usable.
fn credentials(config: &Config) -> BTreeMap<String, AccountCredential> {
    let seat = AccountCredential::unusable(
        UnusableCause::NotLoggedIn,
        FIXTURE_NOW_MS - 1_800_000,
        &SeatFix::of(config, SEAT_ENTRY),
    );
    let mut out = BTreeMap::from([(account(config, SEAT_ENTRY), seat)]);
    for entry in [FOLD_ENTRY, LONE_ENTRY, CALM_ENTRY] {
        out.insert(account(config, entry), AccountCredential::usable());
    }
    out
}

const fn running_driver() -> SeatDriverHealthSnapshot {
    SeatDriverHealthSnapshot {
        running: true,
        last_sweep_ms: Some(FIXTURE_NOW_MS - 60_000),
        last_good_sweep_ms: Some(FIXTURE_NOW_MS - 60_000),
        sweeps: 10,
        transient: 0,
        definitive: 0,
    }
}

const fn no_readings() -> AccountReadings {
    AccountReadings {
        accounts: Vec::new(),
        rejections: RejectionTotals {
            invalid_utilization: 0,
            expired_reset: 0,
            implausible_reset: 0,
            overflow: 0,
            contradictory: 0,
            unknown_length: 0,
            invalid_started: 0,
            malformed_body: 0,
        },
        refused_by_admission: 0,
        corrections_confirmed: 0,
    }
}

/// The serialized models and attention panels of ONE frozen state: both
/// built from the same targets and credentials at the same instant, the
/// attention list with nothing else to report.
fn panels(config: &Config, targets: &[RouteTargetStatus]) -> (Value, Value) {
    let credentials = credentials(config);
    let models = models_panel_of(
        config,
        targets.to_vec(),
        credentials.clone(),
        FIXTURE_NOW_MS,
    );
    let identities = AccountIdentities::from_config(config);
    let fixes = identities
        .iter()
        .map(|(account, identity)| {
            let fix = SeatFix::of(config, &identity.provider_entry);
            (account.to_string(), fix)
        })
        .collect();
    let attention = attention_panel_of(AttentionSources {
        now_ms: FIXTURE_NOW_MS,
        pools: Vec::new(),
        targets: targets.to_vec(),
        identities,
        credentials,
        fixes,
        readings: no_readings(),
        poll: None,
        refresh_failures: BTreeMap::new(),
        drivers: DriverReads {
            refresh: running_driver(),
            poll: running_driver(),
            uptime: Duration::from_hours(1),
        },
        managed_accounts: 0,
        record_loss: LedgerHealth::default(),
        doctor_findings: Some(Vec::new()),
    });
    (
        serde_json::to_value(models).expect("the models panel serializes"),
        serde_json::to_value(attention).expect("the attention panel serializes"),
    )
}

fn rows(models: &Value) -> &Vec<Value> {
    models["data"]["rows"].as_array().expect("models rows")
}

/// The model-problem count: distinct non-null `cause_key` values among the
/// models rows, which is the number of cause lines the Models view draws.
fn model_problem_count(models: &Value) -> usize {
    model_problem_keys(models).len()
}

fn model_problem_keys(models: &Value) -> BTreeSet<String> {
    rows(models)
        .iter()
        .filter_map(|row| row["cause_key"].as_str().map(str::to_string))
        .collect()
}

/// Active attention items by key, with their kind.
fn active_items(attention: &Value) -> BTreeMap<String, String> {
    attention["data"]["items"]
        .as_array()
        .expect("attention items")
        .iter()
        .filter(|item| item["state"] == "active")
        .map(|item| {
            (
                item["key"].as_str().expect("an item key").to_string(),
                item["kind"].as_str().expect("an item kind").to_string(),
            )
        })
        .collect()
}

/// The models cause keys no active attention item carries, by name.
fn unmatched_cause_keys(models: &Value, attention: &Value) -> Vec<String> {
    let active = active_items(attention);
    model_problem_keys(models)
        .into_iter()
        .filter(|key| !active.contains_key(key))
        .collect()
}

fn rows_with_a_cause(models: &Value) -> usize {
    rows(models)
        .iter()
        .filter(|row| row["cause_key"].is_string())
        .count()
}

/// A row's name, its targets, how many lanes carry a cause, the count, and
/// the item kinds its causes join.
type CountCase = (
    &'static str,
    Vec<RouteTargetStatus>,
    usize,
    usize,
    &'static [&'static str],
);

/// On one frozen state the count is the number of distinct causes, however
/// many lanes each covers, and every counted key is an active attention
/// item's key of the kind that cause raises. The calm lane adds a row and no
/// cause. Each row's lanes-with-a-cause count is the premise that it can
/// tell lanes from causes.
///
/// Mutation check: count rows with a cause in model_problem_count -> red
/// on the provider-fold row (4 lanes, 1 cause).
#[test]
fn the_model_problem_count_is_the_distinct_cause_keys_each_an_active_item() {
    let config = config();
    let all: Vec<RouteTargetStatus> = [
        account_blocked(&config),
        provider_fold(&config),
        single_open_route(&config),
        calm(&config),
    ]
    .concat();
    let cases: [CountCase; 4] = [
        (
            "account blocked",
            account_blocked(&config),
            1,
            1,
            &["account_unusable"],
        ),
        (
            "provider fold",
            provider_fold(&config),
            4,
            1,
            &["provider_failing"],
        ),
        (
            "single open route",
            single_open_route(&config),
            1,
            1,
            &["route_circuit_open"],
        ),
        (
            "all three beside a calm lane",
            all,
            6,
            3,
            &["account_unusable", "provider_failing", "route_circuit_open"],
        ),
    ];
    for (name, targets, lanes_with_cause, count, kinds) in cases {
        let (models, attention) = panels(&config, &targets);

        assert_eq!(
            rows_with_a_cause(&models),
            lanes_with_cause,
            "{name}: premise"
        );
        assert_eq!(model_problem_count(&models), count, "{name}: {models:#}");
        assert_eq!(
            unmatched_cause_keys(&models, &attention),
            Vec::<String>::new(),
            "{name}: cause keys no active item carries"
        );
        let active = active_items(&attention);
        let matched: BTreeSet<&str> = model_problem_keys(&models)
            .iter()
            .map(|key| active[key].as_str())
            .collect();
        assert_eq!(matched, kinds.iter().copied().collect(), "{name}");
    }
}

/// A models cause key that names no attention item (the two builders read
/// a moment apart) is still counted, since Models draws a cause line for it,
/// and the join reports it by name.
#[test]
fn a_cause_key_no_attention_item_carries_is_counted_and_named() {
    let config = config();
    let targets = [
        account_blocked(&config),
        provider_fold(&config),
        single_open_route(&config),
        calm(&config),
    ]
    .concat();
    let (mut models, attention) = panels(&config, &targets);
    let calm_row = models["data"]["rows"]
        .as_array_mut()
        .expect("models rows")
        .iter_mut()
        .find(|row| row["lane"] == "calm#calm-up")
        .expect("the calm lane has a row");
    assert!(calm_row["cause_key"].is_null(), "premise: {calm_row}");
    calm_row["cause_key"] = Value::from("route_circuit_open:ghost");

    assert_eq!(model_problem_count(&models), 4);
    assert_eq!(
        unmatched_cause_keys(&models, &attention),
        ["route_circuit_open:ghost"]
    );
}
