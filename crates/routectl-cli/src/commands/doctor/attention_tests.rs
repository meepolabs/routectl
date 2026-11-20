use routectl_core::UpstreamReason;
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{Finding, Status};
use serde_json::Value;

use super::*;

const NOW: i64 = 1_800_000_000_000;
const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;

const ACCOUNT: &str = "anthropic#seat-a";
const DISPLAY: &str = "claude-a (seat-a)";
const FIX: &str = "routectl login anthropic --label seat-a";

fn inputs() -> AttentionInputs {
    AttentionInputs {
        now_ms: NOW,
        drivers: DriverSnapshot {
            refresh_running: true,
            poll_running: true,
        },
        ..AttentionInputs::default()
    }
}

fn account(name: &str) -> AccountSnapshot {
    AccountSnapshot {
        account: name.to_string(),
        display: format!("display of {name}"),
        credential: CredentialState::Usable,
        fix: Some(format!("routectl login {name}")),
        refresh_failure: None,
        poll_failure: None,
        windows: Vec::new(),
    }
}

fn seat_a() -> AccountSnapshot {
    AccountSnapshot {
        display: DISPLAY.to_string(),
        fix: Some(FIX.to_string()),
        ..account(ACCOUNT)
    }
}

const fn logged_out(since_ms: i64) -> CredentialState {
    CredentialState::Unusable {
        cause: UnusableCause::NotLoggedIn,
        since_ms,
    }
}

fn window(length_secs: u64, used: f64) -> WindowSnapshot {
    WindowSnapshot {
        length_secs,
        scope: None,
        used,
        stale: false,
        reset_ms: Some(NOW + 2 * HOUR),
    }
}

fn route(state_key: &str, account: Option<&str>, open: bool) -> RouteSnapshot {
    RouteSnapshot {
        state_key: state_key.to_string(),
        account: account.map(str::to_string),
        circuit_open: open,
        open_since_ms: open.then_some(NOW - 2 * MIN),
    }
}

fn pool(name: &str, configured: usize, eligible: usize) -> PoolSnapshot {
    PoolSnapshot {
        pool: name.to_string(),
        configured,
        eligible,
    }
}

fn finding(section: &'static str, name: &str, status: Status) -> Finding {
    Finding {
        section,
        name: name.to_string(),
        status,
        detail: format!("{name} detail"),
        remediation: Some(format!("{name} remediation")),
    }
}

fn with_account(account: AccountSnapshot) -> AttentionInputs {
    AttentionInputs {
        accounts: vec![account],
        ..inputs()
    }
}

fn count_of(kind: AttentionKind, inputs: &AttentionInputs) -> usize {
    active_items(inputs)
        .iter()
        .filter(|item| item.kind == kind)
        .count()
}

fn kinds_of(inputs: &AttentionInputs) -> Vec<AttentionKind> {
    active_items(inputs).iter().map(|item| item.kind).collect()
}

fn assert_rows(kind: AttentionKind, rows: Vec<(&str, AttentionInputs, usize)>) {
    for (name, row, expected) in rows {
        assert_eq!(count_of(kind, &row), expected, "{}: {name}", kind.as_str());
    }
}

#[test]
fn pool_empty_is_listed_only_for_a_configured_pool_with_no_usable_seat() {
    let rows = vec![
        (
            "no usable seat",
            AttentionInputs {
                pools: vec![pool("pool-a", 2, 0)],
                ..inputs()
            },
            1,
        ),
        (
            "one usable seat",
            AttentionInputs {
                pools: vec![pool("pool-a", 2, 1)],
                ..inputs()
            },
            0,
        ),
        (
            "no seat configured",
            AttentionInputs {
                pools: vec![pool("pool-a", 0, 0)],
                ..inputs()
            },
            0,
        ),
    ];

    assert_rows(AttentionKind::PoolEmpty, rows);
}

