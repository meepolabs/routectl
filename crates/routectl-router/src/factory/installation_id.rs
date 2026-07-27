//! Persistent installation-id resolution for the openai-responses egress.
//!
//! A UUIDv4 kept at `<config-dir>/installation_id` fingerprints this
//! routectl install to the chatgpt.com backend. Resolved once per
//! provider construction: an existing valid file is ADOPTED (normalized
//! to lowercase); an absent one is MINTED via an atomic create+rename
//! (owner-only `0o600`, reusing the shared secret-file writer); an empty
//! or corrupt file is re-minted. Any failure yields `None` and one
//! structured WARN naming the error class -- the egress simply omits the
//! header, the request path is unaffected, and the next construction
//! retries. The id is a stable machine fingerprint and never enters logs.

use std::path::Path;

/// Filename under the config dir holding the persistent installation-id.
const INSTALLATION_ID_FILE: &str = "installation_id";

/// Read-or-create the persistent installation-id under routectl's config
/// dir. Returns `None` on any I/O failure (the header is then omitted).
pub(super) fn resolve_installation_id() -> Option<String> {
    resolve_installation_id_in(&crate::config::routectl_config_dir())
}

/// Config-dir-parameterized core of [`resolve_installation_id`], split
/// out so tests can drive a temp dir without touching the real
/// `~/.config/routectl` or racing on `XDG_CONFIG_HOME`.
fn resolve_installation_id_in(config_dir: &Path) -> Option<String> {
    let path = config_dir.join(INSTALLATION_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) => match parse_uuid_lowercase(&contents) {
            Some(id) => Some(id),
            // Present but empty/corrupt: re-mint atomically over it.
            None => mint(&path),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => mint(&path),
        Err(e) => {
            tracing::warn!(
                error_kind = ?e.kind(),
                "installation-id file unreadable; egress omits the header this run",
            );
            None
        }
    }
}

/// Parse the file contents as a UUID and render it lowercase-canonical.
/// Accepts any case on input; `None` when the (trimmed) contents are not
/// a valid UUID.
fn parse_uuid_lowercase(contents: &str) -> Option<String> {
    uuid::Uuid::try_parse(contents.trim())
        .ok()
        .map(|u| u.hyphenated().to_string())
}

/// Mint a fresh UUIDv4 and persist it atomically as an owner-only file.
/// Returns the minted id, or `None` with a structured WARN (error class
/// only -- never the UUID) when the write fails.
fn mint(path: &Path) -> Option<String> {
    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    match routectl_auth::atomic_write::write_0600_atomic(path, id.as_bytes()) {
        Ok(()) => Some(id),
        Err(reason) => {
            tracing::warn!(
                reason = %reason,
                "installation-id could not be written; egress omits the header this run",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lowercase_uuid(s: &str) -> bool {
        uuid::Uuid::try_parse(s).is_ok() && s == s.to_lowercase()
    }

    #[test]
    fn mints_and_is_stable_across_two_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let first = resolve_installation_id_in(dir.path()).expect("first resolve mints an id");
        assert!(
            is_lowercase_uuid(&first),
            "minted id must be a lowercase UUID: {first}"
        );
        assert!(dir.path().join(INSTALLATION_ID_FILE).exists());

        let second = resolve_installation_id_in(dir.path()).expect("second resolve adopts");
        assert_eq!(first, second, "id must be stable across constructions");
    }

    #[test]
    fn adopts_existing_value_lowercased() {
        let dir = tempfile::tempdir().unwrap();
        let seeded = "550E8400-E29B-41D4-A716-446655440000";
        std::fs::write(dir.path().join(INSTALLATION_ID_FILE), seeded).unwrap();

        let resolved = resolve_installation_id_in(dir.path()).expect("adopt");
        assert_eq!(
            resolved,
            seeded.to_lowercase(),
            "uppercase input must normalize to lowercase"
        );
    }

    #[test]
    fn adopts_value_with_surrounding_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let seeded = "550e8400-e29b-41d4-a716-446655440000";
        std::fs::write(
            dir.path().join(INSTALLATION_ID_FILE),
            format!("  {seeded}\n"),
        )
        .unwrap();

        let resolved = resolve_installation_id_in(dir.path()).expect("adopt");
        assert_eq!(resolved, seeded);
    }

    #[test]
    fn remints_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INSTALLATION_ID_FILE);
        std::fs::write(&path, "not-a-uuid").unwrap();

        let minted = resolve_installation_id_in(dir.path()).expect("corrupt file re-mints");
        assert!(
            is_lowercase_uuid(&minted),
            "re-mint must produce a valid UUID: {minted}"
        );

        // The file now holds the minted value; a follow-up resolve is stable.
        let again = resolve_installation_id_in(dir.path()).expect("adopt re-minted");
        assert_eq!(minted, again);
    }

    #[test]
    fn remints_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(INSTALLATION_ID_FILE), "").unwrap();

        let minted = resolve_installation_id_in(dir.path()).expect("empty file re-mints");
        assert!(is_lowercase_uuid(&minted));
    }

    #[test]
    fn unwritable_dir_yields_none_without_panic() {
        // A config dir whose PARENT is a regular file makes both the read
        // (NotFound) and the mint's create_dir_all fail -- the resolve must
        // return None rather than panic.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let config_dir = blocker.join("routectl");

        assert!(resolve_installation_id_in(&config_dir).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn minted_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        resolve_installation_id_in(dir.path()).expect("mint");
        let mode = std::fs::metadata(dir.path().join(INSTALLATION_ID_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "minted installation-id must be 0600, got {mode:o}"
        );
    }
}
