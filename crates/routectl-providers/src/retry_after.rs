//! Parser for the standard HTTP `Retry-After` response header.
//!
//! Per RFC 9110 the header carries either a delta-seconds integer or an
//! HTTP-date. OpenAI and Anthropic additionally emit a non-standard
//! `retry-after-ms` header carrying an integer count of milliseconds,
//! which this module prefers when present so a sub-second reset hint is
//! not rounded up or lost. This module extracts the indicated wait as a
//! [`std::time::Duration`]; the router and circuit breaker consume the
//! parsed value (carried structurally on `Error::Upstream`) to park a
//! provider after a rate-limit / overload response. The shared
//! extraction primitive lives here so each provider's egress can lift
//! the hint with one call.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, RETRY_AFTER};

/// The non-standard millisecond-granular reset header emitted by OpenAI
/// and Anthropic alongside the standard `Retry-After`. Carries an integer
/// count of milliseconds, so it can express a sub-second wait the
/// second-granular `Retry-After` rounds up or loses entirely.
const RETRY_AFTER_MS: &str = "retry-after-ms";

/// Parse an upstream reset hint into a wait [`Duration`].
///
/// Reads two headers, finer-grained first:
///   - `retry-after-ms`: an integer count of milliseconds ->
///     [`Duration::from_millis`]. Preferred when present and parseable,
///     since it can express a sub-second wait the second-granular
///     `Retry-After` cannot.
///   - `Retry-After` (RFC 9110), in either form:
///       - delta-seconds: an integer `N` -> `Duration::from_secs(N)`.
///       - HTTP-date: a date string -> `date - now`, clamped to
///         `Duration::ZERO` when the date is already in the past.
///
/// When both `retry-after-ms` and `Retry-After` are present the finer
/// `-ms` value wins; a malformed `-ms` value falls through to the
/// standard header. Returns `None` when neither header yields a value.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    if let Some(ms) = parse_retry_after_ms(headers) {
        return Some(ms);
    }
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    // delta-seconds form takes precedence: a bare integer is the common
    // shape and is unambiguous against the HTTP-date form.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    parse_http_date_delta(raw)
}

/// Parse the `retry-after-ms` header into a sub-second-capable
/// [`Duration`]. Returns `None` when the header is absent, empty, or not
/// a bare non-negative integer, so the caller falls back to the standard
/// `Retry-After` grammar.
fn parse_retry_after_ms(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER_MS)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let ms = raw.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
}

/// Report whether a reset hint is meaningful for the given HTTP status.
///
/// Only rate-limit / overload statuses carry a useful reset signal:
///   - 429 rate-limit
///   - 503 service-unavailable
///   - 529 anthropic-overloaded
///
/// A stray `Retry-After` on any other status (400/401/500 etc.) must
/// NOT park a provider, so the egress wiring gates hint extraction on
/// this predicate.
pub const fn is_rate_limit_status(status: u16) -> bool {
    matches!(status, 429 | 503 | 529)
}

/// Parse an HTTP-date `Retry-After` value into the delay from now,
/// clamped to `Duration::ZERO` for a past date. RFC 9110's preferred
/// IMF-fixdate form (e.g. `Wed, 21 Oct 2026 07:28:00 GMT`) parses via
/// the RFC 2822 reader below, which accepts the `GMT` zone token.
fn parse_http_date_delta(raw: &str) -> Option<Duration> {
    let when_ms = http_date_unix_ms(raw)?;
    Some(delay_until_ms(when_ms, unix_ms_now()))
}

/// The wait from `now_ms` until `when_ms`, both Unix milliseconds, or
/// `Duration::ZERO` when `when_ms` is not after `now_ms`. Stays in integers
/// because the platform `SystemTime` range is narrower than the HTTP-date
/// range on some targets (Windows ends near year 30828).
fn delay_until_ms(when_ms: i64, now_ms: i64) -> Duration {
    u64::try_from(when_ms.saturating_sub(now_ms)).map_or(Duration::ZERO, Duration::from_millis)
}