#[test]
fn route_circuit_open_is_listed_per_open_route_whose_account_is_not_already_listed() {
    let parked = AccountSnapshot {
        credential: CredentialState::Parked {
            reason: UpstreamReason::CredentialInvalid,
            until_ms: NOW + HOUR,
        },
        ..seat_a()
    };
    let rows = vec![
        (
            "open, no account",
            AttentionInputs {
                routes: vec![route("fast", None, true)],
                ..inputs()
            },
            1,
        ),
        (
            "open, usable account",
            AttentionInputs {
                routes: vec![route("fast", Some(ACCOUNT), true)],
                ..with_account(seat_a())
            },
            1,
        ),
        (
            "closed",
            AttentionInputs {
                routes: vec![route("fast", None, false)],
                ..inputs()
            },
            0,
        ),
        (
            "open, parked account",
            AttentionInputs {
                routes: vec![route("fast", Some(ACCOUNT), true)],
                ..with_account(parked)
            },
            0,
        ),
    ];

    assert_rows(AttentionKind::RouteCircuitOpen, rows);
}

#[test]
fn account_unusable_is_listed_for_an_account_with_an_unusable_credential() {
    let rows = vec![
        (
            "not logged in",
            with_account(AccountSnapshot {
                credential: logged_out(NOW - 2 * MIN),
                ..seat_a()
            }),
            1,
        ),
        ("usable", with_account(seat_a()), 0),
    ];

    assert_rows(AttentionKind::AccountUnusable, rows);
}

#[test]
fn account_parked_is_listed_for_an_account_with_a_parked_credential() {
    let rows = vec![
        (
            "parked",
            with_account(AccountSnapshot {
                credential: CredentialState::Parked {
                    reason: UpstreamReason::CredentialExpired,
                    until_ms: NOW + HOUR,
                },
                ..seat_a()
            }),
            1,
        ),
        ("usable", with_account(seat_a()), 0),
    ];

    assert_rows(AttentionKind::AccountParked, rows);
}

#[test]
fn account_refresh_failing_is_listed_for_a_usable_account_with_a_failure_run() {
    let failing = RefreshFailure {
        first_failure_ms: NOW - 4 * MIN,
        token: "transient",
    };
    let rows = vec![
        (
            "failure run",
            with_account(AccountSnapshot {
                refresh_failure: Some(failing),
                ..seat_a()
            }),
            1,
        ),
        ("no failure run", with_account(seat_a()), 0),
        (
            "failure run on an unusable account",
            with_account(AccountSnapshot {
                credential: logged_out(NOW - MIN),
                refresh_failure: Some(failing),
                ..seat_a()
            }),
            0,
        ),
    ];

    assert_rows(AttentionKind::AccountRefreshFailing, rows);
}

#[test]
fn driver_stopped_is_listed_per_stopped_driver_only_while_managed_accounts_exist() {
    let stopped = |refresh_running, poll_running, managed_accounts| AttentionInputs {
        drivers: DriverSnapshot {
            refresh_running,
            poll_running,
        },
        managed_accounts,
        ..inputs()
    };
    let rows = vec![
        ("refresh stopped", stopped(false, true, 1), 1),
        ("poll stopped", stopped(true, false, 1), 1),
        ("both stopped", stopped(false, false, 3), 2),
        ("both running", stopped(true, true, 1), 0),
        (
            "both stopped, no managed account",
            stopped(false, false, 0),
            0,
        ),
    ];

    assert_rows(AttentionKind::DriverStopped, rows);
}

#[test]
fn account_near_limit_is_listed_for_a_fresh_window_used_past_ninety_percent() {
    let with_window = |window: WindowSnapshot| {
        with_account(AccountSnapshot {
            windows: vec![window],
            ..seat_a()
        })
    };
    let rows = vec![
        ("used 0.95", with_window(window(18_000, 0.95)), 1),
        ("used 0.9001", with_window(window(18_000, 0.9001)), 1),
        ("used 0.90", with_window(window(18_000, 0.90)), 0),
        ("no window", with_account(seat_a()), 0),
        (
            "stale at 0.95",
            with_window(WindowSnapshot {
                stale: true,
                ..window(18_000, 0.95)
            }),
            0,
        ),
    ];

    assert_rows(AttentionKind::AccountNearLimit, rows);
}

