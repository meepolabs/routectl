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

/// One row: name, `HOME`, `APPDATA`, expected old folder.
type LegacyRow = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<LegacyConfigDir>,
);

#[test]
fn legacy_config_dir_picks_one_candidate_from_appdata_then_home() {
    let library = Some(LegacyConfigDir {
        path: PathBuf::from("/home/u/Library/Application Support/routectl"),
        display: LEGACY_LIBRARY_DISPLAY,
    });
    let appdata = Some(LegacyConfigDir {
        path: PathBuf::from("/appdata/routectl"),
        display: LEGACY_APPDATA_DISPLAY,
    });
    let rows: [LegacyRow; 8] = [
        (
            "appdata wins over home",
            Some("/home/u"),
            Some("/appdata"),
            appdata.clone(),
        ),
        ("appdata alone", None, Some("/appdata"), appdata),
        (
            "appdata empty uses home",
            Some("/home/u"),
            Some(""),
            library.clone(),
        ),
        (
            "appdata relative uses home",
            Some("/home/u"),
            Some("rel"),
            library.clone(),
        ),
        ("appdata unset uses home", Some("/home/u"), None, library),
        ("home empty", Some(""), None, None),
        ("home relative", Some("rel"), Some("rel"), None),
        ("both unset", None, None, None),
    ];
    for (name, home, appdata, want) in rows {
        let got = legacy_config_dir(home.map(OsStr::new), appdata.map(OsStr::new));

        assert_eq!(got, want, "row {name}");
    }
}

#[test]
fn stranded_config_refuses_only_when_the_config_is_only_in_the_old_folder() {
    // (name, plant new, plant old, stranded)
    let rows = [
        ("only old", false, true, true),
        ("both", true, true, false),
        ("neither", false, false, false),
        ("only new", true, false, false),
    ];
    for (name, has_new, has_old, want_stranded) in rows {
        let tmp = tempfile::tempdir().unwrap();
        let new_cfg = tmp.path().join("new").join("routectl").join("config.toml");
        let legacy = tmp.path().join("old").join("routectl");
        if has_new {
            plant(&new_cfg);
        }
        if has_old {
            plant(&legacy.join("config.toml"));
        }

        let got = stranded_config(&new_cfg, &legacy);

        match got {
            Err(ConfigFolderError::Stranded { old, new }) if want_stranded => {
                assert_eq!(old, legacy.join("config.toml"), "row {name}");
                assert_eq!(new, new_cfg, "row {name}");
            }
            Ok(()) if !want_stranded => {}
            other => panic!("row {name}: {other:?}"),
        }
    }
}

#[test]
fn stranded_config_names_the_old_config_it_could_not_inspect() {
    let home = home_with_blocked_legacy_folder();
    let new_cfg = home.path().join("new").join("config.toml");
    let legacy = legacy_config_dir(Some(home.path().as_os_str()), None)
        .unwrap()
        .path;

    let got = stranded_config(&new_cfg, &legacy);

    match got {
        Err(ConfigFolderError::Inspect { path, .. }) => {
            assert_eq!(path, legacy.join("config.toml"));
        }
        other => panic!("an uninspectable folder must not read as absent: {other:?}"),
    }
}

#[test]
fn stranded_config_names_the_new_config_it_could_not_inspect() {
    let home = home_with_blocked_legacy_folder();
    let new_cfg = home.path().join("Library").join("config.toml");
    let legacy = home.path().join("old").join("routectl");
    plant(&legacy.join("config.toml"));

    let got = stranded_config(&new_cfg, &legacy);

    match got {
        Err(ConfigFolderError::Inspect { path, .. }) => assert_eq!(path, new_cfg),
        other => panic!("an uninspectable new config must not read as absent: {other:?}"),
    }
}

#[test]
fn refuse_stranded_config_reports_an_uninspectable_old_folder() {
    let home = home_with_blocked_legacy_folder();
    let new_cfg = home
        .path()
        .join(".config")
        .join("routectl")
        .join("config.toml");
    let legacy = legacy_config_dir(Some(home.path().as_os_str()), None).unwrap();

    let got = refuse_stranded_config(&new_cfg, Some(&legacy.path));

    match got {
        Err(ConfigFolderError::Inspect { path, source }) => {
            assert_eq!(path, legacy.path.join("config.toml"));
            assert_eq!(source.kind(), std::io::ErrorKind::NotADirectory);
        }
        other => panic!("an uninspectable folder must not read as absent: {other:?}"),
    }
}

#[test]
fn refuse_stranded_config_does_nothing_without_an_old_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let new_cfg = tmp.path().join("absent.toml");

    assert!(refuse_stranded_config(&new_cfg, None).is_ok());
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
