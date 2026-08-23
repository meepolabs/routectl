use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::Local;
use routectl_auth::{OAuthError, OAuthStore};
use routectl_providers::anthropic_api::AuthKind;
use routectl_router::{
    AliasValue, CURRENT_CONFIG_VERSION, CatalogImportState, CatalogOverlay, ModelEntry, PoolEntry,
    ProviderEntry,
};
use routectl_testkit::ScopedEnv;

use crate::commands::capability_legacy::present_legacy_capability_keys;
use crate::commands::parse_error_redaction::redact_config_load_error;

use super::gather::{
    build_capability_inputs, derive_knob_rows, derive_pricing_rows, derive_prior_cells,
    gather_capability_matrix, gather_context, gather_context_no_network, gather_orphan_seats,
    gather_orphan_secrets, gather_secret_checks, sanitize_store_open_error,
};
use super::matrix::build_capability_matrix_panel;
use super::sections::{
    freshness_findings, legacy_nudge, secret_finding, section_auth, section_capability,
    section_config, section_knobs, section_pricing, section_probe, section_seat_orphans,
    section_seat_pools, section_secret_orphans, section_version,
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
    let pricing = Some(derive_pricing_rows(&config, &CatalogOverlay::default()));
    let knobs = Some(derive_knob_rows(&config, &CatalogOverlay::default()));
    DoctorContext {
        config,
        raw_config: raw_config.map(str::to_string),
        config_load_error: None,
        probes,
        seats,
        auth_store_error: None,
        secret_checks: Vec::new(),
        orphan_secrets: Vec::new(),
        orphan_seats: Vec::new(),
        probe_results: Vec::new(),
        would_trim: None,
        now_unix: 1_000,
        binary_version: "test",
        capability,
        capability_matrix: CapabilityMatrixSource::Unavailable("no_data"),
        freshness: sample_freshness(),
        pricing,
        knobs,
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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

/// The `version` stamp the running build writes, as raw config text: every
/// fixture whose raw config must PASS the version preflight renders from the
/// const, so a schema bump needs no fixture edit here.
fn current_version_stamp() -> String {
    format!("version = {CURRENT_CONFIG_VERSION}\n")
}

#[test]
fn version_current_passes() {
    let raw = current_version_stamp();
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
    let context = ctx(
        Config::default(),
        Some(&current_version_stamp()),
        Vec::new(),
        seats,
    );
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
        Some(&current_version_stamp()),
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
fn capability_section_drops_absorbed_findings_on_default_config() {
    // On a default config the capability SECTION contributes nothing: the
    // override / prior / learned lines are now structured cells on the
    // matrix panel, and there is no legacy key to nudge.
    let context = ctx(
        Config::default(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    assert!(
        !report.findings.iter().any(|f| f.section == "capability"),
        "capability section must contribute no findings on a default config"
    );
    let text = render_human(&report).join("\n");
    // The superseded finding content is gone.
    assert!(
        !text.contains("runtime-only"),
        "superseded learned line must be gone: {text}"
    );
    assert!(
        !text.contains("no catalog capability priors present"),
        "superseded priors note must be gone: {text}"
    );
    // The matrix panel renders in its place.
    assert!(
        text.contains("capability matrix:"),
        "capability matrix panel missing: {text}"
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
fn override_cells_land_on_the_matrix_panel_with_source_tags() {
    let context = ctx(
        config_with_overrides(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    // The capability SECTION no longer emits an override finding.
    let findings = section_capability(&context);
    assert!(
        !findings.iter().any(|f| f.name == "p"),
        "override rows must no longer be findings"
    );
    assert_eq!(overall_exit(&findings), 0);

    // The provider-scoped overrides land as forced cells on every lane of
    // provider `p` in the matrix panel. `config_with_overrides` has no
    // [models] table, so there is no routed lane to carry them; assert the
    // panel builds and the resolver vocabulary is what the cells would use.
    let panel = build_capability_matrix_panel(&context);
    assert!(
        panel.columns.iter().any(|c| c == "web_search"),
        "well-known columns present: {:?}",
        panel.columns
    );
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
    let cells = &inputs.config.expect("config present").priors;
    assert_eq!(cells.len(), 1);
    assert_eq!(cells[0].nickname, "opus");
    assert_eq!(cells[0].verified_at, today);
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
fn stale_catalog_cell_still_yields_a_prior_for_the_panel() {
    // Staleness is no longer filtered at derivation: a stale overlay cell
    // now yields a prior cell carrying its old `verified_at`, and the matrix
    // panel flags it stale against the operator hint. The prior is surfaced
    // honestly rather than silently dropped.
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
    assert_eq!(cells.len(), 1, "stale cell must still yield a prior");
    assert_eq!(cells[0].verified_at, "2000-01-01");
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
fn overlay_unavailable_leaves_priors_absent_and_still_builds_the_panel() {
    // overlay = None simulates an unreadable overlay: priors are absent
    // (empty), but the config parsed, so the capability section still emits
    // no absorbed findings and the matrix panel still builds (learned +
    // override cells unaffected).
    let inputs = build_capability_inputs(&config_with_overrides(), None, None);
    assert!(
        inputs
            .config
            .as_ref()
            .expect("config present")
            .priors
            .is_empty(),
        "an unreadable overlay leaves priors absent"
    );
    let context = DoctorContext {
        capability: inputs,
        ..ctx(config_with_overrides(), Some("x"), Vec::new(), Vec::new())
    };
    let findings = section_capability(&context);
    assert!(
        !findings.iter().any(|f| f.name == "catalog priors"),
        "the priors-unavailable finding is superseded by the panel"
    );
    assert_eq!(overall_exit(&findings), 0);
    // The panel builds regardless of overlay availability.
    let _ = build_capability_matrix_panel(&context);
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
    let context = ctx(
        config,
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    let findings = section_capability(&context);
    assert!(
        findings.iter().all(|f| f.name != "legacy keys"),
        "no nudge without a legacy list"
    );
}

#[test]
fn schema_version_is_eleven() {
    assert_eq!(SCHEMA_VERSION, 11);

    let context = ctx(
        config_with_overrides(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    let report = build_report(&context);
    assert_eq!(report.schema_version, 11);

    // JSON mode carries the structured capability matrix panel; the
    // superseded override / prior / learned finding text is gone.
    let value = serde_json::to_value(&report).expect("serialize");
    let blob = value.to_string();
    assert!(
        blob.contains("capability_matrix"),
        "json missing the capability matrix panel"
    );
    assert!(
        !blob.contains("route-away"),
        "superseded override finding text must be gone"
    );
    assert!(
        !blob.contains("runtime-only"),
        "superseded learned line must be gone"
    );
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
        Some(&current_version_stamp()),
        vec![
            ("anthropic", LocalProbe::Present),
            ("codex", LocalProbe::Missing),
        ],
        Vec::new(),
    ));
    let b = build_report(&ctx(
        cfg,
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
            Some(&current_version_stamp()),
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
            Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
        Some(&current_version_stamp()),
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
    std::fs::write(&config_path, current_version_stamp().as_bytes()).unwrap();

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
    std::fs::write(
        &config_path,
        format!("version = {CURRENT_CONFIG_VERSION}\nbogus_key = true\n").as_bytes(),
    )
    .unwrap();

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

/// The alias-reaching config with its auth selector set, so the
/// advisory half of the validator suite is empty too. `Config::default()`
/// alone would not do: the config section's Pass is only meaningful on a
/// config that actually carries providers and models.
fn clean_config() -> Config {
    let mut cfg = config_referencing_anthropic();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic").with_auth_kind(AuthKind::OauthBearer),
    );
    cfg
}

/// A config carrying the advisory half only -- an `oauth://` ref whose
/// auth selector is unset -- must render one Warn finding per warning and
/// NOT the "passes the static validator suite" Pass, whose message the
/// advisory contradicts.
#[test]
fn validator_warnings_render_as_warn_findings_without_a_pass() {
    // Arrange: no errors, at least one warning.
    let cfg = config_referencing_anthropic();
    let report = crate::commands::config::validation_report(&cfg, None);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(!report.warnings.is_empty(), "fixture must warn");
    let expected_warnings = report.warnings.len();
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    // Act
    let findings = section_config(&context);

    // Assert
    let validation: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.section == "config" && f.name == "validation")
        .collect();
    assert_eq!(validation.len(), expected_warnings, "{validation:?}");
    for f in &validation {
        assert_eq!(f.status, Status::Warn);
        assert!(f.remediation.is_some(), "{f:?}");
    }
    assert!(
        !findings
            .iter()
            .any(|f| f.section == "config" && f.status == Status::Pass),
        "the validator Pass must not claim a clean suite alongside an advisory: {findings:?}"
    );
}

/// The doctor render shares `config check`'s ceiling, so a long-but-benign
/// advisory (the auth-selector gap message runs past the 256-char log-field
/// budget once the provider name is long) reaches the operator whole.
#[test]
fn a_long_validator_warning_renders_untruncated_in_the_finding_detail() {
    // Arrange: one advisory warning, longer than the 256-char log budget.
    let mut cfg = Config::default();
    let long_name = "anthropic-seat-for-the-shared-review-workload-primary";
    cfg.providers.insert(
        long_name.to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    let report = crate::commands::config::validation_report(&cfg, None);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    let warning = report.warnings[0].clone();
    assert!(
        warning.chars().count() > 256,
        "fixture guard: warning must exceed the 256-char budget, got {}",
        warning.chars().count()
    );
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    // Act
    let findings = section_config(&context);

    // Assert
    let validation: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.section == "config" && f.name == "validation")
        .collect();
    assert_eq!(validation.len(), 1, "{validation:?}");
    assert_eq!(validation[0].status, Status::Warn);
    assert_eq!(
        validation[0].detail, warning,
        "the doctor render must not truncate a legitimate advisory"
    );
}

/// A warnings-only config is exit-code-neutral: the WARN findings render and
/// the overall exit stays 0, so an advisory never turns a healthy doctor run
/// into a failing one.
#[test]
fn a_warnings_only_config_renders_warn_findings_and_exits_zero() {
    let context = ctx(
        config_referencing_anthropic(),
        Some(&current_version_stamp()),
        vec![("anthropic", LocalProbe::Present)],
        Vec::new(),
    );

    let report = build_report(&context);

    assert!(
        report
            .findings
            .iter()
            .any(|f| f.section == "config" && f.name == "validation" && f.status == Status::Warn),
        "expected a config validation Warn: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().all(|f| f.status != Status::Fail),
        "fixture must carry no Fail: {:?}",
        report.findings
    );
    assert_eq!(overall_exit(&report.findings), 0);
}

/// Errors do not suppress advisories: a config tripping both halves renders
/// every error as a Fail AND every warning as a Warn.
#[test]
fn errors_and_warnings_are_both_rendered() {
    let mut cfg = config_referencing_anthropic();
    cfg.models.insert(
        "ghost-bound".to_string(),
        ModelEntry::new("ghost", "claude-sonnet-4-5"),
    );
    let report = crate::commands::config::validation_report(&cfg, None);
    assert!(!report.errors.is_empty(), "fixture must error");
    assert!(!report.warnings.is_empty(), "fixture must warn");
    let (expected_fails, expected_warns) = (report.errors.len(), report.warnings.len());
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    let findings = section_config(&context);

    let validation: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.section == "config" && f.name == "validation")
        .collect();
    assert_eq!(
        validation
            .iter()
            .filter(|f| f.status == Status::Fail)
            .count(),
        expected_fails,
        "{validation:?}"
    );
    assert_eq!(
        validation
            .iter()
            .filter(|f| f.status == Status::Warn)
            .count(),
        expected_warns,
        "{validation:?}"
    );
}

/// The Pass finding survives on a config tripping neither half -- the
/// positive control for the two negative gates above.
#[test]
fn clean_config_passes_validation() {
    let cfg = clean_config();
    let report = crate::commands::config::validation_report(&cfg, None);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    let findings = section_config(&context);

    let validation: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.section == "config" && f.name == "validation")
        .collect();
    assert_eq!(validation.len(), 1, "{validation:?}");
    assert_eq!(validation[0].status, Status::Pass);
    assert!(validation[0].remediation.is_none());
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
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
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

/// A validator message embeds the operator-written table keys it names, so the
/// config battery's one render point control-char-filters the whole suite's
/// output: a key bearing a newline plus an ANSI sequence would otherwise forge
/// a fabricated PASS line in the human render.
#[test]
fn a_control_byte_bearing_config_key_cannot_forge_a_finding_line() {
    // Arrange: the hostile bytes arrive as a `[models]` provider value, which
    // the unknown-provider validator formats into its message verbatim.
    let mut cfg = Config::default();
    cfg.models.insert(
        "sonnet".to_string(),
        ModelEntry::new(
            "ghost\n  \u{1b}[32mPASS forged: all good",
            "claude-sonnet-4-5",
        ),
    );
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    // Act
    let findings = section_config(&context);

    // Assert
    let f = find(&findings, "config", "validation");
    assert_eq!(f.status, Status::Fail);
    assert!(!f.detail.contains('\n'), "{:?}", f.detail);
    assert!(!f.detail.contains('\u{1b}'), "{:?}", f.detail);
    assert!(
        f.detail.chars().all(|c| c.is_ascii_graphic() || c == ' '),
        "{:?}",
        f.detail
    );
}

/// The warning half runs through the SAME control-char filter as the error
/// half: a warning message embeds the operator-written provider name, so an
/// unsanitized advisory render would forge a fabricated finding line just as
/// an unsanitized error would.
#[test]
fn a_control_byte_bearing_config_key_cannot_forge_a_finding_line_through_a_warning() {
    // Arrange: an `oauth://` ref with no auth selector trips the advisory,
    // whose message formats the provider name verbatim.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "claude\n  \u{1b}[32mPASS forged: all good".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    // Act
    let findings = section_config(&context);

    // Assert
    let warnings: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.section == "config" && f.name == "validation" && f.status == Status::Warn)
        .collect();
    assert!(!warnings.is_empty(), "{findings:?}");
    for f in &warnings {
        assert!(!f.detail.contains('\n'), "{:?}", f.detail);
        assert!(!f.detail.contains('\u{1b}'), "{:?}", f.detail);
        assert!(
            f.detail.chars().all(|c| c.is_ascii_graphic() || c == ' '),
            "{:?}",
            f.detail
        );
    }
    // Positive control: the sanitized detail still names the entry, so the
    // filter is not passing by emptying the message.
    assert!(
        warnings.iter().any(|f| f.detail.contains("claude")),
        "{warnings:?}"
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
    let mut context = ctx(
        cfg.clone(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
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
        Some(&current_version_stamp()),
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

/// The DEFAULT seat a bare `oauth://<provider>` ref names is NOT an orphan.
/// Schema version 4 scoped a bare ref to exactly that seat.
#[test]
fn referenced_default_oauth_seat_is_not_an_orphan() {
    let cfg = config_referencing_anthropic();
    let seats = vec![("anthropic".to_string(), token_record(9_000))];

    assert!(gather_orphan_seats(&cfg, &seats).is_empty());
}

/// A LABELLED seat no ref names is an orphan even though a bare-ref entry for
/// its family exists: schema version 4 retired the bare-ref-covers-every-seat
/// reading, so the bare ref vouches for the default seat alone. This is the
/// verdict the pool world flips -- under the old rule the labelled seat read
/// as covered.
#[test]
fn a_labelled_seat_is_an_orphan_despite_a_bare_ref_for_its_family() {
    let cfg = config_referencing_anthropic();
    let seats = vec![
        ("anthropic".to_string(), token_record(9_000)),
        ("anthropic#seat-b".to_string(), token_record(9_000)),
    ];

    assert_eq!(
        gather_orphan_seats(&cfg, &seats),
        vec!["anthropic#seat-b".to_string()],
        "a bare ref covers the default seat only"
    );
}

/// The migrated shape: one account entry per stored seat, grouped by a pool.
/// Every labelled seat is then covered by its own member's pinned ref, so no
/// seat of a migrated provider is reported orphaned.
#[test]
fn no_seat_of_a_migrated_pooled_provider_is_an_orphan() {
    let cfg: Config = toml::from_str(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-seat-b]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#seat-b\"\n\
         [pools.anthropic-pool]\n\
         members = [\"anthropic\", \"anthropic-seat-b\"]\n",
    )
    .expect("pooled fixture parses");
    let seats = vec![
        ("anthropic".to_string(), token_record(9_000)),
        ("anthropic#seat-b".to_string(), token_record(9_000)),
    ];

    assert!(
        gather_orphan_seats(&cfg, &seats).is_empty(),
        "pool membership covers every labelled seat its members name"
    );
}

/// A stored seat no provider entry references surfaces as a Warn naming the
/// seat, and the scan neither refreshes nor removes the credential.
#[test]
fn unreferenced_oauth_seat_warns_by_name() {
    let cfg = config_referencing_anthropic();
    let seats = vec![
        ("anthropic".to_string(), token_record(9_000)),
        ("codex".to_string(), token_record(9_000)),
    ];

    let orphans = gather_orphan_seats(&cfg, &seats);
    assert_eq!(orphans, vec!["codex".to_string()]);

    let mut context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), seats);
    context.orphan_seats = orphans;
    let findings = section_seat_orphans(&context);
    let f = find(&findings, "seats", "codex");
    assert_eq!(f.status, Status::Warn);
    assert_eq!(
        f.detail,
        "seat `codex` has stored credentials but no provider entry uses it"
    );
    assert!(f.remediation.is_some());
    // Advisory only: an orphan seat never flips the exit code.
    assert_eq!(overall_exit(&findings), 0);
}

/// A LABELLED ref pins exactly one seat: it does not cover the provider's
/// default seat, which is reported as the orphan it is.
#[test]
fn labelled_ref_does_not_cover_the_default_seat() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic#seat-b"),
    );
    let seats = vec![
        ("anthropic".to_string(), token_record(9_000)),
        ("anthropic#seat-b".to_string(), token_record(9_000)),
    ];

    assert_eq!(
        gather_orphan_seats(&cfg, &seats),
        vec!["anthropic".to_string()],
        "the default seat is not covered by a label-pinned ref"
    );
}

/// The converse: a ref pinning one label leaves a DIFFERENT stored label an
/// orphan -- sibling labels are distinct seats, never interchangeable.
#[test]
fn labelled_ref_does_not_cover_a_sibling_label() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic#seat-b"),
    );
    let seats = vec![
        ("anthropic#seat-a".to_string(), token_record(9_000)),
        ("anthropic#seat-b".to_string(), token_record(9_000)),
    ];

    assert_eq!(
        gather_orphan_seats(&cfg, &seats),
        vec!["anthropic#seat-a".to_string()],
        "a sibling label is a distinct seat"
    );
}

/// The orphan-seat remediation targets EXACTLY the reported seat: a labelled
/// seat's logout hint carries `--label`, or the bare form would remove the
/// default seat and leave the orphan in place.
#[test]
fn labelled_orphan_remediation_carries_the_label() {
    let mut context = ctx(
        Config::default(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    context.orphan_seats = vec!["anthropic#seat-a".to_string(), "codex".to_string()];
    let findings = section_seat_orphans(&context);

    let labelled = find(&findings, "seats", "anthropic#seat-a")
        .remediation
        .clone()
        .expect("labelled orphan carries a remediation");
    assert!(
        labelled.contains("routectl logout anthropic --label seat-a"),
        "labelled logout hint must pin the label: {labelled}"
    );
    let default = find(&findings, "seats", "codex")
        .remediation
        .clone()
        .expect("default orphan carries a remediation");
    assert!(
        default.contains("routectl logout codex"),
        "default logout hint must name the provider: {default}"
    );
    assert!(
        !default.contains("--label"),
        "the default seat takes no label: {default}"
    );
}

/// A seat label is operator-written (`login --label`), so neither the orphan
/// finding nor its copy-pasteable `logout` remediation may carry control bytes:
/// a label embedding a newline plus an ANSI sequence would otherwise forge a
/// fabricated finding line in the doctor human render.
#[test]
fn a_control_byte_bearing_orphan_seat_label_cannot_forge_a_finding_line() {
    // Arrange
    let hostile = "anthropic#a\n  \u{1b}[32mPASS forged: all good";
    let mut context = ctx(
        Config::default(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    context.orphan_seats = vec![hostile.to_string()];

    // Act
    let findings = section_seat_orphans(&context);

    // Assert
    assert_eq!(findings.len(), 1, "{findings:?}");
    let f = &findings[0];
    let remediation = f.remediation.as_deref().expect("orphan carries a fix");
    for text in [f.name.as_str(), f.detail.as_str(), remediation] {
        assert!(!text.contains('\n'), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(
            text.chars().all(|c| c.is_ascii_graphic() || c == ' '),
            "{text}"
        );
    }
}

/// Same treatment on the auth section, which names every stored seat key: a
/// hostile label reaches `Finding::name` there and, for an expired seat, the
/// `login` remediation too.
#[test]
fn a_control_byte_bearing_seat_label_cannot_forge_an_auth_finding_line() {
    // Arrange: one usable seat and one expired seat, both hostile-labelled, so
    // both arms of the auth finding are covered.
    let hostile_live = "anthropic#live\n  \u{1b}[32mPASS forged: all good";
    let hostile_expired = "anthropic#dead\n  \u{1b}[31mPASS forged: all good";
    let context = ctx(
        Config::default(),
        Some(&current_version_stamp()),
        Vec::new(),
        vec![
            (hostile_live.to_string(), token_record(9_000)),
            (hostile_expired.to_string(), token_record_no_refresh(1)),
        ],
    );

    // Act
    let findings = section_auth(&context);

    // Assert
    assert_eq!(findings.len(), 2, "{findings:?}");
    for f in &findings {
        let texts = [f.name.as_str(), f.detail.as_str()];
        for text in texts.iter().copied().chain(f.remediation.as_deref()) {
            assert!(!text.contains('\n'), "{text}");
            assert!(!text.contains('\u{1b}'), "{text}");
            assert!(
                text.chars().all(|c| c.is_ascii_graphic() || c == ' '),
                "{text}"
            );
        }
    }
}

/// No token material, refresh token, or account identity from a stored seat
/// reaches the rendered report through the orphan-seat section -- on either
/// the human or the JSON surface.
#[test]
fn orphan_seat_report_never_leaks_token_material() {
    const ACCESS: &str = "at-FAKE-SEAT-LEAK-ACCESS";
    const REFRESH: &str = "rt-FAKE-SEAT-LEAK-REFRESH";
    const EMAIL: &str = "leaky@example.invalid";

    let leaky: TokenRecord = serde_json::from_value(serde_json::json!({
        "access_token": ACCESS,
        "refresh_token": REFRESH,
        "token_type": "Bearer",
        "expires_at_unix": 9_000,
        "scopes": ["user:inference"],
        "account": { "email": EMAIL, "account_id": "acct-leak" },
        "obtained_at_unix": 0,
    }))
    .expect("valid TokenRecord json");

    let seats = vec![("codex".to_string(), leaky)];
    let orphans = gather_orphan_seats(&Config::default(), &seats);
    assert_eq!(orphans, vec!["codex".to_string()]);

    let mut context = ctx(
        Config::default(),
        Some(&current_version_stamp()),
        Vec::new(),
        seats.clone(),
    );
    context.orphan_seats = orphans;
    let report = build_report(&context);
    assert_eq!(
        find(&report.findings, "seats", "codex").status,
        Status::Warn
    );

    let human = render_human(&report).join("\n");
    let json = serde_json::to_string(&report).expect("report serializes");
    for surface in [&human, &json] {
        assert!(!surface.contains(ACCESS), "access token leaked: {surface}");
        assert!(
            !surface.contains(REFRESH),
            "refresh token leaked: {surface}"
        );
        assert!(!surface.contains(EMAIL), "account email leaked: {surface}");
        assert!(
            !surface.contains("acct-leak"),
            "account id leaked: {surface}"
        );
        assert!(
            !surface.contains("credentials.json"),
            "a storage path leaked: {surface}"
        );
    }
}

/// The orphan-seat section is wired into BOTH the CLI battery and the
/// offline status doctor, so an orphan is reported on either surface.
#[test]
fn seats_section_is_in_both_section_lists() {
    assert!(
        SECTIONS.iter().any(|(k, _)| *k == "seats"),
        "seats must be in SECTIONS (CLI doctor)"
    );
    assert!(
        NO_NETWORK_SECTIONS.iter().any(|(k, _)| *k == "seats"),
        "seats must be in NO_NETWORK_SECTIONS (offline status doctor)"
    );
}

/// A config with two provider entries reaching the same oauth provider: one
/// bare default-seat ref and one label-pinned ref, neither claimed by a pool.
fn config_with_pool_and_pinned_refs() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.providers.insert(
        "anthropic-work".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic#work"),
    );
    cfg
}

/// A two-account pool over the same two entries -- the shape `config migrate`
/// produces.
fn config_with_a_named_pool() -> Config {
    toml::from_str(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [pools.anthropic-pool]\n\
         members = [\"anthropic\", \"anthropic-work\"]\n\
         seat_selection = \"round-robin\"\n\
         accepts_new_logins = true\n",
    )
    .expect("pooled fixture parses")
}

/// Standalone ref rows stay purely informational: every finding is Pass with
/// no remediation, for BOTH a bare default-seat ref and a label-pinned ref,
/// and they cannot move the overall exit code.
#[test]
fn standalone_seat_ref_findings_are_pass_without_remediation_and_never_move_the_exit() {
    // Arrange
    let context = ctx(
        config_with_pool_and_pinned_refs(),
        Some(&current_version_stamp()),
        Vec::new(),
        vec![
            ("anthropic".to_string(), token_record(9_000)),
            ("anthropic#work".to_string(), token_record(9_000)),
        ],
    );

    // Act
    let pools = section_seat_pools(&context);
    let report = build_report(&context);
    let without_pools: Vec<Finding> = report
        .findings
        .iter()
        .filter(|f| f.section != "pools")
        .cloned()
        .collect();

    // Assert
    assert_eq!(pools.len(), 2, "one finding per oauth ref: {pools:?}");
    for f in &pools {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
    assert!(
        find(&report.findings, "pools", "anthropic")
            .detail
            .contains("pins the default seat"),
        "{:?}",
        find(&report.findings, "pools", "anthropic")
    );
    assert!(
        find(&report.findings, "pools", "anthropic-work")
            .detail
            .contains("pins 1 seat"),
        "{:?}",
        find(&report.findings, "pools", "anthropic-work")
    );
    assert_eq!(
        overall_exit(&report.findings),
        overall_exit(&without_pools),
        "standalone ref findings must not change the exit code"
    );
}

/// A named pool renders as ONE finding carrying its members, their seats, the
/// strategy, and the growth marker -- and its members are not repeated as
/// standalone rows.
#[test]
fn a_named_pool_renders_one_finding_with_members_strategy_and_growth_marker() {
    // Arrange
    let context = ctx(
        config_with_a_named_pool(),
        Some(&current_version_stamp()),
        Vec::new(),
        vec![
            ("anthropic".to_string(), token_record(9_000)),
            ("anthropic#work".to_string(), token_record(9_000)),
        ],
    );

    // Act
    let pools = section_seat_pools(&context);

    // Assert
    assert_eq!(pools.len(), 1, "the pool absorbs both members: {pools:?}");
    let pool = find(&pools, "pools", "anthropic-pool");
    assert_eq!(pool.status, Status::Pass);
    assert!(pool.remediation.is_none(), "{pool:?}");
    assert_eq!(
        pool.detail,
        "pool `anthropic-pool` has 2 members (anthropic=default, anthropic-work=work); \
         seat_selection round-robin; accepts new logins: yes"
    );
}

/// A pool one member of which has no stored credential is a Warn naming that
/// member; a pool no member of which has one is a Fail, because every model
/// naming it is unroutable.
#[test]
fn a_pool_missing_credentials_warns_and_a_pool_with_none_fails() {
    // Arrange
    let degraded_ctx = ctx(
        config_with_a_named_pool(),
        Some(&current_version_stamp()),
        Vec::new(),
        vec![("anthropic".to_string(), token_record(9_000))],
    );
    let unusable_ctx = ctx(
        config_with_a_named_pool(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );

    // Act
    let degraded = find(
        &section_seat_pools(&degraded_ctx),
        "pools",
        "anthropic-pool",
    )
    .clone();
    let unusable = find(
        &section_seat_pools(&unusable_ctx),
        "pools",
        "anthropic-pool",
    )
    .clone();

    // Assert
    assert_eq!(degraded.status, Status::Warn);
    assert!(
        degraded
            .detail
            .ends_with("; no stored credential for anthropic-work"),
        "{degraded:?}"
    );
    assert!(degraded.remediation.is_some(), "{degraded:?}");
    assert_eq!(unusable.status, Status::Fail);
    assert!(
        unusable
            .detail
            .ends_with("; no member has a stored credential"),
        "{unusable:?}"
    );
    assert!(unusable.remediation.is_some(), "{unusable:?}");
}

/// An unreadable credential store still renders the rows, with presence
/// unknown and the strategy retained -- and claims neither Warn nor Fail. The
/// store Fail stays the auth section's alone.
#[test]
fn seat_pool_findings_stay_pass_with_unknown_presence_when_the_store_is_unreadable() {
    // Arrange
    let config: Config = toml::from_str(
        "version = 3\n\
         [providers.anthropic]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic\"\n\
         [providers.anthropic-work]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#work\"\n\
         [pools.anthropic-pool]\n\
         members = [\"anthropic\"]\n\
         seat_selection = \"round-robin\"\n",
    )
    .expect("fixture config parses");
    let mut context = ctx(
        config,
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    context.auth_store_error = Some("oauth credentials store could not be opened".to_string());

    // Act
    let pools = section_seat_pools(&context);

    // Assert
    for f in &pools {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
    let pool = find(&pools, "pools", "anthropic-pool");
    assert!(
        pool.detail.contains("seat_selection round-robin"),
        "an unknown presence answer must still render the configured strategy: {pool:?}"
    );
    assert!(
        pool.detail
            .ends_with("; member credential presence unknown (credential store unavailable)"),
        "{pool:?}"
    );
    let standalone = find(&pools, "pools", "anthropic-work");
    assert!(
        standalone
            .detail
            .contains("store presence unknown - credential store unavailable"),
        "{standalone:?}"
    );
}

/// The pool section is wired into BOTH the CLI battery and the offline status
/// doctor, and the human render carries its title.
#[test]
fn pools_section_is_in_both_section_lists_and_renders_its_title() {
    // Arrange / Act / Assert
    assert!(
        SECTIONS.iter().any(|(k, _)| *k == "pools"),
        "pools must be in SECTIONS (CLI doctor)"
    );
    assert!(
        NO_NETWORK_SECTIONS.iter().any(|(k, _)| *k == "pools"),
        "pools must be in NO_NETWORK_SECTIONS (offline status doctor)"
    );

    let context = ctx(
        config_with_pool_and_pinned_refs(),
        Some(&current_version_stamp()),
        Vec::new(),
        Vec::new(),
    );
    let text = render_human(&build_report(&context)).join("\n");
    assert!(text.contains("[OAuth seat pools]"), "{text}");
}

/// End-to-end through the real gather: a stored seat with no matching
/// provider entry surfaces on the report, and the run leaves the credentials
/// file byte-identical.
#[tokio::test]
#[serial_test::serial]
async fn full_run_reports_an_orphan_seat_without_touching_the_store() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    let cfg_dir = tmp.path().join("routectl");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_path = cfg_dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {CURRENT_CONFIG_VERSION}\n[providers.anthropic]\nkind = \
             \"anthropic-api\"\napi_key_ref = \"oauth://anthropic#seat-b\"\n"
        )
        .as_bytes(),
    )
    .unwrap();

    // Two stored seats: the label-pinned one the config reaches, and the
    // default seat nothing references.
    let creds_path = cfg_dir.join("credentials.json");
    std::fs::write(
        &creds_path,
        br#"{"schema_version":1,"providers":{
            "anthropic":{"access_token":"at-orphan","refresh_token":"rt-orphan",
              "expires_at_unix":9999999999,"scopes":["user:inference"],"obtained_at_unix":0},
            "anthropic#seat-b":{"access_token":"at-used","refresh_token":"rt-used",
              "expires_at_unix":9999999999,"scopes":["user:inference"],"obtained_at_unix":0}
        }}"#,
    )
    .unwrap();
    std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let before = snapshot_dir(tmp.path());
    let context = gather_context_no_network(&config_path).await;
    let report = build_report_no_network(&context);
    let after = snapshot_dir(tmp.path());

    assert_eq!(before, after, "the seat scan must not mutate any file");
    assert_eq!(
        find(&report.findings, "seats", "anthropic").status,
        Status::Warn
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.section == "seats" && f.name == "anthropic#seat-b"),
        "the referenced seat must not be reported as an orphan"
    );
    let human = render_human(&report).join("\n");
    assert!(!human.contains("at-orphan"), "token leaked: {human}");
    assert!(!human.contains("rt-orphan"), "token leaked: {human}");
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
        format!(
            "version = {CURRENT_CONFIG_VERSION}\n[providers.anthropic]\nkind = \
             \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n"
        )
        .as_bytes(),
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
        orphan_seats: Vec::new(),
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
        pricing: Some(Vec::new()),
        knobs: Some(Vec::new()),
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

/// The operator-facing battery table in `docs/CONFIGURATION.md`, as the
/// section key of each row in document order.
///
/// The table went stale twice by omission (a new section landed with no row),
/// which no gate caught: the table is prose, and the section list it documents
/// lives in another crate directory. `include_str!` also makes a moved or
/// renamed docs file a compile error rather than a silently skipped check.
fn documented_battery_sections() -> Vec<String> {
    const CONFIGURATION_MD: &str = include_str!("../../../../../docs/CONFIGURATION.md");
    const TABLE_INTRO: &str = "The battery sections, in render order:";

    let mut lines = CONFIGURATION_MD.lines();
    lines
        .by_ref()
        .find(|line| line.trim() == TABLE_INTRO)
        .expect("battery table intro line present in docs/CONFIGURATION.md");

    lines
        .skip_while(|line| !line.starts_with("| Section "))
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .map(|row| {
            let first_cell = row
                .trim_start_matches('|')
                .split('|')
                .next()
                .expect("a table row has a first cell");
            let key = first_cell
                .split('`')
                .nth(1)
                .expect("each battery row names its section key in backticks");
            key.to_string()
        })
        .collect()
}

/// Every [`SECTIONS`] key is documented in the battery table, in render
/// order, and the table names no section the battery does not run.
#[test]
fn battery_table_documents_every_section_in_render_order() {
    // Arrange: the section keys the command runs, in render order.
    let rendered: Vec<String> = SECTIONS.iter().map(|(key, _)| (*key).to_string()).collect();

    // Act: the section keys the operator docs publish, in document order.
    let documented = documented_battery_sections();

    // Assert: same keys, same order -- an added section with no row, a
    // removed section with a surviving row, and a reordered battery all fail.
    assert_eq!(
        documented, rendered,
        "docs/CONFIGURATION.md's battery table must list every SECTIONS key in render order"
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
        format!(
            "version = {CURRENT_CONFIG_VERSION}\n[providers.anthropic]\nkind = \
             \"anthropic-api\"\napi_key_ref = \"oauth://anthropic\"\n"
        )
        .as_bytes(),
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
        Some(&current_version_stamp()),
        vec![("anthropic", LocalProbe::Present)],
        Vec::new(),
    );
    context.probe_results = vec![("anthropic".to_string(), ProbeOutcome::Reachable)];

    let network = build_report(&context);
    let no_net = build_report_no_network(&context);

    assert_eq!(no_net.schema_version, 11);
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
    std::fs::write(&config_path, current_version_stamp().as_bytes()).unwrap();
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

/// The pricing section names each configured model's rate SOURCE. The
/// catalog-fill case is the one this feature added, so it is asserted by
/// nickname alongside the registry, subscription, and unpriced cases -- naming
/// the wrong source would send an operator to the wrong knob.
#[test]
fn pricing_section_names_each_models_rate_source_by_nickname() {
    // Arrange: five models, one per state.
    //   `filled`   -- an anthropic-api model the baked catalog prices, no
    //                 [registry] row -> Catalog.
    //   `explicit` -- the same upstream WITH a [registry] row -> Registry.
    //   `seat`     -- an oauth:// provider -> Subscription.
    //   `pooled`   -- a model routed at a POOL of oauth:// members. A pool name
    //                 is not a provider key, so a providers-only lookup would
    //                 resolve neither the subscription nor a kind and misreport
    //                 it as unpriced.
    //   `nothing`  -- an openai-compat model whose only matching baked cell is
    //                 the price-ambiguous catch-all -> Unpriced.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "paid".to_string(),
        ProviderEntry::anthropic_api("env://PAID_KEY"),
    );
    cfg.providers.insert(
        "billed".to_string(),
        ProviderEntry::anthropic_api("env://BILLED_KEY"),
    );
    cfg.providers.insert(
        "seatprov".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.providers.insert(
        "vendor".to_string(),
        ProviderEntry::openai_compat("https://example.invalid", "env://VENDOR_KEY"),
    );
    cfg.pools.insert(
        "anthropic-pool".to_string(),
        PoolEntry::new(vec!["seatprov".to_string()]),
    );
    cfg.models.insert(
        "filled".to_string(),
        ModelEntry::new("paid", "claude-sonnet-4-6"),
    );
    cfg.models.insert(
        "explicit".to_string(),
        ModelEntry::new("billed", "priced-by-operator"),
    );
    cfg.models.insert(
        "seat".to_string(),
        ModelEntry::new("seatprov", "claude-sonnet-4-6"),
    );
    cfg.models.insert(
        "pooled".to_string(),
        ModelEntry::new("anthropic-pool", "claude-sonnet-4-6"),
    );
    cfg.models.insert(
        "nothing".to_string(),
        ModelEntry::new("vendor", "some-unpriced-vendor-model"),
    );
    cfg.registry.insert(
        "priced-by-operator".to_string(),
        routectl_router::RegistryEntry {
            pricing: Some(routectl_router::PricingConfig {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                ..Default::default()
            }),
            provider: None,
        },
    );

    // Act
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_pricing(&context);

    // Assert: one row per configured model, each naming its own source.
    assert_eq!(
        findings.len(),
        5,
        "one row per configured model: {findings:?}"
    );

    let filled = find(&findings, "pricing", "filled");
    assert!(
        filled.detail.contains("priced from the baked catalog"),
        "the catalog-fill row must name the catalog: {}",
        filled.detail
    );
    assert!(
        filled.detail.contains("$3") && filled.detail.contains("$15"),
        "the catalog-fill row must name its resolved rates: {}",
        filled.detail
    );

    let explicit = find(&findings, "pricing", "explicit");
    assert!(
        explicit.detail.contains("priced from the [registry] table"),
        "an operator-priced row must name the registry: {}",
        explicit.detail
    );

    let seat = find(&findings, "pricing", "seat");
    assert!(
        seat.detail.contains("billed by subscription"),
        "a managed-OAuth row must report as subscription: {}",
        seat.detail
    );

    let pooled = find(&findings, "pricing", "pooled");
    assert!(
        pooled.detail.contains("billed by subscription"),
        "a pool of managed-OAuth members must report as subscription, not \
         unpriced: {}",
        pooled.detail
    );
    assert!(
        !pooled.detail.contains("unpriced"),
        "a pool name is not a provider key, but must not fall through to \
         unpriced: {}",
        pooled.detail
    );

    let nothing = find(&findings, "pricing", "nothing");
    assert!(
        nothing.detail.contains("unpriced"),
        "a row neither layer prices must say so: {}",
        nothing.detail
    );
    assert!(
        !nothing.detail.contains("$0"),
        "an unpriced row must never render a fabricated zero: {}",
        nothing.detail
    );

    // Purely informational: no row here can move the exit code.
    for f in &findings {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
}

/// A subscription model whose `[registry]` row prices EVERY dimension is the one
/// configuration under which `usage` reports an API-equivalent value, so the row
/// must say the basis is complete -- and must still lead with the billed-by-seat
/// statement, which the equivalence clause does not replace.
#[test]
fn a_subscription_row_with_complete_registry_rates_reports_a_complete_basis() {
    // Arrange: an oauth:// provider plus a [registry] row pricing all five
    // dimensions the equivalent needs.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "seatprov".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.models.insert(
        "seat".to_string(),
        ModelEntry::new("seatprov", "priced-everywhere"),
    );
    cfg.registry.insert(
        "priced-everywhere".to_string(),
        routectl_router::RegistryEntry {
            pricing: Some(routectl_router::PricingConfig {
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(15.0),
                cache_read_per_mtok: Some(0.3),
                cache_write_5m_per_mtok: Some(3.75),
                cache_write_1h_per_mtok: Some(6.0),
            }),
            provider: None,
        },
    );

    // Act
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_pricing(&context);
    let seat = find(&findings, "pricing", "seat");

    // Assert: both clauses, and the priced-row vocabulary is not borrowed.
    assert!(
        seat.detail.contains("billed by subscription"),
        "the billed-by-seat statement must survive the added clause: {}",
        seat.detail
    );
    assert!(
        seat.detail.contains("complete via the [registry] table"),
        "a fully priced [registry] row is a complete equivalence basis: {}",
        seat.detail
    );
    assert!(
        !seat.detail.contains("NO API-equivalent"),
        "a complete basis must not also claim the equivalent is absent: {}",
        seat.detail
    );
    assert!(
        !seat.detail.contains("priced from"),
        "a subscription row must not read as priced by that layer: {}",
        seat.detail
    );
}

/// The catalog can never supply cache rates (it leaves them unset rather than
/// deriving them from unconfirmed multipliers), so a subscription model with no
/// `[registry]` row ALWAYS has an incomplete equivalence basis. That is the case
/// an operator hits by default, and the row must name the three missing cache
/// dimensions rather than saying "incomplete" and leaving them to guess.
#[test]
fn a_subscription_row_on_catalog_rates_names_the_missing_cache_dimensions() {
    // Arrange: an oauth:// provider on an upstream the baked catalog prices,
    // with no [registry] row -- base rates resolve, cache rates cannot.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "seatprov".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.models.insert(
        "seat".to_string(),
        ModelEntry::new("seatprov", "claude-sonnet-4-6"),
    );

    // Act
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_pricing(&context);
    let seat = find(&findings, "pricing", "seat");

    // Assert: the absent equivalent is explained by NAME, per dimension.
    assert!(
        seat.detail.contains("billed by subscription"),
        "the billed-by-seat statement must survive the added clause: {}",
        seat.detail
    );
    assert!(
        seat.detail
            .contains("withholds the API-equivalent value for traffic that uses"),
        "an incomplete basis must scope the withholding to unrated-dimension traffic, \
         never claim the value is unconditionally absent: {}",
        seat.detail
    );
    assert!(
        !seat.detail.contains("NO API-equivalent value"),
        "an incomplete basis must not overclaim unconditional absence (base-only traffic \
         still values): {}",
        seat.detail
    );
    assert!(
        seat.detail.contains("the baked catalog (base rates only)"),
        "the partial basis must name its layer: {}",
        seat.detail
    );
    for dimension in ["cache read", "cache write 5m", "cache write 1h"] {
        assert!(
            seat.detail.contains(dimension),
            "the missing dimension `{dimension}` must be named: {}",
            seat.detail
        );
    }
    // The catalog DID price input and output, so neither is listed as missing.
    assert!(
        !seat.detail.contains("(input,"),
        "a dimension the catalog priced must not read as missing: {}",
        seat.detail
    );
}

/// A subscription model on a selector NEITHER layer prices has no basis at all
/// -- a distinct state from a partial one, since naming missing dimensions would
/// imply the others resolved.
#[test]
fn a_subscription_row_with_no_resolvable_rates_reports_no_basis() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "seatprov".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.models.insert(
        "seat".to_string(),
        ModelEntry::new("seatprov", "no-such-upstream-in-the-catalog"),
    );

    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_pricing(&context);
    let seat = find(&findings, "pricing", "seat");

    assert!(
        seat.detail.contains("billed by subscription"),
        "the billed-by-seat statement must survive the added clause: {}",
        seat.detail
    );
    assert!(
        seat.detail
            .contains("neither [registry] nor the catalog resolves rates"),
        "an unresolvable selector must say nothing prices it: {}",
        seat.detail
    );
    assert!(
        !seat.detail.contains("cache read"),
        "with no basis at all, naming dimensions would imply a partial one: {}",
        seat.detail
    );
}

/// The equivalence clause is SUBSCRIPTION-only: a priced row's own detail is
/// unchanged by this feature, since the complete-or-absent rule the clause
/// explains applies to the equivalent channel alone.
#[test]
fn non_subscription_pricing_rows_carry_no_equivalence_clause() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "paid".to_string(),
        ProviderEntry::anthropic_api("env://PAID_KEY"),
    );
    cfg.providers.insert(
        "vendor".to_string(),
        ProviderEntry::openai_compat("https://example.invalid", "env://VENDOR_KEY"),
    );
    cfg.models.insert(
        "filled".to_string(),
        ModelEntry::new("paid", "claude-sonnet-4-6"),
    );
    cfg.models.insert(
        "nothing".to_string(),
        ModelEntry::new("vendor", "some-unpriced-vendor-model"),
    );

    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_pricing(&context);

    let filled = find(&findings, "pricing", "filled");
    assert!(
        !filled.detail.contains("API-equivalent"),
        "a priced row has no equivalence basis to report: {}",
        filled.detail
    );

    let nothing = find(&findings, "pricing", "nothing");
    assert!(
        !nothing.detail.contains("API-equivalent"),
        "an unpriced API-key row has no equivalence basis to report: {}",
        nothing.detail
    );
}

/// A `[registry]` row whose every `*_per_mtok` field is optional-and-omitted is
/// a legitimate config that deliberately charges nothing. It wins whole, so it
/// is NOT the unpriced state -- but rendering it as "input unset / output unset"
/// reads as a lookup that came up empty. The row is an operator decision, so the
/// detail says the row prices nothing.
#[test]
fn a_registry_row_setting_no_rate_says_it_prices_nothing() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "billed".to_string(),
        ProviderEntry::anthropic_api("env://KEY"),
    );
    cfg.models.insert(
        "free".to_string(),
        ModelEntry::new("billed", "priced-at-nothing"),
    );
    cfg.registry.insert(
        "priced-at-nothing".to_string(),
        routectl_router::RegistryEntry {
            pricing: Some(routectl_router::PricingConfig::default()),
            provider: None,
        },
    );

    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_pricing(&context);

    let free = find(&findings, "pricing", "free");
    assert!(
        free.detail.contains("prices nothing"),
        "an empty [registry] row must read as a deliberate zero-price row: {}",
        free.detail
    );
    assert!(
        !free.detail.contains("unset"),
        "the both-unset wording must not survive: {}",
        free.detail
    );
    assert!(
        !free.detail.contains("$0"),
        "an unset dimension must never render as free: {}",
        free.detail
    );
}

/// The overlay is the rate CORRECTION channel: it disables entries and
/// supersedes baked rates. When it could not be LOADED, a baked figure may be
/// exactly the one the effective pricing does not use, so the section must
/// report nothing resolved rather than that figure. The priced context below is
/// the POSITIVE CONTROL, proving this very selector DOES render the baked
/// figure when the overlay is available -- so its absence is the degradation
/// firing, not a missing catalog cell.
#[test]
fn an_unloadable_overlay_reports_pricing_unavailable_never_the_baked_figure() {
    // Arrange: one model the baked catalog prices at $3.00 / $15.00 -- exactly
    // the kind of selector an overlay cell would disable or reprice.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "paid".to_string(),
        ProviderEntry::anthropic_api("env://PAID_KEY"),
    );
    cfg.models.insert(
        "filled".to_string(),
        ModelEntry::new("paid", "claude-sonnet-4-6"),
    );
    let priced = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    // Positive control: with the overlay available, the baked figure renders.
    let control = section_pricing(&priced);
    assert!(
        control
            .iter()
            .any(|f| f.detail.contains("$3.00") && f.detail.contains("$15.00")),
        "test premise: an available overlay must render the baked figure: {control:?}"
    );

    // Act: the same context with the overlay load having failed.
    let degraded = DoctorContext {
        pricing: None,
        ..priced
    };
    let findings = section_pricing(&degraded);

    // Assert: one honest unavailable line, and NOT the baked figure.
    assert_eq!(
        findings.len(),
        1,
        "one degradation line, not rows: {findings:?}"
    );
    let only = &findings[0];
    assert_eq!(only.status, Status::Warn, "{only:?}");
    assert!(
        only.detail.contains("cost pricing unavailable"),
        "the line must name the degradation: {}",
        only.detail
    );
    assert!(
        only.remediation.is_some(),
        "an unavailable section must name its fix: {only:?}"
    );
    for f in &findings {
        assert!(
            !f.detail.contains("$3.00") && !f.detail.contains("$15.00"),
            "a superseded baked figure must never render: {}",
            f.detail
        );
    }

    // Degradation, not failure: the exit code stays the version section's call.
    assert_eq!(overall_exit(&findings), 0);
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
        Some(&current_version_stamp()),
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

/// The capability matrix panel: lane-by-capability grid assembled from the
/// three signal layers through the shared display resolver.
mod matrix_panel {
    use super::*;

    use std::time::{Duration, Instant};

    use routectl_core::capability::{
        EvidenceSource, FailurePhase, SignalTier, Verdict, WELL_KNOWN_CAPABILITY_KEYS,
    };
    use routectl_router::{
        CapabilityMatrixPanel, LearnedRegistryEntry, MatrixAvailability, MatrixCell,
    };

    /// A config with one provider `p` and two routed model lanes.
    fn matrix_config() -> Config {
        toml::from_str(
            "version = 3\n\
             [providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             [models.laneA]\n\
             provider = \"p\"\n\
             upstream = \"m-a\"\n\
             [models.laneB]\n\
             provider = \"p\"\n\
             upstream = \"m-b\"\n",
        )
        .expect("matrix config parses")
    }

    fn entry(
        state_key: &str,
        feature: &str,
        verdict: Verdict,
        source: EvidenceSource,
        last_seen: Instant,
    ) -> LearnedRegistryEntry {
        LearnedRegistryEntry {
            state_key: state_key.to_string(),
            feature_key: feature.to_string(),
            verdict,
            signal_tier: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: last_seen,
            last_seen,
            expires_at: last_seen,
            phase: FailurePhase::F1,
            source,
        }
    }

    fn matrix_ctx(source: CapabilityMatrixSource, priors: Vec<PriorCell>) -> DoctorContext {
        let base = ctx(
            matrix_config(),
            Some(&current_version_stamp()),
            Vec::new(),
            Vec::new(),
        );
        DoctorContext {
            capability_matrix: source,
            capability: CapabilityInputs {
                config: Some(CapabilityConfig {
                    legacy_keys: Vec::new(),
                    priors,
                }),
                panel_unavailable: None,
            },
            ..base
        }
    }

    fn find_cell<'a>(panel: &'a CapabilityMatrixPanel, lane: &str, cap: &str) -> &'a MatrixCell {
        let li = panel
            .lanes
            .iter()
            .position(|l| l.lane == lane)
            .unwrap_or_else(|| panic!("lane {lane} missing"));
        let ci = panel
            .columns
            .iter()
            .position(|c| c == cap)
            .unwrap_or_else(|| panic!("column {cap} missing"));
        &panel.lanes[li].cells[ci]
    }

    #[test]
    fn seeded_registry_renders_mixed_cells_across_lanes_and_columns() {
        let now = Instant::now();
        let entries = vec![
            entry(
                "laneA",
                "web_search",
                Verdict::VerifiedWorking,
                EvidenceSource::Live,
                now,
            ),
            entry(
                "laneA",
                "thinking",
                Verdict::LearnedBroken(FailurePhase::F1),
                EvidenceSource::Probe,
                now,
            ),
            entry(
                "laneB",
                "custom_tool",
                Verdict::VerifiedWorking,
                EvidenceSource::Live,
                now,
            ),
        ];
        let priors = vec![PriorCell {
            nickname: "laneA".to_string(),
            verified_at: "2026-07-10".to_string(),
            capabilities: vec![("structured_output".to_string(), false)],
        }];
        let ctx = matrix_ctx(
            CapabilityMatrixSource::Available {
                entries,
                now,
                now_ms: 0,
            },
            priors,
        );

        let panel = build_capability_matrix_panel(&ctx);
        assert_eq!(panel.availability, MatrixAvailability::Available);
        // The well-known columns lead; the observed non-well-known key
        // becomes an other column.
        let leading: Vec<&str> = panel.columns[..WELL_KNOWN_CAPABILITY_KEYS.len()]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(leading, WELL_KNOWN_CAPABILITY_KEYS);
        assert!(panel.columns.iter().any(|c| c == "custom_tool"));

        // Verified live cell.
        let verified = find_cell(&panel, "laneA", "web_search");
        assert_eq!(verified.verdict, "verified");
        assert_eq!(verified.source, Some("live"));
        assert_eq!(verified.supported, Some(true));
        assert!(verified.age_ms.is_some());

        // Learned-broken probe cell.
        let broken = find_cell(&panel, "laneA", "thinking");
        assert_eq!(broken.verdict, "broken");
        assert_eq!(broken.source, Some("probe"));
        assert_eq!(broken.supported, Some(false));

        // Prior cell (assumed unsupported).
        let prior = find_cell(&panel, "laneA", "structured_output");
        assert_eq!(prior.verdict, "assumed");
        assert_eq!(prior.source, Some("prior"));
        assert_eq!(prior.supported, Some(false));
        assert_eq!(prior.age_ms, None);

        // Other-column cell on laneB.
        let other = find_cell(&panel, "laneB", "custom_tool");
        assert_eq!(other.verdict, "verified");
        assert_eq!(other.source, Some("live"));

        // A column with no signal for a lane resolves unknown.
        let unknown = find_cell(&panel, "laneB", "web_search");
        assert_eq!(unknown.verdict, "unknown");
        assert_eq!(unknown.source, None);
    }

    #[test]
    fn other_columns_cap_at_ten_with_overflow_count() {
        let now = Instant::now();
        // Twelve distinct non-well-known keys observed on one lane.
        let entries: Vec<LearnedRegistryEntry> = (0..12)
            .map(|i| {
                entry(
                    "laneA",
                    &format!("other_{i:02}"),
                    Verdict::VerifiedWorking,
                    EvidenceSource::Live,
                    now,
                )
            })
            .collect();
        let ctx = matrix_ctx(
            CapabilityMatrixSource::Available {
                entries,
                now,
                now_ms: 0,
            },
            Vec::new(),
        );

        let panel = build_capability_matrix_panel(&ctx);
        // 6 well-known + 10 rendered other columns.
        assert_eq!(panel.columns.len(), 16);
        assert_eq!(panel.other_overflow, 2);
    }

    #[test]
    fn availability_empty_and_unavailable_render_distinctly() {
        let empty =
            build_capability_matrix_panel(&matrix_ctx(CapabilityMatrixSource::Empty, Vec::new()));
        assert_eq!(empty.availability, MatrixAvailability::Empty);

        let unavailable = build_capability_matrix_panel(&matrix_ctx(
            CapabilityMatrixSource::Unavailable("revision_mismatch"),
            Vec::new(),
        ));
        assert_eq!(
            unavailable.availability,
            MatrixAvailability::Unavailable {
                code: "revision_mismatch"
            }
        );
        // Config lanes still render even when the learned source is absent.
        assert!(!unavailable.lanes.is_empty());
    }

    #[test]
    // `Duration::from_days` is unstable in this crate, so the day span is
    // built from seconds; the suboptimal-units lint's suggestion does not
    // compile here.
    #[allow(clippy::duration_suboptimal_units)]
    fn stale_flags_fire_for_verified_and_prior_cells() {
        // now is 30 days past the seeded cell's last_seen; the hint is 14
        // days, so the verified cell is stale.
        let base = Instant::now();
        let now = base + Duration::from_secs(30 * 86_400);
        let entries = vec![entry(
            "laneA",
            "web_search",
            Verdict::VerifiedWorking,
            EvidenceSource::Live,
            base,
        )];
        // A prior stamped in 2000 is far past the hint against the fixed
        // 2026-07-11 "today".
        let priors = vec![PriorCell {
            nickname: "laneA".to_string(),
            verified_at: "2000-01-01".to_string(),
            capabilities: vec![("structured_output".to_string(), true)],
        }];
        let ctx = matrix_ctx(
            CapabilityMatrixSource::Available {
                entries,
                now,
                now_ms: 0,
            },
            priors,
        );

        let panel = build_capability_matrix_panel(&ctx);
        assert!(
            find_cell(&panel, "laneA", "web_search").stale,
            "a verified cell older than the hint is stale"
        );
        assert!(
            find_cell(&panel, "laneA", "structured_output").stale,
            "a prior stamp past the hint is stale"
        );
    }

    #[test]
    fn learned_lane_without_config_entry_renders_unrouted() {
        let now = Instant::now();
        let entries = vec![entry(
            "ghost",
            "web_search",
            Verdict::VerifiedWorking,
            EvidenceSource::Live,
            now,
        )];
        let ctx = matrix_ctx(
            CapabilityMatrixSource::Available {
                entries,
                now,
                now_ms: 0,
            },
            Vec::new(),
        );

        let panel = build_capability_matrix_panel(&ctx);
        let ghost = panel
            .lanes
            .iter()
            .find(|l| l.lane == "ghost")
            .expect("orphan ledger lane rendered");
        assert!(!ghost.routed, "a lane with no config entry is unrouted");
    }
}

/// Cross-surface: a real seeded usage ledger, gathered through the read-only
/// matrix path, resolves against the parsed config's overrides and priors and
/// renders coherently on BOTH the human battery and the `--json` panel. Where
/// the sibling `matrix_panel` mod injects a synthetic source and asserts
/// struct fields, this pins the whole seam -- seed -> gather -> build ->
/// render -- across both output surfaces at once.
mod seeded_matrix_surfaces {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    use routectl_router::CATALOG_VERSION;
    use routectl_usage::{CapabilityEvent, insert_capability_event, open};
    use serde_json::Value;
    use tempfile::TempDir;

    /// Current epoch milliseconds, so seeded rows sit just behind the reader's
    /// pinned `now` and their ages stay small (never stale-flagged).
    fn now_ms() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis(),
        )
        .expect("epoch ms fits i64")
    }

    /// A config with one openai-compat provider, two routed lanes, and a
    /// provider-scoped override, its usage DB pointed at `db`. `literal:` is
    /// accepted by the parser (rejected only at resolve time, which this
    /// read-only path never reaches).
    fn config_at(db: &Path) -> Config {
        let mut config: Config = toml::from_str(
            "version = 3\n\
             [providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             [models.laneA]\n\
             provider = \"p\"\n\
             upstream = \"m-a\"\n\
             [models.laneB]\n\
             provider = \"p\"\n\
             upstream = \"m-b\"\n\
             [capability.overrides.p]\n\
             force_supported = [\"prompt_caching\"]\n",
        )
        .expect("seeded matrix config parses");
        config.usage.db_path = db.to_path_buf();
        config
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        ts: i64,
        lane: &str,
        cap: &str,
        verdict: &str,
        phase: &str,
        source: &str,
        tier: &str,
        evidence: Option<&str>,
    ) -> CapabilityEvent {
        CapabilityEvent {
            ts,
            lane_key: lane.to_string(),
            capability: cap.to_string(),
            verdict: verdict.to_string(),
            phase: phase.to_string(),
            source: source.to_string(),
            tier: tier.to_string(),
            evidence_class: evidence.map(str::to_string),
            upstream_token: None,
            catalog_version: i64::from(CATALOG_VERSION),
            overlay_revision: 0,
        }
    }

    /// The `[lane][cap]` cell of the serialized matrix panel, resolved through
    /// the panel's own `columns` ordering.
    fn json_cell<'a>(panel: &'a Value, lane: &str, cap: &str) -> &'a Value {
        let columns = panel["columns"].as_array().expect("columns array");
        let ci = columns
            .iter()
            .position(|c| c == cap)
            .unwrap_or_else(|| panic!("column {cap} missing"));
        let lanes = panel["lanes"].as_array().expect("lanes array");
        let lane_obj = lanes
            .iter()
            .find(|l| l["lane"] == lane)
            .unwrap_or_else(|| panic!("lane {lane} missing"));
        &lane_obj["cells"][ci]
    }

    #[test]
    fn seeded_ledger_matrix_renders_mixed_verdicts_in_human_and_json() {
        // Arrange: a matched tombstone plus three post-boundary events at a
        // recent instant -- a verified live positive, a probe negative, and a
        // live negative on `prompt_caching` that the override will overrule.
        let tmp = TempDir::new().expect("tempdir");
        let ledger = tmp.path().join("usage.db");
        let db = open(&ledger).expect("open ledger");
        let ts = now_ms();
        let cat = i64::from(CATALOG_VERSION);
        let insert = |e: &CapabilityEvent| {
            insert_capability_event(db.conn(), e).expect("insert capability event");
        };
        insert(&CapabilityEvent::tombstone(ts, cat, 0));
        insert(&event(
            ts,
            "laneA",
            "web_search",
            "verified",
            "f3",
            "live",
            "self-identifying",
            Some("schema_parse"),
        ));
        insert(&event(
            ts,
            "laneA",
            "computer_use",
            "broken",
            "f1",
            "probe",
            "self-identifying",
            None,
        ));
        insert(&event(
            ts,
            "laneA",
            "prompt_caching",
            "broken",
            "f1",
            "live",
            "self-identifying",
            None,
        ));
        drop(db);

        let config = config_at(&ledger);

        // Act 1: the real read-only gather replays the seeded ledger.
        let source = gather_capability_matrix(&config, false, 0);
        assert!(
            matches!(source, CapabilityMatrixSource::Available { .. }),
            "a matched tombstone with post-boundary rows must be Available"
        );

        // A fresh catalog prior for laneB, so the fifth verdict (assumed via
        // the prior layer) is present alongside the learned/override cells.
        let today = Local::now().format("%Y-%m-%d").to_string();
        let context = DoctorContext {
            capability_matrix: source,
            capability: CapabilityInputs {
                config: Some(CapabilityConfig {
                    legacy_keys: Vec::new(),
                    priors: vec![PriorCell {
                        nickname: "laneB".to_string(),
                        verified_at: today,
                        capabilities: vec![("structured_output".to_string(), false)],
                    }],
                }),
                panel_unavailable: None,
            },
            ..ctx(
                config,
                Some(&current_version_stamp()),
                Vec::new(),
                Vec::new(),
            )
        };

        // Act 2: build the report and both render surfaces.
        let report = build_report(&context);
        let human = render_human(&report).join("\n");
        let json = serde_json::to_value(&report).expect("report serializes");

        // Assert (human): the replayed state line plus the compact
        // `verdict[source]` tokens for every learned / override / prior cell.
        assert!(
            human.contains("capability matrix:") && human.contains("learned registry replayed"),
            "human render must carry the replayed matrix state line: {human}"
        );
        for token in [
            "verified[live]",
            "broken[probe]",
            "forced_supported[override]",
            "assumed[prior]",
        ] {
            assert!(
                human.contains(token),
                "human matrix must render the {token} cell: {human}"
            );
        }

        // Assert (json): the panel is Available and every cell carries its
        // verdict, source, polarity, and age -- an override cell wins over the
        // seeded negative, and a no-signal cell resolves to a sourceless
        // unknown.
        let panel = &json["panels"]["capability_matrix"];
        assert_eq!(panel["availability"]["state"], Value::from("available"));

        let verified = json_cell(panel, "laneA", "web_search");
        assert_eq!(verified["verdict"], Value::from("verified"));
        assert_eq!(verified["source"], Value::from("live"));
        assert_eq!(verified["supported"], Value::from(true));
        assert!(
            verified["age_ms"].is_i64(),
            "a live cell carries a concrete age: {verified}"
        );
        assert_eq!(verified["stale"], Value::from(false));

        let broken = json_cell(panel, "laneA", "computer_use");
        assert_eq!(broken["verdict"], Value::from("broken"));
        assert_eq!(broken["source"], Value::from("probe"));
        assert_eq!(broken["supported"], Value::from(false));

        // Override-won: the operator override overrules the seeded live
        // negative for the same cell.
        let overridden = json_cell(panel, "laneA", "prompt_caching");
        assert_eq!(overridden["verdict"], Value::from("forced_supported"));
        assert_eq!(overridden["source"], Value::from("override"));
        assert!(
            overridden["age_ms"].is_null(),
            "an override cell carries no learned age: {overridden}"
        );

        let prior = json_cell(panel, "laneB", "structured_output");
        assert_eq!(prior["verdict"], Value::from("assumed"));
        assert_eq!(prior["source"], Value::from("prior"));
        assert_eq!(prior["supported"], Value::from(false));
        assert!(
            prior["age_ms"].is_null(),
            "a prior cell has no age: {prior}"
        );

        let unknown = json_cell(panel, "laneB", "web_search");
        assert_eq!(unknown["verdict"], Value::from("unknown"));
        assert!(
            unknown["source"].is_null() && unknown["supported"].is_null(),
            "a no-signal cell is a sourceless unknown: {unknown}"
        );
    }

    #[test]
    fn empty_and_unavailable_matrix_render_distinctly_in_human_and_json() {
        // A readable-but-empty source and an unreadable one must never render
        // the same: the empty state is an honest "no learned rows", the
        // unavailable state names its path-free class code -- on BOTH surfaces.
        let ledger = Path::new("/nonexistent/usage.db");

        let empty = build_report(&DoctorContext {
            capability_matrix: CapabilityMatrixSource::Empty,
            ..ctx(
                config_at(ledger),
                Some(&current_version_stamp()),
                Vec::new(),
                Vec::new(),
            )
        });
        let unavailable = build_report(&DoctorContext {
            capability_matrix: CapabilityMatrixSource::Unavailable("revision_mismatch"),
            ..ctx(
                config_at(ledger),
                Some(&current_version_stamp()),
                Vec::new(),
                Vec::new(),
            )
        });

        let empty_human = render_human(&empty).join("\n");
        let unavailable_human = render_human(&unavailable).join("\n");
        assert!(
            empty_human.contains("learned registry empty (no learned rows)"),
            "empty state line missing: {empty_human}"
        );
        assert!(
            unavailable_human.contains("learned registry unavailable (revision_mismatch)"),
            "unavailable state line missing its code: {unavailable_human}"
        );
        assert!(
            !empty_human.contains("unavailable"),
            "an honest empty must never claim to be unavailable: {empty_human}"
        );

        let empty_avail = serde_json::to_value(&empty).expect("serialize")["panels"]
            ["capability_matrix"]["availability"]
            .clone();
        let unavailable_avail = serde_json::to_value(&unavailable).expect("serialize")["panels"]
            ["capability_matrix"]["availability"]
            .clone();
        assert_eq!(empty_avail["state"], Value::from("empty"));
        assert!(
            empty_avail.get("code").is_none(),
            "an empty source carries no class code: {empty_avail}"
        );
        assert_eq!(unavailable_avail["state"], Value::from("unavailable"));
        assert_eq!(unavailable_avail["code"], Value::from("revision_mismatch"));
    }
}

/// The knobs section names each configured model's `max_output_tokens` SOURCE.
/// Naming the wrong one sends an operator to the wrong knob: a row reading
/// "from [models.X]" for a catalog-filled model would have them hunt a config
/// value that is not there, and the reverse would have them believe the catalog
/// is about to raise a ceiling they pinned themselves.
#[test]
fn knobs_section_names_each_models_output_ceiling_source_by_nickname() {
    // Arrange: three models, one per state.
    //   `pinned`  -- an operator max_output_tokens -> Config.
    //   `filled`  -- a Claude upstream the baked catalog confirms, config
    //                silent -> Catalog.
    //   `neither` -- an openai-compat upstream whose only matching cell is
    //                output-ambiguous -> Default.
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://ANTHROPIC_KEY"),
    );
    cfg.providers.insert(
        "vendor".to_string(),
        ProviderEntry::openai_compat("https://example.invalid", "env://VENDOR_KEY"),
    );
    cfg.models.insert(
        "pinned".to_string(),
        ModelEntry::new("anthropic", "claude-opus-4-6").with_max_output_tokens(32_000),
    );
    cfg.models.insert(
        "filled".to_string(),
        ModelEntry::new("anthropic", "claude-opus-4-6"),
    );
    cfg.models.insert(
        "neither".to_string(),
        ModelEntry::new("vendor", "deepseek-v3"),
    );

    // Act
    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_knobs(&context);

    // Assert: one row per configured model, each naming its own source.
    assert_eq!(
        findings.len(),
        3,
        "one row per configured model: {findings:?}"
    );

    let pinned = find(&findings, "knobs", "pinned");
    assert!(
        pinned.detail.contains("32000") && pinned.detail.contains("from [models.pinned]"),
        "an operator-pinned row must name its own value and source: {}",
        pinned.detail
    );

    let filled = find(&findings, "knobs", "filled");
    assert!(
        filled.detail.contains("filled from the catalog"),
        "a catalog-filled row must name the catalog: {}",
        filled.detail
    );
    assert!(
        !filled.detail.contains("32000"),
        "the catalog row must report the CATALOG figure, not the sibling \
         model's pinned one: {}",
        filled.detail
    );

    let neither = find(&findings, "knobs", "neither");
    assert!(
        neither.detail.contains("catalog confirms no ceiling"),
        "a row neither layer supplies must say so: {}",
        neither.detail
    );

    // Purely informational: no row here can move the exit code.
    for f in &findings {
        assert_eq!(f.status, Status::Pass, "{f:?}");
        assert!(f.remediation.is_none(), "{f:?}");
    }
}

/// The doctor's catalog figure and the factory's fill must be the SAME number.
/// They read one accessor, and this pins that they still agree: a doctor that
/// named a ceiling the router does not apply is worse than no diagnostic, since
/// an operator would tune against a figure that never reaches the wire.
///
/// A POOL-BACKED model is asserted alongside the plain one: `[models.X]
/// provider` resolves against providers and pools in ONE namespace, so a
/// kind lookup that consulted only `[providers]` would resolve an empty kind,
/// hit no catalog cell, and report "no ceiling" for a model the factory fills
/// at the very figure the plain row names.
#[test]
fn the_knobs_catalog_figure_matches_what_the_factory_fills() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://ANTHROPIC_KEY"),
    );
    cfg.providers.insert(
        "anthropic-seat".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    cfg.pools.insert(
        "anthropic-pool".to_string(),
        PoolEntry::new(vec!["anthropic-seat".to_string()]),
    );
    cfg.models.insert(
        "filled".to_string(),
        ModelEntry::new("anthropic", "claude-opus-4-6"),
    );
    cfg.models.insert(
        "pooled".to_string(),
        ModelEntry::new("anthropic-pool", "claude-opus-4-6"),
    );

    // The router-side truth: the very accessor the fill is gated on, resolved
    // for the same selector.
    let confirmed = routectl_router::resolve_effective_row(
        "anthropic-api",
        "claude-opus-4-6",
        None,
        &cfg.cache_pricing,
        &CatalogOverlay::default(),
    )
    .output_ceiling_tokens()
    .expect("test premise: the baked table confirms a ceiling for this selector");

    let context = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());
    let findings = section_knobs(&context);
    let filled = find(&findings, "knobs", "filled").detail.clone();
    let pooled = find(&findings, "knobs", "pooled").detail.clone();

    assert!(
        filled.contains(&confirmed.to_string()),
        "the doctor row must name the ceiling the fill would apply ({confirmed}): {filled}"
    );
    assert!(
        pooled.contains("filled from the catalog") && pooled.contains(&confirmed.to_string()),
        "a pool-backed model must resolve its kind off a member and name the \
         same catalog ceiling ({confirmed}): {pooled}"
    );
    assert!(
        !pooled.contains("catalog confirms no ceiling"),
        "a pool name is not a provider kind, but the pool must still resolve \
         one: {pooled}"
    );
}

