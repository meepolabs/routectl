//! The pricing and knobs sections: a source token for every price and for
//! the output ceiling, the price-table dates, and that no string from a
//! price table other than its prices and date reaches doctor output.

use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_router::prices::{PriceTable, parse_and_validate};
use routectl_router::{
    ModelEntry, PricingConfig, ProviderEntry, RegistryEntry, VerdictValue, derive_effective_view,
};
use routectl_testkit::ScopedEnv;
use serde_json::{Value, json};

use super::gather::{derive_knob_rows, derive_pricing_rows};
use super::render::render_human;
use super::sections::{section_knobs, section_pricing};
use super::*;
use crate::commands::pricing::PriceSnapshot;

const REFRESHED_DATE: &str = "2026-10-01";
const SHIPPED_DATE: &str = "2026-09-01";

fn table(date: &str, provider: Value) -> PriceTable {
    let doc = json!({ "schema_version": 1, "generated_at": date, "providers": [provider] });
    parse_and_validate(&serde_json::to_vec(&doc).unwrap()).expect("fixture table parses")
}

/// A refreshed table pricing two Anthropic upstreams partly, and a shipped
/// table pricing one of them in full at other figures, so each price's
/// layer shows in its source token.
fn snapshot() -> PriceSnapshot {
    let refreshed = table(
        REFRESHED_DATE,
        json!({ "id": "anthropic", "models": [
            { "id": "claude-x", "input": 3.0, "output": 15.0, "cache_read": 0.3 },
            { "id": "claude-partial", "input": 1.0, "output": 5.0 },
        ]}),
    );
    let shipped = table(
        SHIPPED_DATE,
        json!({ "id": "anthropic", "models": [
            { "id": "claude-x", "input": 9.0, "output": 9.0, "cache_read": 9.0,
              "cache_write_5m": 3.75 },
        ]}),
    );
    PriceSnapshot::from_tables(
        Some(Arc::new(refreshed)),
        Some(Box::leak(Box::new(shipped))),
    )
}

fn pricing_config() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anth".to_string(),
        ProviderEntry::anthropic_api("env://ANTH_KEY"),
    );
    cfg.providers.insert(
        "seatprov".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.providers.insert(
        "vendor".to_string(),
        ProviderEntry::openai_compat("https://example.invalid/v1", "env://VENDOR_KEY"),
    );
    let mut layered = ModelEntry::new("anth", "claude-x");
    layered.pricing = Some(PricingConfig {
        output_per_mtok: Some(20.0),
        ..Default::default()
    });
    cfg.models.insert("layered".to_string(), layered);
    cfg.models.insert(
        "registered".to_string(),
        ModelEntry::new("anth", "operator-priced"),
    );
    cfg.models.insert(
        "nothing".to_string(),
        ModelEntry::new("vendor", "unlisted-model"),
    );
    cfg.models.insert(
        "seat-complete".to_string(),
        ModelEntry::new("seatprov", "claude-x"),
    );
    cfg.models.insert(
        "seat-partial".to_string(),
        ModelEntry::new("seatprov", "claude-partial"),
    );
    cfg.models.insert(
        "seat-none".to_string(),
        ModelEntry::new("seatprov", "unlisted-model"),
    );
    cfg.registry.insert(
        "operator-priced".to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                input_per_mtok: Some(1.5),
                ..Default::default()
            }),
            provider: None,
        },
    );
    cfg
}

fn pricing_detail(findings: &[Finding], nickname: &str) -> String {
    findings
        .iter()
        .find(|f| f.section == "pricing" && f.name == nickname)
        .unwrap_or_else(|| panic!("no pricing row {nickname}: {findings:?}"))
        .detail
        .clone()
}

fn pricing_context(cfg: Config, prices: &PriceSnapshot) -> DoctorContext {
    let mut context = super::tests::ctx(cfg, None, Vec::new(), Vec::new());
    let view = derive_effective_view(&context.config, prices.refreshed(), prices.shipped());
    context.pricing = derive_pricing_rows(&context.config, &view);
    context
}

