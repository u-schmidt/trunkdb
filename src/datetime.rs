//! `DateTime`, what `Document::DateTime` holds (SPEC §69, §70): an
//! instant in UTC as whole seconds since 1970-01-01T00:00:00Z (negative
//! before) and nanoseconds past that second. The same on every platform,
//! unlike `SystemTime`, which it converts from and to. And its text, RFC
//! 3339 in UTC, for the export, `Display` and reading it as a string.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NANOS_PER_SECOND: u32 = 1_000_000_000;

/// A point in time, in UTC, to the nanosecond (SPEC §69, §70): what a
/// `SystemTime` field is stored as, and what a filter compares it with.
/// Any time an `i64` of seconds reaches, 292 billion years either side of
/// 1970, on every platform: a file written on one reads the same on
/// another.
///
/// ```
/// use std::time::SystemTime;
/// use trunkdb::DateTime;
///
/// let now = DateTime::now();
/// let text = now.to_string();                  // "2026-09-27T14:05:00.123456789Z"
/// assert_eq!(text.parse::<DateTime>().unwrap(), now);
/// assert_eq!(DateTime::from(SystemTime::UNIX_EPOCH), DateTime::UNIX_EPOCH);
/// ```
///
/// A struct can hold a `SystemTime` or a `DateTime`; both are stored as
/// date-times. A `DateTime` also holds what serde's `SystemTime` refuses,
/// times before 1970, and what a platform's `SystemTime` can't, like
/// Windows' times before 1601 or finer than 100 ns.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DateTime {
    // In this order: the derived `Ord` is time order, as `nanos` is
    // always below a second.
    secs: i64,
    nanos: u32,
}

impl DateTime {
    /// 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: DateTime = DateTime { secs: 0, nanos: 0 };

    /// The current time, from `SystemTime::now()`.
    pub fn now() -> Self {
        SystemTime::now().into()
    }

    /// The time `secs` seconds after 1970-01-01T00:00:00Z (before, if
    /// negative) and `nanos` nanoseconds after that. `None` if `nanos` is
    /// a second or more.
    pub fn from_unix(secs: i64, nanos: u32) -> Option<Self> {
        (nanos < NANOS_PER_SECOND).then_some(DateTime { secs, nanos })
    }

    /// Whole seconds since 1970-01-01T00:00:00Z, rounded down: negative
    /// before it.
    pub fn unix_seconds(&self) -> i64 {
        self.secs
    }

    /// Nanoseconds after `unix_seconds`, below a second.
    pub fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// This time as a `SystemTime`, if this platform's holds it: not a
    /// time before 1601 on Windows, and there rounded down to 100 ns.
    pub fn to_system_time(&self) -> Option<SystemTime> {
        if self.secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::new(self.secs as u64, self.nanos))
        } else {
            UNIX_EPOCH
                .checked_sub(Duration::new(self.secs.unsigned_abs(), 0))?
                .checked_add(Duration::new(0, self.nanos))
        }
    }
}

/// Every `SystemTime` a platform has is within an `i64` of seconds.
impl From<SystemTime> for DateTime {
    fn from(time: SystemTime) -> Self {
        let far = "no SystemTime is 292 billion years from 1970";
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => DateTime {
                secs: i64::try_from(after.as_secs()).expect(far),
                nanos: after.subsec_nanos(),
            },
            Err(before) => {
                let before = before.duration();
                let secs = i64::try_from(before.as_secs()).expect(far);
                match before.subsec_nanos() {
                    0 => DateTime {
                        secs: -secs,
                        nanos: 0,
                    },
                    nanos => DateTime {
                        secs: -secs - 1,
                        nanos: NANOS_PER_SECOND - nanos,
                    },
                }
            }
        }
    }
}

