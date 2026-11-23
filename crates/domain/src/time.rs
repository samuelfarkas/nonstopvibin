//! Time as JavaScript's `Date` writes and reads it. Timestamps have the exact shape of
//! `Date.prototype.toISOString()`: stored rows compare them as text, so the shape is part of
//! the data format.

use std::time::{SystemTime, UNIX_EPOCH};

const DAY_MS: i64 = 86_400_000;

/// `Date.prototype.getTime()` of `time`: milliseconds since the epoch, negative before it.
#[must_use]
pub fn epoch_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// `new Date(milliseconds).toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ`, with a signed six-digit
/// year outside 0000–9999.
#[must_use]
pub fn iso_string(milliseconds: i64) -> String {
    let days = milliseconds.div_euclid(DAY_MS);
    let time = milliseconds.rem_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(days);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+07}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600_000,
        time / 60_000 % 60,
        time / 1000 % 60,
        time % 1000,
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The most milliseconds from the epoch a JavaScript `Date` holds (`TimeClip`).
const MAX_TIME: f64 = 8.64e15;

/// `new Date(milliseconds).getTime()`: truncated, or `None` past `TimeClip`'s range.
#[must_use]
pub fn time_clip(milliseconds: f64) -> Option<i64> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is whole and within ±8.64e15"
    )]
    (milliseconds.abs() <= MAX_TIME).then(|| milliseconds.trunc() as i64)
}

