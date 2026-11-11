use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::*;

fn plant(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "version = 5\n").unwrap();
}

/// A home whose `Library` is a regular file, so every existence check under
/// the old folder fails with `NotADirectory` for any uid, root included.
#[cfg(unix)]
fn home_with_blocked_legacy_folder() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("Library"), "not a folder").unwrap();
    home
}

fn appdata_dir(path: &str) -> LegacyConfigDir {
    LegacyConfigDir {
        path: PathBuf::from(path),
        display: LEGACY_APPDATA_DISPLAY,
    }
}

fn library_dir(path: &str) -> LegacyConfigDir {
    LegacyConfigDir {
        path: PathBuf::from(path),
        display: LEGACY_LIBRARY_DISPLAY,
    }
}

/// One row: name, `APPDATA`, `HOME`, expected old folders in order.
type LegacyRow = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Vec<LegacyConfigDir>,
);

#[cfg(unix)]
#[test]
fn legacy_config_dirs_lists_appdata_then_library_for_absolute_values() {
    let library = || library_dir("/home/u/Library/Application Support/routectl");
    let appdata = || appdata_dir("/appdata/routectl");
    let rows: [LegacyRow; 8] = [
        (
            "both, appdata first",
            Some("/appdata"),
            Some("/home/u"),
            vec![appdata(), library()],
        ),
        ("appdata alone", Some("/appdata"), None, vec![appdata()]),
        ("appdata empty", Some(""), Some("/home/u"), vec![library()]),
        (
            "appdata relative",
            Some("rel"),
            Some("/home/u"),
            vec![library()],
        ),
        (
            "windows-style appdata is relative on unix",
            Some("C:\\Users\\u\\AppData\\Roaming"),
            Some("/home/u"),
            vec![library()],
        ),
        ("appdata unset", None, Some("/home/u"), vec![library()]),
        (
            "home with a trailing slash",
            None,
            Some("/home/u/"),
            vec![library()],
        ),
        (
            "non-ascii home",
            None,
            Some("/home/jos\u{e9}"),
            vec![library_dir(
                "/home/jos\u{e9}/Library/Application Support/routectl",
            )],
        ),
    ];
    for (name, appdata, home, want) in rows {
        let got = legacy_config_dirs(appdata.map(OsStr::new), home.map(OsStr::new));

        assert_eq!(got, want, "row {name}");
    }
}

/// An `APPDATA` that names the same folder as the `HOME` library folder is
/// one candidate, checked and reported once under `APPDATA`.
#[cfg(unix)]
#[test]
fn legacy_config_dirs_lists_a_folder_both_variables_name_once() {
    let rows: [(&str, &str); 2] = [
        ("same path", "/home/u/Library/Application Support"),
        (
            "same path with a trailing slash",
            "/home/u/Library/Application Support/",
        ),
    ];
    for (name, appdata) in rows {
        let got = legacy_config_dirs(Some(OsStr::new(appdata)), Some(OsStr::new("/home/u")));

        assert_eq!(
            got,
            vec![appdata_dir("/home/u/Library/Application Support/routectl")],
            "row {name}"
        );
    }
}

/// Values that are not absolute on any OS name no candidate.
#[test]
fn legacy_config_dirs_is_empty_without_an_absolute_value() {
    // (name, APPDATA, HOME)
    let rows: [(&str, Option<&str>, Option<&str>); 4] = [
        ("home empty", None, Some("")),
        ("appdata empty", Some(""), None),
        ("home and appdata relative", Some("rel"), Some("rel")),
        ("both unset", None, None),
    ];
    for (name, appdata, home) in rows {
        let got = legacy_config_dirs(appdata.map(OsStr::new), home.map(OsStr::new));

        assert!(got.is_empty(), "row {name}: {got:?}");
    }
}

/// Plant `config.toml` under each named candidate, in candidate order.
fn candidates_with_configs(root: &Path, planted: [bool; 2]) -> Vec<LegacyConfigDir> {
    let dirs = vec![
        LegacyConfigDir {
            path: root.join("appdata").join("routectl"),
            display: LEGACY_APPDATA_DISPLAY,
        },
        LegacyConfigDir {
            path: root.join("library").join("routectl"),
            display: LEGACY_LIBRARY_DISPLAY,
        },
    ];
    for (dir, plant_it) in dirs.iter().zip(planted) {
        if plant_it {
            plant(&dir.path.join("config.toml"));
        }
    }
    dirs
}

