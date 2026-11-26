//! A group field's date-time cut down to the start of its unit (SPEC
//! §209, §210): SQL's `date_trunc`, for `group by trunc(created, 1m)`
//! and `group by date(created)`.

use crate::datetime::{Date, DateTime, Time};
use crate::document::Document;
use std::time::Duration;

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: u128 = 86_400 * 1_000_000_000;

/// The unit [`Aggregate::group_by_trunc`](crate::query::Aggregate::group_by_trunc)
/// cuts a date-time down to the start of (SPEC §209.2), in UTC:
///
/// ```
/// use std::time::Duration;
/// use trunkdb::query::{Aggregate, Trunc};
///
/// let per_quarter_hour = Aggregate::new()
///     .group_by("server")
///     .group_by_trunc("at", Trunc::every(Duration::from_secs(15 * 60)))
///     .count().named("n");
/// let per_month = Aggregate::new().group_by_trunc("at", Trunc::Month).count().named("n");
/// # let _ = (per_quarter_hour, per_month);
/// ```
///
/// - **`Every(duration)`:** the latest instant at or before the value
///   that is a whole number of `duration` after 1970-01-01T00:00:00Z,
///   counted down before 1970 too: `23:59:30` on 1969-12-31 cut to a
///   minute is `23:59:00`. A zero duration is refused when the aggregate
///   runs.
/// - **`Week`:** midnight of the Monday on or before it, ISO 8601's week.
/// - **`Month`, `Year`:** midnight of the month's or the year's first
///   day.
///
/// A `DateTime` is cut to a `DateTime`. A `Date` is cut by a whole number
/// of days, `Week`, `Month` or `Year` to a `Date`, and is left as it is by
/// a duration that isn't whole days. Any other value is left as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Trunc {
    /// To a whole number of this duration since 1970, rounded down.
    Every(Duration),
    /// To the Monday the date-time's week starts on.
    Week,
    /// To the first day of its month.
    Month,
    /// To the first day of its year.
    Year,
}

impl Trunc {
    /// `Trunc::Every(duration)`: `Trunc::every(Duration::from_secs(60))`
    /// cuts to the minute.
    pub fn every(duration: Duration) -> Trunc {
        Trunc::Every(duration)
    }

    /// `time` cut down to the start of its unit; `DateTime`'s earliest
    /// time where that start is before it.
    pub(crate) fn date_time(self, time: DateTime) -> DateTime {
        match self {
            Trunc::Every(duration) => {
                let size = duration.as_nanos() as i128;
                if size == 0 {
                    return time;
                }
                let nanos = i128::from(time.unix_seconds()) * NANOS_PER_SECOND
                    + i128::from(time.subsec_nanos());
                let start = nanos - nanos.rem_euclid(size);
                i64::try_from(start.div_euclid(NANOS_PER_SECOND))
                    .ok()
                    .and_then(|secs| {
                        DateTime::from_unix(secs, start.rem_euclid(NANOS_PER_SECOND) as u32)
                    })
                    .unwrap_or(DateTime::EARLIEST)
            }
            _ => (self.date(time.date()))
                .at(Time::MIDNIGHT)
                .unwrap_or(DateTime::EARLIEST),
        }
    }

    /// `date` cut down to the first day of its unit, `Date`'s first day
    /// where that is before it; as it is by a duration that isn't whole
    /// days.
    pub(crate) fn date(self, date: Date) -> Date {
        let days = date.days_since_epoch();
        let first = match self {
            Trunc::Every(duration) => {
                let nanos = duration.as_nanos();
                if nanos == 0 || nanos % NANOS_PER_DAY != 0 {
                    return date;
                }
                // Days that many past the epoch: past `i64` the size is
                // longer than every date, all in the one before 1970.
                match i64::try_from(nanos / NANOS_PER_DAY) {
                    Ok(size) => Some(days - days.rem_euclid(size)),
                    Err(_) if days >= 0 => Some(0),
                    Err(_) => None,
                }
            }
            // 1970-01-01 was a Thursday, three days after a Monday.
            Trunc::Week => Some(days - (days + 3).rem_euclid(7)),
            Trunc::Month => {
                Date::from_ymd(date.year(), date.month(), 1).map(|d| d.days_since_epoch())
            }
            Trunc::Year => Date::from_ymd(date.year(), 1, 1).map(|d| d.days_since_epoch()),
        };
        first
            .and_then(Date::from_days_since_epoch)
            .unwrap_or(Date::FIRST_DAY)
    }
}

