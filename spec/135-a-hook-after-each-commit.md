# 135. A hook after each commit (`changes.rs`, `database.rs`, `catalog.rs`)

The time-series workload (§5.1) asks when a position enters or leaves a
geofence. The application works it out itself, after each write it
makes. That works only where it makes every write itself, and
ROADMAP.md plans live queries and live aggregates that need to know,
inside trunkdb, which documents a commit changed. This is the first of
those steps, useful alone: a program learns what each commit changed,
as SQLite's `sqlite3_update_hook` and MongoDB's change streams tell it.

```rust
let changes = db.subscribe()?;
std::thread::spawn(move || {
    for commit in changes {
        for change in &commit.changes {
            match change {
                Change::Inserted { collection, id, .. } => { /* … */ }
                Change::Updated { collection, id, .. } => { /* … */ }
                Change::Deleted { collection, id, .. } => { /* … */ }
                Change::Dropped { collection, .. } => { /* … */ }
                _ => {}
            }
        }
    }
});
```

## 135.1 The API
- **`Database::subscribe() -> Result<Changes>`**: a subscription of its
  own. `Error::Poisoned` as for any other call.
- **`Changes`**: `recv()` waits for the next report, `try_recv()` takes
  one if it is there, `recv_timeout(d)` waits at most `d`. Each gives
  `Option<Commit>`. It is an `Iterator` of `Commit`s too. Dropping it
  ends the subscription.
- **`Commit`** (`#[non_exhaustive]`): `changes`, a `Vec<Change>`, never
  empty.
- **`Change`** (`#[non_exhaustive]`, each variant too, §117):
  `Inserted`, `Updated` and `Deleted`, each with `collection` and `id`,
  and `Dropped` with `collection`. `collection()` and `id()` read them
  without a `match`; `id()` is `None` for `Dropped`.

The ROADMAP's sketch had a struct with a `kind`. An enum is what Rust
reads with a `match`. A dropped collection has no id, which a struct
would have to hold as an `Option` in every change.

## 135.2 What a report holds
**One report per batch,** in commit order. A batch stays whole for
whoever reads it: the 50 writes of one `Batch` are one report, and so
are the documents one `update_many` changed. Each call that writes is
one batch, `import` one per chunk it writes.

**Each document once, with the batch's net effect,** in the order the
batch first touched it:

| Writes in one batch | Reported |
|---|---|
| insert | `Inserted` |
| update | `Updated` |
| delete | `Deleted` |
| insert, then update | `Inserted` |
| update, then delete | `Deleted` |
| insert, then delete | nothing |
| delete, then insert the same id | `Updated` |

A report is the difference between two states, the commit before and
this one. Live queries compare exactly that: a document that came to
match, or no longer does.

It needs no reading of the documents. A batch's first write to a
document says whether it was there before: an insert fails where one
is, an update or a delete fails where none is, and a failed write
fails the batch. Its last write says whether it is there after. So
`ChangeLog` keeps two flags for each document.

`Updated` says the document was written, not that it changed. Two
updates that end where they started, or an update to the same
contents, are still `Updated`. `update_many` and `update_fields`
already leave out the documents they would write unchanged, so those
aren't reported.

**`drop_collection` is one change,** `Dropped`, not one per document.
Dropping a collection of 100,000 documents doesn't make a report of
100,000.

**Ids only, no documents.** The reader reads what it needs. A newer
commit may have changed the document since, which a reader that wants
the present state is content with.

**Not reported:** an index built or dropped, `compact`, `checkpoint`,
`check`, a failed batch, and a batch that changed no document, such as
one that inserted a document and deleted it, or a `delete_many` that
found nothing.

## 135.3 When it is sent
After the batch is in the WAL and published: durable, and seen by new
reads. A `get` on the subscriber's thread of an id the report names
finds the batch's document, or a later one. A crash before the report
is sent is a crash after the commit: the batch is there after the
restart, and its report is lost (135.6).

Still under the writer's lock, so the reports go out in commit order.
Sending takes no time worth counting: the channel never waits.

`subscribe` takes the writer's lock too. So it waits for a batch
under way to commit, and the first report is of the first batch that
begins after it. No batch is half reported.

## 135.4 How it is built
- **Recorded in the batch's catalog.** Each batch works on its own copy
  of the catalog, published if it commits and dropped if it fails
  (§78). It already holds what the batch did to the free-space map
  (§75). `ChangeLog` sits beside it, so a failed batch drops its record
  with the rest. It is taken out before the catalog is published:
  readers never see it.
- **Every write of a document goes through `apply_write_op`**: `Batch`,
  `insert`, `update`, `delete`, `update_many`, `update_fields`,
  `delete_many`, `upsert`, and `import`'s batches. It records each
  write once it has succeeded. `drop_collection` records its own.
  `compact` copies documents without it, and is reported as nothing.
- **Nothing is recorded without a subscriber.** A batch records only if
  a subscription is open as it begins. A subscriber dropped is noticed
  at the next report, whose send fails, and forgotten then.
- **One `mpsc` sender for each subscriber**, kept with the writer. Each
  gets a clone of the report.

