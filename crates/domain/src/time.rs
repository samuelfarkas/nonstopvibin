//! Timestamps in the exact shape of JavaScript's `Date.prototype.toISOString()`. Stored rows
//! compare them as text, so the shape is part of the data format.

const DAY_MS: i64 = 86_400_000;

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

#[cfg(test)]
mod tests {
    use super::iso_string;

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
}
