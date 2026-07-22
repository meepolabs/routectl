//! `apply_remap` / `upstream_status_for_remap`: the pure classify ->
//! remap seam consulted at all three dispatch error arms.
use super::*;

fn native(class: FailureClass, matched_by: MatchedBy) -> ClassifiedFailure {
    ClassifiedFailure { class, matched_by }
}

#[test]
fn apply_remap_empty_map_is_a_no_op() {
    // Arrange: the default, no-op case -- an operator who never
    // configured `class_overrides` must see native classification
    // pass through unchanged, whatever the status.
    let cf = native(FailureClass::ServerError, MatchedBy::Status);
    let overrides = BTreeMap::new();

    // Act
    let (effective, remapped) = apply_remap(cf.clone(), Some(503), &overrides);

    // Assert
    assert_eq!(effective, cf);
    assert!(!remapped);
}

#[test]
fn apply_remap_no_matching_key_is_a_no_op() {
    // Arrange: overrides present, but not for this status.
    let cf = native(FailureClass::ServerError, MatchedBy::Status);
    let mut overrides = BTreeMap::new();
    overrides.insert(429, FailureClass::RateLimited);

    // Act
    let (effective, remapped) = apply_remap(cf.clone(), Some(503), &overrides);

    // Assert
    assert_eq!(effective, cf);
    assert!(!remapped);
}

#[test]
fn apply_remap_none_status_is_a_no_op_even_with_a_matching_key_present() {
    // Arrange: a non-upstream / status-0 error carries no status to
    // key on, so the override table is never consulted at all.
    let cf = native(FailureClass::NetworkError, MatchedBy::Status);
    let mut overrides = BTreeMap::new();
    overrides.insert(503, FailureClass::ContentPolicy);

    // Act
    let (effective, remapped) = apply_remap(cf.clone(), None, &overrides);

    // Assert
    assert_eq!(effective, cf);
    assert!(!remapped);
}

#[test]
fn apply_remap_matching_key_replaces_class_but_keeps_native_matched_by() {
    // Arrange: native classify(503) is ServerError matched_by Status;
    // the operator remaps 503 to ContentPolicy.
    let cf = native(FailureClass::ServerError, MatchedBy::Status);
    let mut overrides = BTreeMap::new();
    overrides.insert(503, FailureClass::ContentPolicy);

    // Act
    let (effective, remapped) = apply_remap(cf, Some(503), &overrides);

    // Assert: the class changed, but matched_by still describes HOW
    // the classifier reached its native decision, not the remap.
    assert_eq!(effective.class, FailureClass::ContentPolicy);
    assert_eq!(effective.matched_by, MatchedBy::Status);
    assert!(remapped);
}

#[test]
fn apply_remap_preserves_upstream_type_matched_by_on_a_lifted_native_class() {
    // Arrange: a native lift (matched_by = UpstreamType) still keeps
    // that provenance after a remap replaces the class.
    let cf = native(
        FailureClass::FeatureUnsupported {
            capability: "unsupported_parameter".to_string(),
        },
        MatchedBy::UpstreamType,
    );
    let mut overrides = BTreeMap::new();
    overrides.insert(400, FailureClass::BadRequest);

    // Act
    let (effective, remapped) = apply_remap(cf, Some(400), &overrides);

    // Assert
    assert_eq!(effective.class, FailureClass::BadRequest);
    assert_eq!(effective.matched_by, MatchedBy::UpstreamType);
    assert!(remapped);
}

#[test]
fn upstream_status_for_remap_extracts_in_range_upstream_status() {
    let err = Error::upstream("p", 503, "body");
    assert_eq!(upstream_status_for_remap(&err), Some(503));
}

#[test]
fn upstream_status_for_remap_none_for_status_zero() {
    let err = Error::upstream("p", 0, "body");
    assert_eq!(upstream_status_for_remap(&err), None);
}

#[test]
fn upstream_status_for_remap_none_for_out_of_range_status() {
    let err = Error::upstream("p", 600, "body");
    assert_eq!(upstream_status_for_remap(&err), None);
}

#[test]
fn upstream_status_for_remap_none_for_non_upstream_variant() {
    let err = Error::Streaming("boom".into());
    assert_eq!(upstream_status_for_remap(&err), None);
}
