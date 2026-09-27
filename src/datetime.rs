//! A point in time as `Document::DateTime` holds it (SPEC §69): a
//! `SystemTime`, an instant in UTC, stored as whole seconds since
//! 1970-01-01T00:00:00Z (negative before) and nanoseconds past that
//! second, so a `SystemTime` reads back exactly as it was written. And its
//! text, RFC 3339 in UTC, for the export and for reading it as a string.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NANOS_PER_SECOND: u32 = 1_000_000_000;

/// `time` as seconds since the epoch, rounded down, and the nanoseconds
/// after that second (`0..1_000_000_000`, also before the epoch). `None`
/// for a time too far from 1970 for an `i64` of seconds, 292 billion
/// years, which no platform's `SystemTime` reaches.
pub(crate) fn to_parts(time: SystemTime) -> Option<(i64, u32)> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Some((i64::try_from(after.as_secs()).ok()?, after.subsec_nanos())),
        Err(before) => {
            let before = before.duration();
            let secs = i64::try_from(before.as_secs()).ok()?;
            match before.subsec_nanos() {
                0 => Some((-secs, 0)),
                nanos => Some((-secs - 1, NANOS_PER_SECOND - nanos)),
            }
        }
    }
}

/// `to_parts` backwards. `None` if `nanos` is a second or more, or the
/// time is beyond what this platform's `SystemTime` holds.
pub(crate) fn from_parts(secs: i64, nanos: u32) -> Option<SystemTime> {
    if nanos >= NANOS_PER_SECOND {
        return None;
    }
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(secs as u64, nanos))
    } else {
        let before = Duration::new(secs.unsigned_abs(), 0);
        UNIX_EPOCH
            .checked_sub(before)?
            .checked_add(Duration::new(0, nanos))
    }
}

/// `time` in RFC 3339, in UTC: `2026-09-27T14:05:00Z`, with as many
/// fractional digits as the nanoseconds need, up to nine. A year before
/// 0 or after 9999 gets a sign and as many digits as it has (ISO 8601's
/// expanded years), so every `SystemTime` has a text.
pub(crate) fn to_rfc3339(time: SystemTime) -> String {
    let (secs, nanos) = to_parts(time).expect("no SystemTime is that far from 1970");
    let (days, second_of_day) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+05}")
    };
    let (hour, minute, second) = (
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    );
    let mut text = format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        text.push('.');
        text.push_str(fraction.trim_end_matches('0'));
    }
    text.push('Z');
    text
}

/// `to_rfc3339` backwards, and RFC 3339 as others write it: an offset
/// (`+02:00`) instead of `Z`, a lower-case `t` or `z`, a space for the
/// `T`. The fraction may have one to nine digits. `None` for anything
/// else, and for a time this platform's `SystemTime` can't hold.
pub(crate) fn from_rfc3339(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.split_at(text.find(['T', 't', ' '])?);
    let rest = &rest[1..];
    let (year, month, day) = parse_date(date)?;
    let (clock, offset) = rest.split_at(rest.find(['Z', 'z', '+', '-'])?);
    let (hms, fraction) = match clock.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (clock, None),
    };
    let [hour, minute, second] = two_digit_fields(hms, ':')?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let nanos = match fraction {
        None => 0,
        Some(f) if (1..=9).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{f:0<9}").parse().ok()?
        }
        Some(_) => return None,
    };
    let offset_secs = match offset {
        "Z" | "z" => 0,
        _ => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let [h, m] = two_digit_fields(&offset[1..], ':')?;
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?
        .checked_sub(offset_secs)?;
    from_parts(secs, nanos)
}

/// `YYYY-MM-DD`, or a signed year of more digits (`+12345-01-01`), a day
/// that month has.
fn parse_date(date: &str) -> Option<(i64, u32, u32)> {
    let (sign, unsigned) = match date.as_bytes().first()? {
        b'+' => (1, &date[1..]),
        b'-' => (-1, &date[1..]),
        _ => (0, date),
    };
    let mut fields = unsigned.split('-');
    let (year, month, day) = (fields.next()?, fields.next()?, fields.next()?);
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if fields.next().is_some() || !digits(year) || month.len() != 2 || day.len() != 2 {
        return None;
    }
    // Four digits without a sign, four or more (up to i64's) with one.
    if year.len() < 4 || (sign == 0 && year.len() != 4) || year.len() > 18 {
        return None;
    }
    let year = if sign < 0 {
        -year.parse::<i64>().ok()?
    } else {
        year.parse().ok()?
    };
    let (month, day): (u32, u32) = (month.parse().ok()?, day.parse().ok()?);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days_in_month)
        .contains(&day)
        .then_some((year, month, day))
}

