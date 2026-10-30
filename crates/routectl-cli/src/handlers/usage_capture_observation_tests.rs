//! The request observation slot reaches the persisted ledger row: each of
//! the five per-request counts, the probe flag, and (when the dispatch meta
//! never arrived) the last target's served identity. Every assertion reads
//! the row back from the database, not the in-memory record, so a column
//! the writer failed to bind would show here.

use super::*;
use routectl_core::{Message, MessageContent, Role};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter};

const ROW_ID: &str = "observed-row";

fn request() -> routectl_core::ChatRequest {
    routectl_core::ChatRequest {
        model: "requested".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

/// A capture over a fresh slot and a readable database.
struct Rig {
    capture: UsageCapture,
    slot: RequestObservation,
    handle: UsageHandle,
    writer: UsageWriter,
    _dir: tempfile::TempDir,
    db_path: std::path::PathBuf,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("usage tempdir");
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let slot = RequestObservation::new();
    let draft = build_usage_draft("anthropic", &request(), ROW_ID.to_string());
    let capture = UsageCapture::new(draft, handle.clone(), "ingress-1".to_string(), slot.clone());
    Rig {
        capture,
        slot,
        handle,
        writer,
        _dir: dir,
        db_path,
    }
}

/// The observation columns of the one persisted row.
#[derive(Debug, PartialEq, Eq)]
struct ObservedRow {
    counts: [Option<i64>; 5],
    probe: Option<i64>,
    operation: Option<String>,
    alias: String,
    model: Option<String>,
    upstream: Option<String>,
    provider: Option<String>,
    provider_kind: Option<String>,
    seat: Option<String>,
}

impl Rig {
    /// Finalize the capture by dropping it (or by `finish`), flush the
    /// writer, and read the row back.
    fn persist(self, finish: impl FnOnce(UsageCapture)) -> ObservedRow {
        let Self {
            capture,
            slot: _,
            handle,
            writer,
            _dir,
            db_path,
        } = self;
        finish(capture);
        drop(handle);
        writer.shutdown();
        read_row(&db_path)
    }
}

fn read_row(db_path: &std::path::Path) -> ObservedRow {
    let conn = rusqlite::Connection::open(db_path).expect("read open");
    conn.query_row(
        "SELECT wire_losses, translation_drops, local_refusals, field_repairs, \
                missing_terminals, probe, operation, alias, model, upstream, provider, \
                provider_kind, seat \
         FROM requests WHERE request_id = ?1",
        [ROW_ID],
        |r| {
            Ok(ObservedRow {
                counts: [r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?],
                probe: r.get(5)?,
                operation: r.get(6)?,
                alias: r.get(7)?,
                model: r.get(8)?,
                upstream: r.get(9)?,
                provider: r.get(10)?,
                provider_kind: r.get(11)?,
                seat: r.get(12)?,
            })
        },
    )
    .expect("exactly one persisted row")
}

fn finalize_ok(mut capture: UsageCapture) {
    capture.finalize(Outcome::Ok);
}

/// Persist a row after `bump` touched the slot; return its five counts.
fn counts_after(bump: impl FnOnce(&RequestObservation)) -> [Option<i64>; 5] {
    let rig = rig();
    bump(&rig.slot);
    rig.persist(finalize_ok).counts
}

#[test]
fn a_wire_loss_on_the_slot_persists_in_wire_losses_only() {
    let counts = counts_after(|slot| slot.add_wire_losses(1));

    assert_eq!(counts, [Some(1), Some(0), Some(0), Some(0), Some(0)]);
}

#[test]
fn a_translation_drop_on_the_slot_persists_in_translation_drops_only() {
    let counts = counts_after(|slot| {
        slot.note_translation_drop("lane", "class");
    });

    assert_eq!(counts, [Some(0), Some(1), Some(0), Some(0), Some(0)]);
}

#[test]
fn a_local_refusal_on_the_slot_persists_in_local_refusals_only() {
    let counts = counts_after(RequestObservation::add_local_refusal);

    assert_eq!(counts, [Some(0), Some(0), Some(1), Some(0), Some(0)]);
}

#[test]
fn a_field_repair_on_the_slot_persists_in_field_repairs_only() {
    let counts = counts_after(|slot| slot.set_field_repairs(1));

    assert_eq!(counts, [Some(0), Some(0), Some(0), Some(1), Some(0)]);
}

#[test]
fn a_missing_terminal_on_the_slot_persists_in_missing_terminals_only() {
    let counts = counts_after(RequestObservation::add_missing_terminal);

    assert_eq!(counts, [Some(0), Some(0), Some(0), Some(0), Some(1)]);
}

#[test]
fn an_untouched_slot_persists_zero_counts_no_probe_and_an_inference() {
    let row = rig().persist(finalize_ok);

    assert_eq!(row.counts, [Some(0); 5], "zero, not NULL");
    assert_eq!(row.probe, Some(0));
    assert_eq!(row.operation.as_deref(), Some("inference"));
}

#[test]
fn a_probe_slot_persists_probe_one() {
    let rig = rig();
    rig.slot.set_probe(true);

    let row = rig.persist(finalize_ok);

    assert_eq!(row.probe, Some(1));
}

fn last_target() -> ServedIdentity {
    ServedIdentity {
        alias: Some("slot-alias".to_string()),
        model: Some("slot-model".to_string()),
        upstream: Some("slot-upstream".to_string()),
        provider: Some("slot-provider".to_string()),
        provider_kind: Some("slot-kind".to_string()),
        seat: Some("slot-seat".to_string()),
    }
}

#[test]
fn a_dropped_capture_without_meta_persists_the_slots_last_target() {
    let rig = rig();
    rig.slot.set_last_target(last_target());

    let row = rig.persist(drop);

    assert_eq!(
        (
            row.alias.as_str(),
            row.model.as_deref(),
            row.upstream.as_deref(),
            row.provider.as_deref(),
            row.provider_kind.as_deref(),
            row.seat.as_deref(),
        ),
        (
            "slot-alias",
            Some("slot-model"),
            Some("slot-upstream"),
            Some("slot-provider"),
            Some("slot-kind"),
            Some("slot-seat"),
        )
    );
}

#[test]
fn a_dropped_capture_with_an_empty_slot_keeps_the_requested_alias() {
    let row = rig().persist(drop);

    assert_eq!(row.alias, "requested");
    assert_eq!(row.provider, None);
}

#[tokio::test]
async fn observed_meta_identity_wins_over_the_slots_last_target() {
    // Arrange: the meta names the target that served; the slot names a
    // different one, so the row shows which source won.
    let meta = super::tests::any_dispatch_meta().await;
    let mut rig = rig();
    rig.slot.set_last_target(last_target());
    rig.capture.observe_meta(&meta, 0, 0);

    // Act
    let row = tokio::task::spawn_blocking(move || rig.persist(finalize_ok))
        .await
        .expect("persist task");

    // Assert
    assert!(
        meta.served_provider.is_some(),
        "the meta names a served target"
    );
    assert_eq!(row.alias, meta.resolved_alias);
    assert_eq!(row.provider, meta.served_provider);
    assert_eq!(row.upstream, meta.served_upstream);
    assert_eq!(row.seat, meta.served_seat);
}
