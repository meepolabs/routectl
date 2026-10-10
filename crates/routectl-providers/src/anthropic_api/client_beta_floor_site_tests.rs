//! Every site that adds or removes an `anthropic-beta` flag after the
//! router's withhold, driven through `build_headers` with each site's
//! condition both true and false.
//!
//! The router's load-time advisory (`beta_floor_override_warnings`, crates
//! `routectl-router` `factory/beta_floor_warnings.rs`) warns that an
//! `unsupported` override is inert wherever one of these sites re-adds the
//! flag. Its site table (module doc of `factory/warnings_beta_floor_tests.rs`)
//! uses the same S1-S8 labels as the rows here; a row here pins the egress
//! fact the advisory row of the same label relies on, so a change to either
//! table updates the other.
//!
//! | label | site | flag present when |
//! |---|---|---|
//! | S1 | provider `header_extras["anthropic-beta"]` pin | always, every leg |
//! | S2 | model pin (`operator_betas`) | always, every leg |
//! | S3 | OAuth gate | cloak lane: own oauth-bearer on the Anthropic API host, any cloak mode |
//! | S4 | Claude Code floor | cloak lane AND classified non-Claude-Code (cloak.mode never -> never) |
//! | S5 | context-management strip | removed whenever the provider sets `context_management` |
//! | S6 | structured outputs | not the forwarded leg AND body carries `output_config.format` |
//! | S7 | effort | cloak lane AND body carries `output_config.effort` |
//! | S8 | redaction drop | removed off the forwarded leg when the body carries `thinking.display` |
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
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
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
    OwnOauth,
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
            Lane::OwnOauth => cfg(OAUTH_HOST, AuthKind::OauthBearer),
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

/// A Claude Code floor flag in no other site's class, so its rows pin S4 alone.
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
            ..row("S1 provider pin, api-key", Lane::ApiKey, PIN, true)
        },
        Row {
            provider_pin: true,
            ..row("S1 provider pin, forwarded", Lane::Forwarded, PIN, true)
        },
        row("S1 no provider pin, api-key", Lane::ApiKey, PIN, false),
        Row {
            model_pin: true,
            ..row("S2 model pin, api-key", Lane::ApiKey, PIN, true)
        },
        Row {
            model_pin: true,
            ..row("S2 model pin, forwarded", Lane::Forwarded, PIN, true)
        },
        row("S2 no model pin, forwarded", Lane::Forwarded, PIN, false),
    ]
}

