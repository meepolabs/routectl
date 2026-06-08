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
//! `anthropic-beta` is intentionally NOT a default: it feeds the
//! three-source beta compose (ingress + provider + model) handled in
//! the egress `build_headers`, so it stays an explicit `header_extras`
//! entry for operators who need it.
//!
//! The version literals below are the "ship with routectl, bump each
//! release" values. Roll them forward when the upstream Claude Code SDK
//! advances so the wire fingerprint stays current.

/// Claude Code CLI version routectl mimics in the default User-Agent.
const CLAUDE_CLI_VERSION: &str = "2.1.167";

/// Stainless SDK package version stamped in `x-stainless-package-version`.
const STAINLESS_PACKAGE_VERSION: &str = "0.94.0";

/// Stainless JS runtime version stamped in `x-stainless-runtime-version`.
const STAINLESS_RUNTIME_VERSION: &str = "v24.3.0";

/// Default `User-Agent` for the OauthBearer surface. Used as the
/// client-level fallback in `AnthropicApiProvider::new()` when the
/// operator leaves `user_agent` unset on an oauth-bearer provider.
/// Composed from `CLAUDE_CLI_VERSION` so a single constant drives both
/// the UA and any future version-keyed default. Computed once per
/// process; subsequent calls return the cached value.
pub fn default_claude_code_user_agent() -> &'static str {
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    UA.get_or_init(|| format!("claude-cli/{CLAUDE_CLI_VERSION} (external, sdk-cli)"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
