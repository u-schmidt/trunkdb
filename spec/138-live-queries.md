# 138. Live queries (`live.rs`, `database.rs`, `collection.rs`)

The time-series workload (§5.1) asks when a position enters or leaves a
geofence. §135 tells a program which documents each commit changed,
and §136 lets a filter hold the application's own geometry. This step
joins them: a `Filter` kept as a subscription, which reports the
documents that come to match it, those that no longer do, and those
written that still do. Other databases call them live queries (Realm,
Firebase, RethinkDB's changefeeds) or continuous queries.

With a position kept in one document of state, updated with each ping,
entering a fence is that document coming to match, and leaving it the
document no longer matching. The live query needs no state of the
application's.

```rust
let live = vans.watch(Filter::new().eq("device", "van-7").and(in_depot))?;
for event in live {
    match event? {
        Live::Entered { id, doc, .. } => { /* came to match */ }
        Live::Left { id, doc, .. } => { /* no longer matches; doc None if deleted */ }
        Live::Changed { id, doc, .. } => { /* written, still matches */ }
        _ => {}
    }
}
```

## 138.1 The API
- **`Collection::watch(filter) -> Result<LiveQuery<T>>`**, on a typed
  collection as on one of `Document`. It waits for a batch being
  committed to finish, as `subscribe` does.
- **`LiveQuery<T>`** reads as `Changes` does (§135.1): `recv`,
  `try_recv`, `recv_timeout`, each an `Option<Result<Live<T>>>`, and
  an `Iterator` of `Result<Live<T>>`. Dropping it ends the query. One
  channel for each live query: a hundred fences are a hundred of them.
- **`Live<T>`** (`#[non_exhaustive]`, each variant too, §117):
  `Entered`, `Left` and `Changed`, each with `id` and `doc`; `Left`'s
  `doc` is an `Option`. `id()` reads the id without a `match`.

The ROADMAP's sketch had `Live::Left(van)`, the document alone. A
deleted document has none, and an application must still know which
van left: every event carries its id.

## 138.2 Checked inside the commit
After a batch is durable and published, still under the writer's lock,
each live query on a collection the batch changed checks the
documents it changed: the ids §135's `ChangeLog` records, now kept
while a subscriber *or* a live query is open. Each document is read
once from the snapshot just published, however many live queries
watch its collection, and checked against each filter there.

So every transition is seen, in commit order, each against the state
its own commit left. Checked later, on the reader's thread from
§135's reports, a newer commit may have moved the van on already: an
entry and a leaving between two reads would both be lost.

At a ping every second or two, reading and checking the one document
changed costs nothing worth counting, so no incremental view
maintenance (DBSP, TimescaleDB's continuous aggregates) is needed.

## 138.3 What matched before
Each live query keeps the set of ids that match as of the last commit.
A document the commit changed is checked; with what the set says:

| Before (in the set) | Now (matches) | Reported |
|---|---|---|
| no | no | nothing |
| no | yes | `Entered`, added to the set |
| yes | no | `Left`, taken out of it |
| yes | yes | `Changed` |

A deleted document matches nothing. One id for each document that
matches: few for a fence. Reading each document's version before the
commit instead would cost a second read for each change.

**The state first:** `watch` takes the writer's lock, reads what
matches now, and hands each document as `Entered` before anything else,
in the order `find` reads them, the set made of them. A van already in the depot at the
start is reported so. No commit falls between that read and the first
commit checked.

**A batch by its net effect** (§135.2): a van moved into the depot and
out again in one batch is nothing, as nothing matches after it.

## 138.4 What is handed out
- **The document as its commit left it,** the position that entered
  the fence and not a later one, which §135's ids can't give. `Left`
  holds it, or `None` if it was deleted.
- **`select` (§106) cuts it,** at the start and after.
- **A typed `T` is converted on the reader's thread,** as it is
  received, never under the writer's lock. A document that doesn't
  convert is an `Err` for it, and the query goes on.
- **`Changed`, though a fence needs only the other two:** what a live
  list on a screen needs ("the vans in the depot, and their speed"),
  and it costs nothing more. As §135's `Updated`, it means written, not
  changed: `update_fields` leaves out what it would write unchanged,
  `update` doesn't.

**Not live: `sort`, `skip`, `limit`.** A window of the first matches
moves with every write. `watch` refuses them with
**`Error::NotLive`**, saying which; nothing is registered. A hint
(`use_index`, §132) steers the first read only, and one that can't be
followed fails the `watch` as it fails a `find`.

## 138.5 What ends one
- **A panic in a `Condition::custom`** as a commit is checked: that
  live query receives `Error::ClosurePanicked` and ends, as its set can
  no longer be trusted. The commit stands, and the other live queries
  and subscribers go on (§136.2). A panic in the first read fails the
  `watch` itself.
- **A document that can't be read** as a commit is checked: each live
  query on its collection receives an `Error::Io` of its kind and
  message, and ends. (`Error` isn't `Clone`.)
- **Its `LiveQuery` dropped:** noticed at the next commit that changes
  its collection, whose send fails, and forgotten then.
- **The database dropped:** the events waiting are read, then the
  iteration ends.

`drop_collection` ends nothing: each document in the set is `Left`,
without a document, and the query goes on, for a collection made again
under the name. A document inserted there under a dropped one's id is
`Entered`.

## 138.6 Rejected
- **Checking on the reader's thread,** from §135's reports: no cost to
  the writer, but transitions lost between two reads (138.2).
- **Ids only:** a read for each, of a state a newer commit may have
  changed.
- **A callback** (§135.5).
- **The first state handed apart,** as `(Vec<T>, LiveQuery)`:
  `Entered` says the same, in one way.
- **A live `sort` and `limit`**, "the ten fastest vans": a window
  that moves with every write, with a document leaving it when another
  comes in. Asked for, it is a step of its own.
- **A panic taken as no match:** the document would `Left` on a bug.

## 138.7 Limits
- **Unbounded, as `Changes`:** a writer never waits, a reader may
  write, and events not read wait in memory.
- **The check runs under the writer's lock:** each changed document of
  a watched collection is read whole and checked against each of its
  live queries before the next batch can begin. A batch that changes a
  million documents of a watched collection reads a million.
- **A dropped `LiveQuery` is noticed late,** at its collection's next
  change; until then batches still record their changes.
- **Events are lost if the process stops** between a commit and its
  check, or before they are read; the state after a restart is the
  start state of the next `watch`. Where an event must not be lost,
  the transactional outbox (§135.6).
- **A read-only open** reports the start state, and nothing after: it
  commits nothing.
- Left to the application: hysteresis at a fence's edge (GPS noise
  flips a position in and out), and pings a tracker sends late, after
  being offline, applied in the order of their time, not of their
  arrival.

## 138.8 Tests
- `tests/live.rs`:
  - the state first, and a collection not there yet;
  - each write: inserted outside and inside, written and still
    matching, in and out with the document as it is, deleted inside
    and outside; `update_many`, `upsert`, `delete_many`;
  - a batch by its net effect: in and out, inserted and deleted,
    inserted and moved in;
  - a late reader: four commits read at the end, each against its own
    state;
  - other collections not reported, a watcher on each, one batch
    across both;
  - `select` at the start and after;
  - a sort, a skip and a limit refused; a hint followed, and one that
    can't be refused;
  - a fence entered and left through a `Condition::custom`;
  - a panic: that query ends with its message, the commit stands, the
    others go on, the database checks; a panic in the first read;
  - a dropped collection: each match `Left`, the query going on, a
    document under a dropped one's id `Entered`;
  - typed: converted as received, a document that doesn't convert an
    `Err` and the query going on;
  - a reader writing on its own thread for each event, four writers
    beside it;
  - iterating ending with the database; a read-only open.
- `a_live_query_records_changes_until_it_is_dropped` (`database.rs`):
  a watcher alone records, nothing in the published catalog, a dropped
  one noticed at its collection's next change, and not before.
- **The `queries` fuzz target** (§105) makes one or two live queries of
  the program's conditions after its prelude, and again at each reopen,
  the old ones ending with the database. After every step each event
  names a document as the model has it: `Entered` one it didn't
  report and that matches, `Left` one it did that no longer matches or
  is gone, `Changed` one it did that still matches. After them, what
  it reports matching is exactly what matches in the model.
  The smoke run's 1,000 programs reported 234,504 events, 2,000 of
  seed 7 458,167, all as the model says.
  It draws more of each program, so a seed no longer gives the program
  it gave.

## 138.9 Mutations
| Mutation | Caught by |
|---|---|
| The start state not sent | the state first, and five more |
| The start state not in the set | a panic, typed, dropped, select |
| Changes recorded only for a subscriber | every test but four, and `records_changes` |
| Live queries never checked | every test but two, and `records_changes` |
| `Changed` never sent | each write, a fence, a panic |
| `Left` without its document | each write, a late reader, typed, select |
| `Entered` not added to the set | each write, a late reader, a fence, typed, a reader writing |
| `Left` not taken out of it | each write, a late reader, a fence |
| A drop reporting nothing | a dropped collection |
| A drop keeping the set | a dropped collection, made stronger: an id again |
| A panic taken as no match | a panic |
| A panic not ending the query | a panic |
| A limit taken | refused |
| A sort taken | refused |
| `select` ignored | `select` |
| A dropped `LiveQuery` kept | `records_changes` |
| Every live query checked against every collection | other collections, made stronger: a watcher on each |
| A deleted document read as there | typed |

Two survived at first. A drop keeping its set went unseen while no
document came back under a dropped id: one now does, and is `Entered`,
not `Changed`. Checking every live query against every collection went
unseen while only one collection was watched: a commit to the other
was skipped before any query was asked.
