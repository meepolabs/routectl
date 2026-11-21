//! The populated panel fixtures are dense: no empty array and no key path
//! null at every occurrence, beyond a short list of named exceptions; every
//! conditional field the fixtures claim to cover also appears null; and each
//! envelope carries its panel's wire version.

use serde_json::{Value, json};

use super::{Allowance, DensityException, aggregate_json, density_gaps, missing_null_states};
use crate::handlers::status::{accounts, attention, config, doctor, health, models, query, usage};

/// One populated payload under test, with the exceptions its shape forces.
struct Fixture {
    name: &'static str,
    payload: fn() -> Value,
    schema_version: u32,
    exceptions: &'static [DensityException],
}

const UNAVAILABLE: DensityException = DensityException {
    path: "unavailable",
    allowance: Allowance::AlwaysNull,
    reason: "an available panel never carries an unavailable code",
};

const AGREED_CELLS: DensityException = DensityException {
    path: "data.report.panels.capability_matrix.lanes[].cells[].nickname_actions",
    allowance: Allowance::MayBeEmpty,
    reason: "a cell every nickname agrees on carries no per-nickname list",
};

const FULLY_PRICED_GROUPS: DensityException = DensityException {
    path: "data.groups[].metrics.missing_price_dimensions",
    allowance: Allowance::MayBeEmpty,
    reason: "a group whose every used dimension has a rate names none",
};

const FULLY_PRICED_BUCKETS: DensityException = DensityException {
    path: "data.series.buckets[].metrics.missing_price_dimensions",
    allowance: Allowance::MayBeEmpty,
    reason: "a priced or zero-traffic bucket names no missing dimension",
};

const FULLY_PRICED_GROUP_PAIRS: DensityException = DensityException {
    path: "data.groups[].metrics.missing_prices",
    allowance: Allowance::MayBeEmpty,
    reason: "a group whose every used dimension has a rate names no model and dimension",
};

const FULLY_PRICED_BUCKET_PAIRS: DensityException = DensityException {
    path: "data.series.buckets[].metrics.missing_prices",
    allowance: Allowance::MayBeEmpty,
    reason: "a priced or zero-traffic bucket names no model and dimension",
};

const NO_WINDOWS: DensityException = DensityException {
    path: "data.accounts[].windows",
    allowance: Allowance::MayBeEmpty,
    reason: "an account with no reading, or one that reported no window, lists none",
};

const NO_POOLS: DensityException = DensityException {
    path: "data.accounts[].pools",
    allowance: Allowance::MayBeEmpty,
    reason: "an account no pool member dispatches on names no pool",
};

const NO_REFUSALS: DensityException = DensityException {
    path: "data.rows[].refusals",
    allowance: Allowance::MayBeEmpty,
    reason: "a lane with no learned negative shown refuses nothing",
};

const UNMEASURED: DensityException = DensityException {
    path: "data.rows[].transit.top_paths",
    allowance: Allowance::MayBeEmpty,
    reason: "a lane the wire check never sampled, or sampled clean, changed no path",
};

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "usage",
        payload: usage::fixture::populated_usage,
        schema_version: usage::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE],
    },
    Fixture {
        name: "health",
        payload: health::fixture::populated_health,
        schema_version: health::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE],
    },
    Fixture {
        name: "config",
        payload: config::fixture::populated_config,
        schema_version: config::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE],
    },
    Fixture {
        name: "doctor",
        payload: doctor::fixture::populated_doctor,
        schema_version: doctor::DOCTOR_SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE, AGREED_CELLS],
    },
    Fixture {
        name: "accounts",
        payload: accounts::fixture::populated_accounts,
        schema_version: accounts::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE, NO_WINDOWS, NO_POOLS],
    },
    Fixture {
        name: "attention",
        payload: attention::fixture::populated_attention,
        schema_version: attention::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE],
    },
    Fixture {
        name: "attention doctor unavailable",
        payload: attention::fixture::populated_attention_doctor_unavailable,
        schema_version: attention::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE],
    },
    Fixture {
        name: "models",
        payload: models::fixture::populated_models,
        schema_version: models::SCHEMA_VERSION,
        exceptions: &[UNAVAILABLE, NO_REFUSALS, UNMEASURED],
    },
    Fixture {
        name: "query grouped",
        payload: query::fixture::populated_query,
        schema_version: query::SCHEMA_VERSION,
        exceptions: &[
            UNAVAILABLE,
            FULLY_PRICED_GROUPS,
            FULLY_PRICED_GROUP_PAIRS,
            DensityException {
                path: "data.series",
                allowance: Allowance::AlwaysNull,
                reason: "a request without a bucket carries no series by contract",
            },
        ],
    },
    Fixture {
        name: "query series",
        payload: query::fixture::populated_query_series,
        schema_version: query::SCHEMA_VERSION,
        exceptions: &[
            UNAVAILABLE,
            FULLY_PRICED_GROUPS,
            FULLY_PRICED_BUCKETS,
            FULLY_PRICED_GROUP_PAIRS,
            FULLY_PRICED_BUCKET_PAIRS,
        ],
    },
];

