use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::Local;
use routectl_auth::{OAuthError, OAuthStore};
use routectl_router::{
    AliasValue, CURRENT_CONFIG_VERSION, CatalogImportState, CatalogOverlay, ModelEntry,
    ProviderEntry,
};
use routectl_testkit::ScopedEnv;

use crate::commands::capability_legacy::present_legacy_capability_keys;
use crate::commands::parse_error_redaction::redact_config_load_error;

use super::gather::{
    build_capability_inputs, derive_prior_cells, gather_capability_matrix, gather_context,
    gather_context_no_network, gather_orphan_secrets, gather_secret_checks,
    sanitize_store_open_error,
};
use super::sections::{
    freshness_findings, learned_line, legacy_nudge, secret_finding, section_auth,
    section_capability, section_config, section_probe, section_secret_orphans, section_version,
};
use super::*;

fn token_record(expires_at: u64) -> TokenRecord {
    token_record_with_refresh(expires_at, "rtok")
}

/// A seat whose access token is expired AND carries no refresh token,
/// so it is genuinely unusable without a fresh login.
fn token_record_no_refresh(expires_at: u64) -> TokenRecord {
    token_record_with_refresh(expires_at, "")
}

fn token_record_with_refresh(expires_at: u64, refresh: &str) -> TokenRecord {
    let json = serde_json::json!({
        "access_token": "tok",
        "refresh_token": refresh,
        "token_type": "Bearer",
        "expires_at_unix": expires_at,
        "scopes": ["user:inference"],
        "account": { "email": "a@example.com", "account_id": "acct-x" },
        "obtained_at_unix": 0,
    });
    serde_json::from_value(json).expect("valid TokenRecord json")
}

fn ctx(
    config: Config,
    raw_config: Option<&str>,
    probes: Vec<(&'static str, LocalProbe)>,
    seats: Vec<(String, TokenRecord)>,
) -> DoctorContext {
    let capability = build_capability_inputs(&config, None, Some(CatalogOverlay::default()));
    DoctorContext {
        config,
        raw_config: raw_config.map(str::to_string),
        config_load_error: None,
        probes,
        seats,
        auth_store_error: None,
        secret_checks: Vec::new(),
        orphan_secrets: Vec::new(),
        probe_results: Vec::new(),
        would_trim: None,
        now_unix: 1_000,
        binary_version: "test",
        capability,
        capability_matrix: CapabilityMatrixSource::Unavailable("no_data"),
        freshness: sample_freshness(),
    }
}

/// A minimal all-honest-missing freshness input: no overlay verification and
/// no recorded import, pinned to a fixed "today" so age assertions are
/// deterministic. Individual freshness tests override the fields they probe.
fn sample_freshness() -> FreshnessInputs {
    FreshnessInputs {
        catalog_version: routectl_router::CATALOG_VERSION,
        snapshot_date: routectl_router::CATALOG_SNAPSHOT_DATE,
        overlay_verified_at: None,
        staleness_hint_days: 14,
        today_epoch_day: FRESHNESS_TODAY,
        last_import: None,
        import_result: None,
    }
}

/// A fixed "today" epoch-day (2026-07-11) for deterministic freshness ages.
const FRESHNESS_TODAY: i64 = 20_645;

/// A config whose alias table reaches an `oauth://anthropic`-backed
/// provider, so the anthropic inventory entry is `referenced_by_aliases`.
fn config_referencing_anthropic() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.models.insert(
        "sonnet".to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    cfg.aliases.insert(
        "default".to_string(),
        AliasValue::Single("sonnet".to_string()),
    );
    cfg
}

fn find<'a>(findings: &'a [Finding], section: &str, name: &str) -> &'a Finding {
    findings
        .iter()
        .find(|f| f.section == section && f.name == name)
        .unwrap_or_else(|| panic!("missing finding {section}/{name}"))
}

#[test]
fn every_warn_or_fail_finding_carries_remediation() {
    let cfg = config_referencing_anthropic();
    let context = ctx(
        cfg,
        Some("version = 1\n"),
        vec![("anthropic", LocalProbe::Missing)],
        Vec::new(),
    );
    let report = build_report(&context);
    for f in &report.findings {
        if matches!(f.status, Status::Warn | Status::Fail) {
            assert!(
                f.remediation.is_some(),
                "{}/{} is {:?} but has no remediation",
                f.section,
                f.name,
                f.status
            );
        }
    }
}

#[test]
fn activated_provider_maps_to_pass_without_remediation() {
    let cfg = config_referencing_anthropic();
    let context = ctx(
        cfg,
        Some("version = 3\n"),
        vec![("anthropic", LocalProbe::Present)],
        Vec::new(),
    );
    let report = build_report(&context);
    let f = find(&report.findings, "inventory", "anthropic");
    assert_eq!(f.status, Status::Pass);
    assert!(f.remediation.is_none());
}

#[test]
fn referenced_unresolved_provider_warns_with_login_remediation() {
    let cfg = config_referencing_anthropic();
    let context = ctx(
        cfg,
        Some("version = 3\n"),
        vec![("anthropic", LocalProbe::Missing)],
        Vec::new(),
    );
    let report = build_report(&context);
    let f = find(&report.findings, "inventory", "anthropic");
    assert_eq!(f.status, Status::Warn);
    assert!(
        f.remediation
            .as_deref()
            .unwrap()
            .contains("routectl login anthropic"),
        "expected login remediation, got {:?}",
        f.remediation
    );
}

#[test]
fn unreferenced_unresolved_provider_is_pass_without_remediation() {
    let context = ctx(
        Config::default(),
        Some("version = 3\n"),
        vec![("anthropic", LocalProbe::Missing)],
        Vec::new(),
    );
    let report = build_report(&context);
    let f = find(&report.findings, "inventory", "anthropic");
    assert_eq!(f.status, Status::Pass);
    assert!(f.remediation.is_none());
}

#[test]
fn version_too_old_fails_with_migrate_remediation() {
    let context = ctx(
        Config::default(),
        Some("version = 1\n"),
        Vec::new(),
        Vec::new(),
    );
    let findings = section_version(&context);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].status, Status::Fail);
    assert!(
        findings[0]
            .remediation
            .as_deref()
            .unwrap()
            .contains("config migrate"),
        "expected migrate remediation, got {:?}",
        findings[0].remediation
    );
}

#[test]
fn version_current_passes() {
    let raw = format!("version = {CURRENT_CONFIG_VERSION}\n");
    let context = ctx(Config::default(), Some(&raw), Vec::new(), Vec::new());
    let findings = section_version(&context);
    assert_eq!(findings[0].status, Status::Pass);
    assert!(findings[0].remediation.is_none());
}

#[test]
fn present_but_broken_config_reports_fail_not_pass() {
    // preflight reads only the `version` key and returns Ok, but the
    // typed load failed for another reason: the section must Fail, not
    // pass on the raw version -- a present-but-broken config is not
    // healthy.
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.config_load_error = Some("config parse error: unknown field `bogus`".to_string());
    let findings = section_version(&context);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].status, Status::Fail);
    assert!(
        findings[0].detail.contains("bogus"),
        "expected the real load error in the detail, got {:?}",
        findings[0].detail
    );
    assert!(findings[0].remediation.is_some());
}

#[test]
fn corrupted_store_reports_error_not_no_seats() {
    // A store that fails to open must surface its actionable error, not
    // be mislabeled as "nobody logged in".
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.auth_store_error =
        Some("oauth credentials file at /x/credentials.json is corrupted: bad json".to_string());
    let findings = section_auth(&context);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].status, Status::Fail);
    assert!(
        findings[0].detail.contains("corrupted"),
        "expected the store error in the detail, got {:?}",
        findings[0].detail
    );
    assert!(
        !findings[0].detail.contains("no oauth providers"),
        "must not fall back to the generic no-seats message"
    );
    assert!(findings[0].remediation.is_some());
}

