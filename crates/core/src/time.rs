//! Server times (seconds since 1970, UTC) as text, and RFC 3339 text back to server times.
//! ARD writes `updatedAt` and reads `createdAfter` in this form.

/// The civil date of a day count since 1970-01-01, by Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// The day count since 1970-01-01 of a civil date, by Howard Hinnant's algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The date and time of `at` in UTC: year, month, day, hour, minute, second.
pub fn utc_parts(at: i64) -> (i64, i64, i64, i64, i64, i64) {
    let (year, month, day) = civil_from_days(at.div_euclid(86_400));
    let seconds = at.rem_euclid(86_400);
    (year, month, day, seconds / 3_600, seconds % 3_600 / 60, seconds % 60)
}

/// `at` as RFC 3339 text in UTC, for example `2026-11-20T08:15:00Z`.
pub fn utc_text(at: i64) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(at);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Reads RFC 3339 text: `YYYY-MM-DDTHH:MM:SS`, optional fraction digits, then `Z` or `±HH:MM`.
/// `t` and `z` may be lower case. Drops the fraction. `None` for anything else.
pub fn parse_utc(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = bytes.get(range)?;
        if part.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(part).ok()?.parse().ok()
        } else {
            None
        }
    };
    let at = |index: usize, expected: &[u8]| bytes.get(index).is_some_and(|b| expected.contains(b));
    if !(at(4, b"-") && at(7, b"-") && at(10, b"Tt") && at(13, b":") && at(16, b":")) {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    // RFC 3339 allows second 60 for a leap second. It counts as the next second.
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = 19;
    if at(rest, b".") {
        rest += 1;
        let start = rest;
        while bytes.get(rest).is_some_and(u8::is_ascii_digit) {
            rest += 1;
        }
        if rest == start {
            return None;
        }
    }
    let offset = match bytes.get(rest)? {
        b'Z' | b'z' if rest + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if rest + 6 == bytes.len() && at(rest + 3, b":") => {
            let (hours, minutes) = (number(rest + 1..rest + 3)?, number(rest + 4..rest + 6)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3_600 + minutes * 60;
            if *sign == b'+' { offset } else { -offset }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_time_is_written_as_rfc_3339_utc() {
        assert_eq!(utc_text(0), "1970-01-01T00:00:00Z");
        // A leap day, and the last second of a year.
        assert_eq!(utc_text(1_709_210_096), "2024-02-29T12:34:56Z");
        assert_eq!(utc_text(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(utc_text(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn written_times_read_back() {
        for at in [0, -1, 951_782_400, 1_709_210_096, 1_767_225_599, 4_102_444_800] {
            assert_eq!(parse_utc(&utc_text(at)), Some(at), "{at}");
        }
    }

    #[test]
    fn offsets_and_fractions_are_read() {
        assert_eq!(parse_utc("2024-02-29T21:34:56+09:00"), Some(1_709_210_096));
        assert_eq!(parse_utc("2024-02-29T07:04:56-05:30"), Some(1_709_210_096));
        assert_eq!(parse_utc("2024-02-29t12:34:56.789z"), Some(1_709_210_096));
    }

    #[test]
    fn text_that_is_not_rfc_3339_is_refused() {
        for text in [
            "",
            "2024-02-29",
            "2024-02-29 12:34:56Z",
            "2024-02-29T12:34:56",
            "2023-02-29T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2024-02-29T24:00:00Z",
            "2024-02-29T12:34:56.Z",
            "2024-02-29T12:34:56+0900",
            "2024-02-29T12:34:56Zjunk",
            "２０２４-02-29T12:34:56Z",
        ] {
            assert_eq!(parse_utc(text), None, "{text}");
        }
    }
}
