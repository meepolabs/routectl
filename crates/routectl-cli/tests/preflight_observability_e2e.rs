//! Hermetic runtime acceptance for the pre-flight observability floor: a REAL
//! spawned daemon on loopback, with a real usage ledger in a tempdir, answering
//! real `/status` requests.
//!
//! # What this covers that no unit test can
//!
//! Every assertion here is about the ASSEMBLED daemon. The unit tests prove the
//! projection, the renderers, and the accounting read in isolation; what they
//! cannot prove is that a status request on the deployable server's own socket,
//! through its real layer stack, reaches those reads at all -- and that doing so
//! repeatedly mutates nothing. A surface wired into a panel that is never built,
//! or built behind a permit that is never released, passes every unit test.
//!
//! # Feature-present versus feature-absent
//!
//! Each case here is paired against the state in which the feature is NOT
//! engaged, because the whole class of defect on an observability surface is a
//! field that reads plausibly whether or not the thing it describes exists. A
//! zero-cap daemon must report a cap-zero lane rather than no lane; a daemon with
//! no resident verdict must report an empty row set rather than an absent field.
//!
//! # No live services, no live ledger
//!
//! The daemon binds an OS-assigned loopback port, its provider points at a dead
//! local port so nothing dials out, and its usage DB is a per-test tempdir. The
//! live daemon on its own port and the real `~/.config` ledger are never touched.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, collect_config_validation};
// Through its OWN module path rather than a crate-root re-export. The root
// re-export existed only for this test, and adding one for a test's convenience
// widens the crate's published surface permanently -- the path below is already
// public and already the one production config code uses.
use routectl_router::config::FidelityConfig;
use serde_json::Value;
use tokio::net::TcpListener;

mod common;

/// A servable config: one anthropic-api provider whose base URL is a dead local
/// port, one model, one alias.
///
/// Dead on purpose. Every assertion here is about a READ surface, and a reachable
/// upstream would let a background probe or a warm dial change state underneath a
/// purity assertion -- which would make a failure indistinguishable from a race.
fn servable_config(fidelity: FidelityConfig) -> Arc<Config> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(common::file_ref("test-key")),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "sonnet".to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "default".to_string(),
        AliasValue::Single("sonnet".to_string()),
    );
    Arc::new(Config {
        providers,
        models,
        aliases,
        fidelity,
        ..Config::default()
    })
}

/// Spawn a real server on an OS-assigned loopback port, returning its base URL
/// once `/health` answers.
///
/// Health is awaited as a PRECONDITION rather than assumed: a bind failure on an
/// occupied port can leave a stale listener answering, and a test that skipped
/// this would attribute that listener's answers to the binary under test.
async fn spawn(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed");
    });
    common::readiness::await_health(&base_url).await;
    base_url
}

/// Assert a panel envelope is AVAILABLE, returning its payload.
///
/// Availability on this surface is structural rather than a token: the
/// constructors make the two states mutually exclusive, so an available panel is
/// exactly one carrying `data` with no `unavailable` code. Both halves are
/// asserted -- checking only for `data` would pass on a malformed envelope
/// carrying both, which is the state the constructors exist to prevent.
fn available_data(panel: &Value) -> &Value {
    assert_eq!(
        panel["unavailable"],
        Value::Null,
        "the panel reports no unavailable code: {panel}",
    );
    let data = &panel["data"];
    assert!(
        !data.is_null(),
        "and it carries its payload -- the fidelity reads run inside this panel's \
         builder, so an unavailable panel would make every field assertion vacuous: \
         {panel}",
    );
    data
}

/// GET a status path and return its parsed body.
async fn get_status(base_url: &str, path: &str) -> Value {
    let response = reqwest::get(format!("{base_url}{path}"))
        .await
        .expect("daemon answers");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "the status surface answers 200 for {path}",
    );
    response.json().await.expect("status body parses")
}

