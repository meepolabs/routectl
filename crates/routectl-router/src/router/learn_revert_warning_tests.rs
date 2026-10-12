//! The `routectl_edit_rejected` warning and what outlives a revert: one
//! warning per lane, path and attribution, rendered only as the masked,
//! sanitized pointer, and nothing remembered by the next request.

use std::sync::Arc;

use routectl_core::wire_check::{EgressKind, IngressDialect, Lane, WirePath};

use super::super::EditAttribution;
use super::{
    ANTHROPIC, ANTHROPIC_LANE, CONTROL_KEY, OP_KEY, OPERATOR_EXTRA, Row, SURFACES,
    ScriptedProvider, anthropic_request, assert_nothing_learned, dispatch, edit_rejected_warnings,
    faithful, refuse_op_key_while_carried, rejection, rejections, router, run_rows, warned,
};
use crate::wire_check_state::FieldRepairOutcome;

#[tokio::test]
async fn the_warning_fires_once_per_lane_path_and_site() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(faithful, refuse_op_key_while_carried);
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, false, &[Arc::clone(&provider)]);

        let first = dispatch(&router, surface, anthropic_request()).await;
        let second = dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(
            edit_rejected_warnings(&first.events).len(),
            1,
            "{surface:?}: the first rejection warns"
        );
        assert!(
            edit_rejected_warnings(&second.events).is_empty(),
            "{surface:?}: the same lane, path and site stays silent"
        );
    }
}

#[tokio::test]
async fn a_revert_is_not_remembered_by_the_next_request() {
    for surface in SURFACES {
        let provider = ScriptedProvider::new(faithful, refuse_op_key_while_carried);
        let block = format!("{ANTHROPIC}{OPERATOR_EXTRA}");
        let router = router(&block, false, &[Arc::clone(&provider)]);

        let first = dispatch(&router, surface, anthropic_request()).await;
        let second = dispatch(&router, surface, anthropic_request()).await;

        assert!(first.served && second.served, "{surface:?}: both served");
        assert_eq!(
            provider.calls(),
            4,
            "{surface:?}: each request re-sent once"
        );
        assert_eq!(
            rejections(&router.wire_check_snapshot()),
            [("routectl_added", FieldRepairOutcome::Repaired); 2],
            "{surface:?}"
        );
        assert_nothing_learned(&router, &second, &format!("{surface:?}"));
    }
}

/// The warning renders the path a rejection names only as its masked,
/// sanitized pointer: a client key, a control character in one included,
/// never reaches the log line.
#[test]
fn the_warning_never_renders_a_client_key() {
    let rows: [(&str, &[&str], &str); 3] = [
        (
            "a key under a masked container",
            &["metadata", OP_KEY],
            "/metadata/*",
        ),
        (
            "a control character in a key under a masked container",
            &["metadata", CONTROL_KEY],
            "/metadata/*",
        ),
        (
            "a control character in a top-level key",
            &[CONTROL_KEY],
            "/*",
        ),
    ];
    let lane = Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi);
    for (name, segments, masked) in rows {
        let router = router(ANTHROPIC, false, &[]);
        let path = WirePath::from_segments(segments.iter().copied());

        let events = routectl_testkit::capture_events(|| {
            router.warn_edit_rejected(lane, &path, EditAttribution::Undeclared);
        });

        assert_eq!(
            warned(&events),
            [(
                ANTHROPIC_LANE.to_owned(),
                masked.to_owned(),
                "undeclared".to_owned()
            )],
            "{name}"
        );
        let raw_key = segments.last().expect("a key");
        for event in &events {
            let line = format!("{} {:?}", event.message, event.fields);
            assert!(!line.contains(raw_key), "{name}: {line}");
            assert!(!line.contains('\u{1b}'), "{name}: {line}");
        }
    }
}

/// End to end, a rejection naming a key with a control character localizes
/// no path, so it is matched to no edit and nothing renders the key.
#[tokio::test]
async fn a_rejected_key_with_a_control_character_is_neither_reverted_nor_rendered() {
    run_rows(&[Row {
        name: "control character in the rejected key",
        extras: OPERATOR_EXTRA,
        cache: false,
        request: anthropic_request,
        shape: faithful,
        answer: |_| Err(rejection(&format!("metadata.{CONTROL_KEY}"))),
        calls: 1,
        served: false,
        field_repairs: 0,
        findings: &[("unlocalized", FieldRepairOutcome::Unrepaired)],
        warning: None,
    }])
    .await;
}
