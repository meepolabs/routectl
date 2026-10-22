//! Tests for `beta_floor_override_warnings`.
//!
//! Source of truth: every site that touches the `anthropic-beta` header after
//! the router's override filter, in the anthropic-api egress
//! (`AnthropicApiProvider::build_headers`,
//! `crates/routectl-providers/src/anthropic_api/client.rs`) and the router
//! inputs it reads. Line numbers are as of this writing; re-derive with
//! `rg -n 'union_|retain|drop_redact|config_betas|operator_betas|withhold_client_betas'`
//! over that file. `forwarded leg` is `should_use_forwarded_bearer`
//! (client.rs:253): `credential_source = "forwarded"` (the factory sets
//! `use_forwarded_bearer` only then, factory/build.rs:432) AND a captured
//! client bearer AND the Anthropic API host. A forwarded provider always
//! takes it: validation pins its host (factory/validate.rs:745) and the
//! router refuses a forwarded target with no captured bearer
//! (router/dispatch.rs:386, :1384, router/count_tokens.rs:376). An own
//! provider never takes it. `cloak lane` (client.rs:535) is oauth-bearer AND
//! Anthropic API host AND not the forwarded leg -- statically, an own
//! oauth-bearer provider on that host (a mantle entry is api-key only and has
//! no configured host).
//!
//! The override filter itself:
//!
//! - F1 router/beta_withhold.rs:91 -- a forwarded target withholds nothing.
//! - F2 router/beta_withhold.rs:132 -- a flag pinned in the provider or model
//!   `header_extras["anthropic-beta"]` (`operator_betas`,
//!   router/overlays.rs:181) is never withheld.
//! - F3 router/beta_withhold.rs:138 -- the override lookup, model cell
//!   `provider:nickname` first, then the provider cell
//!   (override_registry.rs:132); a pool-backed target carries the member
//!   provider name and the model nickname (router/chain.rs:578).
//! - F4 client.rs:607-610 -- the egress drops the withheld client flags,
//!   keeping any in its own `beta_floor` (client.rs:878); on the forwarded
//!   leg it ships the client set untouched.
//!
//! Sites that add or remove a flag after that filter, in order:
//!
//! | site | client.rs | adds / removes | condition | advisory |
//! |---|---|---|---|---|
//! | S1 provider pin | 620 | adds `header_extras["anthropic-beta"]` of the provider | always, every leg | warns |
//! | S2 model pin | 630 | adds `operator_betas` (provider + model pins, beta_capability.rs `operator_betas`) | always, every leg; per model | warns, per model a provider cell reaches |
//! | S3 OAuth gate | 651-652 | adds `oauth-2025-04-20` | cloak lane | warns on own oauth-bearer on the host only |
//! | S4 Claude Code floor | 661-662 | adds the 9-flag floor | cloak lane AND `is_non_cc` (client.rs:1057: cloak.mode always -> true, never -> false, auto -> classified non-Claude-Code: `routectl_internal.originating_claude_code_session` when set, else no Claude Code session header) | warns on own oauth-bearer on the host unless cloak.mode = never |
//! | S5 context-management strip | 671-672 | removes `context-management-2025-06-27` (exact spelling) | provider `context_management = true`, every leg | that flag is never reported there |
//! | S6 structured outputs | 699-700 | adds `structured-outputs-2025-12-15` | not forwarded leg AND body carries `output_config.format` | warns "whenever the request carries", off the forwarded leg |
//! | S7 effort | 714-717 | adds `effort-2025-11-24` | cloak lane AND body carries `output_config.effort` | warns "whenever the request carries", own oauth-bearer on the host only |
//! | S8 redaction drop | 724-725 | removes `redact-thinking-2026-02-12` (exact spelling) | not forwarded leg AND body carries `thinking.display` | the reason names the display exception, off the forwarded leg |
//!
//! No other site writes the header: the composed list is emitted once
//! (client.rs:744), and the `header_extras` copy skips `anthropic-beta`
//! (client.rs:794).
//!
//! Each label's egress fact is pinned independently against the real header
//! composition by the rows of the same label in
//! `crates/routectl-providers/src/anthropic_api/client_beta_floor_site_tests.rs`
//! (each site with its condition true and false, including cloak.mode =
//! "never" for S3, S4 and S7). A change to this table updates that one.
//!
//! S6 and S7 are body-dependent and still warn: the override is meant to keep
//! the flag off a lane, yet every request using the field ships it, so the
//! operator's intent cannot hold for that traffic. The text says "whenever",
//! never "always".

