//! The dated `computer_*` tool type a client sends and the `computer_use` key
//! an operator writes in `[capability.overrides]` must meet on one feature
//! key at the filter seam.

use super::*;

use std::sync::Arc;

use routectl_core::capability::COMPUTER_USE;
use routectl_core::{ChatChunk, ChatResponse, Provider, ToolDef};
use serde_json::json;

use crate::config::Config;
use crate::feature_keys::derive_feature_keys;
use crate::resolved::ResolvedModel;

/// Provider stub; these tests drive the filter seam and never dispatch.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("computer key tests never dispatch")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("computer key tests never dispatch")
    }
}

/// A router with the capability subsystem enabled and two openai-compat
/// providers `p1` and `p2`; `extra` appends further TOML tables.
fn router_with(extra: &str) -> Router {
    let body = format!(
        "version = 5\n\
         [providers.p1]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n\
         [providers.p2]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n\
         [capability]\n\
         enabled = true\n\
         {extra}"
    );
    let config: Config = toml::from_str(&body).expect("config parses");
    Router::new(Arc::new(config))
}

/// A dispatch target on `provider`.
fn target(router: &Router, provider: &str, nickname: &str) -> DispatchTarget {
    let stub: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = ResolvedModel::new(nickname, provider, stub, nickname);
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

/// The feature keys of a request carrying the real wire computer tool type.
fn computer_request_features() -> Vec<String> {
    let tools = [ToolDef::Other(json!({
        "type": "computer_20250124",
        "name": "computer",
        "display_width_px": 1024,
        "display_height_px": 768
    }))];
    derive_feature_keys(&tools, None, None)
}

#[test]
fn dated_computer_request_derives_the_override_key() {
    // Act
    let features = computer_request_features();

    // Assert
    assert_eq!(features, vec![COMPUTER_USE.to_string()]);
}

#[test]
fn computer_use_override_keeps_a_dated_computer_request_off_the_target() {
    // Arrange
    let router = router_with("[capability.overrides.p1]\nunsupported = [\"computer_use\"]\n");
    let chain = vec![target(&router, "p1", "m1"), target(&router, "p2", "m2")];
    let mut admissions = Vec::new();

    // Act
    let filtered = router
        .filter_chain_by_features(
            chain,
            &computer_request_features(),
            "computer-alias",
            DispatchSurface::Complete,
            &mut admissions,
        )
        .expect("the unflagged target survives");

    // Assert
    let survivors: Vec<&str> = filtered.iter().map(|t| t.provider_name.as_str()).collect();
    assert_eq!(survivors, vec!["p2"]);
}
