use std::collections::BTreeSet;

use routectl_core::identity::codex::PINNED_CODEX_VERSION;
use routectl_router::Config;
use serde_json::Value as Json;

use super::{CODE_DEFAULTS, CodeDefault, DefaultScope, redacted_settings_lines};

fn lines_for(raw_text: &str) -> Vec<String> {
    let config: Config = toml::from_str(raw_text).expect("fixture config parses");
    redacted_settings_lines(&config, raw_text).expect("settings render")
}

/// `path=value(source)` -> (`path`, `source`).
fn path_and_source(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    let (path, _) = line.split_once('=').expect("line has `=`");
    let source = line
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.strip_suffix(')'))
        .expect("line ends with `(source)`");
    (path, source)
}

/// Sets about half of the keys of each block it touches; every other key
/// of those blocks keeps its code default.
const HALF_SET_FIXTURE: &str = r#"
version = 2

[server]
port = 9000
host = "127.0.0.1"
strict_translation = true

[retry]
max_attempts = 5
jitter_ms = 10
request_timeout_ms = 120000

[usage]
retention_days = 30

[cache]
normalize_tools = false

[capability]
decay_hours = 24
enabled = true
inferred_window_hours = 2

[trim]
trigger_tokens = 50000
head_keep_messages = 3

[log]
trace_headers = true

[providers.relay]
kind = "openai-compat"
base_url = "https://relay.example/v1"
api_key_ref = "env://RELAY_KEY"
rpm_limit = 60

[models.fast]
provider = "relay"
upstream = "fast-1"
"#;

#[test]
fn a_fixture_setting_half_the_keys_tags_each_set_key_file_and_each_unset_key_default() {
    let expected_file: BTreeSet<&str> = [
        "version",
        "server.port",
        "server.host",
        "server.strict_translation",
        "retry.max_attempts",
        "retry.jitter_ms",
        "retry.request_timeout_ms",
        "usage.retention_days",
        "cache.normalize_tools",
        "capability.decay_hours",
        "capability.enabled",
        "capability.inferred_window_hours",
        "trim.trigger_tokens",
        "trim.head_keep_messages",
        "log.trace_headers",
        "providers.relay.kind",
        "providers.relay.base_url",
        "providers.relay.api_key_ref",
        "providers.relay.rpm_limit",
        "models.fast.provider",
        "models.fast.upstream",
    ]
    .into();
    let expected_default: BTreeSet<&str> = [
        "cache.auto_emit_top_level_breakpoint",
        "cache.k_gated_emission",
        "calibration.enabled",
        "capability.essential",
        "capability.staleness_hint_days",
        "fidelity.prefix_impact_opt_in",
        "log.redact_prompts",
        "log.trace_body_bytes",
        "models.fast.selectable",
        "models.fast.visible_routectl_provider",
        "reduction.enabled",
        "retry.backoff_multiplier",
        "retry.initial_backoff_ms",
        "retry.max_honored_retry_after_ms",
        "retry.probe_max_tokens",
        "seat_quota.enabled",
        "server.allow_disable_fallbacks",
        "server.max_body_bytes",
        "trim.clear_at_least_tokens",
        "trim.keep_recent_messages",
        "usage.db_path",
        "usage.enabled",
        "window_gate.enabled",
        "wire_check.first_n",
        "wire_check.sample_one_in",
        "wire_check.window_secs",
    ]
    .into();

    let lines = lines_for(HALF_SET_FIXTURE);
    let tagged: Vec<(&str, &str)> = lines.iter().map(|l| path_and_source(l)).collect();

    let file: BTreeSet<&str> = tagged
        .iter()
        .filter(|(_, source)| *source == "file")
        .map(|(path, _)| *path)
        .collect();
    assert_eq!(file, expected_file, "file-tagged keys; lines:\n{lines:#?}");
    let default: BTreeSet<&str> = tagged
        .iter()
        .filter(|(_, source)| *source == "default")
        .map(|(path, _)| *path)
        .collect();
    assert_eq!(
        default, expected_default,
        "default-tagged keys; lines:\n{lines:#?}"
    );
    assert_eq!((file.len(), default.len()), (21, 26));
    assert_eq!(
        lines.len(),
        47,
        "every line is tagged file or default, once"
    );
}