use super::beta_floor_override_warnings;
use crate::config::Config;
use routectl_core::identity::anthropic::{
    EFFORT_BETA, OAUTH_ANTHROPIC_BETA, REDACT_THINKING_BETA, STRUCTURED_OUTPUTS_BETA,
    default_claude_code_anthropic_betas,
};
use routectl_providers::anthropic_api::CONTEXT_MANAGEMENT_BETA;

const PIN: &str = "pinned-beta-2026-01-01";
const UNPINNED: &str = "context-1m-2025-08-07";

const API_KEY: &str = "api_key_ref = \"env://KEY\"\n";
const OWN_OAUTH: &str = "api_key_ref = \"env://KEY\"\nauth_kind = \"oauth-bearer\"\n\
                         base_url = \"https://api.anthropic.com\"\n";
const OFF_HOST_OAUTH: &str = "api_key_ref = \"env://KEY\"\nauth_kind = \"oauth-bearer\"\n\
                              base_url = \"https://anthropic-proxy.example.test\"\n";
const FORWARDED_OAUTH: &str = "auth_kind = \"oauth-bearer\"\n\
                               base_url = \"https://api.anthropic.com\"\n\
                               credential_source = \"forwarded\"\n";

/// A Claude Code floor flag in no other class, so its row pins S4 alone.
fn claude_code_only_flag() -> &'static str {
    default_claude_code_anthropic_betas()
        .iter()
        .copied()
        .find(|flag| {
            ![
                OAUTH_ANTHROPIC_BETA,
                STRUCTURED_OUTPUTS_BETA,
                CONTEXT_MANAGEMENT_BETA,
                REDACT_THINKING_BETA,
            ]
            .contains(flag)
        })
        .expect("the Claude Code floor carries a flag outside the other classes")
}

/// Provider `p` (anthropic-api, `provider_body` lines) plus `rest`.
fn config(provider_body: &str, rest: &str) -> Config {
    toml::from_str(&format!(
        "version = 5\n[providers.p]\nkind = \"anthropic-api\"\n{provider_body}{rest}"
    ))
    .expect("fixture must parse")
}

fn provider_pin(flag: &str) -> String {
    format!("[providers.p.header_extras]\nanthropic-beta = \"{flag}\"\n")
}

/// Model `m` on `provider` (a provider or pool name), pinning `pin` when set.
fn model(provider: &str, pin: Option<&str>) -> String {
    let pin = pin.map_or(String::new(), |flag| {
        format!("[models.m.header_extras]\nanthropic-beta = \"{flag}\"\n")
    });
    format!("[models.m]\nprovider = \"{provider}\"\nupstream = \"some-model\"\n{pin}")
}

fn unsupported(spec: &str, flag: &str) -> String {
    format!("[capability.overrides.\"{spec}\"]\nunsupported = [\"beta:{flag}\"]\n")
}

fn force_supported(spec: &str, flag: &str) -> String {
    format!("[capability.overrides.\"{spec}\"]\nforce_supported = [\"beta:{flag}\"]\n")
}

/// What a row expects: no warning, or exactly one warning carrying every
/// `contains` needle and none of the `lacks` needles.
enum Expect {
    Silent,
    Warns {
        contains: &'static [&'static str],
        lacks: &'static [&'static str],
    },
}

const fn warns(contains: &'static [&'static str]) -> Expect {
    Expect::Warns {
        contains,
        lacks: &[],
    }
}

fn check(row: &str, config: &Config, expect: &Expect) {
    let warnings = beta_floor_override_warnings(config);
    match expect {
        Expect::Silent => assert!(warnings.is_empty(), "row `{row}`: {warnings:?}"),
        Expect::Warns { contains, lacks } => {
            assert_eq!(warnings.len(), 1, "row `{row}`: {warnings:?}");
            for needle in *contains {
                assert!(
                    warnings[0].contains(needle),
                    "row `{row}` must contain `{needle}`: {}",
                    warnings[0]
                );
            }
            for needle in *lacks {
                assert!(
                    !warnings[0].contains(needle),
                    "row `{row}` must not contain `{needle}`: {}",
                    warnings[0]
                );
            }
        }
    }
}