/// `N` two-digit numbers separated by `separator`: `14:05:00`.
fn two_digit_fields<const N: usize>(text: &str, separator: char) -> Option<[i64; N]> {
    let mut out = [0; N];
    let mut parts = text.split(separator);
    for slot in &mut out {
        let part = parts.next()?;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// The day, counted from 1970-01-01, of a date in the proleptic
/// Gregorian calendar: Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `days_from_civil` backwards: Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64, nanos: u32) -> SystemTime {
        from_parts(secs, nanos).unwrap()
    }

    #[test]
    fn parts_round_trip_before_and_after_1970() {
        for (secs, nanos) in [
            (0, 0),
            (1, 1),
            (1_790_000_000, 123_456_789),
            (-1, 0),
            (-1, 999_999_999),
            (-86_400 * 365 * 400, 5),
            (253_402_300_799, 999_999_999),
        ] {
            assert_eq!(
                to_parts(at(secs, nanos)),
                Some((secs, nanos)),
                "{secs} {nanos}"
            );
        }
        // Half a second before 1970: the second before, plus half.
        let before = UNIX_EPOCH - Duration::from_millis(500);
        assert_eq!(to_parts(before), Some((-1, 500_000_000)));
        assert_eq!(from_parts(0, NANOS_PER_SECOND), None);
    }

    #[test]
    fn rfc3339_is_written_in_utc_with_the_digits_needed() {
        assert_eq!(to_rfc3339(at(0, 0)), "1970-01-01T00:00:00Z");
        assert_eq!(to_rfc3339(at(1_790_000_000, 0)), "2026-09-21T14:13:20Z");
        assert_eq!(
            to_rfc3339(at(1_790_000_000, 120_000_000)),
            "2026-09-21T14:13:20.12Z"
        );
        assert_eq!(
            to_rfc3339(at(1_790_000_000, 1)),
            "2026-09-21T14:13:20.000000001Z"
        );
        assert_eq!(to_rfc3339(at(-1, 500_000_000)), "1969-12-31T23:59:59.5Z");
        assert_eq!(to_rfc3339(at(951_782_400, 0)), "2000-02-29T00:00:00Z");
        assert_eq!(to_rfc3339(at(-62_135_596_800, 0)), "0001-01-01T00:00:00Z");
        assert_eq!(to_rfc3339(at(253_402_300_800, 0)), "+10000-01-01T00:00:00Z");
        assert_eq!(to_rfc3339(at(-62_198_755_200, 0)), "-0001-01-01T00:00:00Z");
    }

    /// Every day for eight centuries and some odd times: what's written
    /// reads back.
    #[test]
    fn rfc3339_reads_back_what_it_wrote() {
        let mut secs = -62_135_596_800; // 0001-01-01
        while secs < 32_503_680_000 {
            // 3000-01-01
            let time = at(secs + 37_199, (secs.rem_euclid(1000) as u32) * 1_000_003);
            assert_eq!(
                from_rfc3339(&to_rfc3339(time)),
                Some(time),
                "{}",
                to_rfc3339(time)
            );
            secs += 86_400 * 3 + 7;
        }
        for extreme in [
            at(253_402_300_800, 0),
            at(-62_198_755_200, 7),
            at(1 << 40, 1),
        ] {
            assert_eq!(from_rfc3339(&to_rfc3339(extreme)), Some(extreme));
        }
    }

    #[test]
    fn rfc3339_as_others_write_it() {
        let utc = at(1_790_000_000, 0);
        for text in [
            "2026-09-21T14:13:20Z",
            "2026-09-21t14:13:20z",
            "2026-09-21 14:13:20Z",
            "2026-09-21T16:13:20+02:00",
            "2026-09-21T08:43:20-05:30",
            "2026-09-21T14:13:20.000Z",
        ] {
            assert_eq!(from_rfc3339(text), Some(utc), "{text}");
        }
        assert_eq!(
            from_rfc3339("2026-09-21T14:13:20.5Z"),
            Some(at(1_790_000_000, 500_000_000))
        );
        for bad in [
            "",
            "2026-09-21",
            "2026-09-21T14:13:20",
            "2026-09-21T13:33Z",
            "2026-9-21T14:13:20Z",
            "26-09-21T14:13:20Z",
            "2026-02-30T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-09-21T24:00:00Z",
            "2026-09-21T13:60:00Z",
            "2026-09-21T14:13:20.Z",
            "2026-09-21T14:13:20.1234567890Z",
            "2026-09-21T14:13:20+2:00",
            "2026-09-21T14:13:20+02",
            "12026-09-21T14:13:20Z",
            "2026-09-21T14:13:20Zjunk",
        ] {
            assert_eq!(from_rfc3339(bad), None, "{bad}");
        }
        assert_eq!(
            from_rfc3339("2024-02-29T00:00:00Z"),
            Some(at(1_709_164_800, 0))
        );
        assert_eq!(
            from_rfc3339("2000-02-29T00:00:00Z"),
            Some(at(951_782_400, 0))
        );
    }
}
