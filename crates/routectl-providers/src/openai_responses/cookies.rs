//! Cloudflare-cookie persistence for the openai-responses provider.
//!
//! ChatGPT's `chatgpt.com/backend-api/codex` surface sits behind
//! Cloudflare and pins a small set of cookies (`__cf_bm`, `_cfuvid`,
//! `cf_clearance`, ...) on first contact. Without a cookie store the
//! reqwest client tosses every Set-Cookie response, forcing the next
//! request through Cloudflare's challenge cycle each time -- which on
//! a long-lived routectl process burns repeated 5xx / waiting-room
//! shapes from the upstream.
//!
//! This module provides:
//!
//!   - [`build_jar`] -- a fresh in-memory `CookieStoreMutex` ready to
//!     plug into `reqwest::ClientBuilder::cookie_provider`.
//!   - [`load_jar`]  -- hydrate the jar from a JSON file on disk
//!     (returns an empty jar if the file is missing or malformed; the
//!     provider must keep working even on a fresh install).
//!   - [`save_jar`] -- persist the jar to a JSON file with `0600`
//!     permissions on Unix (so other local users cannot lift the
//!     Cloudflare bypass cookies out of the running operator's home).
//!   - [`default_cookie_path`] -- resolve the on-disk path. Honors
//!     `ROUTECTL_COOKIE_FILE` so tests can isolate; otherwise defaults
//!     to `$HOME/.config/routectl/cookies/chatgpt.json`.
//!
//! Codex CLI parity reference:
//! `codex-rs/codex-client/src/chatgpt_cloudflare_cookies.rs`. Codex
//! pins an allowlist of Cloudflare cookie NAMES so the jar never
//! holds chatgpt account / session secrets even if the upstream
//! Set-Cookies them. Routectl follows the same allowlist on save AND
//! load so a stale on-disk file from a previous routectl version
//! cannot smuggle non-allowlisted cookies into the live jar.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest_cookie_store::CookieStoreMutex;

/// Allowlisted Cloudflare cookie names. Mirrors the codex CLI's
/// `is_allowed_cloudflare_cookie_name` (commit pin documented in
/// `docs/PROVIDER-QUIRKS.md`). Keep in lockstep with codex when they
/// extend the list.
const CLOUDFLARE_COOKIE_NAMES: &[&str] = &[
    "__cf_bm",
    "__cflb",
    "__cfruid",
    "__cfseq",
    "__cfwaitingroom",
    "_cfuvid",
    "cf_clearance",
    "cf_ob_info",
    "cf_use_ob",
];

/// True if `name` is a Cloudflare service cookie routectl is willing
/// to persist. Mirrors codex's allowlist (exact-match list above plus
/// the `cf_chl_` challenge-cookie prefix).
fn is_allowed_cloudflare_cookie_name(name: &str) -> bool {
    CLOUDFLARE_COOKIE_NAMES.contains(&name) || name.starts_with("cf_chl_")
}

/// Default on-disk path for the persisted Cloudflare cookie jar.
/// Returns `None` only when neither `ROUTECTL_COOKIE_FILE` nor `HOME`
/// is set, in which case the provider runs without persistence.
pub fn default_cookie_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ROUTECTL_COOKIE_FILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    let mut p = PathBuf::from(home);
    p.push(".config");
    p.push("routectl");
    p.push("cookies");
    p.push("chatgpt.json");
    Some(p)
}

/// Build a fresh empty jar. Caller plugs the Arc into
/// `ClientBuilder::cookie_provider`; the jar implements
/// `reqwest::cookie::CookieStore` so reqwest reads / writes cookies
/// through it on each request.
pub fn build_jar() -> Arc<CookieStoreMutex> {
    Arc::new(CookieStoreMutex::default())
}