#[test]
fn no_seats_warns_without_inheriting_exit_two() {
    let context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let findings = section_auth(&context);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].status, Status::Warn);
    // The no-seat state is a Warn -> overall_exit is 0, NOT whoami's 2.
    assert_eq!(overall_exit(&findings), 0);
}

#[test]
fn expired_seat_warns_valid_seat_passes() {
    let seats = vec![
        ("anthropic".to_string(), token_record(2_000)),
        ("codex#work".to_string(), token_record_no_refresh(500)),
    ];
    let context = ctx(Config::default(), Some("version = 3\n"), Vec::new(), seats);
    let findings = section_auth(&context);
    assert_eq!(find(&findings, "auth", "anthropic").status, Status::Pass);
    let expired = find(&findings, "auth", "codex#work");
    assert_eq!(expired.status, Status::Warn);
    assert!(
        expired
            .remediation
            .as_deref()
            .unwrap()
            .contains("routectl login codex")
    );
}

#[tokio::test]
async fn one_credential_state_agrees_across_report_surfaces() {
    // A credential with an expired access token but a stored refresh
    // token is locally usable -- it can be renewed without a fresh
    // login. Every report surface derives that one state from the shared
    // `is_locally_usable` predicate (config / inventory via the real
    // `probe_local`, auth directly), so they cannot contradict one
    // another for the same credential.
    use routectl_auth::oauth::types::CredentialsFile;

    let now = routectl_auth::oauth::types::unix_now();
    let rec = token_record(now.saturating_sub(100));

    // Seed a real store on disk and take the live probe, so the
    // config/inventory surfaces are exercised through the actual
    // derivation rather than a hand-rolled stand-in.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    let mut file = CredentialsFile::empty();
    file.upsert("anthropic", rec.clone());
    std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let store = OAuthStore::open(&path).await.unwrap();
    let probe = store.probe_local("anthropic").await;
    assert_eq!(
        probe,
        LocalProbe::Present,
        "an expired-but-refreshable seat probes as Present"
    );

    let cfg = config_referencing_anthropic();
    let secret_checks = gather_secret_checks(&cfg, &[("anthropic", probe)]);
    let mut context = ctx(
        cfg,
        Some("version = 3\n"),
        vec![("anthropic", probe)],
        vec![("anthropic".to_string(), rec)],
    );
    context.secret_checks = secret_checks;
    context.now_unix = now;

    let report = build_report(&context);

    // Inventory: usable -> Activated.
    assert_eq!(
        find(&report.findings, "inventory", "anthropic").status,
        Status::Pass,
        "inventory must show the credential as usable"
    );
    // Config: the oauth:// reference resolves.
    assert_eq!(
        find(&report.findings, "config", "anthropic").status,
        Status::Pass,
        "config must show the credential reference as resolving"
    );
    // Auth: logged in -- never "access token expired", which would
    // contradict the present state the other two surfaces show.
    let auth = find(&report.findings, "auth", "anthropic");
    assert_eq!(auth.status, Status::Pass, "auth must agree it is usable");
    assert!(
        !auth.detail.contains("expired"),
        "auth must not contradict the present state: {}",
        auth.detail
    );
}

#[test]
fn capability_section_renders_content_not_placeholder() {
    let context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    assert!(
        report.findings.iter().any(|f| f.section == "capability"),
        "capability section must now contribute findings"
    );
    let text = render_human(&report).join("\n");
    assert!(
        text.contains("[Capability]"),
        "capability header missing: {text}"
    );
    assert!(
        !text.contains("not yet available"),
        "placeholder must be gone: {text}"
    );
    // The runtime-only learned line and the honest empty-priors note are
    // both present on a default config.
    assert!(
        text.contains("runtime-only"),
        "learned line missing: {text}"
    );
    assert!(
        text.contains("no catalog capability priors present"),
        "honest empty-priors note missing: {text}"
    );
}

/// A config with a legacy provider list plus a new override, so the
/// registry snapshot carries both a provider-static and an override row.
fn config_with_overrides() -> Config {
    toml::from_str(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n\
         unsupported_features = [\"web_search\"]\n\
         [capability.overrides.p]\n\
         force_supported = [\"structured_output\"]\n",
    )
    .expect("override config parses")
}

#[test]
fn override_rows_render_with_source_labels_and_never_fail() {
    let context = ctx(
        config_with_overrides(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let findings = section_capability(&context);
    let overrides: Vec<&Finding> = findings.iter().filter(|f| f.name == "p").collect();
    assert_eq!(overrides.len(), 2, "one finding per override cell");
    assert!(overrides.iter().all(|f| f.status == Status::Pass));

    let route_away = overrides
        .iter()
        .find(|f| f.detail.contains("web_search"))
        .expect("provider-static route-away row");
    assert!(route_away.detail.contains("route-away"), "{route_away:?}");
    assert!(
        route_away.detail.contains("source: provider-static"),
        "{route_away:?}"
    );

    let forced = overrides
        .iter()
        .find(|f| f.detail.contains("structured_output"))
        .expect("override force-supported row");
    assert!(forced.detail.contains("force-supported"), "{forced:?}");
    assert!(forced.detail.contains("source: override"), "{forced:?}");

    // Capability findings never flip the exit code.
    assert_eq!(overall_exit(&findings), 0);
}

#[test]
fn prior_cells_render_when_overlay_provides_them() {
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://ANTHROPIC_API_KEY\"\n\
         [models.opus]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-opus-4-8\"\n",
    )
    .expect("config parses");
    // An overlay cell that supplies capability data for the model's
    // (provider_kind, upstream) selector. `verified_at` is stamped TODAY
    // (via the live clock) so the cell is never stale-suppressed
    // regardless of when the suite runs.
    let today = Local::now().format("%Y-%m-%d").to_string();
    let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "revision": 1,
        "cells": {
            "anthropic-api:claude-opus-4-8": {
                "source": "user",
                "verified_at": today,
                "capabilities": { "web_search": true }
            }
        }
    }))
    .expect("valid overlay");

    let inputs = build_capability_inputs(&config, None, Some(overlay));
    let priors = &inputs.config.expect("config present").priors;
    let PriorLayer::Present(cells) = priors else {
        panic!("expected present priors");
    };
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].nickname, "opus");
    assert!(cells[0].selector.contains("claude-opus-4-8"));
    // The overlay cell wins the merge -> source is User.
    assert_eq!(cells[0].source, Source::User);
    // The overlay-supplied capability is present and true (baked keys the
    // overlay does not mention merge through unchanged).
    assert!(
        cells[0]
            .capabilities
            .contains(&("web_search".to_string(), true)),
        "web_search prior missing: {:?}",
        cells[0].capabilities
    );
}

#[test]
fn stale_catalog_cell_yields_no_prior() {
    // A Present overlay cell whose verified_at is far in the past is
    // stale -> suppressed as unknown, never rendered as an authoritative
    // prior (spec 6c). An unparseable stamp is treated as stale too.
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"env://ANTHROPIC_API_KEY\"\n\
         [models.opus]\n\
         provider = \"anthropic\"\n\
         upstream = \"claude-opus-4-8\"\n",
    )
    .expect("config parses");
    let overlay: CatalogOverlay = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "revision": 1,
        "cells": {
            "anthropic-api:claude-opus-4-8": {
                "source": "user",
                "verified_at": "2000-01-01",
                "capabilities": { "web_search": true }
            }
        }
    }))
    .expect("valid overlay");
    let cells = derive_prior_cells(&config, &overlay);
    assert!(cells.is_empty(), "stale cell must yield no prior");
}

#[test]
fn absent_catalog_cell_yields_no_prior_and_no_crash() {
    // A model whose provider is unknown resolves to an empty
    // provider_kind that matches no baked cell -> Missing -> no prior.
    let config: Config = toml::from_str(
        "version = 3\n\
         [models.ghost]\n\
         provider = \"nope\"\n\
         upstream = \"whatever\"\n",
    )
    .expect("config parses");
    let cells = derive_prior_cells(&config, &CatalogOverlay::default());
    assert!(cells.is_empty(), "absent cell must yield no prior");
}

