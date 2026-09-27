# 69. A date-time type (`datetime.rs`, `document.rs`, `serde_bridge.rs`, `index/key.rs`, `query.rs`, `json.rs`, `export.rs`)

A point in time had no type of its own. It went in as a string or a
number, and a filter or an index saw just that. An RFC 3339 string in
UTC, written the same way every time, sorts in time order, and so do
Unix milliseconds; but nothing said the field was a time, so another
string or number in it went unnoticed, and a `SystemTime` field was
stored as serde writes it, an object of seconds and nanoseconds, which
sorts and compares as an object: not at all (§34.1). An application
that stores timestamps asked for better. Now `Document` has
`DateTime`, and a `SystemTime` field is one.

```rust
#[derive(Serialize, Deserialize)]
struct Scan {
    #[serde(rename = "_id")]
    id: Option<DocId>,
    at: SystemTime,
}

scans.ensure_index("at")?;
let last_hour = scans.find(Filter::new().gte("at", SystemTime::now() - Duration::from_secs(3600)))?;
scans.update_fields(Filter::new().id(id), &Update::new().set("at", SystemTime::now()))?;
```

## 69.1 What it holds
`Document::DateTime(SystemTime)`: an instant, in UTC, to the
nanosecond. Asked of the application that needed it:

- **UTC, no offset or time zone.** A time is a moment; the zone it was
  recorded in is for display, and an application that needs it keeps
  it in a field of its own. BSON's datetime does the same.
- **`std::time::SystemTime`**, no new dependency. `chrono`, `time` and
  `jiff` are left for when an application wants them (69.5).
- **Nanoseconds, not BSON's milliseconds**, so a `SystemTime` reads back
  exactly as it was written, and `==` on what was stored holds.
- **A date without a time, or a time without a date**, isn't a type of
  its own yet: an application can store noon, UTC, of the day, and
  ignore the time. A variant for each can come later; `Document` is
  `#[non_exhaustive]` (§60).

## 69.2 Stored, compared, indexed
- **In a document**: tag 9, the whole seconds since 1970-01-01T00:00:00Z
  as an `i64` (negative before), then the nanoseconds past that second
  as a `u32` (0 to 999,999,999, also before 1970), 13 bytes. A decoder
  refuses nanoseconds of a second or more, and a time the platform's
  `SystemTime` can't hold, as damage (§55).
- **In a filter**: date-times compare with date-times, in time order;
  never with a string or a number, as ids don't (§59.5). `From<SystemTime>
  for Document` lets a filter or an update take a `SystemTime` as it is.
- **In a sort**: after ids (§62), before the values nothing orders
  (§34.1).
- **In an index key**: tag 5, after ids' 4, so no older key moves and no
  index needs rebuilding; the seconds big-endian with the sign bit
  flipped, then the nanoseconds big-endian: byte order is time order.
  13 bytes, so a compound key's parts split it (§43.2).
- **File format 11.** A build before it can't decode the tag, so it
  refuses the file, as always (§21.2). Formats 6 to 10 open as they are
  (they hold no date-times) and are stamped 11 by the first write.

## 69.3 `SystemTime` through serde
Serde writes a `SystemTime` as a struct named `SystemTime` with exactly
two fields, `secs_since_epoch` and `nanos_since_epoch`. The serde bridge
(§13) turns a struct of that name and those fields into a `DateTime`,
as it turns `DocId`'s marked newtype into an `Id` (§59). A struct of any
other name keeps being an object.

Read back, a `DateTime` is what serde's `SystemTime` asks for, a struct
of those two fields: so a `SystemTime` field reads it, and so does an
`Option<SystemTime>`, an untagged enum or a `#[serde(flatten)]` that
buffers it first. Read as a string, a `String` field for instance, it is
its RFC 3339 text in UTC (69.4).

Rejected:
- **A `DateTime` type of trunkdb's own**, with serde impls that handle
  times before 1970 too: every struct would have to use it instead of
  `SystemTime`, which it already uses.
- **Milliseconds, as BSON:** a `SystemTime` written and read back would
  no longer be equal to itself, and a filter `eq("at", t)` with the
  same `t` that was stored wouldn't find it.
- **Offsets or zones in the value:** two values for one moment, and a
  question for every comparison.

