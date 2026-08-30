//! The committed cross-mode pair: one interaction captured twice, once
//! reached by base URL and once through the MITM front proxy.
//!
//! The axis under test is the CONNECTION MODE, so everything else -- lane,
//! ingress dialect, wire pattern -- is held constant and asserted equal.
//! What must differ is TRANSIT: the front-proxy half's captured ingress
//! headers carry the seam header the MITM front stamps plus the client's
//! own `authorization`, and the base-url half's carry neither. Without the
//! absence halves a later recapture that lost the axis would leave two
//! identical fixtures passing a green test.
//!
//! IN-BAND SYSTEM TURNS ARE NOT THE CONTRAST, and the zero is asserted on
//! BOTH halves deliberately. `role:"system"` turns in `messages[]` are an
//! ingress-DIALECT property: the system-turn lift runs only for the OpenAI
//! and OpenAI-Responses ingress parsers, never for the Anthropic one,
//! because an Anthropic client sends `system` top-level. So no
//! Anthropic-lane capture exhibits in-band system turns in either mode,
//! and a future one that did would be a dialect change that owes an
//! explanation -- which is what pinning the zero buys.
//!
//! Every clause is adjudicated by [`cross_mode_violations`] over two
//! loaded fixtures, so the same predicate that runs on the committed pair
//! also runs on planted pairs with exactly one clause flipped. A test that
//! only read the two real fixtures could not tell a real check from one
//! that passes on anything.

mod common;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use common::replay::{
    Fixture, current_meta, driver_root, headers_from_pairs, in_band_system_turns, load_fixture,
    plant_driver_case_with_ingress,
};

/// Lane both halves of the pair were captured on.
const PAIR_LANE: &str = "anthropic-api";

/// The base-url half of the committed pair.
const BASE_URL_CASE: &str = "plain-turn-01";

/// The front-proxy half of the committed pair.
const FRONT_PROXY_CASE: &str = "plain-turn-01-fp";

/// `meta.client.connection_mode` each half must claim.
const BASE_URL_MODE: &str = "base-url";
const FRONT_PROXY_MODE: &str = "front-proxy";

/// Seam header the MITM front proxy stamps on a request it re-injected.
/// Its NAME is the transit proof; its value is per-process and never
/// compared.
const MITM_SEAM_HEADER: &str = "x-routectl-mitm-proxied";

/// Credential header the front-proxy path forwards from the client and the
/// base-url path does not carry.
const CLIENT_CREDENTIAL_HEADER: &str = "authorization";

/// Clause tags. Each violation message starts with one, so a control can
/// name the clause it flipped instead of matching on prose.
const CLAUSE_MODE: &str = "connection-mode:";
const CLAUSE_SEAM: &str = "seam-header:";
const CLAUSE_CREDENTIAL: &str = "credential-header:";
const CLAUSE_SYSTEM_TURNS: &str = "in-band-system-turns:";
const CLAUSE_HELD_CONSTANT: &str = "held-constant:";

/// Whether a fixture's captured ingress headers carry `name`.
///
/// Read through the loader's own header bridge rather than off the pairs
/// directly, so the lookup is case-insensitive the way the wire is and no
/// second parse of `*.headers.json` exists to drift.
fn ingress_carries(fixture: &Fixture, name: &str) -> bool {
    headers_from_pairs(&fixture.ingress_request_headers).contains_key(name)
}

/// Every way a loaded pair fails to be the cross-mode pair. Empty means
/// the pair holds every clause.
fn cross_mode_violations(base: &Fixture, front_proxy: &Fixture) -> Vec<String> {
    let mut out = Vec::new();

    for (fixture, expected) in [(base, BASE_URL_MODE), (front_proxy, FRONT_PROXY_MODE)] {
        let claimed = &fixture.meta.client.connection_mode;
        if claimed != expected {
            out.push(format!(
                "{CLAUSE_MODE} `{}` claims connection_mode `{claimed}`, expected `{expected}`",
                fixture.name,
            ));
        }
    }

    for (header, clause) in [
        (MITM_SEAM_HEADER, CLAUSE_SEAM),
        (CLIENT_CREDENTIAL_HEADER, CLAUSE_CREDENTIAL),
    ] {
        if !ingress_carries(front_proxy, header) {
            out.push(format!(
                "{clause} `{}` carries no `{header}`, so nothing evidences the \
                 front-proxy transit",
                front_proxy.name,
            ));
        }
        if ingress_carries(base, header) {
            out.push(format!(
                "{clause} `{}` carries `{header}`, so both halves transited the same \
                 way and the mode axis is gone",
                base.name,
            ));
        }
    }

    for fixture in [base, front_proxy] {
        let turns = in_band_system_turns(&fixture.ingress_request);
        if turns != 0 {
            out.push(format!(
                "{CLAUSE_SYSTEM_TURNS} `{}` carries {turns} in-band `role:\"system\"` \
                 turn(s); the Anthropic ingress takes `system` top-level and never \
                 lifts in-band turns, so this is a dialect change owing an explanation",
                fixture.name,
            ));
        }
    }

    for (field, base_value, fp_value) in [
        ("lane", &base.meta.lane, &front_proxy.meta.lane),
        (
            "ingress_kind",
            &base.meta.ingress_kind,
            &front_proxy.meta.ingress_kind,
        ),
        (
            "wire_pattern",
            &base.meta.wire_pattern,
            &front_proxy.meta.wire_pattern,
        ),
    ] {
        if base_value != fp_value {
            out.push(format!(
                "{CLAUSE_HELD_CONSTANT} {field} differs across the pair (`{base_value}` vs \
                 `{fp_value}`); the mode is the only axis that may vary",
            ));
        }
    }

    out
}

