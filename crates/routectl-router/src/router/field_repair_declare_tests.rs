// The reactive repair declares the field it dropped on the retry it sends,
// and declares nothing when it does not fire.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

/// [`req_on`] with an Anthropic client body captured, so the attempt travels
/// a fidelity lane.
fn anthropic_ingress_req(alias: &str) -> ChatRequest {
    let mut req = req_on(alias);
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&serde_json::json!({}))
            .expect("serialize")
            .into(),
    });
    req
}

fn field_declarations(
    req: &ChatRequest,
) -> Vec<(
    routectl_core::wire_check::EditSite,
    String,
    routectl_core::wire_check::EditKind,
)> {
    req.routectl_internal
        .declared_edits
        .iter()
        .map(|edit| (edit.site(), edit.path().to_owned(), edit.kind()))
        .collect()
}

#[tokio::test]
async fn a_reactive_repair_declares_the_dropped_field_on_its_retry() {
    use routectl_core::wire_check::{EditKind, EditSite};
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::ServeRepaired);

    let served = Box::pin(router.complete(anthropic_ingress_req(ALIAS))).await;

    assert!(served.is_ok(), "{served:?}");
    let repairs: Vec<_> = field_declarations(&seen.attempt(1))
        .into_iter()
        .filter(|(site, ..)| *site == EditSite::FieldRepair)
        .collect();
    assert_eq!(
        repairs,
        [(
            EditSite::FieldRepair,
            "/thinking/display".to_owned(),
            EditKind::Remove
        )]
    );
    assert!(
        field_declarations(&seen.attempt(0))
            .iter()
            .all(|(site, ..)| *site != EditSite::FieldRepair),
        "the carried attempt was not repaired and declares no repair"
    );
}

#[tokio::test]
async fn an_unrelated_error_declares_no_field_repair() {
    use routectl_core::wire_check::EditSite;
    let _injection = provisional::inject(REJECTED_PATH);
    let (router, seen) = single_seat(ALIAS, Answer::UnrelatedError);

    let _ = Box::pin(router.complete(anthropic_ingress_req(ALIAS))).await;

    assert_eq!(seen.repairs(), 0, "premise: no repair fired");
    for attempt in 0..seen.calls() {
        assert!(
            field_declarations(&seen.attempt(attempt))
                .iter()
                .all(|(site, ..)| *site != EditSite::FieldRepair),
            "attempt {attempt}"
        );
    }
}
