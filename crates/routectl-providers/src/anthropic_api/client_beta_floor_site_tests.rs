//! Every site that adds or removes an `anthropic-beta` flag after the
//! router's withhold, driven through `build_headers` with each site's
//! condition both true and false.
//!
//! The router's load-time advisory (`beta_floor_override_warnings`, crates
//! `routectl-router` `factory/beta_floor_warnings.rs`) warns that an
//! `unsupported` override is inert wherever one of these sites re-adds the
//! flag. Its site table (module doc of `factory/warnings_beta_floor_tests.rs`)
//! names the same sites as the rows here; a row here pins the egress fact
//! the advisory row of the same site relies on, so a change to either table
//! updates the other.
//!
//! | site | what it is | flag present when |
//! |---|---|---|
//! | provider pin | provider `header_extras["anthropic-beta"]` | always, every leg |
//! | model pin | model pin (`operator_betas`) | always, every leg |
//! | OAuth gate | the OAuth beta | own oauth-bearer on the Anthropic API host, with or without client identity, any cloak mode |
//! | Claude Code floor | the client-identity beta floor | client identity, not forwarded, AND classified non-Claude-Code (`cloak_mode = "never"` -> never) |
//! | context-management strip | context-management removal | removed whenever the provider sets `context_management` |
//! | structured outputs | structured-outputs beta | not the forwarded leg AND body carries `output_config.format` |
//! | effort | effort beta | client identity, not forwarded, AND body carries `output_config.effort` |
//! | redaction drop | redact-thinking removal | removed off the forwarded leg when the body carries `thinking.display` |
//!
//! Off the forwarded leg the client sends the flag and the router withholds
//! it, so the flag is present only if the egress re-adds it -- the exact
//! claim the advisory makes. The router withholds nothing on the forwarded
//! leg, so there the client sends nothing and the row asks whether the
//! egress mints the flag itself.

use super::*;
use routectl_core::identity::anthropic::{
    EFFORT_BETA, OAUTH_ANTHROPIC_BETA, REDACT_THINKING_BETA, STRUCTURED_OUTPUTS_BETA,
    default_claude_code_anthropic_betas,
};
use routectl_core::{ChatRequest, StaticToken};
use serde_json::json;

const OAUTH_HOST: &str = "https://api.anthropic.com";
const OFF_HOST: &str = "https://anthropic-proxy.example.test";
const PIN: &str = "pinned-beta-2026-01-01";

fn cfg(base_url: &str, auth_kind: AuthKind) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "beta-floor-site-test".into(),
        auth: Arc::new(StaticToken::new("test-token")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        client_identity: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

/// The lane a row's provider egresses on.
#[derive(Clone, Copy)]
enum Lane {
    ApiKey,
    /// api-key auth on the Anthropic API host: the cloak lane needs oauth.
    ApiKeyOnHost,
    /// The managed login on its own host: the factory's verdict grants the
    /// client identity.
    OwnOauth,
    /// An oauth-bearer on the Anthropic API host whose credential is not
    /// the managed login, so the verdict withholds the client identity.
    OwnOauthNoIdentity,
    OffHostOauth,
    Forwarded,
}

/// One row: a lane plus the knobs that flip a site's condition.
struct Row {
    name: &'static str,
    lane: Lane,
    flag: &'static str,
    cloak_mode: CloakMode,
    provider_pin: bool,
    model_pin: bool,
    context_management: bool,
    cc_session_header: bool,
    originating_cc_session: Option<bool>,
    body: Option<Value>,
    present: bool,
}

const fn row(name: &'static str, lane: Lane, flag: &'static str, present: bool) -> Row {
    Row {
        name,
        lane,
        flag,
        cloak_mode: CloakMode::Auto,
        provider_pin: false,
        model_pin: false,
        context_management: false,
        cc_session_header: false,
        originating_cc_session: None,
        body: None,
        present,
    }
}

impl Row {
    fn provider(&self) -> AnthropicApiProvider {
        let mut config = match self.lane {
            Lane::ApiKey => cfg("http://127.0.0.1:18080", AuthKind::ApiKey),
            Lane::ApiKeyOnHost => cfg(OAUTH_HOST, AuthKind::ApiKey),
            Lane::OwnOauth => AnthropicApiConfig {
                client_identity: true,
                ..cfg(OAUTH_HOST, AuthKind::OauthBearer)
            },
            Lane::OwnOauthNoIdentity => cfg(OAUTH_HOST, AuthKind::OauthBearer),
            Lane::OffHostOauth => cfg(OFF_HOST, AuthKind::OauthBearer),
            Lane::Forwarded => cfg(OAUTH_HOST, AuthKind::OauthBearer),
        };
        config.use_forwarded_bearer = matches!(self.lane, Lane::Forwarded);
        config.cloak.mode = self.cloak_mode;
        config.context_management = self.context_management;
        if self.provider_pin {
            config.header_extras = vec![("anthropic-beta".into(), self.flag.into())];
        }
        AnthropicApiProvider::new(config)
    }

    fn request(&self) -> ChatRequest {
        let mut req = ChatRequest::default();
        if matches!(self.lane, Lane::Forwarded) {
            req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
                "sk-ant-oat01-forwarded".into(),
            ));
        } else {
            req.anthropic_beta = vec![self.flag.to_string()];
            req.routectl_internal.withheld_betas = Arc::from(vec![self.flag.to_string()]);
        }
        if self.model_pin {
            req.routectl_internal.operator_betas = vec![self.flag.to_string()];
        }
        if self.cc_session_header {
            req.routectl_internal.claude_code_headers =
                vec![("x-claude-code-session-id".into(), "sess-1".into())];
        }
        req.routectl_internal.originating_claude_code_session = self.originating_cc_session;
        req
    }

    fn check(&self) {
        let provider = self.provider();
        let req = self.request();
        assert_eq!(
            provider.forwarded_leg(&req),
            matches!(self.lane, Lane::Forwarded),
            "row `{}`: lane premise",
            self.name
        );
        let rb = reqwest::Client::new().post("http://127.0.0.1/test");
        let (rb, _decision) = provider.build_headers(rb, &req, "test-token", self.body.as_ref());
        let request = rb.build().expect("build outbound request");
        let header = request
            .headers()
            .get("anthropic-beta")
            .map(|v| v.to_str().expect("ascii header").to_string())
            .unwrap_or_default();
        let shipped = header.split(',').any(|f| f == self.flag);
        assert_eq!(
            shipped, self.present,
            "row `{}`: `{}` in `{header}`",
            self.name, self.flag
        );
    }
}