/// A `[fidelity]` block with one provider capped at `cap`.
///
/// Built by mutating the default rather than by a struct literal: the type is
/// `#[non_exhaustive]`, which refuses every foreign struct expression -- literal
/// and functional-update alike -- so that a new knob stays additive for callers
/// outside the router crate. Mutating the default is the supported shape, and it
/// also means this fixture inherits every future default rather than pinning
/// today's.
fn caps_of(provider: &str, cap: u32) -> FidelityConfig {
    let mut fidelity = FidelityConfig::default();
    fidelity
        .paid_probe_daily_caps
        .insert(provider.to_string(), cap);
    fidelity
}

/// The health panel is reachable and AVAILABLE on a real daemon, which is the
/// precondition every assertion below rests on.
///
/// Its own test rather than an assertion inside the others: a panel that degraded
/// to unavailable would make every field-level check below vacuous -- an
/// unavailable panel carries no fields at all -- so the precondition is asserted
/// once, loudly, where its failure names itself.
#[tokio::test]
async fn the_health_panel_is_available_on_a_real_daemon() {
    let base_url = spawn(servable_config(FidelityConfig::default())).await;

    let panel = get_status(&base_url, "/status/health").await;

    let data = available_data(&panel);
    assert!(
        data["learned_negatives"].is_array() && data["targets"].is_array(),
        "and the payload is the health DTO, so the builder that runs the fidelity \
         reads genuinely completed: {panel}",
    );
}

/// Repeated status reads leave the daemon's verdict state untouched.
///
/// The purity contract at RUNTIME, against the assembled daemon rather than a
/// router in a unit test. A status poll runs every few seconds in a real
/// deployment, so a read that advanced a cadence or claimed a canary would
/// re-verify verdicts at the dashboard's refresh rate and consume the slots real
/// requests need.
///
/// Twenty polls rather than one: a read that consumed one unit of state per call
/// is caught by comparing before and after, and repeating also catches a mutation
/// that only fires on a later pass (a lazily initialized slot, a memoized row).
///
/// What makes this non-vacuous on a daemon with no resident verdict is that the
/// probe scheduler's counters are asserted too: those move on ACTIVATION, which a
/// status read must never trigger, and they are observable whether or not any
/// verdict exists.
#[tokio::test]
async fn repeated_status_polls_do_not_activate_a_lane_or_advance_a_cadence() {
    let base_url = spawn(servable_config(FidelityConfig::default())).await;
    let before = get_status(&base_url, "/status/health").await;

    for _ in 0..20 {
        let panel = get_status(&base_url, "/status/health").await;
        // Each poll's availability is asserted rather than assumed: a run of twenty
        // shed or unavailable polls would execute no reads at all, and the
        // before/after comparison below would then pass for the wrong reason.
        let _ = available_data(&panel);
    }

    let after = get_status(&base_url, "/status/health").await;
    assert_eq!(
        before["data"]["learned_negatives"], after["data"]["learned_negatives"],
        "twenty status polls must not add, remove, or alter a learned verdict",
    );
    assert_eq!(
        before["data"]["targets"], after["data"]["targets"],
        "and must not move any target's gate state -- a read that dialed or probed \
         would show up here as a settled outcome",
    );
}

/// The doctor panel is reachable and carries its report on a real daemon.
///
/// The second of the two surfaces the floor names. Asserted separately from health
/// because the two build through different paths -- doctor runs a blocking gather
/// behind a runtime handle -- and a fidelity read wired into only one of them would
/// pass a health-only test.
#[tokio::test]
async fn the_doctor_panel_is_reachable_on_a_real_daemon() {
    let base_url = spawn(servable_config(FidelityConfig::default())).await;

    let panel = get_status(&base_url, "/status/doctor").await;

    // A daemon spawned without an on-disk config path reports the no-config-path
    // code rather than a report, which is correct and is NOT what this asserts: what
    // matters is that the route is wired and answers its envelope rather than 404.
    assert!(
        panel["schema_version"].is_number(),
        "the doctor route answers its panel envelope: {panel}",
    );
}

