use std::ffi::OsStr;
use std::path::PathBuf;

use super::config_dir_from;

/// One row: name, `XDG_CONFIG_HOME`, `HOME`, expected folder.
type Row = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<PathBuf>,
);

#[test]
fn config_dir_from_resolves_each_variable_combination() {
    let cases: [Row; 6] = [
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
