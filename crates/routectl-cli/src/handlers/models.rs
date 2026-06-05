use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use chrono::Utc;
use serde_json::{json, Value};

use crate::server::AppState;

/// `GET /v1/models` -- discovery endpoint that lists every routable
/// identifier the server accepts on the `model` field.
///
/// Sources walked in order, deduplicated:
///   1. `[aliases]` keys     (wire model -> nickname/chain)
///   2. `[models]` keys      (wire model -> direct nickname)
///
/// Two classes of alias keys are intentionally omitted because they
/// are not selectable identifiers on their own:
///   - the literal `default` catch-all key, used as the fallback
///     when no other route matches
///   - any glob-pattern key (containing `*`, e.g. `claude-opus-*`),
///     used by the router for prefix matching at dispatch time but
///     not a model the client may select. This matters for clients
///     like claude-code 2.1.129+ with
///     `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` that surface
///     `/v1/models` ids as a picker -- listing `claude-opus-*`
///     would put a non-selectable pattern in front of the user.
///
/// Models with `selectable = false` are also omitted -- they exist
/// in the config (operator may be staging an entry) but the router
/// refuses to route to them.
pub async fn list_models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let now = Utc::now().timestamp();
    // Snapshot the live Router once so a hot-swap mid-request does
    // not mix old + new alias state in the response payload.
    let router = state.router.load_full();
    let config = &router.config;

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
        // Skip suffix-glob pattern keys (e.g. `claude-opus-*`) --
        // they're routing patterns the dispatcher matches against,
        // not selectable model ids. Surfacing them as picker entries
        // (claude-code 2.1.129+ gateway model discovery) would let a
        // user click a pattern and produce a request the server
        // can't dispatch.
        if alias.contains('*') {
            continue;
        }
        emit(alias, &mut entries, &mut seen);
    }
    // Skip `selectable = false` model nicknames -- they exist in the
    // config (operator may be staging an entry) but the router refuses
    // to route to them.
    for (nickname, model) in &config.models {
        if !model.selectable {
            continue;
        }
        emit(nickname, &mut entries, &mut seen);
    }

    Json(json!({
        "object": "list",
        "data": entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use routectl_router::{
        AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router, ServerConfig,
    };

    /// claude-code 2.1.129+ supports
    /// `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` which queries the
    /// gateway's `/v1/models` and surfaces ids as a picker. Glob-pattern
    /// alias keys (`claude-opus-*`) are router-side prefix matches, NOT
    /// selectable models -- listing them would let a user click a
    /// pattern that the dispatcher cannot route. The literal `default`
    /// catch-all is omitted for the same reason. Pin the filter so a
    /// future refactor doesn't accidentally re-emit either class.
    #[tokio::test]
    async fn list_models_skips_glob_keys_and_default() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "p".into(),
            ProviderEntry::openai_compat("http://127.0.0.1:1", "literal:test-key"),
        );

        let mut models = BTreeMap::new();
        models.insert("haiku".into(), ModelEntry::new("p", "claude-haiku-x"));
        models.insert("opus".into(), ModelEntry::new("p", "claude-opus-x"));

        let mut aliases = BTreeMap::new();
        aliases.insert("claude-opus-*".into(), AliasValue::Single("opus".into()));
        aliases.insert("default".into(), AliasValue::Single("haiku".into()));
        aliases.insert("claude-3".into(), AliasValue::Single("haiku".into()));

        let config = Arc::new(Config {
            server: ServerConfig::default(),
            providers,
            aliases,
            retry: RetryPolicy::default(),
            models,
            ..Default::default()
        });
        let router = Router::new(config);
        let state = Arc::new(AppState {
            router: Arc::new(arc_swap::ArcSwap::from_pointee(router)),
        });

        let Json(body) = list_models(State(state)).await;
        assert_eq!(body["object"], "list");

        let mut ids: Vec<String> = body["data"]
            .as_array()
            .expect("data must be an array")
            .iter()
            .map(|e| e["id"].as_str().expect("id must be a string").to_string())
            .collect();
        ids.sort();

        assert_eq!(
            ids,
            vec![
                "claude-3".to_string(),
                "haiku".to_string(),
                "opus".to_string(),
            ],
            "expected exactly [claude-3, haiku, opus] (sorted); got: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.contains('*')),
            "no entry may contain '*'; glob alias keys must be filtered: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "default"),
            "literal `default` catch-all must be filtered: {ids:?}"
        );
    }
}