fn oauth_gate_rows() -> Vec<Row> {
    let gate = OAUTH_ANTHROPIC_BETA;
    vec![
        row(
            "S3 OAuth gate, own oauth-bearer",
            Lane::OwnOauth,
            gate,
            true,
        ),
        Row {
            cloak_mode: CloakMode::Always,
            ..row(
                "S3 OAuth gate, own oauth-bearer, cloak always",
                Lane::OwnOauth,
                gate,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Never,
            ..row(
                "S3 OAuth gate, own oauth-bearer, cloak never",
                Lane::OwnOauth,
                gate,
                true,
            )
        },
        Row {
            cc_session_header: true,
            ..row(
                "S3 OAuth gate, own oauth-bearer, Claude Code session",
                Lane::OwnOauth,
                gate,
                true,
            )
        },
        row(
            "S3 OAuth gate, api-key on the host",
            Lane::ApiKeyOnHost,
            gate,
            false,
        ),
        row(
            "S3 OAuth gate, oauth-bearer off the host",
            Lane::OffHostOauth,
            gate,
            false,
        ),
        row("S3 OAuth gate, forwarded", Lane::Forwarded, gate, false),
    ]
}

fn claude_code_floor_rows() -> Vec<Row> {
    let cc = claude_code_only_flag();
    vec![
        row(
            "S4 Claude Code floor, cloak auto, no session",
            Lane::OwnOauth,
            cc,
            true,
        ),
        Row {
            cc_session_header: true,
            ..row(
                "S4 Claude Code floor, cloak auto, Claude Code session",
                Lane::OwnOauth,
                cc,
                false,
            )
        },
        Row {
            originating_cc_session: Some(true),
            ..row(
                "S4 Claude Code floor, cloak auto, originating Claude Code session",
                Lane::OwnOauth,
                cc,
                false,
            )
        },
        Row {
            cc_session_header: true,
            originating_cc_session: Some(false),
            ..row(
                "S4 Claude Code floor, cloak auto, originating non-Claude-Code",
                Lane::OwnOauth,
                cc,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Always,
            cc_session_header: true,
            ..row(
                "S4 Claude Code floor, cloak always, Claude Code session",
                Lane::OwnOauth,
                cc,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Never,
            ..row(
                "S4 Claude Code floor, cloak never",
                Lane::OwnOauth,
                cc,
                false,
            )
        },
        row(
            "S4 Claude Code floor, api-key on the host",
            Lane::ApiKeyOnHost,
            cc,
            false,
        ),
        row(
            "S4 Claude Code floor, forwarded",
            Lane::Forwarded,
            cc,
            false,
        ),
    ]
}

fn context_management_rows() -> Vec<Row> {
    let cm = context_management::CONTEXT_MANAGEMENT_BETA;
    vec![
        Row {
            context_management: true,
            ..row("S5 strip on, Claude Code floor", Lane::OwnOauth, cm, false)
        },
        row("S5 strip off, Claude Code floor", Lane::OwnOauth, cm, true),
        Row {
            context_management: true,
            provider_pin: true,
            ..row("S5 strip on, provider pin", Lane::ApiKey, cm, false)
        },
        Row {
            provider_pin: true,
            ..row("S5 strip off, provider pin", Lane::ApiKey, cm, true)
        },
        Row {
            context_management: true,
            provider_pin: true,
            ..row(
                "S5 strip on, provider pin, forwarded",
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
                "S6 structured outputs, api-key, format",
                Lane::ApiKey,
                so,
                true,
            )
        },
        Row {
            body: Some(json!({"model": "m"})),
            ..row(
                "S6 structured outputs, api-key, no format",
                Lane::ApiKey,
                so,
                false,
            )
        },
        Row {
            body: Some(format),
            ..row(
                "S6 structured outputs, forwarded, format",
                Lane::Forwarded,
                so,
                false,
            )
        },
        Row {
            body: Some(effort.clone()),
            ..row(
                "S7 effort, own oauth-bearer, effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Always,
            body: Some(effort.clone()),
            ..row(
                "S7 effort, own oauth-bearer, cloak always, effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                true,
            )
        },
        Row {
            cloak_mode: CloakMode::Never,
            body: Some(effort.clone()),
            ..row(
                "S7 effort, own oauth-bearer, cloak never, effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                true,
            )
        },
        Row {
            body: Some(json!({"model": "m"})),
            ..row(
                "S7 effort, own oauth-bearer, no effort",
                Lane::OwnOauth,
                EFFORT_BETA,
                false,
            )
        },
        Row {
            body: Some(effort.clone()),
            ..row(
                "S7 effort, api-key on the host, effort",
                Lane::ApiKeyOnHost,
                EFFORT_BETA,
                false,
            )
        },
        Row {
            body: Some(effort),
            ..row(
                "S7 effort, forwarded, effort",
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
                "S8 Claude Code floor, no display",
                Lane::OwnOauth,
                redact,
                true,
            )
        },
        Row {
            body: Some(display.clone()),
            ..row(
                "S8 Claude Code floor, display",
                Lane::OwnOauth,
                redact,
                false,
            )
        },
        Row {
            provider_pin: true,
            body: Some(no_display),
            ..row(
                "S8 provider pin, api-key, no display",
                Lane::ApiKey,
                redact,
                true,
            )
        },
        Row {
            provider_pin: true,
            body: Some(display.clone()),
            ..row(
                "S8 provider pin, api-key, display",
                Lane::ApiKey,
                redact,
                false,
            )
        },
        Row {
            provider_pin: true,
            body: Some(display),
            ..row(
                "S8 provider pin, forwarded, display",
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
