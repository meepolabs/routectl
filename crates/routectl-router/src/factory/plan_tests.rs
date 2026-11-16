//! The plan is decided from the entry alone: a refused entry reaches no
//! credential store method on any build path, and the client-identity
//! verdict holds only for a vendor's own login on its own lane.

use super::*;
use crate::config::{Auth, Config, ModelEntry, PoolEntry};
use crate::factory::{BuildOptions, build_provider, build_resolved_models, pool_member_refusals};
use async_trait::async_trait;
use routectl_auth::{CredentialProbeFailure, SecretRef, SecretStore};
use routectl_providers::anthropic_api::AuthKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts every store call, defaulted trait methods included.
#[derive(Default)]
struct CountingStore {
    calls: AtomicUsize,
}

impl CountingStore {
    fn touch(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SecretStore for CountingStore {
    async fn get(&self, _: &SecretRef) -> routectl_core::Result<String> {
        self.touch();
        Ok("counted-token".to_string())
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        self.touch();
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        self.touch();
        Ok(())
    }
    async fn probe_credential(&self, _: &SecretRef) -> Result<(), CredentialProbeFailure> {
        self.touch();
        Ok(())
    }
    async fn on_auth_failure(&self, _: &SecretRef) -> routectl_core::Result<()> {
        self.touch();
        Ok(())
    }
    async fn account_id(&self, _: &SecretRef) -> routectl_core::Result<Option<String>> {
        self.touch();
        Ok(Some("counted-account".to_string()))
    }
    async fn list_seats(&self, sr: &SecretRef) -> routectl_core::Result<Vec<SecretRef>> {
        self.touch();
        Ok(vec![sr.clone()])
    }
    async fn peek_session_id(&self, _: &SecretRef) -> Option<String> {
        self.touch();
        None
    }
    async fn peek_cloud_project_id(&self, _: &SecretRef) -> Option<String> {
        self.touch();
        None
    }
    async fn set_cloud_project_id(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        self.touch();
        Ok(())
    }
    async fn clear_cloud_project_id_if_matches(
        &self,
        _: &SecretRef,
        _: &str,
    ) -> routectl_core::Result<bool> {
        self.touch();
        Ok(false)
    }
}

#[cfg(any(feature = "bedrock", feature = "gemini"))]
fn entry_from_toml(text: &str) -> ProviderEntry {
    let cfg: Config = toml::from_str(text).expect("fixture parses");
    cfg.providers.into_values().next().expect("one provider")
}

fn anthropic(r: &str, base_url: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(r)
        .with_auth_kind(AuthKind::OauthBearer)
        .with_base_url(base_url)
}

fn openai_chat(r: &str, base_url: &str) -> ProviderEntry {
    ProviderEntry::openai_compat(base_url, r)
}

#[cfg(feature = "openai-responses")]
fn codex(r: &str, base_url: &str) -> ProviderEntry {
    ProviderEntry::openai_responses(r).with_openai_responses_base_url(base_url)
}

#[cfg(feature = "gemini")]
fn cloud_code(r: &str, base_url: &str) -> ProviderEntry {
    entry_from_toml(&format!(
        "[providers.p]\n\
         format = \"gemini-cloud-code\"\n\
         base_url = {}\n\
         credential = \"{r}\"\n\
         auth = \"bearer\"\n",
        toml::Value::String(base_url.to_string())
    ))
}

#[cfg(feature = "bedrock")]
fn mantle_chat(key_ref: &str) -> ProviderEntry {
    entry_from_toml(&format!(
        "[providers.p]\n\
         format = \"openai-chat\"\n\
         cloud = {{ name = \"aws\", region = \"us-east-1\" }}\n\
         credential = \"{key_ref}\"\n\
         auth = \"bearer\"\n"
    ))
}

#[cfg(feature = "bedrock")]
fn mantle_anthropic(key_ref: &str) -> ProviderEntry {
    entry_from_toml(&format!(
        "[providers.p]\n\
         format = \"anthropic-messages\"\n\
         cloud = {{ name = \"aws\", region = \"us-east-1\" }}\n\
         credential = \"{key_ref}\"\n\
         auth = \"bearer\"\n"
    ))
}

#[cfg(all(feature = "bedrock", feature = "openai-responses"))]
fn mantle_responses(key_ref: &str) -> ProviderEntry {
    entry_from_toml(&format!(
        "[providers.p]\n\
         format = \"openai-responses\"\n\
         cloud = {{ name = \"aws\", region = \"us-east-1\" }}\n\
         credential = \"{key_ref}\"\n\
         auth = \"bearer\"\n"
    ))
}

#[cfg(feature = "bedrock")]
fn native_bedrock(key_ref: &str) -> ProviderEntry {
    entry_from_toml(&format!(
        "[providers.p]\n\
         format = \"bedrock-invoke\"\n\
         cloud = {{ name = \"aws\", region = \"us-east-1\" }}\n\
         credential = \"{key_ref}\"\n\
         auth = \"bearer\"\n"
    ))
}

const VENDOR_BASES: [&str; 4] = [
    "https://api.anthropic.com",
    "https://chatgpt.com/backend-api/codex",
    "https://api.x.ai/v1",
    "https://cloudcode-pa.googleapis.com",
];

/// One managed vendor's own lane and how to put an entry of that lane's
/// shape at any base URL.
struct VendorLane {
    vendor: &'static str,
    host: &'static str,
    own_base: &'static str,
    upstream: &'static str,
    entry: fn(&str, &str) -> ProviderEntry,
    /// The same reference on another format, at the vendor's own host.
    wrong_kind: fn(&str) -> ProviderEntry,
    /// Whether a direct build of the own lane reads the store (a session or
    /// account id). The other arms build a lazy token and read nothing until
    /// the first request, so a zero count on their refused rows is not
    /// evidence of the plan; those rows are pinned by the refusal text.
    reads_at_build: bool,
}

fn vendor_lanes() -> Vec<VendorLane> {
    #[cfg_attr(
        not(any(feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut lanes = vec![
        VendorLane {
            vendor: "anthropic",
            host: "api.anthropic.com",
            own_base: VENDOR_BASES[0],
            upstream: "claude-opus-4-7",
            entry: anthropic,
            wrong_kind: |r| openai_chat(r, "https://api.anthropic.com"),
            reads_at_build: true,
        },
        VendorLane {
            vendor: "xai",
            host: "api.x.ai",
            own_base: VENDOR_BASES[2],
            upstream: "grok-4",
            entry: openai_chat,
            wrong_kind: |r| anthropic(r, "https://api.x.ai"),
            reads_at_build: false,
        },
    ];
    #[cfg(feature = "openai-responses")]
    lanes.push(VendorLane {
        vendor: "codex",
        host: "chatgpt.com",
        own_base: VENDOR_BASES[1],
        upstream: "gpt-5-codex",
        entry: codex,
        wrong_kind: |r| openai_chat(r, "https://chatgpt.com/backend-api/codex"),
        reads_at_build: true,
    });
    #[cfg(feature = "gemini")]
    lanes.push(VendorLane {
        vendor: "antigravity",
        host: "cloudcode-pa.googleapis.com",
        own_base: VENDOR_BASES[3],
        upstream: "gemini-3.1-flash-lite",
        entry: cloud_code,
        wrong_kind: |r| openai_chat(r, "https://cloudcode-pa.googleapis.com"),
        reads_at_build: false,
    });
    lanes
}

/// Every refused shape for one vendor's reference `r`, each named.
fn refused_entries(lane: &VendorLane, r: &str) -> Vec<(String, ProviderEntry)> {
    let mut v: Vec<(String, ProviderEntry)> = VENDOR_BASES
        .iter()
        .filter(|base| **base != lane.own_base)
        .map(|base| (format!("other vendor host {base}"), (lane.entry)(r, base)))
        .collect();
    let host = lane.host;
    for (label, base) in [
        ("loopback", "http://127.0.0.1:18080".to_string()),
        ("userinfo", format!("https://user:pass@{host}")),
        ("explicit port", format!("https://{host}:8443")),
        ("invalid url", "not a url".to_string()),
    ] {
        v.push((label.to_string(), (lane.entry)(r, &base)));
    }
    v.push(("wrong kind".to_string(), (lane.wrong_kind)(r)));
    if lane.vendor == "anthropic" {
        v.push((
            "api-key form".to_string(),
            ProviderEntry::anthropic_api(r)
                .with_auth_kind(AuthKind::ApiKey)
                .with_base_url(lane.own_base),
        ));
    }
    #[cfg(feature = "bedrock")]
    {
        v.push(("mantle carrier".to_string(), mantle_chat(r)));
        v.push(("anthropic mantle carrier".to_string(), mantle_anthropic(r)));
        v.push(("native bedrock credential".to_string(), native_bedrock(r)));
    }
    #[cfg(all(feature = "bedrock", feature = "openai-responses"))]
    v.push(("responses mantle carrier".to_string(), mantle_responses(r)));
    v
}

fn one_member_pool(entry: ProviderEntry, upstream: &str) -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert("member".into(), entry);
    cfg.pools
        .insert("pool".into(), PoolEntry::new(vec!["member".into()]));
    cfg.models
        .insert("m".into(), ModelEntry::new("pool", upstream));
    cfg
}

fn direct_model(entry: ProviderEntry, upstream: &str) -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert("p".into(), entry);
    cfg.models
        .insert("m".into(), ModelEntry::new("p", upstream));
    cfg
}

fn quiet() -> BuildOptions {
    BuildOptions::new().with_mint_installation_id(false)
}

fn counting() -> (Arc<CountingStore>, Arc<dyn SecretStore>) {
    let store = Arc::new(CountingStore::default());
    let dyn_store: Arc<dyn SecretStore> = store.clone();
    (store, dyn_store)
}

/// The text every plan refusal carries; no later build step produces it.
const PLAN_REFUSAL: &str = "the managed ";

/// The pool rule a plan refusal maps to. The pool probes a member's
/// credential only after its plan, so on the rows whose own lane reads
/// nothing at build this text, not the zero count, is what pins the plan.
const POOL_PLAN_REFUSAL: &str = "is refused by its provider plan";

/// Build `entry` on all three paths over counting stores; every path must
/// refuse it with the plan's own reason and reach no store method.
async fn assert_refused_before_any_store_call(case: &str, entry: &ProviderEntry, upstream: &str) {
    let (direct, store) = counting();
    let built = build_provider("p", entry, store).await;
    let err = built.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains(PLAN_REFUSAL), "{case}: build_provider: {err}");
    assert_eq!(direct.calls(), 0, "{case}: build_provider");

    let (pooled, store) = counting();
    let refusals =
        pool_member_refusals(&one_member_pool(entry.clone(), upstream), store, quiet()).await;
    assert_eq!(refusals.len(), 1, "{case}: pool refuses");
    assert!(
        refusals[0].contains(POOL_PLAN_REFUSAL),
        "{case}: pool: {refusals:?}"
    );
    assert_eq!(pooled.calls(), 0, "{case}: pool_member_refusals");

    let (resolved, store) = counting();
    let (models, failed) =
        build_resolved_models(&direct_model(entry.clone(), upstream), store, quiet())
            .await
            .expect("a refused provider is skipped, not fatal");
    assert!(models.is_empty(), "{case}: no model built");
    assert_eq!(failed.len(), 1, "{case}: one failure");
    assert!(
        failed[0].1.contains(PLAN_REFUSAL),
        "{case}: build_resolved_models: {failed:?}"
    );
    assert_eq!(resolved.calls(), 0, "{case}: build_resolved_models");
}

#[tokio::test]
async fn containment_refuses_before_any_store_call() {
    for lane in vendor_lanes() {
        for r in [
            format!("oauth://{}", lane.vendor),
            format!("oauth://{}#seat-secret-label", lane.vendor),
        ] {
            for (label, entry) in refused_entries(&lane, &r) {
                assert_refused_before_any_store_call(
                    &format!("{r} {label}"),
                    &entry,
                    lane.upstream,
                )
                .await;
            }
        }
    }
}

/// The own lane of every vendor builds on all three paths. The pool always
/// probes the credential; the direct and resolved-model paths read the store
/// at build exactly on the lanes marked `reads_at_build`, which is what makes
/// the zero count on those lanes' refused rows a pin for the plan.
#[tokio::test]
async fn the_own_lane_reaches_the_store_on_every_build_path() {
    for lane in vendor_lanes() {
        let r = format!("oauth://{}", lane.vendor);
        let entry = (lane.entry)(&r, lane.own_base);

        let (direct, store) = counting();
        let built = build_provider("p", &entry, store).await;
        assert!(built.is_ok(), "{r}: {:?}", built.err());
        assert_eq!(
            direct.calls() > 0,
            lane.reads_at_build,
            "{r}: build_provider"
        );

        let (pooled, store) = counting();
        let refusals = pool_member_refusals(
            &one_member_pool(entry.clone(), lane.upstream),
            store,
            quiet(),
        )
        .await;
        assert!(refusals.is_empty(), "{r}: {refusals:?}");
        assert!(pooled.calls() > 0, "{r}: the pool probes the credential");

        for (path, cfg) in [
            ("pool", one_member_pool(entry.clone(), lane.upstream)),
            ("direct", direct_model(entry.clone(), lane.upstream)),
        ] {
            let (resolved, store) = counting();
            let (models, failed) = build_resolved_models(&cfg, store, quiet())
                .await
                .expect("the own lane builds");
            assert!(failed.is_empty(), "{r} {path}: {failed:?}");
            assert!(models.contains_key("m"), "{r} {path}: model built");
            let expect_reads = path == "pool" || lane.reads_at_build;
            assert_eq!(
                resolved.calls() > 0,
                expect_reads,
                "{r} {path}: build_resolved_models"
            );
        }
    }
}

/// A second reference slot naming another vendor refuses the entry even on
/// the primary credential's own lane, before the account id is read.
#[cfg(feature = "openai-responses")]
#[tokio::test]
async fn a_second_slot_naming_another_vendor_refuses_before_any_store_call() {
    let own = || codex("oauth://codex#seat-b", VENDOR_BASES[1]);
    for foreign in ["oauth://xai", "oauth://anthropic#seat-secret-label"] {
        let entry = own().with_account_id_ref(foreign);
        assert_refused_before_any_store_call(
            &format!("account_id_ref {foreign}"),
            &entry,
            "gpt-5-codex",
        )
        .await;
    }

    let (direct, store) = counting();
    let built = build_provider(
        "p",
        &own().with_account_id_ref("oauth://codex#seat-b"),
        store,
    )
    .await;
    assert!(built.is_ok(), "{:?}", built.err());
    assert!(direct.calls() > 0, "its own account id slot is read");
}

/// Positive control for the Bedrock credential path of
/// `build_resolved_models`: a static key there is read from the store, so the
/// zero count for a managed key on that path is the plan's doing.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn the_bedrock_credential_path_reads_a_static_key() {
    let (resolved, store) = counting();
    let cfg = direct_model(
        native_bedrock("env://ROUTECTL_PLAN_TEST_KEY"),
        "anthropic.claude-opus-4-7",
    );

