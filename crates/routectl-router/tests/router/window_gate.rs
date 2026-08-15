//! Dispatch-path coverage for the proactive context-window gate: a target
//! whose window cannot hold the request is never called at all.

use super::*;

use routectl_router::{CatalogRow, EffectiveRow};

/// A request whose serialized size is far past any window these tests
/// configure.
fn oversized_req(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("oversized-".repeat(2_000)),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    }
}

/// An effective row confirming `window` context tokens.
fn row_with_window(window: u32) -> EffectiveRow {
    let mut row = CatalogRow::sentinel();
    row.max_context_tokens = Some(window);
    EffectiveRow::Present {
        row,
        source: routectl_router::Source::Baked,
        verified_at: "seed".into(),
    }
}

/// Build a two-entry chain `[m1, m2]` whose models carry the given windows.
fn router_with_windows(
    first_window: u32,
    second_window: Option<u32>,
    p1: Arc<dyn Provider>,
    p2: Arc<dyn Provider>,
) -> Router {
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let cfg = Config {
        aliases,
        retry: default_test_retry(),
        ..Default::default()
    };
    let mut router = Router::new(Arc::new(cfg));
    let mut resolved: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    resolved.insert(
        "m1".into(),
        Arc::new(
            ResolvedModel::new("m1", "p1", p1, "m1")
                .with_effective_row(row_with_window(first_window)),
        ),
    );
    let second = ResolvedModel::new("m2", "p2", p2, "m2");
    resolved.insert(
        "m2".into(),
        Arc::new(match second_window {
            Some(window) => second.with_effective_row(row_with_window(window)),
            None => second,
        }),
    );
    router.install_resolved_models(resolved);
    router
}

#[tokio::test]
async fn an_oversized_target_receives_no_upstream_call() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    // m1's window is tiny, m2's is unconfirmed -- so m1 is skipped and m2
    // (kept per the unknown-window rule) serves.
    let r = router_with_windows(
        1_000,
        None,
        p1.clone() as Arc<dyn Provider>,
        p2.clone() as Arc<dyn Provider>,
    );

    let resp = r
        .complete(oversized_req("fast"))
        .await
        .expect("the surviving target serves");

    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(
        p1.calls(),
        0,
        "the skipped target must be reached by no upstream call",
    );
    assert_eq!(p2.calls(), 1);
}

#[tokio::test]
async fn a_last_oversized_target_is_still_attempted() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    // Both windows are too small, so the gate refuses to empty the chain
    // and the caller sees exactly today's behavior: m1 is attempted.
    let r = router_with_windows(
        1_000,
        Some(1_000),
        p1.clone() as Arc<dyn Provider>,
        p2.clone() as Arc<dyn Provider>,
    );

    let resp = r
        .complete(oversized_req("fast"))
        .await
        .expect("the chain still dispatches");

    assert_eq!(resp.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(p1.calls(), 1);
}
