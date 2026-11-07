//! `DateTime`, what `Document::DateTime` holds (SPEC §69, §70): an
//! instant in UTC as whole seconds since 1970-01-01T00:00:00Z (negative
//! before) and nanoseconds past that second. The same on every platform,
//! unlike `SystemTime`, which it converts from and to. And its text, RFC
//! 3339 in UTC, for the export, `Display` and reading it as a string.
//!
//! `Date` and `Time`, a day in the calendar and a time of day, each
//! without the other (SPEC §173), with the same calendar and texts.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NANOS_PER_SECOND: u32 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const NANOS_PER_DAY: u64 = SECONDS_PER_DAY as u64 * NANOS_PER_SECOND as u64;

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

    /// The earliest time there is: every date-time is at or after it.
    pub(crate) const EARLIEST: DateTime = DateTime {
        secs: i64::MIN,
        nanos: 0,
    };

    /// `duration` before this time, or `EARLIEST` if that is before it
    /// (SPEC §139.2).
    pub(crate) fn back(self, duration: Duration) -> DateTime {
        let nanos = i128::from(self.secs) * i128::from(NANOS_PER_SECOND) + i128::from(self.nanos)
            - duration.as_nanos() as i128;
        let secs = nanos.div_euclid(i128::from(NANOS_PER_SECOND));
        match i64::try_from(secs) {
            Ok(secs) => DateTime {
                secs,
                nanos: nanos.rem_euclid(i128::from(NANOS_PER_SECOND)) as u32,
            },
            Err(_) => DateTime::EARLIEST,
        }
    }

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

    /// The day this time falls on, in UTC (SPEC §173):
    /// `DateTime::now().date()` is today there.
    pub fn date(&self) -> Date {
        Date {
            days: self.secs.div_euclid(SECONDS_PER_DAY),
        }
    }

    /// The time of day this time is at, in UTC (SPEC §173).
    pub fn time(&self) -> Time {
        Time {
            nanos: self.secs.rem_euclid(SECONDS_PER_DAY) as u64 * u64::from(NANOS_PER_SECOND)
                + u64::from(self.nanos),
        }
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
        write!(f, "{}T{}Z", self.date(), self.time())
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
        parse_rfc3339(text).ok_or_else(|| ParseDateTimeError::new(text, Expected::DateTime))
    }
}

/// A string that isn't a date and time, a date or a time of day as
/// their `FromStr` reads them: what `str::parse` returns for a
/// `DateTime` (SPEC §70), a `Date` or a `Time` (§173).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDateTimeError {
    input: String,
    expected: Expected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    DateTime,
    Date,
    Time,
}

impl ParseDateTimeError {
    fn new(input: &str, expected: Expected) -> Self {
        ParseDateTimeError {
            input: input.to_string(),
            expected,
        }
    }
}

impl fmt::Display for ParseDateTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.expected {
            Expected::DateTime => "an RFC 3339 date and time (like \"2026-09-27T14:05:00Z\")",
            Expected::Date => "a date (like \"2026-09-27\")",
            Expected::Time => "a time of day (like \"14:05:00\")",
        };
        write!(f, "{:?} is not {what}", self.input)
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

/// A day in the calendar, without a time or a zone (SPEC §173): a
/// birthday, a due date, the day a report covers. What a
/// `Document::Date` holds, written `2026-11-07`; `date("2026-11-07")`
/// in a filter's text. A date compares with dates only, never with a
/// `DateTime`, which is a moment.
///
/// ```
/// use trunkdb::{Date, DateTime, Time};
///
/// let due = Date::from_ymd(2026, 11, 30).unwrap();
/// assert_eq!(due.to_string(), "2026-11-30");
/// assert_eq!("2026-11-30".parse::<Date>().unwrap(), due);
/// let today = DateTime::now().date();          // in UTC
/// let start = due.at(Time::MIDNIGHT).unwrap(); // 2026-11-30T00:00:00Z
/// assert_eq!(start.date(), due);
/// # let _ = today;
/// ```
///
/// Any day a `DateTime` falls on, 292 billion years either side of
/// 1970, in the proleptic Gregorian calendar.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Date {
    days: i64,
}

impl Date {
    /// 1970-01-01.
    pub const UNIX_EPOCH: Date = Date { days: 0 };

    /// The day `DateTime`'s earliest time falls on, and its latest.
    const FIRST: i64 = i64::MIN.div_euclid(SECONDS_PER_DAY);
    const LAST: i64 = i64::MAX.div_euclid(SECONDS_PER_DAY);