#[test]
fn learned_line_is_runtime_only_with_no_counts() {
    let f = learned_line();
    assert_eq!(f.status, Status::Pass);
    assert!(f.detail.contains("runtime-only"));
    assert!(f.detail.contains("serve logs"));
    // No fabricated counts.
    assert!(
        !f.detail.chars().any(|c| c.is_ascii_digit()),
        "learned line must carry no counts: {}",
        f.detail
    );
}

#[test]
fn config_load_error_routes_through_redactor_to_unavailable() {
    // A TOML parse error whose diagnostic inlines a `literal:` secret in
    // the source-line preview. build_capability_inputs receives the
    // ALREADY-redacted string (redact_config_load_error), mirroring the
    // gather path; assert the secret never reaches the finding.
    let raw = "config parse error in `/home/x/config.toml`: TOML parse error at line 2, column 1\n  |\n2 | api_key_ref = \"literal:sk-super-secret\"\n  | ^\ninvalid type: string, expected integer\n";
    let redacted = redact_config_load_error(raw);
    let inputs = build_capability_inputs(&Config::default(), Some(redacted), None);
    let context = DoctorContext {
        capability: inputs,
        ..ctx(Config::default(), Some("x"), Vec::new(), Vec::new())
    };
    let findings = section_capability(&context);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].name, "panel");
    assert_eq!(findings[0].status, Status::Warn);
    assert!(findings[0].detail.contains("unavailable"));
    assert!(
        !findings[0].detail.contains("sk-super-secret"),
        "secret leaked into unavailable finding: {}",
        findings[0].detail
    );
    // Never flips the exit code.
    assert_eq!(overall_exit(&findings), 0);
}

#[test]
fn overlay_unavailable_keeps_override_rows_marks_priors_unavailable() {
    // overlay = None simulates an unreadable overlay: override rows still
    // render, priors are marked unavailable.
    let inputs = build_capability_inputs(&config_with_overrides(), None, None);
    let context = DoctorContext {
        capability: inputs,
        ..ctx(Config::default(), Some("x"), Vec::new(), Vec::new())
    };
    let findings = section_capability(&context);
    assert!(
        findings.iter().any(|f| f.name == "p"),
        "override rows must still render on overlay failure"
    );
    let priors = findings
        .iter()
        .find(|f| f.name == "catalog priors")
        .expect("priors finding");
    assert_eq!(priors.status, Status::Warn);
    assert!(priors.detail.contains("unavailable"));
    assert_eq!(overall_exit(&findings), 0);
}

#[test]
fn legacy_nudge_warns_once_with_guarded_phrasing() {
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n\
         unsupported_features = [\"web_search\"]\n\
         [bedrock]\n\
         allowed_betas = [\"some-beta\"]\n",
    )
    .expect("config parses");
    let keys = present_legacy_capability_keys(&config);
    let nudge = legacy_nudge(&keys).expect("nudge present");
    assert_eq!(nudge.status, Status::Warn);
    assert!(nudge.detail.contains("unsupported_features"), "{nudge:?}");
    assert!(nudge.detail.contains("allowed_betas"), "{nudge:?}");
    // Exact guarded phrasing.
    assert!(
        nudge
            .detail
            .contains("the override layer replaces these via config migrate"),
        "{nudge:?}"
    );
    assert!(
        nudge.detail.contains(
            "the learner discovers use-time rejections automatically; these lists remain \
             operator-owned until the next schema version"
        ),
        "{nudge:?}"
    );
    // Must not imply the lists are safe to delete.
    assert!(!nudge.detail.contains("delete"), "{nudge:?}");
    assert!(!nudge.detail.contains("remove"), "{nudge:?}");
    assert!(
        nudge
            .remediation
            .as_deref()
            .unwrap()
            .contains("config migrate")
    );
}

#[test]
fn legacy_nudge_absent_without_legacy_lists() {
    assert!(legacy_nudge(&[]).is_none());
    // A config with a new override but no legacy list emits no nudge.
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n\
         [capability.overrides.p]\n\
         unsupported = [\"web_search\"]\n",
    )
    .expect("config parses");
    let context = ctx(config, Some("version = 3\n"), Vec::new(), Vec::new());
    let findings = section_capability(&context);
    assert!(
        findings.iter().all(|f| f.name != "legacy keys"),
        "no nudge without a legacy list"
    );
}