#[test]
fn each_fixture_envelope_carries_its_panel_schema_version() {
    for fixture in FIXTURES {
        let payload = (fixture.payload)();

        assert_eq!(
            payload["schema_version"],
            json!(fixture.schema_version),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn each_fixture_has_no_empty_array_and_no_always_null_key() {
    for fixture in FIXTURES {
        let payload = (fixture.payload)();

        let gaps = density_gaps(&payload, fixture.exceptions);

        assert!(
            gaps.is_empty(),
            "{} fixture is sparse: {gaps:#?}",
            fixture.name
        );
    }
}

#[test]
fn the_aggregate_carries_the_seven_get_panels_densely_at_their_versions() {
    let aggregate = aggregate_json();
    let exceptions = [
        DensityException {
            path: "panels.usage.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.health.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.config.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.doctor.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.accounts.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.attention.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.models.unavailable",
            ..UNAVAILABLE
        },
        DensityException {
            path: "panels.models.data.rows[].refusals",
            ..NO_REFUSALS
        },
        DensityException {
            path: "panels.models.data.rows[].transit.top_paths",
            ..UNMEASURED
        },
        DensityException {
            path: "panels.accounts.data.accounts[].windows",
            ..NO_WINDOWS
        },
        DensityException {
            path: "panels.accounts.data.accounts[].pools",
            ..NO_POOLS
        },
        DensityException {
            path: "panels.doctor.data.report.panels.capability_matrix.lanes[].cells[].nickname_actions",
            ..AGREED_CELLS
        },
    ];

    let gaps = density_gaps(&aggregate, &exceptions);

    assert!(gaps.is_empty(), "aggregate is sparse: {gaps:#?}");
    for (panel, version) in [
        ("usage", usage::SCHEMA_VERSION),
        ("health", health::SCHEMA_VERSION),
        ("config", config::SCHEMA_VERSION),
        ("doctor", doctor::DOCTOR_SCHEMA_VERSION),
        ("accounts", accounts::SCHEMA_VERSION),
        ("attention", attention::SCHEMA_VERSION),
        ("models", models::SCHEMA_VERSION),
    ] {
        assert_eq!(
            aggregate["panels"][panel]["schema_version"],
            json!(version),
            "{panel}"
        );
    }
}

/// Conditional fields each fixture covers in BOTH states: the density test
/// proves a non-null occurrence, this proves a null one.
#[test]
fn each_fixture_shows_its_conditional_fields_in_both_states() {
    let cases: [(&str, Value, &[&str]); 8] = [
        (
            "models",
            models::fixture::populated_models(),
            &[
                "data.rows[].since_ms",
                "data.rows[].cause_key",
                "data.rows[].login_label",
                "data.rows[].refusals[].value",
                "data.rows[].capabilities[].supported",
                "data.rows[].capabilities[].source",
                "data.rows[].capabilities[].layer",
            ],
        ),
        (
            "usage",
            usage::fixture::populated_usage(),
            &[
                "data.groups[].provider",
                "data.groups[].model",
                "data.groups[].upstream",
            ],
        ),
        (
            "health",
            health::fixture::populated_health(),
            &[
                "data.targets[].eligibility.reason",
                "data.targets[].eligibility.cause",
                "data.targets[].eligibility.since_ms",
                "data.targets[].eligibility.fix",
                "data.targets[].account",
                "data.targets[].learned_lane",
                "data.targets[].seat_label",
                "data.targets[].login_label",
                "data.targets[].rpm_available",
                "data.targets[].open_since_ms",
                "data.targets[].last_outcome",
                "data.targets[].park_reason",
                "data.targets[].parked_until_ms",
                "data.learned_negatives[].expires_at_ms",
                "data.learned_negatives[].value",
            ],
        ),
        (
            "config",
            config::fixture::populated_config(),
            &[
                "data.models[].context_window",
                "data.models[].fill",
                "data.models[].prices[].per_mtok",
                "data.models[].prices[].table_date",
                "data.providers[].endpoint_origin",
                "data.providers[].cloud",
                "data.providers[].rpm_limit",
                "data.activation[].reason",
            ],
        ),
        (
            "doctor",
            doctor::fixture::populated_doctor(),
            &[
                "data.report.findings[].remediation",
                "data.report.panels.capability_matrix.lanes[].cells[].supported",
                "data.report.panels.capability_matrix.lanes[].cells[].value",
                "data.report.panels.capability_matrix.lanes[].cells[].nickname_actions[].layer",
            ],
        ),
        (
            "accounts",
            accounts::fixture::populated_accounts(),
            &[
                "data.accounts[].login_label",
                "data.accounts[].last_accepted",
                "data.accounts[].windows[].scope",
                "data.accounts[].windows[].remaining",
                "data.accounts[].windows[].reset_at_ms",
                "data.accounts[].windows[].age_ms",
                "data.accounts[].poll.vendor",
                "data.accounts[].poll.last_result",
                "data.placement.pools[].seats[].rank",
                "data.placement.pools[].seats[].score",
                "data.accounts[].credential.cause",
                "data.accounts[].credential.since_ms",
                "data.accounts[].credential.until_ms",
                "data.accounts[].credential.fix",
                "data.accounts[].credential.remediation",
            ],
        ),
        (
            "attention",
            attention::fixture::populated_attention(),
            &[
                "data.items[].account",
                "data.items[].until_ms",
                "data.items[].recovered_at_ms",
                "data.items[].fix",
                "data.items[].remediation",
                "data.items[].targets",
            ],
        ),
        (
            "query series",
            query::fixture::populated_query_series(),
            &[
                "data.series.buckets[].metrics.cost_usd",
                "data.series.buckets[].metrics.ttft_avg_ms",
            ],
        ),
    ];
    for (name, payload, both_states) in cases {
        let missing = missing_null_states(&payload, both_states);

        assert!(missing.is_empty(), "{name} never shows null at {missing:?}");
    }
}

#[test]
fn the_grouped_and_bucketed_query_fixtures_differ_only_in_shape() {
    let grouped = query::fixture::populated_query();
    let series = query::fixture::populated_query_series();

    assert!(grouped["data"]["series"].is_null());
    assert!(
        series["data"]["series"]["buckets"]
            .as_array()
            .is_some_and(|b| b.len() > 1)
    );
    assert_eq!(
        grouped["data"]["totals"]["requests"],
        series["data"]["totals"]["requests"]
    );
    let statuses: Vec<&Value> = grouped["data"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| &g["metrics"]["cost_status"])
        .collect();
    for status in ["priced", "subscription", "unpriced"] {
        assert!(
            statuses.contains(&&json!(status)),
            "a {status} group is present: {statuses:?}"
        );
    }
}

/// The walker itself: an unexcused empty array, an unexcused always-null
/// key, an exception with no reason
/// and a stale exception of each allowance yield one gap each, an excused
/// empty array yields none, and a dense payload yields none.
#[test]
fn the_density_walk_reports_each_kind_of_gap_and_passes_a_dense_payload() {
    let sparse = json!({
        "rows": [{ "a": 1, "b": null, "c": null }, { "a": 2, "b": null, "c": null }],
        "empty": [],
        "kept": [],
    });
    let exceptions = [
        DensityException {
            path: "kept",
            allowance: Allowance::MayBeEmpty,
            reason: "stale: never populated",
        },
        DensityException {
            path: "rows[].a",
            allowance: Allowance::AlwaysNull,
            reason: "stale: never null",
        },
        DensityException {
            path: "rows[].b",
            allowance: Allowance::AlwaysNull,
            reason: " ",
        },
    ];

    let gaps = density_gaps(&sparse, &exceptions);

    assert_eq!(
        gaps,
        vec![
            "empty array at empty".to_string(),
            "never non-null at rows[].c".to_string(),
            "exception MayBeEmpty at kept no longer matches".to_string(),
            "exception AlwaysNull at rows[].a no longer matches".to_string(),
            "exception at rows[].b gives no reason".to_string(),
        ]
    );
    let dense = json!({ "rows": [{ "a": 1, "b": null }, { "a": 2, "b": 3 }] });
    assert!(density_gaps(&dense, &[]).is_empty());
    assert_eq!(
        missing_null_states(&dense, &["rows[].b", "rows[].a"]),
        vec!["rows[].a"]
    );
}
