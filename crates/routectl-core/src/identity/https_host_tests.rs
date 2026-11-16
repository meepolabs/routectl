use super::anthropic::{is_anthropic_api_host, is_anthropic_api_https_base};
use super::is_exact_https_host_in;

const HOSTS: &[&str] = &["api.anthropic.com"];

#[test]
fn accepts_the_exact_host_over_https_in_every_form_the_parser_normalizes() {
    let accepted = [
        ("bare", "https://api.anthropic.com"),
        ("trailing slash", "https://api.anthropic.com/"),
        ("path", "https://api.anthropic.com/v1"),
        ("mixed-case host", "https://API.Anthropic.Com"),
        ("uppercase scheme", "HTTPS://api.anthropic.com"),
        ("explicit default port", "https://api.anthropic.com:443"),
        (
            "query and fragment",
            "https://api.anthropic.com/v1?beta=1#f",
        ),
        (
            "backslash mirror",
            "https://api.anthropic.com\\@evil.example/",
        ),
    ];

    for (label, base) in accepted {
        assert!(is_exact_https_host_in(base, HOSTS), "{label}: {base}");
    }
}

#[test]
fn refuses_cleartext_userinfo_ports_and_foreign_hosts() {
    let refused = [
        ("plain http", "http://api.anthropic.com"),
        ("plain http uppercase", "HTTP://api.anthropic.com"),
        ("http on 443", "http://api.anthropic.com:443"),
        ("non-default port", "https://api.anthropic.com:8443"),
        ("userinfo", "https://user:pass@api.anthropic.com"),
        ("username only", "https://user@api.anthropic.com"),
        ("trailing dot", "https://api.anthropic.com."),
        ("non-ASCII look-alike", "https://api.anthrop\u{131}c.com"),
        ("sibling domain", "https://api.anthropic.com.evil.example"),
        (
            "credentials suffix",
            "https://api.anthropic.com@evil.example",
        ),
        (
            "backslash smuggle",
            "https://evil.example\\@api.anthropic.com/",
        ),
        ("host in path", "https://evil.example/api.anthropic.com"),
        ("other scheme", "wss://api.anthropic.com"),
        ("not a url", "api.anthropic.com"),
        ("empty", ""),
    ];

    for (label, base) in refused {
        assert!(!is_exact_https_host_in(base, HOSTS), "{label}: {base}");
    }
}

/// The host predicate stays scheme-blind while the credential predicate
/// refuses cleartext: the pair differs exactly on the scheme.
#[test]
fn the_anthropic_credential_base_requires_https_where_the_host_test_does_not() {
    for base in ["http://api.anthropic.com", "http://api.anthropic.com:8080"] {
        assert!(is_anthropic_api_host(base), "host test: {base}");
        assert!(
            !is_anthropic_api_https_base(base),
            "credential base: {base}"
        );
    }
    assert!(is_anthropic_api_https_base("https://api.anthropic.com"));
}
