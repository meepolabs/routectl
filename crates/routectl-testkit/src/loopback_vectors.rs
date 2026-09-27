//! Shared base-URL vectors for the loopback host classification.
//!
//! Two crates classify a base URL's host as loopback independently: the
//! router's cleartext-`http://` admission check and the providers crate's
//! proxy-bypass decision. Each keeps its predicate private, so both suites
//! drive these same vectors to keep the two classifications from drifting.
//!
//! Every entry is an `http://` URL; a suite that needs the `https://` form
//! swaps the scheme. No entry is link-local, since the router refuses those
//! for a different reason.

/// Base URLs whose parsed host is loopback: the exact name `localhost` (in
/// any letter case), an IPv4 literal in `127.0.0.0/8` in any spelling the
/// URL parser normalizes, native `::1`, and IPv4-mapped loopback.
pub const LOOPBACK_BASE_URLS: &[&str] = &[
    "http://localhost/",
    "http://LOCALHOST:8080/",
    "http://LocalHost/v1",
    "http://user:pass@localhost:8080/",
    "http://127.0.0.1:8080/",
    "http://127.0.0.5/",
    "http://127.255.255.254/",
    "http://127.1/",
    "http://2130706433/",
    "http://user:pass@127.0.0.1:8080/v1",
    "http://[::1]/",
    "http://[0:0:0:0:0:0:0:1]:8080/v1",
    "http://[::ffff:127.0.0.1]:8080/v1",
    "http://[::ffff:127.0.0.5]/",
    "http://[::ffff:7f00:1]/",
];

/// Base URLs whose parsed host is NOT loopback: DNS names that merely look
/// like loopback, the deprecated IPv4-compatible form (`::a.b.c.d`, which
/// may follow an ordinary IPv6 route), and ordinary remote addresses.
pub const NON_LOOPBACK_BASE_URLS: &[&str] = &[
    "http://127.evil.example/",
    "http://127.0.0.1.nip.io/v1",
    "http://127.0.0.x/",
    "http://127../",
    "http://127.0.0.1%2eevil.example/",
    "http://localhost./",
    "http://api.localhost/",
    "http://localhost.localdomain/",
    "http://[::127.0.0.1]/",
    "http://[::7f00:1]:8080/",
    "http://128.0.0.1/",
    "http://10.0.0.1/",
    "http://0.0.0.0/",
    "http://[::]/",
    "http://[::2]/",
    "http://[::ffff:10.0.0.1]/",
    "http://[2001:db8::1]/",
    "http://api.example.test/v1",
];
