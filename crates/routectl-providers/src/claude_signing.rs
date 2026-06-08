//! Body re-signer for the Claude Code billing-header checksum.
//!
//! The canonical body is mutated on the egress path (reasoning-effort
//! injection, tool-id charset sanitize, signature strip). Any checksum
//! the ingress client computed over the original body is invalidated by
//! those mutations, so the bytes we transmit would carry a stale, wrong
//! checksum. This module re-signs an EXISTING billing block in place so
//! the bytes transmitted are the bytes hashed.
//!
//! Path A only: it re-signs a block the client already produced. It never
//! synthesizes a billing block where none exists -- every shape that does
//! not match is a silent no-op.

/// Marker prefix that identifies the billing-header system block. Unique
/// enough in a serialized body that locating it pinpoints the value we
/// must re-sign.
const BILLING_PREFIX: &[u8] = b"x-anthropic-billing-header:";

/// Token that precedes the 5-hex checksum inside the billing block.
const CCH_TOKEN: &[u8] = b"cch=";

/// Serialized boundary that opens the messages array. routectl's body
/// always serializes the `system` array (where the billing block lives)
/// before `messages`, so a billing prefix found at or after this boundary
/// is a false positive inside user-controlled message text, not the real
/// system-level block -- and must not be signed.
const MESSAGES_KEY: &[u8] = b"\"messages\":";

/// Width of the hex checksum (5 lowercase hex chars).
const CCH_HEX_LEN: usize = 5;

/// XXH64 seed (matches the reference implementation bit-for-bit).
const SEED: u64 = 0x6E52_736A_C806_831E;

/// Mask applied to the 64-bit hash before formatting (20 low bits).
const MASK: u64 = 0xFFFFF;

/// Zero-cost compile-time guard: the algorithm requires at least one hex char.
const _: () = assert!(CCH_HEX_LEN > 0, "CCH_HEX_LEN must be non-zero");

/// ASCII '0', the placeholder byte written before hashing.
const ZERO_HEX: u8 = b'0';

/// Re-sign the FIRST `cch=<5 hex>;` token inside an existing billing-header
/// block, in place, so the transmitted bytes match an upstream recompute.
///
/// Algorithm (byte-level, no JSON parse):
///   1. Locate the billing-header prefix in the buffer.
///   2. From there, find the first `cch=` followed by exactly 5 lowercase
///      hex chars and a `;`.
///   3. Overwrite the 5 hex chars with `00000`.
///   4. Hash the WHOLE buffer with XXH64(seed) and mask to 20 bits.
///   5. Write the resulting 5 hex chars back over the `00000`.
///
/// Every non-matching shape (no billing prefix, no cch token, malformed
/// hex run) is a silent no-op. Infallible: never panics, never errors.
/// The buffer length is unchanged.
pub(crate) fn resign_cch_in_place(body: &mut [u8]) {
    let Some(prefix_at) = find_subslice(body, BILLING_PREFIX) else {
        return;
    };

    // Reject a billing prefix that appears inside (or after) the messages
    // array: that is user-controlled text, not the real system-level block.
    if let Some(msg_at) = find_subslice(body, MESSAGES_KEY) {
        if prefix_at >= msg_at {
            return;
        }
    }

    let Some(hex_at) = find_cch_hex_start(body, prefix_at) else {
        return;
    };

    // Zero the 5 hex chars in place, hash the whole buffer, write back.
    for byte in &mut body[hex_at..hex_at + CCH_HEX_LEN] {
        *byte = ZERO_HEX;
    }

    let hash = xxhash_rust::xxh64::xxh64(body, SEED) & MASK;
    write_hex5(&mut body[hex_at..hex_at + CCH_HEX_LEN], hash);
}

/// Find the byte offset of the first occurrence of `needle` in `haystack`,
/// or `None` if absent.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Starting from `from`, find the offset of the 5-hex run of the first
/// well-formed `cch=<5 lowercase hex>;` token. Returns the offset of the
/// first hex char (just past `cch=`), or `None` if no well-formed token
/// follows.
///
/// A malformed first cch token (not exactly 5 lowercase hex chars followed
/// by `;`) is skipped and the next well-formed token is signed. This
/// matches the reference behavior, where the checksum pattern simply does
/// not match a malformed token.
fn find_cch_hex_start(buf: &[u8], from: usize) -> Option<usize> {
    let mut cursor = from;
    while let Some(rel) = find_subslice(&buf[cursor..], CCH_TOKEN) {
        let hex_start = cursor + rel + CCH_TOKEN.len();
        let hex_end = hex_start + CCH_HEX_LEN;
        if hex_end < buf.len()
            && buf[hex_start..hex_end].iter().all(is_lower_hex)
            && buf[hex_end] == b';'
        {
            return Some(hex_start);
        }
        // Malformed token: advance past this `cch=` and keep scanning so
        // a later well-formed token still gets signed.
        cursor = cursor + rel + CCH_TOKEN.len();
    }
    None
}