## 69.4 Text: the export, and reading as a string
In tagged JSON (§30.1) a date-time is `{"$date": "2026-09-21T14:13:20.12Z"}`:
RFC 3339, in UTC, with as many fractional digits as it needs, none to
nine. A year before 0 or after 9999 gets a sign and more digits (ISO
8601's expanded years), so every `SystemTime` has a text. Reading
accepts RFC 3339 as others write it too: an offset instead of `Z`, a
lower-case `t` or `z`, a space for the `T`. The calendar arithmetic is
Howard Hinnant's `days_from_civil` and `civil_from_days`, about forty
lines, rather than a dependency.

**Export format 2.** In format 1, `$date` wasn't a tag, and an object
`{"$date": ...}` was written as an ordinary object. Read with `$date` as
a tag it would become a time, or fail. So exports now say version 2,
and an import reads both: a format 1 export without the `$date` tag, as
it was written. A build before this refuses a format 2 export, instead
of reading its dates as objects. An object that is `{"$date": ...}` is
now written inside `$object`, like one that looks like any other tag.

## 69.5 Limits
- **Serde's `SystemTime` writes and reads no time before 1970.** That's
  serde's impl, not trunkdb's: a `Document::DateTime` before 1970 is
  stored, compared, indexed and exported like any other, but a struct
  can't hold one in a `SystemTime` field.
- **`chrono`, `time`, `jiff`:** their types serialize as strings, and
  are stored as strings. Reading a `DateTime` as a string gives RFC
  3339, which their `DateTime<Utc>` and similar parse, so a stored time
  reads into them; writing one stores text. Recognizing them on the way
  in, behind a feature each, is open (ROADMAP.md).
- **A field with times written both ways**, as serde's object before
  this change and as a `DateTime` after, sorts the objects apart from
  the times. Rewriting each document once (`update_many` with a closure
  that changes nothing still writes it, since the stored bytes differ)
  moves the old ones over.
- **No date-only or time-only type** (69.1).
- **Arithmetic in an update:** `inc` on a date-time is refused, as on
  anything but a number.

## 69.6 Tests
- `datetime.rs`: seconds and nanoseconds round trip before and after
  1970; RFC 3339 written with the digits needed, before 1970, across
  leap days, for years 1, -1 and 10000; every third day for eight
  hundred years read back as written; other writers' forms read, and
  eighteen malformed texts refused, among them February 29 of 1900 and
  2025.
- `document.rs`: date-times encode and decode to the nanosecond; a
  second of nanoseconds, and a date-time cut short, are damage.
- `index/key.rs`: keys in time order across the sign and nanoseconds
  where little-endian would reorder (1, 256, 65,536), after every id,
  before what compound keys can't order; a compound key split right.
- `query.rs`: comparisons in time order; no equality with the text or a
  number; sorting after ids.
- `serde_bridge.rs`: a `SystemTime` field, an `Option`, a bare
  `SystemTime`, `now()` and the epoch round trip; read as a `String`,
  RFC 3339; as a `serde_json::Value`, serde's shape; a look-alike struct
  stays an object; before 1970, serde's own refusal.
- `json.rs`: `$date` written and read; a `{"$date": ...}` object kept
  apart by `$object`; a format 1 line keeps it an object, a format 2
  line needs RFC 3339; an offset read as the same moment.
- `export.rs`: an import of format 2 reads a date-time; of format 1, the
  object, whatever it holds; a bad `$date` names its line.
- `collection.rs`: a struct with `SystemTime` fields, indexed, found by
  a range through the index, sorted, changed by `update_fields`, the
  same to the nanosecond after a reopen and after an export and
  import; the random values every randomized index, filter, update and
  export test draws from now include date-times.
- `tests/cli.rs`: a format 1 export imports, and the export after says
  format 2; `info` says format 11.
- Checked by breaking it on purpose, seventeen ways. Each of these
  fails a test:
  - the key's seconds without the sign flip; its nanoseconds
    little-endian; a compound part a byte short;
  - date-times not comparing; sorting as unordered;
  - before 1970 without borrowing a second; a second of nanoseconds
    accepted; the fraction untrimmed; an offset's sign reversed; every
    fourth year a leap year;
  - `SystemTime` not recognized, or any struct with its fields
    recognized; no text for a string field;
  - a format 1 export reading `$date` as a tag; the import ignoring the
    version; the export still saying 1; the file format not bumped.
