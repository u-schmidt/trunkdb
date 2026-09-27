# 70. A portable date-time (`datetime.rs`, `document.rs`, `serde_bridge.rs`)

`Document::DateTime` held a `SystemTime` (§69), and a `SystemTime` isn't
the same on every platform. Windows counts in steps of 100 ns from
1601, so the tests that CI ran there failed eight ways, and two of the
failures were real:

- **Precision:** a time stored on macOS or Linux with nanoseconds came
  back on Windows rounded to 100 ns, so it no longer equaled itself.
- **Range:** a time before 1601 has no `SystemTime` on Windows at all.
  The decoder called it damage (§55), so a document holding one,
  written on macOS, couldn't be read on Windows.

A file format has to mean the same everywhere. So `Document::DateTime`
now holds trunkdb's own `DateTime`: the seconds and nanoseconds the
file already stored, unchanged, and the same on every platform.

```rust
use trunkdb::DateTime;

let now = DateTime::now();
let at: DateTime = "2026-09-27T14:05:00Z".parse()?;
let as_system_time = at.to_system_time();    // Option: None where a platform can't hold it
let from_system_time = DateTime::from(std::time::SystemTime::now());
```

## 70.1 `DateTime`
- **What it is:** an `i64` of seconds since 1970-01-01T00:00:00Z, and
  the nanoseconds past that second, below a second. `Copy`, `Eq`,
  `Ord` in time order, `Hash`. Any time an `i64` of seconds reaches,
  292 billion years either side of 1970.
- **Made** by `DateTime::now()`, `DateTime::from_unix(secs, nanos)`
  (`None` for a second or more of nanoseconds), `From<SystemTime>`,
  or `str::parse` of RFC 3339; `DateTime::UNIX_EPOCH`.
- **Read** by `unix_seconds()`, `subsec_nanos()`, `Display` (RFC 3339
  in UTC, §69.4), and `to_system_time()`: an `Option`, as that's the
  one direction that can fail, and where it does depends on the
  platform.
- **`ParseDateTimeError`** for text that isn't RFC 3339, naming the
  text, as `ParseIdError` does for ids (§63).
- **Serde:** a newtype around its RFC 3339 text, named so the serde
  bridge stores it as a date-time, as `DocId`'s is (§59). So a struct
  can hold a `DateTime` field, and gets what a `SystemTime` field can't:
  times before 1970, which serde's `SystemTime` refuses (§69.5), and
  every nanosecond on every platform. Other formats, `serde_json` say,
  see the text.
- **`SystemTime` fields** work as before (§69.3): stored as date-times,
  and read back by serde's own impl from the seconds and nanoseconds.
  Where the platform can't hold the stored time, reading that
  document into a `SystemTime` field fails with serde's error, as any
  value that doesn't fit its field does. A `DateTime` field reads it.

Nothing on disk changes: the file stored seconds and nanoseconds
already (§69.2), and an export writes the same text. Decoding no
longer depends on the platform, so a date-time is damage only if its
nanoseconds are a second or more.

Rejected:
- **Keeping `SystemTime`, rounding to 100 ns and refusing times before
  1601 everywhere:** it would make every platform as narrow as
  Windows, and still change what a time stored on macOS reads as.
- **Keeping `SystemTime` and accepting the difference:** the same file
  would hold documents one platform reads and another calls damaged.
- **`chrono`'s or `time`'s type as the variant's:** a dependency in the
  public API, and the choice between them the application's (§69.5).

## 70.2 Also fixed
The earliest `i64` second, read back from its text, overflowed: the
whole days before it, times 86,400, are more seconds than an `i64`
holds, before the time of day brings it back in range. The sum is now
taken in `i128`. A test that writes and reads both ends of the range
found it.

## 70.3 What it breaks
`Document::DateTime`'s payload changed type, released an hour before in
0.14.0: code that built `Document::DateTime(system_time)` writes
`Document::DateTime(system_time.into())`, or `Document::from(system_time)`,
which works for either. Released as 0.15.0.

## 70.4 Tests
- `datetime.rs`: `SystemTime` to `DateTime` and back for times every
  platform holds, before and after 1970, and half a second before; time
  order across the whole `i64` range; RFC 3339 written and read, now
  also for the first and last `i64` second, and one day past the last
  refused; `Debug` and the parse error's text.
- `serde_bridge.rs`: `SystemTime` fields as before, with times every
  platform holds; a `DateTime` field before 1970, a billion seconds
  before, and to the nanosecond, stored as a date-time and read back; a
  stored `SystemTime` read into a `DateTime` and back; `serde_json`
  seeing the text.
- `collection.rs`: the end-to-end test in whole 100 ns, so it means
  the same on Windows.
- Checked by breaking it on purpose: the seventeen ways of §69.6, those
  in `datetime.rs` and the index key rewritten for the new type, and
  four more. Each fails a test:
  - the seconds summed in `i64`; `to_system_time` dropping the
    nanoseconds;
  - a `DateTime` field read like any other value, or stored as its
    text.
