//! Precedence table for the tuning resolver, every row parsed from real
//! TOML: each tiered key at each tier it supports, the shipped defaults,
//! and the pool membership walk.

use std::num::NonZeroU32;

use super::{BreakerThreshold, Resolved, ResolvedTuning, TuningSource, resolve_tuning};
use crate::config::Config;

const MEMBER: &str = "seat-a";
const SIBLING: &str = "seat-b";
const LONE: &str = "lone";
const POOL: &str = "team-pool";
const MODEL: &str = "slow";

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).expect("valid test toml")
}

/// Two members of `POOL` plus a standalone provider. `member`, `pool` and
/// `retry` are extra lines appended to the `seat-a` provider, the pool and
/// the `[retry]` table.
fn config(member: &str, pool: &str, retry: &str) -> Config {
    config_with_model(member, pool, retry, "")
}

/// [`config`] plus, when `model` is non-empty, a `[models.MODEL]` table on
/// `seat-a` carrying those extra lines.
fn config_with_model(member: &str, pool: &str, retry: &str, model: &str) -> Config {
    let model_table = if model.is_empty() {
        String::new()
    } else {
        format!(
            r#"
[models.{MODEL}]
provider = "{MEMBER}"
upstream = "u"
{model}
"#
        )
    };
    parse(&format!(
        r#"
[providers.{MEMBER}]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "env://K"
{member}

[providers.{SIBLING}]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "env://K"

[providers.{LONE}]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "env://K"

[pools.{POOL}]
members = ["{MEMBER}", "{SIBLING}"]
{pool}

[retry]
{retry}
{model_table}"#
    ))
}

fn at(value: u64, source: TuningSource) -> Resolved<Option<u64>> {
    Resolved {
        value: Some(value),
        source,
    }
}

fn after(n: u32) -> BreakerThreshold {
    BreakerThreshold::After(NonZeroU32::new(n).expect("non-zero"))
}

#[test]
fn defaults_when_no_tier_sets_anything() {
    // Arrange
    let cfg = config("", "", "");

    // Act
    let t = resolve_tuning(&cfg, Some(LONE), None);

    // Assert
    let d = TuningSource::ShippedDefault;
    assert_eq!(
        t.circuit_failures,
        Resolved {
            value: BreakerThreshold::Disabled,
            source: d
        }
    );
    assert_eq!(
        t.circuit_cooldown_ms,
        Resolved {
            value: 30_000,
            source: d
        }
    );
    assert_eq!(
        t.rpm_limit,
        Resolved {
            value: None,
            source: d
        }
    );
    assert_eq!(
        t.request_timeout_ms,
        Resolved {
            value: None,
            source: d
        }
    );
    assert_eq!(t.stream_first_byte_timeout_ms, at(600_000, d));
}

#[test]
fn defaults_when_the_config_has_no_retry_table_and_no_entry() {
    let cfg = parse("");

    let t = resolve_tuning(&cfg, None, None);

    assert_eq!(
        t.stream_first_byte_timeout_ms,
        at(600_000, TuningSource::ShippedDefault)
    );
    assert_eq!(t.circuit_failures.value, BreakerThreshold::Disabled);
}

#[test]
fn an_unknown_entry_resolves_global_then_default() {
    let cfg = config("", "", "request_timeout_ms = 40000");

    let t = resolve_tuning(&cfg, Some("no-such-entry"), None);

    assert_eq!(t.request_timeout_ms, at(40_000, TuningSource::Global));
    assert_eq!(t.circuit_failures.source, TuningSource::ShippedDefault);
}

#[test]
fn circuit_failures_zero_opens_on_the_first_failure() {
    let cfg = config("circuit_failures = 0", "", "");

    let t = resolve_tuning(&cfg, Some(MEMBER), None);

    assert_eq!(
        t.circuit_failures,
        Resolved {
            value: after(1),
            source: TuningSource::Provider
        }
    );
}

