//! Local CA + leaf certificate lifecycle for the MITM front-proxy.
//!
//! Generates a long-lived local CA under `cert_dir` and a matching leaf
//! certificate for the configured MITM host, both signed with the
//! `aws-lc-rs` backend (matching the workspace's rustls crypto
//! provider). The CA is meant to be installed by the operator (e.g. via
//! `NODE_EXTRA_CA_CERTS`) so clients trust the leaf presented by
//! [`load_or_create`]'s [`tokio_rustls::TlsAcceptor`].
//!
//! Design note: this ships the CA-anchored leaf (leaf signed by a local
//! root, not a bare self-signed leaf). Whether a bare self-signed leaf
//! would also validate via `NODE_EXTRA_CA_CERTS` across Claude Code
//! versions was left unproven; collapsing to a
//! bare leaf later is a reversible, smaller change if that is ever
//! confirmed. Building that variant is out of scope here.
//!
//! Expiry tracking is metadata-driven, not X.509-DER-parsing-driven:
//! the exact `not_after` instant used to mint each cert is mirrored
//! into a small sidecar TOML file next to the PEMs, and reload checks
//! that instant directly. Re-parsing the literal DER `notAfter` back
//! out would need an X.509 parser (e.g. `x509-parser`), an extra
//! dependency not warranted here since we control both writer and
//! reader of the sidecar. The mirrored instant is truncated to
//! midnight UTC of its calendar day *before* being handed to both
//! `rcgen::date_time_ymd` (which bakes `Time::MIDNIGHT` into the DER
//! regardless of what we pass in) and the sidecar -- otherwise the
//! sidecar would record a `not_after` up to ~24h later than the DER's
//! real `notAfter`, reporting "valid" for a leaf that TLS clients
//! already reject as expired.
//!
//! On-disk layout (Unix): the CA key/cert, leaf key/cert, and sidecar
//! metadata for one mint are written together into a fresh,
//! unpredictably-named `generations/<gen-id>/` subdirectory, and a
//! single symlink `current` is atomically swapped (via a temp symlink
//! plus `rename`) to point at it once all five files are on disk. Every
//! well-known path this module hands out (`ca_cert_path`,
//! `leaf_cert_path`, etc.) resolves through that stable `current`
//! symlink, so a crash mid-mint can leave at most an orphaned
//! `generations/<gen-id>/` directory -- it can never leave the active
//! generation mixed (e.g. a new leaf paired with a stale CA cert that
//! the operator already trusts, or vice versa). This was chosen over
//! verifying the leaf's signature against the stored CA at load time
//! (which would need `rustls-webpki` as a new direct dependency and
//! still leaves the "how did the mismatch happen" question to the
//! write path) because it prevents the mixed state from ever being
//! constructed in the first place, rather than detecting it after the
//! fact. Non-Unix targets (no unprivileged symlinks) fall back to the
//! original flat layout of write-tmp-then-rename per file; that
//! retains today's single-file atomicity but not the five-file set
//! atomicity, matching this module's existing Unix-first hardening
//! posture (see `write_secret`'s non-Unix fallback below).
//!
//! Regardless of layout, `load_or_mint` treats stored material as
//! invalid -- and re-mints rather than erroring -- when it is missing,
//! expired, or fails to parse as PEM/DER; `load_or_create` additionally
//! re-mints once if the loaded material fails to build into a working
//! `rustls::ServerConfig` (a corrupt-but-parseable leaf/key pair,
//! for instance). Every one of those paths is logged loudly before
//! falling through, and an unrecoverable regeneration failure is
//! always logged via `log_regen_failure` before being returned.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveTime, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use tokio_rustls::TlsAcceptor;

const CA_KEY_FILE: &str = "mitm-ca-key.pem";
const CA_CERT_FILE: &str = "mitm-ca-cert.pem";
const LEAF_KEY_FILE: &str = "mitm-leaf-key.pem";
const LEAF_CERT_FILE: &str = "mitm-leaf-cert.pem";
const META_FILE: &str = "mitm-cert-meta.toml";

/// Mode bits for private-key material written to disk.
const SECRET_MODE: u32 = 0o600;
/// Mode bits for certificate/metadata material written to disk --
/// world-readable is fine, these carry no secret.
const PUBLIC_MODE: u32 = 0o644;

