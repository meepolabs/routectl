//! The naming convention is a CONTRACT between two writers (the migration
//! pass and the login writer), so these tests pin the exact generated
//! strings and every refusal class rather than round-tripping.

use super::{
    SeatNamingError, account_entry_name, growth_pools_for_family, plan_new_seat,
    plan_pool_materialization, pool_name, seat_secret_ref,
};
use crate::config::Config;

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("fixture config parses")
}

/// A config with the two-account anthropic shape the convention produces.
fn two_account_config(accepts_new_logins: bool) -> Config {
    parse(&format!(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\", \"anthropic-work\"]\n\
         accepts_new_logins = {accepts_new_logins}\n"
    ))
}

#[test]
fn pool_takes_the_plain_family_name() {
    assert_eq!(pool_name("anthropic").unwrap(), "anthropic");
}

#[test]
fn default_seat_takes_the_default_suffix_and_labelled_seats_take_the_label() {
    // Arrange / Act / Assert
    assert_eq!(
        account_entry_name("anthropic", None).unwrap(),
        "anthropic-default"
    );
    assert_eq!(
        account_entry_name("anthropic", Some("work")).unwrap(),
        "anthropic-work"
    );
}

#[test]
fn seat_refs_are_bare_for_the_default_seat_and_pinned_for_a_label() {
    assert_eq!(seat_secret_ref("anthropic", None), "oauth://anthropic");
    assert_eq!(
        seat_secret_ref("anthropic", Some("work")),
        "oauth://anthropic#work"
    );
}

/// No lossy normalization: a label carrying a character a config key
/// cannot hold verbatim is refused, never rewritten into something that
/// no longer identifies the seat.
#[test]
fn an_unusable_label_is_refused_rather_than_normalized() {
    // Arrange
    let hostile = ["my seat", "seat.b", "seat#b", "", "seat/b", "se\u{e4}t"];

    // Act / Assert
    for label in hostile {
        let err = account_entry_name("anthropic", Some(label))
            .expect_err("an unusable label must refuse");
        assert!(
            matches!(
                err,
                SeatNamingError::UnusableToken {
                    kind: "seat label",
                    ..
                }
            ),
            "label `{label}` must refuse as an unusable token; got {err:?}"
        );
    }
}

#[test]
fn an_unusable_family_is_refused_for_both_the_pool_and_the_account() {
    // Arrange / Act / Assert
    assert!(matches!(
        pool_name("anth ropic").expect_err("refuses"),
        SeatNamingError::UnusableToken {
            kind: "provider family",
            ..
        }
    ));
    assert!(matches!(
        account_entry_name("anth.ropic", None).expect_err("refuses"),
        SeatNamingError::UnusableToken {
            kind: "provider family",
            ..
        }
    ));
}

/// The literal label `default` would generate the default seat's own entry
/// name, aliasing two distinct credentials onto one entry.
#[test]
fn the_label_default_is_reserved() {
    let err = account_entry_name("anthropic", Some("default")).expect_err("refuses");
    assert!(
        matches!(err, SeatNamingError::ReservedLabel { ref label } if label == "default"),
        "got {err:?}"
    );
}

#[test]
fn materialization_names_the_pool_and_every_account_in_request_order() {
    // Arrange
    let config = Config::default();

    // Act
    let plan = plan_pool_materialization(&config, "anthropic", [None, Some("work")])
        .expect("materialization plans");

    // Assert
    assert_eq!(plan.pool_name, "anthropic");
    assert!(!plan.pool_exists);
    let names: Vec<&str> = plan
        .accounts
        .iter()
        .map(|a| a.entry_name.as_str())
        .collect();
    assert_eq!(names, ["anthropic-default", "anthropic-work"]);
    let refs: Vec<&str> = plan
        .accounts
        .iter()
        .map(|a| a.secret_ref.as_str())
        .collect();
    assert_eq!(refs, ["oauth://anthropic", "oauth://anthropic#work"]);
    assert!(plan.accounts.iter().all(|a| !a.already_present));
}

/// Re-deriving over a config the convention already produced is a no-op:
/// every account reports `already_present` and the pool reports existing.
/// This is what makes the migration pass and the login writer agree.
#[test]
fn re_deriving_over_its_own_output_reports_everything_already_present() {
    // Arrange
    let config = two_account_config(true);

    // Act
    let plan = plan_pool_materialization(&config, "anthropic", [None, Some("work")])
        .expect("materialization plans");

    // Assert
    assert!(plan.pool_exists);
    assert!(plan.accounts.iter().all(|a| a.already_present));
}

/// A generated entry name held by an entry with a DIFFERENT credential is
/// a refusal: writing it would repoint an unrelated entry's credential.
#[test]
fn a_generated_entry_name_held_by_an_unrelated_entry_refuses() {
    // Arrange
    let config = parse(
        "[providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://SOMETHING_ELSE\"\n",
    );

    // Act
    let err = plan_pool_materialization(&config, "anthropic", [Some("work")]).expect_err("refuses");

    // Assert
    assert!(
        matches!(err, SeatNamingError::EntryNameTaken { ref name } if name == "anthropic-work"),
        "got {err:?}"
    );
}

/// Two labels can only generate one name when one of them is `default`,
/// which the reserved-label rule already refuses -- so the duplicate class
/// is reached through a repeated label, and it must still refuse rather
/// than silently collapsing two seats into one entry.
#[test]
fn two_seats_generating_one_name_refuse() {
    // Arrange
    let config = Config::default();

    // Act
    let err = plan_pool_materialization(&config, "anthropic", [Some("work"), Some("work")])
        .expect_err("refuses");

    // Assert
    assert!(
        matches!(
            err,
            SeatNamingError::DuplicateGeneratedName { ref name } if name == "anthropic-work"
        ),
        "got {err:?}"
    );
}

