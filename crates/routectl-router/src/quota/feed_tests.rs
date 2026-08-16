//! Tests for the dual-path feed.
//!
//! The two that matter most are the streaming ones. Streaming quota rides the
//! FIRST chunk only, so a feed that read it per chunk, or waited for
//! end-of-stream, would store nothing on the path a real client actually uses --
//! and nothing would look wrong, because an empty store reads as a fleet of
//! seats that have not reported yet.

use super::*;

use std::time::{Duration, Instant, SystemTime};

use routectl_core::upstream_meta::{AnthropicUnifiedQuota, UpstreamMeta};

use crate::quota::key::seat_key_for_secret_ref;
use crate::quota::window::QuotaWindow;

/// A real captured 5h reset in the epoch SECONDS the family reports.
const CAPTURED_5H_RESET_SECS: u64 = 1_781_001_000;

fn seat() -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: Some("seat-b".to_string()),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

fn store() -> Arc<QuotaStore> {
    let store = Arc::new(QuotaStore::default());
    store.admit_seats([seat()]);
    store
}

/// The Anthropic family as the shipped header parser produces it, with a reset
/// far enough ahead of the wall clock at test time to be plausible for a 5h
/// window. Built from a duration off `SystemTime::now` rather than the captured
/// absolute instant, since the feed stamps at `now` and a captured instant is
/// long past.
fn live_anthropic(utilization: &str) -> AnthropicUnifiedQuota {
    let reset_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs()
        + 3_600;
    let mut quota = AnthropicUnifiedQuota::default();
    quota.utilization = Some(utilization.to_string());
    quota.extras = vec![("5h-reset".into(), reset_secs.to_string())];
    quota
}

fn meta(utilization: &str) -> UpstreamMeta {
    UpstreamMeta::from_anthropic_unified(live_anthropic(utilization))
}

fn response_with(meta: Option<UpstreamMeta>) -> ChatResponse {
    ChatResponse {
        id: "resp".into(),
        model: "opus".into(),
        created: 0,
        choices: vec![],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: meta,
    }
}

fn chunk_with(meta: Option<UpstreamMeta>) -> ChatChunk {
    ChatChunk {
        id: "chunk".into(),
        model: "opus".into(),
        choices: vec![],
        usage: None,
        opaque_events: vec![],
        upstream_meta: meta,
    }
}

fn stored_fraction(store: &QuotaStore) -> Option<f64> {
    let reading = store.reading_for(&seat(), &ObservationStamp::now())?;
    match reading.fast {
        QuotaWindow::Known { utilization, .. } => Some(utilization.fraction()),
        QuotaWindow::Unknown => None,
    }
}

// ---- Non-streaming path ----

#[test]
fn a_non_streaming_response_feeds_the_served_seat() {
    let store = store();

    feed_response(&store, Some(&seat()), &response_with(Some(meta("0.21"))));

    assert_eq!(stored_fraction(&store), Some(0.21));
}

#[test]
fn a_non_streaming_response_with_no_metadata_feeds_nothing() {
    let store = store();

    feed_response(&store, Some(&seat()), &response_with(None));

    assert!(store.is_empty());
}

/// `served_seat`'s documented absent cases -- a pre-dispatch failure, a
/// non-OAuth credential, a forwarded client credential -- are all correct skips:
/// none of them is an account whose subscription budget routectl owns.
#[test]
fn a_response_with_no_served_seat_feeds_nothing() {
    let store = store();

    feed_response(&store, None, &response_with(Some(meta("0.21"))));

    assert!(store.is_empty());
}

// ---- Streaming path ----

#[test]
fn a_stream_feeds_from_the_first_chunk_carrying_metadata() {
    let store = store();
    let mut feed = FirstChunkFeed::armed(store.clone(), Some(seat()));

    feed.offer(&chunk_with(Some(meta("0.33"))));

    assert_eq!(stored_fraction(&store), Some(0.33));
}

/// The chunk field is documented as set only on the first canonical chunk, and
/// a feed must not treat a later content chunk's absence as an observation.
#[test]
fn a_stream_feeds_nothing_from_chunks_carrying_no_metadata() {
    let store = store();
    let mut feed = FirstChunkFeed::armed(store.clone(), Some(seat()));

    feed.offer(&chunk_with(None));
    feed.offer(&chunk_with(None));

    assert!(store.is_empty());
}