/// The current wall clock as Unix milliseconds, negative before the epoch.
fn unix_ms_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

/// The instant an RFC 2822 date names, as Unix milliseconds.
///
/// The grammar is RFC 2822's date-time, relaxed the way existing upstreams'
/// headers were read before: any run of whitespace stands for FWS, a trailing
/// parenthesized comment is skipped, a weekday that does not match the date is
/// refused, a two-digit year below 50 is 20xx and other two- or three-digit
/// years add 1900, a 60th second is the following second, and the zone is a
/// `+hhmm` / `-hhmm` offset, `UT`, `GMT`, `Z`, a US zone name or a military
/// letter (read as UTC).
fn http_date_unix_ms(raw: &str) -> Option<i64> {
    let mut rest = raw.trim_start();
    let weekday = match short_weekday(rest) {
        Some((tail, day)) => {
            rest = tail.strip_prefix(',')?.trim_start();
            Some(day)
        }
        None => None,
    };
    let (tail, day) = digits(rest, 1, 2)?;
    let tail = mandatory_space(tail)?;
    let (tail, month) = short_month(tail)?;
    let tail = mandatory_space(tail)?;
    let (tail, year) = rfc2822_year(tail)?;
    let tail = mandatory_space(tail)?;
    let (tail, hour) = digits(tail, 2, 2)?;
    let tail = tail.trim_start().strip_prefix(':')?.trim_start();
    let (tail, minute) = digits(tail, 2, 2)?;
    let (tail, second) = match tail.trim_start().strip_prefix(':') {
        Some(after) => digits(after, 2, 2)?,
        None => (tail, 0),
    };
    let tail = mandatory_space(tail)?;
    let (mut tail, offset_s) = rfc2822_zone(tail)?;
    while let Some(after) = comment(tail) {
        tail = after;
    }
    if !tail.is_empty() {
        return None;
    }

    if year > MAX_YEAR || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if weekday.is_some_and(|named| named != weekday_of(days)) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - i64::from(offset_s);
    (seconds <= LAST_SECOND).then(|| seconds * 1_000)
}

/// The last calendar year (and, in UTC, the last second) an HTTP-date may
/// name, kept from the reader this one replaced.
const MAX_YEAR: i64 = 262_142;
const LAST_SECOND: i64 = 8_210_266_876_799;

const fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to the proleptic Gregorian date (`year >= 0`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Day of the week of a day count from 1970-01-01, a Thursday.
const fn weekday_of(days: i64) -> Weekday {
    const FROM_THURSDAY: [Weekday; 7] = [
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
    ];
    FROM_THURSDAY[days.rem_euclid(7) as usize]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

/// One or more whitespace characters, then the rest.
fn mandatory_space(s: &str) -> Option<&str> {
    let rest = s.trim_start();
    (rest.len() < s.len()).then_some(rest)
}

/// `min..=max` ASCII digits as a number, then the rest.
fn digits(s: &str, min: usize, max: usize) -> Option<(&str, i64)> {
    let len = s.bytes().take(max).take_while(u8::is_ascii_digit).count();
    if len < min {
        return None;
    }
    let value = s[..len].parse().ok()?;
    Some((&s[len..], value))
}

fn rfc2822_year(s: &str) -> Option<(&str, i64)> {
    let len = s.bytes().take_while(u8::is_ascii_digit).count();
    let (rest, year) = digits(s, 2, len.max(2))?;
    let year = match (len, year) {
        (2, 0..=49) => year + 2000,
        (2 | 3, _) => year + 1900,
        _ => year,
    };
    Some((rest, year))
}

/// A three-letter English name, any case, then the rest.
fn three_letters(s: &str) -> Option<(&str, [u8; 3])> {
    let head = s.get(..3)?;
    let mut lower = [0u8; 3];
    for (out, byte) in lower.iter_mut().zip(head.bytes()) {
        *out = byte.to_ascii_lowercase();
    }
    Some((&s[3..], lower))
}

fn short_weekday(s: &str) -> Option<(&str, Weekday)> {
    let (rest, name) = three_letters(s)?;
    let day = match &name {
        b"mon" => Weekday::Mon,
        b"tue" => Weekday::Tue,
        b"wed" => Weekday::Wed,
        b"thu" => Weekday::Thu,
        b"fri" => Weekday::Fri,
        b"sat" => Weekday::Sat,
        b"sun" => Weekday::Sun,
        _ => return None,
    };
    Some((rest, day))
}

fn short_month(s: &str) -> Option<(&str, i64)> {
    const MONTHS: [&[u8; 3]; 12] = [
        b"jan", b"feb", b"mar", b"apr", b"may", b"jun", b"jul", b"aug", b"sep", b"oct", b"nov",
        b"dec",
    ];
    let (rest, name) = three_letters(s)?;
    let index = MONTHS.iter().position(|month| **month == name)?;
    Some((rest, i64::try_from(index).ok()? + 1))
}

/// The zone as seconds east of UTC, then the rest.
fn rfc2822_zone(s: &str) -> Option<(&str, i32)> {
    let letters = s.bytes().take_while(u8::is_ascii_alphabetic).count();
    if letters == 0 {
        let (sign, rest) = match s.as_bytes().first()? {
            b'+' => (1, &s[1..]),
            b'-' => (-1, &s[1..]),
            _ => return None,
        };
        let bytes = rest.as_bytes();
        let hh = bytes
            .get(..2)
            .filter(|pair| pair.iter().all(u8::is_ascii_digit))?;
        let mm = bytes
            .get(2..4)
            .filter(|pair| pair.iter().all(u8::is_ascii_digit))?;
        let value = |pair: &[u8]| i32::from(pair[0] - b'0') * 10 + i32::from(pair[1] - b'0');
        let (hours, minutes) = (value(hh), value(mm));
        if minutes > 59 || hours > 23 {
            return None;
        }
        return Some((&rest[4..], sign * (hours * 3_600 + minutes * 60)));
    }
    let (name, rest) = s.split_at(letters);
    let hours = match name.to_ascii_lowercase().as_str() {
        "gmt" | "ut" | "z" => 0,
        "edt" => -4,
        "est" | "cdt" => -5,
        "cst" | "mdt" => -6,
        "mst" | "pdt" => -7,
        "pst" => -8,
        one if one.len() == 1 && one != "j" => 0,
        _ => return None,
    };
    Some((rest, hours * 3_600))
}

/// A parenthesized comment (nested and escaped parentheses allowed) after
/// optional whitespace, then the rest.
fn comment(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let body = s.strip_prefix('(')?;
    let mut depth = 1usize;
    let mut escaped = false;
    for (index, byte) in body.bytes().enumerate() {
        match (escaped, byte) {
            (true, _) => escaped = false,
            (false, b'\\') => escaped = true,
            (false, b'(') => depth += 1,
            (false, b')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&body[index + 1..]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        LAST_SECOND, delay_until_ms, http_date_unix_ms, is_rate_limit_status, parse_retry_after,
    };
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use std::time::Duration;

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_str(value).expect("header value"),
        );
        h
    }

    #[test]
    fn parses_retry_after_ms_to_sub_second_duration() {
        // Arrange: a millisecond-granular hint below one second.
        let mut headers = HeaderMap::new();
        headers.insert("retry-after-ms", HeaderValue::from_static("250"));

        // Act
        let got = parse_retry_after(&headers);

        // Assert: preserved as a sub-second Duration, not rounded up.
        assert_eq!(got, Some(Duration::from_millis(250)));
    }

    #[test]
    fn prefers_retry_after_ms_over_integer_seconds() {
        // Arrange: both headers present; the -ms value is finer than the
        // second-granular Retry-After (1500ms vs 2s).
        let mut headers = headers_with("2");
        headers.insert("retry-after-ms", HeaderValue::from_static("1500"));

        // Act
        let got = parse_retry_after(&headers);

        // Assert: the finer -ms value wins.
        assert_eq!(got, Some(Duration::from_millis(1500)));
    }

    #[test]
    fn falls_back_to_retry_after_when_ms_is_garbage() {
        // Arrange: an unparseable -ms value alongside a valid Retry-After.
        let mut headers = headers_with("30");
        headers.insert("retry-after-ms", HeaderValue::from_static("not-a-number"));

        // Act
        let got = parse_retry_after(&headers);

        // Assert: malformed -ms falls through to the standard header.
        assert_eq!(got, Some(Duration::from_secs(30)));
    }

    #[test]
    fn parses_integer_delta_seconds() {
        // Arrange
        let headers = headers_with("120");

        // Act
        let got = parse_retry_after(&headers);

        // Assert
        assert_eq!(got, Some(Duration::from_mins(2)));
    }

    #[test]
    fn parses_http_date_form() {
        // Arrange: a date safely in the future.
        let headers = headers_with("Wed, 21 Oct 2099 07:28:00 GMT");

        // Act
        let got = parse_retry_after(&headers).expect("future date must parse");

        // Assert: a positive, non-zero delay.
        assert!(
            got > Duration::ZERO,
            "future HTTP-date must yield a positive delay; got {got:?}"
        );
    }

    #[test]
    fn returns_none_when_absent() {
        // Arrange
        let headers = HeaderMap::new();

        // Act + Assert
        assert!(parse_retry_after(&headers).is_none());
    }

    #[test]
    fn returns_none_on_garbage() {
        // Arrange
        let headers = headers_with("not-a-date-or-int");

        // Act + Assert
        assert!(parse_retry_after(&headers).is_none());
    }

    #[test]
    fn clamps_past_http_date_to_zero() {
        // Arrange: a date well in the past.
        let headers = headers_with("Wed, 21 Oct 2015 07:28:00 GMT");

        // Act
        let got = parse_retry_after(&headers);

        // Assert: a past date clamps to ZERO rather than returning None.
        assert_eq!(got, Some(Duration::ZERO));
    }

    #[test]
    fn far_future_http_date_yields_large_delay() {
        // Arrange: dates past the narrowest platform SystemTime range,
        // up to the last one the reader accepts.
        let rows = [
            ("year 99999", "31 Dec 99999 23:59:59 GMT"),
            ("max accepted", "31 Dec 262142 23:59:59 GMT"),
        ];
        for (name, raw) in rows {
            // Act
            let got = parse_retry_after(&headers_with(raw));

            // Assert: well over 9000 years (in seconds) of wait.
            let got = got.unwrap_or_else(|| panic!("{name}: {raw:?} must parse"));
            assert!(
                got > Duration::from_hours(9_000 * 365 * 24),
                "{name}: got {got:?}"
            );
        }
    }

    #[test]
    fn delay_until_ms_subtracts_in_integers_across_the_accepted_range() {
        let now_ms = 1_445_412_480_000;
        let rows: &[(&str, i64, i64, Duration)] = &[
            (
                "future",
                now_ms + 1_500,
                now_ms,
                Duration::from_millis(1_500),
            ),
            ("equal", now_ms, now_ms, Duration::ZERO),
            ("past", now_ms - 1, now_ms, Duration::ZERO),
            (
                "pre-epoch date",
                -62_135_596_800_000,
                now_ms,
                Duration::ZERO,
            ),
            (
                "max accepted date",
                LAST_SECOND * 1_000,
                now_ms,
                Duration::from_millis(u64::try_from(LAST_SECOND * 1_000 - now_ms).unwrap()),
            ),
            (
                "clock before epoch",
                LAST_SECOND * 1_000,
                -1_000,
                Duration::from_millis(u64::try_from(LAST_SECOND * 1_000 + 1_000).unwrap()),
            ),
            (
                "extreme clock",
                i64::MAX,
                i64::MIN,
                Duration::from_millis(i64::MAX as u64),
            ),
        ];
        for (name, when_ms, now_ms, expected) in rows {
            assert_eq!(delay_until_ms(*when_ms, *now_ms), *expected, "{name}");
        }
    }

    #[test]
    fn flags_only_rate_limit_statuses() {
        // Arrange + Act + Assert: only the reset-meaningful statuses
        // flag true; everything else (including other 4xx/5xx) is false.
        for status in [429, 503, 529] {
            assert!(
                is_rate_limit_status(status),
                "status {status} must flag as rate-limit"
            );
        }
        for status in [200, 400, 401, 403, 404, 408, 429 + 1, 500, 502, 504] {
            assert!(
                !is_rate_limit_status(status),
                "status {status} must NOT flag as rate-limit"
            );
        }
    }
    /// The HTTP-date reader's accept set and the instant each accepted value
    /// names, pinned per input. Rows were recorded from the RFC 2822 reader
    /// this module used before; a `None` row is an input that reader
    /// refused.
    #[test]
    fn http_date_reader_accepts_and_resolves_each_pinned_input() {
        let rows: &[(&str, &str, Option<i64>)] = &[
            (
                "imf-fixdate",
                "Wed, 21 Oct 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "lowercase gmt",
                "Wed, 21 Oct 2015 07:28:00 gmt",
                Some(1_445_412_480_000),
            ),
            (
                "ut zone",
                "Wed, 21 Oct 2015 07:28:00 UT",
                Some(1_445_412_480_000),
            ),
            (
                "z zone",
                "Wed, 21 Oct 2015 07:28:00 Z",
                Some(1_445_412_480_000),
            ),
            (
                "military letter zone",
                "Wed, 21 Oct 2015 07:28:00 A",
                Some(1_445_412_480_000),
            ),
            (
                "plus zero offset",
                "Wed, 21 Oct 2015 07:28:00 +0000",
                Some(1_445_412_480_000),
            ),
            (
                "minus zero offset",
                "Wed, 21 Oct 2015 07:28:00 -0000",
                Some(1_445_412_480_000),
            ),
            (
                "negative offset",
                "Wed, 21 Oct 2015 07:28:00 -0700",
                Some(1_445_437_680_000),
            ),
            (
                "half-hour offset",
                "Wed, 21 Oct 2015 07:28:00 +0530",
                Some(1_445_392_680_000),
            ),
            (
                "est zone",
                "Wed, 21 Oct 2015 07:28:00 EST",
                Some(1_445_430_480_000),
            ),
            (
                "pdt zone",
                "Wed, 21 Oct 2015 07:28:00 PDT",
                Some(1_445_437_680_000),
            ),
            (
                "no weekday",
                "21 Oct 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "no seconds",
                "Wed, 21 Oct 2015 07:28 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "lowercase names",
                "wed, 21 oct 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "uppercase names",
                "WED, 21 OCT 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "two-digit year",
                "Wed, 21 Oct 15 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "no space after comma",
                "Wed,21 Oct 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "leading space",
                " Wed, 21 Oct 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "double space",
                "Wed, 21  Oct 2015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "tab before zone",
                "Wed, 21 Oct 2015 07:28:00\tGMT",
                Some(1_445_412_480_000),
            ),
            (
                "trailing comment",
                "Wed, 21 Oct 2015 07:28:00 GMT (comment)",
                Some(1_445_412_480_000),
            ),
            (
                "one-digit day",
                "Thu, 1 Oct 2015 07:28:00 GMT",
                Some(1_443_684_480_000),
            ),
            (
                "leap second",
                "Wed, 21 Oct 2015 07:28:60 GMT",
                Some(1_445_412_540_000),
            ),
            (
                "last representable",
                "Fri, 31 Dec 9999 23:59:59 GMT",
                Some(253_402_300_799_000),
            ),
            (
                "two-digit year below 50",
                "21 Oct 49 07:28:00 GMT",
                Some(2_518_414_080_000),
            ),
            (
                "two-digit year 50 and up",
                "21 Oct 50 07:28:00 GMT",
                Some(-605_809_920_000),
            ),
            (
                "three-digit year",
                "21 Oct 115 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "four-digit year below 1000",
                "21 Oct 0999 07:28:00 GMT",
                Some(-30_616_417_920_000),
            ),
            (
                "five-digit year",
                "21 Oct 02015 07:28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "space before colon",
                "Wed, 21 Oct 2015 07 :28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "space after colon",
                "Wed, 21 Oct 2015 07: 28:00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "space before seconds colon",
                "Wed, 21 Oct 2015 07:28 :00 GMT",
                Some(1_445_412_480_000),
            ),
            (
                "leap second at a day end",
                "Wed, 21 Oct 2015 23:59:60 GMT",
                Some(1_445_472_000_000),
            ),
            (
                "year past 9999",
                "21 Oct 12015 07:28:00 GMT",
                Some(317_014_932_480_000),
            ),
            (
                "last year, east of UTC",
                "31 Dec 262142 23:59:59 +0100",
                Some(8_210_266_873_199_000),
            ),
            (
                "last year, past the last UTC second",
                "31 Dec 262142 23:59:59 -0100",
                None,
            ),
            (
                "last UTC second",
                "31 Dec 262142 23:59:59 GMT",
                Some(8_210_266_876_799_000),
            ),
            ("year past the last", "21 Oct 262143 07:28:00 GMT", None),
            (
                "nested comment",
                "Wed, 21 Oct 2015 07:28:00 GMT (a (b) \\) c)",
                Some(1_445_412_480_000),
            ),
            (
                "two comments",
                "Wed, 21 Oct 2015 07:28:00 GMT (x) (y)",
                Some(1_445_412_480_000),
            ),
            (
                "unclosed comment",
                "Wed, 21 Oct 2015 07:28:00 GMT (open",
                None,
            ),
            (
                "space after the seconds colon",
                "Wed, 21 Oct 2015 07:28: 00 GMT",
                None,
            ),
            ("weekday mismatch", "Thu, 21 Oct 2015 07:28:00 GMT", None),
            ("trailing space", "Wed, 21 Oct 2015 07:28:00 GMT ", None),
            (
                "comment before zone",
                "Wed, 21 Oct 2015 07:28:00 (c) GMT",
                None,
            ),
            (
                "space around both colons",
                "Wed, 21 Oct 2015 07 : 28 : 00 GMT",
                None,
            ),
            ("offset hour 24", "Wed, 21 Oct 2015 07:28:00 +2400", None),
            ("utc spelled out", "Wed, 21 Oct 2015 07:28:00 UTC", None),
            ("letter j zone", "Wed, 21 Oct 2015 07:28:00 J", None),
            ("unknown zone", "Wed, 21 Oct 2015 07:28:00 XYZ", None),
            ("no zone", "Wed, 21 Oct 2015 07:28:00", None),
            ("colon offset", "Wed, 21 Oct 2015 07:28:00 +05:30", None),
            ("offset minutes 60", "Wed, 21 Oct 2015 07:28:00 +0060", None),
            ("rfc 850 form", "Wednesday, 21-Oct-15 07:28:00 GMT", None),
            ("asctime form", "Wed Oct 21 07:28:00 2015", None),
            ("pre-1900 year", "Wed, 21 Oct 1899 07:28:00 GMT", None),
            ("hour 24", "Wed, 21 Oct 2015 24:00:00 GMT", None),
            ("not a calendar day", "Wed, 31 Feb 2015 07:28:00 GMT", None),
            ("one-digit hour", "Wed, 21 Oct 2015 7:28:00 GMT", None),
            (
                "fractional seconds",
                "Wed, 21 Oct 2015 07:28:00.5 GMT",
                None,
            ),
            ("long month", "Wed, 21 October 2015 07:28:00 GMT", None),
            ("delta seconds", "120", None),
            ("empty", "", None),
            ("garbage", "not-a-date-or-int", None),
        ];
        for (name, raw, expected) in rows {
            assert_eq!(http_date_unix_ms(raw), *expected, "{name}: {raw:?}");
        }
    }
}