#[test]
fn schema_version_is_three() {
    assert_eq!(SCHEMA_VERSION, 3);
    let context = ctx(
        config_with_overrides(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    assert_eq!(report.schema_version, 3);
    // JSON mode carries the same capability content as the human render.
    let value = serde_json::to_value(&report).expect("serialize");
    let blob = value.to_string();
    assert!(blob.contains("route-away"), "json missing override verdict");
    assert!(
        blob.contains("provider-static"),
        "json missing source label"
    );
    assert!(blob.contains("runtime-only"), "json missing learned line");
}

#[test]
fn human_render_carries_status_battery_and_remediation() {
    let cfg = config_referencing_anthropic();
    let context = ctx(
        cfg,
        Some("version = 1\n"),
        vec![("anthropic", LocalProbe::Missing)],
        Vec::new(),
    );
    let report = build_report(&context);
    let text = render_human(&report).join("\n");
    assert!(text.contains("FAIL"), "expected a FAIL line: {text}");
    assert!(text.contains("WARN"), "expected a WARN line: {text}");
    assert!(text.contains("fix:"), "expected a remediation line: {text}");
    assert!(
        text.contains("summary: PASS"),
        "expected a summary line: {text}"
    );
}

#[test]
fn findings_sorted_deterministically_and_exit_matches() {
    let cfg = config_referencing_anthropic();
    let a = build_report(&ctx(
        cfg.clone(),
        Some("version = 3\n"),
        vec![
            ("anthropic", LocalProbe::Present),
            ("codex", LocalProbe::Missing),
        ],
        Vec::new(),
    ));
    let b = build_report(&ctx(
        cfg,
        Some("version = 3\n"),
        // Reversed probe order must not change the sorted output.
        vec![
            ("codex", LocalProbe::Missing),
            ("anthropic", LocalProbe::Present),
        ],
        Vec::new(),
    ));
    assert_eq!(a.findings, b.findings, "sort must be order-independent");
    assert_eq!(overall_exit(&a.findings), overall_exit(&b.findings));
    // No Fail here (v3 config, present anthropic) -> exit 0.
    assert_eq!(overall_exit(&a.findings), 0);
}

#[test]
fn json_report_carries_schema_version_findings_and_panels() {
    let context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    let value: serde_json::Value = serde_json::to_value(&report).expect("serialize doctor report");
    let obj = value.as_object().expect("top-level object");
    assert!(obj.contains_key("schema_version"));
    assert!(obj["findings"].is_array());
    assert!(obj.contains_key("panels"));
}

fn a_panel() -> WouldTrimPanel {
    WouldTrimPanel {
        candidate_requests: 5,
        would_trim_tokens: 42_000,
        verdict_met: 2,
        verdict_unmet: 1,
        verdict_cold: 1,
        verdict_unpriced: 1,
    }
}

/// The probe section is one finding per configured provider, each mapped
/// through the shared `probe_finding` seam; every Fail/Warn carries a
/// remediation.
#[test]
fn probe_section_is_one_finding_per_provider_via_shared_mapping() {
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.probe_results = vec![
        ("reach".to_string(), ProbeOutcome::Reachable),
        (
            "authf".to_string(),
            ProbeOutcome::AuthFailed("not logged in".into()),
        ),
        ("nofree".to_string(), ProbeOutcome::UnsupportedFreeProbe),
    ];
    let findings = section_probe(&context);
    assert_eq!(findings.len(), 3, "one finding per provider");
    assert!(findings.iter().all(|f| f.section == "probe"));
    assert_eq!(find(&findings, "probe", "reach").status, Status::Pass);
    assert_eq!(find(&findings, "probe", "authf").status, Status::Fail);
    for f in &findings {
        if matches!(f.status, Status::Warn | Status::Fail) {
            assert!(
                f.remediation.is_some(),
                "{} probe finding must carry a remediation",
                f.name
            );
        }
    }
}

/// A forwarded provider probes as Skipped -> an informational Pass line,
/// not a WARN, and carries no remediation.
#[test]
fn forwarded_probe_is_informational_pass_not_warn() {
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.probe_results = vec![("fwd".to_string(), ProbeOutcome::Skipped("forwarded".into()))];
    let findings = section_probe(&context);
    let f = find(&findings, "probe", "fwd");
    assert_eq!(
        f.status,
        Status::Pass,
        "forwarded is informational, not WARN"
    );
    assert!(f.remediation.is_none());
}

/// A provider kind with no free reachability probe surfaces as WARN with a
/// "cannot verify" reason -- never a silent PASS.
#[test]
fn no_free_endpoint_probe_warns_not_silent_pass() {
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.probe_results = vec![("nofree".to_string(), ProbeOutcome::UnsupportedFreeProbe)];
    let findings = section_probe(&context);
    let f = find(&findings, "probe", "nofree");
    assert_eq!(
        f.status,
        Status::Warn,
        "unsupported probe must WARN, not silently pass"
    );
    assert!(f.remediation.is_some());
}

/// Exit code is order-independent with the probe section present: a probe
/// AuthFailed keeps the run nonzero regardless of provider ordering.
#[test]
fn probe_section_keeps_exit_deterministic_across_ordering() {
    let forward = {
        let mut c = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        c.probe_results = vec![
            ("alpha".to_string(), ProbeOutcome::Reachable),
            ("zeta".to_string(), ProbeOutcome::AuthFailed("x".into())),
        ];
        build_report(&c)
    };
    let reversed = {
        let mut c = ctx(
            Config::default(),
            Some("version = 3\n"),
            Vec::new(),
            Vec::new(),
        );
        c.probe_results = vec![
            ("zeta".to_string(), ProbeOutcome::AuthFailed("x".into())),
            ("alpha".to_string(), ProbeOutcome::Reachable),
        ];
        build_report(&c)
    };
    assert_eq!(
        forward.findings, reversed.findings,
        "probe ordering must not change the sorted findings"
    );
    assert_eq!(
        overall_exit(&forward.findings),
        overall_exit(&reversed.findings)
    );
    assert_ne!(
        overall_exit(&forward.findings),
        0,
        "a probe AuthFailed must keep the run nonzero"
    );
}

/// A computed would-trim panel is wired into `panels.would_trim` and
/// rendered in the human battery.
#[test]
fn would_trim_panel_populates_report_and_human_render() {
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.would_trim = Some(a_panel());
    let report = build_report(&context);
    assert_eq!(report.panels.would_trim, Some(a_panel()));
    let text = render_human(&report).join("\n");
    assert!(
        text.contains("would-trim"),
        "human battery must render the panel: {text}"
    );
    assert!(text.contains("5 reqs"), "panel counts must appear: {text}");
}

/// A no-data window yields `panels.would_trim = None` and the human battery
/// omits the would-trim block entirely.
#[test]
fn no_data_would_trim_is_none_and_omitted_from_human_render() {
    let context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    assert_eq!(report.panels.would_trim, None);
    let text = render_human(&report).join("\n");
    assert!(
        !text.contains("would-trim"),
        "no panel -> no would-trim block: {text}"
    );
}

/// The would-trim panel appears under `panels.would_trim` in `--json`.
#[test]
fn would_trim_panel_serializes_under_panels_in_json() {
    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.would_trim = Some(a_panel());
    let report = build_report(&context);
    let value: serde_json::Value = serde_json::to_value(&report).expect("serialize doctor report");
    let panel = &value["panels"]["would_trim"];
    assert_eq!(panel["candidate_requests"], serde_json::json!(5));
    assert_eq!(panel["would_trim_tokens"], serde_json::json!(42_000));
}

fn snapshot_dir(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.insert(path, bytes);
            }
        }
    }
    out
}

#[tokio::test]
#[serial_test::serial]
async fn full_run_leaves_config_dir_byte_identical() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(&config_path, b"version = 3\n").unwrap();

    // Seed a credentials file the store will read, at 0600 (the loader
    // refuses a world-readable credentials file). A doctor run must
    // leave it byte-identical.
    let creds_path = cfg_dir.join("credentials.json");
    std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
    std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let before = snapshot_dir(tmp.path());
    let code = run(&config_path, false).await;
    let after = snapshot_dir(tmp.path());

    assert_eq!(
        before, after,
        "a doctor run must not mutate or create any file"
    );
    // v3 config, empty credentials store: no Fail -> exit 0.
    assert_eq!(code, 0);
}

#[tokio::test]
#[serial_test::serial]
async fn too_old_config_is_not_stamped_and_reports_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    let original = b"version = 1\n";
    std::fs::write(&config_path, original).unwrap();

    let context = gather_context(&config_path).await;
    let report = build_report(&context);

    let after = std::fs::read(&config_path).unwrap();
    assert_eq!(
        after.as_slice(),
        original,
        "version preflight must not stamp the file"
    );
    let f = find(&report.findings, "version", "config schema");
    assert_eq!(f.status, Status::Fail);
    assert!(f.remediation.as_deref().unwrap().contains("config migrate"));
    assert_ne!(overall_exit(&report.findings), 0);
}

#[tokio::test]
#[serial_test::serial]
async fn present_but_unparseable_config_yields_fail_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    // Parseable `version` key, but an unknown field the typed load
    // (deny_unknown_fields) rejects -- the raw preflight passes yet the
    // config is broken.
    std::fs::write(&config_path, b"version = 3\nbogus_key = true\n").unwrap();

    let context = gather_context(&config_path).await;
    let report = build_report(&context);

    let f = find(&report.findings, "version", "config schema");
    assert_eq!(
        f.status,
        Status::Fail,
        "a present-but-broken config must not report all-Pass"
    );
    assert_ne!(overall_exit(&report.findings), 0);
}

/// A config referencing an anthropic provider through an alias is clean:
/// the config section reports a single Pass for the validator suite.
#[test]
fn clean_config_passes_validation() {
    let context = ctx(
        config_referencing_anthropic(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let findings = section_config(&context);
    let f = find(&findings, "config", "validation");
    assert_eq!(f.status, Status::Pass);
    assert!(f.remediation.is_none());
}

/// A model pointing at a provider absent from `[providers]` is a semantic
/// validation error -> a Fail in the config section with remediation.
#[test]
fn semantic_validation_error_reports_fail() {
    let mut cfg = Config::default();
    cfg.models.insert(
        "sonnet".to_string(),
        ModelEntry::new("ghost", "claude-sonnet-4-5"),
    );
    let context = ctx(cfg, Some("version = 3\n"), Vec::new(), Vec::new());
    let findings = section_config(&context);
    let f = find(&findings, "config", "validation");
    assert_eq!(f.status, Status::Fail);
    assert!(f.remediation.is_some());
    assert!(
        f.detail.contains("ghost"),
        "expected the offending provider in the detail, got {:?}",
        f.detail
    );
}

/// When the typed config load failed, the section must NOT run the
/// validator suite against the fallback `Config::default()` -- doing so
/// emits a spurious Pass that contradicts the Fail the version section
/// reports for the same broken file. It short-circuits to a single Warn
/// "validation skipped" finding, still appending the secret checks.
#[test]
fn config_load_error_short_circuits_to_warn_not_pass() {
    let cfg = config_referencing_anthropic();
    let secret_checks = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Missing)]);
    assert!(
        !secret_checks.is_empty(),
        "fixture must produce at least one secret check"
    );
    let mut context = ctx(
        Config::default(),
        Some("bogus = \n"),
        Vec::new(),
        Vec::new(),
    );
    context.config_load_error = Some("config could not be loaded".to_string());
    context.secret_checks = secret_checks;

    let findings = section_config(&context);

    assert!(
        !findings
            .iter()
            .any(|f| f.section == "config" && f.name == "validation" && f.status == Status::Pass),
        "must not emit a spurious validation Pass against the default config: {findings:?}"
    );
    let validation = find(&findings, "config", "validation");
    assert_eq!(validation.status, Status::Warn);
    assert!(
        validation.detail.contains("skipped"),
        "expected a skipped-validation detail, got {:?}",
        validation.detail
    );
    assert!(validation.remediation.is_some());
    assert!(
        findings.iter().any(|f| f.name == "anthropic"),
        "secret checks must still be appended when validation is skipped: {findings:?}"
    );
}

