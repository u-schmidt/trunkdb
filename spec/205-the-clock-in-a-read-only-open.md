# 205. The clock in a read-only open (`live.rs`, `database.rs`)

Step 3 of §202: a read-only open takes a live query or aggregate
against the clock, as the writer's process does (§204), beside a writer
in another process and alone. An admin tool, `trunkdb watch`, or a C#
or Python program beside the application sees a device go silent with
nothing written.

```text
trunkdb watch fleet.trunkdb devices "last_seen < now - 10m"
```

## 205.1 Beside a writer
- **The look reads again at a moment too:** the thread that looks every
  `OpenOptions::watch_every` for a commit (§195.1) reads the clock once
  for the look, where any live query or aggregate compares with it.
  Where the writer committed, every one is read again, as before,
  those against the clock at that time. Where it didn't, those whose
  moment has come are, and no other.
- **The next moment as in the writer's process** (§204.2), found on the
  snapshot the look read: one seek through an index, else every
  `clock_every`. Found anew after every reread, which is whole, so
  every document is checked at the new time.
- **Every read of a look before anything is reported,** the seeks for
  the next moments with the rereads: a snapshot the writer asks to let
  go under any of them (`Error::SnapshotTooOld`, §189) has the whole
  look tried again at the next, as §195 does, and no live query ended.
  Found in review: the seeks came after the reports, and one ended
  there ended its live query.
- **The first read** takes the time once, reads the matches against it,
  and keeps the time and the matches to compare with, as §195 keeps them.
- **What is reported** is §195.2's: the matches read again compared with
  the last, each that came, went or changed handed to the same
  `changed` and `settle`; a live aggregate run again whole.
- **No `trunkdb-clock` thread:** the look keeps time. A moment is seen
  up to `watch_every` late (§202.12).

## 205.2 Alone
A read-only open is beside a writer, whether one is open or not,
wherever the lock files are there or can be made (§185.6): after the
writer closed, the look keeps time as 205.1 says. One in a directory it
may not write, where no writer made them, is alone: it commits nothing
and has no look. There a live query against the clock starts
`trunkdb-clock` (§204.3), which reads it again at its moments on the
state the open read; without the clock, a live query reports its first
state only, as before (§194.7).

## 205.3 Whose clock
The reading process's (§202.6). A writer and a reader on one machine
read the same.

## 205.4 Nothing else changes
`Error::NotLive` is back to a skip and a limit without a sort. The ABI,
the C# and Python bindings and `trunkdb watch` pass the filter as text,
and take `now` with nothing new.

## 205.5 Changed from §204
- **A read-only open takes it** (§204.1 refused it until this step).

## 205.6 Rejected
- **`trunkdb-clock` beside a writer as well:** two threads reading the
  same snapshots, each under the writer's lock, one for commits and one
  for moments. The look already reads the snapshot every
  `watch_every`; it reads a reread's worth more only when a moment has
  come.
- **Every look reading those against the clock again:** a full read per
  live query every 100 ms, where one seek says nothing can have changed.

## 205.7 Tests
- `tests/clock_live.rs`, beside a writer opened apart in the same
  process, looking every 5 ms:
  - a device goes silent, through an index, with `clock_every` an hour;
    `Left` when the writer has it report again, and silent again later,
    as the writer left it;
  - without an index, every `clock_every`;
  - a live aggregate per device ages out: a count falls, then the row
    leaves;
  - a look reads no document for a live query or aggregate whose moment
    hasn't come and whose collection nothing was written to, before the
    device goes silent and after, with nothing ahead, while another
    live query is read again every look: custom conditions count what
    they are given;
  - a live query against the clock started after the look read the
    writer's last commit;
  - after the writer closed; and alone, in a directory the reader may
    not write (Unix, not as root): a live query and a live aggregate
    against the clock.
- `tests/live_beside.rs` and the rest of `tests/clock_live.rs`, as
  before.

## 205.8 Mutations
| Mutation | Caught by |
|---|---|
| The look ignores a moment come | beside a writer: the device, without an index, the aggregate, the look |
| Every one against the clock read again when one is due | a look before the moment |
| No clock kept beside a writer | beside a writer: the device, without an index, the look |
| No next moment found at the first read | a live query started later |
| The look reads no time | beside a writer: six tests |
| A reread not at the look's time | beside a writer: the device, the look |
| A live query not moved to the look's time | beside a writer: the device, without an index, the look |
| A live query's moment not found anew | a look before the moment |
| An aggregate's rows not at the look's time | the aggregate, after the writer closed |
| An aggregate not moved to the look's time | a look before the moment |
| An aggregate's moment not found anew | a look before the moment |
| A live query's or aggregate's moment found, not kept | a look before the moment |
| A read-only open alone refuses it | alone |
| No `trunkdb-clock` thread at all | alone, and eight tests in the writer's process |

Three survive:
- **The first read resolved by `find` itself,** as §204.8 says.
- **A seek for the next moment that fails as too old ending the live
  query:** only a snapshot ended between a look's reread and its seek
  could tell, which no test can time.
- **`trunkdb-clock` beside a writer too:** it would read again against
  the state the open began with, but the look, every 5 ms in the
  tests, reads first at each moment and leaves it nothing due; only a
  race between the two threads could tell.