/// Every pricing row names each of the five prices with the source token
/// that gave it, and the date of each price table read.
///
/// `layered` draws one price from each layer: output from its own
/// `[models.X.pricing]`, input and cache read from the refreshed table (over
/// the shipped figures), the 5-minute write from the shipped table, and the
/// 1-hour write derived as twice the input. `registered` takes its whole
/// `[registry]` row, leaving the rest unknown; `nothing` is priced by no
/// source and reads unknown, never `$0`.
#[test]
fn each_pricing_row_names_a_source_per_price_and_the_table_dates() {
    // Arrange
    let rows: [(&str, &str); 3] = [
        (
            "layered",
            "input $3.00 (refreshed), output $20.00 (model-config), cache read $0.30 \
             (refreshed), cache write 5m $3.75 (shipped), cache write 1h $6.00 (derived)",
        ),
        (
            "registered",
            "input $1.50 (registry), output unknown (unknown), cache read unknown (unknown), \
             cache write 5m unknown (unknown), cache write 1h unknown (unknown)",
        ),
        (
            "nothing",
            "input unknown (unknown), output unknown (unknown), cache read unknown (unknown), \
             cache write 5m unknown (unknown), cache write 1h unknown (unknown)",
        ),
    ];

    // Act
    let findings = section_pricing(&pricing_context(pricing_config(), &snapshot()));

    // Assert
    assert_eq!(findings.len(), 6, "one row per configured model");
    for (nickname, fields) in rows {
        let detail = pricing_detail(&findings, nickname);
        assert!(detail.contains(fields), "{nickname}: {detail}");
        assert!(
            detail.ends_with(&format!(
                "price tables: refreshed {REFRESHED_DATE}, shipped {SHIPPED_DATE}"
            )),
            "{nickname}: {detail}"
        );
        assert!(!detail.contains("$0.00"), "{nickname}: {detail}");
    }
    assert!(pricing_detail(&findings, "nothing").starts_with("unpriced"));
    for f in &findings {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
}

/// An absent table layer reads `none` in the dates, and its prices fall to
/// the next layer.
/// A pool-backed model prices through its first member's entry, the key a
/// usage row served through the pool bills under: a `[registry]` row scoped
/// to that member prices it, one scoped to the pool name does not.
#[test]
fn a_pool_model_prices_through_its_first_member_entry() {
    let rows = [
        (
            "registry scoped to the first member",
            "seat-a",
            "input $4.50 (registry)",
        ),
        (
            "registry scoped to the pool name",
            "seats",
            "input unknown (unknown)",
        ),
    ];
    for (name, scope, expected) in rows {
        let mut cfg = Config::default();
        for seat in ["seat-a", "seat-b"] {
            cfg.providers.insert(
                seat.to_string(),
                ProviderEntry::openai_compat("https://pool.invalid/v1", "env://POOL_KEY"),
            );
        }
        cfg.pools.insert(
            "seats".to_string(),
            routectl_router::PoolEntry::new(vec!["seat-a".to_string(), "seat-b".to_string()]),
        );
        cfg.models
            .insert("pooled".to_string(), ModelEntry::new("seats", "pool-model"));
        cfg.registry.insert(
            "pool-model".to_string(),
            RegistryEntry {
                pricing: Some(PricingConfig {
                    input_per_mtok: Some(4.5),
                    ..Default::default()
                }),
                provider: Some(scope.to_string()),
            },
        );

        let findings = section_pricing(&pricing_context(cfg, &PriceSnapshot::default()));

        let detail = pricing_detail(&findings, "pooled");
        assert!(detail.contains(expected), "{name}: {detail}");
    }
}

#[test]
fn an_absent_refreshed_table_reads_none_and_the_shipped_prices_apply() {
    let shipped = snapshot().shipped();
    let prices = PriceSnapshot::from_tables(None, shipped);

    let findings = section_pricing(&pricing_context(pricing_config(), &prices));
    let detail = pricing_detail(&findings, "layered");

    assert!(
        detail.contains("input $9.00 (shipped)"),
        "the shipped figure applies: {detail}"
    );
    assert!(
        detail.ends_with(&format!(
            "price tables: refreshed none, shipped {SHIPPED_DATE}"
        )),
        "{detail}"
    );
}

/// A subscription row leads with the billed-by-seat statement and keeps its
/// API-equivalence clause: complete when every price resolves, naming the
/// unpriced dimensions when some do not, and saying nothing prices it when
/// no source does.
#[test]
fn a_subscription_row_keeps_its_equivalence_clause() {
    // Arrange
    let rows: [(&str, &str, &[&str]); 3] = [
        (
            "seat-complete",
            "usage values it at API-equivalent rates; every price resolves",
            &["withholds"],
        ),
        (
            "seat-partial",
            "usage withholds the API-equivalent value for traffic that uses unpriced \
             dimensions (cache read, cache write 5m); set them in \
             [models.seat-partial.pricing]",
            &["every price resolves", "(input"],
        ),
        (
            "seat-none",
            "usage reports NO API-equivalent value: no source prices this upstream",
            &["withholds"],
        ),
    ];

    // Act
    let findings = section_pricing(&pricing_context(pricing_config(), &snapshot()));

    // Assert
    for (nickname, clause, absent) in rows {
        let detail = pricing_detail(&findings, nickname);
        assert!(
            detail.starts_with("billed by subscription; no per-token rate applies"),
            "{nickname}: {detail}"
        );
        assert!(detail.contains(clause), "{nickname}: {detail}");
        for text in absent {
            assert!(!detail.contains(text), "{nickname} names {text}: {detail}");
        }
    }
    assert!(
        pricing_detail(&findings, "seat-partial").contains("cache write 1h $2.00 (derived)"),
        "the partial row still lists its resolved prices"
    );
}

/// A non-subscription priced row whose rates do not come from a whole
/// `[registry]` row names the dimensions usage reports as unpriced; a
/// `[registry]` row keeps counting them at $0, so it carries no clause. A
/// matching `[registry]` row that prices nothing is named as the reason the
/// model is unpriced.
#[test]
fn a_priced_row_names_the_dimensions_usage_reports_as_unpriced() {
    // Arrange
    let shipped = table(
        SHIPPED_DATE,
        json!({ "id": "anthropic", "models": [
            { "id": "claude-partial", "input": 1.0, "output": 5.0 },
        ]}),
    );
    let prices = PriceSnapshot::from_tables(None, Some(Box::leak(Box::new(shipped))));
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anth".to_string(),
        ProviderEntry::anthropic_api("env://ANTH_KEY"),
    );
    for (nickname, upstream) in [
        ("shipped-partial", "claude-partial"),
        ("registry-partial", "operator-priced"),
        ("registry-empty", "operator-empty"),
    ] {
        cfg.models
            .insert(nickname.to_string(), ModelEntry::new("anth", upstream));
    }
    for (upstream, pricing) in [
        (
            "operator-priced",
            PricingConfig {
                input_per_mtok: Some(1.5),
                ..Default::default()
            },
        ),
        ("operator-empty", PricingConfig::default()),
    ] {
        cfg.registry.insert(
            upstream.to_string(),
            RegistryEntry {
                pricing: Some(pricing),
                provider: None,
            },
        );
    }
    let clause = "usage reports no cost for traffic that uses unpriced dimensions";
    let rows: [(&str, &str, Option<&str>); 3] = [
        (
            "shipped-partial",
            "priced -- ",
            Some(
                "usage reports no cost for traffic that uses unpriced dimensions (cache read, \
                 cache write 5m); set them in [models.shipped-partial.pricing]",
            ),
        ),
        ("registry-partial", "priced -- ", None),
        (
            "registry-empty",
            "unpriced: the matching [registry] row prices nothing -- ",
            None,
        ),
    ];

    // Act
    let findings = section_pricing(&pricing_context(cfg, &prices));

    // Assert
    for (nickname, lead, expected_clause) in rows {
        let detail = pricing_detail(&findings, nickname);
        assert!(detail.starts_with(lead), "{nickname}: {detail}");
        match expected_clause {
            Some(expected) => assert!(detail.contains(expected), "{nickname}: {detail}"),
            None => assert!(!detail.contains(clause), "{nickname}: {detail}"),
        }
    }
    assert!(
        pricing_detail(&findings, "shipped-partial").contains("cache write 1h $2.00 (derived)"),
        "premise: the derived 1h write resolves and is not named"
    );
}