/// Exactly once per RESPONSE. A per-chunk feed would re-stamp the same reading
/// for every chunk of the stream, which resets the age ceiling the second
/// freshness bound rests on.
#[test]
fn a_stream_feeds_exactly_once_however_many_chunks_carry_metadata() {
    let store = store();
    let mut feed = FirstChunkFeed::armed(store.clone(), Some(seat()));

    feed.offer(&chunk_with(Some(meta("0.10"))));
    feed.offer(&chunk_with(Some(meta("0.90"))));
    feed.offer(&chunk_with(Some(meta("0.95"))));

    assert_eq!(
        stored_fraction(&store),
        Some(0.10),
        "the reading comes off the FIRST chunk; later chunks must not re-feed"
    );
}

#[test]
fn a_stream_on_a_seatless_target_is_inert() {
    let store = store();
    let mut feed = FirstChunkFeed::armed(store.clone(), None);

    feed.offer(&chunk_with(Some(meta("0.33"))));

    assert!(store.is_empty());
}

// ---- The milliseconds-as-seconds fixture, store half ----

/// A real captured reset multiplied by 1000 is a seconds field carrying
/// milliseconds. It places the reset tens of thousands of years out, which every
/// expiry check reads as PERMANENTLY VALID -- so a seat at low utilization would
/// attract every new session forever. The reducer refuses it; this pins the
/// consequence one level up, that NO `Known` window enters the store.
#[test]
fn a_millis_scale_reset_puts_no_known_window_in_the_store() {
    let store = store();
    let mut quota = AnthropicUnifiedQuota::default();
    quota.utilization = Some("0.02".into());
    quota.extras = vec![(
        "5h-reset".into(),
        (CAPTURED_5H_RESET_SECS * 1_000).to_string(),
    )];

    feed_response(
        &store,
        Some(&seat()),
        &response_with(Some(UpstreamMeta::from_anthropic_unified(quota))),
    );

    let reading = store
        .reading_for(&seat(), &ObservationStamp::now())
        .expect("the observation itself is recorded");
    assert_eq!(
        reading.fast,
        QuotaWindow::Unknown,
        "an implausible reset must leave the seat reading as no evidence, NOT \
         as permanently fresh at low utilization"
    );
    assert_eq!(
        store.rejection_totals().implausible_reset,
        1,
        "the refusal must be counted by its own reason"
    );
}

#[test]
fn an_uninterpretable_utilization_is_counted_as_such() {
    let store = store();
    let mut quota = live_anthropic("not-a-number");

    quota.utilization = Some("not-a-number".into());
    feed_response(
        &store,
        Some(&seat()),
        &response_with(Some(UpstreamMeta::from_anthropic_unified(quota))),
    );

    assert_eq!(store.rejection_totals().invalid_utilization, 1);
    assert_eq!(store.rejection_totals().implausible_reset, 0);
}

/// A carrier holding neither vendor family is not an observation. Merging it
/// would advance the stored observation instant, which is what ORDERS later
/// readings -- so an empty carrier could make a genuinely newer reading look
/// older than itself.
#[test]
fn a_carrier_with_no_vendor_family_stores_nothing() {
    let store = store();
    let empty = UpstreamMeta::default();

    feed_response(&store, Some(&seat()), &response_with(Some(empty)));

    assert!(store.is_empty());
}

/// Guard against a stamp taken too late: the feed stamps where the metadata is
/// READ, so a reading is fresh immediately after being fed.
#[test]
fn a_freshly_fed_reading_is_effective_at_once() {
    let store = store();

    feed_response(&store, Some(&seat()), &response_with(Some(meta("0.21"))));

    let reading = store
        .reading_for(
            &seat(),
            &ObservationStamp::from_parts(
                SystemTime::now(),
                Instant::now() + Duration::from_secs(1),
            ),
        )
        .expect("a reading");
    assert!(matches!(reading.fast, QuotaWindow::Known { .. }));
}
