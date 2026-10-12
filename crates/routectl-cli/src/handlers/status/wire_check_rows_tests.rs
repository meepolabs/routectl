//! The wire-check rows show what the router's state holds inside the status
//! window and nothing older, with every unrepaired rejection's five fields.

use routectl_core::wire_check::{
    EgressKind, IngressDialect, Lane, Leg, PathClass, RejectionProvenance, WirePath,
};
use routectl_router::Config;
use routectl_router::wire_check_state::{F1Outcome, WireCheckState};
use serde_json::{Value, json};

use super::project;

const LOST_PATH: &str = "/metadata";

fn lane() -> Lane {
    Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi)
}

fn snapshot_of(state: &WireCheckState) -> routectl_router::wire_check_state::WireCheckSnapshot {
    state.snapshot(&Config::default().wire_check)
}

fn rendered(rows: &super::WireCheckRows) -> Value {
    serde_json::to_value(rows).expect("rows serialize")
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

#[test]
fn a_loss_key_shows_inside_the_window_and_drops_once_last_seen_is_outside_it() {
    let state = WireCheckState::default();
    let path = WirePath::root().child("metadata");
    state.record_compared(lane(), Leg::Request, [(&path, PathClass::Missing)], false);
    state.record_local_refusal(lane());
    let snapshot = snapshot_of(&state);
    let window = snapshot.window_secs;
    let seen = snapshot.loss[0].reading.last_seen_unix_secs;

    let inside = rendered(&project(&snapshot, seen + window));
    let outside = rendered(&project(&snapshot, seen + window + 1));

    assert_eq!(inside["window_secs"], json!(window));
    assert_eq!(
        inside["loss"],
        json!([{
            "lane": "anthropic->anthropic-api",
            "path_class": "missing",
            "count": 1,
            "last_seen_unix_secs": seen,
        }])
    );
    assert_eq!(
        inside["loss_findings"],
        json!([{
            "lane": "anthropic->anthropic-api",
            "leg": "request",
            "path": LOST_PATH,
            "path_class": "missing",
            "ts_unix_secs": seen,
        }])
    );
    assert_eq!(inside["checks"][0]["outcome"], json!("loss"));
    assert_eq!(
        inside["local_refusals"][0]["lane"],
        json!("anthropic->anthropic-api")
    );
    for key in ["loss", "loss_findings", "checks", "local_refusals"] {
        assert_eq!(outside[key], json!([]), "{key} past the window");
    }
}

#[test]
fn an_unrepaired_rejection_renders_all_five_fields_and_a_repaired_one_does_not_render() {
    let state = WireCheckState::default();
    let path = WirePath::root().child("tools").index(0);
    state.record_rejection(
        lane(),
        Leg::Request,
        &path,
        RejectionProvenance::ClientUnchanged,
        F1Outcome::Unrepaired,
    );
    state.record_rejection(
        lane(),
        Leg::Request,
        &WirePath::root().child("metadata"),
        RejectionProvenance::RoutectlAdded,
        F1Outcome::Repaired,
    );
    let snapshot = snapshot_of(&state);

    let rows = rendered(&project(&snapshot, now_unix_secs()));

    let unrepaired = rows["unrepaired_rejections"]
        .as_array()
        .expect("an array of rows");
    assert_eq!(unrepaired.len(), 1, "{rows}");
    assert_eq!(
        rows["loss_findings"],
        json!([]),
        "a rejection is not a loss"
    );
    let row = &unrepaired[0];
    assert_eq!(row["lane"], json!("anthropic->anthropic-api"));
    assert_eq!(row["leg"], json!("request"));
    assert_eq!(row["path"], json!("/tools/0"));
    assert_eq!(row["f1_outcome"], json!("unrepaired"));
    assert_eq!(row["provenance"], json!("client_unchanged"));
}

#[test]
fn an_unrepaired_rejection_older_than_the_window_does_not_render() {
    let state = WireCheckState::default();
    state.record_rejection(
        lane(),
        Leg::Request,
        &WirePath::root().child("metadata"),
        RejectionProvenance::ClientChanged,
        F1Outcome::Unrepaired,
    );
    let snapshot = snapshot_of(&state);
    let ts = snapshot.findings[0].ts_unix_secs;

    let inside = rendered(&project(&snapshot, ts + snapshot.window_secs));
    let outside = rendered(&project(&snapshot, ts + snapshot.window_secs + 1));

    assert_eq!(inside["unrepaired_rejections"][0]["path"], json!(LOST_PATH));
    assert_eq!(outside["unrepaired_rejections"], json!([]));
}
