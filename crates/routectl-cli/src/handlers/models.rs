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
    // the `model` field, deduplicated. Sources, in priority order:
    //   1. [aliases] keys                    (direct alias use)
    //   2. ingress.<dialect>.aliases keys    (model-id rewrite at ingress)
    //   3. default_model                     (catch-all destination)
    //   4. provider:model literals           (any value with a ':')
    //      that appear in alias chains
    // Without (2) and (3), `/v1/models` misreports the server as
    // unable to serve identifiers that routing actually accepts --
    // operators run `curl /v1/models` to discover what works and
    // were getting incomplete answers.
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
        emit(alias, &mut entries, &mut seen);
    }
    for ingress_alias in config.ingress.openai.aliases.keys() {
        emit(ingress_alias, &mut entries, &mut seen);
    }
    for ingress_alias in config.ingress.anthropic.aliases.keys() {
        emit(ingress_alias, &mut entries, &mut seen);
    }
    if let Some(default) = config.default_model.as_deref() {
        emit(default, &mut entries, &mut seen);
    }
    for alias_entry in config.aliases.values() {
        for target in &alias_entry.chain {
            if target.contains(':') {
                emit(target, &mut entries, &mut seen);
            }
        }
    }

    Json(json!({
        "object": "list",
        "data": entries,
    }))
}