/// A table provider id distinct from every config string.
const FOREIGN_PROVIDER: &str = "zz-foreign-provider";
/// A table API path segment no config string carries.
const FOREIGN_PATH: &str = "zzforeignpath";
/// The table model the config's upstream matches as a lookup key.
const SENTINEL_MODEL: &str = "ZZSENTINEL-model";
/// A table model no config entry looks up.
const UNMATCHED_MODEL: &str = "ZZSENTINEL-unmatched";

fn foreign_table_json() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "generated_at": REFRESHED_DATE,
        "providers": [{
            "id": FOREIGN_PROVIDER,
            "api_host": format!("https://zz-host.example/{FOREIGN_PATH}"),
            "models": [
                { "id": SENTINEL_MODEL, "input": 7.0, "output": 11.0 },
                { "id": UNMATCHED_MODEL, "input": 1.0, "output": 1.0 },
            ],
        }],
    }))
    .unwrap()
}

/// The refreshed table in the config folder is read by the CLI doctor, and
/// nothing from it but its prices and date is rendered: the provider it
/// selects by `api_host` origin is used as a lookup key only, so neither its
/// id, its API path, nor a model no config entry names appears in the doctor
/// text or JSON. The upstream itself shows as the config's own string.
///
/// The positive control is the matched row priced from `refreshed` at the
/// table's figures, so the table was read and selected.
///
/// Mutation check: render the selected table provider's id in
/// `pricing_finding` -> red on the provider-id assertion.
#[tokio::test]
#[serial_test::serial]
async fn the_doctor_renders_no_table_string_beyond_prices_and_date() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("prices.json"), foreign_table_json()).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {}\n\
             [providers.vendor]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://zz-host.example/v1\"\n\
             api_key_ref = \"env://ROUTECTL_PRICE_FILL_TEST_KEY\"\n\
             [models.sentinel]\n\
             provider = \"vendor\"\n\
             upstream = \"{SENTINEL_MODEL}\"\n",
            routectl_router::CURRENT_CONFIG_VERSION,
        ),
    )
    .unwrap();

    // Act
    let context = gather_context_no_network(&config_path, GatherSources::Disk, &[]).await;
    let report = build_report_no_network(&context);
    let text = render_human(&report).join("\n");
    let json = serde_json::to_string(&report).unwrap();

    // Assert
    let detail = pricing_detail(&report.findings, "sentinel");
    assert!(
        detail.contains("input $7.00 (refreshed), output $11.00 (refreshed)"),
        "premise: the foreign table prices the model: {detail}"
    );
    assert!(
        detail.contains(&format!("refreshed {REFRESHED_DATE}")),
        "{detail}"
    );
    for (what, output) in [("text", &text), ("json", &json)] {
        assert!(
            !output.contains(FOREIGN_PROVIDER),
            "{what} names the table provider id"
        );
        assert!(
            !output.contains(FOREIGN_PATH),
            "{what} names the table api path"
        );
        assert!(
            !output.contains(UNMATCHED_MODEL),
            "{what} names an unmatched table model"
        );
    }
}

