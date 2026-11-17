//! Zone loading, offset lookup and civil-time resolution.

use super::*;

use std::path::{Path, PathBuf};

fn civil(y: i32, m: u8, d: u8, hour: u8, minute: u8) -> PrimitiveDateTime {
    let date = Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d).unwrap();
    PrimitiveDateTime::new(date, Time::from_hms(hour, minute, 0).unwrap())
}

fn at(unix_s: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix_s).unwrap()
}

#[cfg(unix)]
fn eastern() -> LocalZone {
    LocalZone::from_posix_tz(&crate::test_tz::eastern()).unwrap()
}

#[cfg(unix)]
#[test]
fn offset_at_follows_the_zone_rules_across_a_transition() {
    let zone = eastern();
    let rows = [
        ("winter", 1_768_480_496, -5),
        ("last standard second", 1_772_953_199, -5),
        ("first daylight second", 1_772_953_200, -4),
        ("last daylight second", 1_793_512_799, -4),
        ("first standard second again", 1_793_512_800, -5),
    ];
    for (name, unix_s, hours) in rows {
        assert_eq!(zone.offset_at(unix_s).whole_hours(), hours, "{name}");
    }
}

#[cfg(unix)]
#[test]
fn resolve_reports_each_case_with_exact_instants() {
    let zone = eastern();
    assert_eq!(
        zone.resolve(civil(2026, 6, 11, 0, 0)),
        CivilInstant::Single(at(1_781_150_400).to_offset(zone.offset_at(1_781_150_400)))
    );
    assert_eq!(
        zone.resolve(civil(2026, 3, 8, 2, 30)),
        CivilInstant::Skipped
    );
    let CivilInstant::Ambiguous { earliest, latest } = zone.resolve(civil(2026, 11, 1, 1, 30))
    else {
        panic!("01:30 on the fall-back day happens twice");
    };
    assert_eq!(earliest.unix_timestamp(), 1_793_511_000);
    assert_eq!(latest.unix_timestamp(), 1_793_514_600);
    assert_eq!(
        zone.resolve(civil(2026, 11, 1, 1, 30))
            .latest()
            .map(OffsetDateTime::unix_timestamp),
        Some(1_793_514_600)
    );
}

#[test]
fn utc_resolves_every_civil_time_once() {
    let zone = LocalZone::utc();
    assert_eq!(
        zone.resolve(civil(2026, 3, 8, 2, 30)),
        CivilInstant::Single(at(1_772_937_000))
    );
    assert_eq!(zone.offset_at(1_772_937_000), UtcOffset::UTC);
}

#[test]
fn an_unrepresentable_instant_is_none_and_formats_as_raw_milliseconds() {
    let zone = LocalZone::utc();
    assert!(zone.at_unix_ms(i64::MAX).is_none());
    assert_eq!(format_seconds(&zone, i64::MAX), i64::MAX.to_string());
    assert_eq!(format_minutes(&zone, i64::MIN), i64::MIN.to_string());
}

#[test]
fn unix_ms_floors_toward_the_past() {
    let before_epoch = OffsetDateTime::from_unix_timestamp_nanos(-1).unwrap();
    assert_eq!(unix_ms(before_epoch), -1);
    let mid = OffsetDateTime::from_unix_timestamp_nanos(1_999_999).unwrap();
    assert_eq!(unix_ms(mid), 1);
}

/// `load` reads the zone the way the C library does: `TZ` when set (a POSIX
/// rule or a zone name), with an empty value meaning UTC. Run in children with
/// `TZ` set, so the shared test process's environment is never touched.
#[cfg(unix)]
#[test]
fn load_reads_tz_first_and_treats_an_empty_value_as_utc() {
    let test = "load_reads_tz_first_and_treats_an_empty_value_as_utc";
    crate::test_tz::in_zone(&crate::test_tz::eastern(), module_path!(), test, || {
        assert_eq!(LocalZone::load().offset_at(1_768_480_496).whole_hours(), -5);
        assert_eq!(LocalZone::load().offset_at(1_783_134_309).whole_hours(), -4);
    });
    crate::test_tz::in_zone("<+0530>-5:30", module_path!(), test, || {
        assert_eq!(LocalZone::load().offset_at(0).whole_minutes(), 330);
    });
    crate::test_tz::in_zone("", module_path!(), test, || {
        assert_eq!(LocalZone::load().offset_at(1_783_134_309), UtcOffset::UTC);
    });
}