#[test]
fn an_unset_code_default_key_prints_its_value_from_code() {
    let rows = [
        (
            "retry ceiling",
            "  retry.max_honored_retry_after_ms=3600000(default)",
        ),
        ("trace body cap", "  log.trace_body_bytes=16384(default)"),
        ("trace headers", "  log.trace_headers=true(file)"),
        ("redact prompts", "  log.redact_prompts=false(default)"),
    ];
    let lines = lines_for(HALF_SET_FIXTURE);
    for (name, expected) in rows {
        assert!(
            lines.iter().any(|l| l == expected),
            "row `{name}`: `{expected}` missing; lines:\n{lines:#?}"
        );
    }
}

#[test]
fn a_provider_kind_code_default_applies_only_to_entries_of_that_kind() {
    let raw = r#"
[providers.claude]
kind = "anthropic-api"
api_key_ref = "env://K"

[providers.claude-small]
kind = "anthropic-api"
api_key_ref = "env://K"
max_thinking_entry_bytes = 4096

[providers.compat]
kind = "openai-compat"
base_url = "https://compat.example/v1"
api_key_ref = "env://K"
"#;

    let lines = lines_for(raw);

    let thinking: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("max_thinking_entry_bytes"))
        .collect();
    assert_eq!(
        thinking,
        [
            "  providers.claude.max_thinking_entry_bytes=1048576(default)",
            "  providers.claude-small.max_thinking_entry_bytes=4096(file)",
        ],
        "lines:\n{lines:#?}"
    );
}

#[test]
fn an_unset_codex_version_reads_the_version_the_process_runs_under() {
    let rows = [
        ("no entry sets one", "", PINNED_CODEX_VERSION),
        (
            "another entry sets one",
            "[providers.pinned]\nkind = \"openai-responses\"\napi_key_ref = \"env://K\"\ncodex_version = \"9.9.9\"\n",
            "9.9.9",
        ),
    ];
    for (name, extra, expected) in rows {
        let raw = format!(
            "[providers.plain]\nkind = \"openai-responses\"\napi_key_ref = \"env://K\"\n{extra}"
        );

        let lines = lines_for(&raw);

        let line = format!("  providers.plain.codex_version=\"{expected}\"(default)");
        assert!(
            lines.contains(&line),
            "row `{name}`: `{line}` missing; lines:\n{lines:#?}"
        );
    }
}

#[test]
fn a_credential_bearing_base_url_prints_only_its_origin() {
    let raw = r#"
[providers.relay]
kind = "anthropic-api"
api_key_ref = "literal:sk-ant-FAKE-KEY"
base_url = "https://svc:sk-userinfo-FAKE@internal.example:8443/v1?key=sk-query-FAKE"
"#;

    let lines = lines_for(raw);
    let block = lines.join("\n");

    assert!(
        lines
            .iter()
            .any(|l| l == "  providers.relay.base_url=\"https://internal.example:8443\"(file)"),
        "the origin must still be shown; block:\n{block}"
    );
    for secret in [
        "sk-userinfo-FAKE",
        "sk-query-FAKE",
        "sk-ant-FAKE-KEY",
        "svc:",
        "?key=",
        "/v1",
    ] {
        assert!(
            !block.contains(secret),
            "`{secret}` must not reach the settings block; block:\n{block}"
        );
    }
}

#[test]
fn every_spelling_of_a_key_reads_file() {
    let rows = [
        ("section header", "[server]\nport = 9000\n"),
        ("dotted key", "server.port = 9000\n"),
        ("inline table", "server = { port = 9000 }\n"),
        (
            "nested dotted key in a section",
            "[retry]\nclasses.timeout.retry = 1\n",
        ),
        (
            "nested inline table",
            "[retry]\nclasses = { timeout = { retry = 1 } }\n",
        ),
    ];
    for (name, raw) in rows {
        let lines = lines_for(raw);
        let expected = if raw.contains("port") {
            "  server.port=9000(file)"
        } else {
            "  retry.classes.timeout.retry=1(file)"
        };
        assert!(
            lines.iter().any(|l| l == expected),
            "row `{name}`: `{expected}` missing; lines:\n{lines:#?}"
        );
    }
}

