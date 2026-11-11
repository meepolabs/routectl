use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use routectl_testkit::ScopedEnv;

use super::{config_dir, config_dir_from, windows_home};

/// The filesystem root, so every fixture below is absolute on the OS that
/// runs it.
const ROOT: &str = if cfg!(windows) { "C:\\" } else { "/" };

/// `rel` under [`ROOT`].
fn abs(rel: &str) -> String {
    PathBuf::from(ROOT).join(rel).to_string_lossy().into_owned()
}

/// `rel` under [`ROOT`], with `routectl` joined on.
fn folder(rel: &str) -> Option<PathBuf> {
    Some(PathBuf::from(ROOT).join(rel).join("routectl"))
}

/// One row: name, `XDG_CONFIG_HOME`, `HOME`, expected folder.
type Row = (
    &'static str,
    Option<String>,
    Option<String>,
    Option<PathBuf>,
);

fn s(value: &str) -> Option<String> {
    Some(value.to_owned())
}

#[test]
fn config_dir_from_resolves_each_variable_combination() {
    let cases: [Row; 9] = [
        (
            "xdg set wins over home",
            Some(abs("xdg")),
            Some(abs("home/u")),
            folder("xdg"),
        ),
        (
            "xdg empty falls back to home",
            s(""),
            Some(abs("home/u")),
            folder("home/u/.config"),
        ),
        (
            "xdg unset falls back to home",
            None,
            Some(abs("home/u")),
            folder("home/u/.config"),
        ),
        ("home empty and xdg unset", None, s(""), None),
        ("both empty", s(""), s(""), None),
        ("both unset", None, None, None),
        (
            "xdg relative falls back to home",
            s("rel/xdg"),
            Some(abs("home/u")),
            folder("home/u/.config"),
        ),
        ("home relative and xdg unset", None, s("rel/home"), None),
        (
            "xdg absolute wins over a relative home",
            Some(abs("xdg")),
            s("rel/home"),
            folder("xdg"),
        ),
    ];

    for (name, xdg, home, expected) in cases {
        // Act
        let got = config_dir_from(
            xdg.as_deref().map(OsStr::new),
            home.as_deref().map(OsStr::new),
        );

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

/// `config_dir()` reads the process environment through the same rule.
/// `USERPROFILE` is cleared too: on Windows it stands in for an unset `HOME`.
#[test]
#[serial_test::serial]
fn config_dir_reads_the_process_environment() {
    let rows: [(&str, Option<String>, Option<PathBuf>); 2] = [
        ("absolute xdg", Some(abs("xdg")), folder("xdg")),
        ("xdg and home unset", None, None),
    ];

    for (name, xdg, expected) in rows {
        // Arrange
        let _xdg = match &xdg {
            Some(xdg) => ScopedEnv::set("XDG_CONFIG_HOME", xdg),
            None => ScopedEnv::unset("XDG_CONFIG_HOME"),
        };
        let _home = ScopedEnv::unset("HOME");
        let _userprofile = ScopedEnv::unset("USERPROFILE");

        // Act
        let got = config_dir();

        // Assert
        assert_eq!(got, expected, "row: {name}");
    }
}

/// One row: name, `HOME`, `USERPROFILE`, expected home.
type WindowsRow = (&'static str, Option<String>, Option<String>, Option<String>);

#[test]
fn windows_home_prefers_an_absolute_home_over_userprofile() {
    let rows: [WindowsRow; 5] = [
        (
            "absolute home wins",
            Some(abs("h")),
            Some(abs("p")),
            Some(abs("h")),
        ),
        (
            "empty home uses userprofile",
            s(""),
            Some(abs("p")),
            Some(abs("p")),
        ),
        (
            "relative home uses userprofile",
            s("rel"),
            Some(abs("p")),
            Some(abs("p")),
        ),
        (
            "unset home uses userprofile",
            None,
            Some(abs("p")),
            Some(abs("p")),
        ),
        ("both unset", None, None, None),
    ];

    for (name, home, userprofile, expected) in rows {
        // Act
        let got = windows_home(home.map(OsString::from), userprofile.map(OsString::from));

        // Assert
        assert_eq!(got, expected.map(OsString::from), "row: {name}");
    }
}