    let (models, failed) = build_resolved_models(&cfg, store, quiet())
        .await
        .expect("a static bedrock key builds");

    assert!(failed.is_empty(), "{failed:?}");
    assert!(models.contains_key("m"));
    assert!(resolved.calls() > 0);
}

#[test]
fn a_refused_plan_names_the_provider_and_withholds_the_entry() {
    let entry = anthropic(
        "oauth://anthropic#seat-secret-label",
        "https://gateway.sentinel-host.example/p?k=1",
    );

    let err = plan_provider("gw", &entry)
        .expect_err("foreign host")
        .to_string();

    assert!(err.contains("provider `gw`"), "{err}");
    assert!(err.contains("api.anthropic.com"), "{err}");
    for leaked in ["sentinel-host", "seat-secret-label", "#"] {
        assert!(!err.contains(leaked), "leaks {leaked}: {err}");
    }
    let other_seat_and_host = anthropic("oauth://anthropic#seat-b", "https://other.example/q");
    assert_eq!(
        plan_provider("gw", &other_seat_and_host)
            .expect_err("foreign host")
            .to_string(),
        err,
        "the refusal is rendered from the vendor row, not from the entry"
    );
}

/// A credential that does not parse is refused naming the provider, the key
/// and the scheme rule, never the text after the scheme.
#[test]
fn an_unusable_credential_is_refused_without_echoing_it() {
    let entry = ProviderEntry::anthropic_api("file://sk-ant-SENTINEL");

    let err = plan_provider("p", &entry)
        .expect_err("relative file path")
        .to_string();

    assert!(err.contains("provider `p`"), "{err}");
    assert!(err.contains("`credential`"), "{err}");
    assert!(
        err.contains("file:// URI must be an absolute path"),
        "{err}"
    );
    assert!(!err.contains("SENTINEL"), "{err}");
}