/// An `oauth://` ref resolves via the pre-gathered local probe: Present
/// -> Pass; Missing -> Warn with a `routectl login` remediation. The
/// classification reads the probe only -- it never resolves the token.
#[test]
fn oauth_secret_present_passes_missing_warns_with_login() {
    let cfg = config_referencing_anthropic();

    let present = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Present)]);
    let f = secret_finding(&present[0]);
    assert_eq!(f.status, Status::Pass);
    assert!(f.remediation.is_none());

    let missing = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Missing)]);
    let f = secret_finding(&missing[0]);
    assert_eq!(f.status, Status::Warn);
    assert!(
        f.remediation
            .as_deref()
            .unwrap()
            .contains("routectl login anthropic"),
        "expected a login remediation, got {:?}",
        f.remediation
    );
}

/// An `oauth://` ref whose local probe reports an expired, non-revivable
/// credential is a Warn carrying a `routectl login` remediation.
#[test]
fn oauth_secret_expired_warns_with_login() {
    let cfg = config_referencing_anthropic();
    let checks = gather_secret_checks(&cfg, &[("anthropic", LocalProbe::Expired)]);
    let f = secret_finding(&checks[0]);
    assert_eq!(f.status, Status::Warn);
    assert!(
        f.remediation
            .as_deref()
            .unwrap()
            .contains("routectl login anthropic"),
        "expected a login remediation, got {:?}",
        f.remediation
    );
}

/// A `file://` ref that exists but is not a readable regular file (here a
/// directory) is a Warn that names the scheme, never the path.
#[test]
fn unreadable_file_ref_warns_without_leaking_path() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.providers.insert(
        "compat".to_string(),
        ProviderEntry::openai_compat(
            "https://example.test/v1",
            format!("file://{}", tmp.path().display()),
        ),
    );
    let checks = gather_secret_checks(&cfg, &[]);
    let f = secret_finding(&checks[0]);
    assert_eq!(f.status, Status::Warn);
    assert!(f.remediation.is_some());
    assert!(
        f.detail.contains("file://") && !f.detail.contains(&tmp.path().display().to_string()),
        "detail must name the scheme, never the path: {:?}",
        f.detail
    );
}

/// An `env://` ref that resolves is a Pass; an unset one is a Warn that
/// names the scheme -- never the variable name or value.
#[test]
#[serial_test::serial]
fn env_ref_resolves_passes_unset_warns_without_leaking_name() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "compat".to_string(),
        ProviderEntry::openai_compat("https://example.test/v1", "env://ROUTECTL_DOCTOR_TEST_KEY"),
    );

    {
        let _var = ScopedEnv::set("ROUTECTL_DOCTOR_TEST_KEY", "sk-secret-value");
        let checks = gather_secret_checks(&cfg, &[]);
        let f = secret_finding(&checks[0]);
        assert_eq!(f.status, Status::Pass);
    }

    // The guard above restored the variable to its prior (unset) state on
    // drop, so the ref no longer resolves here.
    let checks = gather_secret_checks(&cfg, &[]);
    let f = secret_finding(&checks[0]);
    assert_eq!(f.status, Status::Warn);
    assert!(f.remediation.is_some());
    assert!(
        f.detail.contains("env://") && !f.detail.contains("ROUTECTL_DOCTOR_TEST_KEY"),
        "detail must name the scheme, never the variable: {:?}",
        f.detail
    );
}

/// A missing `file://` secret path is a Warn that names the scheme, not
/// the path (a full `file://` ref string must never reach a finding).
#[test]
fn missing_file_ref_warns_without_leaking_path() {
    let secret_path = "/nonexistent/routectl-doctor-secret-marker";
    let mut cfg = Config::default();
    cfg.providers.insert(
        "compat".to_string(),
        ProviderEntry::openai_compat("https://example.test/v1", format!("file://{secret_path}")),
    );
    let checks = gather_secret_checks(&cfg, &[]);
    let f = secret_finding(&checks[0]);
    assert_eq!(f.status, Status::Warn);
    assert!(f.remediation.is_some());
    assert!(
        f.detail.contains("file://") && !f.detail.contains("routectl-doctor-secret-marker"),
        "detail must name the scheme, never the path: {:?}",
        f.detail
    );
}

/// No secret value or full ref string reaches any config-section message,
/// across the redacting (`literal:`) and pointer (`file://`) schemes.
#[test]
fn config_findings_never_leak_secret_material() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "compat".to_string(),
        ProviderEntry::openai_compat("https://example.test/v1", "literal:hunter2-do-not-leak"),
    );
    cfg.providers.insert(
        "compat-file".to_string(),
        ProviderEntry::openai_compat(
            "https://example.test/v1",
            "file:///nonexistent/leaky-secret-path",
        ),
    );
    let mut context = ctx(cfg.clone(), Some("version = 3\n"), Vec::new(), Vec::new());
    context.secret_checks = gather_secret_checks(&cfg, &[]);
    let findings = section_config(&context);
    let blob = findings
        .iter()
        .map(|f| format!("{} {}", f.detail, f.remediation.clone().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!blob.contains("hunter2"), "leaked literal value: {blob}");
    assert!(
        !blob.contains("leaky-secret-path"),
        "leaked file path: {blob}"
    );
}

/// A managed secret file no provider references surfaces as a Warn and is
/// NOT deleted -- the scan is a read-only directory diff.
#[test]
#[serial_test::serial]
fn orphan_managed_secret_warns_and_is_not_deleted() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let secret_dir = tmp.path().join("routectl").join("secrets");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let orphan = secret_dir.join("stranded-key");
    std::fs::write(&orphan, b"secret-bytes").unwrap();
    std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600)).unwrap();

    let orphans = gather_orphan_secrets(&Config::default());
    assert_eq!(orphans, vec!["stranded-key".to_string()]);

    let mut context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    context.orphan_secrets = orphans;
    let findings = section_secret_orphans(&context);
    let f = find(&findings, "secrets", "stranded-key");
    assert_eq!(f.status, Status::Warn);
    assert!(f.remediation.is_some());
    assert!(
        orphan.exists(),
        "the orphan scan must never delete a stored secret"
    );
}

/// A file referenced by a `file://` provider ref is not an orphan.
#[test]
#[serial_test::serial]
fn referenced_managed_secret_is_not_an_orphan() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let secret_dir = tmp.path().join("routectl").join("secrets");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let used = secret_dir.join("used-key");
    std::fs::write(&used, b"secret-bytes").unwrap();
    std::fs::set_permissions(&used, std::fs::Permissions::from_mode(0o600)).unwrap();

    let mut cfg = Config::default();
    cfg.providers.insert(
        "compat".to_string(),
        ProviderEntry::openai_compat(
            "https://example.test/v1",
            format!("file://{}", used.display()),
        ),
    );
    assert!(gather_orphan_secrets(&cfg).is_empty());
}

