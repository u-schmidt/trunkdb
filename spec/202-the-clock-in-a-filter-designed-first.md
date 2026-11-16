# 202. The clock in a filter, designed first (`query.rs`, `query_text.rs`, `live.rs`, `database.rs`)

**Designed 2026-11-16; step 1 built the same day (§203), step 2
(§204) and step 3 (§205); where they changed what this says, §203.5,
§204.5 and §205.5 list it.** It takes up the first deferred
item of "Changes as they happen" in ROADMAP.md, a window counted back
from the clock (§139.2), now that a program asks for it. Its four
questions were answered the same day, each as proposed (202.10).

A tracker sends a device's position every one or two seconds. A device
standing still keeps sending the same position; a device switched off,
out of range or broken sends nothing. The application must tell the
two apart: "offline" is a fact about time passing with nothing
written.

No result trunkdb can keep today changes without a commit. A live
query or aggregate is checked when a batch commits (§138.2, §139.1), or,
beside a writer, when a look finds the WAL grown (§195.1). Both windows
count back from a document, not the clock: `window` from the newest of
all, so a silent device ages out only while the others report on
(§139.2), and `window_per_group` from the group's own newest, so a
silent device keeps its last readings for ever (§174.2). A filter can't
say "the last ten minutes" either: `at >= <a date-time>` is fixed when
the filter is made.

## 202.1 A condition on the clock, not a window
A comparison's value may be `now`, or `now` and a duration before or
after it:

```text
last_seen < now - 10m        seen last more than ten minutes ago
at >= now - 5m               the last five minutes
due < now + 1h               due within the hour, or overdue
```

In Rust, two conditions name the common case, and one the rest:

```rust
Condition::older_than("last_seen", Duration::from_secs(600))  // last_seen < now - 10m
Condition::newer_than("at", Duration::from_secs(300))         // at >= now - 5m
Condition::against_now("due", Op::Lt, Offset::after(Duration::from_secs(3600)))
```

`older_than` and `newer_than` split every date-time between them: the
one is `<`, the other `>=`, so a document is in exactly one of the
two, the instant on the edge newer. As `Filter` methods too
(`Filter::new().older_than(..)`), as for every condition.

Why a condition rather than `Aggregate::window_from_now`:

- **Everything that takes a filter takes it:** `find`, `count`,
  cursors, snapshots, `aggregate`, live queries and live aggregates,
  batches' filters, the command, and the bindings, which pass filters
  as text (`Collection.Find(string? filter)` in C#,
  `find(filter: str)` in Python), so neither changes.
- **A window from the clock is this condition** in the aggregate's
  filter: `at >= now - 5m`. An aggregate needs nothing new.
- **Indexes bound it** as any range, once `now` is read (202.2).

## 202.2 `now`, read when a read starts
- **Once per read:** a read takes the time once, from `SystemTime`, and
  every `now` in its filter is that time. A comparison against `now`
  is then an ordinary comparison against a `DateTime` before the read
  is planned, as a parameter is bound before it (§163). The planner,
  the index ranges, `explain` and the matchers see nothing new.
- **A date-time only:** `now - 10m` is a `DateTime` (§69), and
  compares with date-times alone. A field holding a number of seconds,
  or text, never matches it, as now.
- **A snapshot reads its data as of its commit, and the time as of the
  read:** a `Snapshot` kept for an hour and asked `at >= now - 5m` gives
  the readings of its commit that are within five minutes of the time
  it is asked. A snapshot is of data, not of the clock.
- **Printed as written:** `Display` and the text keep `now - 10m`, so a
  filter read and printed comes back the same (§111). `explain` shows
  the time it read as well, since the plan was made for it.
- **`Aggregate::apply` and `Filter::apply`** read the time when called.
  Tests and the fuzz target's model set it (202.9).
- **Past the ends of time:** `now - d` reaching past the earliest
  date-time there is stops there, as `DateTime::back` does (§139.2); a
  `now + d` past the latest stops there.

## 202.3 Two models of "offline"
**A document per device (proposed in the GUIDE).** The application
keeps one document per device beside its readings, with `last_seen`
set by each reading, in the same batch. Then:

```rust
let offline = devices.watch(Filter::new().older_than("last_seen", ten_minutes))?;
for event in offline {
    match event? {
        Live::Entered { id, doc, .. } => { /* gone silent; doc has last_seen */ }
        Live::Left { id, .. } => { /* back */ }
        _ => {}
    }
}
```

- The first events are the devices already silent when it starts.
- `Entered` holds the device's document, its last position and time.
- An index on `last_seen` makes each check one range read over the
  devices, not over the readings.

**On the readings alone.** A live aggregate, `group_by("device")` with
the filter `at >= now - 10m`: a device that stops reporting ages out
and its row `Left` with `None`; when it reports again it is `Entered`.
It needs no second collection, but it never reports a device silent
before it started, and each check reads ten minutes of every device's
readings.

Both work. The GUIDE shows the first, and says why.

## 202.4 Checked again as time passes
The second source of events §139.2 put off:

- **In the writer's process,** a commit checks each changed document
  against its live queries as now, with `now` read at the commit.
  Between commits a thread, `trunkdb-clock`, one per database, wakes
  at the next moment a live query's or aggregate's result can change
  without a write (202.5). For each one due it reads the matches again
  and compares them with the last, as a reader beside a writer does
  (§195.1), and reports what came, went or changed, under the
  writer's lock. That orders its events with the commits' events, at
  the cost of waiting for a batch being committed: late, never lost.