/// Providers, pools and model nicknames share one namespace on a
/// `[models.X] provider` value, so a pool name held by either refuses.
#[test]
fn a_pool_name_held_by_a_provider_or_a_nickname_refuses() {
    // Arrange
    let provider_holder = parse(
        "[providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    );
    let nickname_holder = parse(
        "[models.anthropic]\n\
         provider = \"p\"\n\
         upstream = \"claude\"\n",
    );

    // Act / Assert
    for config in [provider_holder, nickname_holder] {
        let err = plan_pool_materialization(&config, "anthropic", [None]).expect_err("refuses");
        assert!(
            matches!(err, SeatNamingError::PoolNameTaken { ref name } if name == "anthropic"),
            "got {err:?}"
        );
    }
}

#[test]
fn a_new_seat_joins_the_one_growth_marked_pool_of_its_family() {
    // Arrange
    let config = two_account_config(true);

    // Act
    let placement = plan_new_seat(&config, "anthropic", Some("laptop")).expect("placement plans");

    // Assert
    assert_eq!(placement.account.entry_name, "anthropic-laptop");
    assert_eq!(placement.account.secret_ref, "oauth://anthropic#laptop");
    assert!(!placement.account.already_present);
    assert_eq!(placement.pool_name.as_deref(), Some("anthropic"));
    assert!(!placement.already_member);
}

/// A pinned pool (no growth marker) is never auto-joined: the placement
/// carries no pool, and the caller proposes creating one instead.
#[test]
fn a_pinned_pool_is_not_offered_to_a_new_seat() {
    // Arrange
    let config = two_account_config(false);

    // Act
    let placement = plan_new_seat(&config, "anthropic", Some("laptop")).expect("placement plans");

    // Assert
    assert_eq!(placement.pool_name, None);
    assert!(growth_pools_for_family(&config, "anthropic").is_empty());
}

/// An existing seat re-planned against the config it already lives in is
/// fully idempotent: the entry is present and the pool already holds it.
#[test]
fn re_planning_an_existing_seat_reports_it_present_and_already_a_member() {
    // Arrange
    let config = two_account_config(true);

    // Act
    let placement = plan_new_seat(&config, "anthropic", Some("work")).expect("placement plans");

    // Assert
    assert!(placement.account.already_present);
    assert!(placement.already_member);
}

/// Two growth-marked pools serving one family leave the destination
/// undetermined by config, so the placement refuses rather than picking.
#[test]
fn two_growth_marked_pools_for_one_family_refuse_as_ambiguous() {
    // Arrange
    let config = parse(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [pools.first]\n\
         members = [\"anthropic-default\"]\n\
         accepts_new_logins = true\n\
         [pools.second]\n\
         members = [\"anthropic-work\"]\n\
         accepts_new_logins = true\n",
    );

    // Act
    let err = plan_new_seat(&config, "anthropic", Some("laptop")).expect_err("refuses");

    // Assert
    assert!(
        matches!(err, SeatNamingError::AmbiguousPool { ref pools } if pools == &["first", "second"]),
        "got {err:?}"
    );
}

/// A growth-marked pool whose members authenticate against a DIFFERENT
/// family does not serve this one -- the match is by credential ref, not by
/// pool name.
#[test]
fn a_growth_marked_pool_of_another_family_is_not_offered() {
    // Arrange
    let config = parse(
        "[providers.codex-default]\n\
         kind = \"openai-responses\"\n\
         api_key_ref = \"oauth://codex\"\n\
         [pools.codex]\n\
         members = [\"codex-default\"]\n\
         accepts_new_logins = true\n",
    );

    // Act / Assert
    assert!(growth_pools_for_family(&config, "anthropic").is_empty());
    assert_eq!(growth_pools_for_family(&config, "codex"), ["codex"]);
}

/// The derivation reads config only: no credential-store handle, no
/// filesystem access. Pinned by giving it a config naming a family that
/// no store could hold and asserting names still derive.
#[test]
fn derivation_is_pure_over_config_and_needs_no_store() {
    // Arrange
    let config = Config::default();

    // Act
    let plan = plan_pool_materialization(&config, "never-logged-in", [None, Some("x")])
        .expect("names derive without a store");

    // Assert
    assert_eq!(plan.pool_name, "never-logged-in");
    assert_eq!(plan.accounts[0].entry_name, "never-logged-in-default");
    assert_eq!(plan.accounts[1].entry_name, "never-logged-in-x");
}

/// Every refusal renders a message naming the offending token, and none
/// of them leak a credential ref.
#[test]
fn refusal_messages_name_the_problem_without_a_credential() {
    // Arrange
    let errors = [
        SeatNamingError::UnusableToken {
            kind: "seat label",
            token: "my seat".into(),
        },
        SeatNamingError::ReservedLabel {
            label: "default".into(),
        },
        SeatNamingError::DuplicateGeneratedName {
            name: "anthropic-work".into(),
        },
        SeatNamingError::EntryNameTaken {
            name: "anthropic-work".into(),
        },
        SeatNamingError::PoolNameTaken {
            name: "anthropic".into(),
        },
        SeatNamingError::AmbiguousPool {
            pools: vec!["a".into(), "b".into()],
        },
    ];

    // Act / Assert
    for err in errors {
        let rendered = err.to_string();
        assert!(!rendered.is_empty(), "{err:?} must render");
        assert!(!rendered.contains("oauth://"), "{rendered}");
    }
}
