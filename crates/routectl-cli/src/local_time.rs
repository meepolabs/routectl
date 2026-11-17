//! The one place routectl converts between instants and the host's local
//! wall clock: usage windows ("today", "this week"), and the local timestamp
//! text the CLI prints.
//!
//! A [`LocalZone`] is loaded once per CLI command or daemon request and passed
//! down, so a host zone change is picked up by the next command or request
//! without a restart, and tests can build a zone from a POSIX TZ string.
//!
//! On Unix the zone rules come from tz-rs, resolved as the C library does:
//! the `TZ` variable when set (an empty value is UTC), else
//! `/etc/localtime`. Off Unix tz-rs has no rule source, so the offset of each
//! instant is asked of the OS through `time`. Unix never takes that path:
//! there `time` reads the offset through `localtime_r`, which is not sound
//! once other threads run.
//!
//! A zone that fails to load falls back to UTC with one warning per load.
//!
//! [`calendar_text`] renders the date-time layouts routectl prints, local and
//! UTC alike, for every year the text forms have ever carried.

use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// A local civil time resolved to instants: one, two (the repeated hour of a
/// fall-back), or none (the skipped hour of a spring-forward).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CivilInstant {
    /// Exactly one instant shows this wall-clock time.
    Single(OffsetDateTime),
    /// Two instants show it; `earliest` comes first on the timeline.
    Ambiguous {
        earliest: OffsetDateTime,
        latest: OffsetDateTime,
    },
    /// No instant shows it.
    Skipped,
}

impl CivilInstant {
    /// The single instant, or the later of two: the one read at the smaller
    /// UTC offset. This is the instant every local window edge has resolved a
    /// repeated wall-clock time to; picking the earlier one instead moves the
    /// edge back by the repeated span.
    pub const fn latest(self) -> Option<OffsetDateTime> {
        match self {
            Self::Single(at) | Self::Ambiguous { latest: at, .. } => Some(at),
            Self::Skipped => None,
        }
    }
}

/// The host's local time-zone rules for one command or request.
#[derive(Debug, Clone)]
pub struct LocalZone {
    rules: Rules,
}

#[derive(Debug, Clone)]
enum Rules {
    Utc,
    #[cfg(unix)]
    Tz(tz::TimeZone),
    #[cfg(windows)]
    Os,
}

