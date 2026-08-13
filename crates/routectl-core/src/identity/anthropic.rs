//! Compiled Claude Code SDK identity-header defaults -- the `anthropic`
//! half of the provider identity module.
//!
//! These ship with routectl and fire by default on the `oauth-bearer`
//! path so a zero-config operator (auth_kind + api_key_ref only) emits
//! the Stainless SDK fingerprint that api.anthropic.com associates with
//! a Claude Code subscription client, without hand-listing every header
//! in `header_extras`. An operator `header_extras` entry for any of
//! these keys OVERRIDES the default (the build_headers loop inserts
//! after these).
//!
//! `anthropic-beta` is NOT among the `header_extras` identity defaults
//! (it feeds the three-source beta compose in egress `build_headers`).
//! However, a separate floor function --
//! `default_claude_code_anthropic_betas()` -- provides a pinned set of
//! beta flags that the composer merges on the OauthBearer +
//! api.anthropic.com surface before the context_management strip.
//!
//! The version literals below are the "ship with routectl, bump each
//! release" values. Roll them forward when the upstream Claude Code SDK
//! advances so the wire fingerprint stays current.

/// Claude Code CLI version routectl mimics in the default User-Agent.
const CLAUDE_CLI_VERSION: &str = "2.1.169";

/// Stainless SDK package version stamped in `x-stainless-package-version`.
const STAINLESS_PACKAGE_VERSION: &str = "0.94.0";

/// Stainless JS runtime version stamped in `x-stainless-runtime-version`.
const STAINLESS_RUNTIME_VERSION: &str = "v24.3.0";

/// The `anthropic-beta` flag required for OAuth to function on
/// api.anthropic.com. Egress unions this unconditionally on the
/// OauthBearer + api.anthropic.com surface, independent of whether the
/// request is genuine Claude Code or cloaked -- single source of truth so
/// the literal is never duplicated between this floor list and the
/// unconditional union in `build_headers`.
pub const OAUTH_ANTHROPIC_BETA: &str = "oauth-2025-04-20";

/// The `anthropic-beta` flag gating the 1M-token context window. NOT in
/// the floor: it is model-gated, so forcing it 400s on models that do not
/// support it. Reaches upstream only as client-driven pass-through
/// (subject to the ingress allowlist). Single source of truth shared by
/// the provider's `has_context_1m_beta` observability check, so a version
/// bump here can never drift out of sync with the sibling literal.
pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

/// The `anthropic-beta` flag gating mid-conversation system blocks. NOT in
/// the floor (model-gated, same rationale as `CONTEXT_1M_BETA`): reaches
/// upstream only as client-driven pass-through. Named so the provider's 4xx
/// pass-through diagnostics can match on it without re-typing the wire
/// string.
pub const MID_CONVERSATION_SYSTEM_BETA: &str = "mid-conversation-system-2026-04-07";

/// The `anthropic-beta` flag gating the advisor tool. NOT in the floor
/// (model-gated); client-driven pass-through only. Shared with the
/// provider's 4xx pass-through diagnostics.
pub const ADVISOR_TOOL_BETA: &str = "advisor-tool-2026-03-01";

/// The `anthropic-beta` flag gating thinking-token counting. NOT in the
/// floor (model-gated); client-driven pass-through only. Shared with the
/// provider's 4xx pass-through diagnostics.
pub const THINKING_TOKEN_COUNT_BETA: &str = "thinking-token-count-2026-05-13";

/// The `anthropic-beta` flag gating `output_config.effort`. Like
/// `STRUCTURED_OUTPUTS_BETA` it is a server-side capability requirement
/// rather than a client-opted beta: the egress unions it on-demand keyed on
/// the final body carrying `output_config.effort`
/// (`extras::union_effort_beta`), scoped to the OAuth own-anthropic lane. It
/// is NOT in the floor, because forcing it on a model that does not support
/// effort 400s the request.
pub const EFFORT_BETA: &str = "effort-2025-11-24";

/// The `anthropic-beta` flag gating `output_config.format` (structured
/// outputs). A server-side capability requirement, not a client-opted
/// beta: any Anthropic-shape body carrying `output_config.format` must
/// ship this flag or upstream rejects the field. Single source of truth
/// shared by the floor list below and the egress capability union, so the
/// two can never drift.
pub const STRUCTURED_OUTPUTS_BETA: &str = "structured-outputs-2025-12-15";

