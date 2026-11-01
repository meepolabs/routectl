//! A re-login readmits a seat with no restart, by seat kind. A built seat
//! later marked unusable is readmitted in place: the credentials reload lifts
//! its mark and the router is not rebuilt. A placeholder seat (its credential
//! could not be read at build, so it has no provider) is readmitted by a
//! rebuild, whether its record was present but dead and is re-logged under
//! the same key, or was never stored and is added. Either way the next
//! request is served by the readmitted seat with its new token, and a seat
//! still logged out keeps its mark across the rebuild.

use super::*;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_router::RouterOptions;
use routectl_router::runtime_state::UnusableCause;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SEAT_A: &str = "xai#seat-a";
const SEAT_B: &str = "xai#seat-b";
const NEW_TOKEN: &str = "tok-a-relogged";

/// How seat a's record stands in the credentials file before the re-login.
#[derive(Clone, Copy, Debug)]
enum SeatA {
    /// A fresh record: the seat builds.
    Fresh,
    /// A record whose access token expired and whose refresh is refused.
    Dead,
    /// No record at all.
    Absent,
}

/// Pool `the-pool` of seats `seat-a`, `seat-b` and `seat-c` (declaration
/// order, fill-first) behind model `grok`, every seat dialing `upstream`.
/// Seat c is never logged in.
fn config_text(upstream: &str) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [retry]\nmax_attempts = 1\n\n"
    );
    for member in ["seat-a", "seat-b", "seat-c"] {
        text.push_str(&format!(
            "[providers.{member}]\nkind = \"anthropic-api\"\nbase_url = \"{upstream}\"\n\
             api_key_ref = \"oauth://xai#{member}\"\nauth_kind = \"oauth-bearer\"\n\n"
        ));
    }
    text.push_str(
        "[pools.the-pool]\nmembers = [\"seat-a\", \"seat-b\", \"seat-c\"]\n\n\
         [models.grok]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n",
    );
    text
}

/// One stored record: its access token and its expiry.
fn record(token: &str, expires_at_unix: u64, obtained_at_unix: u64) -> serde_json::Value {
    serde_json::json!({
        "access_token": token,
        "refresh_token": "seeded-refresh-token",
        "token_type": "Bearer",
        "expires_at_unix": expires_at_unix,
        "scopes": ["user:inference"],
        "account": { "email": null, "account_id": null },
        "obtained_at_unix": obtained_at_unix
    })
}

/// The live reload rig over one temp credentials file: the scripted-refresh
/// OAuth store, handed to the reload both as the credentials cache and as the
/// secrets the rebuild probes, so the rebuild sees the cache the reload just
/// replaced.
struct Rig {
    creds: PathBuf,
    stamp: u64,
    flow: Arc<ScriptedRefreshFlow>,
    oauth: Arc<routectl_auth::OAuthStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    upstream: MockServer,
    _usage_dir: tempfile::TempDir,
}

impl Rig {
    async fn new(dir: &Path, seat_a: SeatA) -> Self {
        let creds = dir.join("routectl").join("credentials.json");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let upstream = MockServer::start().await;
        let seat_a_record = match seat_a {
            SeatA::Fresh => Some(record("tok-a", stamp + 3600, stamp)),
            SeatA::Dead => Some(record("tok-a-dead", stamp - 3600, stamp - 7200)),
            SeatA::Absent => None,
        };
        write_records(&creds, seat_a_record, stamp);
        let flow = Arc::new(ScriptedRefreshFlow::new(ScriptedRefresh::RefreshExpired));
        let oauth = Arc::new(
            routectl_auth::OAuthStore::open_with_scripted_refresh_for_tests(
                &creds,
                Arc::clone(&flow),
            )
            .await
            .expect("open the scripted credentials store"),
        );
        let mut config = routectl_router::parse_config(&config_text(&upstream.uri()))
            .expect("fixture config parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router = build_router_from_config_with_overlay(
            config.clone(),
            &Arc::default(),
            Self::secrets_of(&oauth),
        )
        .await
        .expect("initial router build");
        Self {
            creds,
            stamp,
            flow,
            oauth,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            upstream,
            _usage_dir: usage_dir,
        }
    }

    fn secrets_of(oauth: &Arc<routectl_auth::OAuthStore>) -> Arc<dyn SecretStore> {
        Arc::new((**oauth).clone())
    }

    /// `routectl login xai --label seat-a`: seat a's record becomes a
    /// fresh one carrying [`NEW_TOKEN`]; seat b's record is rewritten as it
    /// was, so only seat a changed.
    fn relogin_seat_a(&self) {
        write_records(
            &self.creds,
            Some(record(NEW_TOKEN, self.stamp + 3600, self.stamp + 1)),
            self.stamp,
        );
    }