#[test]
fn usage_poll_failing_is_listed_once_the_failure_run_is_fifteen_minutes_old() {
    let failing_since = |since_ms| {
        with_account(AccountSnapshot {
            poll_failure: Some(PollFailure {
                since_ms,
                last_result: Some("http_5xx".to_string()),
            }),
            ..seat_a()
        })
    };
    let rows = vec![
        ("15:00", failing_since(NOW - 15 * MIN), 1),
        ("14:59", failing_since(NOW - 15 * MIN + SEC), 0),
        ("no failure run", with_account(seat_a()), 0),
    ];

    assert_rows(AttentionKind::UsagePollFailing, rows);
}

#[test]
fn records_dropped_is_listed_for_five_minutes_after_a_loss_counter_rise() {
    let rose_at = |last_rise_ms| AttentionInputs {
        record_loss: RecordLoss {
            dropped_full: 3,
            last_rise_ms,
            ..RecordLoss::default()
        },
        ..inputs()
    };
    let rows = vec![
        ("rose 1 min ago", rose_at(Some(NOW - MIN)), 1),
        ("rose 5 min ago", rose_at(Some(NOW - 5 * MIN)), 1),
        (
            "rose just over 5 min ago",
            rose_at(Some(NOW - 5 * MIN - 1)),
            0,
        ),
        ("nonzero, never rose", rose_at(None), 0),
    ];

    assert_rows(AttentionKind::RecordsDropped, rows);
}

#[test]
fn doctor_failure_is_listed_per_fail_finding_outside_the_pools_section() {
    let with_findings = |doctor_findings| AttentionInputs {
        doctor_findings,
        ..inputs()
    };
    let rows = vec![
        (
            "fail in config",
            with_findings(vec![finding("config", "schema", Status::Fail)]),
            1,
        ),
        (
            "warn in config",
            with_findings(vec![finding("config", "schema", Status::Warn)]),
            0,
        ),
        (
            "pass in config",
            with_findings(vec![finding("config", "schema", Status::Pass)]),
            0,
        ),
        (
            "fail in pools",
            with_findings(vec![finding("pools", "pool-a", Status::Fail)]),
            0,
        ),
    ];

    assert_rows(AttentionKind::DoctorFailure, rows);
}

/// What one item of a kind carries, besides its kind and active state.
struct Expected {
    key: &'static str,
    severity: Severity,
    subject: &'static str,
    account: Option<&'static str>,
    cause: &'static str,
    raw_token: &'static str,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    fix: Option<&'static str>,
    target: ViewTarget,
}

