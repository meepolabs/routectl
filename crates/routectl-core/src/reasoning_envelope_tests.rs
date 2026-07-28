//! Tests for [`super`] -- see `reasoning_envelope.rs`.

use super::*;

const SCHEME: &str = "openai-responses-mantle";
const ID: &str = "rs_68a1f4c0b2e94";

fn mantle_shaped_blob() -> String {
    let body: String = std::iter::repeat_n("AbC9xZ", 160).collect();
    format!("rsn_{body}")
}

fn round_trip(scheme_tag: &str, id: &str, blob: &str) -> Option<(String, String, String)> {
    let wrapped = wrap(scheme_tag, Some(id), blob);
    unwrap(&wrapped).map(|(s, i, b)| {
        (
            s.to_string(),
            i.expect("test ids are present").to_string(),
            b.to_string(),
        )
    })
}

fn expected(scheme_tag: &str, id: &str, blob: &str) -> Option<(String, String, String)> {
    Some((scheme_tag.to_string(), id.to_string(), blob.to_string()))
}

#[test]
fn round_trips_an_ascii_blob_byte_exact() {
    let blob = "plain ascii reasoning payload";

    assert_eq!(round_trip(SCHEME, ID, blob), expected(SCHEME, ID, blob));
}

#[test]
fn round_trips_a_base64url_blob_byte_exact() {
    let blob = "gAAAAABm-Xq7_9tZ0aQ3lKcE1vRr4Nn2xYwPsTuVbHjKlMnOpQrStUvW==";

    assert_eq!(round_trip(SCHEME, ID, blob), expected(SCHEME, ID, blob));
}

#[test]
fn round_trips_a_mantle_shaped_blob_byte_exact() {
    let blob = mantle_shaped_blob();
    assert!((950..=1240).contains(&blob.len()));

    let wrapped = wrap(SCHEME, Some(ID), &blob);
    let (scheme, id, unwrapped) = unwrap(&wrapped).expect("mantle-shaped envelope parses");

    assert_eq!(scheme, SCHEME);
    assert_eq!(id, Some(ID));
    assert_eq!(unwrapped, blob);
    assert_eq!(unwrapped.as_bytes(), blob.as_bytes());
}

#[test]
fn round_trips_an_artifact_with_no_id_preserving_its_scheme() {
    let blob = mantle_shaped_blob();

    let wrapped = wrap(SCHEME, None, &blob);
    let (scheme, id, unwrapped) = unwrap(&wrapped).expect("id-less envelope parses");

    assert_eq!(scheme, SCHEME);
    assert_eq!(id, None);
    assert_eq!(unwrapped, blob);
}

#[test]
fn a_present_id_equal_to_the_sentinel_is_rejected_not_read_as_absent() {
    let blob = "payload";

    let wrapped = wrap(SCHEME, Some(ID_ABSENT), blob);

    assert_eq!(
        unwrap(&wrapped),
        None,
        "a real id colliding with the sentinel must not silently become absence"
    );
}

#[test]
fn rejects_an_over_long_scheme_or_id() {
    let blob = "payload";
    let long: String = std::iter::repeat_n('a', MAX_FIELD_BYTES + 1).collect();

    assert_eq!(unwrap(&wrap(&long, Some(ID), blob)), None);
    assert_eq!(unwrap(&wrap(SCHEME, Some(&long), blob)), None);

    let at_limit: String = std::iter::repeat_n('a', MAX_FIELD_BYTES).collect();
    assert!(unwrap(&wrap(&at_limit, Some(ID), blob)).is_some());
}

#[test]
fn round_trips_a_blob_containing_the_separator() {
    let blob = "seg.ment.ed.payload.";

    assert_eq!(round_trip(SCHEME, ID, blob), expected(SCHEME, ID, blob));
}

#[test]
fn round_trips_a_multibyte_blob_byte_exact() {
    let blob = "payload with multibyte: \u{00e9}\u{4e2d}\u{1f600}";

    assert_eq!(round_trip(SCHEME, ID, blob), expected(SCHEME, ID, blob));
}

#[test]
fn round_trips_every_probed_blob_family_byte_exact() {
    for prefix in SEPARATOR_ABSENT_FROM_PROBED_BLOBS {
        let blob = format!("{prefix}{}", "Ab9xZq".repeat(80));

        let wrapped = wrap(SCHEME, Some(ID), &blob);
        let (_, _, unwrapped) = unwrap(&wrapped).expect("probed-family envelope parses");

        assert_eq!(unwrapped, blob, "family {prefix} must survive byte-exact");
    }
}

#[test]
fn rejects_empty_input() {
    assert_eq!(unwrap(""), None);
}

#[test]
fn rejects_an_unwrapped_provider_blob() {
    assert_eq!(unwrap(&mantle_shaped_blob()), None);
}

#[test]
fn rejects_a_truncated_envelope() {
    assert_eq!(
        unwrap("rctl1.openai-responses-mantle.rs_68a1f4c0b2e94"),
        None
    );
    assert_eq!(unwrap("rctl1.openai-responses-mantle"), None);
    assert_eq!(unwrap("rctl1"), None);
}

#[test]
fn rejects_an_unknown_version_prefix() {
    let wrapped = wrap(SCHEME, Some(ID), "payload");

    assert_eq!(unwrap(&wrapped.replacen(VERSION_1, "rctl2", 1)), None);
    assert_eq!(unwrap(&wrapped.replacen(VERSION_1, "rctl0", 1)), None);
    assert_eq!(unwrap(&wrapped.replacen(VERSION_1, "rctl1x", 1)), None);
    assert_eq!(unwrap(&wrapped.replacen(VERSION_1, "", 1)), None);
}

#[test]
fn rejects_a_corrupted_separator() {
    let corrupted = wrap(SCHEME, Some(ID), "payload").replacen(SEPARATOR, ":", 1);

    assert_eq!(unwrap(&corrupted), None);
}

#[test]
fn rejects_an_empty_blob() {
    assert_eq!(round_trip(SCHEME, ID, ""), None);
}

#[test]
fn rejects_a_non_token_scheme_or_id() {
    assert_eq!(round_trip("", ID, "payload"), None);
    assert_eq!(round_trip("scheme with space", ID, "payload"), None);
    assert_eq!(round_trip(SCHEME, "id\nwith\nnewline", "payload"), None);
    assert_eq!(round_trip("scheme/slash", ID, "payload"), None);
    assert_eq!(unwrap("rctl1.scheme..payload"), None);
}

#[test]
fn never_panics_on_adversarial_input() {
    let long_separators: String = std::iter::repeat_n(SEPARATOR, 4096).collect();
    let adversarial = [
        String::new(),
        SEPARATOR.to_string(),
        "...".to_string(),
        long_separators,
        "rctl1...".to_string(),
        "rctl1.\u{0}.\u{0}.\u{0}".to_string(),
        "\u{4e2d}\u{6587}.\u{4e2d}.\u{6587}.x".to_string(),
        "rctl1.a.b.\u{0}".to_string(),
        format!("{}rctl1.a.b.c", "\u{feff}"),
        std::iter::repeat_n('x', 100_000).collect(),
    ];

    for input in adversarial {
        let _ = unwrap(&input);
    }
}

#[test]
fn treats_a_hostile_claim_as_data_not_authority() {
    let hostile = wrap(
        "some-other-scheme",
        Some("rs_forged"),
        &mantle_shaped_blob(),
    );

    let (scheme, _, _) = unwrap(&hostile).expect("a well-formed hostile envelope still parses");

    assert_eq!(scheme, "some-other-scheme");
}
