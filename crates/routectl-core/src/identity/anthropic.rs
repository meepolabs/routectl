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

/// Prefix Claude Code's own `User-Agent` carries ahead of its version:
/// `claude-cli/<version> (external, <surface>)`. The single source of
/// truth for that literal, shared by the minted User-Agent below and by
/// every reader here, so a composer and a reader can never disagree
/// about the shape.
const CLAUDE_CLI_UA_PREFIX: &str = "claude-cli/";

/// Surface token routectl mints in its own `User-Agent` parenthetical.
/// A fingerprint dimension in its own right, named so a reader can pin
/// it without re-typing the composed string.
pub const MINTED_UA_SURFACE: &str = "cli";

/// Upper bound on the whole version token the STRICT parser will return.
/// The returned value becomes a dedup-set key and a log field, so it is
/// bounded here rather than at each caller. Three 8-digit components plus
/// their two separators is 26; 32 leaves headroom without admitting a
/// value worth truncating.
const MAX_CLI_VERSION_TOKEN_LEN: usize = 32;

/// Upper bound on the digits in one version component. A real component
/// is 1-3 digits; 8 admits any plausible future numbering while refusing
/// a value built to grow a key.
const MAX_CLI_VERSION_COMPONENT_DIGITS: usize = 8;

/// Components a stable version carries: `major.minor.patch`.
const CLI_VERSION_COMPONENTS: usize = 3;

/// The compiled Claude Code CLI version routectl mints. Read by any
/// caller that must compare a version observed on the wire against the
/// one this build presents, so the comparison never re-types the literal.
pub const fn compiled_claude_cli_version() -> &'static str {
    CLAUDE_CLI_VERSION
}

/// LOOSE read: the first whitespace-delimited token after the
/// `claude-cli/` prefix, whatever it is -- a stable version, a
/// prerelease, a build-suffixed value, or an opaque string.
///
/// This is the shape a caller wants when it compares against a string a
/// human supplied: an operator's recorded tested-version is matched
/// verbatim, so narrowing what counts as a token here would silently stop
/// warning about the very drift they asked to be told about. Callers that
/// key durable state on the result want [`parse_claude_cli_version`]
/// instead, which refuses the values that vary per request.
///
/// `None` for a value that does not START with the prefix, or that
/// carries no non-whitespace token after it.
pub fn claude_cli_ua_token(user_agent: &str) -> Option<&str> {
    let token = user_agent
        .strip_prefix(CLAUDE_CLI_UA_PREFIX)?
        .split_whitespace()
        .next()?;
    (!token.is_empty()).then_some(token)
}

/// STRICT read: a stable `major.minor.patch` Claude Code CLI version, or
/// nothing.
///
/// Accepts only the bounded stable shape at the START of the value:
/// exactly 3 components (`major.minor.patch`), each 1 to 8 ASCII digits
/// with no leading zero (except a bare `0`), the whole token at most 32
/// bytes, ending at a whitespace boundary. The version may be followed by
/// further platform detail.
///
/// Every refusal has a caller-visible reason. A build or prerelease
/// suffix is what a per-request billing attribution token carries, so a
/// guard deduping on it would re-warn as that suffix changed. A leading
/// zero gives one version two spellings, which a string-keyed set would
/// track as two clients. The length bounds hold because the returned
/// value is stored and logged. Panic-free by construction: it only
/// slices at ASCII boundaries the prefix strip and `split_whitespace`
/// already established.
///
/// `None` is indistinguishable from an absent header to every caller --
/// no version was observed.
pub fn parse_claude_cli_version(user_agent: &str) -> Option<&str> {
    let rest = user_agent.strip_prefix(CLAUDE_CLI_UA_PREFIX)?;
    // A token must start immediately: whitespace here means the client
    // sent a prefix with no version attached to it.
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let token = rest.split_whitespace().next()?;
    if token.is_empty() || token.len() > MAX_CLI_VERSION_TOKEN_LEN {
        return None;
    }

    let mut components = 0usize;
    for component in token.split('.') {
        // Refuse a fourth component before inspecting it, so a long
        // dotted string is rejected on its shape rather than scanned.
        components += 1;
        if components > CLI_VERSION_COMPONENTS {
            return None;
        }
        if component.is_empty() || component.len() > MAX_CLI_VERSION_COMPONENT_DIGITS {
            return None;
        }
        if !component.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if component.len() > 1 && component.starts_with('0') {
            return None;
        }
    }
    (components == CLI_VERSION_COMPONENTS).then_some(token)
}

