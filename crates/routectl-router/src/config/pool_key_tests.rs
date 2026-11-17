//! Pool member agreement: one rejected row per [`PoolKey`] field, each with
//! an accepted control that spells the same value differently, plus the
//! per-seat tuning a pool must keep accepting and the values a refusal must
//! never echo.

use super::{PoolKey, PoolKeyField};
use crate::config::{Config, ProviderEntry};
use crate::factory::validate_pools;

const BASE_URL: &str = "https://api.anthropic.com";

/// One member's TOML body: the shared seat shape with `overrides` lines
/// replacing or adding keys. A key named in `overrides` is dropped from
/// the shared shape so a row states only what it changes.
fn member(name: &str, label: &str, overrides: &[&str]) -> String {
    let defaults = [
        "format = \"anthropic-messages\"".to_string(),
        format!("base_url = \"{BASE_URL}\""),
        format!("credential = \"oauth://anthropic#{label}\""),
        "auth = \"bearer\"".to_string(),
    ];
    let overridden = |line: &str| {
        let key = line.split('=').next().unwrap_or_default().trim();
        overrides.iter().any(|o| {
            let o_key = o.split('=').next().unwrap_or_default().trim();
            o_key == key || (key == "base_url" && o_key == "cloud")
        })
    };
    let mut body = format!("[providers.{name}]\n");
    for line in defaults.iter().filter(|line| !overridden(line)) {
        body.push_str(line);
        body.push('\n');
    }
    for line in overrides {
        body.push_str(line);
        body.push('\n');
    }
    body
}

/// A two-member pool: `seat-a` with `a` overrides, `seat-b` with `b`.
fn pool_config(a: &[&str], b: &[&str]) -> Config {
    let text = format!(
        "{}{}[pools.team]\nmembers = [\"seat-a\", \"seat-b\"]\n",
        member("seat-a", "a", a),
        member("seat-b", "b", b),
    );
    toml::from_str(&text).unwrap_or_else(|e| panic!("fixture parses: {e}\n{text}"))
}

fn entry(config: &Config, name: &str) -> ProviderEntry {
    config.providers[name].clone()
}

/// The pool-agreement lines of a `validate_pools` refusal, without the
/// other rejection classes it may also carry.
fn mismatch_lines(config: &Config) -> Vec<String> {
    match validate_pools(config) {
        Ok(()) => Vec::new(),
        Err(e) => e
            .to_string()
            .lines()
            .filter(|line| line.contains("differs from member"))
            .map(str::to_owned)
            .collect(),
    }
}

struct Row {
    name: &'static str,
    field: PoolKeyField,
    seat_a: &'static [&'static str],
    seat_b: &'static [&'static str],
}

/// One row per key field: the two members differ on that field alone.
const MISMATCH_ROWS: &[Row] = &[
    Row {
        name: "format",
        field: PoolKeyField::Format,
        seat_a: &[],
        seat_b: &["format = \"openai-chat\""],
    },
    Row {
        name: "destination: another host",
        field: PoolKeyField::Destination,
        seat_a: &[],
        seat_b: &["base_url = \"https://api-b.example.invalid\""],
    },
    Row {
        name: "destination: another path",
        field: PoolKeyField::Destination,
        seat_a: &[],
        seat_b: &["base_url = \"https://api.anthropic.com/v2\""],
    },
    Row {
        name: "destination: base_url against cloud",
        field: PoolKeyField::Destination,
        seat_a: &[],
        seat_b: &["cloud = { name = \"aws\", region = \"us-east-1\" }"],
    },
    Row {
        name: "destination: another region",
        field: PoolKeyField::Destination,
        seat_a: &["cloud = { name = \"aws\", region = \"us-east-1\" }"],
        seat_b: &["cloud = { name = \"aws\", region = \"us-west-2\" }"],
    },
    Row {
        name: "auth",
        field: PoolKeyField::Auth,
        seat_a: &[],
        seat_b: &["auth = \"x-api-key\""],
    },
    Row {
        name: "oauth vendor",
        field: PoolKeyField::OauthVendor,
        seat_a: &[],
        seat_b: &["credential = \"oauth://xai#b\""],
    },
    Row {
        name: "cloak_mode",
        field: PoolKeyField::CloakMode,
        seat_a: &[],
        seat_b: &["cloak_mode = \"always\""],
    },
    Row {
        name: "cloak_strict_mode",
        field: PoolKeyField::CloakStrictMode,
        seat_a: &[],
        seat_b: &["cloak_strict_mode = true"],
    },
];

