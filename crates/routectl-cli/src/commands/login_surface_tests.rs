//! The planner's resolution table, pinned arm by arm.
//!
//! Reconciliation is the property under test: the HAND-NAMED case
//! (`plans_membership_not_a_second_entry_for_a_hand_named_entry`) is the
//! one a name-only implementation fails, and every refusal arm asserts
//! that nothing is proposed rather than that some plan came back.

use routectl_auth::SecretRef;
use routectl_router::Config;

use super::{NothingNote, PoolAction, RefuseReason, SurfacePlan, plan, ref_matches, render_delta};

fn parse(text: &str) -> Config {
    toml::from_str(text).expect("fixture config parses")
}

/// The two-account anthropic shape the convention produces, with the pool's
/// growth marker under the caller's control.
fn two_account_config(accepts_new_logins: bool) -> Config {
    parse(&format!(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.anthropic]\n\
         members = [\"anthropic-default\"]\n\
         accepts_new_logins = {accepts_new_logins}\n"
    ))
}

fn expect_write(plan: SurfacePlan) -> super::SurfaceWrite {
    match plan {
        SurfacePlan::Write(write) => write,
        other => panic!("expected a write plan, got {other:?}"),
    }
}

fn expect_refusal(plan: SurfacePlan) -> RefuseReason {
    match plan {
        SurfacePlan::Refuse(reason) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn matches_the_entry_carrying_the_exact_ref_and_no_neighbouring_seat() {
    // Arrange: the default seat's ref and a labelled seat's ref differ by
    // the fragment alone, so a prefix match would conflate them.
    let config = parse(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n",
    );

    // Act
    let default_seat = ref_matches(&config, &SecretRef::parse("oauth://anthropic").unwrap());
    let work_seat = ref_matches(
        &config,
        &SecretRef::parse("oauth://anthropic#work").unwrap(),
    );

    // Assert
    assert_eq!(default_seat, ["anthropic-default"]);
    assert_eq!(work_seat, ["anthropic-work"]);
}

#[test]
fn a_clean_config_plans_a_new_entry_and_creates_the_family_pool() {
    // Arrange
    let config = Config::default();

    // Act
    let write = expect_write(plan(&config, "anthropic", None).expect("plans"));

    // Assert: the convention's name, not the login id.
    assert_eq!(write.entry_name, "anthropic-default");
    assert!(write.new_entry.is_some());
    match &write.pool {
        PoolAction::Create { pool, members } => {
            assert_eq!(pool, "anthropic");
            assert_eq!(members, &["anthropic-default"]);
        }
        other => panic!("expected pool creation, got {other:?}"),
    }
}

#[test]
fn a_second_labelled_login_joins_the_growth_pool_without_recreating_it() {
    // Arrange
    let config = two_account_config(true);

    // Act
    let write = expect_write(plan(&config, "anthropic", Some("work")).expect("plans"));

    // Assert: the post-join member list, and NO growth marker to write.
    assert_eq!(write.entry_name, "anthropic-work");
    match &write.pool {
        PoolAction::Join { pool, members } => {
            assert_eq!(pool, "anthropic");
            assert_eq!(members, &["anthropic-default", "anthropic-work"]);
        }
        other => panic!("expected a join, got {other:?}"),
    }
}

#[test]
fn re_login_against_a_pooled_entry_plans_nothing() {
    // Arrange
    let config = two_account_config(true);

    // Act
    let plan = plan(&config, "anthropic", None).expect("plans");

    // Assert
    match plan {
        SurfacePlan::Nothing { entry_name, note } => {
            assert_eq!(entry_name, "anthropic-default");
            assert_eq!(note, None);
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

/// THE reconciliation case: the entry carrying the ref is hand-named, so
/// the convention's generated name (`anthropic-default`) is free. A planner
/// matching by NAME finds nothing there and proposes a SECOND entry for one
/// credential; matching by REF finds the operator's entry and proposes only
/// the pool membership it lacks.
#[test]
fn plans_membership_not_a_second_entry_for_a_hand_named_entry() {
    // Arrange
    let config = parse(
        "[providers.claude-sub]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    );

    // Act
    let write = expect_write(plan(&config, "anthropic", None).expect("plans"));

    // Assert: the operator's entry serves the seat, and no entry is minted.
    assert_eq!(write.entry_name, "claude-sub");
    assert!(
        write.new_entry.is_none(),
        "a second entry for one credential was planned: {write:?}"
    );
    match &write.pool {
        PoolAction::Create { pool, members } => {
            assert_eq!(pool, "anthropic");
            assert_eq!(members, &["claude-sub"]);
        }
        other => panic!("expected pool creation around the existing entry, got {other:?}"),
    }
    let rendered = render_delta(&SurfacePlan::Write(write));
    assert!(
        !rendered.contains("[providers."),
        "no provider entry may be rendered: {rendered}"
    );
}

/// The matched entry's pool placement must NOT be derived through the
/// convention's default-seat placement: that derivation also validates the
/// generated entry name, so an unrelated entry squatting
/// `<family>-default` refuses it -- and the one-match arm would then create
/// a second pool instead of joining the family's only growth pool, leaving
/// every later login ambiguous.
#[test]
fn a_squatted_generated_name_does_not_turn_a_required_join_into_a_pool_creation() {
    // Arrange: the seat's entry is hand-named and unpooled, the family has
    // exactly one growth pool under an operator's own name, and
    // `anthropic-default` is held by an entry with a DIFFERENT credential.
    let config = parse(
        "[providers.claude-work]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [providers.claude-main]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://SOMETHING_ELSE\"\n\
         [pools.team-anthropic]\n\
         members = [\"claude-main\"]\n\
         accepts_new_logins = true\n",
    );

    // Act
    let write = expect_write(plan(&config, "anthropic", Some("work")).expect("plans"));

    // Assert: joins the operator's growth pool, creates nothing.
    assert_eq!(write.entry_name, "claude-work");
    assert!(write.new_entry.is_none());
    match &write.pool {
        PoolAction::Join { pool, members } => {
            assert_eq!(pool, "team-anthropic");
            assert_eq!(members, &["claude-main", "claude-work"]);
        }
        other => panic!("expected a join of the family's only growth pool, got {other:?}"),
    }
    // The join must not carry the growth marker either -- the pool already
    // has one, and rewriting an operator's marker is never login's business.
    let rendered = render_delta(&SurfacePlan::Write(write));
    assert!(!rendered.contains("accepts_new_logins"), "{rendered}");
    assert!(!rendered.contains("[pools.anthropic]"), "{rendered}");
}

/// A matched entry listed by TWO pools: validation forbids it, but this
/// planner is pure over a config it did not gate, and picking a holder by
/// map order would let a pinned note render while the other pool is
/// growth-marked.
#[test]
fn an_entry_listed_by_two_pools_refuses_naming_both() {
    // Arrange: `[pools.aaa]` sorts first and is PINNED, `[pools.zzz]` is
    // growth-marked -- so a first-match implementation reports "nothing,
    // pinned" and hides the ambiguity.
    let config = parse(
        "[providers.claude-sub]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.aaa]\n\
         members = [\"claude-sub\"]\n\
         accepts_new_logins = false\n\
         [pools.zzz]\n\
         members = [\"claude-sub\"]\n\
         accepts_new_logins = true\n",
    );

    // Act
    let reason = expect_refusal(plan(&config, "anthropic", None).expect("plans"));

    // Assert
    match &reason {
        RefuseReason::EntryInMultiplePools { entry_name, pools } => {
            assert_eq!(entry_name, "claude-sub");
            assert_eq!(pools, &["aaa", "zzz"]);
        }
        other => panic!("expected a multiple-pools refusal, got {other:?}"),
    }
    let rendered = reason.to_string();
    assert!(rendered.contains("aaa"), "{rendered}");
    assert!(rendered.contains("zzz"), "{rendered}");
    assert!(rendered.contains("Nothing was written"), "{rendered}");
}

/// The hand-named entry already in a growth pool is fully idempotent -- the
/// same case one login later.
#[test]
fn a_hand_named_entry_already_pooled_plans_nothing() {
    // Arrange
    let config = parse(
        "[providers.claude-sub]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [pools.anthropic]\n\
         members = [\"claude-sub\"]\n\
         accepts_new_logins = true\n",
    );

    // Act / Assert
    match plan(&config, "anthropic", None).expect("plans") {
        SurfacePlan::Nothing { entry_name, note } => {
            assert_eq!(entry_name, "claude-sub");
            assert_eq!(note, None);
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

#[test]
fn two_entries_carrying_one_ref_refuse_and_name_both() {
    // Arrange
    let config = parse(
        "[providers.anthropic-default]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.claude-sub]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    );

    // Act
    let reason = expect_refusal(plan(&config, "anthropic", None).expect("plans"));

    // Assert
    match &reason {
        RefuseReason::AmbiguousEntries { candidates } => {
            assert_eq!(candidates, &["anthropic-default", "claude-sub"]);
        }
        other => panic!("expected ambiguous entries, got {other:?}"),
    }
    let rendered = reason.to_string();
    assert!(rendered.contains("anthropic-default"), "{rendered}");
    assert!(rendered.contains("claude-sub"), "{rendered}");
    assert!(rendered.contains("Nothing was written"), "{rendered}");
}

#[test]
fn an_entry_in_a_pinned_pool_plans_nothing_with_the_pinned_note() {
    // Arrange
    let config = two_account_config(false);

    // Act
    let plan = plan(&config, "anthropic", None).expect("plans");

    // Assert: the marker is the operator's, so login reports rather than
    // flips it.
    match plan {
        SurfacePlan::Nothing {
            note: Some(note), ..
        } => {
            assert_eq!(
                note,
                NothingNote::PinnedPool {
                    pool: "anthropic".into()
                }
            );
            let rendered = note.to_string();
            assert!(rendered.contains("accepts_new_logins"), "{rendered}");
        }
        other => panic!("expected nothing with a pinned note, got {other:?}"),
    }
}

#[test]
fn two_growth_pools_for_one_family_refuse_as_ambiguous() {
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

    // Act: a THIRD seat, so neither existing entry matches the ref.
    let reason = expect_refusal(plan(&config, "anthropic", Some("laptop")).expect("plans"));

    // Assert: the naming refusal verbatim.
    assert!(
        matches!(
            &reason,
            RefuseReason::Naming(routectl_router::seat_naming::SeatNamingError::AmbiguousPool {
                pools
            }) if pools == &["first", "second"]
        ),
        "got {reason:?}"
    );
}

#[test]
fn a_generated_entry_name_held_by_an_unrelated_entry_refuses() {
    // Arrange: the name `anthropic-work` is taken by an entry with a
    // different credential, so the labelled seat cannot take it.
    let config = parse(
        "[providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://SOMETHING_ELSE\"\n",
    );

    // Act
    let reason = expect_refusal(plan(&config, "anthropic", Some("work")).expect("plans"));

    // Assert
    assert!(
        matches!(
            &reason,
            RefuseReason::Naming(
                routectl_router::seat_naming::SeatNamingError::EntryNameTaken { name }
            ) if name == "anthropic-work"
        ),
        "got {reason:?}"
    );
}

#[test]
fn a_pool_name_held_by_a_provider_entry_refuses() {
    // Arrange: providers, pools and model nicknames share one namespace.
    let config = parse(
        "[providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://SOMETHING_ELSE\"\n",
    );

    // Act
    let reason = expect_refusal(plan(&config, "anthropic", Some("work")).expect("plans"));

    // Assert
    assert!(
        matches!(
            &reason,
            RefuseReason::Naming(
                routectl_router::seat_naming::SeatNamingError::PoolNameTaken { name }
            ) if name == "anthropic"
        ),
        "got {reason:?}"
    );
}

#[test]
fn an_unusable_label_refuses_as_an_unusable_token() {
    // Act
    let reason =
        expect_refusal(plan(&Config::default(), "anthropic", Some("my seat")).expect("plans"));

    // Assert
    assert!(
        matches!(
            &reason,
            RefuseReason::Naming(
                routectl_router::seat_naming::SeatNamingError::UnusableToken {
                    kind: "seat label",
                    ..
                }
            )
        ),
        "got {reason:?}"
    );
}

#[test]
fn the_reserved_label_default_refuses() {
    // Act
    let reason =
        expect_refusal(plan(&Config::default(), "anthropic", Some("default")).expect("plans"));

    // Assert
    assert!(
        matches!(
            &reason,
            RefuseReason::Naming(
                routectl_router::seat_naming::SeatNamingError::ReservedLabel { label }
            ) if label == "default"
        ),
        "got {reason:?}"
    );
}

/// A matched entry whose `kind` and auth selector do not match the shape
/// this provider needs: growing a pool around it would spread an egress
/// that authenticates on the wrong surface. The refusal names FIELDS only.
#[test]
fn auth_field_drift_on_the_matched_entry_refuses_naming_fields_only() {
    // Arrange: right credential, wrong kind AND wrong auth selector.
    let config = parse(
        "[providers.claude-sub]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://gateway.example/v1\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    );

    // Act
    let reason = expect_refusal(plan(&config, "anthropic", None).expect("plans"));

    // Assert
    match &reason {
        RefuseReason::AuthFieldDrift { entry_name, fields } => {
            assert_eq!(entry_name, "claude-sub");
            assert_eq!(fields, &["kind", "auth_kind"]);
        }
        other => panic!("expected auth drift, got {other:?}"),
    }
    let rendered = reason.to_string();
    assert!(rendered.contains("kind"), "{rendered}");
    assert!(
        !rendered.contains("openai-compat"),
        "the entry's current value leaked: {rendered}"
    );
    assert!(
        !rendered.contains("gateway.example"),
        "the entry's current value leaked: {rendered}"
    );
    assert!(rendered.contains("Nothing was written"), "{rendered}");
}

/// The auth selector alone drifting is caught too -- an `api-key` anthropic
/// entry consuming a subscription bearer 401s on every request.
#[test]
fn a_wrong_auth_selector_alone_refuses() {
    // Arrange
    let config = parse(
        "[providers.claude-sub]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"api-key\"\n\
         api_key_ref = \"oauth://anthropic\"\n",
    );

    // Act
    let reason = expect_refusal(plan(&config, "anthropic", None).expect("plans"));

    // Assert
    match &reason {
        RefuseReason::AuthFieldDrift { fields, .. } => assert_eq!(fields, &["auth_kind"]),
        other => panic!("expected auth drift, got {other:?}"),
    }
}

/// A provider whose variant carries NO auth-selector field has only its
/// `kind` checked -- inventing a selector requirement there would refuse
/// every correct xai entry (the entry is `deny_unknown_fields`).
#[test]
fn a_variant_without_an_auth_selector_is_checked_on_kind_alone() {
    // Arrange
    let config = parse(
        "[providers.grok]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://api.x.ai/v1\"\n\
         api_key_ref = \"oauth://xai\"\n",
    );

    // Act / Assert: a pool is proposed, not a refusal.
    let write = expect_write(plan(&config, "xai", None).expect("plans"));
    assert_eq!(write.entry_name, "grok");
}

#[test]
fn the_rendered_delta_carries_no_credential_material() {
    // Arrange: the needles login_provider_block's own render test uses,
    // plus the sentinel values a stored token record would hold.
    let needles = [
        "sk-ant-",
        "sk-",
        "ya29.",
        "eyJ",
        "access_token",
        "refresh_token",
        "Bearer ",
        "SENTINEL-ACCESS",
        "SENTINEL-REFRESH",
    ];

    // Act / Assert: every arm that renders anything, for every login id.
    for id in routectl_auth::oauth::known_provider_ids() {
        for label in [None, Some("seat-b")] {
            for config in [Config::default(), two_account_config(true)] {
                let rendered = render_delta(&plan(&config, id, label).expect("plans"));
                for needle in needles {
                    assert!(
                        !rendered.contains(needle),
                        "`{needle}` in delta for `{id}`: {rendered}"
                    );
                }
            }
        }
    }

    // A no-leak assertion over an EMPTY string is vacuous, so pin that the
    // arm which renders the most actually rendered: the entry header, the
    // pool block, and the credential REFERENCE the entry consumes.
    let created = render_delta(&plan(&Config::default(), "anthropic", None).expect("plans"));
    for required in [
        "[providers.anthropic-default]",
        r#"api_key_ref = "oauth://anthropic""#,
        "[pools.anthropic]",
        r#"members = ["anthropic-default"]"#,
    ] {
        assert!(
            created.contains(required),
            "the delta must carry `{required}`, else the needle scan is vacuous: {created}"
        );
    }
}

#[test]
fn a_created_pool_carries_the_growth_marker_and_a_joined_one_never_does() {
    // Arrange / Act
    let created = render_delta(&plan(&Config::default(), "anthropic", None).expect("plans"));
    let joined =
        render_delta(&plan(&two_account_config(true), "anthropic", Some("work")).expect("plans"));

    // Assert: login sets the marker only on a pool it creates -- flipping
    // an operator's existing marker is never login's business.
    assert!(created.contains("accepts_new_logins = true"), "{created}");
    assert!(
        !joined.contains("accepts_new_logins"),
        "a join must not write the marker: {joined}"
    );
    assert!(
        joined.contains(r#"members = ["anthropic-default", "anthropic-work"]"#),
        "{joined}"
    );
}

#[test]
fn the_rendered_entry_is_byte_identical_to_the_printed_fallback_block() {
    // Arrange: the fallback print and the shown delta are one renderer, so
    // the entry bytes must match exactly.
    let expected = crate::commands::login_provider_block::provider_block("anthropic", Some("work"))
        .expect("block")
        .render();

    // Act
    let rendered =
        render_delta(&plan(&Config::default(), "anthropic", Some("work")).expect("plans"));

    // Assert
    assert!(rendered.starts_with(&expected), "{rendered}");
}

#[test]
fn the_planned_delta_parses_and_validates_as_config() {
    // Arrange: the plan for a fresh anthropic login, plus the minimum a
    // config needs around it.
    let rendered = render_delta(&plan(&Config::default(), "anthropic", None).expect("plans"));

    // Act
    let config: Config = toml::from_str(&format!("version = 4\n{rendered}"))
        .unwrap_or_else(|e| panic!("planned delta must parse: {e}\n{rendered}"));

    // Assert
    let pool = config.pools.get("anthropic").expect("pool");
    assert_eq!(pool.members, ["anthropic-default"]);
    assert!(pool.accepts_new_logins);
    assert!(config.providers.contains_key("anthropic-default"));
}

/// The convergence property in its pure form: the shape a migration
/// produces (suffixed account entries, the plain-named pool listing them)
/// needs no login write for any of its seats, whether the pool is pinned or
/// growth-marked. The pin over the REAL migration pipeline lives beside that
/// pipeline's own fixtures, in `config_migrate_cmd`'s tests.
#[test]
fn the_migrated_config_shape_plans_nothing_for_either_of_its_seats() {
    // Arrange
    let mut doc: toml_edit::DocumentMut = "version = 4\n\
         [providers.anthropic-managed]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         auth_kind = \"oauth-bearer\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n"
        .parse()
        .expect("fixture parses");
    routectl_router::upsert_pool_members(
        &mut doc,
        "anthropic",
        &["anthropic-managed", "anthropic-work"],
    );
    let pinned: Config = toml::from_str(&doc.to_string()).expect("migrated config parses");
    let grown: Config = toml::from_str(&doc.to_string().replace(
        "[pools.anthropic]",
        "[pools.anthropic]\naccepts_new_logins = true",
    ))
    .expect("grown config parses");

    // Act / Assert
    for config in [pinned, grown] {
        for label in [None, Some("work")] {
            let planned = plan(&config, "anthropic", label).expect("plans");
            assert!(
                matches!(planned, SurfacePlan::Nothing { .. }),
                "the migrated shape must need no login write; label {label:?} got {planned:?}"
            );
            assert!(render_delta(&planned).is_empty());
        }
    }
}

#[test]
fn a_login_id_with_no_provider_shape_is_an_error_not_a_plan() {
    // The CLI validates the id against the login registry, so this is
    // unreachable through the command surface -- pinned so it stays a
    // typed refusal rather than a silent empty plan.
    assert!(plan(&Config::default(), "not-a-provider", None).is_err());
}