/// A learned entry for the `max_tokens` field verdict on `lane`.
fn learned_ceiling(
    lane: &str,
    ceiling: u32,
    now: Instant,
    expires: Instant,
) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        state_key: lane.to_string(),
        feature_key: format!("{}{}max_tokens", "fie", "ld:"),
        verdict: Verdict::LearnedBroken(FailurePhase::F1),
        signal_tier: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: now,
        last_seen: now,
        expires_at: expires,
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        recheck_owned: false,
        value: Some(VerdictValue::Ceiling(ceiling)),
    }
}

fn knob_config() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anth".to_string(),
        ProviderEntry::anthropic_api("env://ANTH_KEY"),
    );
    cfg.providers.insert(
        "vendor".to_string(),
        ProviderEntry::openai_compat("https://example.invalid/v1", "env://VENDOR_KEY"),
    );
    cfg.providers.insert(
        "masked".to_string(),
        ProviderEntry::anthropic_api("env://MASKED_KEY"),
    );
    let max_tokens_key = format!("{}{}max_tokens", "fie", "ld:");
    for spec in ["anth:masked-model", "masked"] {
        cfg.capability.overrides.insert(
            spec.to_string(),
            routectl_router::OverrideEntry {
                unsupported: Vec::new(),
                force_supported: vec![max_tokens_key.clone()],
            },
        );
    }
    let models = [
        ("configured", "anth", "up-configured", Some(32_000)),
        (
            "learned-lowers-config",
            "anth",
            "up-lowers-config",
            Some(32_000),
        ),
        (
            "learned-above-config",
            "anth",
            "up-above-config",
            Some(16_000),
        ),
        ("learned-lowers-default", "anth", "up-lowers-default", None),
        ("learned-below-floor", "anth", "up-below-floor", None),
        ("learned-lapsed", "anth", "up-lapsed", None),
        ("default-fills", "anth", "up-default", None),
        ("default-forwards", "vendor", "up-vendor", None),
        (
            "configured-forwards",
            "vendor",
            "up-vendor-configured",
            Some(32_000),
        ),
        ("masked-model", "anth", "up-masked-model", None),
        ("masked-provider", "masked", "up-masked-provider", None),
    ];
    for (nickname, provider, upstream, ceiling) in models {
        let entry = ModelEntry::new(provider, upstream);
        let entry = match ceiling {
            Some(c) => entry.with_max_output_tokens(c),
            None => entry,
        };
        cfg.models.insert(nickname.to_string(), entry);
    }
    cfg
}

