//! Operator-written provider, pool and member names reach validation
//! messages through the shared operator-name renderer.

use super::{collect_config_validation, validate_pools};
use crate::config::Config;
use routectl_core::MAX_PROVIDER_NAME_BYTES;

fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).unwrap_or_else(|e| panic!("fixture parses: {e}\n{toml_text}"))
}

/// One provider entry under the quoted key `name`, with `body` lines.
fn provider_with(name: &str, body: &str) -> String {
    format!("[providers.\"{name}\"]\nformat = \"anthropic-messages\"\n{body}")
}

/// A row whose entry breaks exactly one rule, reported by the shape check or
/// by the plan check.
struct RefusalRow {
    label: &'static str,
    body: &'static str,
    marker: &'static str,
}

const REFUSAL_ROWS: &[RefusalRow] = &[
    RefusalRow {
        label: "shape refusal",
        body: "base_url = \"ftp://api.anthropic.com\"\ncredential = \"env://K\"\n\
               auth = \"x-api-key\"\n",
        marker: "base_url scheme is not allowed",
    },
    RefusalRow {
        label: "plan refusal",
        body: "base_url = \"https://api.anthropic.com\"\ncredential = \"aws://chain\"\n\
               auth = \"x-api-key\"\n",
        marker: "an aws:// credential signs requests",
    },
];

fn refusal_line(label: &str, name: &str, row: &RefusalRow) -> String {
    let errors = collect_config_validation(&parse(&provider_with(name, row.body))).errors;
    let mut lines = errors.iter().filter(|e| e.contains(row.marker));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("{label}: no `{}` refusal in {errors:?}", row.marker));
    assert!(lines.next().is_none(), "{label}: {errors:?}");
    line.clone()
}

#[test]
fn a_provider_name_over_the_byte_ceiling_is_withheld_from_its_refusals() {
    let over = "q".repeat(MAX_PROVIDER_NAME_BYTES + 1);
    for row in REFUSAL_ROWS {
        let line = refusal_line(row.label, &over, row);

        assert!(!line.contains(&over), "{}: {line}", row.label);
        assert!(
            line.contains(&format!("provider `<{}-byte name withheld>`", over.len())),
            "{}: {line}",
            row.label
        );
    }
}

#[test]
fn a_provider_name_at_the_byte_ceiling_is_shown_in_its_refusals() {
    let at = "q".repeat(MAX_PROVIDER_NAME_BYTES);
    for row in REFUSAL_ROWS {
        let line = refusal_line(row.label, &at, row);

        assert!(
            line.contains(&format!("provider `{at}`")),
            "{}: {line}",
            row.label
        );
    }
}

#[test]
fn a_provider_name_with_control_bytes_is_rendered_without_them() {
    let name = "work\\u001b[31m\\nfake";
    for row in REFUSAL_ROWS {
        let line = refusal_line(row.label, name, row);

        assert!(
            !line.chars().any(char::is_control),
            "{}: {line:?}",
            row.label
        );
        assert!(
            line.contains("provider `workfake`"),
            "{}: {line}",
            row.label
        );
    }
}

/// A pool name carrying an ANSI color sequence and a newline, as a quoted
/// TOML key, and the name it renders as.
const POOL: &str = "\"east\\u001b[31m\\nwing\"";
const POOL_SHOWN: &str = "eastwing";
/// A member name carrying a screen-clear sequence and a newline.
const MEMBER: &str = "\"seat\\u001b[2J\\nb\"";
const MEMBER_SHOWN: &str = "seatb";

fn oauth_seat(key: &str, label: &str) -> String {
    format!(
        "[providers.{key}]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         credential = \"oauth://anthropic#{label}\"\nauth = \"bearer\"\n"
    )
}

fn pool(key: &str, members: &[&str]) -> String {
    format!("[pools.{key}]\nmembers = [{}]\n", members.join(", "))
}

/// The single message `validate_pools` reports for `toml_text`.
fn only_pool_message(label: &str, toml_text: &str) -> String {
    let message = validate_pools(&parse(toml_text))
        .expect_err(label)
        .to_string();
    assert_eq!(message.lines().count(), 1, "{label}: {message:?}");
    message
}

