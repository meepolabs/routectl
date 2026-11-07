//! Containment of the managed subscription tokens: each `oauth://<family>`
//! reference, bare or labeled, rides only its own family's lane on that
//! vendor's exact first-party host over https. The Anthropic family's own
//! shape cases live with `validate_managed_anthropic_credential`.

#[cfg(any(feature = "openai-responses", feature = "gemini"))]
use super::collect_config_validation;
use super::{
    MANAGED_ANTIGRAVITY, MANAGED_CODEX, MANAGED_OAUTH_FAMILIES, check_managed_oauth_family,
    validate_managed_oauth_credentials,
};
#[cfg(any(feature = "bedrock", feature = "openai-responses", feature = "gemini"))]
use crate::config::Config;
use crate::config::ProviderEntry;

const CODEX_REFS: [&str; 2] = ["oauth://codex", "oauth://codex#seat-secret-label"];
const ANTIGRAVITY_REFS: [&str; 2] = [
    "oauth://antigravity",
    "oauth://antigravity#seat-secret-label",
];
#[cfg(any(feature = "openai-responses", feature = "gemini"))]
const SENTINEL_HOST: &str = "https://gateway.sentinel-host.example/sentinel-path?k=1";

/// Hosts no managed family accepts: loopback, cleartext, smuggles, and
/// malformed values. Each family's own lookalikes are listed with it.
#[cfg(any(feature = "openai-responses", feature = "gemini"))]
const SHARED_REJECTED: [&str; 10] = [
    "http://127.0.0.1:18080",
    "http://localhost:18080",
    "https://127.0.0.1",
    "https://localhost",
    "http://[::1]:18080",
    "https://evil.example",
    SENTINEL_HOST,
    "ftp://evil.example",
    "",
    "not a url",
];

#[cfg(any(feature = "bedrock", feature = "openai-responses", feature = "gemini"))]
fn parse(text: &str) -> Config {
    toml::from_str(text).expect("fixture config parses")
}

#[cfg(any(feature = "bedrock", feature = "gemini"))]
fn only_entry(text: &str) -> ProviderEntry {
    parse(text)
        .providers
        .into_values()
        .next()
        .expect("one provider")
}

fn codex_rejects(entry: &ProviderEntry) -> bool {
    check_managed_oauth_family(&MANAGED_CODEX, "p", entry).is_err()
}

fn antigravity_rejects(entry: &ProviderEntry) -> bool {
    check_managed_oauth_family(&MANAGED_ANTIGRAVITY, "p", entry).is_err()
}

#[cfg(feature = "openai-responses")]
mod codex {
    use super::*;
    use routectl_providers::openai_responses::AuthKind;

    const ACCEPTED: [&str; 6] = [
        "https://chatgpt.com/backend-api/codex",
        "https://chatgpt.com",
        "https://chatgpt.com/",
        "https://ChatGPT.com/backend-api/codex",
        "https://chatgpt.com:443/backend-api/codex",
        "https://chatgpt.com/other/path?q=1",
    ];
    const REJECTED: [&str; 17] = [
        "https://chatgpt.com.evil.test",
        "https://chatgpt.com.evil.test/backend-api/codex",
        "https://evilchatgpt.com",
        "https://evil-chatgpt.com",
        "https://api.chatgpt.com",
        "https://www.chatgpt.com",
        "https://chatgpt.co",
        "https://chatgpt.com.",
        "https://evil.example/chatgpt.com",
        "https://evil.example?h=chatgpt.com",
        "https://evil.example#chatgpt.com",
        "https://chatgpt.com@evil.example",
        "https://evil.example\\@chatgpt.com",
        "https://user:pass@chatgpt.com",
        "https://user@chatgpt.com",
        "https://chatgpt.com:8443",
        "http://chatgpt.com",
    ];

    fn responses(r: &str, base_url: &str) -> ProviderEntry {
        ProviderEntry::openai_responses(r).with_openai_responses_base_url(base_url)
    }

    #[test]
    fn accepts_only_the_exact_https_chatgpt_host_for_bare_and_labeled_refs() {
        for r in CODEX_REFS {
            for url in ACCEPTED {
                assert!(
                    !codex_rejects(&responses(r, url)),
                    "must accept {url} with {r}"
                );
            }
            for url in REJECTED.iter().chain(SHARED_REJECTED.iter()) {
                assert!(
                    codex_rejects(&responses(r, url)),
                    "must reject {url:?} with {r}"
                );
            }
        }
    }