/// Each knobs row names the fill the model's egress sends and its source:
/// `model-config` for the configured value, `learned` where a resident
/// learned ceiling lowers it, else `default` with the egress baseline. On an
/// egress that forwards an omitted limit it reads unset, or names a
/// configured value as sizing the window-gate reserve. A learned ceiling that would not lower the value, sits below the
/// clamp floor, has lapsed, or is masked by a `force_supported` override on
/// `max_tokens` (model or provider tier) does not apply, as at dispatch.
#[test]
fn each_knobs_row_names_model_config_learned_or_default() {
    // Arrange
    let now = Instant::now();
    let live = now + Duration::from_hours(1);
    let lapsed = now.checked_sub(Duration::from_secs(1)).expect("instant");
    let entries = vec![
        learned_ceiling("anth#up-lowers-config", 20_000, now, live),
        learned_ceiling("anth#up-above-config", 24_000, now, live),
        learned_ceiling("anth#up-lowers-default", 8_192, now, live),
        learned_ceiling("anth#up-below-floor", 2_048, now, live),
        learned_ceiling("anth#up-lapsed", 8_192, now, lapsed),
        learned_ceiling("anth#up-masked-model", 8_192, now, live),
        learned_ceiling("masked#up-masked-provider", 8_192, now, live),
    ];
    let source = CapabilityMatrixSource::Available {
        entries,
        now,
        now_ms: 1_000,
        replay: None,
        seed_clears: Vec::new(),
    };
    let rows = [
        ("configured", "max_output_tokens 32000 (model-config)"),
        (
            "learned-lowers-config",
            "max_output_tokens 20000 (learned) -- kind anthropic-api upstream up-lowers-config; \
             learned from an upstream refusal, it lowers 32000",
        ),
        (
            "learned-above-config",
            "max_output_tokens 16000 (model-config)",
        ),
        (
            "learned-lowers-default",
            "max_output_tokens 8192 (learned) -- kind anthropic-api upstream up-lowers-default; \
             learned from an upstream refusal, it lowers 64000",
        ),
        ("learned-below-floor", "max_output_tokens 64000 (default)"),
        ("learned-lapsed", "max_output_tokens 64000 (default)"),
        ("default-fills", "max_output_tokens 64000 (default)"),
        ("default-forwards", "max_output_tokens unset --"),
        (
            "configured-forwards",
            "max_output_tokens 32000 (model-config) -- kind openai-compat upstream \
             up-vendor-configured; the egress forwards an omitted limit as omitted; this value \
             sizes the window-gate reserve",
        ),
        ("masked-model", "max_output_tokens 64000 (default)"),
        ("masked-provider", "max_output_tokens 64000 (default)"),
    ];
    let mut context = super::tests::ctx(knob_config(), None, Vec::new(), Vec::new());
    let view = derive_effective_view(&context.config, None, None);
    context.knobs = derive_knob_rows(&context.config, &view, &source);

    // Act
    let findings = section_knobs(&context);

    // Assert
    assert_eq!(findings.len(), rows.len(), "one row per model");
    for (nickname, expected) in rows {
        let detail = &findings
            .iter()
            .find(|f| f.name == nickname)
            .unwrap_or_else(|| panic!("no knobs row {nickname}"))
            .detail;
        assert!(detail.starts_with(expected), "{nickname}: {detail}");
    }
    for f in &findings {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
}

fn lifetime_map(entries: &[(&str, f64)]) -> Option<std::collections::BTreeMap<String, f64>> {
    Some(
        entries
            .iter()
            .map(|(key, rate)| ((*key).to_string(), *rate))
            .collect(),
    )
}

/// One `anth`-served model per row, each carrying its own pricing block.
fn own_pricing_config(rows: &[(&str, PricingConfig)]) -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anth".to_string(),
        ProviderEntry::anthropic_api("env://ANTH_KEY"),
    );
    for (nickname, pricing) in rows {
        let mut entry = ModelEntry::new("anth", "unlisted-model");
        entry.pricing = Some(pricing.clone());
        cfg.models.insert((*nickname).to_string(), entry);
    }
    cfg
}

