//! The path defaults that live in the config folder are empty when no folder
//! resolves, never a path relative to the working directory.
//!
//! Its own test binary with one test: it rewrites `HOME`, `USERPROFILE` (the
//! Windows home fallback) and `XDG_CONFIG_HOME` for the whole process, and no
//! sibling test may read the environment while it does.

use std::path::PathBuf;

use routectl_router::{MitmConfig, UsageConfig};
use routectl_testkit::ScopedEnv;

#[test]
#[serial_test::serial]
fn folder_defaults_are_empty_without_an_absolute_home_or_xdg() {
    let unset = None;
    let rows: &[(&str, Option<&str>, Option<&str>)] = &[
        ("both unset", unset, unset),
        ("both relative", Some("rel/xdg"), Some("rel/home")),
        ("both empty", Some(""), Some("")),
    ];
    let mut failures = Vec::new();
    for (name, xdg, home) in rows {
        let _xdg = scoped("XDG_CONFIG_HOME", *xdg);
        let _home = scoped("HOME", *home);
        let _userprofile = scoped("USERPROFILE", None);

        let db_path = UsageConfig::default().db_path;
        let cert_dir = MitmConfig::default().cert_dir;

        if db_path != PathBuf::new() || cert_dir != PathBuf::new() {
            failures.push(format!(
                "{name}: db_path {db_path:?}, cert_dir {cert_dir:?}"
            ));
        }
    }

    // Positive control: an absolute XDG_CONFIG_HOME yields absolute defaults.
    let _xdg = scoped("XDG_CONFIG_HOME", Some("/abs/xdg"));
    let _home = scoped("HOME", None);
    let _userprofile = scoped("USERPROFILE", None);
    assert_eq!(
        UsageConfig::default().db_path,
        PathBuf::from("/abs/xdg/routectl/usage.db")
    );
    assert_eq!(
        MitmConfig::default().cert_dir,
        PathBuf::from("/abs/xdg/routectl/mitm-certs")
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn scoped(key: &str, value: Option<&str>) -> ScopedEnv {
    match value {
        Some(value) => ScopedEnv::set(key, value),
        None => ScopedEnv::unset(key),
    }
}
