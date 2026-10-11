// A learned capability strip declares each tool it removed; a strip that
// does not fire declares nothing.
// An `include!`d FRAGMENT of `strip_interceptor_dispatch_tests.rs`.

fn with_anthropic_ingress(mut req: ChatRequest) -> ChatRequest {
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&serde_json::json!({}))
            .expect("serialize")
            .into(),
    });
    req
}

fn learned_declarations(req: &ChatRequest) -> Vec<(String, routectl_core::wire_check::EditKind)> {
    req.routectl_internal
        .declared_edits
        .iter()
        .filter(|edit| edit.site() == routectl_core::wire_check::EditSite::LearnedRepair)
        .map(|edit| (edit.path().to_owned(), edit.kind()))
        .collect()
}

#[tokio::test]
async fn a_learned_strip_declares_the_removed_tool_by_index() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);
    router
        .learned_capabilities
        .import_entries(vec![acting_advisor_negative("prov#claude-haiku")]);
    let mut req = advisor_request();
    req.tools = Some(vec![
        ToolDef::Other(json!({"type": "bash_20250124", "name": "bash"})),
        ToolDef::Other(json!({"type": "advisor", "name": "advisor"})),
    ]);

    Box::pin(router.complete(with_anthropic_ingress(req)))
        .await
        .expect("ok");

    let cap = cap.lock();
    let upstream = cap.first().expect("one upstream call");
    assert_eq!(
        dispatched_tool_types(upstream),
        ["bash_20250124"],
        "premise"
    );
    assert_eq!(
        learned_declarations(upstream),
        [(
            "/tools/1".to_owned(),
            routectl_core::wire_check::EditKind::Remove
        )]
    );
}

#[tokio::test]
async fn no_learned_negative_declares_no_learned_strip() {
    let cap = captured();
    let provider: Arc<dyn Provider> = Arc::new(ProbeProvider {
        captured: cap.clone(),
    });
    let router = build_router(provider, true);

    Box::pin(router.complete(with_anthropic_ingress(advisor_request())))
        .await
        .expect("ok");

    let cap = cap.lock();
    let upstream = cap.first().expect("one upstream call");
    assert_eq!(dispatched_tool_types(upstream), ["advisor"], "premise");
    assert!(learned_declarations(upstream).is_empty());
}
