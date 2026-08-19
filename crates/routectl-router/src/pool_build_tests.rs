//! Pure-shape tests for the pool build outcomes: the omission vocabulary, the
//! degraded / unavailable predicates, and the boot-refusal message.

use super::*;

fn omission(member: &str, reason: PoolOmissionReason) -> PoolMemberOmission {
    PoolMemberOmission {
        member: member.to_string(),
        provider_kind: "anthropic-api",
        reason,
    }
}

fn report(
    pool: &str,
    models: &[&str],
    usable: usize,
    omissions: Vec<PoolMemberOmission>,
) -> PoolReport {
    PoolReport {
        pool: pool.to_string(),
        models: models.iter().map(|m| (*m).to_string()).collect(),
        configured_members: usable + omissions.len(),
        usable_members: usable,
        omissions,
    }
}

#[test]
fn every_omission_reason_has_its_own_snake_case_token() {
    // The token vocabulary reaches a log field and an operator report, so a
    // collision would silently merge two distinct causes.
    let reasons = [
        PoolOmissionReason::CredentialMissing,
        PoolOmissionReason::CredentialUnreadable,
        PoolOmissionReason::CredentialInvalid,
        PoolOmissionReason::ProviderInitFailed,
    ];

    let tokens: Vec<&str> = reasons.iter().map(|r| r.token()).collect();

    assert_eq!(
        tokens,
        vec![
            "credential_missing",
            "credential_unreadable",
            "credential_invalid",
            "provider_init_failed",
        ]
    );
    let mut deduped = tokens.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), tokens.len(), "tokens must be distinct");
}

#[test]
fn a_pool_serving_survivors_reads_as_degraded_not_unavailable() {
    // Arrange: two configured members, one lost.
    let r = report(
        "anthropic-pool",
        &["opus"],
        1,
        vec![omission(
            "anthropic-b",
            PoolOmissionReason::CredentialUnreadable,
        )],
    );

    // Act / Assert
    assert!(r.is_degraded(), "one survivor plus one loss is degraded");
    assert!(
        !r.is_unavailable(),
        "a survivor means the pool still serves"
    );
    assert_eq!(r.configured_members, 2);
}

#[test]
fn a_fully_healthy_pool_is_neither_degraded_nor_unavailable() {
    let r = report("anthropic-pool", &["opus"], 2, Vec::new());

    assert!(!r.is_degraded());
    assert!(!r.is_unavailable());
}

#[test]
fn zero_usable_members_reads_as_unavailable() {
    let r = report(
        "anthropic-pool",
        &["opus"],
        0,
        vec![omission(
            "anthropic-a",
            PoolOmissionReason::CredentialMissing,
        )],
    );

    assert!(r.is_unavailable());
    assert!(
        !r.is_degraded(),
        "an unavailable pool is not a degraded one -- it serves nothing"
    );
}

#[test]
fn the_boot_refusal_names_both_the_pool_and_every_model_routed_at_it() {
    // The whole point of the refusal: an operator reading it must not have to
    // work out which model the dead pool was serving.
    let reports = vec![report(
        "anthropic-pool",
        &["opus", "sonnet"],
        0,
        vec![
            omission("anthropic-a", PoolOmissionReason::CredentialUnreadable),
            omission("anthropic-b", PoolOmissionReason::ProviderInitFailed),
        ],
    )];

    let detail = unavailable_pool_error(&reports).expect("a zero-usable pool must refuse");

    assert!(detail.contains("anthropic-pool"), "{detail}");
    assert!(detail.contains("opus"), "{detail}");
    assert!(detail.contains("sonnet"), "{detail}");
    assert!(detail.contains("credential_unreadable"), "{detail}");
    assert!(detail.contains("provider_init_failed"), "{detail}");
}

#[test]
fn a_degraded_pool_does_not_refuse_the_build() {
    let reports = vec![report(
        "anthropic-pool",
        &["opus"],
        1,
        vec![omission(
            "anthropic-b",
            PoolOmissionReason::CredentialInvalid,
        )],
    )];

    assert_eq!(
        unavailable_pool_error(&reports),
        None,
        "a degraded pool serves its survivors rather than failing boot"
    );
}

#[test]
fn an_unavailable_pool_no_model_names_does_not_refuse_the_build() {
    // A pool nothing routes at cannot break a route, so refusing boot over it
    // would ground a server for an unused block.
    let reports = vec![report(
        "unused-pool",
        &[],
        0,
        vec![omission(
            "anthropic-a",
            PoolOmissionReason::CredentialMissing,
        )],
    )];

    assert_eq!(unavailable_pool_error(&reports), None);
}

#[test]
fn the_refusal_lists_every_unavailable_pool_not_just_the_first() {
    let reports = vec![
        report(
            "pool-a",
            &["opus"],
            0,
            vec![omission("a-1", PoolOmissionReason::CredentialMissing)],
        ),
        report(
            "pool-b",
            &["sonnet"],
            0,
            vec![omission("b-1", PoolOmissionReason::CredentialMissing)],
        ),
    ];

    let detail = unavailable_pool_error(&reports).expect("both pools refuse");

    assert!(detail.contains("pool-a"), "{detail}");
    assert!(detail.contains("pool-b"), "{detail}");
    assert_eq!(
        detail.lines().count(),
        2,
        "one line per dead pool: {detail}"
    );
}

#[test]
fn a_ready_outcome_exposes_its_seats_and_an_unavailable_one_does_not() {
    let unavailable = PoolOutcome::Unavailable {
        omissions: vec![omission("a-1", PoolOmissionReason::CredentialMissing)],
    };

    assert!(unavailable.seats().is_none());
    assert_eq!(unavailable.omissions().len(), 1);
}
