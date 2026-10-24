use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use super::{config_dir_from, config_dir_or_relative_from, windows_home};

/// One row: name, `XDG_CONFIG_HOME`, `HOME`, expected folder.
type Row = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<PathBuf>,
);

#[test]
fn config_dir_from_resolves_each_variable_combination() {
    let cases: [Row; 9] = [
        (
            "xdg set wins over home",
            Some("/xdg"),
            Some("/home/u"),
            Some(PathBuf::from("/xdg/routectl")),
        ),
        (
            "xdg empty falls back to home",
            Some(""),
            Some("/home/u"),
            Some(PathBuf::from("/home/u/.config/routectl")),
        ),
        (
            "xdg unset falls back to home",
            None,
            Some("/home/u"),
            Some(PathBuf::from("/home/u/.config/routectl")),
        ),
        ("home empty and xdg unset", None, Some(""), None),
        ("both empty", Some(""), Some(""), None),
        ("both unset", None, None, None),
        (
            "xdg relative falls back to home",
            Some("rel/xdg"),
            Some("/home/u"),
            Some(PathBuf::from("/home/u/.config/routectl")),
        ),
        ("home relative and xdg unset", None, Some("rel/home"), None),
        (
            "xdg absolute wins over a relative home",
            Some("/xdg"),
            Some("rel/home"),
            Some(PathBuf::from("/xdg/routectl")),
        ),
    ];

    for (name, xdg, home, expected) in cases {
        // Act
        let got = config_dir_from(xdg.map(OsStr::new), home.map(OsStr::new));

        // Assert
        assert_eq!(got, expected, "row: {name}");
    }
}

#[cfg(unix)]
#[test]
fn config_dir_from_honors_non_utf8_xdg() {
    use std::os::unix::ffi::OsStrExt;

    // Arrange
    let xdg = OsStr::from_bytes(b"/x\xff");

    // Act
    let got = config_dir_from(Some(xdg), None);

    // Assert
    assert_eq!(got, Some(PathBuf::from(xdg).join("routectl")));
}

#[test]
fn config_dir_or_relative_from_falls_back_to_a_relative_folder() {
    let rows: [Row; 3] = [
        (
            "both unset",
            None,
            None,
            Some(PathBuf::from(".config/routectl")),
        ),
        (
            "both relative",
            Some("rel/xdg"),
            Some("rel/home"),
            Some(PathBuf::from(".config/routectl")),
        ),
        (
            "home set",
            None,
            Some("/home/u"),
            Some(PathBuf::from("/home/u/.config/routectl")),
        ),
    ];

    for (name, xdg, home, expected) in rows {
        // Act
        let got = config_dir_or_relative_from(xdg.map(OsStr::new), home.map(OsStr::new));

        // Assert
        assert_eq!(Some(got), expected, "row: {name}");
    }
}

/// One row: name, `HOME`, `USERPROFILE`, expected home.
type WindowsRow = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

#[test]
fn windows_home_prefers_an_absolute_home_over_userprofile() {
    let rows: [WindowsRow; 5] = [
        ("absolute home wins", Some("/h"), Some("/p"), Some("/h")),
        (
            "empty home uses userprofile",
            Some(""),
            Some("/p"),
            Some("/p"),
        ),
        (
            "relative home uses userprofile",
            Some("rel"),
            Some("/p"),
            Some("/p"),
        ),
        ("unset home uses userprofile", None, Some("/p"), Some("/p")),
        ("both unset", None, None, None),
    ];

    for (name, home, userprofile, expected) in rows {
        // Act
        let got = windows_home(home.map(OsString::from), userprofile.map(OsString::from));

        // Assert
        assert_eq!(got, expected.map(OsString::from), "row: {name}");
    }
}