/// Hydrate a jar from a JSON file written by a previous [`save_jar`]
/// call. A missing or malformed file returns an empty jar -- this is
/// a soft-fail by design: a fresh install, a corrupted persistence
/// file, or a routectl version mismatch must not crash the provider.
/// Filtered to allowlisted Cloudflare names so a stale file cannot
/// smuggle non-allowlisted cookies into the live jar.
pub fn load_jar(path: &Path) -> Arc<CookieStoreMutex> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return build_jar(),
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                error = %e,
                "openai-responses: cookie jar read failed; starting empty"
            );
            return build_jar();
        }
    };
    let filtered = match filter_persisted_json(&bytes) {
        Ok(j) => j,
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                error = %e,
                "openai-responses: cookie jar decode failed; starting empty"
            );
            return build_jar();
        }
    };
    let store = match cookie_store::serde::json::load(filtered.as_slice()) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                error = %e,
                "openai-responses: cookie jar decode failed; starting empty"
            );
            return build_jar();
        }
    };
    Arc::new(CookieStoreMutex::new(store))
}

/// Strip non-allowlisted entries from a `cookie_store::serde::json`
/// dump. The on-disk format is a single JSON array of cookie records,
/// each carrying a `raw_cookie` field (`"name=value; ..."`); we keep
/// records whose extracted name is on the allowlist and drop the rest.
/// A non-array root, malformed entry, or missing `raw_cookie` field
/// drops to "no cookies" rather than blowing up -- defense-in-depth on
/// a corrupted persistence file.
fn filter_persisted_json(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let parsed: serde_json::Value = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
    let serde_json::Value::Array(entries) = parsed else {
        return Ok(b"[]\n".to_vec());
    };
    let kept: Vec<serde_json::Value> = entries
        .into_iter()
        .filter(
            |entry| match entry.get("raw_cookie").and_then(|r| r.as_str()) {
                Some(raw) => raw
                    .split_once('=')
                    .map(|(n, _)| is_allowed_cloudflare_cookie_name(n.trim()))
                    .unwrap_or(false),
                None => false,
            },
        )
        .collect();
    let mut out = serde_json::to_vec_pretty(&serde_json::Value::Array(kept))
        .map_err(std::io::Error::other)?;
    out.push(b'\n');
    Ok(out)
}

/// Persist the jar to `path` as JSON with mode `0600` on Unix. Creates
/// missing parent directories. The on-disk dump is post-filtered so
/// only allowlisted Cloudflare names land in the file even if the
/// in-memory jar somehow accumulated more (defense-in-depth on the
/// write path).
pub fn save_jar(jar: &CookieStoreMutex, path: &Path) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = jar
        .lock()
        .map_err(|_| std::io::Error::other("cookie jar mutex poisoned"))?;
    let mut buf: Vec<u8> = Vec::new();
    cookie_store::serde::json::save(&store, &mut buf)
        .map_err(|e| std::io::Error::other(format!("cookie jar serialize: {e}")))?;
    drop(store);

    let filtered = filter_persisted_json(&buf)?;
    write_file_0600(path, &filtered)?;
    Ok(filtered.len())
}