/// End-to-end: a doctor run over a config with an `oauth://` provider and
/// a populated managed secret directory leaves the config dir, the
/// credentials record, and the secret dir byte-identical -- the oauth
/// presence check uses `probe_local` (no refresh) and the orphan scan is
/// read-only.
#[tokio::test]
#[serial_test::serial]
async fn full_run_leaves_secret_dir_byte_identical() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(
        &config_path,
        b"version = 3\n[providers.anthropic]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n",
    )
    .unwrap();

    let creds_path = cfg_dir.join("credentials.json");
    std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
    std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let secret_dir = cfg_dir.join("secrets");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let orphan = secret_dir.join("stranded-key");
    std::fs::write(&orphan, b"secret-bytes").unwrap();
    std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600)).unwrap();

    let before = snapshot_dir(tmp.path());
    let context = gather_context(&config_path).await;
    let report = build_report(&context);
    let after = snapshot_dir(tmp.path());

    assert_eq!(
        before, after,
        "a doctor run must not mutate, create, or delete any file"
    );
    let orphan_finding = find(&report.findings, "secrets", "stranded-key");
    assert_eq!(orphan_finding.status, Status::Warn);
    assert!(orphan.exists(), "the orphan secret must survive the run");
}

#[test]
fn store_open_error_never_discloses_the_full_credentials_path() {
    // Every path-bearing variant embeds the FULL store path in its Display
    // (SchemaMismatch/CorruptedFile via `path`, Io via the interpolated
    // message); the sanitized message keeps only the failure class +
    // basename.
    let full = "/home/someone/.config/routectl/credentials.json";
    let schema = sanitize_store_open_error(&OAuthError::SchemaMismatch {
        found: 1,
        expected: 2,
        path: full.to_string(),
    });
    assert!(!schema.contains(full), "path leaked: {schema}");
    assert!(!schema.contains("/home/someone"), "dir leaked: {schema}");
    assert!(schema.contains("credentials.json"), "{schema}");

    let corrupt = sanitize_store_open_error(&OAuthError::CorruptedFile {
        path: full.to_string(),
        detail: "trailing comma at line 4".to_string(),
    });
    assert!(!corrupt.contains(full), "path leaked: {corrupt}");
    assert!(!corrupt.contains("/home/someone"), "dir leaked: {corrupt}");
    assert!(corrupt.contains("credentials.json"), "{corrupt}");

    // The Io variant interpolates the path mid-message, so it too must be
    // reduced to a class-only string.
    let io = sanitize_store_open_error(&OAuthError::Io(format!(
        "credentials file {full} has permissions 644; use chmod 600"
    )));
    assert!(!io.contains(full), "path leaked: {io}");
    assert!(!io.contains("/home/someone"), "dir leaked: {io}");
    assert!(io.contains("credentials.json"), "{io}");
}

#[test]
fn store_open_error_wildcard_is_a_path_free_class_message() {
    // A variant the sanitizer does not special-case must NOT forward its
    // raw Display (the enum is #[non_exhaustive] and a future variant could
    // embed a path); the fallback is a fixed class-only message.
    let msg = sanitize_store_open_error(&OAuthError::Internal(
        "/home/someone/.config/routectl/credentials.json is wedged".to_string(),
    ));
    assert!(!msg.contains("/home/someone"), "path leaked: {msg}");
    assert!(msg.contains("credentials.json"), "{msg}");
}

#[test]
fn config_load_error_redacts_a_secret_bearing_parse_error() {
    // The loader wraps the toml diagnostic as `config parse error in
    // <path>: TOML parse error ...`; a mistyped secret in the numeric
    // `port` field lands verbatim in the `invalid type:` clause.
    let raw = "config parse error in `/home/someone/.config/routectl/config.toml`: \
               TOML parse error at line 5, column 8\n  |\n5 | port = \"sk-live-LEAKED\"\n  \
               |        ^^^^^^^^^^^^^^^^\ninvalid type: string \"sk-live-LEAKED\", expected u16";
    let redacted = redact_config_load_error(raw);
    assert!(!redacted.contains("sk-live-LEAKED"), "{redacted}");
    // Dropping everything before the toml header also drops the wrapping
    // config path.
    assert!(!redacted.contains("/home/someone"), "{redacted}");
    assert!(redacted.contains("line 5, column 8"), "{redacted}");
    assert!(redacted.contains("port"), "{redacted}");
}

#[test]
fn config_load_error_sanitizes_path_bearing_non_parse_errors() {
    // The loader formats an unreadable config and a catalog-overlay failure
    // with the FULL filesystem path; both must collapse to a path-free
    // class message.
    let unreadable = "cannot read config `/home/someone/.config/routectl/config.toml`: \
                      Permission denied (os error 13)";
    let redacted = redact_config_load_error(unreadable);
    assert!(!redacted.contains("/home/someone"), "{redacted}");
    assert!(!redacted.contains("config.toml"), "{redacted}");

    let overlay = "catalog overlay load error: failed to read \
                   /home/someone/.config/routectl/catalog_overlay.json: broken";
    let redacted = redact_config_load_error(overlay);
    assert!(!redacted.contains("/home/someone"), "{redacted}");
    assert!(!redacted.contains("catalog_overlay.json"), "{redacted}");
}

#[test]
fn config_load_error_keeps_a_path_free_error_verbatim() {
    // A version/legacy-key rejection carries no path or value, so it stays
    // actionable rather than being reduced to a class message.
    let raw = "config schema v9, this binary expects v3";
    assert_eq!(redact_config_load_error(raw), raw);
}

#[test]
fn rendered_report_leaks_neither_a_config_secret_nor_a_store_path() {
    // End-to-end through BOTH renderers: a version finding built from a
    // redacted parse error and an auth finding built from a sanitized
    // store-open error must not surface the secret or the full path in the
    // human render OR the JSON serialization.
    const SECRET: &str = "sk-live-REPORT-LEAK";
    const FULL_PATH: &str = "/home/someone/.config/routectl/credentials.json";

    let raw_load_error = format!(
        "config parse error in `/home/someone/.config/routectl/config.toml`: \
         TOML parse error at line 5, column 8\n  |\n5 | port = \"{SECRET}\"\n  \
         |        ^^^^^^\ninvalid type: string \"{SECRET}\", expected u16"
    );
    let raw_store_error =
        OAuthError::Io(format!("open {FULL_PATH}: Permission denied (os error 13)"));

    let context = DoctorContext {
        config: Config::default(),
        raw_config: Some("version = 3\n".to_string()),
        config_load_error: Some(redact_config_load_error(&raw_load_error)),
        probes: Vec::new(),
        seats: Vec::new(),
        auth_store_error: Some(sanitize_store_open_error(&raw_store_error)),
        secret_checks: Vec::new(),
        orphan_secrets: Vec::new(),
        probe_results: Vec::new(),
        would_trim: None,
        now_unix: 1_000,
        binary_version: "test",
        capability: build_capability_inputs(
            &Config::default(),
            Some(redact_config_load_error(&raw_load_error)),
            None,
        ),
        capability_matrix: CapabilityMatrixSource::Unavailable("config_unavailable"),
        freshness: sample_freshness(),
    };
    let report = build_report(&context);

    // Both the version and auth findings must be present and failing.
    assert_eq!(
        find(&report.findings, "version", "config schema").status,
        Status::Fail
    );
    assert_eq!(
        find(&report.findings, "auth", "oauth credentials").status,
        Status::Fail
    );

    let human = render_human(&report).join("\n");
    let json = serde_json::to_string(&report).expect("report serializes");
    for surface in [&human, &json] {
        assert!(!surface.contains(SECRET), "config secret leaked: {surface}");
        assert!(!surface.contains(FULL_PATH), "store path leaked: {surface}");
        assert!(
            !surface.contains("/home/someone"),
            "a directory leaked: {surface}"
        );
    }
}

/// The no-network section list is exactly [`SECTIONS`] minus the `probe`
/// entry, order preserved: `probe` is present in the full list and absent
/// from the no-network one.
#[test]
fn no_network_sections_are_sections_minus_probe() {
    let full: Vec<&str> = SECTIONS.iter().map(|(k, _)| *k).collect();
    let no_net: Vec<&str> = NO_NETWORK_SECTIONS.iter().map(|(k, _)| *k).collect();
    assert!(full.contains(&"probe"), "SECTIONS must contain probe");
    assert!(
        !no_net.contains(&"probe"),
        "no-network sections must omit probe"
    );
    let expected: Vec<&str> = full.iter().copied().filter(|k| *k != "probe").collect();
    assert_eq!(
        no_net, expected,
        "no-network sections must equal SECTIONS minus probe, order preserved"
    );
}