#[test]
fn a_key_path_segment_that_is_not_a_bare_key_is_quoted() {
    let raw = "[aliases]\n\"claude-opus-*\" = \"fast\"\n";

    let lines = lines_for(raw);

    assert!(
        lines
            .iter()
            .any(|l| l == "  aliases.\"claude-opus-*\"=\"fast\"(file)"),
        "lines:\n{lines:#?}"
    );
}

/// Option keys resolved in the effective view's resolved tuning block.
const TUNING_BLOCK: &[&str] = &[
    "ModelEntry.stream_first_byte_timeout_ms",
    "PoolEntry.circuit_cooldown_ms",
    "PoolEntry.circuit_failures",
    "PoolEntry.request_timeout_ms",
    "PoolEntry.rpm_limit",
    "PoolEntry.stream_first_byte_timeout_ms",
    "ProviderEntry.circuit_cooldown_ms",
    "ProviderEntry.circuit_failures",
    "ProviderEntry.request_timeout_ms",
    "ProviderEntry.rpm_limit",
    "ProviderEntry.stream_first_byte_timeout_ms",
    "RetryPolicy.request_timeout_ms",
    "RetryPolicy.stream_first_byte_timeout_ms",
];

/// Option keys whose unset value means the feature is off, or that the
/// value is inherited from another table, kind rule or effective-view
/// section; each names what unset means.
const UNSET_MEANS_OFF_OR_INHERIT: &[(&str, &str)] = &[
    (
        "BedrockCredsConfig.session_token_ref",
        "off: no session token",
    ),
    (
        "ClassPolicy.fallback",
        "inherit: retry class policy section",
    ),
    ("ClassPolicy.retry", "inherit: retry class policy section"),
    ("Config.mitm", "off: no MITM listener"),
    ("MitmConfig.tested_cc_version", "off: no version check"),
    (
        "ModelEntry.context_window",
        "off: window unknown, model facts section",
    ),
    (
        "ModelEntry.history_reasoning",
        "inherit: the dialect's own rule",
    ),
    (
        "ModelEntry.max_output_tokens",
        "inherit: egress fill, model facts section",
    ),
    (
        "ModelEntry.pricing",
        "inherit: price tables, model facts section",
    ),
    (
        "ModelEntry.reasoning_dialect",
        "inherit: the egress dialect",
    ),
    ("ModelEntry.reported_model", "off: echo the requested model"),
    ("PricingConfig.cache_read_per_mtok", "inherit: price tables"),
    (
        "PricingConfig.cache_write_1h_per_mtok",
        "inherit: price tables",
    ),
    (
        "PricingConfig.cache_write_5m_per_mtok",
        "inherit: price tables",
    ),
    ("PricingConfig.input_per_mtok", "inherit: price tables"),
    ("PricingConfig.output_per_mtok", "inherit: price tables"),
    ("ProviderEntry.account_id_ref", "off: no ChatGPT account"),
    (
        "ProviderEntry.auto_emit_per_block_breakpoints",
        "inherit: kind rule",
    ),
    (
        "ProviderEntry.auto_emit_top_level_breakpoint",
        "inherit: [cache]",
    ),
    ("ProviderEntry.base_url", "inherit: the auth_kind endpoint"),
    ("ProviderEntry.bedrock_mantle", "off: no mantle lane"),
    ("ProviderEntry.cache_capability", "inherit: kind rule"),
    ("ProviderEntry.cloud_project_id", "off: project discovery"),
    ("ProviderEntry.reduction_enabled", "inherit: [reduction]"),
    ("ProviderEntry.user_agent", "inherit: the egress User-Agent"),
    ("RegistryEntry.pricing", "off: row unpriced"),
    (
        "RegistryEntry.provider",
        "off: row applies to every provider",
    ),
    (
        "RetryPolicy.retry_on_429",
        "inherit: retry class policy section",
    ),
    (
        "RetryPolicy.retry_on_5xx",
        "inherit: retry class policy section",
    ),
    (
        "RetryPolicy.retry_on_network",
        "inherit: retry class policy section",
    ),
    ("ServerConfig.auth", "off: no listener auth"),
];

fn is_nullable(property: &Json) -> bool {
    let type_has_null = match property.get("type") {
        Some(Json::Array(types)) => types.iter().any(|t| t == "null"),
        _ => false,
    };
    let variant_is_null = ["anyOf", "oneOf"].iter().any(|k| {
        property.get(*k).and_then(Json::as_array).is_some_and(|vs| {
            vs.iter()
                .any(|v| v.get("type") == Some(&Json::from("null")))
        })
    });
    type_has_null || variant_is_null
}