    #[test]
    fn accepts_the_default_base_url_for_bare_and_labeled_refs() {
        for r in CODEX_REFS {
            assert!(
                !codex_rejects(&ProviderEntry::openai_responses(r)),
                "default base_url: {r}"
            );
            let explicit = ProviderEntry::openai_responses(r)
                .with_openai_responses_auth_kind(AuthKind::ChatgptOauth);
            assert!(!codex_rejects(&explicit), "explicit chatgpt-oauth: {r}");
        }
    }

    #[test]
    fn rejects_the_api_key_auth_kind_even_on_the_chatgpt_host() {
        for r in CODEX_REFS {
            for base in [None, Some("https://chatgpt.com/backend-api/codex")] {
                let mut entry = ProviderEntry::openai_responses(r)
                    .with_openai_responses_auth_kind(AuthKind::ApiKey);
                if let Some(b) = base {
                    entry = entry.with_openai_responses_base_url(b);
                }
                assert!(
                    codex_rejects(&entry),
                    "api-key must reject {r} base={base:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_a_managed_ref_in_the_account_id_slot_on_a_foreign_host() {
        let entry = ProviderEntry::openai_responses("env://OPENAI_JWT")
            .with_account_id_ref("oauth://codex#seat-secret-label")
            .with_openai_responses_base_url("http://127.0.0.1:18080");
        assert!(codex_rejects(&entry));

        let on_host = ProviderEntry::openai_responses("env://OPENAI_JWT")
            .with_account_id_ref("oauth://codex");
        assert!(!codex_rejects(&on_host), "account slot on the default host");
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn rejects_a_managed_ref_on_the_responses_mantle_lane() {
        let entry = only_entry(
            "[providers.m]\n\
             kind = \"openai-responses\"\n\
             api_key_ref = \"\"\n\
             bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"bearer-key\", key_ref = \"oauth://codex\" } }\n",
        );
        assert!(codex_rejects(&entry));
    }

    #[test]
    fn leaves_static_credentials_and_other_families_alone_on_any_host() {
        let cases = [
            responses("env://OPENAI_JWT", "http://127.0.0.1:18080"),
            responses("file:///tmp/jwt", "https://chatgpt.com.evil.test"),
            ProviderEntry::openai_responses("env://OPENAI_KEY")
                .with_openai_responses_auth_kind(AuthKind::ApiKey)
                .with_openai_responses_base_url(SENTINEL_HOST),
            responses("oauth://codex-a", SENTINEL_HOST),
            responses("oauth://antigravity", SENTINEL_HOST),
        ];
        for entry in &cases {
            assert!(!codex_rejects(entry), "must not touch {entry:?}");
        }
    }
}

#[cfg(feature = "gemini")]
mod antigravity {
    use super::*;

    const ACCEPTED: [&str; 8] = [
        "https://cloudcode-pa.googleapis.com",
        "https://cloudcode-pa.googleapis.com/",
        "https://cloudcode-pa.googleapis.com/v1internal",
        "https://CloudCode-PA.googleapis.com:443",
        "https://daily-cloudcode-pa.googleapis.com",
        "https://daily-cloudcode-pa.googleapis.com/",
        "https://daily-cloudcode-pa.googleapis.com:443",
        "https://DAILY-cloudcode-pa.googleapis.com",
    ];
    const REJECTED: [&str; 19] = [
        "https://cloudcode-pa.googleapis.com.evil.test",
        "https://daily-cloudcode-pa.googleapis.com.evil.test",
        "https://evilcloudcode-pa.googleapis.com",
        "https://evil-daily-cloudcode-pa.googleapis.com",
        "https://staging-cloudcode-pa.googleapis.com",
        "https://generativelanguage.googleapis.com",
        "https://googleapis.com",
        "https://cloudcode-pa.googleapis.co",
        "https://cloudcode-pa.googleapis.com.",
        "https://daily-cloudcode-pa.googleapis.com.",
        "https://evil.example/cloudcode-pa.googleapis.com",
        "https://evil.example?h=daily-cloudcode-pa.googleapis.com",
        "https://cloudcode-pa.googleapis.com@evil.example",
        "https://evil.example\\@cloudcode-pa.googleapis.com",
        "https://user:pass@cloudcode-pa.googleapis.com",
        "https://user@daily-cloudcode-pa.googleapis.com",
        "https://cloudcode-pa.googleapis.com:8443",
        "http://cloudcode-pa.googleapis.com",
        "http://daily-cloudcode-pa.googleapis.com",
    ];

    fn gemini(r: &str, auth_mode: &str, base_url: Option<&str>) -> ProviderEntry {
        let pin = base_url.map_or_else(String::new, |b| {
            format!("base_url = {}\n", toml::Value::String(b.to_string()))
        });
        only_entry(&format!(
            "[providers.g]\n\
             kind = \"gemini\"\n\
             api_key_ref = \"{r}\"\n\
             auth_mode = \"{auth_mode}\"\n\
             {pin}"
        ))
    }

    #[test]
    fn accepts_only_the_exact_https_cloud_code_hosts_for_bare_and_labeled_refs() {
        for r in ANTIGRAVITY_REFS {
            for url in ACCEPTED {
                let entry = gemini(r, "cloud-code", Some(url));
                assert!(!antigravity_rejects(&entry), "must accept {url} with {r}");
            }
            for url in REJECTED.iter().chain(SHARED_REJECTED.iter()) {
                let entry = gemini(r, "cloud-code", Some(url));
                assert!(antigravity_rejects(&entry), "must reject {url:?} with {r}");
            }
        }
    }

    #[test]
    fn accepts_the_default_cloud_code_base_for_bare_and_labeled_refs() {
        for r in ANTIGRAVITY_REFS {
            assert!(
                !antigravity_rejects(&gemini(r, "cloud-code", None)),
                "unset base_url takes the daily host: {r}"
            );
        }
    }

    #[test]
    fn rejects_the_api_key_auth_mode_on_any_host() {
        for r in ANTIGRAVITY_REFS {
            for base in [
                None,
                Some("https://generativelanguage.googleapis.com/v1beta"),
                Some("https://cloudcode-pa.googleapis.com"),
            ] {
                let entry = gemini(r, "api-key", base);
                assert!(
                    antigravity_rejects(&entry),
                    "api-key must reject {r} base={base:?}"
                );
            }
        }
    }

    #[test]
    fn leaves_static_credentials_and_other_families_alone_on_any_host() {
        let cases = [
            gemini(
                "env://GEMINI_API_KEY",
                "api-key",
                Some("http://127.0.0.1:18080"),
            ),
            gemini("file:///tmp/k", "cloud-code", Some(SENTINEL_HOST)),
            gemini(
                "oauth://antigravity-a",
                "cloud-code",
                Some("http://127.0.0.1:18080"),
            ),
            gemini("oauth://codex", "cloud-code", Some(SENTINEL_HOST)),
        ];
        for entry in &cases {
            assert!(!antigravity_rejects(entry), "must not touch {entry:?}");
        }
    }
}

/// Every provider kind other than the family's own lane rejects both families,
/// bare and labeled, even on the family's first-party host.
#[test]
fn rejects_every_foreign_provider_kind_for_both_families() {
    let refs = CODEX_REFS.iter().chain(ANTIGRAVITY_REFS.iter());
    for r in refs {
        #[cfg_attr(
            not(any(feature = "bedrock", feature = "openai-responses", feature = "gemini")),
            allow(unused_mut)
        )]
        let mut foreign = vec![
            ProviderEntry::openai_compat("https://chatgpt.com/backend-api/codex", *r),
            ProviderEntry::openai_compat("https://cloudcode-pa.googleapis.com", *r),
            ProviderEntry::anthropic_api(*r),
            ProviderEntry::anthropic_api(*r).with_base_url("https://chatgpt.com"),
        ];
        #[cfg(feature = "bedrock")]
        foreign.push(only_entry(&format!(
            "[providers.br]\n\
             kind = \"bedrock\"\n\
             region = \"us-east-1\"\n\
             creds = {{ kind = \"bearer-key\", key_ref = \"{r}\" }}\n"
        )));
        let is_codex = r.starts_with("oauth://codex");
        #[cfg(feature = "openai-responses")]
        if !is_codex {
            foreign.push(ProviderEntry::openai_responses(*r));
        }
        #[cfg(feature = "gemini")]
        if is_codex {
            foreign.push(ProviderEntry::gemini(*r));
            foreign.push(
                ProviderEntry::gemini(*r)
                    .with_gemini_auth_mode(routectl_providers::gemini::GeminiAuthMode::CloudCode),
            );
        }
        for entry in &foreign {
            let rejected = if is_codex {
                codex_rejects(entry)
            } else {
                antigravity_rejects(entry)
            };
            assert!(rejected, "{r} must be rejected on {entry:?}");
        }
    }
}

#[cfg(any(feature = "openai-responses", feature = "gemini"))]
#[test]
fn config_validation_rejects_foreign_hosts_naming_the_provider_only() {
    let mut text = String::new();
    let mut names = Vec::new();
    #[cfg(feature = "openai-responses")]
    for (i, r) in CODEX_REFS.iter().enumerate() {
        text.push_str(&format!(
            "[providers.cx-gw{i}]\n\
             kind = \"openai-responses\"\n\
             api_key_ref = \"{r}\"\n\
             base_url = \"{SENTINEL_HOST}\"\n\
             [providers.cx-lo{i}]\n\
             kind = \"openai-responses\"\n\
             api_key_ref = \"{r}\"\n\
             base_url = \"http://127.0.0.1:18080\"\n"
        ));
        names.push((format!("cx-gw{i}"), "Codex"));
        names.push((format!("cx-lo{i}"), "Codex"));
    }
    #[cfg(feature = "gemini")]
    for (i, r) in ANTIGRAVITY_REFS.iter().enumerate() {
        text.push_str(&format!(
            "[providers.ag-gw{i}]\n\
             kind = \"gemini\"\n\
             auth_mode = \"cloud-code\"\n\
             api_key_ref = \"{r}\"\n\
             base_url = \"{SENTINEL_HOST}\"\n\
             [providers.ag-lo{i}]\n\
             kind = \"gemini\"\n\
             auth_mode = \"cloud-code\"\n\
             api_key_ref = \"{r}\"\n\
             base_url = \"http://127.0.0.1:18080\"\n"
        ));
        names.push((format!("ag-gw{i}"), "Antigravity"));
        names.push((format!("ag-lo{i}"), "Antigravity"));
    }
    let config = parse(&text);

    let errors = collect_config_validation(&config).errors;

    for (name, family) in &names {
        let needle = format!("provider `{name}`:");
        let hit: Vec<&String> = errors
            .iter()
            .filter(|e| e.contains(&needle) && e.contains(&format!("managed {family}")))
            .collect();
        assert_eq!(hit.len(), 1, "one containment error for {name}: {errors:?}");
        let msg = hit[0];
        for leaked in [
            "sentinel-host",
            "sentinel-path",
            "127.0.0.1",
            "oauth://codex",
            "oauth://antigravity",
            "seat-secret-label",
        ] {
            assert!(!msg.contains(leaked), "message leaks {leaked}: {msg}");
        }
        assert!(
            msg.chars().count() <= 300,
            "containment error must fit the reported-line cap: {msg}"
        );
    }
}

/// The shipped default shapes -- what `routectl login codex` / `routectl
/// login antigravity` offer and what `examples/config.toml` documents --
/// stay valid, alongside static credentials on loopback and gateways.
#[cfg(any(feature = "openai-responses", feature = "gemini"))]
#[test]
fn config_validation_keeps_the_deployed_default_shapes_valid() {
    let mut text = String::new();
    #[cfg(feature = "openai-responses")]
    text.push_str(
        "[providers.codex-default]\n\
         kind        = \"openai-responses\"\n\
         auth_kind   = \"chatgpt-oauth\"\n\
         api_key_ref = \"oauth://codex\"\n\
         [providers.codex-seat]\n\
         kind        = \"openai-responses\"\n\
         api_key_ref = \"oauth://codex#seat-b\"\n\
         base_url    = \"https://chatgpt.com/backend-api/codex\"\n\
         [providers.codex-static-mock]\n\
         kind           = \"openai-responses\"\n\
         auth_kind      = \"chatgpt-oauth\"\n\
         api_key_ref    = \"env://OPENAI_JWT\"\n\
         account_id_ref = \"env://OPENAI_ACCOUNT_ID\"\n\
         base_url       = \"http://127.0.0.1:18080\"\n",
    );
    #[cfg(feature = "gemini")]
    text.push_str(
        "[providers.antigravity-default]\n\
         kind        = \"gemini\"\n\
         api_key_ref = \"oauth://antigravity\"\n\
         auth_mode   = \"cloud-code\"\n\
         [providers.antigravity-prod]\n\
         kind        = \"gemini\"\n\
         api_key_ref = \"oauth://antigravity#seat-b\"\n\
         auth_mode   = \"cloud-code\"\n\
         base_url    = \"https://cloudcode-pa.googleapis.com\"\n\
         [providers.antigravity-daily]\n\
         kind        = \"gemini\"\n\
         api_key_ref = \"oauth://antigravity\"\n\
         auth_mode   = \"cloud-code\"\n\
         base_url    = \"https://daily-cloudcode-pa.googleapis.com\"\n\
         [providers.gemini-static-mirror]\n\
         kind        = \"gemini\"\n\
         api_key_ref = \"env://GEMINI_API_KEY\"\n\
         base_url    = \"https://gemini-mirror.example/v1beta\"\n",
    );
    let config = parse(&text);

    let errors = collect_config_validation(&config).errors;

    assert!(
        errors.is_empty(),
        "supported shapes must validate: {errors:?}"
    );
}

/// The shipped example config carries the deployed codex entry; it must
/// clear every managed containment rule as written.
#[cfg(all(feature = "openai-responses", feature = "gemini", feature = "bedrock"))]
#[test]
fn shipped_example_config_clears_managed_containment() {
    let example = include_str!("../../../../examples/config.toml");
    assert!(
        example.contains("oauth://codex"),
        "example carries a codex entry"
    );
    let config = parse(example);

    for (name, entry) in &config.providers {
        assert!(
            validate_managed_oauth_credentials(name, entry).is_ok(),
            "example provider `{name}`"
        );
    }
}

/// Every `oauth://` provider routectl can log into has exactly one
/// containment row, and the table names no other family.
#[test]
fn the_containment_table_covers_exactly_the_known_oauth_providers() {
    let table: std::collections::BTreeSet<&str> = MANAGED_OAUTH_FAMILIES
        .iter()
        .map(|row| row.family)
        .collect();
    let known: std::collections::BTreeSet<&str> = routectl_auth::oauth::known_provider_ids()
        .iter()
        .copied()
        .collect();

    assert_eq!(table, known);
    assert_eq!(
        table.len(),
        MANAGED_OAUTH_FAMILIES.len(),
        "one row per family"
    );
}

/// One vendor's managed lane: its family, its first-party host and path, and
/// how to build an entry on that lane at a given `base_url`.
struct VendorLane {
    family: &'static str,
    host: &'static str,
    path: &'static str,
    entry: fn(&str, &str) -> ProviderEntry,
}

/// Each vendor's first-party base, listed even when that vendor's lane is
/// compiled out: it is still a foreign host for every other family.
const VENDOR_BASES: [&str; 4] = [
    "https://api.anthropic.com",
    "https://chatgpt.com/backend-api/codex",
    "https://api.x.ai/v1",
    "https://cloudcode-pa.googleapis.com",
];

fn anthropic_lane(r: &str, base_url: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(r)
        .with_auth_kind(routectl_providers::anthropic_api::AuthKind::OauthBearer)
        .with_base_url(base_url)
}

fn xai_lane(r: &str, base_url: &str) -> ProviderEntry {
    ProviderEntry::openai_compat(base_url, r)
}

#[cfg(feature = "openai-responses")]
fn codex_lane(r: &str, base_url: &str) -> ProviderEntry {
    ProviderEntry::openai_responses(r).with_openai_responses_base_url(base_url)
}

#[cfg(feature = "gemini")]
fn antigravity_lane(r: &str, base_url: &str) -> ProviderEntry {
    only_entry(&format!(
        "[providers.g]\n\
         kind = \"gemini\"\n\
         api_key_ref = \"{r}\"\n\
         auth_mode = \"cloud-code\"\n\
         base_url = {}\n",
        toml::Value::String(base_url.to_string())
    ))
}

fn vendor_lanes() -> Vec<VendorLane> {
    #[cfg_attr(
        not(any(feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut lanes = vec![
        VendorLane {
            family: "anthropic",
            host: "api.anthropic.com",
            path: "",
            entry: anthropic_lane,
        },
        VendorLane {
            family: "xai",
            host: "api.x.ai",
            path: "/v1",
            entry: xai_lane,
        },
    ];
    #[cfg(feature = "openai-responses")]
    lanes.push(VendorLane {
        family: "codex",
        host: "chatgpt.com",
        path: "/backend-api/codex",
        entry: codex_lane,
    });
    #[cfg(feature = "gemini")]
    lanes.push(VendorLane {
        family: "antigravity",
        host: "cloudcode-pa.googleapis.com",
        path: "",
        entry: antigravity_lane,
    });
    lanes
}

/// Each managed family on its own lane: accepted on its vendor's host, and
/// refused on every other vendor's host, on loopback, with an explicit port,
/// and with userinfo on its own host.
#[test]
fn each_managed_family_is_accepted_only_on_its_own_vendor_host() {
    for lane in vendor_lanes() {
        let own = format!("https://{}{}", lane.host, lane.path);
        assert!(VENDOR_BASES.contains(&own.as_str()), "{own} is listed");
        let mut refused: Vec<String> = VENDOR_BASES
            .iter()
            .filter(|base| **base != own)
            .map(|base| (*base).to_string())
            .collect();
        refused.push("http://127.0.0.1:18080".to_string());
        refused.push(format!("https://{}:8443{}", lane.host, lane.path));
        refused.push(format!("https://user:pass@{}{}", lane.host, lane.path));
        refused.push(format!("https://user@{}{}", lane.host, lane.path));

        for r in [
            format!("oauth://{}", lane.family),
            format!("oauth://{}#seat-secret-label", lane.family),
        ] {
            let accepted = validate_managed_oauth_credentials("p", &(lane.entry)(&r, &own));
            assert!(accepted.is_ok(), "{r} on {own}: {accepted:?}");
            for url in &refused {
                let entry = (lane.entry)(&r, url);
                let err = validate_managed_oauth_credentials("p", &entry)
                    .expect_err(&format!("{r} must be refused on {url}"));
                let msg = err.to_string();
                assert!(msg.contains("provider `p`"), "{r} on {url}: {msg}");
                for leaked in ["seat-secret-label", "oauth://", "127.0.0.1", "user:pass"] {
                    assert!(!msg.contains(leaked), "{r} on {url} leaks {leaked}: {msg}");
                }
            }
        }
    }
}

/// The xAI row's own shape cases: the login-written base and the bare host
/// are accepted (the path is not constrained); lookalikes, cleartext, smuggles
/// and the Bedrock mantle lane are refused.
#[test]
fn the_xai_token_rides_only_openai_compat_on_exactly_api_x_ai() {
    let accepted = [
        "https://api.x.ai/v1",
        "https://api.x.ai",
        "https://api.x.ai/",
        "https://API.X.AI/v1",
        "https://api.x.ai:443/v1",
        "https://api.x.ai/other/path?q=1",
    ];
    let refused = [
        "http://api.x.ai/v1",
        "https://api.x.ai.evil.test/v1",
        "https://evil-api.x.ai/v1",
        "https://x.ai/v1",
        "https://api.x.ai./v1",
        "https://api.x.ai@evil.example/v1",
        "https://evil.example\\@api.x.ai/v1",
        "https://evil.example/api.x.ai",
        "https://evil.example?h=api.x.ai",
        "http://localhost:18080/v1",
        "",
        "not a url",
    ];
    for r in ["oauth://xai", "oauth://xai#seat-secret-label"] {
        for url in accepted {
            let verdict = validate_managed_oauth_credentials("p", &xai_lane(r, url));
            assert!(verdict.is_ok(), "{r} on {url}: {verdict:?}");
        }
        for url in refused {
            let verdict = validate_managed_oauth_credentials("p", &xai_lane(r, url));
            assert!(verdict.is_err(), "{r} must be refused on {url:?}");
        }
        assert!(
            validate_managed_oauth_credentials("p", &anthropic_lane(r, "https://api.x.ai/v1"))
                .is_err(),
            "{r} on anthropic-api"
        );
    }
}

#[cfg(feature = "bedrock")]
#[test]
fn the_xai_token_is_refused_on_the_openai_compat_mantle_lane() {
    let entry = only_entry(
        "[providers.m]\n\
         kind = \"openai-compat\"\n\
         api_key_ref = \"oauth://xai\"\n\
         bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"bearer-key\", key_ref = \"env://K\" } }\n",
    );

    assert!(validate_managed_oauth_credentials("p", &entry).is_err());
}