/// Rows for S1, S2 and the override-filter cells (F1-F3).
fn pin_rows() -> Vec<(&'static str, Config, Expect)> {
    vec![
        (
            "S1 provider pin, api-key provider",
            config(
                API_KEY,
                &format!("{}{}", provider_pin(PIN), unsupported("p", PIN)),
            ),
            warns(&["provider header_extras", PIN, "\"p\""]),
        ),
        (
            "S1 provider pin, forwarded provider",
            config(
                FORWARDED_OAUTH,
                &format!("{}{}", provider_pin(PIN), unsupported("p", PIN)),
            ),
            warns(&["provider header_extras"]),
        ),
        (
            "S2 model pin, model-scoped cell",
            config(
                API_KEY,
                &format!("{}{}", model("p", Some(PIN)), unsupported("p:m", PIN)),
            ),
            warns(&["model header_extras of `m`", "\"p:m\""]),
        ),
        (
            "S2 model pin, provider-scoped cell",
            config(
                API_KEY,
                &format!("{}{}", model("p", Some(PIN)), unsupported("p", PIN)),
            ),
            warns(&["model header_extras of `m`", "\"p\""]),
        ),
        (
            "S2 model pin through a pool, provider-scoped cell",
            config(
                OWN_OAUTH,
                &format!(
                    "[pools.pl]\nmembers = [\"p\"]\n{}{}",
                    model("pl", Some(PIN)),
                    unsupported("p", PIN)
                ),
            ),
            warns(&["model header_extras of `m`"]),
        ),
        (
            "S2 model pin, provider cell shadowed by the model's own cell",
            config(
                API_KEY,
                &format!(
                    "{}{}{}",
                    model("p", Some(PIN)),
                    unsupported("p", PIN),
                    force_supported("p:m", PIN)
                ),
            ),
            Expect::Silent,
        ),
        (
            "S2 model pin on a model of another provider",
            config(
                API_KEY,
                &format!(
                    "[providers.q]\nkind = \"anthropic-api\"\n{API_KEY}{}{}",
                    model("q", Some(PIN)),
                    unsupported("p", PIN)
                ),
            ),
            Expect::Silent,
        ),
        (
            "F4 unpinned client flag on an api-key provider",
            config(API_KEY, &unsupported("p", UNPINNED)),
            Expect::Silent,
        ),
    ]
}

/// Rows for the own-OAuth sites S3, S4 and S7.
fn own_oauth_rows() -> Vec<(&'static str, Config, Expect)> {
    let cc = claude_code_only_flag();
    vec![
        (
            "S3 OAuth gate, own oauth-bearer on the host",
            config(OWN_OAUTH, &unsupported("p", OAUTH_ANTHROPIC_BETA)),
            warns(&["OAuth gate", "Claude Code floor", "its own credential"]),
        ),
        (
            "S3 OAuth gate, own oauth-bearer on the host, cloak never",
            config(
                OWN_OAUTH,
                &format!(
                    "[providers.p.cloak]\nmode = \"never\"\n{}",
                    unsupported("p", OAUTH_ANTHROPIC_BETA)
                ),
            ),
            Expect::Warns {
                contains: &["OAuth gate", "its own credential"],
                lacks: &["Claude Code floor"],
            },
        ),
        (
            "S3 OAuth gate, forwarded oauth-bearer",
            config(FORWARDED_OAUTH, &unsupported("p", OAUTH_ANTHROPIC_BETA)),
            Expect::Silent,
        ),
        (
            "S3 OAuth gate, api-key provider",
            config(API_KEY, &unsupported("p", OAUTH_ANTHROPIC_BETA)),
            Expect::Silent,
        ),
        (
            "S3 OAuth gate, oauth-bearer off the host",
            config(OFF_HOST_OAUTH, &unsupported("p", OAUTH_ANTHROPIC_BETA)),
            Expect::Silent,
        ),
        (
            "S4 Claude Code floor, cloak auto",
            config(OWN_OAUTH, &unsupported("p", cc)),
            warns(&[
                "Claude Code floor",
                "classifies as non-Claude-Code (normally: no Claude Code session header)",
            ]),
        ),
        (
            "S4 Claude Code floor, cloak always",
            config(
                OWN_OAUTH,
                &format!(
                    "[providers.p.cloak]\nmode = \"always\"\n{}",
                    unsupported("p", cc)
                ),
            ),
            warns(&["Claude Code floor", "cloak.mode = \"always\""]),
        ),
        (
            "S4 Claude Code floor, cloak never",
            config(
                OWN_OAUTH,
                &format!(
                    "[providers.p.cloak]\nmode = \"never\"\n{}",
                    unsupported("p", cc)
                ),
            ),
            Expect::Silent,
        ),
        (
            "S4 Claude Code floor, forwarded oauth-bearer",
            config(FORWARDED_OAUTH, &unsupported("p", cc)),
            Expect::Silent,
        ),
        (
            "S7 effort, own oauth-bearer on the host",
            config(OWN_OAUTH, &unsupported("p", EFFORT_BETA)),
            Expect::Warns {
                contains: &[
                    "effort:",
                    "whenever the request carries output_config.effort",
                ],
                lacks: &["always"],
            },
        ),
        (
            "S7 effort, own oauth-bearer on the host, cloak never",
            config(
                OWN_OAUTH,
                &format!(
                    "[providers.p.cloak]\nmode = \"never\"\n{}",
                    unsupported("p", EFFORT_BETA)
                ),
            ),
            warns(&[
                "effort:",
                "whenever the request carries output_config.effort",
            ]),
        ),
        (
            "S7 effort, forwarded oauth-bearer",
            config(FORWARDED_OAUTH, &unsupported("p", EFFORT_BETA)),
            Expect::Silent,
        ),
        (
            "S7 effort, api-key provider",
            config(API_KEY, &unsupported("p", EFFORT_BETA)),
            Expect::Silent,
        ),
    ]
}

