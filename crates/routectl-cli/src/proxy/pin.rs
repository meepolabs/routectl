//! The one host and origin the MITM front-proxy terminates and forwards
//! to. Fixed in code, never read from config: the proxy forwards the
//! client's full-scope claude.ai token, which must never reach a
//! non-Anthropic egress, so no config file can name another one.
//!
//! [`listener_config`] is the only production way to build a
//! [`ProxyListenerConfig`]: its fields are private to the listener module,
//! whose one production constructor reads the pair below.

use std::path::PathBuf;
use std::sync::Arc;

use super::listener::ProxyListenerConfig;

/// TLS SNI / `Host` the proxy terminates, and the CONNECT authority it
/// decrypts instead of blind-tunnelling. Compared exactly, never as a
/// suffix.
pub const MITM_HOST: &str = "api.anthropic.com";

/// Origin every forwarded (non-reinjected) request is sent to: https,
/// [`MITM_HOST`], default port, no path.
pub const MITM_UPSTREAM_ORIGIN: &str = "https://api.anthropic.com";

/// The listener inputs for a `[mitm]` block, with host and origin taken
/// from the pinned pair.
pub fn listener_config(
    listen_port: u16,
    cert_dir: PathBuf,
    reinject_port: u16,
    seam_nonce: Arc<crate::ingress::MitmSeamNonce>,
) -> ProxyListenerConfig {
    ProxyListenerConfig::pinned(listen_port, cert_dir, reinject_port, seam_nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_origin_is_https_on_the_pinned_host_with_nothing_else() {
        let url = reqwest::Url::parse(MITM_UPSTREAM_ORIGIN).expect("the pinned origin parses");

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some(MITM_HOST));
        assert_eq!(url.port(), None, "no explicit port");
        assert_eq!(url.username(), "");
        assert_eq!(url.password(), None);
        assert_eq!(url.path(), "/");
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), None);
        assert_eq!(
            MITM_UPSTREAM_ORIGIN,
            format!("https://{MITM_HOST}"),
            "spelled without a trailing path"
        );
    }

    #[test]
    fn listener_config_takes_host_and_origin_from_the_pin_and_the_rest_from_its_arguments() {
        let nonce = Arc::new(crate::ingress::MitmSeamNonce::generate());

        let config = listener_config(9443, PathBuf::from("/tmp/certs"), 9100, Arc::clone(&nonce));

        assert_eq!(config.mitm_host(), "api.anthropic.com");
        assert_eq!(config.upstream_origin(), "https://api.anthropic.com");
        assert_eq!(config.listen_port(), 9443);
        assert_eq!(config.cert_dir(), PathBuf::from("/tmp/certs"));
        assert_eq!(config.reinject_port(), 9100);
        assert!(Arc::ptr_eq(config.seam_nonce(), &nonce));
    }
}