/// What a group field's value becomes before it is grouped (SPEC §210).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// `trunc(path, unit)`.
    Trunc(Trunc),
    /// `date(path)`: a date-time's day, as a `Date`.
    Date,
}

impl Cut {
    /// `value` cut: a date-time or a date as 209.2 says, anything else
    /// as it is.
    pub(crate) fn apply(self, value: &Document) -> Document {
        match (self, value) {
            (Cut::Trunc(unit), Document::DateTime(time)) => {
                Document::DateTime(unit.date_time(*time))
            }
            (Cut::Trunc(unit), Document::Date(date)) => Document::Date(unit.date(*date)),
            (Cut::Date, Document::DateTime(time)) => Document::Date(time.date()),
            _ => value.clone(),
        }
    }

    /// The first value of `value`'s kind past its bucket, for an index
    /// walked in order to tell a key in the bucket by its bytes alone
    /// (SPEC §212): every value of that kind from `value` up to it cuts as
    /// `value` does. `None` for a kind a cut leaves, and past the latest
    /// time. Checked here: the value just before it cuts as `value` does,
    /// and a cut keeps order, so every one between does too; an end too
    /// soon only costs a key read the longer way.
    pub(crate) fn end(self, value: &Document) -> Option<Document> {
        let end = match *value {
            Document::DateTime(time) => Document::DateTime(self.time_end(time)?),
            Document::Date(date) => Document::Date(self.date_end(date)?),
            _ => return None,
        };
        let before = match &end {
            Document::DateTime(end) => Document::DateTime(from_nanos(nanos(*end) - 1)?),
            Document::Date(end) => {
                Document::Date(Date::from_days_since_epoch(end.days_since_epoch() - 1)?)
            }
            _ => unreachable!("a date-time or a date"),
        };
        (self.apply(&before) == self.apply(value)).then_some(end)
    }

    fn time_end(self, time: DateTime) -> Option<DateTime> {
        match self {
            Cut::Trunc(Trunc::Every(duration)) => {
                let size = duration.as_nanos() as i128;
                let now = nanos(time);
                from_nanos(now - now.rem_euclid(size.max(1)) + size.max(1))
            }
            _ => self.date_end(time.date())?.at(Time::MIDNIGHT),
        }
    }

    fn date_end(self, date: Date) -> Option<Date> {
        let days = date.days_since_epoch();
        let next = |days: i64| Date::from_days_since_epoch(days);
        match self {
            Cut::Date => next(days + 1),
            Cut::Trunc(Trunc::Every(duration)) => {
                let size = duration.as_nanos();
                match size != 0 && size % NANOS_PER_DAY == 0 {
                    true => {
                        let size = i64::try_from(size / NANOS_PER_DAY).ok()?;
                        next(days.checked_sub(days.rem_euclid(size))?.checked_add(size)?)
                    }
                    // Left as it is: each date its own bucket.
                    false => next(days + 1),
                }
            }
            Cut::Trunc(Trunc::Week) => next(days - (days + 3).rem_euclid(7) + 7),
            Cut::Trunc(Trunc::Month) => match date.month() {
                12 => Date::from_ymd(date.year() + 1, 1, 1),
                month => Date::from_ymd(date.year(), month + 1, 1),
            },
            Cut::Trunc(Trunc::Year) => Date::from_ymd(date.year() + 1, 1, 1),
        }
    }
}

/// A date-time as nanoseconds since 1970.
fn nanos(time: DateTime) -> i128 {
    i128::from(time.unix_seconds()) * NANOS_PER_SECOND + i128::from(time.subsec_nanos())
}

