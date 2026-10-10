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
//! daemon with no resident verdict must report an empty row set rather than an
//! absent field.
//!
//! # No live services, no live ledger
//!
//! The daemon binds an OS-assigned loopback port, its provider points at a dead
//! local port so nothing dials out, and its usage DB is a per-test tempdir. The
//! live daemon on its own port and the real `~/.config` ledger are never touched.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry};
use serde_json::Value;
use tokio::net::TcpListener;

mod common;

/// A servable config: one anthropic-api provider whose base URL is a dead local
/// port, one model, one alias.
///
/// Dead on purpose. Every assertion here is about a READ surface, and a reachable
/// upstream would let a background probe or a warm dial change state underneath a
/// purity assertion -- which would make a failure indistinguishable from a race.
fn servable_config() -> Arc<Config> {
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

/// The health panel is reachable and AVAILABLE on a real daemon, which is the
/// precondition every assertion below rests on.
///
/// Its own test rather than an assertion inside the others: a panel that degraded
/// to unavailable would make every field-level check below vacuous -- an
/// unavailable panel carries no fields at all -- so the precondition is asserted
/// once, loudly, where its failure names itself.
#[tokio::test]
async fn the_health_panel_is_available_on_a_real_daemon() {
    let base_url = spawn(servable_config()).await;

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
    let base_url = spawn(servable_config()).await;
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
    let base_url = spawn(servable_config()).await;

    let panel = get_status(&base_url, "/status/doctor").await;

    // A daemon spawned without an on-disk config path reports the no-config-path
    // code rather than a report, which is correct and is NOT what this asserts: what
    // matters is that the route is wired and answers its envelope rather than 404.
    assert!(
        panel["schema_version"].is_number(),
        "the doctor route answers its panel envelope: {panel}",
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
    let base_url = spawn(servable_config()).await;

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