#[test]
fn refuse_stranded_config_refuses_only_when_the_config_is_only_in_an_old_folder() {
    // (name, plant new, plant in [appdata, library], expected stranded index)
    let rows: [(&str, bool, [bool; 2], Option<usize>); 6] = [
        ("only appdata", false, [true, false], Some(0)),
        ("only library", false, [false, true], Some(1)),
        (
            "both old folders, appdata named",
            false,
            [true, true],
            Some(0),
        ),
        ("new and old", true, [true, true], None),
        ("neither", false, [false, false], None),
        ("only new", true, [false, false], None),
    ];
    for (name, has_new, planted, want) in rows {
        let tmp = tempfile::tempdir().unwrap();
        let new_cfg = tmp.path().join("new").join("routectl").join("config.toml");
        let dirs = candidates_with_configs(tmp.path(), planted);
        if has_new {
            plant(&new_cfg);
        }

        let got = refuse_stranded_config(&new_cfg, &dirs);

        match (got, want) {
            (Err(ConfigFolderError::Stranded { old, new }), Some(at)) => {
                assert_eq!(old, dirs[at].path.join("config.toml"), "row {name}");
                assert_eq!(new, new_cfg, "row {name}");
            }
            (Ok(()), None) => {}
            (other, _) => panic!("row {name}: {other:?}"),
        }
    }
}