fn field_rows() -> Vec<(AttentionKind, AttentionInputs, Expected)> {
    vec![
        (
            AttentionKind::PoolEmpty,
            AttentionInputs {
                pools: vec![pool("pool-a", 2, 0)],
                ..inputs()
            },
            Expected {
                key: "pool_empty:pool-a",
                severity: Severity::Broken,
                subject: "pool-a",
                account: None,
                cause: "No usable seat, 0 of 2",
                raw_token: "pool_empty (eligible=0, configured=2)",
                since_ms: None,
                until_ms: None,
                fix: None,
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::RouteCircuitOpen,
            AttentionInputs {
                routes: vec![route("fast#seat-a", Some(ACCOUNT), true)],
                ..with_account(seat_a())
            },
            Expected {
                key: "route_circuit_open:fast#seat-a",
                severity: Severity::Degraded,
                subject: "fast#seat-a",
                account: Some(ACCOUNT),
                cause: "Circuit open, for 2 min",
                raw_token: "breaker_open",
                since_ms: Some(NOW - 2 * MIN),
                until_ms: None,
                fix: None,
                target: ViewTarget::Models,
            },
        ),
        (
            AttentionKind::AccountUnusable,
            with_account(AccountSnapshot {
                credential: logged_out(NOW - 2 * MIN),
                ..seat_a()
            }),
            Expected {
                key: "account_unusable:anthropic#seat-a",
                severity: Severity::Degraded,
                subject: DISPLAY,
                account: Some(ACCOUNT),
                cause: "Not logged in, for 2 min",
                raw_token: "credential_unusable (not_logged_in)",
                since_ms: Some(NOW - 2 * MIN),
                until_ms: None,
                fix: Some(FIX),
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::AccountParked,
            with_account(AccountSnapshot {
                credential: CredentialState::Parked {
                    reason: UpstreamReason::CredentialInvalid,
                    until_ms: NOW + 40 * MIN,
                },
                ..seat_a()
            }),
            Expected {
                key: "account_parked:anthropic#seat-a",
                severity: Severity::Degraded,
                subject: DISPLAY,
                account: Some(ACCOUNT),
                cause: "Credential refused by the provider, retried in 40 min",
                raw_token: "account_parked (credential-invalid)",
                since_ms: None,
                until_ms: Some(NOW + 40 * MIN),
                fix: Some(FIX),
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::AccountRefreshFailing,
            with_account(AccountSnapshot {
                refresh_failure: Some(RefreshFailure {
                    first_failure_ms: NOW - 4 * MIN,
                    token: "transient",
                }),
                ..seat_a()
            }),
            Expected {
                key: "account_refresh_failing:anthropic#seat-a",
                severity: Severity::Degraded,
                subject: DISPLAY,
                account: Some(ACCOUNT),
                cause: "Token refresh failing, for 4 min",
                raw_token: "refresh_failing (transient)",
                since_ms: Some(NOW - 4 * MIN),
                until_ms: None,
                fix: Some(FIX),
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::DriverStopped,
            AttentionInputs {
                drivers: DriverSnapshot {
                    refresh_running: false,
                    poll_running: true,
                },
                managed_accounts: 1,
                ..inputs()
            },
            Expected {
                key: "driver_stopped:refresh",
                severity: Severity::Degraded,
                subject: "token refresh driver",
                account: None,
                cause: "Not running",
                raw_token: "driver_stopped (refresh)",
                since_ms: None,
                until_ms: None,
                fix: None,
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::DriverStopped,
            AttentionInputs {
                drivers: DriverSnapshot {
                    refresh_running: true,
                    poll_running: false,
                },
                managed_accounts: 1,
                ..inputs()
            },
            Expected {
                key: "driver_stopped:poll",
                severity: Severity::Degraded,
                subject: "usage poll driver",
                account: None,
                cause: "Not running",
                raw_token: "driver_stopped (poll)",
                since_ms: None,
                until_ms: None,
                fix: None,
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::AccountNearLimit,
            with_account(AccountSnapshot {
                windows: vec![WindowSnapshot {
                    scope: Some("opus".to_string()),
                    ..window(7 * 86_400, 0.93)
                }],
                ..seat_a()
            }),
            Expected {
                key: "account_near_limit:anthropic#seat-a",
                severity: Severity::Degraded,
                subject: DISPLAY,
                account: Some(ACCOUNT),
                cause: "93% of the 7 day opus window used, resets in 2 h",
                raw_token: "account_near_limit (used=0.93, length_secs=604800, scope=opus)",
                since_ms: None,
                until_ms: Some(NOW + 2 * HOUR),
                fix: None,
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::UsagePollFailing,
            with_account(AccountSnapshot {
                poll_failure: Some(PollFailure {
                    since_ms: NOW - 20 * MIN,
                    last_result: Some("http_5xx".to_string()),
                }),
                ..seat_a()
            }),
            Expected {
                key: "usage_poll_failing:anthropic#seat-a",
                severity: Severity::Degraded,
                subject: DISPLAY,
                account: Some(ACCOUNT),
                cause: "Usage poll failing, for 20 min",
                raw_token: "usage_poll_failing (http_5xx)",
                since_ms: Some(NOW - 20 * MIN),
                until_ms: None,
                fix: None,
                target: ViewTarget::Accounts,
            },
        ),
        (
            AttentionKind::RecordsDropped,
            AttentionInputs {
                record_loss: RecordLoss {
                    dropped_full: 3,
                    dropped_closed: 0,
                    write_errors: 1,
                    last_rise_ms: Some(NOW - MIN),
                },
                ..inputs()
            },
            Expected {
                key: "records_dropped:ledger",
                severity: Severity::Degraded,
                subject: "usage ledger",
                account: None,
                cause: "Usage records dropped, last 1 min ago",
                raw_token: "records_dropped (dropped_full=3, dropped_closed=0, write_errors=1)",
                since_ms: Some(NOW - MIN),
                until_ms: None,
                fix: None,
                target: ViewTarget::Traffic,
            },
        ),
        (
            AttentionKind::DoctorFailure,
            AttentionInputs {
                doctor_findings: vec![finding("config", "schema", Status::Fail)],
                ..inputs()
            },
            Expected {
                key: "doctor_failure:config/schema",
                severity: Severity::Degraded,
                subject: "schema",
                account: None,
                cause: "schema detail",
                raw_token: "doctor_fail (config/schema)",
                since_ms: None,
                until_ms: None,
                fix: Some("schema remediation"),
                target: ViewTarget::Doctor,
            },
        ),
    ]
}

#[test]
fn each_kind_carries_its_severity_wording_raw_token_fix_and_target() {
    for (kind, row, want) in field_rows() {
        let items = active_items(&row);

        assert_eq!(items.len(), 1, "{}: {items:?}", kind.as_str());
        let item = &items[0];
        let name = want.key;
        assert_eq!(item.kind, kind, "{name}");
        assert_eq!(item.state, ItemState::Active, "{name}");
        assert_eq!(item.key, want.key, "{name}");
        assert_eq!(item.severity, want.severity, "{name}");
        assert_eq!(item.subject, want.subject, "{name}");
        assert_eq!(item.account.as_deref(), want.account, "{name}");
        assert_eq!(item.cause, want.cause, "{name}");
        assert_eq!(item.raw_token, want.raw_token, "{name}");
        assert_eq!(item.since_ms, want.since_ms, "{name}");
        assert_eq!(item.until_ms, want.until_ms, "{name}");
        assert_eq!(item.recovered_at_ms, None, "{name}");
        assert_eq!(item.fix.as_deref(), want.fix, "{name}");
        assert_eq!(item.target, Some(want.target), "{name}");
    }
}

#[test]
fn only_pool_empty_is_broken() {
    let kinds_seen: Vec<AttentionKind> = field_rows().into_iter().map(|row| row.0).collect();

    for kind in AttentionKind::ALL {
        assert!(
            kinds_seen.contains(&kind),
            "{} has no field row",
            kind.as_str()
        );
        let expected = if kind == AttentionKind::PoolEmpty {
            Severity::Broken
        } else {
            Severity::Degraded
        };
        assert_eq!(kind.severity(), expected, "{}", kind.as_str());
    }
}

#[test]
fn every_built_string_is_ascii() {
    for (kind, row, _) in field_rows() {
        let json = serde_json::to_string(&build(&row)).expect("serializes");

        assert!(json.is_ascii(), "{}: {json}", kind.as_str());
    }
}

#[test]
fn an_unusable_account_behind_three_open_routes_is_one_item() {
    let row = AttentionInputs {
        routes: vec![
            route("fast#seat-a", Some(ACCOUNT), true),
            route("slow#seat-a", Some(ACCOUNT), true),
            route("chat#seat-a", Some(ACCOUNT), true),
        ],
        ..with_account(AccountSnapshot {
            credential: logged_out(NOW - MIN),
            ..seat_a()
        })
    };

    let kinds = kinds_of(&row);

    assert_eq!(kinds, vec![AttentionKind::AccountUnusable]);
}

#[test]
fn an_empty_pool_of_two_logged_out_seats_is_one_pool_item_and_two_account_items() {
    let row = AttentionInputs {
        pools: vec![pool("pool-a", 2, 0)],
        routes: vec![
            route("fast#a", Some("anthropic#a"), true),
            route("fast#b", Some("anthropic#b"), true),
        ],
        accounts: vec![
            AccountSnapshot {
                credential: logged_out(NOW - MIN),
                ..account("anthropic#a")
            },
            AccountSnapshot {
                credential: logged_out(NOW - MIN),
                ..account("anthropic#b")
            },
        ],
        ..inputs()
    };

    let kinds = kinds_of(&row);

    assert_eq!(
        kinds,
        vec![
            AttentionKind::PoolEmpty,
            AttentionKind::AccountUnusable,
            AttentionKind::AccountUnusable,
        ]
    );
}

#[test]
fn two_windows_past_the_limit_on_one_account_are_one_item_naming_the_fullest() {
    let row = with_account(AccountSnapshot {
        windows: vec![
            window(18_000, 0.92),
            window(7 * 86_400, 0.97),
            WindowSnapshot {
                stale: true,
                ..window(86_400, 0.99)
            },
        ],
        ..seat_a()
    });

    let items = active_items(&row);

    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        items[0].raw_token,
        "account_near_limit (used=0.97, length_secs=604800)"
    );
}

#[test]
fn an_unusable_account_with_a_failing_refresh_is_one_unusable_item_naming_the_refresh() {
    let row = with_account(AccountSnapshot {
        credential: CredentialState::Unusable {
            cause: UnusableCause::Expired,
            since_ms: NOW - 3 * MIN,
        },
        refresh_failure: Some(RefreshFailure {
            first_failure_ms: NOW - 10 * MIN,
            token: "transient",
        }),
        ..seat_a()
    });

    let items = active_items(&row);

    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].kind, AttentionKind::AccountUnusable);
    assert_eq!(
        items[0].raw_token,
        "credential_unusable (expired); refresh_failing (transient)"
    );
}

#[test]
fn a_parked_account_with_a_failing_refresh_is_one_parked_item_naming_the_refresh() {
    let row = with_account(AccountSnapshot {
        credential: CredentialState::Parked {
            reason: UpstreamReason::CredentialExpired,
            until_ms: NOW + 5 * MIN,
        },
        refresh_failure: Some(RefreshFailure {
            first_failure_ms: NOW - 10 * MIN,
            token: "transient",
        }),
        ..seat_a()
    });

    let items = active_items(&row);

    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].kind, AttentionKind::AccountParked);
    assert_eq!(
        items[0].raw_token,
        "account_parked (credential-expired); refresh_failing (transient)"
    );
}

fn near_limit_cause(window: WindowSnapshot) -> AttentionItem {
    let row = with_account(AccountSnapshot {
        windows: vec![window],
        ..seat_a()
    });
    let items = active_items(&row);
    assert_eq!(items.len(), 1, "{items:?}");
    items.into_iter().next().expect("one item")
}

#[test]
fn a_near_limit_cause_names_a_plain_scope_and_keeps_a_limit_id_to_the_raw_token() {
    let scoped = |scope: Option<&str>| WindowSnapshot {
        scope: scope.map(str::to_string),
        ..window(7 * 86_400, 0.93)
    };
    let rows = [
        (
            "model family",
            scoped(Some("sonnet")),
            "93% of the 7 day sonnet window used, resets in 2 h",
            ", scope=sonnet)",
        ),
        (
            "vendor limit id",
            scoped(Some("codex_bengalfox")),
            "93% of the 7 day window used, resets in 2 h",
            ", scope=codex_bengalfox)",
        ),
        (
            "digits in the tag",
            scoped(Some("gpt5")),
            "93% of the 7 day window used, resets in 2 h",
            ", scope=gpt5)",
        ),
        (
            "non-ASCII letters",
            scoped(Some("\u{fc}ber")),
            "93% of the 7 day window used, resets in 2 h",
            ", scope=\u{fc}ber)",
        ),
        (
            "empty tag",
            scoped(Some("")),
            "93% of the 7 day window used, resets in 2 h",
            ", scope=)",
        ),
        (
            "account-wide",
            scoped(None),
            "93% of the 7 day window used, resets in 2 h",
            "length_secs=604800)",
        ),
    ];

    for (name, window, cause, token_tail) in rows {
        let item = near_limit_cause(window);

        assert_eq!(item.cause, cause, "{name}");
        assert!(item.raw_token.ends_with(token_tail), "{name}: {item:?}");
    }
}

#[test]
fn a_near_limit_cause_reads_above_the_line_and_below_full_until_full() {
    let rows = [
        ("just past the line", 0.9001, "91%"),
        ("a whole percent", 0.93, "93%"),
        ("a hair under full", 0.995, "99%"),
        ("full", 1.0, "100%"),
    ];

    for (name, used, percent) in rows {
        let item = near_limit_cause(window(18_000, used));

        assert_eq!(
            item.cause,
            format!("{percent} of the 5 h window used, resets in 2 h"),
            "{name}"
        );
    }
}

#[test]
fn credential_cause_words_each_state_with_its_age() {
    let rows = [
        ("usable", CredentialState::Usable, None),
        (
            "not logged in, 30 s",
            logged_out(NOW - 30 * SEC),
            Some("Not logged in, for under 1 min"),
        ),
        (
            "since in the future",
            logged_out(NOW + MIN),
            Some("Not logged in, for under 1 min"),
        ),
        (
            "unreadable, 3 h",
            CredentialState::Unusable {
                cause: UnusableCause::Unreadable,
                since_ms: NOW - 3 * HOUR - 59 * MIN,
            },
            Some("Credential cannot be read, for 3 h"),
        ),
        (
            "expired, 1 day",
            CredentialState::Unusable {
                cause: UnusableCause::Expired,
                since_ms: NOW - DAY,
            },
            Some("Login expired, for 1 day"),
        ),
        (
            "revoked, 3 days",
            CredentialState::Unusable {
                cause: UnusableCause::Revoked,
                since_ms: NOW - 3 * DAY,
            },
            Some("Login revoked by the provider, for 3 days"),
        ),
        (
            "parked",
            CredentialState::Parked {
                reason: UpstreamReason::CredentialExpired,
                until_ms: NOW + 59 * SEC,
            },
            Some("Credential refused by the provider, retried in under 1 min"),
        ),
    ];

    for (name, state, expected) in rows {
        assert_eq!(credential_cause(&state, NOW).as_deref(), expected, "{name}");
    }
}

fn item(kind: AttentionKind, subject: &str) -> AttentionItem {
    AttentionItem {
        key: format!("{}:{subject}", kind.as_str()),
        kind,
        severity: kind.severity(),
        state: ItemState::Active,
        subject: subject.to_string(),
        account: None,
        cause: format!("{subject} cause"),
        raw_token: kind.as_str().to_string(),
        since_ms: None,
        until_ms: None,
        recovered_at_ms: None,
        fix: None,
        target: Some(kind.target()),
    }
}

#[test]
fn the_verdict_is_the_worst_active_routing_or_account_item() {
    let pool = item(AttentionKind::PoolEmpty, "pool-a");
    let route = item(AttentionKind::RouteCircuitOpen, "fast");
    let seat = item(AttentionKind::AccountUnusable, "claude-a (a)");
    let rows = [
        (
            "nothing listed",
            vec![],
            VerdictSeverity::Ok,
            "All routes healthy",
            None,
        ),
        (
            "one degraded",
            vec![seat.clone()],
            VerdictSeverity::Degraded,
            "claude-a (a): claude-a (a) cause",
            Some(ViewTarget::Accounts),
        ),
        (
            "two degraded",
            vec![route.clone(), seat.clone()],
            VerdictSeverity::Degraded,
            "fast: fast cause, and 1 more",
            Some(ViewTarget::Models),
        ),
        (
            "broken after degraded",
            vec![route, seat, pool],
            VerdictSeverity::Broken,
            "pool-a: pool-a cause, and 2 more",
            Some(ViewTarget::Accounts),
        ),
    ];

    for (name, items, severity, text, target) in rows {
        let attention = Attention::from_items(items);

        assert_eq!(attention.verdict.severity, severity, "{name}");
        assert_eq!(attention.verdict.text, text, "{name}");
        assert_eq!(attention.verdict.target, target, "{name}");
    }
}

#[test]
fn doctor_failures_and_dropped_records_count_but_never_move_the_verdict() {
    let rows = [
        (
            "only other items",
            vec![
                item(AttentionKind::DoctorFailure, "schema"),
                item(AttentionKind::RecordsDropped, "usage ledger"),
            ],
            VerdictSeverity::Ok,
            "All routes healthy; 2 other items need attention",
            None,
            Counts {
                broken: 0,
                degraded: 2,
                recovered: 0,
                doctor_fail: 1,
            },
        ),
        (
            "one other item beside a routing item",
            vec![
                item(AttentionKind::RecordsDropped, "usage ledger"),
                item(AttentionKind::AccountParked, "claude-a (a)"),
            ],
            VerdictSeverity::Degraded,
            "claude-a (a): claude-a (a) cause; 1 other item needs attention",
            Some(ViewTarget::Accounts),
            Counts {
                broken: 0,
                degraded: 2,
                recovered: 0,
                doctor_fail: 0,
            },
        ),
    ];

    for (name, items, severity, text, target, counts) in rows {
        let attention = Attention::from_items(items);

        assert_eq!(attention.verdict.severity, severity, "{name}");
        assert_eq!(attention.verdict.text, text, "{name}");
        assert_eq!(attention.verdict.target, target, "{name}");
        assert_eq!(attention.counts, counts, "{name}");
    }
}

#[test]
fn recovered_items_never_count_toward_the_verdict_or_the_active_tallies() {
    let recovered_pool = item(AttentionKind::PoolEmpty, "pool-a").into_recovered(NOW - MIN);
    let recovered_doctor = item(AttentionKind::DoctorFailure, "schema").into_recovered(NOW);
    let active = item(AttentionKind::AccountUnusable, "claude-a (a)");

    let attention = Attention::from_items(vec![recovered_pool, recovered_doctor, active]);

    assert_eq!(attention.verdict.severity, VerdictSeverity::Degraded);
    assert_eq!(attention.verdict.text, "claude-a (a): claude-a (a) cause");
    assert_eq!(
        attention.counts,
        Counts {
            broken: 0,
            degraded: 1,
            recovered: 2,
            doctor_fail: 0,
        }
    );
}

#[test]
fn items_are_listed_active_before_recovered_and_worst_first() {
    let recovered = item(AttentionKind::PoolEmpty, "pool-b").into_recovered(NOW);
    let doctor = item(AttentionKind::DoctorFailure, "schema");
    let seat = item(AttentionKind::AccountUnusable, "claude-a (a)");
    let pool = item(AttentionKind::PoolEmpty, "pool-a");

    let attention = Attention::from_items(vec![recovered, doctor, seat, pool]);

    let keys: Vec<&str> = attention.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "pool_empty:pool-a",
            "account_unusable:claude-a (a)",
            "doctor_failure:schema",
            "pool_empty:pool-b",
        ]
    );
}

#[test]
fn the_built_list_serializes_every_contract_key_with_closed_tokens() {
    let row = AttentionInputs {
        pools: vec![pool("pool-a", 1, 0)],
        ..inputs()
    };

    let json = serde_json::to_value(build(&row)).expect("serializes");

    let keys = |value: &Value| {
        let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(keys(&json), ["counts", "items", "verdict"]);
    assert_eq!(keys(&json["verdict"]), ["severity", "target", "text"]);
    assert_eq!(
        keys(&json["counts"]),
        ["broken", "degraded", "doctor_fail", "recovered"]
    );
    assert_eq!(
        keys(&json["items"][0]),
        [
            "account",
            "cause",
            "fix",
            "key",
            "kind",
            "raw_token",
            "recovered_at_ms",
            "severity",
            "since_ms",
            "state",
            "subject",
            "target",
            "until_ms",
        ]
    );
    assert_eq!(json["verdict"]["severity"], "broken");
    assert_eq!(json["verdict"]["target"], "accounts");
    assert_eq!(json["items"][0]["kind"], "pool_empty");
    assert_eq!(json["items"][0]["severity"], "broken");
    assert_eq!(json["items"][0]["state"], "active");
    assert_eq!(json["counts"]["broken"], 1);
}