    /// The day `day` of `month` (1 to 12) of `year`; `None` for a day that
    /// month doesn't have. Year 0 is 1 BC, as in ISO 8601.
    pub fn from_ymd(year: i64, month: u32, day: u32) -> Option<Date> {
        if !(1..=days_in_month(year, month)?).contains(&day) {
            return None;
        }
        // A year far enough out that the arithmetic would overflow is
        // past `LAST` anyway.
        if year.unsigned_abs() > 300_000_000_000 {
            return None;
        }
        Date::from_days_since_epoch(days_from_civil(year, month, day))
    }

    /// The day `days` days after 1970-01-01 (before, if negative), as
    /// `days_since_epoch` counts them: `Date + n days` is
    /// `Date::from_days_since_epoch(date.days_since_epoch() + n)`.
    /// `None` past the days a `DateTime` falls on.
    pub fn from_days_since_epoch(days: i64) -> Option<Date> {
        (Date::FIRST..=Date::LAST)
            .contains(&days)
            .then_some(Date { days })
    }

    /// Days since 1970-01-01: negative before it.
    pub fn days_since_epoch(&self) -> i64 {
        self.days
    }

    /// The year, 0 for 1 BC and negative before it.
    pub fn year(&self) -> i64 {
        civil_from_days(self.days).0
    }

    /// The month, 1 to 12.
    pub fn month(&self) -> u32 {
        civil_from_days(self.days).1
    }

    /// The day of the month, from 1.
    pub fn day(&self) -> u32 {
        civil_from_days(self.days).2
    }

    /// This day at `time`, in UTC. `None` only at the ends of
    /// `DateTime`'s range, where the day is cut short.
    pub fn at(self, time: Time) -> Option<DateTime> {
        let secs = i128::from(self.days) * i128::from(SECONDS_PER_DAY)
            + i128::from(time.nanos / u64::from(NANOS_PER_SECOND));
        DateTime::from_unix(
            i64::try_from(secs).ok()?,
            (time.nanos % u64::from(NANOS_PER_SECOND)) as u32,
        )
    }
}

/// `YYYY-MM-DD`. A year before 0 or after 9999 gets a sign and as many
/// digits as it has, as in a `DateTime`'s text.
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.days);
        if (0..=9999).contains(&year) {
            write!(f, "{year:04}")?;
        } else {
            write!(f, "{year:+05}")?;
        }
        write!(f, "-{month:02}-{day:02}")
    }
}

/// The text, as `Display` writes it.
impl fmt::Debug for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Date({self})")
    }
}

/// What `Display` writes: `YYYY-MM-DD`, or a signed year of more digits.
impl std::str::FromStr for Date {
    type Err = ParseDateTimeError;

    fn from_str(text: &str) -> Result<Self, ParseDateTimeError> {
        parse_date(text)
            .and_then(|(year, month, day)| Date::from_ymd(year, month, day))
            .ok_or_else(|| ParseDateTimeError::new(text, Expected::Date))
    }
}

/// A time of day, without a date or a zone (SPEC §173), to the
/// nanosecond: opening hours, a daily alarm. What a `Document::Time`
/// holds, written `14:05:00`; `time("14:05")` in a filter's text. A time
/// compares with times only.
///
/// ```
/// use trunkdb::Time;
///
/// let opens = Time::from_hms(9, 30, 0).unwrap();
/// assert_eq!(opens.to_string(), "09:30:00");
/// assert_eq!("09:30".parse::<Time>().unwrap(), opens);
/// assert!(opens < "17:00:00.5".parse().unwrap());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Time {
    nanos: u64,
}

impl Time {
    /// 00:00:00, the start of a day.
    pub const MIDNIGHT: Time = Time { nanos: 0 };

    /// `hour`:`minute`:`second`; `None` past 23:59:59. No leap second.
    pub fn from_hms(hour: u32, minute: u32, second: u32) -> Option<Time> {
        Time::from_hms_nano(hour, minute, second, 0)
    }