/// Default `User-Agent` for the OauthBearer surface. Used as the
/// client-level fallback in `AnthropicApiProvider::new()` when the
/// operator leaves `user_agent` unset on an oauth-bearer provider.
/// Composed from `CLAUDE_CLI_VERSION` so a single constant drives both
/// the UA and any future version-keyed default. Computed once per
/// process; subsequent calls return the cached value.
pub fn default_claude_code_user_agent() -> &'static str {
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    UA.get_or_init(|| format!("claude-cli/{CLAUDE_CLI_VERSION} (external, cli)"))
        .as_str()
}

/// Map `std::env::consts::ARCH` to the Stainless `x-stainless-arch`
/// wire value. Stainless emits Node's `process.arch` shape ("x64",
/// "arm64"), not Rust's target arch ("x86_64", "aarch64").
fn stainless_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Map `std::env::consts::OS` to the Stainless `x-stainless-os` wire
/// value. Stainless emits a capitalized OS name ("Linux", "MacOS",
/// "Windows"), not Rust's lowercase cfg string.
fn stainless_os() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "MacOS",
        "windows" => "Windows",
        other => other,
    }
}

/// Compiled Claude Code SDK identity-header defaults as `(name, value)`
/// pairs. The static SDK fingerprint plus the two dynamic, host-derived
/// entries (`x-stainless-arch`, `x-stainless-os`). Excludes
/// `anthropic-beta` (composed separately) and auth headers (injected by
/// the auth dispatcher).
pub fn default_claude_code_identity_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("x-app", "cli"),
        ("x-stainless-lang", "js"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-runtime-version", STAINLESS_RUNTIME_VERSION),
        ("x-stainless-package-version", STAINLESS_PACKAGE_VERSION),
        ("x-stainless-timeout", "600"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-arch", stainless_arch()),
        ("x-stainless-os", stainless_os()),
    ]
}

/// Pinned Claude Code beta flags forming a floor for the OauthBearer
/// surface when talking to api.anthropic.com. Merged into the composed
/// anthropic-beta header before the context_management strip, bypassing
/// the ingress allowlist (these are operator-equivalent pins).
///
/// 9 universally-supported flags forming the model-agnostic base every
/// non-CC OAuth request depends on. The 5 model-gated flags removed from
/// the old floor (context-1m, effort, thinking-token-count,
/// mid-conversation-system, advisor-tool) are deliberately EXCLUDED: they
/// flow through as client-driven pass-through (subject to the ingress
/// allowlist), so a model that rejects them never sees them forced by the
/// floor. (effort additionally gains an on-demand egress union keyed on
/// `output_config.effort` in `extras::union_effort_beta`, mirroring
/// structured-outputs.)
pub const fn default_claude_code_anthropic_betas() -> &'static [&'static str] {
    &[
        "claude-code-20250219",
        OAUTH_ANTHROPIC_BETA,
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
        STRUCTURED_OUTPUTS_BETA,
        "fast-mode-2026-02-01",
        "redact-thinking-2026-02-12",
        "token-efficient-tools-2026-03-28",
    ]
}