/// The no-network gather leaves `probe_results` empty (no
/// `gather_probe_results` -> no `CompositeStore`/`probe_all` dial), and a
/// report built from it carries no `probe` section rows.
#[tokio::test]
#[serial_test::serial]
async fn gather_context_no_network_yields_no_probe_results() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(
        &config_path,
        b"version = 3\n[providers.anthropic]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n",
    )
    .unwrap();
    let creds_path = cfg_dir.join("credentials.json");
    std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
    std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let context = gather_context_no_network(&config_path).await;
    assert!(
        context.probe_results.is_empty(),
        "the no-network gather must not populate probe results"
    );
    // The gather populates the capability matrix source. Under the hermetic
    // XDG this run has no usage ledger, so the source is unavailable (cold)
    // rather than a silent empty.
    assert!(
        matches!(
            context.capability_matrix,
            CapabilityMatrixSource::Unavailable(_)
        ),
        "a run with no ledger reports the matrix unavailable, never empty"
    );

    let report = build_report_no_network(&context);
    assert!(
        report.findings.iter().all(|f| f.section != "probe"),
        "a no-network report must carry no probe findings"
    );
}

/// `build_report_no_network` keeps the schema version, emits no `probe`
/// rows, and its findings are exactly the network report's non-probe
/// findings for the same context.
#[test]
fn build_report_no_network_matches_network_minus_probe() {
    let mut context = ctx(
        config_referencing_anthropic(),
        Some("version = 3\n"),
        vec![("anthropic", LocalProbe::Present)],
        Vec::new(),
    );
    context.probe_results = vec![("anthropic".to_string(), ProbeOutcome::Reachable)];

    let network = build_report(&context);
    let no_net = build_report_no_network(&context);

    assert_eq!(no_net.schema_version, 3);
    assert!(
        no_net.findings.iter().all(|f| f.section != "probe"),
        "no-network report must have no probe rows"
    );
    assert!(
        network.findings.iter().any(|f| f.section == "probe"),
        "the network report must have probe rows for the same context"
    );

    let network_non_probe: Vec<&Finding> = network
        .findings
        .iter()
        .filter(|f| f.section != "probe")
        .collect();
    let no_net_all: Vec<&Finding> = no_net.findings.iter().collect();
    assert_eq!(
        no_net_all, network_non_probe,
        "non-probe sections must be byte-identical across the two builders"
    );
}

/// The shared gather body cannot drift: the network and no-network entry
/// points, run against the same fixture, produce identical non-probe
/// findings. A field added in one path but not the other would break this.
#[tokio::test]
#[serial_test::serial]
async fn network_and_no_network_gather_agree_outside_probe() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(&config_path, b"version = 3\n").unwrap();
    let creds_path = cfg_dir.join("credentials.json");
    std::fs::write(&creds_path, br#"{"schema_version":1,"providers":{}}"#).unwrap();
    std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let network = gather_context(&config_path).await;
    let no_network = gather_context_no_network(&config_path).await;

    // Both contexts fed through the no-network builder must agree: the
    // shared gather body produced the same non-probe inputs on both paths.
    let from_network = build_report_no_network(&network);
    let from_no_network = build_report_no_network(&no_network);
    assert_eq!(
        from_network.findings, from_no_network.findings,
        "the shared gather body must yield identical non-probe findings on both entry points"
    );
}

/// The capability-matrix data path: availability as a first-class tri-state.
/// Honest-`Empty` is reserved for a readable, revision-matched ledger with
/// zero post-boundary rows; every unreadable / cold / foreign-boundary /
/// unparseable-config case is `Unavailable` with a path-free class token.
mod capability_matrix {
    use super::*;
    use routectl_router::CATALOG_VERSION;
    use routectl_usage::open;
    use rusqlite::params;
    use tempfile::TempDir;

    fn config_at(db_path: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.usage.db_path = db_path.to_path_buf();
        config
    }

    fn seed_tombstone(conn: &rusqlite::Connection, ts: i64, cat: i64, ov: i64) {
        conn.execute(
            "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
             tier, evidence_class, upstream_token, catalog_version, overlay_revision) \
             VALUES (?1, '', '', 'tombstone', '', '', '', NULL, NULL, ?2, ?3)",
            params![ts, cat, ov],
        )
        .expect("seed tombstone");
    }

    fn seed_broken(conn: &rusqlite::Connection, ts: i64, lane: &str, cap: &str, cat: i64, ov: i64) {
        conn.execute(
            "INSERT INTO capability_events (ts, lane_key, capability, verdict, phase, source, \
             tier, evidence_class, upstream_token, catalog_version, overlay_revision) \
             VALUES (?1, ?2, ?3, 'broken', 'f1', 'live', 'self-identifying', NULL, NULL, ?4, ?5)",
            params![ts, lane, cap, cat, ov],
        )
        .expect("seed broken negative");
    }

    #[test]
    fn config_parse_failure_is_unavailable_never_empty() {
        // A config that would not parse: the db path and revision knobs are
        // untrusted, so the matrix reports unavailable rather than an
        // empty-from-default read.
        let tmp = TempDir::new().expect("tempdir");
        let config = config_at(&tmp.path().join("usage.db"));

        let source = gather_capability_matrix(&config, true, 0);
        assert!(
            matches!(
                source,
                CapabilityMatrixSource::Unavailable("config_unavailable")
            ),
            "a config parse failure is unavailable, not empty"
        );
    }

    #[test]
    fn absent_ledger_is_unavailable_not_empty() {
        // No ledger yet: there is no readable, matched source, so this is
        // unavailable (a distinct signal from an honest zero-row empty).
        let tmp = TempDir::new().expect("tempdir");
        let config = config_at(&tmp.path().join("absent.db"));

        let source = gather_capability_matrix(&config, false, 0);
        assert!(
            matches!(source, CapabilityMatrixSource::Unavailable("no_data")),
            "an absent ledger is unavailable, never a silent empty"
        );
    }

    #[test]
    fn unreadable_ledger_is_unavailable_with_path_free_token() {
        let tmp = TempDir::new().expect("tempdir");
        let ledger = tmp.path().join("usage.db");
        std::fs::write(&ledger, b"this is not a sqlite database").expect("write junk");
        let config = config_at(&ledger);

        let CapabilityMatrixSource::Unavailable(code) = gather_capability_matrix(&config, false, 0)
        else {
            panic!("an unreadable ledger must be unavailable");
        };
        assert!(
            !code.contains('/') && !code.contains("usage.db"),
            "the class token must be path-free: {code}"
        );
    }

    #[test]
    fn matched_zero_row_ledger_is_honest_empty() {
        // Readable ledger, tombstone matching this run's revision, no
        // post-boundary rows: the one case that is honestly empty.
        let tmp = TempDir::new().expect("tempdir");
        let ledger = tmp.path().join("usage.db");
        let db = open(&ledger).expect("open ledger");
        seed_tombstone(db.conn(), 100, i64::from(CATALOG_VERSION), 0);
        drop(db);
        let config = config_at(&ledger);

        assert!(
            matches!(
                gather_capability_matrix(&config, false, 0),
                CapabilityMatrixSource::Empty
            ),
            "a readable, matched, zero-row ledger is honest-empty"
        );
    }

    #[test]
    fn foreign_revision_tombstone_is_unavailable() {
        let tmp = TempDir::new().expect("tempdir");
        let ledger = tmp.path().join("usage.db");
        let db = open(&ledger).expect("open ledger");
        // Tombstone stamped a different overlay revision than this run's.
        seed_tombstone(db.conn(), 100, i64::from(CATALOG_VERSION), 0);
        drop(db);
        let config = config_at(&ledger);

        assert!(
            matches!(
                gather_capability_matrix(&config, false, 99),
                CapabilityMatrixSource::Unavailable("revision_mismatch")
            ),
            "a foreign-revision tombstone is unavailable, not empty"
        );
    }

    #[test]
    fn matching_slice_replays_and_pins_one_age_anchor() {
        // A matching tombstone plus a post-boundary negative stamped in the
        // FUTURE: the entry replays, and the future-dated row clamps to the
        // reader's single pinned `now` (age zero, never negative).
        let tmp = TempDir::new().expect("tempdir");
        let ledger = tmp.path().join("usage.db");
        let db = open(&ledger).expect("open ledger");
        let far_future = i64::MAX / 2;
        seed_tombstone(db.conn(), 100, i64::from(CATALOG_VERSION), 0);
        seed_broken(
            db.conn(),
            far_future,
            "gpt-nick",
            "web_search",
            i64::from(CATALOG_VERSION),
            0,
        );
        drop(db);
        let config = config_at(&ledger);

        let CapabilityMatrixSource::Available {
            entries,
            now,
            now_ms,
        } = gather_capability_matrix(&config, false, 0)
        else {
            panic!("a matching tombstone with a post-boundary row must be Available");
        };
        assert!(
            now_ms > 0,
            "the pinned epoch-ms anchor is a real wall-clock reading"
        );
        let entry = entries
            .iter()
            .find(|e| e.state_key == "gpt-nick" && e.feature_key == "web_search")
            .expect("the replayed negative is resident");
        assert!(
            entry.last_seen <= now,
            "a future-dated row clamps to the pinned now (age never negative)"
        );
        assert_eq!(
            now.duration_since(entry.last_seen),
            std::time::Duration::ZERO,
            "the age anchor is pinned once; a future-dated row maps to age zero"
        );
    }

    #[test]
    fn gather_leaves_db_byte_identical() {
        let tmp = TempDir::new().expect("tempdir");
        let ledger = tmp.path().join("usage.db");
        let db = open(&ledger).expect("open ledger");
        seed_tombstone(db.conn(), 100, i64::from(CATALOG_VERSION), 0);
        seed_broken(
            db.conn(),
            200,
            "gpt-nick",
            "web_search",
            i64::from(CATALOG_VERSION),
            0,
        );
        drop(db);
        let before = std::fs::read(&ledger).expect("read ledger before");

        let _ = gather_capability_matrix(&config_at(&ledger), false, 0);

        let after = std::fs::read(&ledger).expect("read ledger after");
        assert_eq!(
            before, after,
            "the read-only matrix gather must not mutate the usage db"
        );
    }
}