/// A Claude Code floor flag in no other site's class, so its rows pin the Claude Code floor alone.
fn claude_code_only_flag() -> &'static str {
    default_claude_code_anthropic_betas()
        .iter()
        .copied()
        .find(|flag| {
            ![
                OAUTH_ANTHROPIC_BETA,
                STRUCTURED_OUTPUTS_BETA,
                context_management::CONTEXT_MANAGEMENT_BETA,
                REDACT_THINKING_BETA,
            ]
            .contains(flag)
        })
        .expect("the Claude Code floor carries a flag outside the other classes")
}

fn pin_rows() -> Vec<Row> {
    vec![
        Row {
            provider_pin: true,
            ..row("provider pin, api-key", Lane::ApiKey, PIN, true)
        },
        Row {
            provider_pin: true,
            ..row("provider pin, forwarded", Lane::Forwarded, PIN, true)
        },
        row("no provider pin, api-key", Lane::ApiKey, PIN, false),
        Row {
            model_pin: true,
            ..row("model pin, api-key", Lane::ApiKey, PIN, true)
        },
        Row {
            model_pin: true,
            ..row("model pin, forwarded", Lane::Forwarded, PIN, true)
        },
        row("no model pin, forwarded", Lane::Forwarded, PIN, false),
    ]
}