impl LocalZone {
    /// The host's zone. Never fails: a zone that cannot be read is UTC, with
    /// one warning naming the failure class.
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(zone) => zone,
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "local time zone could not be loaded; local usage windows and \
                     timestamps use UTC"
                );
                Self::utc()
            }
        }
    }

    #[cfg(unix)]
    fn try_load() -> Result<Self, &'static str> {
        let rules = match std::env::var_os("TZ") {
            Some(tz) if tz.is_empty() => return Ok(Self::utc()),
            Some(tz) => {
                let tz = tz.to_str().ok_or("tz_not_utf8")?;
                tz::TimeZone::from_posix_tz(tz).map_err(|_| "tz_unreadable")?
            }
            None => tz::TimeZone::local().map_err(|_| "localtime_unreadable")?,
        };
        Ok(Self {
            rules: Rules::Tz(rules),
        })
    }

    #[cfg(windows)]
    #[allow(clippy::unnecessary_wraps)]
    fn try_load() -> Result<Self, &'static str> {
        Ok(Self { rules: Rules::Os })
    }

    #[cfg(not(any(unix, windows)))]
    fn try_load() -> Result<Self, &'static str> {
        Err("no_zone_source")
    }

    /// UTC, with no transitions.
    pub const fn utc() -> Self {
        Self { rules: Rules::Utc }
    }

    /// The zone a POSIX TZ string or zone name describes, as the `TZ`
    /// variable would, or `None` when it does not parse.
    #[cfg(all(test, unix))]
    pub(crate) fn from_posix_tz(tz: &str) -> Option<Self> {
        let rules = tz::TimeZone::from_posix_tz(tz).ok()?;
        Some(Self {
            rules: Rules::Tz(rules),
        })
    }

    /// The UTC offset in force at a Unix instant (seconds). UTC when the
    /// rules have no answer for it.
    pub fn offset_at(&self, unix_s: i64) -> UtcOffset {
        match &self.rules {
            Rules::Utc => UtcOffset::UTC,
            #[cfg(unix)]
            Rules::Tz(rules) => rules
                .find_local_time_type(unix_s)
                .ok()
                .and_then(|kind| UtcOffset::from_whole_seconds(kind.ut_offset()).ok())
                .unwrap_or(UtcOffset::UTC),
            #[cfg(windows)]
            Rules::Os => os_offset_at(unix_s).unwrap_or(UtcOffset::UTC),
        }
    }

    /// `at` in this zone's local offset.
    pub fn to_local(&self, at: OffsetDateTime) -> OffsetDateTime {
        at.to_offset(self.offset_at(at.unix_timestamp()))
    }

    /// The instant now, in this zone's local offset.
    pub fn now(&self) -> OffsetDateTime {
        self.to_local(OffsetDateTime::now_utc())
    }

    /// The Unix-millisecond instant in this zone's local offset, or `None`
    /// outside the representable calendar.
    pub fn at_unix_ms(&self, unix_ms: i64) -> Option<OffsetDateTime> {
        let nanos = i128::from(unix_ms) * 1_000_000;
        let at = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
        let local = at.checked_to_offset(self.offset_at(at.unix_timestamp()))?;
        Some(local)
    }

    /// Every instant whose local wall clock reads `civil`. The candidates are
    /// `civil` read at the offsets in force a day either side of it; a
    /// candidate counts if the zone's offset at that instant is the one it was
    /// read at, or if `civil` is the first wall-clock time of a spring-forward
    /// gap that is kept as an instant at the old offset (see
    /// `keeps_gap_start`).
    pub fn resolve(&self, civil: PrimitiveDateTime) -> CivilInstant {
        let naive_s = civil.assume_utc().unix_timestamp();
        let mut found: Vec<OffsetDateTime> = [-86_400, 86_400]
            .into_iter()
            .filter_map(|probe| {
                let offset = self.offset_at(naive_s.checked_add(probe)?);
                let at = civil.assume_offset(offset);
                let unix_s = at.unix_timestamp();
                let holds = self.offset_at(unix_s) == offset
                    || (self.offset_at(unix_s.saturating_sub(1)) == offset
                        && self.keeps_gap_start(civil, unix_s));
                holds.then_some(at)
            })
            .collect();
        found.sort();
        found.dedup();
        match found.as_slice() {
            [] => CivilInstant::Skipped,
            [one] => CivilInstant::Single(*one),
            [earliest, .., latest] => CivilInstant::Ambiguous {
                earliest: *earliest,
                latest: *latest,
            },
        }
    }
}

/// The OS's offset for one instant. Windows only: this reads the offset
/// through `SystemTimeToTzSpecificLocalTime`, which is thread-safe there; on
/// Unix the same call goes through `localtime_r` and must never be made.
#[cfg(windows)]
fn os_offset_at(unix_s: i64) -> Option<UtcOffset> {
    let at = OffsetDateTime::from_unix_timestamp(unix_s).ok()?;
    UtcOffset::local_offset_at(at).ok()
}

/// Unix milliseconds of an instant, floored.
pub const fn unix_ms(at: OffsetDateTime) -> i64 {
    at.unix_timestamp_nanos().div_euclid(1_000_000) as i64
}

impl LocalZone {
    /// Whether the first wall-clock time of the spring-forward gap that
    /// starts at `unix_s` names an instant (read at the old offset) rather
    /// than nothing. This reproduces the local-time code routectl used
    /// before, so window edges keep their instants: a transition from the
    /// zone's transition table keeps it, and so does one from the trailing
    /// POSIX rule unless the jump is the later of the rule's two transitions
    /// in the calendar year (a southern-hemisphere rule), where it is
    /// skipped.
    fn keeps_gap_start(&self, civil: PrimitiveDateTime, unix_s: i64) -> bool {
        match &self.rules {
            #[cfg(unix)]
            Rules::Tz(rules) => {
                let table = rules.as_ref().transitions();
                let from_rule = table
                    .last()
                    .is_none_or(|last| unix_s > last.unix_leap_time());
                if !from_rule {
                    return true;
                }
                let year_start = midnight(civil.date().replace_ordinal(1).unwrap_or(civil.date()));
                let jan_first = year_start.assume_utc().unix_timestamp();
                self.offset_at(jan_first) != self.offset_at(unix_s)
            }
            _ => true,
        }
    }
}

