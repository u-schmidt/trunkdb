//! What GUIDE.md says about `chrono` and `jiff`: their types are stored
//! as strings (SPEC §69.5), and the text is the bridge to trunkdb's.

use serde::{Deserialize, Serialize};
use trunkdb::query::{Filter, Kind};
use trunkdb::{Database, Date, DateTime, Time};

#[derive(Serialize, Deserialize)]
struct Reading {
    chrono: chrono::DateTime<chrono::Utc>,
    jiff: jiff::Timestamp,
    trunkdb: DateTime,
}

#[test]
fn their_fields_are_stored_as_strings() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("readings.trunkdb")).unwrap();
    let readings = db.collection::<Reading>("readings");
    let at: DateTime = "2026-11-07T14:05:00.5Z".parse().unwrap();
    readings
        .insert(Reading {
            chrono: at.to_string().parse().unwrap(),
            jiff: at.to_string().parse().unwrap(),
            trunkdb: at,
        })
        .unwrap();
    for field in ["chrono", "jiff"] {
        assert_eq!(
            readings
                .count(Filter::new().is_kind(field, Kind::String))
                .unwrap(),
            1
        );
        assert_eq!(
            readings
                .count(Filter::new().gte(field, DateTime::UNIX_EPOCH))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        readings
            .count(Filter::new().is_kind("trunkdb", Kind::DateTime))
            .unwrap(),
        1
    );
    assert_eq!(
        readings
            .count(Filter::new().gte("trunkdb", DateTime::UNIX_EPOCH))
            .unwrap(),
        1
    );
}

#[test]
fn chrono_sorts_as_text_out_of_time_order() {
    let whole: chrono::DateTime<chrono::Utc> = "2026-11-07T14:05:00Z".parse().unwrap();
    let later = whole + chrono::Duration::milliseconds(500);
    let texts = [
        serde_json::to_string(&whole).unwrap(),
        serde_json::to_string(&later).unwrap(),
    ];
    assert_eq!(texts[0], r#""2026-11-07T14:05:00Z""#);
    assert_eq!(texts[1], r#""2026-11-07T14:05:00.500Z""#);
    assert!(texts[1] < texts[0], "the later one sorts first as text");
}

#[test]
fn the_text_is_the_bridge() {
    let at: DateTime = "2026-11-07T14:05:00.123456789Z".parse().unwrap();

    let c: chrono::DateTime<chrono::Utc> = at.to_string().parse().unwrap();
    assert_eq!(c.to_rfc3339().parse::<DateTime>().unwrap(), at);
    let j: jiff::Timestamp = at.to_string().parse().unwrap();
    assert_eq!(j.to_string().parse::<DateTime>().unwrap(), at);

    // Another offset is the same moment.
    let berlin = c.with_timezone(&chrono::FixedOffset::east_opt(3600).unwrap());
    assert!(berlin.to_rfc3339().ends_with("+01:00"));
    assert_eq!(berlin.to_rfc3339().parse::<DateTime>().unwrap(), at);

    let day: Date = chrono::NaiveDate::from_ymd_opt(2026, 11, 7)
        .unwrap()
        .to_string()
        .parse()
        .unwrap();
    assert_eq!(day, Date::from_ymd(2026, 11, 7).unwrap());
    assert_eq!(
        day.to_string().parse::<jiff::civil::Date>().unwrap(),
        jiff::civil::date(2026, 11, 7)
    );
    let starts: Time = jiff::civil::time(14, 5, 0, 0).to_string().parse().unwrap();
    assert_eq!(starts, Time::from_hms(14, 5, 0).unwrap());
    let back: chrono::NaiveTime = starts.to_string().parse().unwrap();
    assert_eq!(back, chrono::NaiveTime::from_hms_opt(14, 5, 0).unwrap());
}

#[test]
fn the_three_exceptions() {
    // A jiff `Zoned` writes its zone's name; its timestamp converts.
    let zoned = jiff::Timestamp::from_second(0)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::fixed(jiff::tz::offset(1)));
    assert!(zoned.to_string().parse::<DateTime>().is_err());
    assert_eq!(
        zoned.timestamp().to_string().parse::<DateTime>().unwrap(),
        DateTime::UNIX_EPOCH
    );

    // A chrono `NaiveDateTime` has no zone, so no moment, until given one.
    let naive = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    assert!(naive.to_string().parse::<DateTime>().is_err());
    assert_eq!(
        naive.and_utc().to_rfc3339().parse::<DateTime>().unwrap(),
        DateTime::UNIX_EPOCH
    );

    // Past 9999, with a sign: chrono reads it, jiff's years end at 9999.
    let far = Date::from_ymd(10000, 1, 1)
        .unwrap()
        .at(Time::MIDNIGHT)
        .unwrap();
    assert_eq!(far.to_string(), "+10000-01-01T00:00:00Z");
    let c: chrono::DateTime<chrono::Utc> = far.to_string().parse().unwrap();
    assert_eq!(c.to_rfc3339().parse::<DateTime>().unwrap(), far);
    assert!(far.to_string().parse::<jiff::Timestamp>().is_err());
}