/// The forwarded credential obeys the managed Anthropic login's host rule:
/// https, exact host, no userinfo, no port. Each refused base is planted with
/// a sentinel where the URL allows one, and the refusal names the key and
/// the rule without echoing the base. The accepted rows are the forms the
/// URL parser normalizes onto the canonical base.
#[test]
fn the_forwarded_credential_plans_only_over_https_to_the_anthropic_host() {
    let refused = [
        ("plain http", "http://api.anthropic.com/sentinel-path"),
        (
            "plain http, uppercase scheme",
            "HTTP://api.anthropic.com/sentinel-path",
        ),
        (
            "http on port 443",
            "http://api.anthropic.com:443/sentinel-path",
        ),
        (
            "non-default port",
            "https://api.anthropic.com:8443/sentinel-path",
        ),
        ("userinfo", "https://sentinel-user:pw@api.anthropic.com"),
        (
            "trailing-dot host",
            "https://api.anthropic.com./sentinel-path",
        ),
        (
            "non-ASCII look-alike host",
            "https://api.anthrop\u{131}c.com/sentinel-path",
        ),
    ];
    let accepted = [
        ("canonical", "https://api.anthropic.com"),
        ("uppercase scheme", "HTTPS://api.anthropic.com"),
        ("explicit default port", "https://api.anthropic.com:443"),
    ];

    for (label, base) in refused {
        let entry = ProviderEntry::anthropic_api("forwarded")
            .with_auth(Auth::Bearer)
            .with_base_url(base);
        let err = plan_provider("fwd", &entry).expect_err(label).to_string();
        assert!(err.contains("provider `fwd`"), "{label}: {err}");
        assert!(err.contains("credential = \"forwarded\""), "{label}: {err}");
        assert!(err.contains("https base_url"), "{label}: {err}");
        assert!(err.contains("withheld"), "{label}: {err}");
        assert!(!err.contains("sentinel"), "{label}: echoes the base: {err}");
    }
    for (label, base) in accepted {
        let entry = ProviderEntry::anthropic_api("forwarded")
            .with_auth(Auth::Bearer)
            .with_base_url(base);
        assert!(plan_provider("fwd", &entry).is_ok(), "{label}: {base}");
    }
}