/// `Date.parse(text)` for the ECMAScript date-time string format: `YYYY[-MM[-DD]]` or
/// `±YYYYYY…`, then `THH:mm[:ss[.fff…]]` and `Z` or `±HH:mm`. V8's other, implementation-defined
/// formats are not read. A date-time without an offset is read as UTC (V8: local time).
#[must_use]
pub fn parse_iso(text: &str) -> Option<i64> {
    let mut cursor = Cursor(text.as_bytes());
    let year = match cursor.peek() {
        Some(sign @ (b'+' | b'-')) => {
            cursor.skip();
            let digits = cursor.digits(6)?;
            if sign == b'-' && digits == 0 {
                return None;
            }
            if sign == b'-' { -digits } else { digits }
        }
        _ => cursor.digits(4)?,
    };
    let (month, day) = if cursor.eat(b'-') {
        let month = cursor.digits(2)?;
        (
            month,
            if cursor.eat(b'-') {
                cursor.digits(2)?
            } else {
                1
            },
        )
    } else {
        (1, 1)
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut milliseconds = 0;
    let mut offset = 0;
    if cursor.eat(b'T') {
        let hour = cursor.digits(2)?;
        if !cursor.eat(b':') {
            return None;
        }
        let minute = cursor.digits(2)?;
        let mut second = 0;
        let mut fraction = 0;
        if cursor.eat(b':') {
            second = cursor.digits(2)?;
            if cursor.eat(b'.') {
                fraction = cursor.fraction()?;
            }
        }
        let midnight = hour == 24 && minute == 0 && second == 0 && fraction == 0;
        if !(hour < 24 || midnight) || minute > 59 || second > 59 {
            return None;
        }
        milliseconds = ((hour * 60 + minute) * 60 + second) * 1000 + fraction;
        match cursor.peek() {
            Some(b'Z') => cursor.skip(),
            Some(sign @ (b'+' | b'-')) => {
                cursor.skip();
                let hours = cursor.digits(2)?;
                if !cursor.eat(b':') {
                    return None;
                }
                let minutes = cursor.digits(2)?;
                if hours > 23 || minutes > 59 {
                    return None;
                }
                offset = (hours * 60 + minutes) * 60_000;
                if sign == b'-' {
                    offset = -offset;
                }
            }
            _ => {}
        }
    }
    if !cursor.0.is_empty() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    #[expect(
        clippy::cast_precision_loss,
        reason = "years of six digits keep the time far below 2^53"
    )]
    time_clip((days * 86_400_000 + milliseconds - offset) as f64)
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.0.first().copied()
    }

    fn skip(&mut self) {
        self.0 = self.0.get(1..).unwrap_or_default();
    }

    fn eat(&mut self, byte: u8) -> bool {
        let matched = self.peek() == Some(byte);
        if matched {
            self.skip();
        }
        matched
    }

    /// Exactly `count` ASCII digits.
    fn digits(&mut self, count: usize) -> Option<i64> {
        let digits = self.0.get(..count)?;
        let mut value = 0;
        for &byte in digits {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        self.0 = self.0.get(count..).unwrap_or_default();
        Some(value)
    }

    /// One or more digits of a fraction, as milliseconds (the first three, truncated).
    fn fraction(&mut self) -> Option<i64> {
        let length = self.0.iter().take_while(|b| b.is_ascii_digit()).count();
        if length == 0 {
            return None;
        }
        let mut milliseconds = 0;
        for index in 0..3 {
            let digit = self
                .0
                .get(index)
                .filter(|_| index < length)
                .map_or(0, |b| b - b'0');
            milliseconds = milliseconds * 10 + i64::from(digit);
        }
        self.0 = self.0.get(length..).unwrap_or_default();
        Some(milliseconds)
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// zod 4's `z.iso.date()`: `YYYY-MM-DD` with a real day, leap years included.
#[must_use]
pub fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10 && parse_iso(text).is_some() && bytes.get(4) == Some(&b'-')
}

/// zod 4's `z.string().datetime({ offset: true })`: an ISO date, `THH:mm:ss`, an optional
/// fraction, then `Z` or `±HH:mm`.
#[must_use]
pub fn is_offset_datetime(text: &str) -> bool {
    let Some((date, time)) = text.split_once('T') else {
        return false;
    };
    let (clock, zone) = match time.find(['Z', '+', '-']) {
        Some(index) => time.split_at(index),
        None => return false,
    };
    let zone_ok = zone == "Z" || {
        let zone = zone.as_bytes();
        zone.len() == 6
            && zone.get(3) == Some(&b':')
            && two_digits(zone.get(1..3), 23)
            && two_digits(zone.get(4..6), 59)
    };
    let (seconds, fraction) = clock.split_at(clock.find('.').unwrap_or(clock.len()));
    let fraction_ok = fraction.is_empty()
        || (fraction.len() > 1 && fraction.bytes().skip(1).all(|b| b.is_ascii_digit()));
    let seconds = seconds.as_bytes();
    is_iso_date(date)
        && zone_ok
        && fraction_ok
        && seconds.len() == 8
        && seconds.get(2) == Some(&b':')
        && seconds.get(5) == Some(&b':')
        && two_digits(seconds.get(..2), 23)
        && two_digits(seconds.get(3..5), 59)
        && two_digits(seconds.get(6..8), 59)
}

fn two_digits(bytes: Option<&[u8]>, most: u8) -> bool {
    match bytes {
        Some(&[tens, ones]) if tens.is_ascii_digit() && ones.is_ascii_digit() => {
            (tens - b'0') * 10 + (ones - b'0') <= most
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_javascript_to_iso_string() {
        // Expected values from Bun's `new Date(ms).toISOString()`.
        assert_eq!(iso_string(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_string(1_767_603_600_000), "2026-01-05T09:00:00.000Z");
        assert_eq!(iso_string(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(iso_string(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            iso_string(253_402_300_800_000),
            "+010000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso_string(-62_198_755_200_000),
            "-000001-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso_string(-62_198_755_200_001),
            "-000002-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn iso_timestamps_parse_back_to_their_milliseconds() {
        for milliseconds in [0, 1_763_812_345_678, 951_782_400_000, 4_102_444_799_999, -1] {
            assert_eq!(parse_iso(&iso_string(milliseconds)), Some(milliseconds));
        }
    }

    #[test]
    fn dates_parse_the_ecmascript_format() {
        let iso = |text: &str| parse_iso(text).map(iso_string);
        assert_eq!(
            iso("2026-11-27T00:00:00+02:00").as_deref(),
            Some("2026-11-26T22:00:00.000Z")
        );
        assert_eq!(
            iso("2026-12-01").as_deref(),
            Some("2026-12-01T00:00:00.000Z")
        );
        assert_eq!(iso("2026").as_deref(), Some("2026-01-01T00:00:00.000Z"));
        assert_eq!(
            iso("2026-11-22T14:00:00.1239Z").as_deref(),
            Some("2026-11-22T14:00:00.123Z")
        );
        assert_eq!(
            iso("+275760-09-13T00:00:00Z").as_deref(),
            Some("+275760-09-13T00:00:00.000Z")
        );
        for invalid in [
            "not a date",
            "bad",
            "2026-02-30",
            "2026-11-22T25:00Z",
            "+275760-09-13T00:00:00.001Z",
            "-000000-01-01",
            "2026-11-22 14:00",
        ] {
            assert_eq!(parse_iso(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn zod_date_formats() {
        assert!(is_iso_date("2024-02-29"));
        assert!(!is_iso_date("2026-02-29"));
        assert!(!is_iso_date("2026-1-01"));
        assert!(is_offset_datetime("2026-12-01T00:00:00+00:00"));
        assert!(is_offset_datetime("2026-12-01T00:00:00.5Z"));
        assert!(!is_offset_datetime("2026-12-01T00:00:00"));
        assert!(!is_offset_datetime("2026-12-01T00:00Z"));
        assert!(!is_offset_datetime("invalid"));
    }
}
