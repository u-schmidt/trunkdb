# 204. Live queries read again as time passes (`live.rs`, `database.rs`, `query.rs`, `aggregate.rs`)

Step 2 of §202: in the writer's process, a live query or aggregate that
compares with the clock is taken, and a thread, `trunkdb-clock`, reads
it again at the next moment its result can change with nothing
written. A device gone silent is reported with nothing written.

```rust
let silent = devices.watch(Filter::new().older_than("last_seen", Duration::from_secs(600)))?;
for event in silent {
    match event? {
        Live::Entered { id, doc, .. } => { /* silent ten minutes: doc as last written */ }
        Live::Left { id, .. } => { /* reported again */ }
        _ => {}
    }
}
```

## 204.1 Taken in the writer's process
- **`watch` and `watch_aggregate`** take a comparison with `now` in the
  filter, a column's condition or `having`, where §203.4 refused it.
- **One time for the first read:** the clock is read once, the first
  matches read against it, and the time kept with the live query.
- **`=`, `!=`, `contains`, `starts with` against `now`:** refused as by
  any read, an `InvalidInput` error, before anything is kept.
- **A read-only open refuses it,** beside a writer or alone, with
  `Error::NotLive`, "a comparison with `now` in a read-only open, not
  yet": nothing there reads it again as time passes until step 3.

## 204.2 The next moment
- **One seek for each comparison** (§202.5): for `field < now - d` and
  `field >= now - d`, the soonest date-time at or after the edge, `field
  >= now - d`, sorted by `field`, limit 1; for `<=` and `>`, the
  soonest after it. Run through `find`'s own path, it reads one index
  entry where an index orders `field`. The moment is that date-time
  plus `d` (minus, for `now + d`), and for `<` and `>=` a nanosecond
  after, as those change once the edge is past it, `<=` and `>` at it.
- **Unbounded,** read again `every` `OpenOptions::clock_every`, 1 s by
  default: where no index orders the field (the planner's own test,
  `index_order`, decides), a path with `[*]`, a comparison inside
  `has`, whose path starts at an element, and one in `having`, on
  rows.
- **The soonest of all a live query's comparisons,** in its filter, a
  NOT, an AND or an OR; an aggregate's filter's and its columns'.
  `LATEST` where every edge has passed every document: nothing to wait
  for until a write.
- **Found anew** at the start, after each reread, and after each commit
  that writes to the collection, as a document written may be the
  soonest; there never later than the one kept, as the commit checked
  only the documents it wrote: one read again `every` apart is still
  due `every` after its last whole read, however often the collection
  is written to.

## 204.3 Read again
- **The thread, `trunkdb-clock`:** started by the first live query or
  aggregate against the clock, ended when it finds none left, holding
  the database weakly, as `trunkdb-live` (§195). Under the writer's
  lock it reads the clock, reads again every one whose moment has come,
  and sleeps until the soonest of the rest: at most 1 s, so a clock set
  forward is seen within a second, and at least 1 ms, so moments closer
  than that are read for together.
- **A bell:** a commit or a new live query whose moment is sooner than
  the thread would wake rings it, apart from the database, so a
  sleeping thread keeps nothing open.
- **What is reported:** without a sort, the ids matching against the new
  time, compared with those kept: `Left` for each gone, with the
  document as it is, then `Entered` for each come. With a sort, the
  list read again, and the documents that came or went handed to the
  same `settle` a commit's go through (§145.2), so the places are those
  a commit's events would name. No `Changed`: time moves a document in
  or out and changes none. A live aggregate runs again whole, and its
  rows compared, so a count may change (§139).
- **At a commit,** the clock is read once where any live query compares
  with it; each document written is checked against that time, then
  each whose moment has come is read again at it, so the commit's
  events come first, and a document written isn't reported twice.
- **A clock set back:** found earlier than the time last read, it reads
  again, so what had aged out and is back is reported.

## 204.4 `OpenOptions::clock_every`
How often an unbounded one is read again (204.2). Default 1 s. Only a
live query or aggregate against the clock uses it; nothing reads the
clock for others.

