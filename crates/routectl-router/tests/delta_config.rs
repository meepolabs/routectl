//! Delta-config semantics pinned through a real TOML load.
//!
//! There is no config-layer merge engine: a single user `config.toml` is
//! the whole configuration. The layering the schema contract promises is
//! realized by two EXISTING mechanisms, and this suite pins both against
//! the contract wording so a future change cannot quietly regress them:
//!
//! - a per-class MAP (`retry.classes`) whose leaves are `Option<T>` --
//!   an absent leaf inherits the baked class default, a present leaf
//!   overrides only itself, and each class key is independent
//!   (`RetryPolicy::resolved_class`);
//! - plain serde for Vec fields (`bedrock.allowed_betas`) -- a list is one
//!   atomic value taken wholesale from the file, never element-merged.
//!
//! Contract rules pinned (config-schema v3, layering section):
//!   1. maps override one class key without restating the rest;
//!   2. every `ClassPolicy` leaf is `Option`: absent = inherit;
//!   3. maps merge per-key, Vecs replace whole;
//!   4. an empty table carries no semantic meaning (absent == empty).

use routectl_core::failure_class::FailureClass;
use routectl_router::parse_config;

/// A `[retry]` block whose knobs are all distinct, so a resolved cap
/// reveals exactly which baked default fed it. Baked outcomes under this
/// block: server-error -> (2, true), rate-limited -> (1, true),
/// network-error -> (3, true), auth -> (0, true).
const DISTINCT_RETRY_BLOCK: &str = "
[retry]
max_attempts = 6
retry_on_429 = 1
retry_on_5xx = 2
retry_on_network = 3
";

#[test]
fn sparse_class_leaf_override_changes_only_that_leaf_others_stay_baked() {
    // Arrange: override only server-error's retry cap.
    let toml_text = format!(
        "{DISTINCT_RETRY_BLOCK}
[retry.classes.server-error]
retry = 4
"
    );

    // Act
    let cfg = parse_config(&toml_text).expect("valid config parses");

    // Assert: the one overridden leaf changed...
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::ServerError),
        (4, true),
        "server-error retry cap is the override; its fallback leaf stays baked"
    );
    // ...and every other class / leaf still resolves to its baked default.
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::RateLimited),
        (1, true),
        "a sibling class with no entry is untouched"
    );
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::NetworkError),
        (3, true)
    );
    assert_eq!(cfg.retry.resolved_class(&FailureClass::Auth), (0, true));
}

#[test]
fn vec_field_is_taken_whole_from_the_file_never_element_merged() {
    // Arrange: two configs that each set bedrock.allowed_betas to a
    // different whole list, plus one that omits it entirely.
    let with_pair =
        parse_config("[bedrock]\nallowed_betas = [\"a\", \"b\"]\n").expect("valid config parses");
    let with_single =
        parse_config("[bedrock]\nallowed_betas = [\"c\"]\n").expect("valid config parses");
    let omitted = parse_config("[retry]\nmax_attempts = 6\n").expect("valid config parses");

    // Assert: each load holds EXACTLY its file's list -- no union with the
    // baked (empty) default, and no accumulation across loads.
    assert_eq!(with_pair.bedrock.allowed_betas, vec!["a", "b"]);
    assert_eq!(with_single.bedrock.allowed_betas, vec!["c"]);
    // An omitted Vec is the baked default (empty), not something merged in.
    assert!(omitted.bedrock.allowed_betas.is_empty());
}

#[test]
fn map_merges_per_key_sibling_keys_stay_independent() {
    // Arrange: set two DIFFERENT class keys, each touching a different leaf.
    let toml_text = format!(
        "{DISTINCT_RETRY_BLOCK}
[retry.classes.server-error]
retry = 4

[retry.classes.rate-limited]
fallback = false
"
    );

    // Act
    let cfg = parse_config(&toml_text).expect("valid config parses");

    // Assert: writing server-error did not clobber rate-limited; each key
    // merges only its own leaf over the baked default.
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::ServerError),
        (4, true),
        "server-error: retry overridden, fallback still baked"
    );
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::RateLimited),
        (1, false),
        "rate-limited: fallback overridden, retry cap still the baked 1"
    );
    // A class key present in neither entry stays fully baked.
    assert_eq!(cfg.retry.resolved_class(&FailureClass::Auth), (0, true));
}

#[test]
fn empty_class_table_behaves_as_absent() {
    // Arrange: an empty [retry.classes.server-error] table (no leaves) vs a
    // config that never names the class at all.
    let with_empty_table = parse_config(&format!(
        "{DISTINCT_RETRY_BLOCK}
[retry.classes.server-error]
"
    ))
    .expect("valid config parses");
    let without_table = parse_config(DISTINCT_RETRY_BLOCK).expect("valid config parses");

    // Assert: both resolve server-error to the identical baked default --
    // the empty table adds no semantic weight.
    let baked = without_table
        .retry
        .resolved_class(&FailureClass::ServerError);
    assert_eq!(baked, (2, true));
    assert_eq!(
        with_empty_table
            .retry
            .resolved_class(&FailureClass::ServerError),
        baked,
        "an empty class table must be indistinguishable from an absent one"
    );
}