## 135.5 Rejected
- **A callback** run inside the commit. It would run under the writer's
  lock, so one that writes would wait for the writer it runs in. A
  channel is read on the application's own thread, which then reads and
  writes as it likes.
- **A bounded channel that waits when full.** A subscriber that writes
  waits for the writer, which waits for the subscriber's channel to
  have room. That is a deadlock. Unbounded, a writer never waits, and a
  reader far behind costs memory instead (135.6).
- **A bounded channel that drops reports,** and says so, as tokio's
  `broadcast` does with `Lagged`. A live query that lost a report would
  have to start over. Without a reader that falls behind, nothing asks
  for it yet.
- **Every write in order,** not the net effect. Live queries would have
  to fold them into the net effect themselves.
- **A snapshot of the commit with each report,** so a reader sees
  exactly the state the report describes. It would keep the commit's
  pages in memory for as long as the report waits (§80), and a reader
  that falls behind would make `snapshot_memory` end other snapshots.
- **A subscription for one collection.** A program reading only one
  skips the others' changes. A filter on what is reported is the next
  step's job: live queries.
- **`crossbeam` or an async channel.** A public dependency is a
  decision (ROADMAP.md, "A settled API"). `std`'s `mpsc` is enough for
  a thread that waits, and an async program can wait for it on a
  thread of its own.

## 135.6 Limits
- **A report is lost if the process stops between a commit and its
  send,** or before the subscriber reads it. Where an event must not
  be lost (a message sent once), write it as a document in the same
  batch as the change that caused it, and work those off apart: the
  transactional outbox. Both or neither are in the file.
- **No report of what happened while the program was down.** Nothing
  did: the file is locked to one writing process (§21).
- **Reports wait in memory without limit.** A subscriber that never
  reads, but isn't dropped, keeps every report.
- **`Updated` for a write that changed nothing,** as 135.2 says.
- **Iterating ends only when the database is dropped:** every
  `Database` clone and every handle on it, a `Collection` among them. A
  subscriber thread that holds one, to read with, keeps its own loop
  going. `try_recv` or `recv_timeout` let it stop on its own terms.
- **A read-only open commits nothing,** so it reports nothing.

## 135.7 Tests
- `changes.rs`: the net effect of every order of insert, update and
  delete, across two collections, in the order first touched; a
  collection dropped, ending its documents' entries; `collection()`
  and `id()`.
- `tests/changes.rs`:
  - Each way of writing reported as its batch: `insert`, `update`, a
    `Batch` across two collections, `update_many`, `update_fields`,
    `upsert` inserting and updating, `delete_many`, `delete`,
    `drop_collection`.
  - A batch's net effect, and a batch that left everything as it was
    reported as nothing.
  - Not reported: an index built and dropped, `compact`, `checkpoint`,
    `check`, writes that found nothing, an update of a missing id, a
    collection that wasn't there, two failed batches.
  - Every subscriber gets each commit after it, a second through a
    clone of the database; one dropped doesn't stop the others.
  - Four writer threads, 400 inserts: the subscriber's thread finds
    each document its report names, and each writer's inserts come in
    its own order.
  - A subscriber that writes on its own thread, once for each report:
    no deadlock, and its writes reported too.
  - Iterating ends when the database is dropped, after the reports
    waiting.
  - A typed collection, `import`, a read-only open.
- `changes_are_recorded_only_for_a_subscriber` (`database.rs`): no
  record without a subscription, a record with one, none in the
  published catalog; a subscription dropped is forgotten at the next
  report, and recording stops.
- **The `queries` fuzz target** (§105) subscribes after its prelude, and
  again at each reopen. After every step it takes what was reported:
  nothing for a step that writes no document, at most one commit for
  one that does. `Inserted` is exactly the ids the model gained,
  `Deleted` exactly those it lost, and `Updated` only ids it kept,
  among them every one whose contents changed. Each id once. In 1,000
  programs of the smoke run, 17,054 documents were reported, all as
  the model says.

## 135.8 Mutations
| Mutation | Caught by |
|---|---|
| Updates not recorded | the net effect |
| Every document counted as new at its first write | the net effect |
| Inserted and deleted in a batch reported as `Deleted` | the net effect; recording only for a subscriber |
| A later write to a document ignored | the net effect; recording only for a subscriber |
| A drop keeping the collection's earlier entries | a collection dropped |
| `drop_collection` not recorded | every write reported |
| `apply_write_op` not recording | recording only for a subscriber |
| Recording without a subscriber | recording only for a subscriber |
| The record published with the catalog | recording only for a subscriber |
| A report of a batch that changed nothing | recording only for a subscriber |
| Subscribers dropped kept | recording only for a subscriber |
| Changes in id order, not the order first touched | a collection dropped |
| Only the first subscriber kept | every subscriber gets each commit |
| `try_recv` never giving a report | every write reported, and four more |

A report sent before the batch is published, right after the log, is
caught by the four writer threads: the subscriber looks up an id it
was told of and finds nothing. That depends on timing, and caught it
in 3 runs of 3.