/// The api-key form of a managed Anthropic login is refused with a message
/// that names the auth it needs and withholds the seat label.
#[test]
fn the_api_key_form_of_a_managed_anthropic_login_is_refused_naming_the_selector() {
    let entry = ProviderEntry::anthropic_api("oauth://anthropic#seat-secret-label");

    let err = plan_provider("claude", &entry)
        .expect_err("api-key form")
        .to_string();

    assert!(err.contains("provider `claude`"), "{err}");
    assert!(err.contains(PLAN_REFUSAL), "{err}");
    assert!(err.contains("auth = \"bearer\""), "{err}");
    for leaked in ["seat-secret-label", "#"] {
        assert!(!err.contains(leaked), "leaks {leaked}: {err}");
    }
    assert!(
        plan_provider("claude", &entry.with_auth_kind(AuthKind::OauthBearer)).is_ok(),
        "the bearer form of the same entry plans"
    );
}

#[test]
fn client_identity_per_lane() {
    #[cfg_attr(
        not(any(feature = "bedrock", feature = "openai-responses", feature = "gemini")),
        allow(unused_mut)
    )]
    let mut rows: Vec<(&str, ProviderEntry, bool)> = vec![
        (
            "managed anthropic on its host",
            anthropic("oauth://anthropic", "https://api.anthropic.com"),
            true,
        ),
        (
            "labeled managed anthropic on its host",
            anthropic("oauth://anthropic#seat-b", "https://api.anthropic.com/v1"),
            true,
        ),
        (
            "managed xai on its host",
            openai_chat("oauth://xai", "https://api.x.ai/v1"),
            false,
        ),
        (
            "env bearer to api.anthropic.com",
            anthropic("env://ANTHROPIC_TOKEN", "https://api.anthropic.com"),
            false,
        ),
        (
            "env key to api.anthropic.com",
            ProviderEntry::anthropic_api("env://ANTHROPIC_API_KEY"),
            false,
        ),
        (
            "env bearer to openrouter",
            openai_chat("env://OPENROUTER_API_KEY", "https://openrouter.ai/api/v1"),
            false,
        ),
        (
            "env bearer to openrouter on anthropic-api",
            anthropic("env://OPENROUTER_API_KEY", "https://openrouter.ai/api"),
            false,
        ),
        (
            "forwarded",
            ProviderEntry::anthropic_api("forwarded").with_auth(Auth::Bearer),
            false,
        ),
        (
            "forwarded with the managed ref in its second slot",
            {
                let mut entry = ProviderEntry::anthropic_api("forwarded").with_auth(Auth::Bearer);
                entry.account_id_ref = Some("oauth://anthropic".into());
                entry
            },
            false,
        ),
        (
            "managed anthropic in the api-key form on its host",
            ProviderEntry::anthropic_api("oauth://anthropic")
                .with_auth_kind(AuthKind::ApiKey)
                .with_base_url("https://api.anthropic.com"),
            false,
        ),
        (
            "labeled managed anthropic in the builder's api-key form",
            ProviderEntry::anthropic_api("oauth://anthropic#seat-b"),
            false,
        ),
        (
            "managed anthropic off its host",
            anthropic("oauth://anthropic", "https://gateway.example"),
            false,
        ),
        (
            "codex ref on anthropic-api at the anthropic host",
            anthropic("oauth://codex", "https://api.anthropic.com"),
            false,
        ),
    ];
    #[cfg(feature = "openai-responses")]
    {
        use routectl_providers::openai_responses::AuthKind as ResponsesAuth;
        rows.push((
            "managed codex default base",
            ProviderEntry::openai_responses("oauth://codex"),
            true,
        ));
        rows.push((
            "labeled managed codex on its host",
            codex(
                "oauth://codex#seat-b",
                "https://chatgpt.com/backend-api/codex",
            ),
            true,
        ));
        rows.push((
            "codex chatgpt lane on chatgpt.com with account_id_ref = oauth://xai",
            codex("oauth://codex", "https://chatgpt.com/backend-api/codex")
                .with_account_id_ref("oauth://xai"),
            false,
        ));
        rows.push((
            "codex default base with a labeled anthropic account_id_ref",
            ProviderEntry::openai_responses("oauth://codex")
                .with_account_id_ref("oauth://anthropic#seat-b"),
            false,
        ));
        rows.push((
            "codex chatgpt lane with its own account_id_ref",
            codex("oauth://codex", "https://chatgpt.com/backend-api/codex")
                .with_account_id_ref("oauth://codex#seat-b"),
            true,
        ));
        rows.push((
            "env bearer on the codex lane",
            codex("env://OPENAI_JWT", "https://chatgpt.com/backend-api/codex"),
            false,
        ));
        rows.push((
            "responses api-key",
            ProviderEntry::openai_responses("env://OPENAI_API_KEY")
                .with_openai_responses_auth_kind(ResponsesAuth::ApiKey),
            false,
        ));
        rows.push((
            "managed codex on the platform host",
            ProviderEntry::openai_responses("oauth://codex")
                .with_openai_responses_base_url("https://api.openai.com/v1"),
            false,
        ));
    }
    #[cfg(feature = "gemini")]
    {
        rows.push((
            "managed antigravity on the daily host",
            cloud_code(
                "oauth://antigravity",
                "https://daily-cloudcode-pa.googleapis.com",
            ),
            true,
        ));
        rows.push((
            "managed antigravity on the prod host",
            cloud_code(
                "oauth://antigravity#seat-b",
                "https://cloudcode-pa.googleapis.com",
            ),
            true,
        ));
        rows.push((
            "codex ref on the cloud code host",
            cloud_code("oauth://codex", "https://cloudcode-pa.googleapis.com"),
            false,
        ));
        rows.push((
            "gemini api-key",
            ProviderEntry::gemini("env://GEMINI_API_KEY"),
            false,
        ));
    }
    #[cfg(feature = "bedrock")]
    {
        rows.push((
            "anthropic mantle",
            mantle_anthropic("oauth://anthropic"),
            false,
        ));
        rows.push(("openai-chat mantle", mantle_chat("oauth://xai"), false));
        rows.push(("native bedrock", native_bedrock("oauth://anthropic"), false));
    }
    #[cfg(all(feature = "bedrock", feature = "openai-responses"))]
    rows.push((
        "openai-responses mantle",
        mantle_responses("oauth://codex"),
        false,
    ));

    for (label, entry, expected) in rows {
        let verdict = client_identity(&entry);

        assert_eq!(verdict, expected, "{label}");
        if let Ok(plan) = plan_provider("p", &entry) {
            assert_eq!(
                plan.client_identity, expected,
                "{label}: plan carries the verdict"
            );
        }
    }
}