/// The overlay both CORRECTS and DISABLES catalog ceilings, so when it could
/// not be LOADED a baked figure may be exactly the one the router does not fill
/// from. The available-overlay context below is the POSITIVE CONTROL, proving
/// this selector DOES render a figure when the overlay is readable -- so its
/// absence is the degradation firing, not a missing catalog cell.
#[test]
fn an_unloadable_overlay_reports_knobs_unavailable_never_the_baked_ceiling() {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://ANTHROPIC_KEY"),
    );
    cfg.models.insert(
        "filled".to_string(),
        ModelEntry::new("anthropic", "claude-opus-4-6"),
    );
    let resolved = ctx(cfg, Some(&current_version_stamp()), Vec::new(), Vec::new());

    // Positive control: with the overlay available, a catalog figure renders.
    let control = section_knobs(&resolved);
    let control_detail = find(&control, "knobs", "filled").detail.clone();
    assert!(
        control_detail.contains("filled from the catalog"),
        "test premise: an available overlay must attribute the fill: {control_detail}"
    );

    // Act: the same context with the overlay load having failed.
    let degraded = DoctorContext {
        knobs: None,
        ..resolved
    };
    let findings = section_knobs(&degraded);

    // Assert: one honest unavailable line, and NOT an attributed ceiling.
    assert_eq!(
        findings.len(),
        1,
        "one degradation line, not rows: {findings:?}"
    );
    let only = &findings[0];
    assert_eq!(only.status, Status::Warn, "{only:?}");
    assert!(
        only.detail.contains("output-ceiling sources unavailable"),
        "the line must name the degradation: {}",
        only.detail
    );
    assert!(
        only.remediation.is_some(),
        "an unavailable section must name its fix: {only:?}"
    );
    assert!(
        !only.detail.contains("filled from the catalog"),
        "a superseded baked ceiling must never be attributed: {}",
        only.detail
    );

    // Degradation, not failure: the exit code stays the version section's call.
    assert_eq!(overall_exit(&findings), 0);
}