/// Every nullable property of `object` and of each of its tagged variants,
/// as `<owner>.<field>`.
fn nullable_properties(owner: &str, object: &Json, out: &mut BTreeSet<String>) {
    if let Some(properties) = object.get("properties").and_then(Json::as_object) {
        for (field, property) in properties {
            if is_nullable(property) {
                out.insert(format!("{owner}.{field}"));
            }
        }
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        for variant in object
            .get(key)
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            nullable_properties(owner, variant, out);
        }
    }
}

fn schema_option_keys() -> BTreeSet<String> {
    let schema: Json = serde_json::from_str(&routectl_router::schema_gen::render_schema_json())
        .expect("schema is JSON");
    let mut keys = BTreeSet::new();
    nullable_properties("Config", &schema, &mut keys);
    let defs = schema
        .get("$defs")
        .and_then(Json::as_object)
        .expect("schema has $defs");
    for (name, def) in defs {
        nullable_properties(name, def, &mut keys);
    }
    keys
}

/// A code-default entry as the census names it: `<schema type>.<field>`.
fn census_key(default: &CodeDefault) -> String {
    let owner = match default.scope {
        DefaultScope::Section("retry") => "RetryPolicy",
        DefaultScope::Section("log") => "LogConfig",
        DefaultScope::ProviderKind(_) => "ProviderEntry",
        DefaultScope::Section(other) => panic!("no schema type mapped for [{other}]"),
    };
    format!("{owner}.{}", default.key)
}

#[test]
fn every_option_key_in_the_config_schema_has_a_known_unset_meaning() {
    let schema_keys = schema_option_keys();
    let mut classified: Vec<&str> = TUNING_BLOCK.to_vec();
    classified.extend(UNSET_MEANS_OFF_OR_INHERIT.iter().map(|(key, _)| *key));
    let code_defaults: Vec<String> = CODE_DEFAULTS.iter().map(census_key).collect();
    classified.extend(code_defaults.iter().map(String::as_str));

    let mut seen = BTreeSet::new();
    let duplicated: Vec<&str> = classified
        .iter()
        .copied()
        .filter(|key| !seen.insert(*key))
        .collect();
    let unclassified: Vec<&String> = schema_keys
        .iter()
        .filter(|key| !seen.contains(key.as_str()))
        .collect();
    let stale: Vec<&&str> = seen
        .iter()
        .filter(|key| !schema_keys.contains(**key))
        .collect();

    assert!(
        unclassified.is_empty(),
        "Option keys with no entry: add each to the tuning block list, the code-default \
         table or the unset-means-off-or-inherit list: {unclassified:?}"
    );
    assert!(stale.is_empty(), "entries naming no schema key: {stale:?}");
    assert!(
        duplicated.is_empty(),
        "keys classified twice: {duplicated:?}"
    );
}

/// One credential-bearing field class: a fixture planting a distinct fake
/// secret, and the redacted line each surface must still print (key name
/// plus sentinel) so the absence is not vacuous.
struct SecretRow {
    name: &'static str,
    raw: &'static str,
    secrets: &'static [&'static str],
    settings_line: &'static str,
    show_line: &'static str,
}