/// A `TZ` that names no readable zone falls back to UTC with exactly one warn
/// line per load, naming only a fixed reason token.
#[cfg(unix)]
#[test]
fn an_unreadable_zone_falls_back_to_utc_with_one_warning() {
    let test = "an_unreadable_zone_falls_back_to_utc_with_one_warning";
    crate::test_tz::in_zone("/nonexistent/zone/file", module_path!(), test, || {
        let mut zone = None;
        let events = routectl_testkit::capture_events(|| zone = Some(LocalZone::load()));
        let warns: Vec<_> = events
            .iter()
            .filter(|event| event.level == tracing::Level::WARN)
            .collect();
        assert_eq!(warns.len(), 1, "{events:?}");
        assert_eq!(warns[0].field("reason"), Some("tz_unreadable"));
        assert!(!warns[0].message.contains("nonexistent"), "{events:?}");
        assert_eq!(zone.unwrap().offset_at(1_783_134_309), UtcOffset::UTC);
    });
    crate::test_tz::in_zone(&crate::test_tz::eastern(), module_path!(), test, || {
        let events = routectl_testkit::capture_events(|| drop(LocalZone::load()));
        assert!(
            events.is_empty(),
            "a readable zone logs nothing: {events:?}"
        );
    });
}

// --- the libc local-offset guard ---

/// `time`'s calls that read the OS offset, directly or through
/// `local_offset_at`. On Unix they run `localtime_r` with no check that the
/// process is single-threaded, so no production code may name them outside
/// this module's Windows-only function.
fn forbidden_calls() -> [String; 3] {
    [
        format!("{}{}", "local_offset", "_at"),
        format!("{}{}", "current_local", "_offset"),
        format!("{}{}", "now", "_local"),
    ]
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The text of `src` with the Windows-only offset function removed: from its
/// `#[cfg(windows)]` attribute to the closing brace at column zero.
fn without_the_windows_offset_fn(src: &str) -> String {
    let opener = format!("#[cfg(windows)]\nfn {}{}(", "os_offset", "_at");
    let Some(start) = src.find(&opener) else {
        return src.to_string();
    };
    let end = src[start..]
        .find("\n}\n")
        .map_or(src.len(), |off| start + off + 3);
    format!("{}{}", &src[..start], &src[end..])
}

/// Every line of a production source in the workspace's crates that names a
/// forbidden call. `_tests.rs` sidecars and inline test modules are
/// excluded; so is the Windows-only function in this module.
fn forbidden_call_sites(crates: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for krate in std::fs::read_dir(crates).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    assert!(files.len() > 200, "walk reached only {} files", files.len());
    let mut hits = Vec::new();
    for path in files {
        if path.to_string_lossy().ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let production =
            crate::handlers::status::production_source::production_source(&text).to_string();
        let scanned = if path.ends_with("routectl-cli/src/local_time.rs") {
            without_the_windows_offset_fn(&production)
        } else {
            production
        };
        for (index, line) in scanned.lines().enumerate() {
            if forbidden_calls()
                .iter()
                .any(|call| line.contains(call.as_str()))
            {
                hits.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }
    hits
}

#[test]
fn no_production_source_reads_the_os_offset_outside_the_windows_path() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let hits = forbidden_call_sites(crates);
    assert!(
        hits.is_empty(),
        "OS offset read outside the Windows path:\n{}",
        hits.join("\n")
    );
}

/// Positive controls for the guard's pieces: the Windows function is the only
/// thing cut from this module, and a forbidden call anywhere else is seen.
#[test]
fn the_offset_guard_sees_a_call_outside_the_windows_function() {
    let module = include_str!("local_time.rs");
    let [offset_at, ..] = forbidden_calls();
    assert!(
        module.contains(&offset_at),
        "the Windows path names the call"
    );
    let cut = without_the_windows_offset_fn(module);
    assert!(
        !cut.contains(&offset_at),
        "the cut removes the Windows function"
    );
    assert!(
        cut.contains("pub fn offset_at("),
        "the cut keeps everything else"
    );

    let planted = format!(
        "fn f() {{ UtcOffset::{offset_at}(now) }}\n#[cfg(windows)]\n{}",
        ""
    );
    assert!(without_the_windows_offset_fn(&planted).contains(&offset_at));
}

/// The first wall-clock time of a spring-forward gap: an instant at the old
/// offset for a northern rule's jump, nothing for a midnight jump that is the
/// later of a southern rule's two transitions. Rows recorded from the
/// local-time code this module replaced.
#[cfg(unix)]
#[test]
fn the_first_minute_of_a_gap_resolves_as_before() {
    let southern = LocalZone::from_posix_tz(&crate::test_tz::midnight_dst()).unwrap();
    let rows = [
        (
            "northern rule jump",
            eastern(),
            civil(2026, 3, 8, 2, 0),
            Some(1_772_953_200),
        ),
        (
            "northern rule, inside",
            eastern(),
            civil(2026, 3, 8, 2, 30),
            None,
        ),
        (
            "southern rule midnight jump",
            southern.clone(),
            civil(2026, 11, 1, 0, 0),
            None,
        ),
        (
            "southern rule, after",
            southern,
            civil(2026, 11, 1, 1, 0),
            Some(1_793_502_000),
        ),
    ];
    for (name, zone, civil, expected) in rows {
        assert_eq!(
            zone.resolve(civil)
                .latest()
                .map(OffsetDateTime::unix_timestamp),
            expected,
            "{name}"
        );
    }
}
