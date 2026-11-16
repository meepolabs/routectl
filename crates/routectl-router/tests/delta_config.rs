//! Delta-config semantics pinned through a real TOML load.
//!
//! There is no config-layer merge engine: a single user `config.toml` is
//! the whole configuration. The layering the schema contract promises is
//! realized by two EXISTING mechanisms, and this suite pins both against
//! the contract wording so a future change cannot quietly regress them:
//!
//! - a per-class MAP (`retry.classes`) of same-target retry counts -- an
//!   absent class inherits the baked class default, a present class
//!   overrides only its own retry cap, and each class key is independent
//!   (`RetryPolicy::resolved_class`);
//! - plain serde for Vec fields (`capability.essential`) -- a list is one
//!   atomic value taken wholesale from the file, never element-merged.
//!
//! Contract rules pinned:
//!   1. maps override one class key without restating the rest;
//!   2. an absent class key inherits the baked default;
//!   3. maps merge per-key, Vecs replace whole;
//!   4. an empty table carries no semantic meaning (absent == empty).

use routectl_core::failure_class::FailureClass;
use routectl_router::parse_config;

/// A `[retry]` block with a non-default `max_attempts`, so a resolved cap
/// reveals whether the baked default or a class count fed it. Baked
/// outcomes under this block: the five health classes -> (6, true),
/// auth -> (0, true).
const RETRY_BLOCK: &str = "
[retry]
max_attempts = 6
";

#[test]
fn one_class_count_changes_only_that_class_others_stay_baked() {
    // Arrange: a count on server-error only.
    let toml_text = format!(
        "{RETRY_BLOCK}
[retry.classes]
server-error = 4
"
    );

    // Act
    let cfg = parse_config(&toml_text).expect("valid config parses");

    // Assert: the one counted class changed...
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::ServerError),
        (4, true),
        "server-error retry cap is the count; its fallback stays built in"
    );
    // ...and every other class still resolves to its baked default.
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::RateLimited),
        (6, true),
        "a sibling class with no count is untouched"
    );
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::NetworkError),
        (6, true)
    );
    assert_eq!(cfg.retry.resolved_class(&FailureClass::Auth), (0, true));
}

#[test]
fn vec_field_is_taken_whole_from_the_file_never_element_merged() {
    // Arrange: two configs that each set capability.essential to a
    // different whole list, plus one that omits it entirely.
    let with_pair =
        parse_config("[capability]\nessential = [\"a\", \"b\"]\n").expect("valid config parses");
    let with_single =
        parse_config("[capability]\nessential = [\"c\"]\n").expect("valid config parses");
    let omitted = parse_config("[retry]\nmax_attempts = 6\n").expect("valid config parses");

    // Assert: each load holds EXACTLY its file's list -- no union with the
    // baked (empty) default, and no accumulation across loads.
    assert_eq!(with_pair.capability.essential, vec!["a", "b"]);
    assert_eq!(with_single.capability.essential, vec!["c"]);
    // An omitted Vec is the baked default (empty), not something merged in.
    assert!(omitted.capability.essential.is_empty());
}

#[test]
fn map_merges_per_key_sibling_keys_stay_independent() {
    // Arrange: counts on two DIFFERENT classes.
    let toml_text = format!(
        "{RETRY_BLOCK}
[retry.classes]
server-error = 4
rate-limited = 1
"
    );

    // Act
    let cfg = parse_config(&toml_text).expect("valid config parses");

    // Assert: each key sets only its own class.
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::ServerError),
        (4, true)
    );
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::RateLimited),
        (1, true)
    );
    // A class key present in neither entry stays fully baked.
    assert_eq!(
        cfg.retry.resolved_class(&FailureClass::Overloaded),
        (6, true)
    );
}

#[test]
fn empty_class_table_behaves_as_absent() {
    // Arrange: an empty [retry.classes] table vs a config that never names it.
    let with_empty_table = parse_config(&format!(
        "{RETRY_BLOCK}
[retry.classes]
"
    ))
    .expect("valid config parses");
    let without_table = parse_config(RETRY_BLOCK).expect("valid config parses");

    // Assert: both resolve server-error to the identical baked default --
    // the empty table adds no semantic weight.
    let baked = without_table
        .retry
        .resolved_class(&FailureClass::ServerError);
    assert_eq!(baked, (6, true));
    assert_eq!(
        with_empty_table
            .retry
            .resolved_class(&FailureClass::ServerError),
        baked,
        "an empty class table must be indistinguishable from an absent one"
    );
}
