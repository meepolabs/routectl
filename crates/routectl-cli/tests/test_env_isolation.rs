//! Pins the cargo-level test sandbox: the workspace `.cargo/config.toml`
//! forces `XDG_CONFIG_HOME` to `<workspace>/target/test-xdg` for every process
//! cargo runs, so a default config path resolved under `cargo test` lands in
//! the build tree instead of the operator's real `$HOME/.config/routectl`.
//!
//! A caller may replace that forced entry through cargo's
//! `CARGO_ENV_XDG_CONFIG_HOME` override (the live-gate isolation script does,
//! per leg); cargo hands its value to the test as `XDG_CONFIG_HOME`, and the
//! override variable itself is inherited, so the expected root follows it.
//! The override drops the `force` flag along with the value, so an
//! `XDG_CONFIG_HOME` already exported in the shell wins over it; set both to
//! the same directory. Check the override path with:
//!
//! ```text
//! XDG_CONFIG_HOME=<scratch dir> CARGO_ENV_XDG_CONFIG_HOME=<scratch dir> \
//!     cargo test -p routectl-cli --test test_env_isolation
//! ```
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
    std::env::var_os("CARGO_ENV_XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || workspace_root().join("target").join("test-xdg"),
            PathBuf::from,
        )
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