/// Each row sets the key at the tier under test AND every lower tier, with
/// distinct values, so the row only passes if the named tier wins.
#[test]
fn each_key_at_each_tier_reports_value_and_source() {
    struct Row {
        name: &'static str,
        member: &'static str,
        pool: &'static str,
        retry: &'static str,
        entry: &'static str,
        model: &'static str,
        check: fn(&ResolvedTuning),
    }
    let rows = [
        Row {
            name: "circuit_failures provider beats pool",
            member: "circuit_failures = 4",
            pool: "circuit_failures = 7",
            retry: "",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(t.circuit_failures.value, after(4));
                assert_eq!(t.circuit_failures.source, TuningSource::Provider);
            },
        },
        Row {
            name: "circuit_failures pool",
            member: "",
            pool: "circuit_failures = 7",
            retry: "",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(t.circuit_failures.value, after(7));
                assert_eq!(t.circuit_failures.source, TuningSource::Pool);
            },
        },
        Row {
            name: "circuit_cooldown_ms provider beats pool",
            member: "circuit_cooldown_ms = 11000",
            pool: "circuit_cooldown_ms = 22000",
            retry: "",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(t.circuit_cooldown_ms.value, 11_000);
                assert_eq!(t.circuit_cooldown_ms.source, TuningSource::Provider);
            },
        },
        Row {
            name: "circuit_cooldown_ms pool",
            member: "",
            pool: "circuit_cooldown_ms = 22000",
            retry: "",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(t.circuit_cooldown_ms.value, 22_000);
                assert_eq!(t.circuit_cooldown_ms.source, TuningSource::Pool);
            },
        },
        Row {
            name: "rpm_limit provider beats pool",
            member: "rpm_limit = 12",
            pool: "rpm_limit = 34",
            retry: "",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(t.rpm_limit.value, Some(12));
                assert_eq!(t.rpm_limit.source, TuningSource::Provider);
            },
        },
        Row {
            name: "rpm_limit pool",
            member: "",
            pool: "rpm_limit = 34",
            retry: "",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(t.rpm_limit.value, Some(34));
                assert_eq!(t.rpm_limit.source, TuningSource::Pool);
            },
        },
        Row {
            name: "request_timeout_ms provider beats pool and global",
            member: "request_timeout_ms = 120000",
            pool: "request_timeout_ms = 240000",
            retry: "request_timeout_ms = 1800000",
            entry: MEMBER,
            model: "",
            check: |t| assert_eq!(t.request_timeout_ms, at(120_000, TuningSource::Provider)),
        },
        Row {
            name: "request_timeout_ms pool beats global",
            member: "",
            pool: "request_timeout_ms = 240000",
            retry: "request_timeout_ms = 1800000",
            entry: MEMBER,
            model: "",
            check: |t| assert_eq!(t.request_timeout_ms, at(240_000, TuningSource::Pool)),
        },
        Row {
            name: "request_timeout_ms global",
            member: "",
            pool: "",
            retry: "request_timeout_ms = 1800000",
            entry: MEMBER,
            model: "",
            check: |t| assert_eq!(t.request_timeout_ms, at(1_800_000, TuningSource::Global)),
        },
        Row {
            name: "first-byte model beats provider, pool and global",
            member: "stream_first_byte_timeout_ms = 60000",
            pool: "stream_first_byte_timeout_ms = 70000",
            retry: "stream_first_byte_timeout_ms = 80000",
            entry: MEMBER,
            model: "stream_first_byte_timeout_ms = 5000",
            check: |t| {
                assert_eq!(
                    t.stream_first_byte_timeout_ms,
                    at(5_000, TuningSource::Model)
                );
            },
        },
        Row {
            name: "first-byte provider beats pool and global",
            member: "stream_first_byte_timeout_ms = 60000",
            pool: "stream_first_byte_timeout_ms = 70000",
            retry: "stream_first_byte_timeout_ms = 80000",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(
                    t.stream_first_byte_timeout_ms,
                    at(60_000, TuningSource::Provider)
                );
            },
        },
        Row {
            name: "first-byte pool beats global",
            member: "",
            pool: "stream_first_byte_timeout_ms = 70000",
            retry: "stream_first_byte_timeout_ms = 80000",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(
                    t.stream_first_byte_timeout_ms,
                    at(70_000, TuningSource::Pool)
                );
            },
        },
        Row {
            name: "first-byte global",
            member: "",
            pool: "",
            retry: "stream_first_byte_timeout_ms = 80000",
            entry: MEMBER,
            model: "",
            check: |t| {
                assert_eq!(
                    t.stream_first_byte_timeout_ms,
                    at(80_000, TuningSource::Global)
                );
            },
        },
    ];
    for row in rows {
        let cfg = config_with_model(row.member, row.pool, row.retry, row.model);
        let model_ms = cfg
            .models
            .get(MODEL)
            .and_then(|m| m.stream_first_byte_timeout_ms);
        let t = resolve_tuning(&cfg, Some(row.entry), model_ms);
        eprintln!("row: {}", row.name);
        (row.check)(&t);
    }
}

#[test]
fn provider_request_timeout_beats_global() {
    let cfg = config(
        "request_timeout_ms = 120000",
        "",
        "request_timeout_ms = 1800000",
    );

    let t = resolve_tuning(&cfg, Some(MEMBER), None);

    assert_eq!(t.request_timeout_ms, at(120_000, TuningSource::Provider));
}

#[test]
fn provider_first_byte_reachable_when_global_omitted() {
    // A present [retry] table that omits the key must not hide the
    // provider tier behind a filled-in default.
    let cfg = config(
        "stream_first_byte_timeout_ms = 60000",
        "",
        "max_attempts = 2",
    );

    let t = resolve_tuning(&cfg, Some(MEMBER), None);

    assert_eq!(
        t.stream_first_byte_timeout_ms,
        at(60_000, TuningSource::Provider)
    );
}

#[test]
fn explicit_global_equal_to_default_reports_global() {
    let cfg = config("", "", "stream_first_byte_timeout_ms = 600000");

    let t = resolve_tuning(&cfg, Some(LONE), None);

    assert_eq!(
        t.stream_first_byte_timeout_ms,
        at(600_000, TuningSource::Global)
    );
}

