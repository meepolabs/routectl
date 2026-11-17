use super::ProviderEntry;
use crate::config::provider_descriptor::VENDOR_ROWS;

const MANAGED_VENDORS: &[&str] = &["anthropic", "codex", "xai", "antigravity"];

#[test]
fn the_named_rows_cover_every_managed_vendor() {
    let described: Vec<&str> = VENDOR_ROWS.iter().map(|row| row.vendor).collect();

    assert_eq!(MANAGED_VENDORS, described.as_slice());
}

#[test]
fn a_managed_login_entry_takes_its_vendor_rows_format_auth_and_base() {
    for &vendor in MANAGED_VENDORS {
        // Arrange
        let row = VENDOR_ROWS
            .iter()
            .find(|row| row.vendor == vendor)
            .unwrap_or_else(|| panic!("`{vendor}` has a descriptor row"));
        let credential = format!("oauth://{vendor}");

        // Act
        let entry = ProviderEntry::for_managed_login(vendor, credential.clone())
            .unwrap_or_else(|| panic!("`{vendor}` builds an entry"));

        // Assert
        assert_eq!(entry.format, row.format, "vendor `{vendor}`");
        assert_eq!(entry.auth, row.auth, "vendor `{vendor}`");
        assert_eq!(
            entry.base_url.as_deref(),
            Some(row.default_base),
            "vendor `{vendor}`"
        );
        assert!(entry.cloud.is_none(), "vendor `{vendor}`");
        assert_eq!(entry.credential, credential, "vendor `{vendor}`");
    }
}

#[test]
fn an_unmanaged_vendor_builds_no_entry() {
    assert!(ProviderEntry::for_managed_login("anthropic", "oauth://anthropic").is_some());

    assert!(ProviderEntry::for_managed_login("unknown", "oauth://unknown").is_none());
}
