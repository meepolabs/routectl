// The trailing `--last-24h` / `--last-7d` windows: exact durations ending at
// the pinned now, whatever the zone and across daylight-saving changes.
// `include!`d into `usage_tests.rs`, so these compile into THAT module and its
// helpers stay in scope; all imports live in the host file -- do not add `use`
// lines here.

const TRAILING_WINDOWS: [(WindowFlag, i64, &str); 2] = [
    (WindowFlag::Last24h, 86_400_000, "== last 24 hours =="),
    (WindowFlag::Last7d, 604_800_000, "== last 7 days =="),
];

/// Every zone the bounds are read in: a trailing window must not depend on it.
#[cfg(unix)]
fn trailing_zones() -> Vec<(&'static str, LocalZone)> {
    vec![
        ("eastern", zone()),
        (
            "midnight dst",
            LocalZone::from_posix_tz(&crate::test_tz::midnight_dst()).unwrap(),
        ),
        ("utc", LocalZone::utc()),
    ]
}

#[cfg(unix)]
#[test]
fn trailing_windows_are_exact_durations_ending_now_across_dst_changes() {
    // Eastern local instants whose trailing day or week straddles a 23-hour or
    // a 25-hour day, plus an ordinary afternoon.
    let nows = [
        ("end of the 23-hour day", local_now(2026, 3, 8, 23, 59)),
        ("morning after spring-forward", local_now(2026, 3, 9, 1, 30)),
        ("week after spring-forward", local_now(2026, 3, 13, 12, 0)),
        ("end of the 25-hour day", local_now(2026, 11, 1, 23, 59)),
        ("morning after fall-back", local_now(2026, 11, 2, 1, 30)),
        ("week after fall-back", local_now(2026, 11, 6, 12, 0)),
        ("ordinary afternoon", fixed_now()),
    ];
    for (name, now) in nows {
        for (zone_name, zone) in trailing_zones() {
            for (flag, span_ms, _) in TRAILING_WINDOWS {
                let bounds = window_bounds(&zone, flag, now);
                assert_eq!(
                    bounds.to_ms,
                    unix_ms(now) + 1,
                    "{name} in {zone_name}: {flag:?} ends at the pinned now"
                );
                assert_eq!(
                    bounds.to_ms - bounds.from_ms,
                    span_ms,
                    "{name} in {zone_name}: {flag:?} is exactly {span_ms} ms"
                );
            }
        }
    }
}

#[test]
fn trailing_windows_select_rows_and_print_their_own_title() {
    // Arrange: one row just inside each trailing window's lower edge and one
    // just outside it. The window is half-open, so the row AT `from_ms` counts.
    let (_dir, _path, mut db) = temp_db();
    let to_ms = unix_ms(fixed_now()) + 1;
    let seeds = [
        ("in-24h", to_ms - 86_400_000),
        ("out-24h", to_ms - 86_400_000 - 1),
        ("in-7d", to_ms - 604_800_000),
        ("out-7d", to_ms - 604_800_000 - 1),
    ];
    for (id, ts) in seeds {
        insert_row(
            &db,
            id,
            ts,
            "m",
            "paid",
            "up-paid",
            "al",
            "ok",
            Some(10),
            Some(10),
            5,
        );
    }
    let config = cost_config();
    let expected = [(WindowFlag::Last24h, 1), (WindowFlag::Last7d, 3)];

    for ((flag, _, title), (_, requests)) in TRAILING_WINDOWS.into_iter().zip(expected) {
        let args = UsageArgs {
            window: flag,
            since: None,
            until: None,
            by: None,
            detail: false,
            db: None,
            k_calibration: false,
        };

        // Act
        let blocks =
            build_blocks(&mut db, &config, &no_prices(), &args, &zone(), fixed_now()).unwrap();
        let bounds = window_bounds(&zone(), flag, fixed_now());
        let report = build_window_report(
            &mut db,
            &config,
            &no_prices(),
            title.into(),
            bounds,
            None,
            false,
        )
        .unwrap();

        // Assert
        assert_eq!(blocks.len(), 1, "{flag:?} is one window");
        assert!(
            blocks[0].starts_with(title),
            "{flag:?} prints `{title}`: {}",
            blocks[0]
        );
        assert_eq!(
            find(&report, "total").metrics.requests,
            requests,
            "{flag:?} selects its rows"
        );
    }
}