/// The first candidate that cannot be inspected wins, even when a later one
/// holds a config: an unknown is never read as absent. Unix only: the
/// fixture relies on a path under a regular file failing as `NotADirectory`.
#[cfg(unix)]
#[test]
fn refuse_stranded_config_names_the_first_old_config_it_could_not_inspect() {
    // (name, block appdata, block library, plant library config, blocked index)
    let rows: [(&str, bool, bool, bool, usize); 3] = [
        ("library blocked", false, true, false, 1),
        (
            "appdata blocked before a stranded library",
            true,
            false,
            true,
            0,
        ),
        ("both blocked", true, true, false, 0),
    ];
    for (name, block_appdata, block_library, library_config, want) in rows {
        let tmp = tempfile::tempdir().unwrap();
        let new_cfg = tmp.path().join("new").join("config.toml");
        let dirs = candidates_with_configs(tmp.path(), [false, library_config]);
        for (dir, block) in dirs.iter().zip([block_appdata, block_library]) {
            if block {
                plant(&dir.path);
            }
        }

        let got = refuse_stranded_config(&new_cfg, &dirs);

        match got {
            Err(ConfigFolderError::Inspect { path, source }) => {
                assert_eq!(path, dirs[want].path.join("config.toml"), "row {name}");
                assert_eq!(
                    source.kind(),
                    std::io::ErrorKind::NotADirectory,
                    "row {name}"
                );
            }
            other => {
                panic!("row {name}: an uninspectable folder must not read as absent: {other:?}")
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn refuse_stranded_config_names_the_new_config_it_could_not_inspect() {
    let home = home_with_blocked_legacy_folder();
    let new_cfg = home.path().join("Library").join("config.toml");
    let dirs = candidates_with_configs(home.path(), [true, false]);

    let got = refuse_stranded_config(&new_cfg, &dirs);

    match got {
        Err(ConfigFolderError::Inspect { path, .. }) => assert_eq!(path, new_cfg),
        other => panic!("an uninspectable new config must not read as absent: {other:?}"),
    }
}

/// With no candidate nothing is checked, so even an uninspectable new config
/// passes.
#[cfg(unix)]
#[test]
fn refuse_stranded_config_does_nothing_without_an_old_folder() {
    let home = home_with_blocked_legacy_folder();
    let new_cfg = home.path().join("Library").join("config.toml");

    assert!(refuse_stranded_config(&new_cfg, &[]).is_ok());
}

/// A link at the new config path whose target is missing is something the
/// operator put there, not an absent config, so the old folder is not
/// reported as stranded.
#[cfg(unix)]
#[test]
fn refuse_stranded_config_reads_a_dangling_link_at_the_new_path_as_present() {
    let tmp = tempfile::tempdir().unwrap();
    let new_cfg = tmp.path().join("new").join("config.toml");
    std::fs::create_dir_all(new_cfg.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("missing.toml"), &new_cfg).unwrap();
    let dirs = candidates_with_configs(tmp.path(), [true, false]);

    let got = refuse_stranded_config(&new_cfg, &dirs);

    assert!(got.is_ok(), "{got:?}");
}

#[test]
fn stranded_error_names_both_paths_and_both_ways_out() {
    let err = ConfigFolderError::Stranded {
        old: PathBuf::from("/h/Library/Application Support/routectl/config.toml"),
        new: PathBuf::from("/h/.config/routectl/config.toml"),
    };

    let text = err.to_string();

    assert_eq!(
        text,
        "the config is in the old folder: found \
         \"/h/Library/Application Support/routectl/config.toml\", but routectl now reads \
         \"/h/.config/routectl/config.toml\", which does not exist. routectl moved nothing. \
         Move the files in \"/h/Library/Application Support/routectl\" to \
         \"/h/.config/routectl\", or pass --config \
         \"/h/Library/Application Support/routectl/config.toml\""
    );
    let redacted = crate::commands::parse_error_redaction::redact_config_load_error(&text);
    assert_eq!(
        redacted, text,
        "the redactor must keep the refusal verbatim"
    );
}

#[test]
fn inspect_error_survives_the_redactor_naming_its_path() {
    let err = ConfigFolderError::Inspect {
        path: PathBuf::from("/h/Library/Application Support/routectl/config.toml"),
        source: std::io::Error::from(std::io::ErrorKind::NotADirectory),
    };

    let text = err.to_string();
    let redacted = crate::commands::parse_error_redaction::redact_config_load_error(&text);

    assert_eq!(redacted, text);
    assert!(
        text.contains("/h/Library/Application Support/routectl/config.toml"),
        "{text}"
    );
    assert!(text.contains("routectl moved nothing"), "{text}");
    assert!(text.contains("--config"), "{text}");
}

/// Which error renders the path in an escaping row.
#[derive(Clone, Copy, Debug)]
enum Rendered {
    StrandedOld,
    StrandedNew,
    Inspect,
}

fn render_with(path: &Path, rendered: Rendered) -> String {
    let other = PathBuf::from("/h/.config/routectl/config.toml");
    let err = match rendered {
        Rendered::StrandedOld => ConfigFolderError::Stranded {
            old: path.to_path_buf(),
            new: other,
        },
        Rendered::StrandedNew => ConfigFolderError::Stranded {
            old: other,
            new: path.to_path_buf(),
        },
        Rendered::Inspect => ConfigFolderError::Inspect {
            path: path.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::NotADirectory),
        },
    };
    err.to_string()
}

/// The paths come from environment variables, so a control character in one
/// is printed escaped, never raw, while a non-ASCII folder name is printed as
/// it is so the operator can copy it.
#[test]
fn config_folder_errors_escape_control_characters_and_keep_non_ascii_paths() {
    // (name, path, rendered as, text that must appear)
    let rows: [(&str, &str, Rendered, &str); 6] = [
        (
            "newline in the old path",
            "/h/a\nb/routectl/config.toml",
            Rendered::StrandedOld,
            "\"/h/a\\nb/routectl/config.toml\"",
        ),
        (
            "escape in the new path",
            "/h/a\u{1b}[2Jb/routectl/config.toml",
            Rendered::StrandedNew,
            "\"/h/a\\u{1b}[2Jb/routectl/config.toml\"",
        ),
        (
            "newline in the inspected path",
            "/h/a\nb/routectl/config.toml",
            Rendered::Inspect,
            "\"/h/a\\nb/routectl/config.toml\"",
        ),
        (
            "non-ascii old path",
            "/home/jos\u{e9}/routectl/config.toml",
            Rendered::StrandedOld,
            "\"/home/jos\u{e9}/routectl/config.toml\"",
        ),
        (
            "non-ascii new path",
            "/home/jos\u{e9}/routectl/config.toml",
            Rendered::StrandedNew,
            "\"/home/jos\u{e9}/routectl/config.toml\"",
        ),
        (
            "non-ascii inspected path",
            "/home/jos\u{e9}/routectl/config.toml",
            Rendered::Inspect,
            "\"/home/jos\u{e9}/routectl/config.toml\"",
        ),
    ];
    for (name, path, rendered, want) in rows {
        let text = render_with(Path::new(path), rendered);

        assert!(text.contains(want), "row {name}: {text}");
        assert!(
            !text.contains(|c: char| c.is_control()),
            "row {name}: a raw control character: {text:?}"
        );
    }
}