    async fn reload_credentials(&self) {
        handle_credentials_reload(
            &Some(self.oauth.clone()),
            &self.config,
            &Arc::new(CatalogOverlay::default()),
            Self::secrets_of(&self.oauth),
            &self.swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;
    }

    /// The upstream answers only a request carrying the re-logged token.
    async fn accept_only_the_new_token(&self) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", format!("Bearer {NEW_TOKEN}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "model": "claude-sonnet-4-6",
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&self.upstream)
            .await;
    }

    /// The member that served one request, or the error that refused it.
    async fn served_by(&self) -> Result<Option<String>, String> {
        let dispatched = self
            .swap
            .load()
            .complete_with_options(request(), RouterOptions::new())
            .await;
        dispatched
            .result
            .map(|_| dispatched.meta.served_provider)
            .map_err(|e| e.to_string())
    }

    fn mark_of(&self, member: &str) -> Option<UnusableCause> {
        let state_key = format!("grok#{member}");
        self.swap
            .load()
            .status_targets(Instant::now())
            .into_iter()
            .find(|target| target.state_key == state_key)
            .unwrap_or_else(|| panic!("no status target {state_key}"))
            .gate
            .credential_unusable
            .map(|mark| mark.cause)
    }
}

/// Write the credentials file with seat a's record (when any) and seat b's
/// fresh record stamped at `stamp`; seat c is never stored.
fn write_records(creds: &Path, seat_a: Option<serde_json::Value>, stamp: u64) {
    let mut providers = serde_json::Map::new();
    if let Some(seat_a) = seat_a {
        providers.insert(SEAT_A.to_string(), seat_a);
    }
    providers.insert(SEAT_B.to_string(), record("tok-b", stamp + 3600, stamp));
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    std::fs::create_dir_all(creds.parent().expect("creds parent")).expect("mkdir creds parent");
    std::fs::write(
        creds,
        serde_json::to_vec_pretty(&doc).expect("serialize creds"),
    )
    .expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(creds, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "grok".into(),
        max_tokens: Some(16),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

#[tokio::test]
#[serial_test::serial]
async fn a_relogin_readmits_a_built_seat_in_place_without_a_rebuild() {
    // Arrange: seat a built, then found dead and marked revoked.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let rig = Rig::new(dir.path(), SeatA::Fresh).await;
    assert!(
        !rig.swap.load().is_placeholder_account(SEAT_A),
        "premise: seat a is built"
    );
    assert!(
        rig.swap
            .load()
            .mark_credential_unusable_for_tests("grok#seat-a", UnusableCause::Revoked),
        "premise: seat a's account is marked"
    );
    let before = rig.swap.load_full();
    rig.accept_only_the_new_token().await;
    rig.relogin_seat_a();

    // Act
    rig.reload_credentials().await;

    // Assert
    assert!(
        Arc::ptr_eq(&before, &rig.swap.load_full()),
        "a built seat is readmitted in place, not by a rebuild"
    );
    assert_eq!(rig.mark_of("seat-a"), None);
    assert_eq!(rig.served_by().await, Ok(Some("seat-a".to_string())));
}

/// `(name, seat a's record before the re-login, its expected build mark)`.
type PlaceholderRow = (&'static str, SeatA, UnusableCause);

#[tokio::test]
#[serial_test::serial]
async fn a_relogin_readmits_a_placeholder_seat_by_rebuilding_the_router() {
    let rows: [PlaceholderRow; 2] = [
        (
            "dead record re-logged under the same key",
            SeatA::Dead,
            UnusableCause::Unreadable,
        ),
        (
            "never-logged-in seat gets a record",
            SeatA::Absent,
            UnusableCause::NotLoggedIn,
        ),
    ];
    for (name, seat_a, build_mark) in rows {
        // Arrange: seat a is a placeholder; seat c is logged out throughout.
        let dir = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let rig = Rig::new(dir.path(), seat_a).await;
        assert!(
            rig.swap.load().is_placeholder_account(SEAT_A),
            "{name}: premise, seat a is a placeholder"
        );
        assert_eq!(rig.mark_of("seat-a"), Some(build_mark), "{name}: premise");
        if matches!(seat_a, SeatA::Dead) {
            assert!(
                rig.flow.post_count() >= 1,
                "{name}: premise, the build tried to refresh the dead record"
            );
        }
        let before = rig.swap.load_full();
        let keys_before = rig.oauth.credential_keys().await;
        rig.accept_only_the_new_token().await;
        rig.relogin_seat_a();

        // Act
        rig.reload_credentials().await;

        // Assert
        if matches!(seat_a, SeatA::Dead) {
            assert_eq!(
                keys_before,
                rig.oauth.credential_keys().await,
                "{name}: premise, the key set is unchanged"
            );
        }
        assert!(
            !Arc::ptr_eq(&before, &rig.swap.load_full()),
            "{name}: a placeholder is readmitted by a rebuild"
        );
        assert!(
            !rig.swap.load().is_placeholder_account(SEAT_A),
            "{name}: the rebuild built seat a"
        );
        assert_eq!(rig.mark_of("seat-a"), None, "{name}");
        assert_eq!(
            rig.mark_of("seat-c"),
            Some(UnusableCause::NotLoggedIn),
            "{name}: a seat still logged out keeps its mark"
        );
        assert_eq!(
            rig.served_by().await,
            Ok(Some("seat-a".to_string())),
            "{name}"
        );
    }
}