- **Beside a writer,** the look that already runs every
  `OpenOptions::watch_every` (§195.1) also reads again a live query
  whose moment has come, with or without a commit.
- **A read-only open with no writer** has had no thread so far: its
  live queries report their first state (§194.7). A live query against
  `now` starts the look there too, since time passes with no writer.
- **The thread runs while a live query against `now` does:** started by
  the first, ended when none is left, holding the database weakly, as
  §195's look.
- **A live query without `now` is never read again by the clock.**

## 202.5 The next moment
A comparison of a path `p` against `now - d` changes for a document at
one moment: when the time reaches `p + d`. Until the soonest such
moment, nothing can change:

- **Against `now - d`,** for `<`, `<=`, `>` and `>=` alike, the
  soonest moment belongs to the document with the smallest `p` at or
  after `now - d`: `<` gains it then, `>=` loses it. Through an index
  on `p` that is one seek.
- **Against `now + d`,** the same with `p - d`.
- **`=` and `!=` against `now`** hold for one nanosecond at most; they
  are refused when the filter is made, in code and in the text.
- **Several such conditions:** the soonest of their moments.
- **Without an index on `p`:** the soonest moment is unknown without
  reading every document. The thread then reads the query again every
  `OpenOptions::clock_every` (1 s by default) instead.
- **Read again, a result may not change:** a moment comes for a
  document the rest of the filter leaves out. The reread compares and
  reports nothing, as §195 does for a commit that changed nothing it
  shows.
- **A burst:** readings every second leave a ten-minute window every
  second, so a live aggregate on the readings is read again about as
  often as it is by commits. Moments closer than a millisecond are
  taken together.

## 202.6 Whose clock
- **The host's,** `SystemTime::now()`, in the process that reads. A
  reader beside a writer uses its own clock, not the writer's; on one
  machine they are the same.
- **A device's clock is not trunkdb's business.** A position stamped by
  a device whose clock is off ages by the host's clock all the same. The
  GUIDE says to store the time a reading arrived (`DateTime::now()`
  when it is written) and compare that, where devices' clocks can't be
  trusted.
- **The clock jumps:** the thread sleeps at most a second at a time and
  reads the clock again, so a jump forward reports what aged out within
  a second, and a jump back reports what came back, as any change.

## 202.7 What is reported
- **The events as they are:** `Live` and `LiveRow`, their variants and
  fields, the C ABI's JSON (§194) and the bindings' records. An event
  from the clock looks as one from a commit.
- **A live query's `Changed` never comes from the clock:** time moves a
  document in or out, and doesn't change it. A live aggregate's row
  can change, a count falling as readings age out.
- **Several moments between two looks come as one,** beside a writer,
  as several commits do (§195.2).

## 202.8 What it doesn't change
- **The file format:** a filter isn't stored.
- **Filters without `now`:** read, planned, matched and checked live as
  before; nothing reads the clock for them.
- **The ABI:** filters cross as text; `trunkdb_watch` takes `now` with
  nothing new. The ABI version stays.

## 202.9 Steps, a section each
| Step | What | Seen from outside |
|---|---|---|
| 1 | `now` in conditions: the text, `older_than`, `newer_than`, `against_now`; read once per read and bound before planning; `explain` showing the time; `=`/`!=` refused; a clock the tests and the fuzz target's model set | `find`, `count`, `aggregate` and the command with `now` |
| 2 | The clock in the writer's process: `trunkdb-clock`, the next moment through an index, `clock_every` without one, a reread under the writer's lock | live queries and aggregates that see devices go silent |
| 3 | Beside a writer and alone: the look reads again at a moment too, and runs in a read-only open without a writer | `trunkdb watch`, C# and Python beside the application |
| 4 | The GUIDE's "devices that go silent", both models (202.3); C# and Python tests of a live query against `now` | the recipe |

## 202.10 The questions, decided
Each was decided on 2026-11-16 as proposed.

1. **A condition (proposed), or `Aggregate::window_from_now`?** The
   condition reaches every read (202.1); the window would reach
   aggregates alone.
2. **The names:** `older_than`, `newer_than` and `against_now`, and
   `now ± duration` in the text (proposed)? Durations are written as in
   `window at 5m` (§139).
3. **Without an index on the path:** read again every `clock_every`
   (proposed), or refuse a live query against `now` without one?
4. **A snapshot's `now`:** the time of the read (proposed), or the time
   of the snapshot's commit, which would make a kept snapshot's
   answers repeatable but "the last five minutes" of an hour-old
   snapshot an hour old?

## 202.11 Rejected
- **A heartbeat:** the application writing every device's state on a
  timer so a commit comes. It writes to say nothing, and it is the
  timer this section puts in the library.
- **The application polling,** `find` every few seconds and a diff of
  its own. It works today. The library knows when the next change can
  happen (202.5) and has the diff (§195.1).
- **Events marked as from the clock:** a device silent is silent
  whatever noticed it. Asked for, a field can come later.
- **A deadline per document,** a heap of every document's moment: one
  seek per query finds the soonest, and only the soonest matters.
- **`now` in an update** (`set last_seen = now`): a separate need, met
  by `DateTime::now()` in the program today.

## 202.12 Limits
- **As punctual as the thread:** in the writer's process, the moment
  plus the wait for a batch being committed; beside a writer, up to
  `watch_every` later; without an index, up to `clock_every` later.
- **Not repeatable:** the same documents give other answers at other
  times. A test sets the clock.
- **As §138.7:** events wait in memory without limit, and are lost if
  the process stops.