/// Upper bound on the surface token [`parse_claude_cli_ua_surface`] will
/// return. The value becomes a pinned observation and a comparison key, so
/// it is bounded at the parse; the real tokens are 3 and 7 bytes.
const MAX_UA_SURFACE_LEN: usize = 32;

/// The SURFACE token a Claude CLI `User-Agent` names in its
/// parenthetical: the `sdk-cli` in `claude-cli/2.1.246 (external,
/// sdk-cli)`.
///
/// Its own dimension, deliberately separate from the version: a client
/// and routectl can agree on one and differ on the other, and a caller
/// pinning only the version must not read as having pinned the whole
/// User-Agent.
///
/// Accepts only the shape a Claude CLI actually sends: the `claude-cli/`
/// prefix at the START, then a `(<origin>, <surface>)` parenthetical with
/// both halves non-empty, the surface at most 32 bytes drawn from ASCII
/// alphanumerics plus `-` and `_`. Surrounding whitespace is trimmed; the
/// comma may have none.
///
/// Every requirement earns its place. Without the prefix, any browser
/// `User-Agent` -- `Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)` and
/// its many commas -- would yield a "surface" that then landed in a pinned
/// observation. The charset and the length bound keep a path, a URL, or
/// non-ASCII bytes from becoming a comparison key, and they are also what
/// refuses an AMBIGUOUS multi-field parenthetical: a second comma lands
/// inside the surface half, where the charset rejects it, so a value
/// carrying more fields than this shape names is never resolved by
/// guessing which field was meant. `None` for anything else --
/// indistinguishable, to every caller, from a client that reported no
/// surface at all.
pub fn parse_claude_cli_ua_surface(user_agent: &str) -> Option<&str> {
    let after_prefix = user_agent.strip_prefix(CLAUDE_CLI_UA_PREFIX)?;
    let parenthetical = after_prefix.split_once('(')?.1.split_once(')')?.0;

    let (origin, surface) = parenthetical.split_once(',')?;
    if origin.trim().is_empty() {
        return None;
    }

    let surface = surface.trim();
    if surface.is_empty() || surface.len() > MAX_UA_SURFACE_LEN {
        return None;
    }
    // Also the multi-field refusal: a second comma is not in this charset.
    if !surface
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(surface)
}

/// Stainless SDK package version stamped in `x-stainless-package-version`.
const STAINLESS_PACKAGE_VERSION: &str = "0.94.0";

/// Stainless JS runtime version stamped in `x-stainless-runtime-version`.
const STAINLESS_RUNTIME_VERSION: &str = "v24.3.0";

/// The inbound header whose PRESENCE identifies a genuine Claude Code client.
///
/// A real Claude Code client always sends it; a non-CC client routectl cloaks
/// as one never does. Every consumer that classifies a request reads THIS
/// literal, so a rename cannot leave one arm scanning for the old spelling
/// while another scans for the new -- the two would then disagree about what
/// the same request is, and a beta floor or cloak decision would be applied
/// to one side of a request and not the other.
pub const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