    /// `from_hms`, and `nano` nanoseconds after that second, below a
    /// second.
    pub fn from_hms_nano(hour: u32, minute: u32, second: u32, nano: u32) -> Option<Time> {
        if hour > 23 || minute > 59 || second > 59 || nano >= NANOS_PER_SECOND {
            return None;
        }
        let seconds = u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second);
        Some(Time {
            nanos: seconds * u64::from(NANOS_PER_SECOND) + u64::from(nano),
        })
    }

    /// The time `nanos` nanoseconds after midnight; `None` for a day or
    /// more.
    pub fn from_nanos_since_midnight(nanos: u64) -> Option<Time> {
        (nanos < NANOS_PER_DAY).then_some(Time { nanos })
    }

    /// Nanoseconds since midnight, below a day's.
    pub fn nanos_since_midnight(&self) -> u64 {
        self.nanos
    }

    /// The hour, 0 to 23.
    pub fn hour(&self) -> u32 {
        (self.seconds() / 3600) as u32
    }

    /// The minute, 0 to 59.
    pub fn minute(&self) -> u32 {
        (self.seconds() / 60 % 60) as u32
    }

    /// The second, 0 to 59.
    pub fn second(&self) -> u32 {
        (self.seconds() % 60) as u32
    }

    /// Nanoseconds after `second`, below a second.
    pub fn nanosecond(&self) -> u32 {
        (self.nanos % u64::from(NANOS_PER_SECOND)) as u32
    }

    fn seconds(&self) -> u64 {
        self.nanos / u64::from(NANOS_PER_SECOND)
    }
}

/// `HH:MM:SS`, with as many fractional digits as the nanoseconds need,
/// up to nine, as in a `DateTime`'s text.
impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}",
            self.hour(),
            self.minute(),
            self.second()
        )?;
        if self.nanosecond() != 0 {
            let fraction = format!("{:09}", self.nanosecond());
            write!(f, ".{}", fraction.trim_end_matches('0'))?;
        }
        Ok(())
    }
}

/// The text, as `Display` writes it.
impl fmt::Debug for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Time({self})")
    }
}

/// `HH:MM:SS` with a fraction of one to nine digits or none, as
/// `Display` writes it, or `HH:MM`. No offset: a time of day has no
/// zone.
impl std::str::FromStr for Time {
    type Err = ParseDateTimeError;

    fn from_str(text: &str) -> Result<Self, ParseDateTimeError> {
        parse_clock(text, true)
            .and_then(|(hour, minute, second, nanos)| {
                Time::from_hms_nano(hour as u32, minute as u32, second as u32, nanos)
            })
            .ok_or_else(|| ParseDateTimeError::new(text, Expected::Time))
    }
}

/// The names `Date`'s and `Time`'s serde impls give their newtypes, as
/// `DateTime`'s does (SPEC §70, §173).
pub(crate) const DATE_NEWTYPE: &str = "$trunkdb::Date";
pub(crate) const TIME_NEWTYPE: &str = "$trunkdb::Time";

/// Serde impls for a type whose text is its `Display` and `FromStr`, as
/// a newtype of `name` around it.
macro_rules! serde_as_text {
    ($type:ty, $name:expr, $expecting:literal) => {
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_newtype_struct($name, &self.to_string())
            }
        }

        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct TextVisitor;

                impl<'de> serde::de::Visitor<'de> for TextVisitor {
                    type Value = $type;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str($expecting)
                    }

                    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<$type, E> {
                        s.parse().map_err(E::custom)
                    }

                    fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                        self,
                        deserializer: D,
                    ) -> Result<$type, D::Error> {
                        deserializer.deserialize_str(self)
                    }
                }

                deserializer.deserialize_newtype_struct($name, TextVisitor)
            }
        }
    };
}

serde_as_text!(Date, DATE_NEWTYPE, "a date, as text like \"2026-09-27\"");
serde_as_text!(
    Time,
    TIME_NEWTYPE,
    "a time of day, as text like \"14:05:00\""
);

/// `Display` backwards (see `FromStr`). `None` for anything else.
fn parse_rfc3339(text: &str) -> Option<DateTime> {
    let (date, rest) = text.split_at(text.find(['T', 't', ' '])?);
    let rest = &rest[1..];
    let (year, month, day) = parse_date(date)?;
    let (clock, offset) = rest.split_at(rest.find(['Z', 'z', '+', '-'])?);
    let (hour, minute, second, nanos) = parse_clock(clock, false)?;
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

/// `HH:MM:SS`, with a fraction of one to nine digits after a `.`, or
/// `HH:MM` too if `minutes_alone`: the hour, minute, second and
/// nanoseconds. No leap second.
fn parse_clock(clock: &str, minutes_alone: bool) -> Option<(i64, i64, i64, u32)> {
    let (hms, fraction) = match clock.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (clock, None),
    };
    let [hour, minute, second] = match two_digit_fields(hms, ':') {
        Some(fields) => fields,
        None if minutes_alone && fraction.is_none() => {
            let [hour, minute] = two_digit_fields(hms, ':')?;
            [hour, minute, 0]
        }
        None => return None,
    };
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
    Some((hour, minute, second, nanos))
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
    (1..=days_in_month(year, month)?)
        .contains(&day)
        .then_some((year, month, day))
}

