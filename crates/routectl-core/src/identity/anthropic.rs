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

/// The `anthropic-beta` flag gating the 1M-token context window. Single
/// source of truth shared by the floor list below and the provider's
/// `has_context_1m_beta` observability check, so a version bump here can
/// never drift out of sync with the sibling literal.
pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

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
/// 14 corpus-verified flags matching the set genuine Claude Code emits
/// on the OAuth egress (confirmed against a captured request corpus).
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
        CONTEXT_1M_BETA,
        "thinking-token-count-2026-05-13",
        "mid-conversation-system-2026-04-07",
        "advisor-tool-2026-03-01",
        "effort-2025-11-24",
    ]
}

/// True when `base_url`'s host is EXACTLY `api.anthropic.com`
/// (case-insensitive), independent of scheme, port, path, query,
/// fragment, or `user:pass@` credentials.
///
/// The single source of truth for "is this the Anthropic host" shared by
/// the WIRE gate (which decides whether to stamp the Claude-Code session
/// identity headers) and the ROUTER pure-passthrough gate (which decides
/// whether a forwarded request may egress at all). Both MUST agree, so
/// the predicate lives here rather than being reimplemented per crate.
///
/// A precise host match, NOT a substring / suffix test:
/// `base_url.contains("api.anthropic.com")` would also match a
/// misconfigured `https://api.anthropic.com.evil.example` (sibling-domain
/// takeover), `https://proxy.example/api.anthropic.com` (host in the
/// path), or a credentials-suffix smuggle such as
/// `https://api.anthropic.com@evil.example`. An exact host match rejects
/// all of those.
///
/// The host is the authority between the scheme and the first `/?#`, minus
/// any `user@` credentials and `:port`. Kept dependency-free (no `url`
/// crate) since the shape is fixed and validated upstream by base-url
/// scheme validation.
pub fn is_anthropic_api_host(base_url: &str) -> bool {
    let after_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Drop optional `user:pass@` credentials, then the optional `:port`.
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority);
    host.eq_ignore_ascii_case("api.anthropic.com")
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
        // A credentials prefix on the authority is stripped before the host
        // check, so it cannot be used to smuggle a different real host.
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
    fn anthropic_betas_floor_contains_all_fourteen_pinned_flags() {
        let betas = default_claude_code_anthropic_betas();
        assert_eq!(betas.len(), 14, "floor must carry exactly 14 pinned betas");
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
            "context-1m-2025-08-07",
            "thinking-token-count-2026-05-13",
            "mid-conversation-system-2026-04-07",
            "advisor-tool-2026-03-01",
            "effort-2025-11-24",
        ];
        for flag in &expected {
            assert!(
                betas.contains(flag),
                "floor must contain {flag}; got: {betas:?}"
            );
        }
    }
}
