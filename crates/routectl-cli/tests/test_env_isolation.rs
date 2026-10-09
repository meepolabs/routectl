//! Pins the cargo-level test sandbox: the workspace `.cargo/config.toml`
//! forces `XDG_CONFIG_HOME` to `<workspace>/target/test-xdg` for every process
//! cargo runs, so a default config path resolved under `cargo test` lands in
//! the build tree instead of the operator's real `$HOME/.config/routectl`.
//!
//! Its own binary: nothing here mutates the environment, and no sibling test
//! can set `XDG_CONFIG_HOME` underneath these reads.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate manifest dir sits two levels below the workspace root")
        .to_path_buf()
}

fn sandbox_dir() -> PathBuf {
    workspace_root().join("target").join("test-xdg")
}

#[test]
fn cargo_pins_xdg_config_home_to_the_workspace_sandbox() {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);

    assert_eq!(xdg, Some(sandbox_dir()));
}

#[test]
fn default_usage_db_path_resolves_inside_the_sandbox() {
    let db_path = routectl_router::UsageConfig::default().db_path;

    assert!(
        db_path.starts_with(sandbox_dir()),
        "default usage db `{}` must resolve under `{}`",
        db_path.display(),
        sandbox_dir().display()
    );
}

#[test]
fn default_usage_db_path_avoids_the_real_user_config_dir() {
    let home = std::env::var_os("HOME").expect("cargo test runs with HOME set");
    let real_config = PathBuf::from(home).join(".config");

    let db_path = routectl_router::UsageConfig::default().db_path;

    assert!(
        !db_path.starts_with(&real_config),
        "default usage db `{}` must not resolve under `{}`",
        db_path.display(),
        real_config.display()
    );
}