const SECRET_ROWS: &[SecretRow] = &[
    SecretRow {
        name: "provider header_extras",
        raw: "[providers.gw]\nkind = \"openai-compat\"\nbase_url = \"https://gw.example\"\n\
              api_key_ref = \"env://K\"\n[providers.gw.header_extras]\n\
              cf-aig-authorization = \"Bearer sk-header-FAKE\"\n",
        secrets: &["sk-header-FAKE"],
        settings_line: "  providers.gw.header_extras.cf-aig-authorization=\"[REDACTED]\"(file)",
        show_line: "cf-aig-authorization = \"[REDACTED]\"",
    },
    SecretRow {
        name: "model header_extras",
        raw: "[models.m]\nprovider = \"gw\"\nupstream = \"u\"\n[models.m.header_extras]\n\
              helicone-auth = \"Bearer sk-modelheader-FAKE\"\n",
        secrets: &["sk-modelheader-FAKE"],
        settings_line: "  models.m.header_extras.helicone-auth=\"[REDACTED]\"(file)",
        show_line: "helicone-auth = \"[REDACTED]\"",
    },
    SecretRow {
        name: "provider payload_extras nested string",
        raw: "[providers.gw]\nkind = \"openai-compat\"\nbase_url = \"https://gw.example\"\n\
              api_key_ref = \"env://K\"\n[providers.gw.payload_extras.metadata]\n\
              api_token = \"sk-payload-FAKE\"\ntags = [\"sk-payload-array-FAKE\"]\n",
        secrets: &["sk-payload-FAKE", "sk-payload-array-FAKE"],
        settings_line: "  providers.gw.payload_extras.metadata.api_token=\"[REDACTED]\"(file)",
        show_line: "api_token = \"[REDACTED]\"",
    },
    SecretRow {
        name: "model payload_extras",
        raw: "[models.m]\nprovider = \"gw\"\nupstream = \"u\"\n[models.m.payload_extras]\n\
              user = \"sk-modelpayload-FAKE\"\n",
        secrets: &["sk-modelpayload-FAKE"],
        settings_line: "  models.m.payload_extras.user=\"[REDACTED]\"(file)",
        show_line: "user = \"[REDACTED]\"",
    },
    SecretRow {
        name: "model additional_request_fields",
        raw: "[models.m]\nprovider = \"gw\"\nupstream = \"u\"\n\
              [models.m.additional_request_fields]\nsession = \"sk-addfields-FAKE\"\n",
        secrets: &["sk-addfields-FAKE"],
        settings_line: "  models.m.additional_request_fields.session=\"[REDACTED]\"(file)",
        show_line: "session = \"[REDACTED]\"",
    },
    SecretRow {
        name: "mantle creds literal",
        raw: "[providers.mantle]\nkind = \"openai-compat\"\napi_key_ref = \"\"\n\
              [providers.mantle.bedrock_mantle]\nregion = \"us-west-2\"\n\
              [providers.mantle.bedrock_mantle.creds]\nkind = \"bearer-key\"\n\
              key_ref = \"literal:sk-mantle-FAKE\"\n",
        secrets: &["sk-mantle-FAKE"],
        settings_line: "  providers.mantle.bedrock_mantle.creds.key_ref=\"literal:[REDACTED]\"(file)",
        show_line: "key_ref = \"literal:[REDACTED]\"",
    },
    SecretRow {
        name: "server.auth.tokens literal",
        raw: "[server.auth]\ntokens = [\"literal:sk-listener-FAKE\", \"env://LISTENER\"]\n",
        secrets: &["sk-listener-FAKE"],
        settings_line: "  server.auth.tokens=[\"literal:[REDACTED]\", \"env://LISTENER\"](file)",
        show_line: "\"literal:[REDACTED]\",",
    },
    SecretRow {
        name: "bare api_key_ref",
        raw: "[providers.gw]\nkind = \"openai-compat\"\nbase_url = \"https://gw.example\"\n\
              api_key_ref = \"sk-bare-FAKE\"\n",
        secrets: &["sk-bare-FAKE"],
        settings_line: "  providers.gw.api_key_ref=\"[REDACTED]\"(file)",
        show_line: "api_key_ref = \"[REDACTED]\"",
    },
];

#[test]
fn no_credential_bearing_value_prints_from_show_or_the_settings_block() {
    for row in SECRET_ROWS {
        let config: Config = toml::from_str(row.raw).expect("fixture config parses");

        let block = lines_for(row.raw).join("\n");
        let shown = crate::commands::config::render_show(&config).expect("show renders");

        assert!(
            block.lines().any(|l| l == row.settings_line),
            "row `{}`: `{}` missing; block:\n{block}",
            row.name,
            row.settings_line
        );
        assert!(
            shown.lines().any(|l| l.trim() == row.show_line),
            "row `{}`: `{}` missing; shown:\n{shown}",
            row.name,
            row.show_line
        );
        for secret in row.secrets {
            assert!(
                !block.contains(secret),
                "row `{}`: `{secret}` reached the settings block:\n{block}",
                row.name
            );
            assert!(
                !shown.contains(secret),
                "row `{}`: `{secret}` reached `config show`:\n{shown}",
                row.name
            );
        }
    }
}