/// True when `base_url`'s host is EXACTLY `api.anthropic.com`
/// (case-insensitive), independent of scheme, port, path, query,
/// fragment, or `user:pass@` credentials.
///
/// The single source of truth for "is this the Anthropic host", shared by
/// every caller that must agree with the egress: the WIRE gate (which
/// decides whether to stamp the Claude-Code session identity headers), the
/// ROUTER pure-passthrough gate (which decides whether a forwarded request
/// may egress at all), the terminal-Anthropic envelope unwrap, and the
/// config/CLI surfaces. Because the answer gates first-party identity
/// treatment and a wire rewrite, it MUST match the host the request path
/// actually egresses to.
///
/// A precise host match, NOT a substring / suffix test:
/// `base_url.contains("api.anthropic.com")` would also match a
/// misconfigured `https://api.anthropic.com.evil.example` (sibling-domain
/// takeover), `https://proxy.example/api.anthropic.com` (host in the
/// path), or a credentials-suffix smuggle such as
/// `https://api.anthropic.com@evil.example`. An exact host match rejects
/// all of those.
///
/// Parses with `url::Url` -- the same parser the request path uses -- and
/// compares `host_str()` case-insensitively. Using the request parser is
/// what closes the divergence class BY CONSTRUCTION: a hand-rolled
/// authority split disagrees with the WHATWG URL rules the request path
/// follows (e.g. a backslash is a path separator under a special scheme),
/// so an authority like `https://evil.example\@api.anthropic.com/` egresses
/// to `evil.example` while a naive `@`-split would read it as the Anthropic
/// host. Invalid URLs, URLs with no host, and non-hierarchical URLs return
/// `false`. This does NOT rely on config-time validation: several callers
/// (core and provider APIs) reach the predicate directly, so it must be
/// self-sufficient.
pub fn is_anthropic_api_host(base_url: &str) -> bool {
    match url::Url::parse(base_url) {
        Ok(url) => url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.anthropic.com")),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_anthropic_api_host_matches_only_the_exact_host() {
        // Exact host, with and without a path / port, and any case, matches.
        assert!(is_anthropic_api_host("https://api.anthropic.com"));
        assert!(is_anthropic_api_host(
            "https://api.anthropic.com/v1/messages"
        ));
        assert!(is_anthropic_api_host("https://api.anthropic.com:443/v1"));
        assert!(is_anthropic_api_host("https://API.Anthropic.Com"));
        // A credentials prefix on the authority is stripped by the parser
        // before the host check, so it cannot be used to smuggle a
        // different real host.
        assert!(is_anthropic_api_host("https://user:pass@api.anthropic.com"));
        // Sibling-domain takeover, host-in-path/query/fragment, and a
        // credentials-suffix smuggle must NOT match.
        assert!(!is_anthropic_api_host(
            "https://api.anthropic.com.evil.example"
        ));
        assert!(!is_anthropic_api_host(
            "https://proxy.example/api.anthropic.com"
        ));
        assert!(!is_anthropic_api_host(
            "https://evil.example#api.anthropic.com"
        ));
        assert!(!is_anthropic_api_host(
            "https://evil.example?h=api.anthropic.com"
        ));
        assert!(!is_anthropic_api_host("https://anthropic.com"));
        assert!(!is_anthropic_api_host(
            "https://api.anthropic.com@evil.example"
        ));
    }

    #[test]
    fn backslash_authority_matches_the_url_parsers_egress_host() {
        // NEGATIVE CONTROL for the resolver-divergence fix. Under a special
        // scheme the WHATWG URL parser treats a backslash as a path
        // separator, so this authority egresses to `evil.example`. The old
        // hand-rolled split answered `true` here (it read the segment after
        // the last `@`), granting first-party Anthropic treatment to a
        // request that actually leaves for `evil.example`. The predicate
        // must now agree with the parser and return `false`.
        assert_eq!(
            url::Url::parse("https://evil.example\\@api.anthropic.com/")
                .unwrap()
                .host_str(),
            Some("evil.example"),
            "parser egress host is evil.example, not the Anthropic host"
        );
        assert!(!is_anthropic_api_host(
            "https://evil.example\\@api.anthropic.com/"
        ));
        assert!(!is_anthropic_api_host(
            "https://evil.example\\@api.anthropic.com:443/v1"
        ));

        // The mirror shape: the backslash makes `api.anthropic.com` the real
        // host, so the predicate must answer `true` -- matching the parser.
        assert_eq!(
            url::Url::parse("https://api.anthropic.com\\@evil.example/")
                .unwrap()
                .host_str(),
            Some("api.anthropic.com")
        );
        assert!(is_anthropic_api_host(
            "https://api.anthropic.com\\@evil.example/"
        ));
    }

    #[test]
    fn control_bytes_and_case_fold_track_the_parser() {
        // A tab inside the authority is stripped by the parser, so the real
        // host is `api.anthropic.com`: the predicate matches the parser.
        assert!(is_anthropic_api_host(
            "https://evil.example\t@api.anthropic.com"
        ));
        // Backslash truncates the authority, so the real host here is
        // `API.ANTHROPIC.COM` -- matched case-insensitively.
        assert!(is_anthropic_api_host(
            "https://API.ANTHROPIC.COM\\@evil.example"
        ));
        // A control byte before a backslash-truncated evil host still
        // resolves away from the Anthropic host.
        assert!(!is_anthropic_api_host(
            "https://evil.example\n\\@api.anthropic.com"
        ));
    }

    #[test]
    fn invalid_missing_host_and_non_hierarchical_urls_do_not_match() {
        // Unparseable / hostless / non-hierarchical inputs are never the
        // Anthropic host.
        assert!(!is_anthropic_api_host(""));
        assert!(!is_anthropic_api_host("https://"));
        assert!(!is_anthropic_api_host("not a url"));
        assert!(!is_anthropic_api_host("api.anthropic.com"));
        assert!(!is_anthropic_api_host("mailto:api.anthropic.com"));
        // An invalid port makes the whole URL unparseable -> false.
        assert!(!is_anthropic_api_host("https://api.anthropic.com:evil"));
    }

    #[test]
    fn ipv6_authority_does_not_match_the_anthropic_host() {
        assert!(!is_anthropic_api_host("https://[::1]"));
        assert!(!is_anthropic_api_host("https://[::1]:8080/v1"));
    }

    #[test]
    fn trailing_dot_does_not_match() {
        // A fully-qualified trailing-dot host is a DIFFERENT host string.
        assert!(!is_anthropic_api_host("https://api.anthropic.com."));
    }

    #[test]
    fn query_and_fragment_do_not_defeat_the_host_match() {
        // The host is read from the authority, so trailing query/fragment
        // components leave a genuine Anthropic URL matching.
        assert!(is_anthropic_api_host(
            "https://api.anthropic.com/v1?beta=1#frag"
        ));
    }

    #[test]
    fn user_agent_pins_claude_cli_version() {
        let ua = default_claude_code_user_agent();
        assert!(
            ua.contains(CLAUDE_CLI_VERSION),
            "UA must carry the pinned claude-cli version; got {ua}"
        );
        assert!(
            ua.starts_with("claude-cli/"),
            "UA must use the claude-cli/ prefix; got {ua}"
        );
    }

    #[test]
    fn defaults_carry_static_stainless_fingerprint() {
        let headers = default_claude_code_identity_headers();
        let lookup = |name: &str| headers.iter().find_map(|(n, v)| (*n == name).then_some(*v));
        assert_eq!(lookup("x-app"), Some("cli"));
        assert_eq!(lookup("x-stainless-lang"), Some("js"));
        assert_eq!(lookup("x-stainless-runtime"), Some("node"));
        assert_eq!(
            lookup("x-stainless-runtime-version"),
            Some(STAINLESS_RUNTIME_VERSION)
        );
        assert_eq!(
            lookup("x-stainless-package-version"),
            Some(STAINLESS_PACKAGE_VERSION)
        );
        assert_eq!(lookup("x-stainless-timeout"), Some("600"));
        assert_eq!(lookup("x-stainless-retry-count"), Some("0"));
    }

    #[test]
    fn defaults_omit_anthropic_beta() {
        let headers = default_claude_code_identity_headers();
        assert!(
            !headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta")),
            "anthropic-beta must NOT be a compiled default (it feeds the beta compose)",
        );
    }

    #[test]
    fn defaults_omit_dangerous_direct_browser_access() {
        let headers = default_claude_code_identity_headers();
        assert!(
            !headers
                .iter()
                .any(|(n, _)| *n == "anthropic-dangerous-direct-browser-access"),
            "anthropic-dangerous-direct-browser-access must NOT be sent on the \
             OAuth path -- real Claude Code omits it for OAuth and sends it only \
             in raw-API-key mode",
        );
    }

    #[test]
    fn dynamic_arch_and_os_map_to_stainless_shape() {
        let headers = default_claude_code_identity_headers();
        let lookup = |name: &str| headers.iter().find_map(|(n, v)| (*n == name).then_some(*v));
        let arch = lookup("x-stainless-arch").expect("arch present");
        let os = lookup("x-stainless-os").expect("os present");
        // Must never leak Rust's raw cfg strings.
        assert_ne!(arch, "x86_64", "arch must be mapped to Node shape");
        assert_ne!(arch, "aarch64", "arch must be mapped to Node shape");
        assert_ne!(os, "linux", "os must be mapped to capitalized shape");
        assert_ne!(os, "macos", "os must be mapped to capitalized shape");
    }

    #[test]
    fn anthropic_betas_floor_is_exactly_the_nine_base_flags() {
        let betas = default_claude_code_anthropic_betas();
        let expected = [
            "claude-code-20250219",
            "oauth-2025-04-20",
            "interleaved-thinking-2025-05-14",
            "context-management-2025-06-27",
            "prompt-caching-scope-2026-01-05",
            "structured-outputs-2025-12-15",
            "fast-mode-2026-02-01",
            "redact-thinking-2026-02-12",
            "token-efficient-tools-2026-03-28",
        ];
        assert_eq!(
            betas, expected,
            "floor must be exactly the 9 base flags in order"
        );

        // The 5 model-gated flags removed from the floor must never
        // return: forcing any of them 400s models that do not support it.
        let removed = [
            "context-1m-2025-08-07",
            "mid-conversation-system-2026-04-07",
            "advisor-tool-2026-03-01",
            "effort-2025-11-24",
            "thinking-token-count-2026-05-13",
        ];
        for flag in &removed {
            assert!(
                !betas.contains(flag),
                "floor must NOT contain removed model-gated flag {flag}; got: {betas:?}"
            );
        }
    }
}