/// A date-time text layout routectl prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextLayout {
    /// `YYYY-MM-DD HH:MM:SS`
    Seconds,
    /// `YYYY-MM-DD HH:MM`
    Minutes,
    /// `YYYY-MM-DDTHH:MM:SSZ`; meaningful only at the UTC offset.
    Rfc3339Utc,
    /// `YYYY-MM-DD HH:MM:SS UTC`; meaningful only at the UTC offset.
    SecondsUtc,
}

impl TextLayout {
    const fn description(self) -> &'static [BorrowedFormatItem<'static>] {
        match self {
            Self::Seconds => format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
            Self::Minutes => format_description!("[year]-[month]-[day] [hour]:[minute]"),
            Self::Rfc3339Utc => {
                format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z")
            }
            Self::SecondsUtc => {
                format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC")
            }
        }
    }
}

/// The text of a Unix second count read at `offset`, or `None` when the
/// instant (in UTC) is outside years -262143..=262142; the local date may
/// fall a day beyond that. Years -9999..=9999 are four digits (a negative year
/// is signed); a later year is written whole with a `+` sign, an earlier one
/// whole with its `-`.
pub fn calendar_text(unix_s: i64, offset: UtcOffset, layout: TextLayout) -> Option<String> {
    if !in_text_range(unix_s) {
        return None;
    }
    let local_s = unix_s.checked_add(i64::from(offset.whole_seconds()))?;
    if let Ok(fields) = OffsetDateTime::from_unix_timestamp(local_s) {
        return fields.format(layout.description()).ok();
    }
    Some(extended_year_text(local_s, layout))
}

/// The local text of a Unix second count in `zone`.
pub fn local_text(zone: &LocalZone, unix_s: i64, layout: TextLayout) -> Option<String> {
    calendar_text(unix_s, zone.offset_at(unix_s), layout)
}

/// `YYYY-MM-DD HH:MM:SS` local, or the raw milliseconds when the instant is
/// outside the printable calendar.
pub fn format_seconds(zone: &LocalZone, unix_ms: i64) -> String {
    local_text(zone, unix_ms.div_euclid(1000), TextLayout::Seconds)
        .unwrap_or_else(|| unix_ms.to_string())
}

/// `YYYY-MM-DD HH:MM` local, or the raw milliseconds when the instant is
/// outside the printable calendar.
pub fn format_minutes(zone: &LocalZone, unix_ms: i64) -> String {
    local_text(zone, unix_ms.div_euclid(1000), TextLayout::Minutes)
        .unwrap_or_else(|| unix_ms.to_string())
}

/// First and last Unix second of years -262143..=262142.
const TEXT_RANGE_S: (i64, i64) = (-8_334_601_228_800, 8_210_266_876_799);

const fn in_text_range(unix_s: i64) -> bool {
    TEXT_RANGE_S.0 <= unix_s && unix_s <= TEXT_RANGE_S.1
}

/// The layout's text for a year outside `time`'s calendar.
fn extended_year_text(local_s: i64, layout: TextLayout) -> String {
    let (year, month, day) = civil_from_days(local_s.div_euclid(86_400));
    let second_of_day = local_s.rem_euclid(86_400);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    );
    let year = if year > 9_999 {
        format!("+{year}")
    } else {
        year.to_string()
    };
    match layout {
        TextLayout::Seconds => {
            format!("{year}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
        }
        TextLayout::Minutes => format!("{year}-{month:02}-{day:02} {hour:02}:{minute:02}"),
        TextLayout::Rfc3339Utc => {
            format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
        }
        TextLayout::SecondsUtc => {
            format!("{year}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
        }
    }
}

/// The proleptic Gregorian date of a day count from 1970-01-01.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Midnight starting `date`.
pub const fn midnight(date: Date) -> PrimitiveDateTime {
    PrimitiveDateTime::new(date, Time::MIDNIGHT)
}

#[cfg(test)]
#[path = "local_time_tests.rs"]
mod tests;
