// A pre-flight drop declares the field it removed; a request the planner
// leaves alone declares nothing.
// An `include!`d FRAGMENT of `field_preflight_tests.rs`.

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

fn preflight_declarations(req: &ChatRequest) -> Vec<(String, routectl_core::wire_check::EditKind)> {
    req.routectl_internal
        .declared_edits
        .iter()
        .filter(|edit| edit.site() == routectl_core::wire_check::EditSite::Preflight)
        .map(|edit| (edit.path().to_owned(), edit.kind()))
        .collect()
}

#[tokio::test]
async fn a_preflight_drop_declares_the_removed_field() {
    let (router, seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let dispatched = Box::pin(
        router.complete_with_options(with_anthropic_ingress(req_on(ALIAS)), RouterOptions::new()),
    )
    .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(
        preflight_declarations(&seen.attempt(0)),
        [(
            "/thinking/display".to_owned(),
            routectl_core::wire_check::EditKind::Remove
        )]
    );
}

#[tokio::test]
async fn a_request_the_planner_leaves_alone_declares_no_preflight() {
    let (router, seen) = single_seat(Answer::ServeImmediately);

    let dispatched = Box::pin(
        router.complete_with_options(with_anthropic_ingress(req_on(ALIAS)), RouterOptions::new()),
    )
    .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(seen.carried_per_attempt(), vec![true], "premise: no drop");
    assert!(preflight_declarations(&seen.attempt(0)).is_empty());
}