/// One control per key field: the members spell one value two ways (or
/// share it outright), and the pool is accepted.
const CONTROL_ROWS: &[(&str, &[&str], &[&str])] = &[
    ("format: same format", &[], &[]),
    (
        "destination: trailing slash",
        &[],
        &["base_url = \"https://api.anthropic.com/\""],
    ),
    (
        "destination: trailing slash after a path",
        &["base_url = \"https://api.anthropic.com/v1\""],
        &["base_url = \"https://api.anthropic.com/v1/\""],
    ),
    (
        "destination: scheme and host case",
        &[],
        &["base_url = \"HTTPS://API.Anthropic.COM\""],
    ),
    (
        "destination: same region",
        &["cloud = { name = \"aws\", region = \"us-east-1\" }"],
        &["cloud = { name = \"aws\", region = \"us-east-1\" }"],
    ),
    ("auth: same auth", &[], &["auth = \"bearer\""]),
    (
        "oauth vendor: bare login against a labeled one",
        &[],
        &["credential = \"oauth://anthropic\""],
    ),
    (
        "cloak_mode: unset against explicit default",
        &[],
        &["cloak_mode = \"auto\""],
    ),
    (
        "cloak_strict_mode: unset against explicit default",
        &[],
        &["cloak_strict_mode = false"],
    ),
];

#[test]
fn a_member_differing_on_one_key_field_is_refused_naming_only_that_field() {
    for row in MISMATCH_ROWS {
        // Arrange
        let config = pool_config(row.seat_a, row.seat_b);

        // Act
        let lines = mismatch_lines(&config);

        // Assert
        assert_eq!(
            lines.len(),
            1,
            "`{}` must yield exactly one mismatch line: {lines:?}",
            row.name
        );
        let line = &lines[0];
        for needle in ["pool `team`", "`seat-a`", "`seat-b`", row.field.as_str()] {
            assert!(
                line.contains(needle),
                "`{}` line must name {needle}: {line}",
                row.name
            );
        }
    }
}

#[test]
fn a_key_field_spelled_two_equivalent_ways_is_accepted() {
    for (name, seat_a, seat_b) in CONTROL_ROWS {
        // Arrange
        let config = pool_config(seat_a, seat_b);

        // Act
        let result = validate_pools(&config);

        // Assert
        assert!(
            result.is_ok(),
            "`{name}` must pass: {:?}",
            result.err().map(|e| e.to_string())
        );
    }
}

#[test]
fn per_seat_tuning_differences_are_accepted() {
    // Arrange
    let config = pool_config(
        &[
            "rpm_limit = 10",
            "circuit_failures = 3",
            "circuit_cooldown_ms = 1000",
            "request_timeout_ms = 30000",
            "stream_first_byte_timeout_ms = 5000",
        ],
        &[
            "rpm_limit = 50",
            "circuit_failures = 0",
            "circuit_cooldown_ms = 9000",
            "request_timeout_ms = 120000",
            "stream_first_byte_timeout_ms = 60000",
        ],
    );

    // Act
    let result = validate_pools(&config);

    // Assert
    assert!(
        result.is_ok(),
        "per-seat tuning must not split a pool: {:?}",
        result.err().map(|e| e.to_string())
    );
}

#[test]
fn every_differing_field_gets_its_own_line() {
    // Arrange: seat-b differs on all six fields at once.
    let config = pool_config(
        &[],
        &[
            "format = \"openai-chat\"",
            "base_url = \"https://api-b.example.invalid\"",
            "auth = \"x-api-key\"",
            "credential = \"oauth://xai#b\"",
            "cloak_mode = \"never\"",
            "cloak_strict_mode = true",
        ],
    );

    // Act
    let differing = PoolKey::of(&entry(&config, "seat-a"))
        .differing_fields(&PoolKey::of(&entry(&config, "seat-b")));
    let lines = mismatch_lines(&config);

    // Assert
    assert_eq!(
        differing,
        vec![
            PoolKeyField::Format,
            PoolKeyField::Destination,
            PoolKeyField::Auth,
            PoolKeyField::OauthVendor,
            PoolKeyField::CloakMode,
            PoolKeyField::CloakStrictMode,
        ]
    );
    assert_eq!(lines.len(), differing.len(), "{lines:?}");
}

#[test]
fn a_refusal_never_echoes_a_destination_region_or_credential_value() {
    // Arrange: every value a mismatch could be tempted to print carries a
    // sentinel; the names are what the message is allowed to show.
    let base_a = "https://sentinel-host-a.example.invalid/sentinel-path-a";
    let base_b = "https://sentinel-host-b.example.invalid/sentinel-path-b";
    let configs = [
        pool_config(
            &[
                &format!("base_url = \"{base_a}\""),
                "credential = \"oauth://anthropic#sentinel-label-a\"",
            ],
            &[
                &format!("base_url = \"{base_b}\""),
                "credential = \"oauth://xai#sentinel-label-b\"",
                "auth = \"x-api-key\"",
            ],
        ),
        pool_config(
            &["cloud = { name = \"aws\", region = \"eu-sentinel-1\" }"],
            &["cloud = { name = \"aws\", region = \"ap-sentinel-2\" }"],
        ),
    ];
    let sentinels = ["sentinel", "SENTINEL", "example.invalid", "-1", "-2", "#"];

    for config in &configs {
        // Act
        let lines = mismatch_lines(config);

        // Assert: the control proves a refusal was rendered at all.
        assert!(!lines.is_empty(), "the sentinel pool must be refused");
        for line in &lines {
            for sentinel in sentinels {
                assert!(
                    !line.contains(sentinel),
                    "a mismatch line echoed `{sentinel}`: {line}"
                );
            }
        }
    }
}

