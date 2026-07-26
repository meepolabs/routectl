//! `routectl provider probe [<name>]` -- a read-only reachability report.
//!
//! A probe run mutates NOTHING: config, credentials, catalog overlay, and
//! usage DB are all byte-identical afterward. The shared orchestration
//! (`probe_all`) is the one place the per-credential branch logic lives,
//! reused by the doctor aggregator so both surfaces classify a provider the
//! same way.
//!
//! The credential branch (BINDING, read-only):
//!   - `credential_source = forwarded` -> [`ProbeOutcome::Skipped`]: no
//!     build, no upstream call.
//!   - an `oauth://` ref -> the in-memory-only [`routectl_auth::OAuthStore::probe_local`]
//!     (no network, no refresh). `CompositeStore::get` is NEVER called for
//!     these -- it refreshes near-expiry tokens, which a probe must not do.
//!   - `env://` / `file://` / `literal:` / bedrock -> build the provider and
//!     call its free `probe()`.
//!
//! One shared wall-clock deadline caps the whole fan-out: a probe that
//! overruns it collapses to [`ProbeOutcome::Unreachable`] rather than
//! hanging the command.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use routectl_auth::oauth::known_provider_ids;
use routectl_auth::{LocalProbe, SecretRef, SecretStore};
use routectl_core::ProbeOutcome;
use routectl_router::{Config, Finding, ProviderEntry, Status, build_provider, overall_exit};

use crate::server::CompositeStore;

#[cfg(feature = "bedrock")]
pub mod capture;

/// UNSTABLE report schema version for `--json`. Bumped only when the JSON
/// shape changes in a way a consumer would care about.
const SCHEMA_VERSION: u32 = 1;

/// Small cap on in-flight probes so a config with many providers does not
/// open an unbounded burst of upstream connections at once.
const PROBE_CONCURRENCY: usize = 8;

/// One shared wall-clock budget for the whole fan-out. Set above the
/// per-probe [`routectl_providers`] timeout (10s) so a legitimately slow
/// but responsive upstream is not preempted, while a black-holed endpoint
/// still collapses to a typed outcome instead of hanging the command.
/// Shared with the doctor aggregator so both probe surfaces bound alike.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(20);