/// Atomic 0600 write. Serializes to a tempfile in the SAME directory as
/// `path`, fsyncs it, then persists it onto `path` via rename so a
/// crashed or concurrent save can never promote a half-written jar into
/// the persistence slot. Mirrors the OAuth credentials writer.
fn write_file_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cookie path has no parent",
        )
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".chatgpt.tmp.")
        .suffix(".json")
        .tempfile_in(parent)?;
    // On Unix, force 0o600 BEFORE writing so a partial write never has
    // wider permissions than the final file (defense-in-depth; mkstemp
    // already creates the tempfile 0600).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    // Re-assert 0o600 on the renamed file as defense-in-depth in case a
    // paranoid umask stripped bits during persist. Best-effort: the
    // rename preserves the temp's mode in practice, so a failure here is
    // not fatal. Mirrors the OAuth credentials writer.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore;
    use reqwest::header::HeaderValue;

    fn touch_jar_with(jar: &CookieStoreMutex, name: &str, value: &str) {
        let url = reqwest::Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap();
        // Use an absolute `Expires` (rather than `Max-Age`) so the
        // cookie is persistent and survives the session-cookie drop in
        // `cookie_store::serde::json::save`. Real Cloudflare cookies
        // always carry an explicit lifetime.
        let header = HeaderValue::from_str(&format!(
            "{name}={value}; Domain=chatgpt.com; Path=/; \
             Expires=Tue, 19 Jan 2038 03:14:07 GMT; Secure; HttpOnly"
        ))
        .unwrap();
        jar.set_cookies(&mut std::iter::once(&header), &url);
    }

    fn jar_cookie_value(jar: &CookieStoreMutex, name: &str) -> Option<String> {
        let url = reqwest::Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap();
        let header = jar.cookies(&url)?;
        let header = header.to_str().ok()?.to_string();
        for entry in header.split(';') {
            let entry = entry.trim();
            let (n, v) = entry.split_once('=')?;
            if n == name {
                return Some(v.to_string());
            }
        }
        None
    }

    #[test]
    fn save_then_load_round_trips_cloudflare_cookie() {
        // Arrange: a jar holding both an allowlisted Cloudflare cookie
        // and a non-allowlisted one (which the save+load filter must
        // drop).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt.json");
        let jar = build_jar();
        touch_jar_with(&jar, "cf_clearance", "clearance-token");
        touch_jar_with(&jar, "_cfuvid", "visitor-id");
        touch_jar_with(&jar, "chatgpt_session", "session-secret");

        // Sanity: the Cloudflare cookies must be visible on the live
        // jar (not the disk path) before save/load.
        assert_eq!(
            jar_cookie_value(&jar, "cf_clearance").as_deref(),
            Some("clearance-token"),
            "live jar should hold the cookie before save"
        );

        // Act: persist, then hydrate a fresh jar.
        save_jar(&jar, &path).expect("save_jar");
        let reloaded = load_jar(&path);

        // Assert: both Cloudflare cookies survive; the account session
        // cookie is dropped on save.
        assert_eq!(
            jar_cookie_value(&reloaded, "cf_clearance").as_deref(),
            Some("clearance-token")
        );
        assert_eq!(
            jar_cookie_value(&reloaded, "_cfuvid").as_deref(),
            Some("visitor-id")
        );
        assert!(
            jar_cookie_value(&reloaded, "chatgpt_session").is_none(),
            "non-allowlisted cookie must NOT survive load"
        );
    }

    #[test]
    fn load_missing_file_returns_empty_jar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        let jar = load_jar(&path);
        assert!(jar_cookie_value(&jar, "cf_clearance").is_none());
    }

    #[test]
    fn load_malformed_file_returns_empty_jar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("malformed.json");
        std::fs::write(&path, b"{not-json").expect("write malformed");
        let jar = load_jar(&path);
        assert!(jar_cookie_value(&jar, "cf_clearance").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_jar_writes_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt.json");
        let jar = build_jar();
        touch_jar_with(&jar, "cf_clearance", "v");
        save_jar(&jar, &path).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "cookie file must be 0600 to keep cf cookies private"
        );
    }

    #[test]
    fn save_jar_leaves_no_tempfile_behind() {
        // Arrange: a jar with one allowlisted cookie, saved twice to the
        // same dest so any leaked sibling tempfile would accumulate.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chatgpt.json");
        let jar = build_jar();
        touch_jar_with(&jar, "cf_clearance", "v");

        // Act
        save_jar(&jar, &path).expect("first save");
        save_jar(&jar, &path).expect("second save");

        // Assert: the atomic-rename write must not leave a sibling
        // `.chatgpt.tmp.` file in the persistence directory.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".chatgpt.tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "atomic cookie write left tempfiles: {leftover:?}"
        );
    }

    #[test]
    fn default_cookie_path_honors_env_override() {
        let prior = std::env::var_os("ROUTECTL_COOKIE_FILE");
        std::env::set_var("ROUTECTL_COOKIE_FILE", "/tmp/routectl-cookie-test.json");
        let p = default_cookie_path().expect("path");
        assert_eq!(p, Path::new("/tmp/routectl-cookie-test.json"));
        match prior {
            Some(v) => std::env::set_var("ROUTECTL_COOKIE_FILE", v),
            None => std::env::remove_var("ROUTECTL_COOKIE_FILE"),
        }
    }

    #[test]
    fn allowlist_blocks_non_cloudflare_cookie_name() {
        assert!(is_allowed_cloudflare_cookie_name("cf_clearance"));
        assert!(is_allowed_cloudflare_cookie_name("_cfuvid"));
        assert!(is_allowed_cloudflare_cookie_name("cf_chl_rc_i"));
        assert!(!is_allowed_cloudflare_cookie_name(
            "__Secure-next-auth.session-token"
        ));
        assert!(!is_allowed_cloudflare_cookie_name("chatgpt_session"));
    }
}