/// A model priced for 5m, 1h and 30m writes lists a segment per lifetime
/// with its source, the 30m one after the fixed five; a lifetime priced in
/// a `[registry]` row carries the registry source.
#[test]
fn each_extra_priced_write_lifetime_gets_its_own_segment() {
    let mut cfg = own_pricing_config(&[(
        "lifetimes",
        PricingConfig {
            input_per_mtok: Some(3.0),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_per_mtok: lifetime_map(&[("1h", 6.0), ("30m", 4.5)]),
            ..Default::default()
        },
    )]);
    cfg.models.insert(
        "registered".to_string(),
        ModelEntry::new("anth", "operator-priced"),
    );
    cfg.registry.insert(
        "operator-priced".to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                cache_write_per_mtok: lifetime_map(&[("30m", 2.25)]),
                ..Default::default()
            }),
            provider: None,
        },
    );
    let rows = [
        (
            "lifetimes",
            "input $3.00 (model-config), output unknown (unknown), cache read unknown \
             (unknown), cache write 5m $3.75 (model-config), cache write 1h $6.00 \
             (model-config), cache write 30m $4.50 (model-config); ",
        ),
        (
            "registered",
            "cache write 5m unknown (unknown), cache write 1h unknown (unknown), cache write \
             30m $2.25 (registry); ",
        ),
    ];

    let findings = section_pricing(&pricing_context(cfg, &PriceSnapshot::default()));

    for (nickname, fields) in rows {
        let detail = pricing_detail(&findings, nickname);
        assert!(detail.contains(fields), "{nickname}: {detail}");
    }
}

/// A model with legacy prices only renders the exact row it rendered before
/// lifetime-keyed prices existed: five segments, no conflict note.
#[test]
fn a_legacy_only_pricing_row_renders_unchanged() {
    let cfg = own_pricing_config(&[(
        "legacy",
        PricingConfig {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(15.0),
            cache_read_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: Some(6.0),
            ..Default::default()
        },
    )]);

    let findings = section_pricing(&pricing_context(cfg, &PriceSnapshot::default()));

    assert_eq!(
        pricing_detail(&findings, "legacy"),
        "priced -- provider anth (kind anthropic-api) upstream unlisted-model; input $3.00 \
         (model-config), output $15.00 (model-config), cache read $0.30 (model-config), \
         cache write 5m $3.75 (model-config), cache write 1h $6.00 (model-config); price \
         tables: refreshed none, shipped none"
    );
}