/// RFC 3339, in UTC: `2026-09-27T14:05:00Z`, with as many fractional
/// digits as the nanoseconds need, up to nine. A year before 0 or after
/// 9999 gets a sign and as many digits as it has (ISO 8601's expanded
/// years), so every `DateTime` has a text.
impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (days, second_of_day) = (self.secs.div_euclid(86_400), self.secs.rem_euclid(86_400));
        let (year, month, day) = civil_from_days(days);
        if (0..=9999).contains(&year) {
            write!(f, "{year:04}")?;
        } else {
            write!(f, "{year:+05}")?;
        }
        let (hour, minute, second) = (
            second_of_day / 3600,
            second_of_day / 60 % 60,
            second_of_day % 60,
        );
        write!(f, "-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")?;
        if self.nanos != 0 {
            let fraction = format!("{:09}", self.nanos);
            write!(f, ".{}", fraction.trim_end_matches('0'))?;
        }
        f.write_str("Z")
    }
}

/// The text, as `Display` writes it.
impl fmt::Debug for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DateTime({self})")
    }
}

/// RFC 3339: what `Display` writes, and as others write it too, with an
/// offset (`+02:00`) instead of `Z`, a lower-case `t` or `z`, or a space
/// for the `T`. The fraction may have one to nine digits.
impl std::str::FromStr for DateTime {
    type Err = ParseDateTimeError;

    fn from_str(text: &str) -> Result<Self, ParseDateTimeError> {
        parse_rfc3339(text).ok_or_else(|| ParseDateTimeError {
            input: text.to_string(),
        })
    }
}

/// A string that isn't an RFC 3339 date and time: what
/// `str::parse::<DateTime>` returns (SPEC §70).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDateTimeError {
    input: String,
}

impl fmt::Display for ParseDateTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not an RFC 3339 date and time (like \"2026-09-27T14:05:00Z\")",
            self.input
        )
    }
}

impl std::error::Error for ParseDateTimeError {}

/// The name `DateTime`'s serde impls give their newtype (SPEC §70), so the
/// serde bridge stores it as `Document::DateTime`, as it does `DocId`
/// (§59). Any other format sees a newtype around the RFC 3339 text.
pub(crate) const DATETIME_NEWTYPE: &str = "$trunkdb::DateTime";

impl serde::Serialize for DateTime {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_newtype_struct(DATETIME_NEWTYPE, &self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for DateTime {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextVisitor;

        impl<'de> serde::de::Visitor<'de> for TextVisitor {
            type Value = DateTime;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a date and time, as RFC 3339 text")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<DateTime, E> {
                s.parse().map_err(E::custom)
            }

            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<DateTime, D::Error> {
                deserializer.deserialize_str(self)
            }
        }

        deserializer.deserialize_newtype_struct(DATETIME_NEWTYPE, TextVisitor)
    }
}