/// Directory of one committed half.
fn committed_case(case_id: &str) -> PathBuf {
    driver_root().join(PAIR_LANE).join(case_id)
}

/// Load a committed half, or report it missing BY NAME. A checkout without
/// the corpus is missing evidence, not a regression.
fn load_committed(dir: &Path) -> Option<Fixture> {
    if !dir.exists() {
        eprintln!(
            "cross-mode pair: NOT RUN (committed fixture `{}` is absent from this checkout)",
            dir.display(),
        );
        return None;
    }
    Some(
        load_fixture(dir)
            .unwrap_or_else(|e| panic!("committed fixture `{}` must load: {e}", dir.display())),
    )
}

#[test]
fn the_committed_pair_differs_only_in_connection_mode() {
    let base_dir = committed_case(BASE_URL_CASE);
    let fp_dir = committed_case(FRONT_PROXY_CASE);
    let (Some(base), Some(front_proxy)) = (load_committed(&base_dir), load_committed(&fp_dir))
    else {
        return;
    };

    let violations = cross_mode_violations(&base, &front_proxy);

    assert!(
        violations.is_empty(),
        "the committed cross-mode pair no longer holds the mode axis:\n{}",
        violations.join("\n"),
    );
}

// ---- paired controls: each clause, flipped, over a planted pair ----
//
// The planter is the one writer of a synthetic corpus, so these pairs are
// shaped exactly as a rig-written fixture is. Each helper below plants the
// CORRECT pair and each control mutates one clause, so the correct pair is
// itself asserted violation-free first -- a planter that drifted out of
// agreement with the predicate would otherwise make every control pass for
// the wrong reason.

/// `meta.json` for one planted half.
fn planted_meta(case_id: &str, mode: &str) -> Value {
    let mut meta = current_meta();
    meta["lane"] = json!(PAIR_LANE);
    meta["case_id"] = json!(case_id);
    meta["client"]["connection_mode"] = json!(mode);
    meta
}

/// A body with `system` top-level and `n` in-band `role:"system"` turns --
/// zero being the Anthropic-lane shape.
fn planted_body(in_band_system_turns: usize) -> Value {
    let mut messages: Vec<Value> = (0..in_band_system_turns)
        .map(|_| json!({"role": "system", "content": "planted"}))
        .collect();
    messages.push(json!({"role": "user", "content": "planted"}));
    json!({
        "model": "claude-sonnet-4-5",
        "system": [{"type": "text", "text": "planted"}],
        "messages": messages,
    })
}

/// Ingress headers of a base-url capture: no seam header, no forwarded
/// client credential.
fn base_url_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("content-type", "application/json"),
        ("x-api-key", "[REDACTED]"),
    ]
}

/// Ingress headers of a front-proxy capture: the seam header plus the
/// client's own credential, on top of the base-url set.
fn front_proxy_headers() -> Vec<(&'static str, &'static str)> {
    let mut headers = base_url_headers();
    headers.push((MITM_SEAM_HEADER, "1"));
    headers.push((CLIENT_CREDENTIAL_HEADER, "[REDACTED]"));
    headers
}

/// One planted half's inputs, so a control can alter exactly one of them.
struct PlantedHalf {
    case_id: &'static str,
    meta: Value,
    headers: Vec<(&'static str, &'static str)>,
    body: Value,
}

impl PlantedHalf {
    fn base_url() -> Self {
        Self {
            case_id: BASE_URL_CASE,
            meta: planted_meta(BASE_URL_CASE, BASE_URL_MODE),
            headers: base_url_headers(),
            body: planted_body(0),
        }
    }

    fn front_proxy() -> Self {
        Self {
            case_id: FRONT_PROXY_CASE,
            meta: planted_meta(FRONT_PROXY_CASE, FRONT_PROXY_MODE),
            headers: front_proxy_headers(),
            body: planted_body(0),
        }
    }
}

