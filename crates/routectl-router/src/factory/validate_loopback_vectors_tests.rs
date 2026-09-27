use super::validate_base_url_scheme;
use routectl_testkit::loopback_vectors::{LOOPBACK_BASE_URLS, NON_LOOPBACK_BASE_URLS};

#[test]
fn shared_loopback_vectors_pass_the_cleartext_check() {
    for url in LOOPBACK_BASE_URLS {
        let result = validate_base_url_scheme("p", url);

        assert!(
            result.is_ok(),
            "{url} is loopback and must pass; got: {result:?}"
        );
    }
}

#[test]
fn shared_non_loopback_vectors_are_refused_as_cleartext() {
    for url in NON_LOOPBACK_BASE_URLS {
        let result = validate_base_url_scheme("p", url);

        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.to_string().contains("cleartext")),
            "{url} is not loopback and must be refused as cleartext; got: {result:?}"
        );
    }
}