/// A lifetime set under both spellings at different values gets a note naming
/// the legacy key as the one in force; equal values get none. Either way the
/// row stays Pass and prices the lifetime at the legacy value.
#[test]
fn a_double_spelled_write_lifetime_notes_that_the_legacy_key_wins() {
    let rows: [(&str, PricingConfig, Option<&str>); 3] = [
        (
            "differ-5m",
            PricingConfig {
                cache_write_5m_per_mtok: Some(3.75),
                cache_write_per_mtok: lifetime_map(&[("5m", 4.0)]),
                ..Default::default()
            },
            Some(
                "[models.differ-5m.pricing] prices the 5m cache write twice: \
                 cache_write_5m_per_mtok $3.75 is used and cache_write_per_mtok.5m $4.00 is \
                 ignored (the legacy key wins)",
            ),
        ),
        (
            "differ-1h",
            PricingConfig {
                cache_write_1h_per_mtok: Some(6.0),
                cache_write_per_mtok: lifetime_map(&[("60m", 7.5)]),
                ..Default::default()
            },
            Some(
                "[models.differ-1h.pricing] prices the 1h cache write twice: \
                 cache_write_1h_per_mtok $6.00 is used and cache_write_per_mtok.60m $7.50 is \
                 ignored (the legacy key wins)",
            ),
        ),
        (
            "equal-5m",
            PricingConfig {
                cache_write_5m_per_mtok: Some(3.75),
                cache_write_per_mtok: lifetime_map(&[("5m", 3.75)]),
                ..Default::default()
            },
            None,
        ),
    ];
    let cfg = own_pricing_config(
        &rows
            .iter()
            .map(|(nickname, pricing, _)| (*nickname, pricing.clone()))
            .collect::<Vec<_>>(),
    );

    let findings = section_pricing(&pricing_context(cfg, &PriceSnapshot::default()));

    for (nickname, pricing, note) in &rows {
        let detail = pricing_detail(&findings, nickname);
        match note {
            Some(note) => assert!(detail.contains(note), "{nickname}: {detail}"),
            None => assert!(!detail.contains("twice"), "{nickname}: {detail}"),
        }
        let legacy = pricing
            .cache_write_5m_per_mtok
            .map(|rate| format!("cache write 5m ${rate:.2} (model-config)"))
            .or_else(|| {
                pricing
                    .cache_write_1h_per_mtok
                    .map(|rate| format!("cache write 1h ${rate:.2} (model-config)"))
            })
            .expect("each row sets a legacy key");
        assert!(detail.contains(&legacy), "{nickname}: {detail}");
        let finding = findings
            .iter()
            .find(|f| f.name == *nickname)
            .expect("finding");
        assert_eq!(finding.status, Status::Pass, "{nickname}");
    }
}

/// The note also covers a matching `[registry]` row's pricing block.
#[test]
fn a_double_spelled_registry_lifetime_is_noted_against_the_registry_row() {
    let mut cfg = own_pricing_config(&[]);
    cfg.models.insert(
        "registered".to_string(),
        ModelEntry::new("anth", "operator-priced"),
    );
    cfg.registry.insert(
        "operator-priced".to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                cache_write_1h_per_mtok: Some(6.0),
                cache_write_per_mtok: lifetime_map(&[("1h", 5.0)]),
                ..Default::default()
            }),
            provider: None,
        },
    );

    let findings = section_pricing(&pricing_context(cfg, &PriceSnapshot::default()));

    let detail = pricing_detail(&findings, "registered");
    assert!(
        detail.contains(
            "the matching [registry] row prices the 1h cache write twice: \
             cache_write_1h_per_mtok $6.00 is used and cache_write_per_mtok.1h $5.00 is ignored"
        ),
        "{detail}"
    );
}