/// Nanoseconds since 1970 as a date-time; `None` past either end.
fn from_nanos(nanos: i128) -> Option<DateTime> {
    let secs = i64::try_from(nanos.div_euclid(NANOS_PER_SECOND)).ok()?;
    DateTime::from_unix(secs, nanos.rem_euclid(NANOS_PER_SECOND) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime {
        text.parse().unwrap()
    }

    fn day(text: &str) -> Date {
        text.parse().unwrap()
    }

    fn every(secs: u64) -> Trunc {
        Trunc::every(Duration::from_secs(secs))
    }

    #[test]
    fn a_date_time_is_cut_to_the_start_of_its_unit() {
        let t = at("2026-11-23T14:37:52.123456789Z");
        assert_eq!(
            Trunc::every(Duration::from_millis(1)).date_time(t),
            at("2026-11-23T14:37:52.123Z")
        );
        assert_eq!(every(1).date_time(t), at("2026-11-23T14:37:52Z"));
        assert_eq!(every(60).date_time(t), at("2026-11-23T14:37:00Z"));
        assert_eq!(every(15 * 60).date_time(t), at("2026-11-23T14:30:00Z"));
        assert_eq!(every(3600).date_time(t), at("2026-11-23T14:00:00Z"));
        assert_eq!(every(86_400).date_time(t), at("2026-11-23T00:00:00Z"));
        // 2026-11-23 is a Monday; the 29th a Sunday.
        assert_eq!(Trunc::Week.date_time(t), at("2026-11-23T00:00:00Z"));
        assert_eq!(
            Trunc::Week.date_time(at("2026-11-29T23:59:59Z")),
            at("2026-11-23T00:00:00Z")
        );
        assert_eq!(
            Trunc::Week.date_time(at("2026-11-22T23:59:59Z")),
            at("2026-11-16T00:00:00Z")
        );
        assert_eq!(Trunc::Month.date_time(t), at("2026-11-01T00:00:00Z"));
        assert_eq!(Trunc::Year.date_time(t), at("2026-01-01T00:00:00Z"));
        // A start is its own start.
        let start = at("2026-11-23T14:00:00Z");
        assert_eq!(every(3600).date_time(start), start);
        // Seven minutes counted from 1970, not from midnight.
        assert_eq!(
            every(7 * 60).date_time(at("1970-01-01T00:15:00Z")),
            at("1970-01-01T00:14:00Z")
        );
    }

    #[test]
    fn before_1970_it_is_cut_down_not_toward_1970() {
        assert_eq!(
            every(60).date_time(at("1969-12-31T23:59:30Z")),
            at("1969-12-31T23:59:00Z")
        );
        assert_eq!(
            Trunc::every(Duration::from_millis(1)).date_time(at("1969-12-31T23:59:59.9995Z")),
            at("1969-12-31T23:59:59.999Z")
        );
        assert_eq!(
            every(86_400).date_time(at("1969-12-31T00:00:01Z")),
            at("1969-12-31T00:00:00Z")
        );
        // 1969-12-29 was a Monday.
        assert_eq!(
            Trunc::Week.date_time(at("1969-12-31T12:00:00Z")),
            at("1969-12-29T00:00:00Z")
        );
        assert_eq!(
            Trunc::Month.date_time(at("1969-02-14T12:00:00Z")),
            at("1969-02-01T00:00:00Z")
        );
    }

    #[test]
    fn at_the_ends_of_time_it_stops_at_the_earliest() {
        let earliest = DateTime::EARLIEST;
        assert_eq!(every(3600).date_time(earliest), earliest);
        assert_eq!(Trunc::Year.date_time(earliest), earliest);
        let latest = DateTime::from_unix(i64::MAX, 999_999_999).unwrap();
        assert!(every(3600).date_time(latest) <= latest);
        assert!(Trunc::Year.date_time(latest) <= latest);
        assert_eq!(
            Trunc::every(Duration::MAX).date_time(at("2026-11-23T00:00:00Z")),
            DateTime::UNIX_EPOCH
        );
    }

    #[test]
    fn a_date_is_cut_by_whole_days_and_left_by_less() {
        let d = day("2026-11-25");
        assert_eq!(every(86_400).date(d), d);
        assert_eq!(Trunc::Week.date(d), day("2026-11-23"));
        assert_eq!(Trunc::Month.date(d), day("2026-11-01"));
        assert_eq!(Trunc::Year.date(d), day("2026-01-01"));
        assert_eq!(every(3600).date(d), d);
        assert_eq!(every(36 * 3600).date(d), d);
        // Two days since 1970: 2026-11-25 is day 20,782.
        assert_eq!(every(2 * 86_400).date(d), day("2026-11-25"));
        assert_eq!(every(2 * 86_400).date(day("2026-11-26")), day("2026-11-25"));
        assert_eq!(every(2 * 86_400).date(day("1969-12-31")), day("1969-12-30"));
    }

    #[test]
    fn a_cut_changes_date_times_and_dates_alone() {
        let t = at("2026-11-23T14:37:52Z");
        let hour = Cut::Trunc(every(3600));
        assert_eq!(
            hour.apply(&Document::DateTime(t)),
            Document::DateTime(at("2026-11-23T14:00:00Z"))
        );
        assert_eq!(
            Cut::Date.apply(&Document::DateTime(t)),
            Document::Date(day("2026-11-23"))
        );
        assert_eq!(
            Cut::Date.apply(&Document::Date(day("2026-11-23"))),
            Document::Date(day("2026-11-23"))
        );
        // A date: cut by a month, left by an hour.
        assert_eq!(
            Cut::Trunc(Trunc::Month).apply(&Document::Date(day("2026-11-23"))),
            Document::Date(day("2026-11-01"))
        );
        assert_eq!(
            hour.apply(&Document::Date(day("2026-11-23"))),
            Document::Date(day("2026-11-23"))
        );
        for other in [
            Document::Null,
            Document::Int(5),
            Document::from("14:37"),
            Document::Bool(true),
        ] {
            assert_eq!(hour.apply(&other), other);
            assert_eq!(Cut::Date.apply(&other), other);
        }
    }

    /// A bucket's end is the first value past it: the one before cuts as
    /// the value does, the end itself doesn't. Across units, kinds, before
    /// 1970 and at the ends of time.
    #[test]
    fn a_bucket_ends_where_the_next_begins() {
        let cuts = [
            Cut::Trunc(Trunc::every(Duration::from_nanos(1))),
            Cut::Trunc(Trunc::every(Duration::from_millis(250))),
            Cut::Trunc(every(60)),
            Cut::Trunc(every(7 * 3600)),
            Cut::Trunc(every(86_400)),
            Cut::Trunc(every(3 * 86_400)),
            Cut::Trunc(Trunc::Week),
            Cut::Trunc(Trunc::Month),
            Cut::Trunc(Trunc::Year),
            Cut::Date,
        ];
        let values = [
            Document::DateTime(at("2026-11-23T14:37:52.123456789Z")),
            Document::DateTime(at("2026-12-31T23:59:59.999999999Z")),
            Document::DateTime(at("1969-12-31T23:59:30Z")),
            Document::DateTime(at("1970-01-01T00:00:00Z")),
            Document::DateTime(at("2024-02-29T12:00:00Z")),
            Document::Date(day("2026-11-23")),
            Document::Date(day("2026-12-31")),
            Document::Date(day("1969-12-31")),
            Document::Date(day("2024-02-29")),
        ];
        for cut in cuts {
            for value in &values {
                let end = cut.end(value).unwrap_or_else(|| panic!("{cut:?} {value}"));
                assert_ne!(cut.apply(&end), cut.apply(value), "{cut:?} {value}: {end}");
                assert_eq!(
                    std::mem::discriminant(&end),
                    std::mem::discriminant(value),
                    "{cut:?} {value}"
                );
            }
            for other in [
                Document::Int(3),
                Document::String("a".into()),
                Document::Null,
            ] {
                assert_eq!(cut.end(&other), None);
            }
            let latest = Document::DateTime(DateTime::LATEST);
            assert_eq!(cut.end(&latest), None, "{cut:?}");
        }
    }
}