/// Subdirectory (Unix layout) holding one directory per mint attempt.
#[cfg(unix)]
const GENERATIONS_DIR: &str = "generations";
/// Stable symlink (Unix layout) pointing at the active generation
/// directory; every well-known path resolves through this.
#[cfg(unix)]
const CURRENT_LINK: &str = "current";
/// Staging name for the symlink swap. Fixed (not per-attempt-random)
/// because it never holds secret content -- it is just a pointer --
/// so a leftover from a crashed prior attempt is safe to unlink and
/// retry rather than needing a fresh name each time.
#[cfg(unix)]
const CURRENT_TMP_LINK: &str = "current.tmp";

/// Both the CA and the leaf are "long-lived" per the task spec: this is
/// a locally-generated, operator-installed root, not a publicly
/// trusted one, so it is not bound by the ~398-day lifetime browsers
/// enforce for public CAs.
const CA_VALIDITY_DAYS: i64 = 3650;
const LEAF_VALIDITY_DAYS: i64 = 3650;
/// Backdate `not_before` by a day so a client with a slightly slow
/// clock never sees a "not yet valid" leaf.
const NOT_BEFORE_BACKDATE_DAYS: i64 = 1;

const CA_COMMON_NAME: &str = "routectl local MITM CA";

const ALPN_HTTP1: &[u8] = b"http/1.1";

/// Errors from the CA + leaf certificate lifecycle. All variants carry
/// enough context (path, underlying error) for an operator to act on
/// without re-running with extra logging.
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("failed to create MITM cert directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write MITM cert material to {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read MITM cert material from {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize MITM cert metadata: {0}")]
    MetaEncode(#[from] toml::ser::Error),
    #[error("failed to generate MITM certificate material: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("failed to parse stored MITM {kind} PEM material at {path}: {source}")]
    Pem {
        kind: &'static str,
        path: PathBuf,
        source: rustls_pki_types::pem::Error,
    },
    #[error("failed to build MITM TLS server config: {0}")]
    Tls(#[from] rustls::Error),
}

/// Sidecar bookkeeping stored next to the PEM files. `not_after_unix`
/// fields mirror the exact instant baked into the corresponding X.509
/// cert at mint time (see the module doc for why we track expiry this
/// way instead of re-parsing the DER, and for the midnight-UTC
/// truncation both this struct and the DER share).
#[derive(Debug, Serialize, Deserialize)]
struct CertMeta {
    mitm_host: String,
    ca_not_after_unix: i64,
    leaf_not_after_unix: i64,
}

/// Leaf + CA cert (public, for parseability self-checks) + metadata,
/// loaded from disk or freshly minted. Never holds the CA private key
/// -- nothing after mint time needs it, since `regenerate` always
/// re-mints a fresh CA alongside a fresh leaf rather than reusing an
/// existing CA to sign a new leaf.
struct StoredMaterial {
    meta: CertMeta,
    ca_cert_pem: String,
    leaf_cert_pem: String,
    leaf_key_pem: String,
}

/// Freshly generated CA + leaf, not yet written to disk.
struct MintedMaterial {
    ca_cert_pem: String,
    ca_key_pem: String,
    leaf_cert_pem: String,
    leaf_key_pem: String,
    ca_not_after_unix: i64,
    leaf_not_after_unix: i64,
}

#[cfg(unix)]
fn material_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join(CURRENT_LINK)
}

#[cfg(not(unix))]
fn material_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.to_path_buf()
}

// Used unconditionally by tests (mode-bits, file-existence checks) but
// only by production code on the non-Unix flat layout, where the
// caller still needs to name the CA key's on-disk path explicitly
// (the Unix generation-dir layout builds that path inline instead).
#[cfg_attr(unix, allow(dead_code))]
fn ca_key_path(cert_dir: &Path) -> PathBuf {
    material_dir(cert_dir).join(CA_KEY_FILE)
}

/// Path to the CA certificate, for the operator to trust (e.g. via
/// `NODE_EXTRA_CA_CERTS`, surfaced by `rc env`). Pure path arithmetic;
/// does not touch disk or require the CA to already exist. Stable
/// across regenerations even though the file it resolves to (via the
/// `current` symlink on Unix) changes generation on every re-mint.
pub fn ca_cert_path(cert_dir: &Path) -> PathBuf {
    material_dir(cert_dir).join(CA_CERT_FILE)
}

fn leaf_key_path(cert_dir: &Path) -> PathBuf {
    material_dir(cert_dir).join(LEAF_KEY_FILE)
}

fn leaf_cert_path(cert_dir: &Path) -> PathBuf {
    material_dir(cert_dir).join(LEAF_CERT_FILE)
}

fn meta_path(cert_dir: &Path) -> PathBuf {
    material_dir(cert_dir).join(META_FILE)
}

static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();

