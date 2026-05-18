use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use chrono::Utc;
use serde_json::{json, Value};

use crate::server::AppState;

pub async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = Utc::now().timestamp();
    let config = &state.router.config;

    // Collect every routable identifier the server will accept on
    // the `model` field, deduplicated. v0.6.0 sources:
    //   1. [aliases] keys      (wire model -> nickname/chain)
    //   2. [models] keys       (wire model -> direct nickname)
    // Without listing both, /v1/models would misreport the server as
    // unable to serve identifiers that routing actually accepts.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut entries: Vec<Value> = Vec::new();
    let emit =
        |id: &str, entries: &mut Vec<Value>, seen: &mut std::collections::BTreeSet<String>| {
            if !seen.insert(id.to_string()) {
                return;
            }
            entries.push(json!({
                "id": id,
                "object": "model",
                "created": now,
                "owned_by": "routectl",
            }));
        };

    for alias in config.aliases.keys() {
        // Skip the `default` catch-all key; it isn't a routable
        // identifier on its own.
        if alias == "default" {
            continue;
        }
        emit(alias, &mut entries, &mut seen);
    }
    for nickname in config.models.keys() {
        emit(nickname, &mut entries, &mut seen);
    }

    Json(json!({
        "object": "list",
        "data": entries,
    }))
}