/// Whether `headers` carries a genuine Claude Code session capture.
///
/// THE presence predicate. Case-insensitive on the header name because the
/// capture is taken from inbound HTTP, where the name's case is not guaranteed.
///
/// Takes the captured pairs -- the exact shape of
/// `RoutectlInternal::claude_code_headers` -- rather than a whole request, so
/// the router (which classifies an admitted request at activation) and the
/// Anthropic egress (which classifies the request it is about to send) call the
/// same function over the same data, neither re-deriving the scan. Concrete
/// rather than generic over `AsRef<str>`: there is exactly one carrier, and a
/// generic parameter would let a caller pass some other pair list that merely
/// resembles it.
#[must_use]
pub fn has_claude_code_session(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(CLAUDE_CODE_SESSION_HEADER))
}

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
    UA.get_or_init(|| {
        format!("{CLAUDE_CLI_UA_PREFIX}{CLAUDE_CLI_VERSION} (external, {MINTED_UA_SURFACE})")
    })
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
    fn compiled_claude_cli_version_is_the_minted_literal() {
        assert_eq!(compiled_claude_cli_version(), CLAUDE_CLI_VERSION);
        assert!(
            default_claude_code_user_agent().contains(compiled_claude_cli_version()),
            "the accessor and the minted User-Agent must read one constant"
        );
    }

    #[test]
    fn parse_extracts_the_version_from_a_claude_cli_user_agent() {
        assert_eq!(
            parse_claude_cli_version("claude-cli/2.1.246 (external, sdk-cli)"),
            Some("2.1.246")
        );
        assert_eq!(
            parse_claude_cli_version("claude-cli/2.1.169 (external, cli)"),
            Some("2.1.169")
        );
        // The whole value may be just the prefix + version, with no
        // trailing platform detail.
        assert_eq!(
            parse_claude_cli_version("claude-cli/2.1.263"),
            Some("2.1.263")
        );
    }

    #[test]
    fn parse_accepts_the_minted_user_agent_routectl_itself_emits() {
        // Positive control for the reject cases below: the shape routectl
        // puts on the wire must parse, or the rejects prove nothing.
        assert_eq!(
            parse_claude_cli_version(default_claude_code_user_agent()),
            Some(CLAUDE_CLI_VERSION)
        );
    }

    #[test]
    fn parse_rejects_a_non_claude_cli_user_agent() {
        assert_eq!(parse_claude_cli_version("Mozilla/5.0 (X11; Linux)"), None);
        assert_eq!(parse_claude_cli_version("anthropic-sdk/0.112.1"), None);
        assert_eq!(parse_claude_cli_version(""), None);
        // The prefix must be at the START; a value that merely contains it
        // is a different client quoting one.
        assert_eq!(
            parse_claude_cli_version("proxy/1.0 (claude-cli/2.1.246)"),
            None
        );
    }

    #[test]
    fn parse_rejects_a_version_token_that_is_not_a_stable_release() {
        // A build/prerelease suffix is exactly what the billing block's
        // per-request `cc_version` carries; the stable UA token never does,
        // so accepting one here would make an unstable value look stable.
        assert_eq!(parse_claude_cli_version("claude-cli/2.1.246.1e8"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2.1.246-beta.1"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2.1.246+build"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/abc"), None);
    }

    #[test]
    fn parse_rejects_a_malformed_or_empty_version_token() {
        assert_eq!(parse_claude_cli_version("claude-cli/"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/ (external)"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2."), None);
        assert_eq!(parse_claude_cli_version("claude-cli/.2"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2..1"), None);
        // Non-ASCII digits must not pass as numeric components.
        assert_eq!(parse_claude_cli_version("claude-cli/2.1.\u{0664}"), None);
    }

    #[test]
    fn the_strict_parser_bounds_the_token_it_will_return() {
        // The returned value becomes a dedup-set key and a log field, so its
        // size is bounded at the parse rather than at every caller.
        let overlong_component = format!("claude-cli/2.1.{}", "9".repeat(9));
        assert_eq!(parse_claude_cli_version(&overlong_component), None);
        let at_the_component_bound = format!("claude-cli/2.1.{}", "9".repeat(8));
        assert_eq!(
            parse_claude_cli_version(&at_the_component_bound).map(str::len),
            Some(12),
            "a component at the bound is still a version"
        );

        // The TOTAL bound is the outer guard: it refuses an oversized token
        // before the component walk runs at all, so a long dotted string is
        // rejected on its size rather than scanned component by component.
        let long_dotted = "1.2.3.4.5.6.7.8.9.10.11.12.13.14.15.16.17";
        assert!(
            long_dotted.len() > MAX_CLI_VERSION_TOKEN_LEN,
            "control: this token must exceed the total bound"
        );
        assert_eq!(
            parse_claude_cli_version(&format!("claude-cli/{long_dotted}")),
            None
        );
    }

    #[test]
    fn the_strict_parser_rejects_more_than_three_components_without_scanning_further() {
        assert_eq!(parse_claude_cli_version("claude-cli/1.2.3.4"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/1.2.3.4.5.6.7.8"), None);
    }

    #[test]
    fn the_strict_parser_rejects_a_leading_zero_but_accepts_a_bare_zero() {
        // A leading zero makes two spellings of one version, so a dedup set
        // keyed on the string would track them as different clients.
        assert_eq!(parse_claude_cli_version("claude-cli/02.1.3"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2.01.3"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2.1.03"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/2.1.00"), None);
        // A single zero is the only spelling of zero, so it is legal.
        assert_eq!(parse_claude_cli_version("claude-cli/0.0.0"), Some("0.0.0"));
        assert_eq!(parse_claude_cli_version("claude-cli/2.0.1"), Some("2.0.1"));
    }

    #[test]
    fn the_strict_parser_rejects_whitespace_where_the_version_should_start() {
        assert_eq!(parse_claude_cli_version("claude-cli/ 2.1.3"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/\t2.1.3"), None);
        assert_eq!(parse_claude_cli_version("claude-cli/\n2.1.3"), None);
    }

    #[test]
    fn the_strict_parser_accepts_the_real_shapes_both_clients_send() {
        // Paired real-shape accepts for the reject batches above: the
        // corpus's own surface token and the one routectl mints. A parser
        // that rejected these would pass every reject test and be useless.
        assert_eq!(
            parse_claude_cli_version("claude-cli/2.1.246 (external, sdk-cli)"),
            Some("2.1.246")
        );
        assert_eq!(
            parse_claude_cli_version("claude-cli/2.1.270 (external, cli)"),
            Some("2.1.270")
        );
    }

    #[test]
    fn the_loose_helper_returns_any_post_prefix_token_including_unstable_ones() {
        // The MITM guard's contract, preserved: it warns on whatever the
        // client put there, including a prerelease, a build suffix, or an
        // opaque token, because an operator's tested-version string is
        // compared verbatim.
        assert_eq!(
            claude_cli_ua_token("claude-cli/2.1.246 (external, sdk-cli)"),
            Some("2.1.246")
        );
        assert_eq!(
            claude_cli_ua_token("claude-cli/2.1.246.1e8 (external, cli)"),
            Some("2.1.246.1e8")
        );
        assert_eq!(
            claude_cli_ua_token("claude-cli/2.1.246-beta.1"),
            Some("2.1.246-beta.1")
        );
        assert_eq!(claude_cli_ua_token("claude-cli/abc"), Some("abc"));
    }

    #[test]
    fn the_loose_helper_still_requires_the_prefix_and_a_token() {
        assert_eq!(claude_cli_ua_token("claude-cli/"), None);
        assert_eq!(claude_cli_ua_token("claude-cli/   "), None);
        assert_eq!(claude_cli_ua_token("Mozilla/5.0 (X11; Linux)"), None);
        assert_eq!(claude_cli_ua_token(""), None);
        // Whitespace after the prefix is SKIPPED, not refused: the token is
        // whatever the client put first. This is the long-standing MITM
        // reading and is preserved deliberately -- that guard compares the
        // token against a string a human typed, so it must not decide for
        // itself which values are worth reporting. The strict parser refuses
        // this same value.
        assert_eq!(
            claude_cli_ua_token("claude-cli/ (external)"),
            Some("(external)")
        );
        assert_eq!(parse_claude_cli_version("claude-cli/ (external)"), None);
    }

    #[test]
    fn the_two_parsers_differ_exactly_on_token_stability() {
        // The loose helper accepts a superset. Where the strict one answers,
        // both must answer the SAME token -- otherwise one of them is
        // reading a different part of the value.
        for ua in [
            "claude-cli/2.1.246 (external, sdk-cli)",
            "claude-cli/2.1.246.1e8",
            "claude-cli/abc",
            "claude-cli/",
            "Mozilla/5.0",
        ] {
            if let Some(strict) = parse_claude_cli_version(ua) {
                assert_eq!(claude_cli_ua_token(ua), Some(strict));
            }
        }
        assert!(parse_claude_cli_version("claude-cli/2.1.246.1e8").is_none());
        assert!(claude_cli_ua_token("claude-cli/2.1.246.1e8").is_some());
    }

    #[test]
    fn the_minted_user_agent_surface_token_is_readable_on_its_own() {
        // The surface token is a fingerprint dimension in its own right: the
        // corpus client and routectl spell it differently, and the version
        // parse says nothing about it.
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external, sdk-cli)"),
            Some("sdk-cli")
        );
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.169 (external, cli)"),
            Some("cli")
        );
        assert_eq!(
            parse_claude_cli_ua_surface(default_claude_code_user_agent()),
            Some(MINTED_UA_SURFACE),
            "the minted surface token must be readable out of the minted UA"
        );
    }

    /// Paired accepts for the reject batches below: every shape a real
    /// Claude CLI is known to send must still parse, or the rejects prove
    /// nothing about the parser's usefulness.
    #[test]
    fn the_surface_parser_accepts_the_real_shapes_and_tolerates_spacing() {
        for (ua, expected) in [
            ("claude-cli/2.1.246 (external, sdk-cli)", "sdk-cli"),
            ("claude-cli/2.1.169 (external, cli)", "cli"),
            // No space after the comma, and extra padding around it: both
            // are the same self-report.
            ("claude-cli/2.1.246 (external,sdk-cli)", "sdk-cli"),
            ("claude-cli/2.1.246 (external,   cli)", "cli"),
            // A version this parser does not vet is still fine here: the
            // surface is its own dimension.
            ("claude-cli/2.1.246.1e8 (external, sdk-cli)", "sdk-cli"),
            // Trailing detail after the parenthetical does not matter.
            ("claude-cli/2.1.246 (external, cli) extra", "cli"),
        ] {
            assert_eq!(parse_claude_cli_ua_surface(ua), Some(expected), "ua={ua}");
        }
    }

    #[test]
    fn the_surface_parser_requires_the_claude_cli_prefix() {
        // A foreign client's parenthetical is not a Claude CLI surface
        // report, however comma-shaped it happens to be. Without the prefix
        // requirement, every browser UA below would yield a "surface".
        assert_eq!(
            parse_claude_cli_ua_surface(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36"
            ),
            None
        );
        assert_eq!(
            parse_claude_cli_ua_surface("Mozilla/5.0 (X11; Linux x86_64)"),
            None
        );
        assert_eq!(
            parse_claude_cli_ua_surface("some-proxy/1.0 (external, cli)"),
            None
        );
        // Prefix present but not at the START is a different client quoting
        // one, exactly as the two version readers treat it.
        assert_eq!(
            parse_claude_cli_ua_surface("proxy/1.0 (claude-cli/2.1.246 (external, cli))"),
            None
        );
    }

    #[test]
    fn the_surface_parser_requires_the_two_part_parenthetical() {
        // No parenthetical, unclosed, empty, one part, or an empty surface.
        assert_eq!(parse_claude_cli_ua_surface("claude-cli/2.1.246"), None);
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external, sdk-cli"),
            None
        );
        assert_eq!(parse_claude_cli_ua_surface("claude-cli/2.1.246 ()"), None);
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external)"),
            None
        );
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external, )"),
            None
        );
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (, cli)"),
            None,
            "an empty origin half is not the expected shape either"
        );
    }

    #[test]
    fn the_surface_parser_rejects_an_ambiguous_multi_part_surface() {
        // More than one comma means the value carries more fields than this
        // shape describes, and picking one would be a guess about which
        // field is the surface.
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external, cli, extra)"),
            None
        );
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external, cli,)"),
            None
        );
    }

    #[test]
    fn the_surface_parser_bounds_the_token_it_will_return() {
        // The returned value becomes a pinned observation and a comparison
        // key, so its size is bounded at the parse.
        let at_bound = format!("claude-cli/2.1.246 (external, {})", "a".repeat(32));
        assert_eq!(
            parse_claude_cli_ua_surface(&at_bound).map(str::len),
            Some(32),
            "a token at the bound is still a surface"
        );
        let over_bound = format!("claude-cli/2.1.246 (external, {})", "a".repeat(33));
        assert_eq!(parse_claude_cli_ua_surface(&over_bound), None);
    }

    #[test]
    fn the_surface_parser_allows_only_a_narrow_ascii_token() {
        // The charset admits `cli` / `sdk-cli` and their plausible
        // relatives, and nothing that could carry a path, a URL, a version
        // string with spaces, or non-ASCII bytes into a pinned observation.
        for ok in ["cli", "sdk-cli", "sdk_cli", "cli2", "SDK-CLI"] {
            let ua = format!("claude-cli/2.1.246 (external, {ok})");
            assert_eq!(parse_claude_cli_ua_surface(&ua), Some(ok));
        }
        for bad in [
            "sdk cli",
            "sdk/cli",
            "sdk.cli",
            "cli;drop",
            "cli\tx",
            "cl\u{00ed}",
            "cli\u{200b}",
        ] {
            let ua = format!("claude-cli/2.1.246 (external, {bad})");
            assert_eq!(
                parse_claude_cli_ua_surface(&ua),
                None,
                "surface {bad:?} must not be accepted"
            );
        }
    }

    #[test]
    fn an_unparseable_surface_is_none_rather_than_a_guess() {
        assert_eq!(parse_claude_cli_ua_surface("claude-cli/2.1.246"), None);
        assert_eq!(parse_claude_cli_ua_surface("claude-cli/2.1.246 ()"), None);
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external)"),
            None
        );
        assert_eq!(parse_claude_cli_ua_surface("Mozilla/5.0 (X11)"), None);
        assert_eq!(
            parse_claude_cli_ua_surface("claude-cli/2.1.246 (external, )"),
            None
        );
    }

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