// ---------------------------------------------------------------------------
// Freshness section: three findings-shaped rows, honest missing, no Fail.
// ---------------------------------------------------------------------------

fn import_state(date: &str) -> CatalogImportState {
    let mut per_source_counts = BTreeMap::new();
    per_source_counts.insert("anthropic-api".to_string(), 7);
    per_source_counts.insert("openai".to_string(), 5);
    CatalogImportState {
        schema_version: 1,
        last_import_date: date.to_string(),
        per_source_counts,
        per_family_counts: BTreeMap::new(),
        source_hashes: BTreeMap::new(),
    }
}

#[test]
fn freshness_renders_three_rows_with_both_honest_missing_lines() {
    let findings = freshness_findings(&sample_freshness());
    assert_eq!(findings.len(), 3, "freshness renders exactly three rows");

    let baked = find(&findings, "freshness", "baked catalog");
    assert!(baked.detail.contains("baked catalog v"));
    assert!(
        baked
            .detail
            .contains(routectl_router::CATALOG_SNAPSHOT_DATE)
    );
    assert_eq!(baked.status, Status::Pass);

    let overlay = find(&findings, "freshness", "overlay");
    assert_eq!(overlay.status, Status::Pass);
    assert!(
        overlay.detail.contains("no overlay verified stamp"),
        "absent overlay stamp must render honestly: {}",
        overlay.detail
    );

    let import = find(&findings, "freshness", "last successful import");
    assert_eq!(import.status, Status::Pass);
    assert_eq!(import.detail, "no successful import recorded");
}

#[test]
fn freshness_never_says_result_only_last_successful_import() {
    let mut f = sample_freshness();
    f.last_import = Some(import_state("2026-07-01"));
    let findings = freshness_findings(&f);
    for finding in &findings {
        assert!(
            !finding.detail.to_lowercase().contains("result"),
            "freshness must never say 'result': {}",
            finding.detail
        );
    }
    let import = find(&findings, "freshness", "last successful import");
    assert!(import.name.contains("last successful import"));
    assert!(import.detail.contains("12 rows across 2 sources"));
    assert!(import.detail.contains("10 days ago"));
}

#[test]
fn freshness_overlay_and_import_warn_when_stale_past_hint() {
    let mut f = sample_freshness();
    f.staleness_hint_days = 14;
    // 30 days before the pinned today: stale past the 14-day hint.
    f.overlay_verified_at = Some("2026-06-11".to_string());
    f.last_import = Some(import_state("2026-06-11"));

    let findings = freshness_findings(&f);
    let overlay = find(&findings, "freshness", "overlay");
    assert_eq!(overlay.status, Status::Warn);
    assert!(overlay.detail.contains("stale past 14 days"));
    assert!(overlay.remediation.is_some());

    let import = find(&findings, "freshness", "last successful import");
    assert_eq!(import.status, Status::Warn);
    assert!(import.remediation.is_some());
}

#[test]
fn freshness_fresh_overlay_and_import_pass() {
    let mut f = sample_freshness();
    f.overlay_verified_at = Some("2026-07-05".to_string());
    f.last_import = Some(import_state("2026-07-05"));

    let findings = freshness_findings(&f);
    assert_eq!(find(&findings, "freshness", "overlay").status, Status::Pass);
    assert_eq!(
        find(&findings, "freshness", "last successful import").status,
        Status::Pass
    );
}

#[test]
fn freshness_future_stamp_ages_clamp_to_zero() {
    let mut f = sample_freshness();
    // A post-dated stamp (skewed clock) must read as 0 days, never negative.
    f.overlay_verified_at = Some("2026-08-11".to_string());
    f.last_import = Some(import_state("2026-08-11"));

    let findings = freshness_findings(&f);
    let overlay = find(&findings, "freshness", "overlay");
    assert_eq!(overlay.status, Status::Pass);
    assert!(overlay.detail.contains("0 days ago"), "{}", overlay.detail);

    let import = find(&findings, "freshness", "last successful import");
    assert!(import.detail.contains("0 days ago"), "{}", import.detail);
    assert!(
        !import.detail.contains("-1 days"),
        "no negative age: {}",
        import.detail
    );
}

#[test]
fn freshness_never_emits_fail() {
    // Exercise every branch: missing, fresh, stale, malformed.
    for overlay in [
        None,
        Some("2026-07-05".to_string()),
        Some("2026-01-01".to_string()),
        Some("not-a-date".to_string()),
    ] {
        for import in [None, Some(import_state("2026-01-01"))] {
            let mut f = sample_freshness();
            f.overlay_verified_at = overlay.clone();
            f.last_import = import;
            for finding in freshness_findings(&f) {
                assert_ne!(
                    finding.status,
                    Status::Fail,
                    "freshness must never emit Fail: {}",
                    finding.detail
                );
            }
        }
    }
}

#[test]
fn freshness_registered_in_both_section_lists_and_render_title() {
    assert!(
        SECTIONS.iter().any(|(k, _)| *k == "freshness"),
        "freshness must be in SECTIONS (CLI doctor)"
    );
    assert!(
        NO_NETWORK_SECTIONS.iter().any(|(k, _)| *k == "freshness"),
        "freshness must be in NO_NETWORK_SECTIONS (offline status doctor)"
    );
    let context = ctx(
        Config::default(),
        Some("version = 3\n"),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    let text = render_human(&report).join("\n");
    assert!(
        text.contains("Catalog freshness"),
        "freshness section title must render: {text}"
    );
}