/// How many days `month` of `year` has; `None` for a month that isn't
/// 1 to 12.
fn days_in_month(year: i64, month: u32) -> Option<u32> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    })
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
    fn back_counts_back_through_seconds_and_stops_at_the_earliest() {
        let d = |secs, nanos| Duration::new(secs, nanos);
        assert_eq!(at(10, 500).back(d(3, 200)), at(7, 300));
        assert_eq!(at(10, 100).back(d(0, 200)), at(9, 999_999_900));
        assert_eq!(at(0, 0).back(d(1, 1)), at(-2, 999_999_999));
        assert_eq!(at(5, 0).back(d(0, 0)), at(5, 0));
        assert_eq!(at(i64::MIN, 5).back(d(0, 5)), DateTime::EARLIEST);
        assert_eq!(at(i64::MIN, 5).back(d(0, 6)), DateTime::EARLIEST);
        // u64::MAX seconds back from the latest time: before i64::MIN.
        assert_eq!(at(i64::MAX, 0).back(Duration::MAX), DateTime::EARLIEST);
        assert_eq!(at(0, 0).back(Duration::MAX), DateTime::EARLIEST);
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
    fn day(year: i64, month: u32, day: u32) -> Date {
        Date::from_ymd(year, month, day).unwrap()
    }

    /// SPEC §173: a date is its day, written `YYYY-MM-DD`, read back as
    /// written, in calendar order; the days a `DateTime` falls on and no
    /// more.
    #[test]
    fn dates_are_days_written_and_read_as_iso_8601() {
        assert_eq!(day(1970, 1, 1), Date::UNIX_EPOCH);
        assert_eq!(day(1970, 1, 2).days_since_epoch(), 1);
        assert_eq!(day(1969, 12, 31).days_since_epoch(), -1);
        assert_eq!(day(2026, 11, 7).days_since_epoch(), 20_764);
        for (date, text) in [
            (day(2026, 11, 7), "2026-11-07"),
            (day(1969, 12, 31), "1969-12-31"),
            (day(2024, 2, 29), "2024-02-29"),
            (day(1, 1, 1), "0001-01-01"),
            (day(0, 12, 31), "0000-12-31"),
            (day(-1, 3, 1), "-0001-03-01"),
            (day(9999, 12, 31), "9999-12-31"),
            (day(10_000, 1, 1), "+10000-01-01"),
        ] {
            assert_eq!(date.to_string(), text);
            assert_eq!(format!("{date:?}"), format!("Date({text})"));
            assert_eq!(text.parse::<Date>(), Ok(date), "{text}");
            let (y, m, d) = (date.year(), date.month(), date.day());
            assert_eq!(Date::from_ymd(y, m, d), Some(date), "{text}");
        }
        // Every 37th day across eight hundred years reads back.
        for days in (-150_000..150_000).step_by(37) {
            let date = Date::from_days_since_epoch(days).unwrap();
            assert_eq!(date.to_string().parse::<Date>(), Ok(date), "{days}");
        }
        assert!(day(1969, 12, 31) < day(1970, 1, 1));
        assert!(day(2026, 1, 31) < day(2026, 2, 1));

        // The days of `DateTime`'s first and last moments, and no further.
        let first = DateTime::EARLIEST.date();
        let last = at(i64::MAX, 999_999_999).date();
        assert_eq!(
            Date::from_days_since_epoch(first.days_since_epoch() - 1),
            None
        );
        assert_eq!(
            Date::from_days_since_epoch(last.days_since_epoch() + 1),
            None
        );
        assert_eq!(first.to_string().parse::<Date>(), Ok(first));
        assert_eq!(last.to_string().parse::<Date>(), Ok(last));
        assert_eq!(Date::from_ymd(i64::MAX, 1, 1), None);
        assert_eq!(Date::from_ymd(-300_000_000_000, 1, 1), None);

        for bad in [
            "",
            "2026-11-7",
            "2026-11-07T00:00:00Z",
            "2026-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-00-01",
            "2026-11-00",
            "12026-11-07",
            "+1000000000000-01-01",
            "+292277026597-01-01",
            "2026/11/07",
        ] {
            assert!(bad.parse::<Date>().is_err(), "{bad}");
        }
        assert_eq!(Date::from_ymd(2026, 2, 29), None);
        assert_eq!(Date::from_ymd(2026, 13, 1), None);
        let error = "soon".parse::<Date>().unwrap_err().to_string();
        assert_eq!(error, r#""soon" is not a date (like "2026-09-27")"#);
    }

    /// SPEC §173: a time of day to the nanosecond, written with the
    /// digits it needs; `HH:MM` read too; no zone, no leap second.
    #[test]
    fn times_of_day_are_written_and_read_to_the_nanosecond() {
        let time = |h, m, s, n| Time::from_hms_nano(h, m, s, n).unwrap();
        for (t, text) in [
            (Time::MIDNIGHT, "00:00:00"),
            (time(9, 30, 0, 0), "09:30:00"),
            (time(23, 59, 59, 999_999_999), "23:59:59.999999999"),
            (time(14, 5, 0, 500_000_000), "14:05:00.5"),
            (time(0, 0, 0, 1), "00:00:00.000000001"),
        ] {
            assert_eq!(t.to_string(), text);
            assert_eq!(format!("{t:?}"), format!("Time({text})"));
            assert_eq!(text.parse::<Time>(), Ok(t), "{text}");
            let parts = (t.hour(), t.minute(), t.second(), t.nanosecond());
            assert_eq!(
                Time::from_hms_nano(parts.0, parts.1, parts.2, parts.3),
                Some(t)
            );
            assert_eq!(
                Time::from_nanos_since_midnight(t.nanos_since_midnight()),
                Some(t)
            );
        }
        assert_eq!("09:30".parse::<Time>(), Ok(time(9, 30, 0, 0)));
        assert_eq!(time(1, 2, 3, 4).nanos_since_midnight(), 3_723_000_000_004);
        assert!(time(9, 59, 59, 999_999_999) < time(10, 0, 0, 0));
        let last = NANOS_PER_DAY - 1;
        assert_eq!(
            Time::from_nanos_since_midnight(last),
            Some(time(23, 59, 59, 999_999_999))
        );
        assert_eq!(Time::from_nanos_since_midnight(NANOS_PER_DAY), None);
        for (h, m, s, n) in [
            (24, 0, 0, 0),
            (0, 60, 0, 0),
            (0, 0, 60, 0),
            (0, 0, 0, NANOS_PER_SECOND),
        ] {
            assert_eq!(Time::from_hms_nano(h, m, s, n), None, "{h}:{m}:{s}.{n}");
        }
        for bad in [
            "",
            "9:30",
            "09:30:0",
            "24:00",
            "23:60",
            "23:59:60",
            "09:30.5",
            "09:30:00.",
            "09:30:00.1234567890",
            "09:30:00Z",
            "09:30:00+02:00",
            "09",
            "09:30:00:00",
        ] {
            assert!(bad.parse::<Time>().is_err(), "{bad}");
        }
        let error = "noon".parse::<Time>().unwrap_err().to_string();
        assert_eq!(error, r#""noon" is not a time of day (like "14:05:00")"#);
    }

    /// SPEC §173: a date-time's day and time of day in UTC, and a day at
    /// a time back to the date-time.
    #[test]
    fn a_date_time_is_a_day_at_a_time_of_day() {
        for (secs, nanos) in [
            (0, 0),
            (1_790_000_000, 120_000_000),
            (-1, 999_999_999),
            (-86_400, 0),
            (-86_401, 5),
            (i64::MAX, 999_999_999),
            (i64::MAX - 86_400, 0),
        ] {
            let moment = at(secs, nanos);
            let (date, time) = (moment.date(), moment.time());
            assert_eq!(date.at(time), Some(moment), "{moment}");
            assert_eq!(moment.to_string(), format!("{date}T{time}Z"));
        }
        assert_eq!(at(-1, 0).date(), day(1969, 12, 31));
        assert_eq!(at(-1, 0).time().to_string(), "23:59:59");
        // The first day is cut short before `EARLIEST`: midnight there
        // is no date-time.
        let first = DateTime::EARLIEST.date();
        assert_eq!(first.at(Time::MIDNIGHT), None);
        assert_eq!(
            first.at(DateTime::EARLIEST.time()),
            Some(DateTime::EARLIEST)
        );
        // And the last ends at the last second.
        let last = at(i64::MAX, 0).date();
        assert_eq!(last.at(Time::from_hms(23, 59, 59).unwrap()), None);
    }
}
