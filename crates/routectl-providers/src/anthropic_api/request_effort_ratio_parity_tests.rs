use super::effort_ratio;
use crate::effort::VALID_EFFORT_TOKENS;

/// Assert that every token listed in VALID_EFFORT_TOKENS returns a
/// ratio distinct from the default fallback arm (0.50). The only
/// token that should legitimately equal 0.50 is "medium". All
/// others must have a dedicated arm.
///
/// If a new token is added to VALID_EFFORT_TOKENS without a
/// matching arm in effort_ratio, it will silently receive 0.50
/// (the default). This test surfaces that gap.
#[test]
fn every_valid_effort_token_has_non_default_ratio_or_is_medium() {
    // Tokens that are EXPECTED to map to 0.50 (the default ratio).
    // Only "medium" is intentional.
    const EXPECTED_DEFAULT: &[&str] = &["medium"];

    for &token in &VALID_EFFORT_TOKENS {
        let ratio = effort_ratio(token);
        if EXPECTED_DEFAULT.contains(&token) {
            // "medium" is intentionally 0.50.
            assert_eq!(
                ratio, 0.50,
                "token \"{token}\" expected 0.50 but got {ratio}"
            );
        } else {
            // All other tokens must have a dedicated arm (not the 0.50 default).
            assert_ne!(
                ratio, 0.50,
                "token \"{token}\" maps to the default ratio 0.50; \
                 add a dedicated arm to effort_ratio for this token"
            );
        }
    }
}
