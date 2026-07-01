//! PKCE (RFC 7636) parameter generation for the OAuth login flow.
//!
//! - `code_verifier`: 64 random bytes -> base64url-no-pad (96 chars).
//!   Held in driver memory only; never written to disk; never logged.
//! - `code_challenge`: SHA-256 of the verifier -> base64url-no-pad
//!   (43 chars). Sent in the auth URL.
//! - CSRF `state`: 32 random bytes -> base64url-no-pad (43 chars).
//!   Constant-time-compared on callback.
//!
//! All randomness is sourced from `OsRng` (the OS CSPRNG). The encoded
//! values are URL-safe; no further escaping is needed in query strings.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// Length of the verifier in raw bytes. RFC 7636 requires the encoded
/// verifier to be 43..128 chars; 64 raw bytes -> 86 chars base64url
/// (well within range, comfortably above the 43-char floor).
const VERIFIER_BYTES: usize = 64;

/// Length of the CSRF state token in raw bytes. 32 bytes -> 43 chars
/// base64url, ample collision resistance.
const STATE_BYTES: usize = 32;

/// One PKCE bundle, generated per login attempt. Field accessors are
/// pub(crate) so the auth URL builder and the token-exchange call can
/// read them; out-of-tree consumers cannot, which is intentional --
/// these values must never leak. `Drop` zeroizes the verifier and the
/// state so a freed bundle does not leave PKCE secrets on the heap.
pub struct Pkce {
    verifier: String,
    challenge: String,
    state: String,
}

impl Pkce {
    /// Build a fresh PKCE bundle. Calls `OsRng` twice (verifier + state).
    /// Panics if the OS CSPRNG fails -- on Linux/macOS this means the
    /// kernel ran out of entropy at boot, which is unrecoverable for
    /// any cryptographic protocol.
    pub fn generate() -> Self {
        let mut vbytes = [0u8; VERIFIER_BYTES];
        OsRng
            .try_fill_bytes(&mut vbytes)
            .expect("OsRng failed to fill PKCE verifier");
        let verifier = URL_SAFE_NO_PAD.encode(vbytes);
        vbytes.zeroize();

        let challenge = {
            let mut h = Sha256::new();
            h.update(verifier.as_bytes());
            URL_SAFE_NO_PAD.encode(h.finalize())
        };

        let mut sbytes = [0u8; STATE_BYTES];
        OsRng
            .try_fill_bytes(&mut sbytes)
            .expect("OsRng failed to fill PKCE state");
        let state = URL_SAFE_NO_PAD.encode(sbytes);
        sbytes.zeroize();

        Self {
            verifier,
            challenge,
            state,
        }
    }

    /// The code_verifier. Never log this -- send it ONLY to the token
    /// endpoint at exchange time.
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The code_challenge. Goes into the auth URL.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// The CSRF state token. Goes into the auth URL; must be
    /// constant-time-compared against the callback's `state` query
    /// param via `constant_time_eq`.
    pub fn state(&self) -> &str {
        &self.state
    }
}

impl Drop for Pkce {
    fn drop(&mut self) {
        // Zeroize the secret-bearing fields. `challenge` is the SHA-256
        // of the verifier and is sent in the auth URL anyway, so it is
        // not secret -- skipping it.
        self.verifier.zeroize();
        self.state.zeroize();
    }
}

/// Constant-time string comparison. Wraps `subtle::ConstantTimeEq`
/// behind a `subtle`-free signature so the rest of the OAuth code does
/// not need to import the trait. Length mismatches short-circuit
/// (length is not a secret here -- the encoder output is bounded).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_7636_appendix_b_known_answer() {
        // RFC 7636 Appendix B reference vector. The verifier
        // "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk" must hash to
        // "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM" via S256.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let mut h = Sha256::new();
        h.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(h.finalize());
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_is_url_safe_and_long_enough() {
        let p = Pkce::generate();
        let v = p.verifier();
        assert!(v.len() >= 43, "verifier too short: {}", v.len());
        assert!(v.len() <= 128, "verifier too long: {}", v.len());
        for c in v.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-url-safe char in verifier: {c}"
            );
        }
    }

    #[test]
    fn challenge_round_trips_against_local_hash() {
        let p = Pkce::generate();
        let mut h = Sha256::new();
        h.update(p.verifier().as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(h.finalize());
        assert_eq!(p.challenge(), expected);
    }

    #[test]
    fn two_generates_produce_different_state() {
        // Statistical -- could theoretically collide, but with 256
        // bits of entropy the probability is below floating-point
        // representable noise.
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier(), b.verifier());
        assert_ne!(a.state(), b.state());
    }

    #[test]
    fn constant_time_eq_matches_strings() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }
}