/// Probe every provider in `config`, read-only, under one shared deadline.
/// THE shared orchestration: the per-credential branch logic lives here
/// once so `provider probe` and the doctor aggregator agree on every
/// classification. Results are sorted by provider name so the output (and
/// any downstream diff) is deterministic regardless of completion order.
pub(crate) async fn probe_all(
    config: &Config,
    store: &CompositeStore,
    deadline: Duration,
) -> Vec<(String, ProbeOutcome)> {
    let secrets: Arc<dyn SecretStore> = Arc::new(store.clone());
    let deadline_at = tokio::time::Instant::now() + deadline;

    let mut results: Vec<(String, ProbeOutcome)> = stream::iter(config.providers.iter())
        .map(|(name, entry)| {
            let secrets = Arc::clone(&secrets);
            async move {
                let outcome = match tokio::time::timeout_at(
                    deadline_at,
                    probe_entry(name, entry, store, secrets),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => ProbeOutcome::Unreachable("probe deadline exceeded".into()),
                };
                (name.clone(), outcome)
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;

    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Classify one provider by its credential shape. The forwarded and
/// oauth branches short-circuit BEFORE any build or network call; only the
/// static-credential and bedrock kinds reach [`build_provider`].
async fn probe_entry(
    name: &str,
    entry: &ProviderEntry,
    store: &CompositeStore,
    secrets: Arc<dyn SecretStore>,
) -> ProbeOutcome {
    if entry.forwarded_base_url().is_some() {
        return ProbeOutcome::Skipped("forwarded".into());
    }

    if let Some(ref_str) = entry.api_key_ref()
        && let Ok(SecretRef::OAuth { provider, .. }) = SecretRef::parse(ref_str)
    {
        return probe_oauth(&provider, store).await;
    }

    match build_provider(name, entry, secrets).await {
        Ok(provider) => provider.probe().await,
        Err(_) => ProbeOutcome::Unreachable("provider could not be built for probing".into()),
    }
}

/// Read-only oauth probe: the in-memory-cache-only `probe_local`, never a
/// resolving `get` (which would refresh a near-expiry token). A dropped
/// oauth arm (no HOME/XDG) yields [`LocalProbe::StoreUnavailable`].
async fn probe_oauth(provider: &str, store: &CompositeStore) -> ProbeOutcome {
    let probe = match store.oauth() {
        Some(oauth) => oauth.probe_local(provider).await,
        None => LocalProbe::StoreUnavailable,
    };
    map_local_probe(probe)
}

/// Map a read-only [`LocalProbe`] to a [`ProbeOutcome`]. The reason string
/// states the symptom only; the operator remediation (which `routectl login`
/// to run) is attached later by [`probe_finding`] so both the `provider
/// probe` render and the doctor aggregator populate it identically.
fn map_local_probe(probe: LocalProbe) -> ProbeOutcome {
    match probe {
        LocalProbe::Present => ProbeOutcome::Reachable,
        LocalProbe::Expired => ProbeOutcome::AuthFailed("token expired".into()),
        LocalProbe::Missing => ProbeOutcome::AuthFailed("not logged in".into()),
        LocalProbe::StoreUnavailable => {
            ProbeOutcome::Unreachable("oauth credential store unavailable".into())
        }
        _ => ProbeOutcome::Unreachable("oauth probe returned an unrecognized state".into()),
    }
}

/// Run `provider probe` against `config_path` and render the report.
/// Read-only and infallible in posture: a load failure degrades to
/// defaults. Returns the process exit code (nonzero iff any provider probe
/// failed). A `<name>` that is not configured is an actionable error.
pub async fn run(config_path: &Path, name: Option<String>, json: bool) -> i32 {
    let config = crate::server::load_effective_config_unvalidated(config_path)
        .map(|loaded| loaded.config)
        .unwrap_or_default();

    let target = match select_target(config, name.as_deref()) {
        Ok(target) => target,
        Err(msg) => {
            eprintln!("error: {msg}");
            return 1;
        }
    };

    let store = match CompositeStore::open_default().await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let results = probe_all(&target, &store, PROBE_DEADLINE).await;

    if json {
        match serde_json::to_string_pretty(&report_json(&results)) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("error: failed to serialize probe report: {e}");
                return 1;
            }
        }
    } else {
        for line in render_human(&target, &results) {
            println!("{line}");
        }
    }

    overall_exit(&to_findings(&results))
}

/// Narrow `config` to the requested provider, or keep it whole when no
/// name was given. An unknown name is an error rather than a silent
/// empty run.
fn select_target(config: Config, name: Option<&str>) -> Result<Config, String> {
    match name {
        None => Ok(config),
        Some(name) => {
            if !config.providers.contains_key(name) {
                return Err(format!(
                    "no provider named `{name}` is configured; run `routectl provider probe` \
                     to probe all configured providers"
                ));
            }
            let mut config = config;
            config.providers.retain(|key, _| key == name);
            Ok(config)
        }
    }
}

/// Map a batch of probe results to findings via the shared [`probe_finding`]
/// seam. Used for the process exit code (`overall_exit`), which reads only
/// each finding's status -- the login remediation is irrelevant here, so no
/// login id is resolved.
fn to_findings(results: &[(String, ProbeOutcome)]) -> Vec<Finding> {
    results
        .iter()
        .map(|(name, outcome)| probe_finding(name, outcome, None))
        .collect()
}

/// The canonical `routectl login` argument for `name`, or `None` when the
/// provider is not oauth-backed (a static-credential scheme names no login
/// command) or names an oauth id the login parser would reject. Resolving
/// against [`known_provider_ids`] guarantees the suggested command round-trips
/// through login's clap value-parser instead of naming the config key, which
/// may differ from the oauth login id.
pub(crate) fn login_id_for(config: &Config, name: &str) -> Option<&'static str> {
    let entry = config.providers.get(name)?;
    let SecretRef::OAuth { provider, .. } = SecretRef::parse(entry.api_key_ref()?).ok()? else {
        return None;
    };
    known_provider_ids()
        .iter()
        .copied()
        .find(|id| *id == provider)
}

/// THE shared `ProbeOutcome` -> [`Finding`] classification. Both the
/// `provider probe` render and the doctor aggregator call this so the two
/// surfaces never drift on the status, detail, or remediation for a given
/// outcome. Every non-clean outcome (Fail or Warn) carries a populated
/// `remediation`; the remediation text lives HERE, not baked into the
/// outcome's reason string, so it is attached once for both callers.
/// `login_id` is the oauth login argument to suggest for an `AuthFailed`
/// outcome (see [`login_id_for`]); `None` suppresses the login suggestion for
/// a non-oauth credential.
pub(crate) fn probe_finding(name: &str, outcome: &ProbeOutcome, login_id: Option<&str>) -> Finding {
    let (detail, remediation) = describe(outcome, login_id);
    Finding {
        section: "probe",
        name: name.to_string(),
        status: outcome_status(outcome),
        detail,
        remediation,
    }
}

/// Exit-code mapping: `AuthFailed` / `Unreachable` are failures; `Reachable`
/// and `Skipped` are clean; `IndeterminateHttp`, `UnsupportedFreeProbe`
/// (and any future variant) are non-failing warnings.
pub(crate) const fn outcome_status(outcome: &ProbeOutcome) -> Status {
    match outcome {
        ProbeOutcome::Reachable | ProbeOutcome::Skipped(_) => Status::Pass,
        ProbeOutcome::AuthFailed(_) | ProbeOutcome::Unreachable(_) => Status::Fail,
        ProbeOutcome::IndeterminateHttp { .. } | ProbeOutcome::UnsupportedFreeProbe => Status::Warn,
        _ => Status::Warn,
    }
}

/// Operator-facing detail line + remediation for a probe outcome. The
/// remediation is `Some` for every Fail/Warn outcome and `None` for clean
/// ones; it never carries a token, path, or env value. `login_id` is the
/// oauth login argument to name for an `AuthFailed` outcome; `None` (a
/// non-oauth credential) points at the configured api key instead.
fn describe(outcome: &ProbeOutcome, login_id: Option<&str>) -> (String, Option<String>) {
    match outcome {
        ProbeOutcome::Reachable => ("reachable".to_string(), None),
        ProbeOutcome::Skipped(reason) => (format!("skipped ({reason})"), None),
        ProbeOutcome::AuthFailed(reason) => {
            (reason.clone(), Some(auth_failed_remediation(login_id)))
        }
        ProbeOutcome::Unreachable(reason) => (
            reason.clone(),
            Some("verify the provider's base URL and network reachability".to_string()),
        ),
        ProbeOutcome::IndeterminateHttp { status } => (
            format!("endpoint answered HTTP {status}; reachability not confirmed"),
            Some(indeterminate_remediation(*status)),
        ),
        ProbeOutcome::UnsupportedFreeProbe => (
            "no free reachability probe available".to_string(),
            Some(
                "this provider kind has no cheap probe; verify it with a real request".to_string(),
            ),
        ),
        _ => (
            "unrecognized probe outcome".to_string(),
            Some("upgrade routectl to a build that understands this outcome".to_string()),
        ),
    }
}

/// Remediation for an `AuthFailed` outcome. An oauth credential names the
/// exact `routectl login` argument (already resolved to a parser-valid login
/// id); a non-oauth credential points at the configured api key instead of a
/// login command that would not apply.
fn auth_failed_remediation(login_id: Option<&str>) -> String {
    match login_id {
        Some(id) => format!("run `routectl login {id}` to re-authenticate this provider"),
        None => "verify this provider's configured api key".to_string(),
    }
}

/// Status-family remediation for an `IndeterminateHttp` outcome. The probe
/// reached the endpoint but the status does not confirm a healthy provider;
/// the hint points at the most likely cause for that status family.
fn indeterminate_remediation(status: u16) -> String {
    match status {
        404 | 405 | 410 => format!(
            "the probe endpoint returned HTTP {status}; verify the base URL and probe endpoint path"
        ),
        300..=399 => format!(
            "the endpoint redirected (HTTP {status}) and routectl does not follow redirects; \
             verify the base URL points at the API root"
        ),
        429 => "the provider rate-limited the probe (HTTP 429); retry shortly".to_string(),
        500..=599 => format!(
            "the upstream returned HTTP {status} and may be unhealthy; retry, or verify with \
             a real request"
        ),
        _ => format!(
            "the endpoint answered HTTP {status}; verify the base URL and confirm with a \
             real request"
        ),
    }
}

const fn status_label(status: Status) -> &'static str {
    match status {
        Status::Pass => "PASS",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
    }
}

