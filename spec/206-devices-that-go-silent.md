# 206. Devices that go silent (`GUIDE.md`, `bindings/`)

Step 4 of §202, the last: the recipe in the GUIDE, and the bindings
tested against the clock. Nothing in the library changes.

## 206.1 The GUIDE's "Devices that go silent"
- **Both models of §202.3,** each with its code: a document per device,
  its `last_seen` set in the same batch as each reading, watched with
  `older_than`; and a live aggregate on the readings alone, grouped by
  device, with `newer_than` in its filter.
- **Why the first:** the devices already silent come first; `Entered`
  holds the device's last document; an index on `last_seen` finds the
  next with one seek, and each time one goes silent the live query
  reads again the devices silent then (§204.3), not every device's
  readings. The aggregate never reports a device silent before it
  started, and reads ten minutes of every device's readings each time
  one ages out.
- **What each costs, as it is** (found in review): the GUIDE first said
  the document per device reads one index entry per device that goes
  silent; it reads every device silent at that moment, through the
  index.
- **Whose clock** (§202.6): store the time a reading arrived where
  devices' clocks can't be trusted.
- **From another process** (§205): the same beside the application,
  and `trunkdb watch` with the filter.
- "The last five minutes, live" points to it: five minutes back from
  the clock is a filter, read again as time passes; `window` counts
  back from the newest reading.
- Both examples compiled and run against the library as written.

## 206.2 The bindings
The filters cross as text (§202.8): nothing in the C ABI, C# or Python
changes. Their READMEs say a filter against `now` is read again as time
passes, and the agent skill says `trunkdb watch` reports it.

## 206.3 Tests
- **C#,** `A_live_query_against_now_sees_a_device_go_silent`
  (`LiveTests.cs`): a device seen now, an index on `lastSeen`, a live
  query `lastSeen < now - 300ms` and a live aggregate's count with
  `>=`, in the writer's process: nothing at first, then `Entered` with
  the device as written, never before its moment, and the count falls
  to 0. Then, read-only beside it: `Entered`, `Left` when the writer
  updates the device, `Entered` again as the writer left it.
- **Python,** `test_a_live_query_against_now_sees_a_device_go_silent`
  (`test_live.py`): the same, `watch_every` 20 ms beside the writer.
- They run with the bindings' CI jobs: C# on three systems, Python 3.11
  and 3.14 on three.

## 206.4 Rejected
- **`clock_every` in the bindings' options:** without an index a live
  query against the clock is read again every second, which the
  recipe's index makes moot. Asked for, it is one more option, as
  `watch_every` is in Python.
