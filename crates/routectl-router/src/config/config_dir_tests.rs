use std::path::PathBuf;

use routectl_testkit::ScopedEnv;

use super::routectl_config_dir;

/// `#[serial_test::serial]` is the `ScopedEnv` contract. The guards are
/// dropped before asserting so the unset window stays as short as the call.
#[test]
#[serial_test::serial]
fn routectl_config_dir_is_relative_when_no_base_variable_is_set() {
    // Arrange + Act
    let dir = {
        let _xdg = ScopedEnv::unset("XDG_CONFIG_HOME");
        let _home = ScopedEnv::unset("HOME");
        routectl_config_dir()
    };

    // Assert
    assert_eq!(dir, PathBuf::from(".config").join("routectl"));
}
