use super::*;

#[test]
fn is_loopback_covers_full_127_range() {
    // Arrange + Act + Assert
    assert!(is_loopback("127.0.0.1"));
    assert!(is_loopback("127.0.0.2"));
    assert!(is_loopback("127.255.255.254"));
    assert!(is_loopback("::1"));
    assert!(is_loopback("localhost"));
    assert!(!is_loopback("0.0.0.0"));
    assert!(!is_loopback("192.168.1.1"));
    assert!(!is_loopback("not-an-address"));
}

#[test]
fn is_loopback_handles_ipv4_mapped_ipv6() {
    // Arrange + Act + Assert: IPv4-mapped IPv6 addresses
    // (::ffff:127.x.x.x) must be treated as loopback; non-loopback
    // IPv4-mapped addresses must not be.
    assert!(is_loopback("::ffff:127.0.0.1"));
    assert!(!is_loopback("::ffff:192.168.1.1"));
}
