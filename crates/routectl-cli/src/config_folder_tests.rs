use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::*;

fn plant(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "version = 5\n").unwrap();
}

/// A home whose `Library` is a regular file, so every existence check under
/// the old folder fails with `NotADirectory` for any uid, root included.
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

/// One row: name, `HOME`, `APPDATA`, expected old folders in order.
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
            Some("/home/u"),
            Some("/appdata"),
            vec![appdata(), library()],
        ),
        ("appdata alone", None, Some("/appdata"), vec![appdata()]),
        ("appdata empty", Some("/home/u"), Some(""), vec![library()]),
        (
            "appdata relative",
            Some("/home/u"),
            Some("rel"),
            vec![library()],
        ),
        (
            "windows-style appdata is relative on unix",
            Some("/home/u"),
            Some("C:\\Users\\u\\AppData\\Roaming"),
            vec![library()],
        ),
        ("appdata unset", Some("/home/u"), None, vec![library()]),
        (
            "home with a trailing slash",
            Some("/home/u/"),
            None,
            vec![library()],
        ),
        (
            "non-ascii home",
            Some("/home/jos\u{e9}"),
            None,
            vec![library_dir(
                "/home/jos\u{e9}/Library/Application Support/routectl",
            )],
        ),
    ];
    for (name, home, appdata, want) in rows {
        let got = legacy_config_dirs(home.map(OsStr::new), appdata.map(OsStr::new));

        assert_eq!(got, want, "row {name}");
    }
}

/// Values that are not absolute on any OS name no candidate.
#[test]
fn legacy_config_dirs_is_empty_without_an_absolute_value() {
    let rows: [(&str, Option<&str>, Option<&str>); 4] = [
        ("home empty", Some(""), None),
        ("appdata empty", None, Some("")),
        ("home and appdata relative", Some("rel"), Some("rel")),
        ("both unset", None, None),
    ];
    for (name, home, appdata) in rows {
        let got = legacy_config_dirs(home.map(OsStr::new), appdata.map(OsStr::new));

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
/// holds a config: an unknown is never read as absent.
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
#[test]
fn refuse_stranded_config_does_nothing_without_an_old_folder() {
    let home = home_with_blocked_legacy_folder();
    let new_cfg = home.path().join("Library").join("config.toml");

    assert!(refuse_stranded_config(&new_cfg, &[]).is_ok());
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
         /h/Library/Application Support/routectl/config.toml, but routectl now reads \
         /h/.config/routectl/config.toml, which does not exist. routectl moved nothing. \
         Move the files in /h/Library/Application Support/routectl to /h/.config/routectl, \
         or pass --config \"/h/Library/Application Support/routectl/config.toml\""
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