fn oauth_gate_rows() -> Vec<Row> {
    let gate = OAUTH_ANTHROPIC_BETA;
    vec![
        row("OAuth gate, own oauth-bearer", Lane::OwnOauth, gate, true),
        Row {
            cloak_mode: CloakMode::Always,
            ..row(
                "OAuth gate, own oauth-bearer, cloak always",
                Lane::OwnOauth,
                gate,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Never,
            ..row(
                "OAuth gate, own oauth-bearer, cloak never",
                Lane::OwnOauth,
                gate,
                true,
            )
        },
        Row {
            cc_session_header: true,
            ..row(
                "OAuth gate, own oauth-bearer, Claude Code session",
                Lane::OwnOauth,
                gate,
                true,
            )
        },
        row(
            "OAuth gate, api-key on the host",
            Lane::ApiKeyOnHost,
            gate,
            false,
        ),
        row(
            "OAuth gate, oauth-bearer off the host",
            Lane::OffHostOauth,
            gate,
            false,
        ),
        row(
            "OAuth gate, own oauth-bearer without client identity",
            Lane::OwnOauthNoIdentity,
            gate,
            true,
        ),
        row("OAuth gate, forwarded", Lane::Forwarded, gate, false),
    ]
}

fn claude_code_floor_rows() -> Vec<Row> {
    let cc = claude_code_only_flag();
    vec![
        row(
            "Claude Code floor, cloak auto, no session",
            Lane::OwnOauth,
            cc,
            true,
        ),
        Row {
            cc_session_header: true,
            ..row(
                "Claude Code floor, cloak auto, Claude Code session",
                Lane::OwnOauth,
                cc,
                false,
            )
        },
        Row {
            originating_cc_session: Some(true),
            ..row(
                "Claude Code floor, cloak auto, originating Claude Code session",
                Lane::OwnOauth,
                cc,
                false,
            )
        },
        Row {
            cc_session_header: true,
            originating_cc_session: Some(false),
            ..row(
                "Claude Code floor, cloak auto, originating non-Claude-Code",
                Lane::OwnOauth,
                cc,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Always,
            cc_session_header: true,
            ..row(
                "Claude Code floor, cloak always, Claude Code session",
                Lane::OwnOauth,
                cc,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Never,
            ..row("Claude Code floor, cloak never", Lane::OwnOauth, cc, false)
        },
        row(
            "Claude Code floor, api-key on the host",
            Lane::ApiKeyOnHost,
            cc,
            false,
        ),
        row(
            "Claude Code floor, own oauth-bearer without client identity",
            Lane::OwnOauthNoIdentity,
            cc,
            false,
        ),
        Row {
            cloak_mode: CloakMode::Always,
            ..row(
                "Claude Code floor, own oauth-bearer without client identity, cloak always",
                Lane::OwnOauthNoIdentity,
                cc,
                false,
            )
        },
        row("Claude Code floor, forwarded", Lane::Forwarded, cc, false),
    ]
}

fn context_management_rows() -> Vec<Row> {
    let cm = context_management::CONTEXT_MANAGEMENT_BETA;
    vec![
        Row {
            context_management: true,
            ..row(
                "context-management strip on, Claude Code floor",
                Lane::OwnOauth,
                cm,
                false,
            )
        },
        row(
            "context-management strip off, Claude Code floor",
            Lane::OwnOauth,
            cm,
            true,
        ),
        Row {
            context_management: true,
            provider_pin: true,
            ..row(
                "context-management strip on, provider pin",
                Lane::ApiKey,
                cm,
                false,
            )
        },
        Row {
            provider_pin: true,
            ..row(
                "context-management strip off, provider pin",
                Lane::ApiKey,
                cm,
                true,
            )
        },
        Row {
            context_management: true,
            provider_pin: true,
            ..row(
                "context-management strip on, provider pin, forwarded",
                Lane::Forwarded,
                cm,
                false,
            )
        },
    ]
}

fn body_union_rows() -> Vec<Row> {
    let format = json!({"output_config": {"format": {"type": "json_schema"}}});
    let effort = json!({"output_config": {"effort": "high"}});
    let so = STRUCTURED_OUTPUTS_BETA;
    vec![
        Row {
            body: Some(format.clone()),
            ..row(
                "structured outputs, api-key, format",
                Lane::ApiKey,
                so,
                true,
            )
        },
        Row {
            body: Some(json!({"model": "m"})),
            ..row(
                "structured outputs, api-key, no format",
                Lane::ApiKey,
                so,
                false,
            )
        },
        Row {
            body: Some(format),
            ..row(
                "structured outputs, forwarded, format",
                Lane::Forwarded,
                so,
                false,
            )
        },
        Row {
            body: Some(effort.clone()),
            ..row(
                "effort, own oauth-bearer, effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Always,
            body: Some(effort.clone()),
            ..row(
                "effort, own oauth-bearer, cloak always, effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Never,
            body: Some(effort.clone()),
            ..row(
                "effort, own oauth-bearer, cloak never, effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                true,
            )
        },
        Row {
            body: Some(json!({"model": "m"})),
            ..row(
                "effort, own oauth-bearer, no effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                false,
            )
        },
        Row {
            body: Some(effort.clone()),
            ..row(
                "effort, own oauth-bearer without client identity, effort",
                Lane::OwnOauthNoIdentity,
                EFFORT_BETA,
                false,
            )
        },
        Row {
            body: Some(effort.clone()),
            ..row(
                "effort, api-key on the host, effort",
                Lane::ApiKeyOnHost,
                EFFORT_BETA,
                false,
            )
        },
        Row {
            body: Some(effort),
            ..row(
                "effort, forwarded, effort",
                Lane::Forwarded,
                EFFORT_BETA,
                false,
            )
        },
    ]
}

fn redaction_rows() -> Vec<Row> {
    let display = json!({"thinking": {"type": "adaptive", "display": "summarized"}});
    let no_display = json!({"thinking": {"type": "adaptive"}});
    let redact = REDACT_THINKING_BETA;
    vec![
        Row {
            body: Some(no_display.clone()),
            ..row(
                "redaction drop, Claude Code floor, no display",
                Lane::OwnOauth,
                redact,
                true,
            )
        },
        Row {
            body: Some(display.clone()),
            ..row(
                "redaction drop, Claude Code floor, display",
                Lane::OwnOauth,
                redact,
                false,
            )
        },
        Row {
            provider_pin: true,
            body: Some(no_display),
            ..row(
                "redaction drop, provider pin, api-key, no display",
                Lane::ApiKey,
                redact,
                true,
            )
        },
        Row {
            provider_pin: true,
            body: Some(display.clone()),
            ..row(
                "redaction drop, provider pin, api-key, display",
                Lane::ApiKey,
                redact,
                false,
            )
        },
        Row {
            provider_pin: true,
            body: Some(display),
            ..row(
                "redaction drop, provider pin, forwarded, display",
                Lane::Forwarded,
                redact,
                true,
            )
        },
    ]
}

#[test]
fn each_beta_site_ships_its_flag_exactly_where_its_condition_holds() {
    let rows = pin_rows()
        .into_iter()
        .chain(oauth_gate_rows())
        .chain(claude_code_floor_rows())
        .chain(context_management_rows())
        .chain(body_union_rows())
        .chain(redaction_rows());

    for row in rows {
        row.check();
    }
}