/// Installs the process-wide rustls default crypto provider exactly
/// once. Safe to call repeatedly and safe to race with any other
/// installer (e.g. a client-side rustls user elsewhere in the binary):
/// `install_default` itself is idempotent by contract (an `Err` just
/// means someone else already installed one), and the `OnceLock` keeps
/// us from even attempting it more than once per process.
fn ensure_crypto_provider_installed() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn calendar_date(at: DateTime<Utc>) -> (i32, u8, u8) {
    (at.year(), at.month() as u8, at.day() as u8)
}

/// Truncates `at` to midnight UTC of its own calendar day -- the exact
/// instant `rcgen::date_time_ymd` bakes into the DER for that day
/// (`Time::MIDNIGHT`, always UTC). Used so the sidecar's `not_after`
/// mirrors the DER's real `notAfter` exactly rather than the untruncated
/// `now + N days` instant, which can be up to ~24h later than what the
/// cert actually expires at.
fn midnight_utc(at: DateTime<Utc>) -> DateTime<Utc> {
    at.date_naive().and_time(NaiveTime::MIN).and_utc()
}

fn mint(mitm_host: &str) -> Result<MintedMaterial, CaError> {
    let now = Utc::now();
    let not_before = now - ChronoDuration::days(NOT_BEFORE_BACKDATE_DAYS);
    let ca_not_after = midnight_utc(now + ChronoDuration::days(CA_VALIDITY_DAYS));
    let leaf_not_after = midnight_utc(now + ChronoDuration::days(LEAF_VALIDITY_DAYS));

    let (nb_y, nb_m, nb_d) = calendar_date(not_before);
    let (ca_y, ca_m, ca_d) = calendar_date(ca_not_after);
    let (leaf_y, leaf_m, leaf_d) = calendar_date(leaf_not_after);

    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.not_before = rcgen::date_time_ymd(nb_y, nb_m, nb_d);
    ca_params.not_after = rcgen::date_time_ymd(ca_y, ca_m, ca_d);
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(DnType::CommonName, CA_COMMON_NAME);
    ca_params.distinguished_name = ca_dn;
    let ca_cert = ca_params.self_signed(&ca_key)?;

    let leaf_key = KeyPair::generate()?;
    let mut leaf_params = CertificateParams::new(vec![mitm_host.to_string()])?;
    leaf_params.not_before = rcgen::date_time_ymd(nb_y, nb_m, nb_d);
    leaf_params.not_after = rcgen::date_time_ymd(leaf_y, leaf_m, leaf_d);
    leaf_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let mut leaf_dn = DistinguishedName::new();
    leaf_dn.push(DnType::CommonName, mitm_host);
    leaf_params.distinguished_name = leaf_dn;
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer)?;

    Ok(MintedMaterial {
        ca_cert_pem: ca_cert.pem(),
        ca_key_pem: ca_key.serialize_pem(),
        leaf_cert_pem: leaf_cert.pem(),
        leaf_key_pem: leaf_key.serialize_pem(),
        ca_not_after_unix: ca_not_after.timestamp(),
        leaf_not_after_unix: leaf_not_after.timestamp(),
    })
}

/// Opens `path` for a brand-new secret or public file: `create_new`
/// (std) already guarantees POSIX `O_EXCL` semantics, which fail the
/// open with `EEXIST` if `path` exists in any form -- including a
/// symlink, dangling or not, regardless of where it points -- so this
/// alone rules out following a pre-planted symlink or reusing a
/// pre-existing inode. `O_NOFOLLOW` is added on Unix as explicit,
/// filesystem-independent belt-and-suspenders on top of that
/// guarantee. Mode is set atomically at creation (`OpenOptions::mode`)
/// rather than via a separate `chmod` afterward, so a secret file is
/// never briefly world-readable.
#[cfg(unix)]
fn create_new_secure(path: &Path, mode: u32) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn create_new_secure(path: &Path, _mode: u32) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