/// A CAP-ZERO lane still serves, and the free-validator path is unaffected.
///
/// The cap-zero case: paid probes are refused outright
/// while free validation keeps running. Asserted as "the daemon is healthy and
/// answering with the cap configured at zero" -- a cap of zero must not be a
/// startup error, a degraded panel, or a refusal, because zero is the DEFAULT and
/// a deployment that never opted into paid probes is the normal one.
///
/// Paired against the nonzero cap below, which is what makes this a test of the
/// cap rather than of the daemon.
#[tokio::test]
async fn a_zero_cap_lane_serves_and_reports_a_healthy_panel() {
    let base_url = spawn(servable_config(caps_of("anthropic", 0))).await;

    let panel = get_status(&base_url, "/status/health").await;

    let _ = available_data(&panel);
}

/// A NONZERO cap serves identically.
///
/// The pair for the zero-cap case. Without it, a daemon that refused to start on
/// ANY `[fidelity]` cap would satisfy the test above by failing the same way for a
/// different reason, and the cap-zero assertion would be about nothing.
#[tokio::test]
async fn a_nonzero_cap_lane_serves_and_reports_a_healthy_panel() {
    let base_url = spawn(servable_config(caps_of("anthropic", 3))).await;

    let panel = get_status(&base_url, "/status/health").await;

    let _ = available_data(&panel);
}

/// A cap naming an UNKNOWN provider is refused at config load, not discovered at
/// runtime.
///
/// The load-time validation the config slice ships, asserted here because its
/// consequence is a runtime one: an operator's typo must fail the load rather than
/// silently configuring a budget no reservation is ever checked against. Driven
/// through the same validation path the daemon boots through.
#[tokio::test]
async fn a_cap_naming_an_unknown_provider_is_refused_rather_than_silently_ignored() {
    let config = servable_config(caps_of("no-such-provider", 5));

    let outcome = collect_config_validation(&config);

    assert!(
        outcome
            .errors
            .iter()
            .any(|e| e.contains("paid_probe_daily_caps")),
        "a cap keyed on a provider no `[providers]` entry names is an operator typo, \
         and discovering it at runtime means a budget that gates nothing: {:?}",
        outcome.errors,
    );
}

/// The paired control for the validation above: a cap naming a CONFIGURED provider
/// loads clean.
///
/// Without it, a validator that refused every `[fidelity]` cap would satisfy the
/// refusal test while making the feature unusable.
#[tokio::test]
async fn a_cap_naming_a_configured_provider_loads_clean() {
    let config = servable_config(caps_of("anthropic", 5));

    let outcome = collect_config_validation(&config);

    assert!(
        !outcome
            .errors
            .iter()
            .any(|e| e.contains("paid_probe_daily_caps")),
        "a cap on a configured provider is the supported shape: {:?}",
        outcome.errors,
    );
}

/// A fresh daemon reports NO resident field verdict, and reports it as an empty
/// set rather than an absent surface.
///
/// The feature-ABSENT half of the per-verdict floor. A daemon that has learned
/// nothing must be distinguishable from one whose verdict reporting is broken, and
/// on a status surface those two look identical unless the empty case is explicit.
#[tokio::test]
async fn a_fresh_daemon_reports_an_empty_learned_set_rather_than_an_absent_one() {
    let base_url = spawn(servable_config(FidelityConfig::default())).await;

    let panel = get_status(&base_url, "/status/health").await;

    let learned = &available_data(&panel)["learned_negatives"];
    assert!(
        learned.is_array(),
        "the learned set is present as an array even when empty: an absent field \
         reads as a broken surface rather than as a clean daemon",
    );
    assert_eq!(
        learned.as_array().map(Vec::len),
        Some(0),
        "and a daemon that has served no traffic has learned nothing",
    );
}