## 204.5 Changed from §202
- **The 1 ms** of §202.5 ("moments closer than a millisecond are taken
  together") is the thread's shortest sleep.
- **A read-only open refuses** a live query against `now` until step 3,
  which §202.9 left unsaid.
- **`having`'s comparisons are unbounded:** §202.5 spoke of documents'
  paths; `having`'s are on rows, read again every `clock_every`.

## 204.6 Rejected
- **A sleeping thread holding the database:** a `Condvar` paired with
  the writer's lock would keep the database open, and its files locked,
  for up to a second after the program let go of it. The bell is
  apart.
- **Each due moment read alone,** for a live query only the documents
  that crossed: the soonest is known, the others aren't without a read
  per moment, and the whole read is what a sorted list needs anyway.

## 204.7 Tests
- `tests/clock_live.rs`, ages of 300 ms, each event asserted never
  before its moment and given 10 s after it:
  - a device that goes silent is reported, `Left` when it reports
    again, silent again later, through an index;
  - an index finds the moment: with `clock_every` an hour, watched before
    any device is there;
  - the silent ones come first, then each at its moment, in order;
  - `newer_than` loses each as it ages;
  - without an index, and inside `has`, every `clock_every`; and
    without an index while another device reports every 20 ms;
  - a sorted list with a limit: the older out from 0, the newer in at 0;
    and one that only loses, each leaving as it ages;
  - as a commit reports: a `select` cuts it, nothing more comes;
  - a live aggregate per device: a count falls, `Changed`, then the row
    `Left` with none;
  - a column's condition, and `having`, every `clock_every`;
  - commits and the clock take turns: a write entered by its commit,
    an unrelated write, a delete;
  - a read-only open refuses it; `= now` is refused.
- `the_next_moment_is_one_seek_away` (`live.rs`): the moment for `<`,
  `<=` and `now + 10m`, the edge on a document, none ahead, no index,
  inside `has` on a field of an indexed name; the seek reads one key.
- `a_clock_set_back_is_due` (`live.rs`).
- `tests/clock.rs`: a live query and aggregate against `now` taken, the
  first events the silent ones; nested in `has`, a NOT, an OR.
- **The `queries` fuzz target** watches its filters and aggregates
  with the clock too, where §203 counted them refused; no edge comes
  near a value while a program runs, so each reports what the model
  has. 1,000 programs of seed 7 made 709 such live queries and
  aggregates, all as the model says.

## 204.8 Mutations
| Mutation | Caught by |
|---|---|
| `<=` and `>` seek from the edge itself | the next moment |
| No nanosecond after for `<` and `>=` | the next moment |
| The offset moved the wrong way | the next moment |
| A comparison inside `has` sought | the next moment |
| No index used, `clock_every` always | the next moment |
| A write doesn't find the moment anew | an index finds the moment, a device that goes silent |
| A write puts the moment later | writes more often than `clock_every` |
| The clock's reread reports no `Left` | `newer_than` loses them, inside `has` |
| The clock's reread reports no `Entered` | a device that goes silent, and six more |
| A sorted list not settled by the clock | a sorted list moves |
| A sorted list's gone ones not noted | a sorted list only loses |
| A clock set back not seen | a clock set back |
| An aggregate's columns give no edge | a column's condition |
| `having` gives no edge | `having` against the clock |
| The thread reads none again | every test of the clock but the refusals |
| A read-only open takes it | a read-only open refuses it |
| `= now` taken by a live query | `= now` is refused |
| No clock kept for a live query | nine tests |

Three survive:
- **A commit not rerunning an aggregate whose moment has come:** the
  thread reruns it at the same moment.
- **The first read resolved by `find` itself,** a few microseconds after
  the time kept: only a document crossing its edge between the two
  could tell, and the first reread puts it right.
- **The bell never rung:** the thread then wakes within a second
  anyway; only a timing test, which would be flaky, could tell.
