use std::path::{Path, PathBuf};

use routectl_router::{Config, MitmConfig};

use super::*;

fn config_with(db_path: &str, cert_dir: Option<&str>) -> Config {
    let mut config = Config::default();
    config.usage.db_path = PathBuf::from(db_path);
    config.mitm = cert_dir.map(|dir| MitmConfig {
        cert_dir: PathBuf::from(dir),
        ..MitmConfig::default()
    });
    config
}

#[test]
fn the_refusal_names_the_key_and_both_remedies() {
    let err = NoConfigFolder {
        key: USAGE_DB_PATH_KEY,
    };

    assert_eq!(
        err.to_string(),
        "no config folder for usage.db_path: set HOME, or set usage.db_path"
    );
}

/// `(row name, db_path, [mitm] cert_dir, refused key or Ok)`.
type ServeRow = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Result<(), &'static str>,
);

#[test]
fn serve_paths_refuse_an_empty_path_by_key() {
    let rows: &[ServeRow] = &[
        ("empty db path", "", None, Err(USAGE_DB_PATH_KEY)),
        (
            "empty db path wins over mitm",
            "",
            Some(""),
            Err(USAGE_DB_PATH_KEY),
        ),
        (
            "empty cert dir with mitm",
            "/abs/usage.db",
            Some(""),
            Err(MITM_CERT_DIR_KEY),
        ),
        ("both set", "/abs/usage.db", Some("/abs/certs"), Ok(())),
        ("no mitm block", "/abs/usage.db", None, Ok(())),
        (
            "relative paths the operator wrote",
            "usage.db",
            Some("certs"),
            Ok(()),
        ),
    ];
    for (name, db, cert, want) in rows {
        let got = require_serve_paths(&config_with(db, *cert)).map_err(|e| e.key);

        assert_eq!(got, *want, "row `{name}`");
    }
}

#[test]
fn usage_db_path_prefers_the_flag_and_refuses_an_empty_default() {
    let empty = config_with("", None);
    let set = config_with("/abs/usage.db", None);

    assert_eq!(
        usage_db_path(Some(PathBuf::from("/flag.db")), &empty),
        Ok(PathBuf::from("/flag.db")),
        "an explicit --db needs no config folder"
    );
    assert_eq!(
        usage_db_path(None, &set),
        Ok(PathBuf::from("/abs/usage.db"))
    );
    assert_eq!(
        usage_db_path(None, &empty),
        Err(NoConfigFolder {
            key: USAGE_DB_PATH_KEY
        })
    );
}

#[test]
fn require_derived_path_passes_a_non_empty_path_through() {
    assert_eq!(
        require_derived_path(Path::new("x"), MITM_CERT_DIR_KEY),
        Ok(Path::new("x"))
    );
}