/// Rows for the body-dependent union S6 and the removals S5 and S8.
fn body_and_removal_rows() -> Vec<(&'static str, Config, Expect)> {
    vec![
        (
            "S6 structured outputs, api-key provider",
            config(API_KEY, &unsupported("p", STRUCTURED_OUTPUTS_BETA)),
            Expect::Warns {
                contains: &["whenever the request carries output_config.format"],
                lacks: &["always"],
            },
        ),
        (
            "S6 structured outputs, forwarded oauth-bearer",
            config(FORWARDED_OAUTH, &unsupported("p", STRUCTURED_OUTPUTS_BETA)),
            Expect::Silent,
        ),
        (
            "S5 context-management pin, emulation on",
            config(
                &format!("{API_KEY}context_management = true\n"),
                &format!(
                    "{}{}",
                    provider_pin(CONTEXT_MANAGEMENT_BETA),
                    unsupported("p", CONTEXT_MANAGEMENT_BETA)
                ),
            ),
            Expect::Silent,
        ),
        (
            "S5 context-management Claude Code floor, emulation on",
            config(
                &format!("{OWN_OAUTH}context_management = true\n"),
                &unsupported("p", CONTEXT_MANAGEMENT_BETA),
            ),
            Expect::Silent,
        ),
        (
            "S5 context-management pin, emulation off",
            config(
                API_KEY,
                &format!(
                    "{}{}",
                    provider_pin(CONTEXT_MANAGEMENT_BETA),
                    unsupported("p", CONTEXT_MANAGEMENT_BETA)
                ),
            ),
            warns(&["provider header_extras"]),
        ),
        (
            "S8 redaction beta, Claude Code floor",
            config(OWN_OAUTH, &unsupported("p", REDACT_THINKING_BETA)),
            warns(&[
                "Claude Code floor",
                "unless the request carries thinking.display",
            ]),
        ),
        (
            "S8 redaction beta, provider pin",
            config(
                API_KEY,
                &format!(
                    "{}{}",
                    provider_pin(REDACT_THINKING_BETA),
                    unsupported("p", REDACT_THINKING_BETA)
                ),
            ),
            warns(&[
                "provider header_extras",
                "unless the request carries thinking.display",
            ]),
        ),
        (
            "S8 redaction beta, provider pin on the forwarded leg",
            config(
                FORWARDED_OAUTH,
                &format!(
                    "{}{}",
                    provider_pin(REDACT_THINKING_BETA),
                    unsupported("p", REDACT_THINKING_BETA)
                ),
            ),
            Expect::Warns {
                contains: &["provider header_extras"],
                lacks: &["thinking.display"],
            },
        ),
    ]
}

#[test]
fn each_egress_reassertion_site_warns_exactly_where_it_fires() {
    let rows = pin_rows()
        .into_iter()
        .chain(own_oauth_rows())
        .chain(body_and_removal_rows());

    for (row, config, expect) in rows {
        check(row, &config, &expect);
    }
}

#[test]
fn a_case_variant_floor_flag_warns_naming_the_spelling_written() {
    let variant = EFFORT_BETA.to_ascii_uppercase();

    let warnings = beta_floor_override_warnings(&config(OWN_OAUTH, &unsupported("p", &variant)));

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(&variant), "{}", warnings[0]);
    assert!(warnings[0].contains("effort:"), "{}", warnings[0]);
}

#[test]
fn a_force_supported_floor_flag_is_silent() {
    let config = config(API_KEY, &force_supported("p", STRUCTURED_OUTPUTS_BETA));

    assert!(beta_floor_override_warnings(&config).is_empty());
}

#[test]
fn a_floor_flag_override_on_openai_compat_is_silent() {
    let config: Config = toml::from_str(&format!(
        "version = 5\n[providers.p]\nkind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n{API_KEY}{}",
        unsupported("p", STRUCTURED_OUTPUTS_BETA)
    ))
    .expect("fixture must parse");

    assert!(beta_floor_override_warnings(&config).is_empty());
}