/// `Display` backwards (see `FromStr`). `None` for anything else.
fn parse_rfc3339(text: &str) -> Option<DateTime> {
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
    // In `i128`: at the start of the range, the whole days before it are
    // already more seconds than an `i64` holds, and the time of day brings
    // it back.
    let days = days_from_civil(year, month, day);
    let secs = i128::from(days) * 86_400 + i128::from(hour * 3600 + minute * 60 + second)
        - i128::from(offset_secs);
    DateTime::from_unix(i64::try_from(secs).ok()?, nanos)
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
    // Four digits without a sign, four to twelve with one: 292 billion
    // years is as far as an `i64` of seconds goes, and the arithmetic
    // below checks for going past it.
    if year.len() < 4 || (sign == 0 && year.len() != 4) || year.len() > 12 {
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

    fn at(secs: i64, nanos: u32) -> DateTime {
        DateTime::from_unix(secs, nanos).unwrap()
    }

    #[test]
    fn system_times_convert_both_ways() {
        // Times every platform's `SystemTime` holds: after 1601, in whole
        // 100 ns.
        for (secs, nanos) in [
            (0, 0),
            (1, 100),
            (1_790_000_000, 123_456_700),
            (-1, 0),
            (-1, 999_999_900),
            (-86_400 * 365 * 300, 500),
            (253_402_300_799, 999_999_900),
        ] {
            let time = at(secs, nanos);
            let system = time.to_system_time().unwrap();
            assert_eq!(DateTime::from(system), time, "{time}");
        }
        // Half a second before 1970: the second before, plus half.
        let before = UNIX_EPOCH - Duration::from_millis(500);
        assert_eq!(DateTime::from(before), at(-1, 500_000_000));
        assert_eq!(DateTime::from(UNIX_EPOCH), DateTime::UNIX_EPOCH);
        assert_eq!(DateTime::from_unix(0, NANOS_PER_SECOND), None);
    }

    #[test]
    fn datetimes_order_in_time() {
        let times = [
            at(i64::MIN, 0),
            at(-1, 0),
            at(-1, 999_999_999),
            at(0, 0),
            at(0, 1),
            at(i64::MAX, 999_999_999),
        ];
        assert!(times.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(at(7, 5).unix_seconds(), 7);
        assert_eq!(at(7, 5).subsec_nanos(), 5);
    }

    #[test]
    fn rfc3339_is_written_in_utc_with_the_digits_needed() {
        let text = |secs, nanos| at(secs, nanos).to_string();
        assert_eq!(text(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(text(1_790_000_000, 0), "2026-09-21T14:13:20Z");
        assert_eq!(text(1_790_000_000, 120_000_000), "2026-09-21T14:13:20.12Z");
        assert_eq!(text(1_790_000_000, 1), "2026-09-21T14:13:20.000000001Z");
        assert_eq!(text(-1, 500_000_000), "1969-12-31T23:59:59.5Z");
        assert_eq!(text(951_782_400, 0), "2000-02-29T00:00:00Z");
        assert_eq!(text(-62_135_596_800, 0), "0001-01-01T00:00:00Z");
        assert_eq!(text(253_402_300_800, 0), "+10000-01-01T00:00:00Z");
        assert_eq!(text(-62_198_755_200, 0), "-0001-01-01T00:00:00Z");
        assert_eq!(format!("{:?}", at(0, 0)), "DateTime(1970-01-01T00:00:00Z)");
    }

    /// Every third day or so for eight centuries, some odd times, and the
    /// ends of the range: what's written reads back.
    #[test]
    fn rfc3339_reads_back_what_it_wrote() {
        let mut secs = -62_135_596_800; // 0001-01-01
        while secs < 32_503_680_000 {
            // 3000-01-01
            let time = at(secs + 37_199, (secs.rem_euclid(1000) as u32) * 1_000_003);
            assert_eq!(parse_rfc3339(&time.to_string()), Some(time), "{time}");
            secs += 86_400 * 3 + 7;
        }
        for extreme in [
            at(253_402_300_800, 0),
            at(-62_198_755_200, 7),
            at(1 << 40, 1),
            at(i64::MAX, 999_999_999),
            at(i64::MIN, 0),
        ] {
            assert_eq!(
                parse_rfc3339(&extreme.to_string()),
                Some(extreme),
                "{extreme}"
            );
        }
        let past_the_end = format!("{}", at(i64::MAX, 0)).replace("-12-04T", "-12-05T");
        assert_eq!(parse_rfc3339(&past_the_end), None, "{past_the_end}");
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
            assert_eq!(text.parse::<DateTime>(), Ok(utc), "{text}");
        }
        assert_eq!(
            parse_rfc3339("2026-09-21T14:13:20.5Z"),
            Some(at(1_790_000_000, 500_000_000))
        );
        for bad in [
            "",
            "2026-09-21",
            "2026-09-21T14:13:20",
            "2026-09-21T14:13Z",
            "2026-9-21T14:13:20Z",
            "26-09-21T14:13:20Z",
            "2026-02-30T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-09-21T24:00:00Z",
            "2026-09-21T14:60:00Z",
            "2026-09-21T14:13:20.Z",
            "2026-09-21T14:13:20.1234567890Z",
            "2026-09-21T14:13:20+2:00",
            "2026-09-21T14:13:20+02",
            "12026-09-21T14:13:20Z",
            "+1000000000000-01-01T00:00:00Z",
            "2026-09-21T14:13:20Zjunk",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
        assert_eq!(
            parse_rfc3339("2024-02-29T00:00:00Z"),
            Some(at(1_709_164_800, 0))
        );
        assert_eq!(
            parse_rfc3339("2000-02-29T00:00:00Z"),
            Some(at(951_782_400, 0))
        );
        let error = "soon".parse::<DateTime>().unwrap_err().to_string();
        assert!(error.starts_with("\"soon\" is not an RFC 3339"), "{error}");
    }
}
