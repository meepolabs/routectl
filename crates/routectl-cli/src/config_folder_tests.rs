use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;

fn plant(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "version = 5\n").unwrap();
}

#[test]
fn legacy_config_dir_is_under_library_application_support() {
    let rows: &[(&str, Option<&OsStr>, Option<PathBuf>)] = &[
        (
            "home set",
            Some(OsStr::new("/home/u")),
            Some(PathBuf::from(
                "/home/u/Library/Application Support/routectl",
            )),
        ),
        ("home empty", Some(OsStr::new("")), None),
        ("home unset", None, None),
    ];
    for (name, home, want) in rows {
        assert_eq!(&legacy_config_dir(*home), want, "row {name}");
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

        let got = stranded_config(&new_cfg, &legacy).unwrap();

        let want = want_stranded.then(|| StrandedConfig {
            old: legacy.join("config.toml"),
            new: new_cfg.clone(),
        });
        assert_eq!(got, want, "row {name}");
    }
}

#[test]
fn stranded_config_errors_when_the_old_folder_cannot_be_inspected() {
    let tmp = tempfile::tempdir().unwrap();
    let new_cfg = tmp.path().join("new").join("config.toml");
    let locked = tmp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let legacy = locked.join("routectl");

    let got = stranded_config(&new_cfg, &legacy);

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        got.is_err(),
        "an uninspectable folder must not read as absent: {got:?}"
    );
}

#[test]
fn stranded_error_names_both_paths_and_both_ways_out() {
    let err = StrandedConfig {
        old: PathBuf::from("/h/Library/Application Support/routectl/config.toml"),
        new: PathBuf::from("/h/.config/routectl/config.toml"),
    }
    .into_error();

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
fn inspect_error_survives_the_redactor_with_both_paths() {
    let err = LegacyInspectError {
        legacy_dir: PathBuf::from("/h/Library/Application Support/routectl"),
        new: PathBuf::from("/h/.config/routectl/config.toml"),
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };

    let text = err.to_string();
    let redacted = crate::commands::parse_error_redaction::redact_config_load_error(&text);

    assert_eq!(redacted, text);
    assert!(
        text.contains("/h/Library/Application Support/routectl"),
        "{text}"
    );
    assert!(text.contains("/h/.config/routectl/config.toml"), "{text}");
}

#[test]
fn refuse_stranded_config_does_nothing_without_home() {
    let tmp = tempfile::tempdir().unwrap();
    let new_cfg = tmp.path().join("absent.toml");

    assert!(refuse_stranded_config(&new_cfg, None).is_ok());
    assert!(refuse_stranded_config(&new_cfg, Some(OsStr::new(""))).is_ok());
}