/// Plant both halves in a fresh tempdir, load them, and adjudicate.
fn violations_over(base: &PlantedHalf, front_proxy: &PlantedHalf) -> Vec<String> {
    let tmp = tempfile::tempdir().expect("a tempdir for the planted pair");
    let load = |half: &PlantedHalf| {
        let dir = plant_driver_case_with_ingress(
            tmp.path(),
            PAIR_LANE,
            half.case_id,
            &half.meta,
            &half.headers,
            &half.body,
        );
        load_fixture(&dir).expect("a planted half must load")
    };
    let base = load(base);
    let front_proxy = load(front_proxy);
    cross_mode_violations(&base, &front_proxy)
}

/// Which clause a control expected to break, asserted by tag so the
/// control cannot be satisfied by an unrelated violation.
fn assert_flags_only(violations: &[String], clause: &str) {
    assert!(
        !violations.is_empty(),
        "the flipped clause `{clause}` produced no violation, so the predicate passes on \
         anything",
    );
    for violation in violations {
        assert!(
            violation.starts_with(clause),
            "expected only `{clause}` violations, got: {violation}",
        );
    }
}

/// POSITIVE CONTROL for every control below: a correctly planted pair is
/// violation-free. Without it a predicate that flagged everything would
/// satisfy all the negative controls.
#[test]
fn a_correctly_planted_pair_holds_every_clause() {
    let violations = violations_over(&PlantedHalf::base_url(), &PlantedHalf::front_proxy());

    assert!(
        violations.is_empty(),
        "the planted pair must model the committed one:\n{}",
        violations.join("\n"),
    );
}

#[test]
fn a_half_claiming_the_wrong_connection_mode_is_flagged() {
    let mut base = PlantedHalf::base_url();
    base.meta["client"]["connection_mode"] = json!(FRONT_PROXY_MODE);

    let violations = violations_over(&base, &PlantedHalf::front_proxy());

    assert_flags_only(&violations, CLAUSE_MODE);
}

#[test]
fn a_front_proxy_half_missing_the_seam_header_is_flagged() {
    let mut front_proxy = PlantedHalf::front_proxy();
    front_proxy.headers.retain(|(n, _)| *n != MITM_SEAM_HEADER);

    let violations = violations_over(&PlantedHalf::base_url(), &front_proxy);

    assert_flags_only(&violations, CLAUSE_SEAM);
}

/// The ABSENCE half of the seam clause: a base-url capture carrying the
/// seam header means both halves transited the front proxy, which is the
/// state a lost mode axis actually looks like.
#[test]
fn a_base_url_half_carrying_the_seam_header_is_flagged() {
    let mut base = PlantedHalf::base_url();
    base.headers.push((MITM_SEAM_HEADER, "1"));

    let violations = violations_over(&base, &PlantedHalf::front_proxy());

    assert_flags_only(&violations, CLAUSE_SEAM);
}

#[test]
fn a_front_proxy_half_missing_the_client_credential_is_flagged() {
    let mut front_proxy = PlantedHalf::front_proxy();
    front_proxy
        .headers
        .retain(|(n, _)| *n != CLIENT_CREDENTIAL_HEADER);

    let violations = violations_over(&PlantedHalf::base_url(), &front_proxy);

    assert_flags_only(&violations, CLAUSE_CREDENTIAL);
}

#[test]
fn a_base_url_half_carrying_the_client_credential_is_flagged() {
    let mut base = PlantedHalf::base_url();
    base.headers.push((CLIENT_CREDENTIAL_HEADER, "[REDACTED]"));

    let violations = violations_over(&base, &PlantedHalf::front_proxy());

    assert_flags_only(&violations, CLAUSE_CREDENTIAL);
}

/// The deliberately-pinned zero, flipped on EACH half in turn: the clause
/// is about the Anthropic dialect and holds regardless of mode, so a
/// control on one half alone would leave the other unpinned.
#[test]
fn either_half_growing_in_band_system_turns_is_flagged() {
    let mut base = PlantedHalf::base_url();
    base.body = planted_body(1);
    let base_violations = violations_over(&base, &PlantedHalf::front_proxy());
    assert_flags_only(&base_violations, CLAUSE_SYSTEM_TURNS);

    let mut front_proxy = PlantedHalf::front_proxy();
    front_proxy.body = planted_body(2);
    let fp_violations = violations_over(&PlantedHalf::base_url(), &front_proxy);
    assert_flags_only(&fp_violations, CLAUSE_SYSTEM_TURNS);
}

#[test]
fn a_pair_disagreeing_on_a_held_constant_field_is_flagged() {
    for field in ["lane", "ingress_kind", "wire_pattern"] {
        let mut front_proxy = PlantedHalf::front_proxy();
        front_proxy.meta[field] = json!("a-value-the-other-half-does-not-carry");

        let violations = violations_over(&PlantedHalf::base_url(), &front_proxy);

        assert_flags_only(&violations, CLAUSE_HELD_CONSTANT);
    }
}