/// True when `b` is a lowercase hex digit (`0-9` or `a-f`).
fn is_lower_hex(b: &u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// Write `value` as exactly 5 lowercase, zero-padded hex chars into `dst`.
/// `dst` must be at least 5 bytes; callers always pass a 5-byte slice.
fn write_hex5(dst: &mut [u8], value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, slot) in dst.iter_mut().enumerate().take(CCH_HEX_LEN) {
        let shift = 4 * (CCH_HEX_LEN - 1 - i);
        let nibble = ((value >> shift) & 0xF) as usize;
        *slot = HEX[nibble];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal serialized body whose `system[0].text` is a billing
    /// block carrying `cch=<seed>;`. Matches the compact serde_json shape
    /// (no extra whitespace) the send path produces.
    fn billing_body(cch: &str) -> Vec<u8> {
        format!(
            r#"{{"model":"m","system":[{{"type":"text","text":"x-anthropic-billing-header: v=1; cch={cch}; end"}}],"messages":[]}}"#
        )
        .into_bytes()
    }

    /// Extract the 5 hex chars that follow the first `cch=` in `buf`.
    fn read_cch(buf: &[u8]) -> Option<String> {
        let at = find_subslice(buf, BILLING_PREFIX)?;
        let hex = find_cch_hex_start(buf, at)?;
        Some(String::from_utf8(buf[hex..hex + CCH_HEX_LEN].to_vec()).unwrap())
    }

    #[test]
    fn signs_billing_block_to_expected_checksum() {
        // Arrange: start with a NON-zero cch so the zero-out step is
        // exercised. Compute the expected hash independently from a
        // separately-constructed zeroed body to pin the algorithm.
        let mut body = billing_body("fffff");
        let zeroed = billing_body("00000");
        let expected_hash = xxhash_rust::xxh64::xxh64(&zeroed, SEED) & MASK;
        let expected = format!("{expected_hash:05x}");

        // Act
        resign_cch_in_place(&mut body);

        // Assert
        assert_eq!(read_cch(&body).as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn signed_bytes_recompute_to_themselves() {
        // The upstream recompute: zero the signed cch, hash, expect the
        // value that is actually present. This is the round-trip the
        // whole feature exists to guarantee.
        let mut body = billing_body("12345");
        resign_cch_in_place(&mut body);

        let present = read_cch(&body).expect("cch present after signing");

        // Zero it back out and recompute exactly as upstream would.
        let hex_at = find_cch_hex_start(&body, find_subslice(&body, BILLING_PREFIX).unwrap())
            .expect("cch locatable");
        for byte in &mut body[hex_at..hex_at + CCH_HEX_LEN] {
            *byte = ZERO_HEX;
        }
        let recomputed = format!("{:05x}", xxhash_rust::xxh64::xxh64(&body, SEED) & MASK);

        assert_eq!(present, recomputed);
    }

    #[test]
    fn noop_when_no_billing_prefix() {
        let original =
            br#"{"system":[{"type":"text","text":"just a normal prompt cch=12345;"}]}"#.to_vec();
        let mut body = original.clone();

        resign_cch_in_place(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn noop_when_no_system_text() {
        let original = br#"{"model":"m","messages":[]}"#.to_vec();
        let mut body = original.clone();

        resign_cch_in_place(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn noop_when_cch_pattern_missing() {
        let original =
            br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: v=1; end"}]}"#
                .to_vec();
        let mut body = original.clone();

        resign_cch_in_place(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn noop_when_cch_hex_malformed() {
        // `cch=` present but the run is not 5 lowercase hex chars.
        let original =
            br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cch=ZZZZZ;"}]}"#
                .to_vec();
        let mut body = original.clone();

        resign_cch_in_place(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn signs_first_of_multiple_cch_tokens_only() {
        // Two well-formed cch tokens; only the first is re-signed.
        let mut body =
            br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cch=aaaaa; cch=bbbbb;"}],"messages":[]}"#
                .to_vec();

        resign_cch_in_place(&mut body);

        // First token changed away from its placeholder.
        assert_ne!(read_cch(&body).as_deref(), Some("aaaaa"));
        // Second token untouched.
        assert!(find_subslice(&body, b"cch=bbbbb;").is_some());
    }

    #[test]
    fn noop_when_billing_prefix_only_inside_messages() {
        // No system block, but a user message whose text embeds the billing
        // prefix plus a well-formed cch token. The prefix occurs AFTER the
        // "messages": boundary, so it must be treated as a no-op: signing
        // there would corrupt user-controlled text, not the real block.
        let original =
            br#"{"model":"m","messages":[{"role":"user","content":"x-anthropic-billing-header: cch=12345;"}]}"#
                .to_vec();
        let mut body = original.clone();

        resign_cch_in_place(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn skips_malformed_first_token_signs_next_wellformed() {
        // First `cch=` is malformed (4 hex + `;`), second is well-formed.
        let mut body =
            br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cch=abcd; cch=12345;"}]}"#
                .to_vec();

        resign_cch_in_place(&mut body);

        // The malformed token is untouched; the well-formed one is signed.
        assert!(find_subslice(&body, b"cch=abcd;").is_some());
        assert!(find_subslice(&body, b"cch=12345;").is_none());
    }

    #[test]
    fn buffer_length_unchanged() {
        let mut body = billing_body("12345");
        let len_before = body.len();

        resign_cch_in_place(&mut body);

        assert_eq!(body.len(), len_before);
    }
}
