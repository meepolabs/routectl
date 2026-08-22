//! Compiled Antigravity IDE identity defaults -- the `antigravity` third
//! of the provider identity module.
//!
//! The Cloud Code egress (`cloudcode-pa.googleapis.com`) associates a
//! request with an Antigravity IDE install through a `User-Agent` of the
//! shape `antigravity/{ide_version} {os}/{arch}`, and repeats the version
//! and product name inside the `onboardUser` metadata body. This module is
//! the single source of those three values so no wire literal is spelled
//! twice.
//!
//! The platform pair is the REAL host platform, mapped into the reference
//! client's wire vocabulary -- the client reports where it actually runs,
//! so pinning one platform would make every non-matching host's
//! fingerprint a lie.

use std::env::consts;
use std::sync::OnceLock;

/// Antigravity `ideVersion` (the `ideVersion` key of the installed client's
/// `product.json`, NOT its app `version`, which tracks the VS Code base and
/// moves independently) that routectl reports on the Cloud Code lane.
///
/// CAPABILITY-BEARING, NOT COSMETIC: the upstream gates the model catalog on
/// this version. A stale pin silently loses the newest model tier -- the
/// catalog stops listing those ids and inference against one answers `404
/// NOT_FOUND`, which reads as a bad model id rather than as a rejected
/// client. Measured with one bearer, one project, one body and only the
/// version differing: the pin preceding this one listed 25 catalog ids and
/// 404'd `gemini-3.7-flash-high`, while this one lists 28 and answers it 200.
///
/// STALENESS RISK: this is a compiled pin with no live version fetcher
/// behind it (deliberately -- the lane adds no egress to discover it). Roll
/// it forward by hand when the upstream client updates; the ordered surfaces
/// a bump must touch are in `docs/PROVIDER-QUIRKS.md` under the cloud-code
/// client-identity paragraph.
pub const PINNED_IDE_VERSION: &str = "2.5.5";

/// Product name reported in the `User-Agent` and in the `onboardUser`
/// metadata `ide_name` field.
pub const IDE_NAME: &str = "antigravity";

/// Compose the Cloud Code `User-Agent` from a Rust `OS` / `ARCH` pair.
///
/// Takes both as arguments rather than reading [`consts`] so the mapping
/// is testable for platforms the test host is not running on. Unmapped
/// values pass through unchanged: an unrecognized platform reports itself
/// honestly rather than masquerading as a mapped one.
fn compose(os: &str, arch: &str) -> String {
    let wire_os = match os {
        "macos" => "darwin",
        "windows" => "windows",
        other => other,
    };
    let wire_arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    format!("{IDE_NAME}/{PINNED_IDE_VERSION} {wire_os}/{wire_arch}")
}

/// Default `User-Agent` for the Cloud Code ("antigravity") egress, seeded
/// into `GeminiConfig.user_agent` by the cloud-code constructor. Computed
/// once per process; subsequent calls return the cached value.
pub fn antigravity_user_agent() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| compose(consts::OS, consts::ARCH))
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_maps_the_reference_client_platform_vocabulary() {
        assert_eq!(
            compose("macos", "aarch64"),
            "antigravity/2.5.5 darwin/arm64"
        );
        assert_eq!(compose("macos", "x86_64"), "antigravity/2.5.5 darwin/amd64");
        assert_eq!(
            compose("windows", "x86_64"),
            "antigravity/2.5.5 windows/amd64"
        );
        assert_eq!(compose("windows", "x86"), "antigravity/2.5.5 windows/386");
        assert_eq!(compose("linux", "aarch64"), "antigravity/2.5.5 linux/arm64");
    }

    #[test]
    fn compose_passes_unmapped_os_and_arch_through_unchanged() {
        assert_eq!(
            compose("freebsd", "riscv64"),
            "antigravity/2.5.5 freebsd/riscv64"
        );
        // `linux` is already the wire spelling, so it must not be rewritten.
        assert_eq!(compose("linux", "x86_64"), "antigravity/2.5.5 linux/amd64");
    }

    #[test]
    fn user_agent_pins_the_ide_version_and_carries_one_platform_pair() {
        let ua = antigravity_user_agent();
        assert!(
            ua.starts_with("antigravity/2.5.5 "),
            "UA must lead with the product and pinned version; got {ua}"
        );
        let parts: Vec<&str> = ua.split(' ').collect();
        assert_eq!(
            parts.len(),
            2,
            "UA must be exactly `product/version os/arch`; got {ua}"
        );
        assert_eq!(
            parts[1].matches('/').count(),
            1,
            "UA must carry exactly one platform pair; got {ua}"
        );
    }

    #[test]
    fn user_agent_reports_the_real_host_platform() {
        let ua = antigravity_user_agent();
        assert_eq!(
            ua,
            compose(consts::OS, consts::ARCH),
            "UA must be composed from the running host, not a pinned platform"
        );
    }

    #[test]
    fn user_agent_is_memoized() {
        assert!(std::ptr::eq(
            antigravity_user_agent(),
            antigravity_user_agent()
        ));
    }

    #[test]
    fn user_agent_drops_the_abandoned_cli_shape() {
        // NEGATIVE CONTROL for the abandoned `{IDE_NAME}/cli/<version>
        // (aidev_client; ...)` shape: the positive assertions above prove a
        // real UA is produced, so this absence is meaningful. The needle is
        // composed rather than spelled so the retired literal exists nowhere
        // in the tree.
        let ua = antigravity_user_agent();
        let abandoned_prefix = format!("{IDE_NAME}/cli");
        assert!(
            !ua.contains(&abandoned_prefix),
            "UA must not use the abandoned cli/ shape; got {ua}"
        );
        assert!(
            !ua.contains("aidev_client"),
            "UA must not carry the abandoned aidev_client marker; got {ua}"
        );
        // The product segment carries exactly one `/`: product/version, never
        // a third path component.
        let product = ua.split(' ').next().expect("product segment");
        assert_eq!(
            product.matches('/').count(),
            1,
            "product segment must be `product/version`; got {product}"
        );
    }
}