#[test]
fn a_hostile_member_name_is_rendered_stripped_in_the_mismatch_line() {
    // Arrange
    let text = format!(
        "{}{}[pools.team]\nmembers = [\"seat-a\", \"seat\\u001b[31m-b\"]\n",
        member("seat-a", "a", &[]),
        member("\"seat\\u001b[31m-b\"", "b", &["cloak_mode = \"never\""]),
    );
    let config: Config = toml::from_str(&text).expect("fixture parses");

    // Act
    let lines = mismatch_lines(&config);

    // Assert
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].chars().any(char::is_control), "{:?}", lines[0]);
    assert!(!lines[0].contains('?'), "{:?}", lines[0]);
    assert!(lines[0].contains("member `seat-b`"), "{:?}", lines[0]);
}

#[test]
fn the_oauth_only_restriction_still_applies_to_a_matching_pool() {
    // Arrange: two identical env-keyed members agree on every key field.
    let config = pool_config(
        &["credential = \"env://SEAT_KEY\""],
        &["credential = \"env://SEAT_KEY\""],
    );

    // Act
    let err = validate_pools(&config)
        .expect_err("an API-key pool is refused")
        .to_string();

    // Assert
    assert!(err.contains("`oauth://` credential"), "{err}");
    assert!(mismatch_lines(&config).is_empty(), "{err}");
}

/// The OAuth-only rule reads the seat's `credential` alone: a static
/// `account_id_ref` companion beside a managed login keeps it a pool seat.
#[cfg(feature = "openai-responses")]
#[test]
fn managed_codex_seats_with_static_account_id_refs_pool_together() {
    // Arrange
    let codex = |label: &str, account_var: &str| {
        vec![
            "format = \"openai-responses\"".to_string(),
            "base_url = \"https://chatgpt.com/backend-api/codex\"".to_string(),
            format!("credential = \"oauth://codex#{label}\""),
            format!("account_id_ref = \"env://{account_var}\""),
        ]
    };
    let a = codex("a", "SEAT_A_ACCOUNT");
    let b = codex("b", "SEAT_B_ACCOUNT");
    let a_refs: Vec<&str> = a.iter().map(String::as_str).collect();
    let b_refs: Vec<&str> = b.iter().map(String::as_str).collect();
    let config = pool_config(&a_refs, &b_refs);

    // Act
    let verdict = validate_pools(&config);

    // Assert
    verdict.expect("two managed codex seats with static account ids pool");
    assert_eq!(
        entry(&config, "seat-a").account_id_ref.as_deref(),
        Some("env://SEAT_A_ACCOUNT"),
        "control: the fixture carries the companion"
    );
}

/// The two pool roles a real install carries pass the whole collected
/// suite: five managed Anthropic seats (labeled and bare, each with its own
/// user agent) and a one-seat Cloud Code pool.
#[cfg(feature = "gemini")]
#[test]
fn a_managed_anthropic_pool_and_a_single_cloud_code_seat_pool_pass() {
    // Arrange
    let mut text = String::new();
    let mut members = Vec::new();
    for (i, credential) in [
        "oauth://anthropic",
        "oauth://anthropic#seat-b",
        "oauth://anthropic#seat-c",
        "oauth://anthropic#seat-d",
        "oauth://anthropic#seat-e",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("anthropic-{i}");
        text.push_str(&format!(
            "[providers.{name}]\n\
             format = \"anthropic-messages\"\n\
             base_url = \"https://api.anthropic.com\"\n\
             credential = \"{credential}\"\n\
             auth = \"bearer\"\n\
             user_agent = \"seat-agent/{i}\"\n"
        ));
        members.push(format!("\"{name}\""));
    }
    text.push_str(&format!(
        "[pools.anthropic]\n\
         members = [{}]\n\
         seat_selection = \"fill-first\"\n\
         accepts_new_logins = true\n\
         [providers.cloud-code-seat]\n\
         format = \"gemini-cloud-code\"\n\
         base_url = \"https://daily-cloudcode-pa.googleapis.com\"\n\
         credential = \"oauth://antigravity#seat-a\"\n\
         auth = \"bearer\"\n\
         [pools.antigravity]\n\
         members = [\"cloud-code-seat\"]\n\
         accepts_new_logins = true\n",
        members.join(", ")
    ));
    let config: Config = toml::from_str(&text).expect("fixture parses");

    // Act
    let errors = crate::factory::collect_config_validation(&config).errors;

    // Assert
    assert!(errors.is_empty(), "{errors:?}");
}
