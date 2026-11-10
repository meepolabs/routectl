// Price-table pickup through `/status/query`: a table renamed into the config
// folder reprices the next query on the SAME state, with no Router rebuild, and
// a malformed copy keeps the last good figure. `include!`d into
// `query_tests.rs`; all imports come from the host (its `use super::*`); do not
// add `use` lines here.
//
// --- the hand-computed oracle -------------------------------------------
//
// `seed_ledger` writes one row with 100 input + 20 output tokens served by
// provider `p` on upstream `u`. `p` is an API-key anthropic-api entry, so the
// first-party fallback selects the table provider `anthropic`:
//
//   table A  input 1000, output 1000 $/Mtok: 100*1000/1e6 + 20*1000/1e6 = $0.12
//   table B  input 2000, output 2000 $/Mtok: 100*2000/1e6 + 20*2000/1e6 = $0.24

const TABLE_A_USD: f64 = 0.12;
const TABLE_B_USD: f64 = 0.24;

fn table_json(rate: f64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "generated_at": "2026-11-01",
        "providers": [{
            "id": "anthropic",
            "models": [{ "id": "u", "input": rate, "output": rate }],
        }],
    }))
    .unwrap()
}

/// Stage `bytes` beside `target`, give it a distinct mtime, and rename it into
/// place in one step, the way `routectl prices refresh` replaces the file.
fn rename_in(target: &Path, bytes: &[u8], mtime_secs: u64) {
    let staged = target.with_extension("staged");
    std::fs::write(&staged, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&staged)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime_secs))
        .unwrap();
    std::fs::rename(&staged, target).unwrap();
}

/// A live status state over a ledger and a price book in `dir`, serving a
/// config whose provider `p` the ledger rows name, plus the router swap it
/// reads so a test can see whether the Router was replaced.
fn priced_state(dir: &Path, ledger: PathBuf) -> (Arc<StatusState>, Arc<ArcSwap<Router>>) {
    let mut config = Config::default();
    config.providers.insert(
        "p".to_string(),
        routectl_router::ProviderEntry::anthropic_api("env://P_KEY"),
    );
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(Router::new(Arc::new(
        config,
    )))));
    let mut status = StatusState::from_app(&app, None, DaemonMeta::for_test());
    status.usage_db_path = ledger;
    status.prices = Arc::new(routectl_router::prices::PriceBook::new(
        dir.join("prices.json"),
    ));
    (Arc::new(status), Arc::clone(&app.router))
}

fn total_cost(json: &Value) -> Option<f64> {
    json["data"]["totals"]["cost_usd"].as_f64()
}

fn assert_cost(json: &Value, expected: f64, what: &str) {
    let cost = total_cost(json).unwrap_or_else(|| panic!("{what}: no cost in {json}"));
    assert!(
        (cost - expected).abs() < 1e-9,
        "{what}: {cost} != {expected}"
    );
}

#[tokio::test]
async fn a_renamed_in_price_table_reprices_the_next_query_without_a_router_rebuild() {
    // Arrange: the state exists BEFORE any table does, so a figure can only
    // come from a read at query time.
    let dir = TempDir::new().unwrap();
    let ledger = dir.path().join("usage.db");
    seed_ledger(&ledger, &[Local::now().timestamp_millis()]);
    let table = dir.path().join("prices.json");
    let (state, swap) = priced_state(dir.path(), ledger);
    let router_before = swap.load_full();

    // Act + Assert: no table yet -> unpriced, never $0.
    let (_, none) = send(Arc::clone(&state), QUERY_METHOD, VALID_BODY).await;
    assert_eq!(total_cost(&none), None, "no table prices nothing: {none}");
    assert_eq!(none["data"]["totals"]["cost_status"], "unpriced");

    rename_in(&table, &table_json(1000.0), 1_000);
    let (_, first) = send(Arc::clone(&state), QUERY_METHOD, VALID_BODY).await;
    assert_cost(&first, TABLE_A_USD, "table A");

    rename_in(&table, &table_json(2000.0), 2_000);
    let (_, second) = send(Arc::clone(&state), QUERY_METHOD, VALID_BODY).await;
    assert_cost(&second, TABLE_B_USD, "table B renamed in");

    rename_in(&table, b"{\"schema_version\":1,", 3_000);
    let (_, kept) = send(Arc::clone(&state), QUERY_METHOD, VALID_BODY).await;
    assert_cost(
        &kept,
        TABLE_B_USD,
        "a malformed copy keeps the last good table",
    );

    // The router the state serves is the one it started with.
    assert!(Arc::ptr_eq(&router_before, &swap.load_full()));
}
