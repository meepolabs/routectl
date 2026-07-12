//! `routectl rc <subcommand>` -- `env` and `regen-ca`, the thin CLI
//! surface over the `[mitm]` front-proxy config. `env` prints the two
//! env vars an operator needs to point a first-party client (e.g. Claude
//! Code) at the local MITM listener; `regen-ca` forces a CA rotation.
//!
//! Deliberately a plain printer: no rich formatting, no colors, no shell
//! detection, no flags. A future `routectl setup claude-code` supersedes
//! this with a richer onboarding flow; this
//! command stays minimal until then.

use routectl_core::{Error, Result};
use routectl_router::{Config, MitmConfig};

use crate::proxy::ca;

const NOT_CONFIGURED_MSG: &str =
    "mitm passthrough not configured: add a [mitm] block to your config to enable it";

/// Print `HTTPS_PROXY` and `NODE_EXTRA_CA_CERTS` for the configured MITM
/// listener. Returns the process exit code: `0` when `[mitm]` is present
/// (the lines are printed to stdout), non-zero otherwise (a clear
/// not-configured message goes to stderr and nothing is printed to
/// stdout -- never a bogus/default value).
pub fn env(config: &Config) -> Result<i32> {
    let Some(mitm) = &config.mitm else {
        eprintln!("{NOT_CONFIGURED_MSG}");
        return Ok(1);
    };
    for line in render_env(mitm) {
        println!("{line}");
    }
    Ok(0)
}

/// Pure line-builder for `rc env`, split out so the output shape is
/// unit-testable without a real config file or a minted CA on disk.
fn render_env(mitm: &MitmConfig) -> Vec<String> {
    let ca_path = ca::ca_cert_path(&mitm.cert_dir);
    vec![
        format!("HTTPS_PROXY=http://127.0.0.1:{}", mitm.listen_port),
        format!("NODE_EXTRA_CA_CERTS={}", ca_path.display()),
    ]
}

/// Force a CA + leaf re-mint via `proxy::ca::regenerate` and report the
/// new CA cert path. Safe to re-run: `regenerate` always re-mints
/// cleanly, overwriting whatever material was there. Never prints or
/// logs key material -- only the CA certificate's path.
pub fn regen_ca(config: &Config) -> Result<i32> {
    let Some(mitm) = &config.mitm else {
        eprintln!("{NOT_CONFIGURED_MSG}");
        return Ok(1);
    };
    let ca_path = ca::regenerate(&mitm.cert_dir, &mitm.mitm_host)
        .map_err(|e| Error::Internal(format!("mitm CA regeneration failed: {e}")))?;
    println!("regenerated MITM CA: {}", ca_path.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn mitm_config(cert_dir: PathBuf) -> MitmConfig {
        MitmConfig {
            upstream_origin: "https://api.anthropic.com".into(),
            listen_port: 8788,
            cert_dir,
            mitm_host: "api.anthropic.com".into(),
            tested_cc_version: None,
        }
    }

    #[test]
    fn render_env_prints_https_proxy_and_ca_cert_path() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let mitm = mitm_config(dir.path().to_path_buf());

        // Act
        let lines = render_env(&mitm);

        // Assert: exactly the two plain lines, no export prefix, no
        // decoration, the CA path matches ca::ca_cert_path.
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "HTTPS_PROXY=http://127.0.0.1:8788");
        let expected_ca_path = ca::ca_cert_path(dir.path());
        assert_eq!(
            lines[1],
            format!("NODE_EXTRA_CA_CERTS={}", expected_ca_path.display())
        );
        for line in &lines {
            assert!(!line.starts_with("export "), "line must not export: {line}");
        }
    }

    #[test]
    fn env_returns_nonzero_when_mitm_not_configured() {
        // Arrange
        let config = Config::default();
        assert!(config.mitm.is_none());

        // Act
        let code = env(&config).unwrap();

        // Assert
        assert_ne!(code, 0);
    }

    #[test]
    fn regen_ca_returns_nonzero_when_mitm_not_configured() {
        // Arrange
        let config = Config::default();

        // Act
        let code = regen_ca(&config).unwrap();

        // Assert
        assert_ne!(code, 0);
    }

    #[test]
    fn regen_ca_is_idempotent_safe_to_rerun() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            mitm: Some(mitm_config(dir.path().to_path_buf())),
            ..Default::default()
        };

        // Act: run twice. The first call also covers "mints and reports
        // a valid CA path" -- a separate single-call test would only
        // duplicate this assertion.
        let first = regen_ca(&config).unwrap();
        let second = regen_ca(&config).unwrap();

        // Assert
        assert_eq!(first, 0);
        assert_eq!(second, 0);
        assert!(ca::ca_cert_path(dir.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn regen_ca_wraps_ca_error_as_internal_error() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        // Arrange: cert_dir's parent is unwritable, so ca::regenerate
        // cannot even create cert_dir, forcing a CaError::CreateDir.
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let cert_dir = parent.path().join("certs");
        let config = Config {
            mitm: Some(mitm_config(cert_dir)),
            ..Default::default()
        };

        // Act
        let result = regen_ca(&config);

        // Restore write permission before the tempdir's Drop cleans up,
        // regardless of the assertion outcome.
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();

        // Assert: the CaError is preserved via Display in an
        // Error::Internal, not silently dropped or panicked on.
        match result {
            Err(Error::Internal(msg)) => {
                assert!(
                    msg.contains("mitm CA regeneration failed"),
                    "expected wrapped CaError context, got: {msg}"
                );
            }
            other => panic!("expected Err(Error::Internal(_)), got: {other:?}"),
        }
    }
}
