# 82. A snapshot to keep (`snapshot.rs`, `collection.rs`, `cursor.rs`, `database.rs`)

The fifth step of §77's plan, and the first a caller sees. Since §80 a
read keeps its commit for one call. `Database::snapshot` hands that
commit out to keep: every read through it, in every collection, is of
the moment it was taken. A count and the `find` after it agree, which
is the second thing §77.1 asked for. And a cursor now shows one moment,
a change from §29.4.

```rust
let snapshot = db.snapshot()?;
let orders = snapshot.collection::<Order>("orders"); // a View<Order>
let count = orders.count(Filter::new())?;
let all = orders.find(Filter::new())?;                // count == all.len()
snapshot.export(file)?;                               // the same moment
```

## 82.1 `Snapshot`
`Database::snapshot()` returns a `Snapshot`: the database handle and
the commit that was the last when it was called. `Clone`, `Send +
Sync`; every clone is the same moment.

| On a `Snapshot` | As of the snapshot |
|---|---|
| `collection::<T>(name)` | a `View<T>` (82.2); empty if the collection didn't exist |
| `collections()` | the names there were |
| `export(out)` | the export that would have been made then, byte for byte |
| `check()` | the commit checked; the scan for damaged pages is of the file as it is now (§78.6) |
| `file_info()` | the header's facts then |

`Database`'s own `collections`, `export`, `check` and `file_info` are
the same code, on a commit taken for the call: each is split into the
call and a function of a `ReadGuard`, which both use.

**It keeps the database open**, as every handle does (§27): the file
stays locked until snapshots, views and cursors are dropped too.

**It can only read.** What a snapshot's reads lead to is written
through the `Database`, as an ordinary batch (§77.2), and isn't in the
snapshot.

## 82.2 `View<T>`
`snapshot.collection::<T>(name)` gives a `View<T>`: `get`, `find`,
`find_one`, `count`, `cursor`, `explain`, `indexes`, `name`. For any
serde type, and for `Document`, as `Collection`.

- **A type of its own**, not a `Collection` whose writes fail: a view
  has no `insert`, and calling one doesn't compile (a `compile_fail`
  example in its documentation says so). Decided in §77.2.
- **No second read path.** A `View` holds a `Collection` that carries
  the snapshot's commit (`Collection::at`), and passes on its read
  methods. `Collection::read` is the one place that differs: a handle
  with a commit reads that commit (`Database::read_at`), one without
  takes the last (`Database::read`). `retyped`, which `clone` and the
  typed methods' way to `Collection<Document>` go through, carries the
  commit along.
- **The indexes are the snapshot's too**: `explain` and `find` use the
  ones it had, not one made since.

## 82.3 What keeping costs
A kept commit is a snapshot open in §80's sense, found by the same
`Arc` count, with nothing new in the storage layer:
- **memory**: a page changed since stays as it was, once, until the
  last holder lets go (§80.3). No limit yet: §83;
- **`compact` is refused**, `Error::SnapshotOpen` (§80.5);
- **reads of it hold no gate.** A read on the last commit holds the
  gate so that a compaction waits for it. A kept commit refuses
  compaction for as long as it exists, so its reads have nothing to
  wait out. Only taking it holds the gate, for the moment of taking
  (`Database::pin`): a compaction that has found no snapshot kept can't
  be handed one before it has committed.

## 82.4 A cursor shows one moment
§29.4's cursor took the candidates' ids when it was made and read each
document when it got there, each at its own moment: one deleted
meanwhile was skipped, one updated was seen as updated. It was "read
committed per document, not a snapshot", and holding the read lock for
the cursor's life was rejected for good reasons.

Now a cursor keeps the commit of its creation and reads everything
from it. A document deleted meanwhile is still handed out, one updated
meanwhile as it was, one inserted meanwhile not at all. No lock is
held: writes go on, also in the loop that iterates it.

- **It keeps locations, not ids.** §29.4 rejected that because a slot
  freed by a delete can be taken by another document. Within one commit
  it can't. So an item costs a page read, and no longer a lookup in the
  primary index first.
- **A cursor is a kept snapshot** until it's dropped: memory, and no
  `compact` (82.3). A sorted cursor reads everything when it's made
  (§34.2) and keeps nothing.
- **`find_one`** is a cursor of one item (§29.2), so it takes a commit
  and lets it go within the call.

Breaking, for code that iterates a cursor and relies on seeing its own
writes to documents not yet reached. Decided in §77.2; no such code is
known.

## 82.5 Tests
- `snapshot.rs`:
  - one snapshot, then updates, deletes, inserts, an index made and one
    dropped, a collection made and one dropped, a checkpoint: `count`,
    `find`, `get`, `find_one`, `cursor`, `indexes`, `explain`,
    `collections`, `check` and `file_info` all answer as of the
    snapshot, typed and untyped; the database and a snapshot taken
    afterwards have the newer state;
  - a clone, a view and a view's clone are the same moment, each
    keeping it after the others are dropped;
  - a snapshot's export, after deletes and inserts, is byte for byte
    the export made when it was taken;
  - a snapshot, its clone, a view, a cursor: each alone refuses
    `compact`, the database writes meanwhile, and with the last dropped
    it compacts;
  - with the database handle dropped, a view still reads and the file
    can't be opened again; with the view dropped it can;
  - a reader that counts, sleeps and lists on a snapshot, forty times,
    beside a writer inserting and deleting back to back: the two agree
    every time, also through an index, the snapshots move on, and the
    writer isn't held up;
  - a poisoned database refuses a view's reads, a snapshot's, and a new
    snapshot;
  - `Snapshot` and `View<T>` are `Send + Sync`, whatever `T` is.
- `collection.rs`:
  - a cursor hands out a document deleted after its creation, one
    updated as it was, and not one inserted, with the deleted one's
    slot taken by another and all of it checkpointed; a cursor made
    then has the new state. This test asserted the opposite before;
  - an open cursor refuses `compact` and reads on; a sorted one
    doesn't.
- Two examples in the documentation run as tests: taking a snapshot,
  and a view's `insert` not compiling.
- Checked by breaking it on purpose, nine ways, each failing a test: a
  view reading the last commit; a typed view losing its commit on the
  way to the documents; a view given the commit of now; a view's cursor
  taking a commit of its own; a snapshot's `export`, `collections`,
  `check` and `file_info` of the last commit; a kept commit read on a
  poisoned database; a cursor reading each document as it is now.

## 82.6 Limits
- **No limit on the memory a kept snapshot takes**: §83. Until then a
  snapshot forgotten beside a busy writer grows without bound.
- **A cursor left open** is such a snapshot, and less visibly so than
  one named `Snapshot`.
- **Taking a snapshot holds the gate for a moment**, so it waits for a
  `compact` in its commit.
- **The read path of §80.8** is as it was: with kept snapshots
  countable now, `compact` could tell them from reads under way without
  the gate. Not done here.
- **No write through a snapshot**, and no read and write as one unit
  (§77.2).
- **CI doesn't run the documentation's examples**: `cargo test
  --all-targets` leaves them out. They were run by hand.
