//! The price table embedded in this build: the lowest price layer, under
//! any refreshed copy.

use std::sync::OnceLock;

use super::{PriceTable, parse_and_validate};

/// The canonical table generated for this release.
pub(super) const SHIPPED_JSON: &str = include_str!("../prices_shipped.json");

/// The table shipped with this build, parsed on first use and kept for the
/// life of the process. `None` only if the embedded bytes do not validate,
/// which the crate's tests rule out for every build they pass on; the
/// failure is logged once at ERROR, with the parser's fixed-text reason.
pub fn shipped_table() -> Option<&'static PriceTable> {
    static TABLE: OnceLock<Option<PriceTable>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            parse_and_validate(SHIPPED_JSON.as_bytes())
                .inspect_err(|e| {
                    tracing::error!(error = %e, "shipped price table refused; no shipped prices");
                })
                .ok()
        })
        .as_ref()
}