fn write_new_secure(path: &Path, contents: &str, mode: u32) -> Result<(), CaError> {
    use std::io::Write as _;

    let mut file = create_new_secure(path, mode).map_err(|source| CaError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(contents.as_bytes())
        .map_err(|source| CaError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// `<dest>` with a `.tmp` suffix appended, used as the staging path for
/// the write-then-rename sequence in the non-Unix `mint_and_store`.
#[cfg(not(unix))]
fn tmp_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(not(unix))]
fn rename_into_place(tmp: &Path, dest: &Path) -> Result<(), CaError> {
    fs::rename(tmp, dest).map_err(|source| CaError::Write {
        path: dest.to_path_buf(),
        source,
    })
}

/// Writes a staged file at a predictable `.tmp` path (non-Unix layout
/// only). A predictable path can carry a stale leftover from a prior
/// crashed mint attempt, so any existing entry there is unlinked first
/// -- safe because `remove_file` only ever removes the directory entry
/// itself, never dereferencing a symlink -- before creating fresh via
/// `create_new_secure`. If something reappears at that path between
/// the unlink and the create (a race, not a legitimate stale leftover),
/// `create_new` fails closed rather than following or overwriting it.
#[cfg(not(unix))]
fn write_staged_secure(path: &Path, contents: &str, mode: u32) -> Result<(), CaError> {
    let _ = fs::remove_file(path);
    write_new_secure(path, contents, mode)
}

/// Mints a fresh CA + leaf under a brand-new, unpredictably-named
/// `generations/<gen-id>/` directory, then atomically swaps the
/// `current` symlink to point at it. See the module doc for why this
/// layout -- rather than an in-place five-file write-then-rename --
/// is what makes the published generation always internally
/// consistent, even if the process crashes mid-mint.
#[cfg(unix)]
fn mint_and_store(cert_dir: &Path, mitm_host: &str) -> Result<StoredMaterial, CaError> {
    let generations_root = cert_dir.join(GENERATIONS_DIR);
    fs::create_dir_all(&generations_root).map_err(|source| CaError::CreateDir {
        path: generations_root.clone(),
        source,
    })?;

    let minted = mint(mitm_host)?;
    let meta = CertMeta {
        mitm_host: mitm_host.to_string(),
        ca_not_after_unix: minted.ca_not_after_unix,
        leaf_not_after_unix: minted.leaf_not_after_unix,
    };
    let meta_toml = toml::to_string_pretty(&meta)?;

    let gen_name = format!("gen-{}", uuid::Uuid::new_v4());
    let gen_dir = generations_root.join(&gen_name);
    fs::create_dir(&gen_dir).map_err(|source| CaError::CreateDir {
        path: gen_dir.clone(),
        source,
    })?;

    write_new_secure(&gen_dir.join(CA_KEY_FILE), &minted.ca_key_pem, SECRET_MODE)?;
    write_new_secure(
        &gen_dir.join(CA_CERT_FILE),
        &minted.ca_cert_pem,
        PUBLIC_MODE,
    )?;
    write_new_secure(
        &gen_dir.join(LEAF_KEY_FILE),
        &minted.leaf_key_pem,
        SECRET_MODE,
    )?;
    write_new_secure(
        &gen_dir.join(LEAF_CERT_FILE),
        &minted.leaf_cert_pem,
        PUBLIC_MODE,
    )?;
    write_new_secure(&gen_dir.join(META_FILE), &meta_toml, PUBLIC_MODE)?;

    swap_current_pointer(cert_dir, &gen_name)?;
    prune_stale_generations(&generations_root, &gen_name);

    Ok(StoredMaterial {
        meta,
        ca_cert_pem: minted.ca_cert_pem,
        leaf_cert_pem: minted.leaf_cert_pem,
        leaf_key_pem: minted.leaf_key_pem,
    })
}

/// Mints a fresh CA + leaf and stores it under `cert_dir`, overwriting
/// whatever was there. Every file is first written to a `.tmp` sibling
/// and only renamed into place once all five writes have succeeded.
/// Unlike the Unix layout, the five renames below are not one atomic
/// unit, so a crash between them can still leave a mixed set on this
/// fallback path (no unprivileged symlink primitive to swap instead).
#[cfg(not(unix))]
fn mint_and_store(cert_dir: &Path, mitm_host: &str) -> Result<StoredMaterial, CaError> {
    fs::create_dir_all(cert_dir).map_err(|source| CaError::CreateDir {
        path: cert_dir.to_path_buf(),
        source,
    })?;

    let minted = mint(mitm_host)?;
    let meta = CertMeta {
        mitm_host: mitm_host.to_string(),
        ca_not_after_unix: minted.ca_not_after_unix,
        leaf_not_after_unix: minted.leaf_not_after_unix,
    };
    let meta_toml = toml::to_string_pretty(&meta)?;

    let ca_key_dest = ca_key_path(cert_dir);
    let ca_cert_dest = ca_cert_path(cert_dir);
    let leaf_key_dest = leaf_key_path(cert_dir);
    let leaf_cert_dest = leaf_cert_path(cert_dir);
    let meta_dest = meta_path(cert_dir);

    let ca_key_tmp = tmp_path(&ca_key_dest);
    let ca_cert_tmp = tmp_path(&ca_cert_dest);
    let leaf_key_tmp = tmp_path(&leaf_key_dest);
    let leaf_cert_tmp = tmp_path(&leaf_cert_dest);
    let meta_tmp = tmp_path(&meta_dest);

    write_staged_secure(&ca_key_tmp, &minted.ca_key_pem, SECRET_MODE)?;
    write_staged_secure(&ca_cert_tmp, &minted.ca_cert_pem, PUBLIC_MODE)?;
    write_staged_secure(&leaf_key_tmp, &minted.leaf_key_pem, SECRET_MODE)?;
    write_staged_secure(&leaf_cert_tmp, &minted.leaf_cert_pem, PUBLIC_MODE)?;
    write_staged_secure(&meta_tmp, &meta_toml, PUBLIC_MODE)?;

    rename_into_place(&ca_key_tmp, &ca_key_dest)?;
    rename_into_place(&ca_cert_tmp, &ca_cert_dest)?;
    rename_into_place(&leaf_key_tmp, &leaf_key_dest)?;
    rename_into_place(&leaf_cert_tmp, &leaf_cert_dest)?;
    rename_into_place(&meta_tmp, &meta_dest)?;

    Ok(StoredMaterial {
        meta,
        ca_cert_pem: minted.ca_cert_pem,
        leaf_cert_pem: minted.leaf_cert_pem,
        leaf_key_pem: minted.leaf_key_pem,
    })
}

/// Atomically publishes `gen_name` as the active generation by
/// swapping the `current` symlink. The staging name is fixed (not
/// per-attempt-random): it holds no secret content, so a leftover from
/// a crashed prior swap is safe to unlink and retry. `symlink` itself
/// fails closed (`EEXIST`) if something reappears at that path between
/// the unlink and the create.
#[cfg(unix)]
fn swap_current_pointer(cert_dir: &Path, gen_name: &str) -> Result<(), CaError> {
    let tmp_link = cert_dir.join(CURRENT_TMP_LINK);
    let current_link = cert_dir.join(CURRENT_LINK);
    let target = Path::new(GENERATIONS_DIR).join(gen_name);

    let _ = fs::remove_file(&tmp_link);

    std::os::unix::fs::symlink(&target, &tmp_link).map_err(|source| CaError::Write {
        path: tmp_link.clone(),
        source,
    })?;

    fs::rename(&tmp_link, &current_link).map_err(|source| CaError::Write {
        path: current_link,
        source,
    })
}

/// Best-effort garbage collection of orphaned generation directories
/// (superseded generations, or ones abandoned by a crash before the
/// pointer swap). Never fatal to the mint that just succeeded -- a
/// failure here just means cleanup happens on a later mint instead.
#[cfg(unix)]
fn prune_stale_generations(generations_root: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(generations_root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != std::ffi::OsStr::new(keep) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Loads existing cert material from `cert_dir` if present, parseable,
/// and minted for `mitm_host`. Returns `Ok(None)` (never an error) for
/// any of "nothing there yet", "metadata corrupt", or "host changed
/// since last mint" -- all three are self-healing by falling through
/// to a fresh mint, not operator-facing failures.
fn load_existing(cert_dir: &Path, mitm_host: &str) -> Result<Option<StoredMaterial>, CaError> {
    let paths = [
        meta_path(cert_dir),
        leaf_cert_path(cert_dir),
        leaf_key_path(cert_dir),
        ca_cert_path(cert_dir),
    ];
    if !paths.iter().all(|p| p.exists()) {
        return Ok(None);
    }

    let meta_raw = fs::read_to_string(meta_path(cert_dir)).map_err(|source| CaError::Read {
        path: meta_path(cert_dir),
        source,
    })?;
    let meta: CertMeta = match toml::from_str(&meta_raw) {
        Ok(meta) => meta,
        Err(error) => {
            tracing::warn!(
                path = %meta_path(cert_dir).display(),
                %error,
                "MITM cert metadata is corrupt; regenerating"
            );
            return Ok(None);
        }
    };

    if meta.mitm_host != mitm_host {
        tracing::info!(
            configured_host = %mitm_host,
            stored_host = %meta.mitm_host,
            "MITM host changed since certs were last minted; regenerating"
        );
        return Ok(None);
    }

    let ca_cert_pem =
        fs::read_to_string(ca_cert_path(cert_dir)).map_err(|source| CaError::Read {
            path: ca_cert_path(cert_dir),
            source,
        })?;
    let leaf_cert_pem =
        fs::read_to_string(leaf_cert_path(cert_dir)).map_err(|source| CaError::Read {
            path: leaf_cert_path(cert_dir),
            source,
        })?;
    let leaf_key_pem =
        fs::read_to_string(leaf_key_path(cert_dir)).map_err(|source| CaError::Read {
            path: leaf_key_path(cert_dir),
            source,
        })?;

    Ok(Some(StoredMaterial {
        meta,
        ca_cert_pem,
        leaf_cert_pem,
        leaf_key_pem,
    }))
}

fn is_expired(meta: &CertMeta) -> bool {
    let now = Utc::now().timestamp();
    now >= meta.ca_not_after_unix || now >= meta.leaf_not_after_unix
}

/// Whether every PEM in `material` parses as valid DER. Deliberately
/// only checks parseability (not that the leaf chains to the CA, or
/// that the leaf/key pair match) -- the Unix on-disk layout already
/// makes a mismatched pair structurally impossible to publish (see the
/// module doc), and `load_or_create` catches a parseable-but-broken
/// pair when it fails to build a `rustls::ServerConfig`.
fn is_parseable(material: &StoredMaterial) -> bool {
    CertificateDer::from_pem_slice(material.ca_cert_pem.as_bytes()).is_ok()
        && CertificateDer::from_pem_slice(material.leaf_cert_pem.as_bytes()).is_ok()
        && PrivateKeyDer::from_pem_slice(material.leaf_key_pem.as_bytes()).is_ok()
}

/// Loads valid existing cert material, or mints fresh material when
/// none exists, it is minted for a different host, it has expired, or
/// it fails to parse. A mint failure at any of those points is logged
/// loudly at `error` level before being returned -- this is the
/// operator-facing "regeneration is impossible" path, since it means
/// the MITM listener cannot start.
fn load_or_mint(cert_dir: &Path, mitm_host: &str) -> Result<StoredMaterial, CaError> {
    match load_existing(cert_dir, mitm_host)? {
        Some(existing) if is_expired(&existing.meta) => {
            tracing::warn!(cert_dir = %cert_dir.display(), "MITM cert material expired; regenerating");
            mint_and_store(cert_dir, mitm_host).map_err(log_regen_failure(cert_dir))
        }
        Some(existing) if !is_parseable(&existing) => {
            tracing::warn!(
                cert_dir = %cert_dir.display(),
                "MITM cert material on disk is corrupt or unparseable; regenerating"
            );
            mint_and_store(cert_dir, mitm_host).map_err(log_regen_failure(cert_dir))
        }
        Some(existing) => {
            tracing::debug!(cert_dir = %cert_dir.display(), "MITM cert material is valid; reusing");
            Ok(existing)
        }
        None => {
            tracing::info!(cert_dir = %cert_dir.display(), "no MITM cert material found; generating");
            mint_and_store(cert_dir, mitm_host).map_err(log_regen_failure(cert_dir))
        }
    }
}

fn log_regen_failure(cert_dir: &Path) -> impl FnOnce(CaError) -> CaError + '_ {
    move |err| {
        tracing::error!(
            cert_dir = %cert_dir.display(),
            error = %err,
            "failed to generate the MITM CA and leaf certificate; the MITM proxy cannot start until this is resolved"
        );
        err
    }
}

fn build_acceptor(cert_dir: &Path, material: &StoredMaterial) -> Result<TlsAcceptor, CaError> {
    ensure_crypto_provider_installed();

    let leaf_der =
        CertificateDer::from_pem_slice(material.leaf_cert_pem.as_bytes()).map_err(|source| {
            CaError::Pem {
                kind: "leaf certificate",
                path: leaf_cert_path(cert_dir),
                source,
            }
        })?;
    let key_der =
        PrivateKeyDer::from_pem_slice(material.leaf_key_pem.as_bytes()).map_err(|source| {
            CaError::Pem {
                kind: "leaf private key",
                path: leaf_key_path(cert_dir),
                source,
            }
        })?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![leaf_der], key_der)?;
    config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Loads or mints the CA + leaf pair for `mitm_host` under `cert_dir`
/// and returns a ready-to-use TLS acceptor advertising only
/// `http/1.1` via ALPN. Idempotent: an existing, valid, matching-host
/// leaf is reused as-is; nothing is re-minted unless it is missing,
/// corrupt, minted for a different host, or expired.
///
/// If the loaded material fails to build into a working TLS config --
/// parseable individually but broken as a pair, e.g. a leaf/key
/// mismatch that `is_parseable` cannot see -- this re-mints exactly
/// once more before giving up, logging loudly both before the retry
/// and (via `log_regen_failure`) if the retry itself fails. This is
/// the loud-log-before-failing contract holding on the reload path,
/// not just the mint path.
pub fn load_or_create(cert_dir: &Path, mitm_host: &str) -> Result<TlsAcceptor, CaError> {
    let material = load_or_mint(cert_dir, mitm_host)?;
    match build_acceptor(cert_dir, &material) {
        Ok(acceptor) => Ok(acceptor),
        Err(error) => {
            tracing::warn!(
                cert_dir = %cert_dir.display(),
                %error,
                "loaded MITM cert material failed to build a TLS server config; regenerating"
            );
            let material =
                mint_and_store(cert_dir, mitm_host).map_err(log_regen_failure(cert_dir))?;
            build_acceptor(cert_dir, &material)
        }
    }
}

/// Unconditionally re-mints the CA + leaf pair for `mitm_host` under
/// `cert_dir`, overwriting whatever was there, and returns the CA
/// certificate path. Used by `rc regen-ca` to let an operator force a
/// rotation.
pub fn regenerate(cert_dir: &Path, mitm_host: &str) -> Result<PathBuf, CaError> {
    mint_and_store(cert_dir, mitm_host).map_err(log_regen_failure(cert_dir))?;
    Ok(ca_cert_path(cert_dir))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::TlsConnector;

    use super::*;

    const HOST: &str = "api.anthropic.com";

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn generate_creates_all_expected_files() {
        let dir = tempfile::tempdir().unwrap();
        let _acceptor = load_or_create(dir.path(), HOST).unwrap();

        assert!(ca_key_path(dir.path()).exists());
        assert!(ca_cert_path(dir.path()).exists());
        assert!(leaf_key_path(dir.path()).exists());
        assert!(leaf_cert_path(dir.path()).exists());
        assert!(meta_path(dir.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn ca_private_key_file_is_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let _acceptor = load_or_create(dir.path(), HOST).unwrap();

        assert_eq!(file_mode(&ca_key_path(dir.path())), 0o600);
        assert_eq!(file_mode(&leaf_key_path(dir.path())), 0o600);
    }

    #[test]
    fn reload_is_idempotent_and_does_not_remint() {
        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after_first = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        let _second = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after_second = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        assert_eq!(leaf_after_first, leaf_after_second);
    }

    #[test]
    fn different_mitm_host_triggers_remint() {
        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after_first = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        let _second = load_or_create(dir.path(), "other.example.com").unwrap();
        let leaf_after_second = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        assert_ne!(leaf_after_first, leaf_after_second);
    }

    /// Synthesizes an expired cert by overwriting the sidecar metadata
    /// with a `not_after` in the past -- see the module doc for why
    /// expiry is metadata-driven rather than DER-parsing-driven.
    fn expire_stored_meta(dir: &Path) {
        let mut meta: CertMeta =
            toml::from_str(&fs::read_to_string(meta_path(dir)).unwrap()).unwrap();
        let past = Utc::now().timestamp() - 1;
        meta.ca_not_after_unix = past;
        meta.leaf_not_after_unix = past;
        fs::write(meta_path(dir), toml::to_string_pretty(&meta).unwrap()).unwrap();
    }

    #[test]
    fn expired_cert_triggers_silent_regen() {
        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after_first = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        expire_stored_meta(dir.path());

        let _second = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after_second = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        assert_ne!(
            leaf_after_first, leaf_after_second,
            "expired material should have been re-minted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn expired_cert_with_unwritable_dir_is_a_loud_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        expire_stored_meta(dir.path());

        // The generation subdirectory itself remains writable (only
        // cert_dir's own entries -- the `current`/`current.tmp`
        // symlinks -- are blocked), so the regen attempt succeeds all
        // the way through minting a fresh generation and only fails
        // when it tries to flip the pointer. That failure must still
        // surface loudly rather than silently keep serving the
        // expired leaf.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = load_or_create(dir.path(), HOST);
        // Always restore write permission before the tempdir's Drop
        // tries to clean up, regardless of the assertion outcome.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            result.is_err(),
            "regeneration blocked from publishing its result must fail loudly, not silently reuse an expired cert"
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_remint_preserves_previous_generation() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        let leaf_before = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();
        let ca_before = fs::read_to_string(ca_cert_path(dir.path())).unwrap();

        expire_stored_meta(dir.path());

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = load_or_create(dir.path(), HOST);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            result.is_err(),
            "a remint that cannot flip the pointer must fail, not silently serve a half-written generation"
        );

        let leaf_after = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();
        let ca_after = fs::read_to_string(ca_cert_path(dir.path())).unwrap();
        assert_eq!(
            leaf_before, leaf_after,
            "a failed remint must never disturb the previously active generation"
        );
        assert_eq!(
            ca_before, ca_after,
            "a failed remint must never disturb the previously active generation"
        );
    }

    #[test]
    fn corrupt_leaf_cert_triggers_remint() {
        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        let leaf_before = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        fs::write(leaf_cert_path(dir.path()), "not a valid pem").unwrap();

        let _second = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        assert_ne!(
            leaf_before, leaf_after,
            "corrupt leaf material should have been re-minted"
        );
        assert!(
            CertificateDer::from_pem_slice(leaf_after.as_bytes()).is_ok(),
            "re-minted leaf material must be valid PEM again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn secure_create_rejects_preexisting_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real_target = dir.path().join("real-secret");
        fs::write(&real_target, b"do-not-overwrite").unwrap();
        let staged_path = dir.path().join("staged-secret");
        std::os::unix::fs::symlink(&real_target, &staged_path).unwrap();

        let result = create_new_secure(&staged_path, SECRET_MODE);

        assert!(
            result.is_err(),
            "must not follow or overwrite a pre-existing symlink at the staged path"
        );
        assert_eq!(
            fs::read_to_string(&real_target).unwrap(),
            "do-not-overwrite",
            "the symlink's target must be untouched"
        );
    }

    #[test]
    fn sidecar_not_after_matches_der_real_not_after() {
        let dir = tempfile::tempdir().unwrap();
        let _acceptor = load_or_create(dir.path(), HOST).unwrap();
        let meta: CertMeta =
            toml::from_str(&fs::read_to_string(meta_path(dir.path())).unwrap()).unwrap();

        let leaf_not_after = DateTime::<Utc>::from_timestamp(meta.leaf_not_after_unix, 0).unwrap();
        let (y, m, d) = calendar_date(leaf_not_after);
        let expected_der_not_after = rcgen::date_time_ymd(y, m, d).unix_timestamp();

        assert_eq!(
            meta.leaf_not_after_unix, expected_der_not_after,
            "sidecar not_after must exactly match the midnight-UTC instant rcgen bakes into the DER"
        );

        let ca_not_after = DateTime::<Utc>::from_timestamp(meta.ca_not_after_unix, 0).unwrap();
        let (ca_y, ca_m, ca_d) = calendar_date(ca_not_after);
        let expected_ca_der_not_after = rcgen::date_time_ymd(ca_y, ca_m, ca_d).unix_timestamp();
        assert_eq!(meta.ca_not_after_unix, expected_ca_der_not_after);
    }

    #[test]
    fn regenerate_remints_and_returns_ca_path() {
        let dir = tempfile::tempdir().unwrap();
        let _first = load_or_create(dir.path(), HOST).unwrap();
        let leaf_after_first = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        let returned_path = regenerate(dir.path(), HOST).unwrap();
        let leaf_after_regen = fs::read_to_string(leaf_cert_path(dir.path())).unwrap();

        assert_eq!(returned_path, ca_cert_path(dir.path()));
        assert_ne!(leaf_after_first, leaf_after_regen);
    }

    /// Full client/server TLS handshake over a loopback TCP socket: the
    /// leaf minted by `load_or_create` must validate when the client's
    /// only trust anchor is the CA written alongside it, and the
    /// negotiated ALPN protocol must be exactly `http/1.1`.
    #[tokio::test]
    async fn leaf_validates_against_ca_and_negotiates_http1_alpn() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = load_or_create(dir.path(), HOST).unwrap();
        let ca_pem = fs::read_to_string(ca_cert_path(dir.path())).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.unwrap();
            let negotiated_alpn = tls.get_ref().1.alpn_protocol().map(|proto| proto.to_vec());
            let mut buf = [0u8; 5];
            tls.read_exact(&mut buf).await.unwrap();
            tls.write_all(b"world").await.unwrap();
            negotiated_alpn
        });

        let mut root_store = rustls::RootCertStore::empty();
        let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap();
        root_store.add(ca_der).unwrap();
        let mut client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
        let connector = TlsConnector::from(Arc::new(client_config));

        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = ServerName::try_from(HOST).unwrap().to_owned();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();
        tls.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
        assert_eq!(
            tls.get_ref().1.alpn_protocol(),
            Some(ALPN_HTTP1),
            "client side should also observe the negotiated http/1.1 ALPN"
        );

        let negotiated_alpn = server.await.unwrap();
        assert_eq!(negotiated_alpn, Some(ALPN_HTTP1.to_vec()));
    }
}