#[test]
fn model_first_byte_zero_falls_through_to_the_next_tier() {
    // Arrange: the model override parsed from a real [models.X] table.
    let mut text = String::from(
        r#"
[models.slow]
provider = "seat-a"
upstream = "u"
stream_first_byte_timeout_ms = 0
"#,
    );
    text.push_str(
        r#"
[providers.seat-a]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "env://K"
stream_first_byte_timeout_ms = 60000
"#,
    );
    let cfg = parse(&text);
    let model_ms = cfg.models["slow"].stream_first_byte_timeout_ms;
    assert_eq!(model_ms, Some(0), "fixture carries the zero override");

    // Act
    let t = resolve_tuning(&cfg, Some(MEMBER), model_ms);

    // Assert
    assert_eq!(
        t.stream_first_byte_timeout_ms,
        at(60_000, TuningSource::Provider)
    );
}

#[test]
fn pool_value_applies_to_a_member() {
    let cfg = config("", "circuit_failures = 2\nrpm_limit = 9", "");

    let t = resolve_tuning(&cfg, Some(SIBLING), None);

    assert_eq!(t.circuit_failures.value, after(2));
    assert_eq!(t.circuit_failures.source, TuningSource::Pool);
    assert_eq!(t.rpm_limit.value, Some(9));
    assert_eq!(t.rpm_limit.source, TuningSource::Pool);
}

#[test]
fn pool_value_applies_when_the_entry_is_the_pool_itself() {
    let cfg = config("", "circuit_failures = 2", "");

    let t = resolve_tuning(&cfg, Some(POOL), None);

    assert_eq!(t.circuit_failures.value, after(2));
    assert_eq!(t.circuit_failures.source, TuningSource::Pool);
}

#[test]
fn member_value_overrides_the_pool() {
    let cfg = config("circuit_failures = 6", "circuit_failures = 2", "");

    let t = resolve_tuning(&cfg, Some(MEMBER), None);

    assert_eq!(t.circuit_failures.value, after(6));
    assert_eq!(t.circuit_failures.source, TuningSource::Provider);
}

#[test]
fn non_member_provider_ignores_the_pool() {
    let cfg = config("", "circuit_failures = 2\nrequest_timeout_ms = 5000", "");

    let t = resolve_tuning(&cfg, Some(LONE), None);

    assert_eq!(t.circuit_failures.value, BreakerThreshold::Disabled);
    assert_eq!(t.circuit_failures.source, TuningSource::ShippedDefault);
    assert_eq!(t.request_timeout_ms.source, TuningSource::ShippedDefault);
}

#[test]
fn gate_carries_threshold_cooldown_and_rpm() {
    let cfg = config(
        "",
        "circuit_failures = 3\ncircuit_cooldown_ms = 4500\nrpm_limit = 8",
        "",
    );

    let gate = resolve_tuning(&cfg, Some(MEMBER), None).gate();

    assert_eq!(gate.threshold, after(3));
    assert_eq!(gate.cooldown, std::time::Duration::from_millis(4_500));
    assert_eq!(gate.rpm_limit, Some(8));
}

#[test]
fn source_render_tokens_are_frozen() {
    let tokens: Vec<&str> = [
        TuningSource::Model,
        TuningSource::Provider,
        TuningSource::Pool,
        TuningSource::Global,
        TuningSource::ShippedDefault,
    ]
    .into_iter()
    .map(TuningSource::as_str)
    .collect();
    assert_eq!(tokens, ["model", "provider", "pool", "global", "default"]);
}

#[test]
fn retry_table_without_the_first_byte_key_parses_to_unset() {
    // The global tier is a plain option now: omitting the key inherits the
    // shipped default through the resolver, not through serde.
    let cfg = config("", "", "max_attempts = 5");
    assert_eq!(cfg.retry.stream_first_byte_timeout_ms, None);
    assert_eq!(cfg.retry.max_attempts, 5);
}

#[test]
fn pool_tuning_keys_round_trip_and_unknown_keys_still_fail() {
    let cfg = config(
        "",
        "circuit_failures = 1\ncircuit_cooldown_ms = 2\nrpm_limit = 3\n\
         request_timeout_ms = 4\nstream_first_byte_timeout_ms = 5",
        "",
    );
    let pool = &cfg.pools[POOL];
    assert_eq!(
        (
            pool.circuit_failures,
            pool.circuit_cooldown_ms,
            pool.rpm_limit,
            pool.request_timeout_ms,
            pool.stream_first_byte_timeout_ms
        ),
        (Some(1), Some(2), Some(3), Some(4), Some(5))
    );

    let rejected = toml::from_str::<Config>(&format!(
        "[pools.{POOL}]\nmembers = [\"{MEMBER}\"]\ncircuit_failure = 1\n"
    ));
    assert!(rejected.is_err(), "a misspelled pool key must not parse");
}