#[test]
fn hostile_pool_and_member_names_reach_every_pool_message_without_a_control_byte() {
    let too_many: Vec<String> = (0..=crate::factory::MAX_POOL_MEMBERS)
        .map(|i| format!("s{i:03}"))
        .collect();
    let too_many_seats: String = too_many.iter().map(|m| oauth_seat(m, m)).collect();
    let too_many: Vec<String> = too_many.iter().map(|m| format!("\"{m}\"")).collect();
    let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
    let rows: Vec<(&str, String, String)> = vec![
        (
            "pool named like a provider entry",
            format!("{}{}", oauth_seat(POOL, "a"), pool(POOL, &[POOL])),
            format!("pool `{POOL_SHOWN}`: a provider entry of the same name exists"),
        ),
        (
            "pool named like a model",
            format!(
                "{}[models.{POOL}]\nprovider = \"x\"\nupstream = \"u\"\n{}",
                oauth_seat(MEMBER, "a"),
                pool(POOL, &[MEMBER])
            ),
            format!("pool `{POOL_SHOWN}`: a model nickname of the same name exists"),
        ),
        (
            "empty pool",
            pool(POOL, &[]),
            format!("pool `{POOL_SHOWN}`: `members` is empty"),
        ),
        (
            "too many members",
            format!("{too_many_seats}{}", pool(POOL, &too_many)),
            format!("pool `{POOL_SHOWN}`: 33 members exceeds"),
        ),
        (
            "member claimed by two pools",
            format!(
                "{}{}{}",
                oauth_seat(MEMBER, "a"),
                pool(POOL, &[MEMBER]),
                pool("plain", &[MEMBER])
            ),
            format!(
                "provider `{MEMBER_SHOWN}` is a member of both pool `{POOL_SHOWN}` and pool \
                 `plain`"
            ),
        ),
        (
            "member listed twice in one pool",
            format!(
                "{}{}",
                oauth_seat(MEMBER, "a"),
                pool(POOL, &[MEMBER, MEMBER])
            ),
            format!("pool `{POOL_SHOWN}`: member `{MEMBER_SHOWN}` is listed more than once"),
        ),
        (
            "unknown member",
            pool(POOL, &[MEMBER]),
            format!("pool `{POOL_SHOWN}`: member `{MEMBER_SHOWN}` is not a known provider entry"),
        ),
        (
            "member without an oauth credential",
            format!(
                "{}{}",
                provider_with(
                    "seat\\u001b[2J\\nb",
                    "base_url = \"https://api.anthropic.com\"\ncredential = \"env://K\"\n\
                     auth = \"x-api-key\"\n"
                ),
                pool(POOL, &[MEMBER])
            ),
            format!("pool `{POOL_SHOWN}`: member `{MEMBER_SHOWN}` does not authenticate"),
        ),
    ];
    for (label, toml_text, expected) in rows {
        let message = only_pool_message(label, &toml_text);

        assert!(
            !message.chars().any(char::is_control),
            "{label}: {message:?}"
        );
        assert!(message.contains(&expected), "{label}: {message}");
    }
}

#[test]
fn a_hostile_pool_name_in_a_mismatch_line_carries_no_control_byte() {
    let toml_text = format!(
        "{}{}cloak_mode = \"never\"\n{}",
        oauth_seat("seat-a", "a"),
        oauth_seat("seat-b", "b"),
        pool(POOL, &["\"seat-a\"", "\"seat-b\""])
    );

    let message = only_pool_message("mismatch", &toml_text);

    assert!(!message.chars().any(char::is_control), "{message:?}");
    assert!(message.contains("differs from member"), "{message}");
}

/// How `validate_pools` must answer one membership layout: `None` accepts,
/// `Some(lines)` refuses with exactly those lines.
struct MembershipRow {
    label: &'static str,
    pools: &'static [(&'static str, &'static [&'static str])],
    refusal: Option<&'static [&'static str]>,
}

const REPEAT_LINE: &str = "pool `east`: member `seat-a` is listed more than once; a repeated \
                           member would be placed as two seats of one account -- list each \
                           provider entry once";

const MEMBERSHIP_ROWS: &[MembershipRow] = &[
    MembershipRow {
        label: "member repeated twice in one pool",
        pools: &[("east", &["seat-a", "seat-b", "seat-a"])],
        refusal: Some(&[REPEAT_LINE]),
    },
    MembershipRow {
        label: "member repeated three times in one pool",
        pools: &[("east", &["seat-a", "seat-a", "seat-b", "seat-a"])],
        refusal: Some(&[REPEAT_LINE]),
    },
    MembershipRow {
        label: "two members each repeated in one pool",
        pools: &[("east", &["seat-a", "seat-b", "seat-a", "seat-b"])],
        refusal: Some(&[
            REPEAT_LINE,
            "pool `east`: member `seat-b` is listed more than once; a repeated member would be \
             placed as two seats of one account -- list each provider entry once",
        ]),
    },
    MembershipRow {
        label: "member in two pools",
        pools: &[("east", &["seat-a"]), ("west", &["seat-a", "seat-b"])],
        refusal: Some(&[
            "provider `seat-a` is a member of both pool `east` and pool `west`; a provider \
             entry belongs to at most one pool",
        ]),
    },
    MembershipRow {
        label: "distinct members",
        pools: &[("east", &["seat-a", "seat-b"])],
        refusal: None,
    },
];

fn membership_config(pools: &[(&str, &[&str])]) -> Config {
    let mut text = format!("{}{}", oauth_seat("seat-a", "a"), oauth_seat("seat-b", "b"));
    for (name, members) in pools {
        let quoted: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
        let quoted: Vec<&str> = quoted.iter().map(String::as_str).collect();
        text.push_str(&pool(name, &quoted));
    }
    parse(&text)
}

#[test]
fn pool_membership_is_refused_once_per_repeated_or_shared_member() {
    for row in MEMBERSHIP_ROWS {
        let config = membership_config(row.pools);

        let outcome = validate_pools(&config).map_err(|e| e.to_string());

        match row.refusal {
            None => assert!(outcome.is_ok(), "{}: {outcome:?}", row.label),
            Some(lines) => {
                let message = outcome.expect_err(row.label);
                let expected = format!("config: {}", lines.join("\n"));
                assert_eq!(message, expected, "{}", row.label);
            }
        }
    }
}
