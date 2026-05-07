use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use chrono::Utc;
use serde_json::{json, Value};

use crate::server::AppState;

pub async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = Utc::now().timestamp();
    let config = &state.router.config;

    let mut entries: Vec<Value> = config
        .aliases
        .keys()
        .map(|alias| {
            json!({
                "id": alias,
                "object": "model",
                "created": now,
                "owned_by": "routectl"
            })
        })
        .collect();

    // Also expose direct provider:model targets that appear in chains.
    let mut seen_direct: std::collections::BTreeSet<String> =
        config.aliases.keys().cloned().collect();
    for alias_entry in config.aliases.values() {
        for target in &alias_entry.chain {
            if target.contains(':') && !seen_direct.contains(target) {
                seen_direct.insert(target.clone());
                entries.push(json!({
                    "id": target,
                    "object": "model",
                    "created": now,
                    "owned_by": "routectl"
                }));
            }
        }
    }

    Json(json!({
        "object": "list",
        "data": entries
    }))
}