fn render_human(config: &Config, results: &[(String, ProbeOutcome)]) -> Vec<String> {
    if results.is_empty() {
        return vec!["no providers configured".to_string()];
    }
    let mut out = Vec::new();
    for (name, outcome) in results {
        let finding = probe_finding(name, outcome, login_id_for(config, name));
        out.push(format!(
            "{} {}: {}",
            status_label(finding.status),
            finding.name,
            finding.detail
        ));
        if let Some(rem) = &finding.remediation {
            out.push(format!("       fix: {rem}"));
        }
    }
    out
}

fn report_json(results: &[(String, ProbeOutcome)]) -> serde_json::Value {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "providers": results
            .iter()
            .map(|(name, outcome)| serde_json::json!({ "name": name, "outcome": outcome }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use routectl_router::config::CredentialSource;

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn config_with(providers: &[(&str, ProviderEntry)]) -> Config {
        let mut config = Config::default();
        for (name, entry) in providers {
            config.providers.insert((*name).to_string(), entry.clone());
        }
        config
    }

    /// A CompositeStore whose oauth arm reads an isolated (possibly
    /// nonexistent) credentials file. Used for the static-credential and
    /// oauth branches without touching the operator's real store.
    async fn store_at(path: &Path) -> CompositeStore {
        CompositeStore::open_at(path).await.expect("open store")
    }

    /// Seed a credentials.json holding one provider record at 0600 (the
    /// store refuses a world-readable file). `refresh` toggles whether a
    /// refresh token is stored.
    fn seed_credentials(path: &Path, provider: &str, expires_at: u64, refresh: bool) {
        let record = serde_json::json!({
            "access_token": "tok-not-real",
            "refresh_token": if refresh { "rtok-not-real" } else { "" },
            "token_type": "Bearer",
            "expires_at_unix": expires_at,
            "scopes": ["user:inference"],
            "account": { "email": "a@example.com", "account_id": "acct-x" },
            "obtained_at_unix": 0,
        });
        let file = serde_json::json!({
            "schema_version": 1,
            "providers": { provider: record },
        });
        std::fs::write(path, serde_json::to_vec(&file).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    // -----------------------------------------------------------------
    // Forwarded: Skipped, no build, no upstream call.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn forwarded_provider_is_skipped_without_build() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir.path().join("creds.json")).await;
        let entry =
            ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded);
        let config = config_with(&[("fwd", entry)]);

        let results = probe_all(&config, &store, PROBE_DEADLINE).await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].1,
            ProbeOutcome::Skipped("forwarded".to_string()),
            "a forwarded provider must short-circuit to Skipped"
        );
    }

    // -----------------------------------------------------------------
    // oauth: probed read-only; the credentials file is byte-identical
    // after the run (no refresh, no token-endpoint POST).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn oauth_provider_probed_read_only_leaves_credentials_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        // Near-expiry WITH a refresh token: a resolving `get` would try to
        // refresh (and rewrite the file); `probe_local` must not.
        seed_credentials(&creds, "anthropic", now_unix() + 30, true);
        let before = std::fs::read(&creds).unwrap();

        let store = store_at(&creds).await;
        let config = config_with(&[(
            "anthropic",
            ProviderEntry::anthropic_api("oauth://anthropic"),
        )]);

        let results = probe_all(&config, &store, PROBE_DEADLINE).await;

        assert_eq!(results[0].1, ProbeOutcome::Reachable);
        assert_eq!(
            std::fs::read(&creds).unwrap(),
            before,
            "an oauth probe must leave the credentials store byte-identical"
        );
    }

    #[tokio::test]
    async fn oauth_missing_seat_maps_to_auth_failed() {
        let dir = tempfile::tempdir().unwrap();
        // No seed: the store has no record for anthropic -> Missing.
        let store = store_at(&dir.path().join("creds.json")).await;
        let config = config_with(&[(
            "anthropic",
            ProviderEntry::anthropic_api("oauth://anthropic"),
        )]);

        let results = probe_all(&config, &store, PROBE_DEADLINE).await;

        let outcome = &results[0].1;
        match outcome {
            ProbeOutcome::AuthFailed(reason) => {
                // The symptom lives in the reason; the `routectl login`
                // remediation is attached by `probe_finding`, not the reason.
                assert_eq!(reason, "not logged in");
                assert!(!reason.contains("routectl login"), "reason: {reason}");
            }
            other => panic!("expected AuthFailed for a missing seat, got {other:?}"),
        }
        let finding = probe_finding(&results[0].0, outcome, login_id_for(&config, &results[0].0));
        assert_eq!(finding.status, Status::Fail);
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap()
                .contains("routectl login anthropic"),
            "remediation: {:?}",
            finding.remediation
        );
    }

    #[test]
    fn map_local_probe_expired_is_auth_failed_with_login_remediation() {
        let outcome = map_local_probe(LocalProbe::Expired);
        match &outcome {
            ProbeOutcome::AuthFailed(reason) => assert_eq!(reason, "token expired"),
            other => panic!("expected AuthFailed for an expired token, got {other:?}"),
        }
        let finding = probe_finding("anthropic", &outcome, Some("anthropic"));
        assert_eq!(finding.status, Status::Fail);
        assert!(
            finding
                .remediation
                .as_deref()
                .unwrap()
                .contains("routectl login anthropic"),
            "remediation: {:?}",
            finding.remediation
        );
    }

    #[test]
    fn map_local_probe_store_unavailable_is_unreachable_with_remediation() {
        let outcome = map_local_probe(LocalProbe::StoreUnavailable);
        assert!(
            matches!(outcome, ProbeOutcome::Unreachable(_)),
            "expected Unreachable, got {outcome:?}"
        );
        let finding = probe_finding("anthropic", &outcome, None);
        assert_eq!(finding.status, Status::Fail);
        assert!(
            finding.remediation.is_some(),
            "an Unreachable finding must carry a remediation"
        );
    }

    #[test]
    fn every_fail_or_warn_finding_carries_a_remediation() {
        for outcome in [
            ProbeOutcome::AuthFailed("x".into()),
            ProbeOutcome::Unreachable("x".into()),
            ProbeOutcome::IndeterminateHttp { status: 500 },
            ProbeOutcome::UnsupportedFreeProbe,
        ] {
            let finding = probe_finding("p", &outcome, None);
            assert!(
                matches!(finding.status, Status::Fail | Status::Warn),
                "outcome {outcome:?} should be Fail or Warn"
            );
            assert!(
                finding.remediation.is_some(),
                "{outcome:?} must carry a remediation"
            );
        }
    }

    // -----------------------------------------------------------------
    // AuthFailed remediation: the suggested `routectl login` argument
    // must be one the login clap parser accepts (an id from
    // `known_provider_ids`), and a non-oauth credential earns no login
    // suggestion at all.
    // -----------------------------------------------------------------

    #[test]
    fn oauth_authfailed_names_a_parser_valid_login_id_not_the_config_key() {
        // The config key deliberately differs from the oauth login id: the
        // login parser accepts `anthropic`, never the arbitrary key `claude`.
        let config = config_with(&[("claude", ProviderEntry::anthropic_api("oauth://anthropic"))]);

        let login_id = login_id_for(&config, "claude");
        assert_eq!(
            login_id,
            Some("anthropic"),
            "the login suggestion must map the oauth ref to its login id, not the config key"
        );
        assert!(
            routectl_auth::oauth::known_provider_ids().contains(&login_id.unwrap()),
            "the suggested login id must round-trip through the login value-parser"
        );

        let finding = probe_finding(
            "claude",
            &ProbeOutcome::AuthFailed("not logged in".into()),
            login_id,
        );
        let rem = finding
            .remediation
            .expect("AuthFailed carries a remediation");
        assert!(
            rem.contains("routectl login anthropic"),
            "remediation must name the parser-valid login id: {rem}"
        );
        assert!(
            !rem.contains("claude"),
            "remediation must not name the config key the login parser rejects: {rem}"
        );
    }

    #[test]
    fn non_oauth_authfailed_omits_the_login_suggestion() {
        let config = config_with(&[("static", ProviderEntry::anthropic_api("literal:k"))]);

        let login_id = login_id_for(&config, "static");
        assert_eq!(
            login_id, None,
            "a non-oauth credential names no login command"
        );

        let finding = probe_finding(
            "static",
            &ProbeOutcome::AuthFailed("provider rejected the probe credential".into()),
            login_id,
        );
        let rem = finding
            .remediation
            .expect("a non-oauth AuthFailed still carries a remediation");
        assert!(
            !rem.contains("routectl login"),
            "a non-oauth remediation must not suggest `routectl login`: {rem}"
        );
    }

    #[test]
    fn oauth_ref_the_login_parser_rejects_yields_no_login_suggestion() {
        // `made-up` parses as an oauth ref but is not a known login id, so
        // suggesting `routectl login made-up` would be rejected by clap.
        let config = config_with(&[("weird", ProviderEntry::anthropic_api("oauth://made-up"))]);
        assert_eq!(
            login_id_for(&config, "weird"),
            None,
            "an oauth id the login parser would reject yields no login suggestion"
        );
    }

    // -----------------------------------------------------------------
    // IndeterminateHttp: WARN, never a pass and never a hard fail, with
    // status-family remediation. One case per family the classifier emits.
    // -----------------------------------------------------------------

    #[test]
    fn indeterminate_http_is_warn_not_fail_or_pass() {
        for status in [302u16, 404, 429, 500, 503] {
            let outcome = ProbeOutcome::IndeterminateHttp { status };
            assert_eq!(
                outcome_status(&outcome),
                Status::Warn,
                "HTTP {status} must be a warning, not a pass or a hard fail"
            );
        }
    }

    #[test]
    fn indeterminate_http_does_not_fail_the_exit_code() {
        let results = vec![
            (
                "a".to_string(),
                ProbeOutcome::IndeterminateHttp { status: 404 },
            ),
            (
                "b".to_string(),
                ProbeOutcome::IndeterminateHttp { status: 503 },
            ),
        ];
        assert_eq!(
            overall_exit(&to_findings(&results)),
            0,
            "an IndeterminateHttp WARN must not fail the exit code"
        );
    }

    #[test]
    fn indeterminate_http_remediation_matches_status_family() {
        let cases = [
            (404u16, "base URL and probe endpoint path"),
            (302, "does not follow redirects"),
            (429, "rate-limited"),
            (503, "may be unhealthy"),
        ];
        for (status, needle) in cases {
            let finding = probe_finding("p", &ProbeOutcome::IndeterminateHttp { status }, None);
            let rem = finding.remediation.expect("remediation present");
            assert!(
                rem.contains(needle),
                "HTTP {status} remediation {rem:?} should mention {needle:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Fan-out: one result per provider, sorted by name.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn probe_all_returns_one_sorted_result_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir.path().join("creds.json")).await;
        let fwd =
            ProviderEntry::anthropic_api("").with_credential_source(CredentialSource::Forwarded);
        let config = config_with(&[("zeta", fwd.clone()), ("alpha", fwd)]);

        let results = probe_all(&config, &store, PROBE_DEADLINE).await;

        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"], "results must be name-sorted");
    }

    // -----------------------------------------------------------------
    // Bounded deadline: a slow endpoint collapses to a typed outcome
    // within the deadline; proven without a fixed sleep.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn slow_provider_collapses_to_deadline_exceeded_without_hanging() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir.path().join("creds.json")).await;
        let entry =
            ProviderEntry::openai_compat(server.uri(), crate::test_secret::file_ref("probe-key"));
        let config = config_with(&[("slow", entry)]);

        let results = probe_all(&config, &store, Duration::from_millis(150)).await;

        assert_eq!(
            results[0].1,
            ProbeOutcome::Unreachable("probe deadline exceeded".to_string()),
            "a probe past the shared deadline must collapse to a typed Unreachable"
        );
    }

    // -----------------------------------------------------------------
    // Exit-code mapping: stable and order-independent.
    // -----------------------------------------------------------------

    #[test]
    fn exit_zero_for_reachable_skipped_and_warn() {
        let results = vec![
            ("a".to_string(), ProbeOutcome::Reachable),
            ("b".to_string(), ProbeOutcome::Skipped("forwarded".into())),
            ("c".to_string(), ProbeOutcome::UnsupportedFreeProbe),
        ];
        assert_eq!(overall_exit(&to_findings(&results)), 0);
    }

    #[test]
    fn exit_nonzero_for_authfailed_or_unreachable() {
        let auth = vec![("a".to_string(), ProbeOutcome::AuthFailed("x".into()))];
        let unreach = vec![("a".to_string(), ProbeOutcome::Unreachable("x".into()))];
        assert_ne!(overall_exit(&to_findings(&auth)), 0);
        assert_ne!(overall_exit(&to_findings(&unreach)), 0);
    }

    #[test]
    fn exit_is_stable_across_ordering() {
        let forward = vec![
            ("a".to_string(), ProbeOutcome::Reachable),
            ("b".to_string(), ProbeOutcome::AuthFailed("x".into())),
            ("c".to_string(), ProbeOutcome::Skipped("forwarded".into())),
        ];
        let reversed: Vec<_> = forward.iter().cloned().rev().collect();
        assert_eq!(
            overall_exit(&to_findings(&forward)),
            overall_exit(&to_findings(&reversed))
        );
        assert_ne!(overall_exit(&to_findings(&forward)), 0);
    }

    // -----------------------------------------------------------------
    // JSON: carries schema_version + per-provider outcomes.
    // -----------------------------------------------------------------

    #[test]
    fn json_report_carries_schema_version_and_per_provider_outcomes() {
        let results = vec![
            ("a".to_string(), ProbeOutcome::Reachable),
            ("b".to_string(), ProbeOutcome::AuthFailed("nope".into())),
        ];
        let value = report_json(&results);
        let obj = value.as_object().expect("top-level object");

        assert_eq!(obj["schema_version"], serde_json::json!(SCHEMA_VERSION));
        let providers = obj["providers"].as_array().expect("providers array");
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["name"], serde_json::json!("a"));
        assert_eq!(providers[0]["outcome"], serde_json::json!("Reachable"));
        assert_eq!(
            providers[1]["outcome"],
            serde_json::json!({ "AuthFailed": "nope" })
        );
    }

    // -----------------------------------------------------------------
    // Name filtering.
    // -----------------------------------------------------------------

    #[test]
    fn select_target_unknown_name_is_an_error() {
        let config = config_with(&[("alpha", ProviderEntry::anthropic_api("literal:k"))]);
        let err = select_target(config, Some("missing")).expect_err("unknown name must error");
        assert!(err.contains("missing"), "err: {err}");
    }

    #[test]
    fn select_target_narrows_to_one_named_provider() {
        let config = config_with(&[
            ("alpha", ProviderEntry::anthropic_api("literal:k")),
            ("beta", ProviderEntry::anthropic_api("literal:k")),
        ]);
        let narrowed = select_target(config, Some("beta")).expect("known name");
        assert_eq!(narrowed.providers.len(), 1);
        assert!(narrowed.providers.contains_key("beta"));
    }

    #[test]
    fn select_target_none_keeps_every_provider() {
        let config = config_with(&[
            ("alpha", ProviderEntry::anthropic_api("literal:k")),
            ("beta", ProviderEntry::anthropic_api("literal:k")),
        ]);
        let all = select_target(config, None).expect("no filter");
        assert_eq!(all.providers.len(), 2);
    }

    // -----------------------------------------------------------------
    // Full command run: unknown name exits nonzero; a real run mutates
    // nothing on disk.
    // -----------------------------------------------------------------

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

    use routectl_testkit::ScopedEnv;

    const V3_ONE_PROVIDER: &str = "\
version = 3

[providers.alpha]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:k\"
";

    #[tokio::test]
    #[serial_test::serial]
    async fn run_unknown_name_exits_nonzero_without_probing() {
        let tmp = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(&config_path, V3_ONE_PROVIDER).unwrap();

        let before = snapshot_dir(tmp.path());
        let code = run(&config_path, Some("missing".to_string()), false).await;
        let after = snapshot_dir(tmp.path());

        assert_eq!(code, 1, "an unknown provider name must exit nonzero");
        assert_eq!(
            before, after,
            "the unknown-name error path must write nothing"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn full_run_leaves_config_dir_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
        let cfg_dir = tmp.path().join("routectl");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config_path = cfg_dir.join("config.toml");
        std::fs::write(&config_path, V3_ONE_PROVIDER).unwrap();

        let before = snapshot_dir(tmp.path());
        // alpha points at a closed loopback port -> Unreachable -> nonzero,
        // but the run itself mutates nothing.
        let code = run(&config_path, None, false).await;
        let after = snapshot_dir(tmp.path());

        assert_ne!(code, 0, "an unreachable provider must exit nonzero");
        assert_eq!(before, after, "a probe run must not mutate any file");
    }
}
