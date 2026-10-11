// A canary restores its field undeclared, and its repaired retry declares
// the drop it makes.
// An `include!`d FRAGMENT of `field_canary_settlement_tests.rs`.

fn canary_ingress_req() -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&serde_json::json!({}))
            .expect("serialize")
            .into(),
    });
    req
}

fn site_declarations(
    req: &ChatRequest,
    site: routectl_core::wire_check::EditSite,
) -> Vec<(String, routectl_core::wire_check::EditKind)> {
    req.routectl_internal
        .declared_edits
        .iter()
        .filter(|edit| edit.site() == site)
        .map(|edit| (edit.path().to_owned(), edit.kind()))
        .collect()
}

#[tokio::test]
async fn a_canary_repaired_retry_declares_its_drop_and_the_canary_itself_declares_none() {
    use routectl_core::wire_check::{EditKind, EditSite};
    let _injection = inject_resolution();
    let (router, seen) = single_seat(Answer::ServeRepaired);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched =
        Box::pin(router.complete_with_options(canary_ingress_req(), RouterOptions::new())).await;

    assert!(dispatched.result.is_ok());
    assert_eq!(seen.carried_per_attempt(), vec![true, false], "premise");
    let attempts = seen.dispatched.lock().clone();
    assert!(site_declarations(&attempts[0], EditSite::Canary).is_empty());
    assert!(
        site_declarations(&attempts[0], EditSite::Preflight).is_empty(),
        "the canary restored the field, so nothing was dropped pre-flight"
    );
    assert_eq!(
        site_declarations(&attempts[1], EditSite::Canary),
        [("/thinking/display".to_owned(), EditKind::Remove)]
    );
}

#[tokio::test]
async fn a_served_canary_declares_no_canary_drop() {
    use routectl_core::wire_check::EditSite;
    let (router, seen) = single_seat(Answer::ServeEverything);
    plant_eligible_with_canary_due(&router, "m0");

    let dispatched =
        Box::pin(router.complete_with_options(canary_ingress_req(), RouterOptions::new())).await;

    assert!(dispatched.result.is_ok());
    assert_eq!(seen.calls(), 1, "premise: the canary served unrepaired");
    let attempts = seen.dispatched.lock().clone();
    assert!(site_declarations(&attempts[0], EditSite::Canary).is_empty());
}
