# trunkdb

[![CI](https://github.com/u-schmidt/trunkdb/actions/workflows/ci.yml/badge.svg)](https://github.com/u-schmidt/trunkdb/actions/workflows/ci.yml)

An embedded, single-file, schema-less document database — a learning-first,
from-scratch reimplementation of the [LiteDB](https://www.litedb.org/) idea
in Rust. No tables, no joins, no schema migrations: open a file, get
collections of documents.

**Status: v0 core is real and usable.** Page storage, the catalog, a real
B-tree primary index, document encoding (both the untyped `Document` path
and a serde bridge so any `T: Serialize + DeserializeOwned` works),
scan/filter/sort/skip/limit queries (`find`, `find_one`, `count`, a streaming
`cursor`), `upsert`, secondary indexes (also unique, on nested paths like
`address.city`, multikey on array elements like `tags[*]`, compound on
several fields like `(status, created)`, and sparse for fields few documents
have),
conditions on array elements and on whether a field exists, overflow pages for documents larger
than a page, export/import as JSON Lines, a checksum on every page so a
damaged one is an error instead of garbage, compaction that rebuilds the file
into as few pages as it needs, a page-image write-ahead log making
every write crash-safe, atomic multi-collection batch writes
(typed and untyped) with real rollback, snapshots, read-only opens, and
`explain_run` and `stats` to see how a query ran and where a file's
space goes are all real and tested — not stubs. Still deliberately out of scope for v0:
a cost-based query planner, and concurrent writers (`Database` is a
cloneable, thread-safe handle; reads run together, writes one at a
time beside them, and neither waits for the other, but for a `compact`,
SPEC §80). See [DESIGN.md](DESIGN.md) for how it works,
[the spec](spec/README.md) for why, and [ROADMAP.md](ROADMAP.md) for progress.

## Why

Coming from C#, [LiteDB](https://www.litedb.org/) was the go-to storage
layer for small personal projects — single file, no setup, document-shaped
data instead of tables/joins/schema versions. LiteDB is .NET-only, and this
project isn't a port of it: it's an independent design, built to (a) get
that same easy embedded-document-store experience outside the .NET world,
and (b) actually learn how a database is built, from the page layer up.

## Design principles

- **Clean layer boundaries over completeness.** Every subsystem (storage,
  indexing, transactions, durability, query execution) sits behind a small
  trait. A layer is allowed to be a deliberately naive placeholder
  implementation as long as the trait's shape is one a serious
  implementation can later drop into without changes rippling upward.
- **Feature-poor is fine; unclear boundaries are not.** v0 intentionally
  skips a real query planner and concurrent writers —
  see [the spec](spec/README.md) for the full list and why each one is deferred
  rather than missing.
- **Own decisions, not a translation.** LiteDB (and the Rust embedded DBs
  `sled`, `redb`, `PoloDB`) are references for calibration, not code to
  port.

## Project layout

```
DESIGN.md              how it works today, one subsystem at a time
spec/                  why: one numbered section per step (SPEC §N)
ROADMAP.md             what's done, what's open
src/
├── lib.rs             public re-exports, the crate's Error type
├── database.rs        Database: open, recovery, the commit protocol,
│                        checkpoints, OpenOptions
├── collection.rs      Collection<T>: CRUD, find, indexes, update_many,
│                        update_fields
├── batch.rs           Batch: typed atomic multi-op writes
├── cursor.rs          Cursor: streaming find, of one moment
├── snapshot.rs        Snapshot and View<T>: the database at one
│                        moment, to keep reading
├── query.rs           Filter, Condition, Sort; the planner; index
│                        entries deciding a filter
├── path.rs            field paths, parsed once per query
├── explain.rs         explain and explain_run: how a query ran
├── stats.rs           Database::stats: where a file's space goes
├── update.rs          Update: set, unset and inc on dotted paths
├── document.rs        Document and DocId: the schema-less value type
├── datetime.rs        DateTime, what Document::DateTime holds
├── serde_bridge.rs    any T: Serialize + DeserializeOwned <-> Document
├── serde_stored.rs    a stored document straight into T, for typed reads
├── json.rs            tagged JSON <-> Document, lossless
├── id.rs              IdGenerator + UuidV7Generator
├── catalog.rs         collections and their indexes (page 1)
├── data.rs            documents on data pages, overflow chains
├── free_space.rs      FreeSpace: data pages with room, by free bytes
├── storage/
│   ├── mod.rs         PageStore, page types
│   ├── file.rs        FileStore: the writer's staged pages, the
│   │                    header, the file lock
│   ├── pages.rs       Pages: the file, checksums, the committed
│   │                    pages, shared by readers and the writer
│   ├── page.rs        Page: a page's bytes, shared, copied on change
│   ├── snapshot.rs    SnapshotStore: the pages as of one commit
│   ├── slotted.rs     SlottedPage: slot directory + cells
│   ├── cache.rs       PageCache: CLOCK eviction
│   └── memory.rs      MemoryStore: pages in memory (compaction, tests)
├── index/
│   ├── mod.rs         Index trait
│   ├── btree.rs       BTreeIndex: every index, primary and secondary
│   ├── key.rs         index key encoding, KeyRange
│   ├── leaf.rs        leaf entry encoding
│   ├── branch.rs      branch entry encoding
│   └── in_memory.rs   InMemoryIndex: a fake, for tests
├── durability/
│   ├── mod.rs         Durability trait
│   └── wal.rs         WalDurability: the page-image write-ahead log
├── txn/
│   ├── mod.rs         TransactionManager trait, WriteOp
│   └── global_lock.rs GlobalLockTxnManager: one batch at a time
├── decode.rs          reading bytes from disk: running out is an error
├── crc32.rs           CRC-32C, in hardware where the CPU has it
├── export.rs          Database::export/import (JSON Lines)
├── check.rs           Database::check: consistency check
├── compact.rs         Database::compact: rebuild, file shrinks
├── fuzzing.rs         hooks for fuzz/, behind a feature, hidden
└── bin/trunkdb.rs     the `trunkdb` command (info, check, compact,
                         export, import)
tests/                 the command line, and a time-series-shaped
                         workload through the public API
bench/                 against SQLite, redb and sled (its own crate)
fuzz/                  fuzz targets: damaged files, WALs and exports
                         (its own crate, nightly)
```

## Usage

```rust
use serde::{Deserialize, Serialize};
use trunkdb::{Database, DocId, IndexOptions, query::{Filter, Update}};

#[derive(Serialize, Deserialize)]
struct User {
    #[serde(rename = "_id")]
    id: Option<DocId>, // None on insert: one is made; filled in on every read
    name: String,
    email: String,
    team: String,
    age: i64,
    nick: Option<String>,
    tags: Vec<String>,
}

let db = Database::open("app.trunkdb")?;
let users = db.collection::<User>("users");
users.ensure_index_with("email", IndexOptions::new().unique())?; // once at startup; a no-op after
users.ensure_index("age")?;
users.ensure_index(["team", "age"])?; // compound: by team, in age order
users.ensure_index_with("nick", IndexOptions::new().sparse())?; // no entries for users without a nick

let id = users.insert(User { id: None, name: "Ada".into(), email: "ada@example.com".into(), team: "core".into(), age: 36, nick: None, tags: vec![] })?;
let ada = users.get(&id)?.unwrap(); // ada.id == Some(id)
let same = users.find_one(Filter::new().id(id))?; // a lookup, like get
let parsed: DocId = id.to_string().parse()?; // ids go to text and back

let oldest_adults = users.find(Filter::new().gte("age", 18).sort_desc("age").limit(10))?;
let youngest_in_core = users.find(Filter::new().eq("team", "core").sort_asc("age").limit(5))?;
let by_team_then_age = users.find(Filter::new().sort_asc("team").then_asc("age").limit(20))?;
let third_page = users.find(Filter::new().sort_asc("name").skip(40).limit(20))?;

let renamed = users.update_fields(Filter::new().eq("team", "core"), &Update::new().set("team", "platform").inc("age", 1))?;
let tagged = users.update_fields(Filter::new().id(id), &Update::new().add_to_set("tags", "admin").pull("tags", "guest"))?;
```

### Reads that agree with each other

Each call reads the database as it is at that moment, so a write on
another thread can land between two calls. A snapshot is one moment
for as many reads as you like, in every collection, and holds up
nobody:

```rust
let snapshot = db.snapshot()?;
let users = snapshot.collection::<User>("users"); // a View<User>: reads only
let how_many = users.count(Filter::new())?;
let everyone = users.find(Filter::new())?; // how_many == everyone.len(), always
snapshot.export(std::fs::File::create("backup.jsonl")?)?; // the same moment
```

Drop it when the reads are done: while it's open, pages changed since
stay in memory as they were, and `compact` is refused. A `cursor` is a
snapshot of its own, for as long as it's iterated.

That memory has a limit, 256 MiB unless `OpenOptions::snapshot_memory`
says otherwise. A snapshot left open beside so many writes that it's
passed is ended, the oldest first: its reads return
`Error::SnapshotTooOld`, and a new snapshot reads on. Writes are never
held up or refused for it. `db.snapshot_info()` shows how much is kept.

### Reading only some fields

A collection can be opened with a smaller type: serde ignores the
fields it doesn't declare, so this hands back only names and teams, as
LINQ's `Select` or a SQL column list would:

```rust
#[derive(Serialize, Deserialize)]
struct UserSummary {
    name: String,
    team: String,
}

let summaries = db.collection::<UserSummary>("users").find(Filter::new().eq("team", "core"))?;
```

Read through it, never write: `update`, `upsert` and `insert` store the
whole value, so a `UserSummary` written back replaces the user and
drops every field it lacks. To change some fields, use `update_fields`.
A typed read decodes each document straight into the type, so the
fields a summary doesn't declare are skipped, not built (SPEC §99).

## Building

```
cargo build
cargo test
```

## Command line

```
cargo install --path .
trunkdb info app.trunkdb                 # format, pages and sizes, collections, indexes
trunkdb check app.trunkdb                # consistency check; exit code 1 on problems
trunkdb compact app.trunkdb              # rebuild into as few pages as needed
trunkdb export app.trunkdb backup.jsonl  # JSON Lines, or to standard output
trunkdb import copy.trunkdb backup.jsonl # `-` reads standard input
```

`info`, `check` and `export` open the file read-only: they change
nothing in it or in its WAL, even after a crash of the program that had
it open.

## Performance

Reads are level with SQLite, redb and sled or ahead of them; writes and
compaction are still behind. `bench/` runs one document workload
against trunkdb and the three others: same documents and ids, same three
indexes, durable commits everywhere, and the same answers checked
across all four. On an
M1 Pro with 100,000 documents, the median of three runs (0.19.0 with
§90–§104):

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert, 1000 per commit | 22k/s | 42k/s | 37k/s | 28k/s |
| one commit | 4.7 ms | 4.9 ms | 5.0 ms | 10.2 ms |
| get by id | 2.5 µs | 17 µs | 2.1 µs | 3.1 µs |
| 1000 documents by index | 0.98 ms | 2.1 ms | 1.2 ms | 2.1 ms |
| count of 1000 by index | 0.01 ms | 0.03 ms | 0.04 ms | 0.28 ms |
| count of a range of 1000 | 0.01 ms | 0.02 ms | 0.03 ms | 0.37 ms |
| count of all 100,000 | 0.09 ms | 0.02 ms | 0.00 ms | 25 ms |
| oldest 20 of a status | 19 µs | 25 µs | 16 µs | 22 µs |
| page 50 of 20 of a status | 27 µs | 58 µs | 47 µs | 277 µs |
| first 50 by id | 33 µs | 40 µs | 25 µs | 33 µs |
| 50 by id after skipping 5000 | 40 µs | 133 µs | 160 µs | 1.1 ms |
| a tenant's first 20 by id | 23 µs | 1.3 ms | 21 µs | 23 µs |
| scan, 100,000 for a fifth | 64 ms | 49 ms | 52 ms | 70 ms |
| update, 1000 per commit | 9.1k/s | 12k/s | 17k/s | 21k/s |
| delete, 1000 per commit | 15k/s | 17k/s | 21k/s | 33k/s |
| compact | 1.1 s | 0.19 s | 0.44 s | — |
| file size after compact | 31 MB | 22 MB | 30 MB | — |

Both counts by index take about 15 µs (SPEC §104.8). Where reads stand:
- **Through an index, reads are fast:** a count reads no document, and
  counts a leaf at a time where its ends decide it (§100, §101, §104).
  A page by index reads only the page, and a deep one skips leaves
  whole. A sort by `_id` walks the primary index, or the range an `Eq`
  fixes (§96, §103).
- **Still behind:**
  - the count of all documents, which trunkdb counts leaf by leaf where
    the others keep a total;
  - a scan, 1.3× SQLite: each document's filtered fields are decoded
    into a small object to be checked;
  - "first 50 by id", 1.3× redb.
- **Writes are about half as fast:** nearly every batch of 1,000 crosses
  the default checkpoint threshold once the indexes are large (§53).
  Compaction inserts its entries one by one (§48.4), and its file is
  1.4× SQLite's.

How it got here, earliest first: "oldest 20" took 19 ms before the lazy
B-tree walk (§49); a lookup by id 32 µs before the page cache (§50); a
commit 16 ms before one flush per commit (§51); batched inserts 7.5k/s
before index pages were changed in place (§52). In October 2026,
§96–§104 took "first 50 by id" from 189 ms to 33 µs, a scan from 168 ms
to 64, a count by index from 0.9 ms to 0.01, page 50 of 20 from 1.2 ms
to 27 µs, a tenant's first 20 by id from 0.6 ms to 23 µs, and a count of
a range from 0.14 ms to 0.01. `explain_run` (§94) says how any query of
yours ran.

```
cd bench && cargo run --release
cd bench && cargo run --release --bin reader_wait   # readers while a writer commits
cd bench && cargo run --release --bin long_read     # one long snapshot beside single writes
```

## Roadmap (short version)

Done so far (see [ROADMAP.md](ROADMAP.md) for the full list, and
[spec/](spec/README.md) for the reasoning):

1. **Correctness and file format**: a page-image WAL (SPEC §19),
   several documents per data page (§20), a file lock and format
   version (§21), and three data-risk fixes found in review (§22).
2. **A small app replacing MongoDB**: `find` returning ids (§23), a
   typed batch API (§24), case-insensitive substring search (§25).
3. **A desktop app with large documents**: overflow pages (§26), a
   thread-safe `Database` handle (§27), secondary indexes (§28), and
   `find_one`/`count`/`upsert`/`cursor` (§29) → 0.3.0.

4. **Export/import** as JSON Lines (§30): a backup you can read, and the
   migration path between file format versions.
5. **Nested-field paths** (§31): `address.city` in filters, sorts and
   `ensure_index`.
6. **Null and missing fields** (§32): `x == null` finds both, through an
   index too (file format 4).
7. **Unique indexes** (§33): `ensure_unique_index("email")`; file format
   5, which still opens format-4 files as they are.
8. **Sorting through an index** (§34): "the newest 20" reads 20
   documents, not all of them.

   Items 4–8 → 0.4.0.
9. **A filter builder** (§35): `Filter::new().eq("status", "Complete")`.
10. **OR, NOT and nesting** (§36): `.any_of([...])`, `|`, `&`, `!` —
    an OR of indexed values reads just those index ranges.
11. **`delete_many` and `drop_collection`** (§37): delete by filter
    (sort and limit included), drop a collection and free its pages.

    Items 9–11 → 0.5.0.
12. **`update_many`** (§38): change what a filter finds with a closure,
    `|task| task.status = ...`, in one atomic batch.
13. **The `trunkdb` command** (§39): `info`, `check`, `export`, `import`;
    `Database::check` finds leaked pages and indexes that disagree.
14. **Page checksums** (§40): a damaged page is an error, not garbage,
    and `check` names it; file format 6 — older files move up by export
    and import.

    Items 12–14 → 0.6.0.
15. **Compaction** (§41): `db.compact()` or `trunkdb compact` rebuilds
    the file into as few pages as it needs; index leaves now fill when
    keys come in order.
16. **Array conditions and multikey indexes** (§42): `eq("tags[*]",
    "rust")`, `gt("comments[*].likes", 10)`, `ensure_index("tags[*]")`;
    file format 7, which still opens format 6 as it is.

    Items 15–16 → 0.7.0.
17. **Compound indexes** (§43): `ensure_index(["status", "created"])`
    finds by status and reads in creation order at once;
    `ensure_unique_index(["tenant", "email"])`; file format 8.
18. **Sparse indexes** (§44): `ensure_index_with("nick", IndexOptions {
    sparse: true, .. })` leaves out null and missing values, for fields
    few documents have; used where the filter rules nulls out.

    Items 17–18 → 0.8.0.
19. **`Exists` and array size** (§45): `exists("nick")` tells a stored
    null from a missing field, `missing("team")` finds documents written
    before it; `size("tags", Op::Eq, 0)` finds empty arrays. `Op` and
    `Condition` are now `#[non_exhaustive]`.
20. **Conditions on one element** (§46): `elem_match("lines",
    Condition::eq("sku", "A1") & Condition::gte("qty", 9))` needs one line
    to meet both; multikey indexes like `lines[*].sku` bound it.
21. **Sorting by several fields** (§47): `sort_asc("status").then_desc("created")`;
    an index serves the leading keys its fields match, the rest are
    sorted in memory. `Filter.sort` is now a `Vec<Sort>`.

    Items 19–21 → 0.9.0.
22. **Benchmarks** (§48): `bench/` against SQLite, redb and sled; the
    measured gaps order what comes next.
23. **A lazy B-tree walk** (§49): reading in sort order stops at the
    limit, forward or backward — "oldest 20" 150–200× faster.
24. **A page cache** (§50): 256 MiB by default, `OpenOptions::cache_size`
    to change it; lookups by id 4× faster.

    Items 22–24 → 0.10.0.
25. **One flush per commit** (§51): pages are written back at a
    checkpoint every 8 MB, not every commit; a commit takes 4.6 ms
    instead of 16. `db.checkpoint()` makes the file complete on its own.
26. **B-tree pages changed in place** (§52): an index insert writes one
    cell instead of rebuilding its page; batched inserts 3× faster,
    compaction 7×.
27. **A configurable checkpoint threshold** (§53):
    `OpenOptions::checkpoint_pages` trades memory and WAL size for
    fewer write-backs in large batches.
28. **Pages checked when read** (§54): a page whose slots don't fit its
    layout is an error saying what is wrong, not a panic in the host app.

    Items 25–28 → 0.11.0.
29. **Fuzzing** (§55): `fuzz/` feeds trunkdb damaged databases, WALs and
    exports; fourteen ways one crashed or hung it are fixed, each with a
    test.
30. **A limit on nesting** (§56): documents nest at most 64 levels, so a
    damaged one can't overflow the stack, and every export imports again.
31. **Concurrency, decided** (§57): one writer, readers in parallel;
    snapshot reads deferred, with rules that keep them possible.
32. **Reader waits, measured** (§58): paced writers cost readers
    nothing; back-to-back commits starve them; the page cache's mutex
    keeps readers from running in parallel.
33. **A struct carries its own id** (§59): `#[serde(rename = "_id")] id:
    Option<DocId>` is filled in on every read and used on insert; the id
    is stored once, in the cell (file format 9, 6% smaller files);
    filters on ids now match; `find_with_ids` is deprecated.
34. **A smaller public API** (§60): the internals are private; one
    `Batch` for typed and untyped collections; `Filter` and
    `IndexOptions` built with methods; `indexes()` lists what each index
    is; limits as constants on `Document` and `Database`; `Error`,
    `Document` and the reports `#[non_exhaustive]`.

    Items 29–34 → 0.12.0: file format 9 (still opens 6–8), and breaking
    API changes; SPEC §59 and §60 say what moved where.

35. **Filters on the id through the primary index** (§61):
    `eq("_id", id)`, or an OR of ids, is a lookup like `get`, not a scan
    — 3.9 µs instead of 67 ms on 100,000 documents.
36. **An index key for ids** (§62): an index on a reference, a `DocId`
    field, finds its documents, sorts by it and keeps it unique; ids
    sort after strings, by when they were made. File format 10: an
    older file's indexes are checked once at open, and those holding
    ids rebuilt.
37. **A filter on the id, and ids parsed from text** (§63):
    `Filter::new().id(id)` and `Condition::id(id)` instead of spelling
    `"_id"`; `"…".parse::<DocId>()`, with `ParseIdError`.

    Items 35–37 → 0.13.0: file format 10 (still opens 6–9, and rebuilds
    an older index that holds ids at the first open); no breaking API
    change.

38. **Shared pages** (§64): a read gets the page cache's page instead
    of a copy, and a change copies it first. One reader is 30% faster;
    two readers now do 1.7 times the work of one, where before they did
    less.
39. **A read lock for the page cache** (§65): readers look pages up
    together. Four readers do 3.2 times the work of one; before §64,
    four did as much as one.
40. **A page checked once** (§66): as it enters the cache or its batch
    commits, not on every read; one reader 14% faster.
41. **`write_page` takes the page** (§67): a changed page is copied
    once, not twice; compaction 11% faster.
42. **Update operators** (§68): `update_fields(filter,
    &Update::new().set(..).inc(..).unset(..))`, on dotted paths, next
    to `update_many`'s closure.
43. **A date-time type** (§69): `Document::DateTime`, what a
    `SystemTime` field stores, to the nanosecond; compared, sorted and
    indexed in time order; `{"$date": "<RFC 3339>"}` in an export
    (export format 2). File format 11, which still opens 6–10.

    Items 38–43 → 0.14.0: file format 11 (still opens 6–10) and export
    format 2 (still imports 1); new API only, no breaking change.

44. **A portable date-time** (§70): `Document::DateTime` holds
    `trunkdb::DateTime`, not a `SystemTime`, so a time reads the same on
    every platform; a `DateTime` field holds times before 1970 too.

    Item 44 → 0.15.0: breaking only for code that builds or matches
    `Document::DateTime` (write `system_time.into()`); files and exports
    as in 0.14.0.

45. **Documents as JSON text** (§71): `doc.to_string()` writes a
    `Document` as tagged JSON (`{:#}` indented), and
    `text.parse::<Document>()` reads it back, with `ParseDocumentError`:
    for tools that work in JSON, through `Collection<Document>`.
46. **Skip, for paging** (§72): `Filter::skip(n)`, after the sort and
    before the limit, in every call that takes a filter; through an
    index, the reading stops after skip plus limit.
47. **More update operators** (§73): `min`, `max` (null counts as
    missing: "the latest seen"), `rename`, and `push`, `add_to_set` and
    `pull` on arrays.

    Items 45–47 → 0.16.0: new API only, no breaking change; files and
    exports as in 0.14.0.

48. **`find_with_ids` removed** (§74), and a `cursor` hands out `T`, as
    `find` does: a type that wants its id declares an `_id` field; one
    from another crate is wrapped, its fields flattened.
49. **A free-space map** (§75): inserts refill the room deletes and
    updates leave in older data pages, fullest page first, so a
    collection deleting and inserting as much stops growing; in memory,
    learned from writes, no format change.
50. **A WAL size limit** (§76): `OpenOptions::checkpoint_wal_bytes`
    writes back once the WAL is that long, twice `checkpoint_pages`
    pages' bytes unless set, so commits that keep changing the same few
    pages cannot grow it without bound; no format change.

    Items 48–49 → 0.17.0: breaking for code that calls `find_with_ids`
    or `find_one_with_id`, or takes `(id, doc)` from a cursor (§74.3
    says how to migrate); files and exports as in 0.14.0.

51. **The committed pages, apart from the writer's** (§77): snapshot
    reads reopened and planned, in six steps; the first splits the
    store, and numbers the commits. Nothing visible.
52. **Reads through a snapshot** (§78): a read gets the last commit's
    catalog and pages, and can't reach a batch being staged. Nothing
    visible.
53. **Writers beside the readers** (§79): a batch is staged, logged,
    flushed and checkpointed without making readers wait; they wait
    only while it is published. Beside a writer committing back to
    back, readers do 18 to 31 times the reads. A commit still waits for
    the reads under way.
54. **Older versions kept for open snapshots** (§80): a read keeps
    reading its own commit, and commits don't wait for it. A writer
    beside an export that never stops: from 132 s for one commit to
    24 ms. `compact` waits for the reads under way; a new error,
    `Error::SnapshotOpen`, for one refused by a kept snapshot.

55. **A failed log, taken back alone** (§81): a batch whose write to the
    WAL fails no longer empties the WAL, which also held the batches
    committed before it and not yet written back; a crash soon after
    lost those. A fix to §51.

56. **A snapshot to keep** (§82): `db.snapshot()` is the database at
    one moment for as many reads as wanted, through `View<T>`, a
    collection that only reads; `export`, `check`, `collections` and
    `file_info` on it too. A `cursor` now shows the moment it was made,
    not each document as it is when reached (breaking for code that
    relied on that).
57. **A limit on the memory snapshots take** (§83):
    `OpenOptions::snapshot_memory`, 256 MiB unless set. A snapshot
    kept open beside so many writes that the old pages pass it is
    ended, the oldest first: its reads fail with
    `Error::SnapshotTooOld`, and writes go on. `db.snapshot_info()`
    says how much is kept.

    Items 50–57 → 0.18.0: breaking for code that relied on a `cursor`
    showing each document as it is when reached, which it shows as it
    was when the cursor was made now (§82); `compact` can fail with
    `Error::SnapshotOpen` while a snapshot or cursor is kept (§80), and
    a read of a snapshot kept past `snapshot_memory` with
    `Error::SnapshotTooOld` (§83); files and exports as in 0.14.0.
58. **A read-only open** (§88): `OpenOptions::read_only(true)` takes a
    shared lock and writes nothing, not even recovery, which it keeps
    in memory. Every write fails with `Error::ReadOnly`. The command's
    `info`, `check` and `export` open read-only.
59. **Damaged or hostile input handled** (§84–§87, §89): `trunkdb
    export` writes through a temporary file and refuses the database or
    its WAL as the destination; damaged header fields are recovered from
    the WAL; an import line is limited to 256 MiB unless
    `ImportOptions::max_line_bytes` says otherwise; a WAL header can't
    grow the file past the pages it holds; a huge `checkpoint_pages`
    no longer overflows the default WAL limit.

    Items 58–59 → 0.19.0: nothing breaking; an import stops at a line
    over 256 MiB now (§86); files and exports as in 0.14.0.
60. **Compaction in a third of the memory** (§90): the WAL record is
    written from the pages instead of built beside them, so compacting
    takes about 1.2× the new file's size instead of 3.3×. A batch over
    4 GiB of pages was logged with its length cut to 32 bits, which
    recovery after a crash misread; §91 lifts the limit.
61. **Crash recovery in the memory of one record** (§91): a batch over
    32 MiB is logged in several records, each flushed before the next,
    and the open after a crash reads the WAL a record at a time. After
    a crash in a compaction to 1.2 GB it took 37 MB, where it took
    2.5 GB. No batch size limit. WAL version 3; a version 2 WAL left by
    a crash is still recovered.
62. **Typed format errors** (§92): an open of a file this build can't
    read at all fails with `Error::Format`: not a trunkdb file, a format
    too old or too new (with the versions), a WAL that isn't trunkdb's
    or of a version it doesn't read. The messages are the same.
    `InvalidData` at an open now means damage only, and `WouldBlock` a
    file another open holds, as before.

63. **Storage statistics** (§93): `db.stats()` says where the file's
    space goes: pages by kind, each collection's documents, bytes, data
    and overflow pages and free room, each index's entries, pages and
    depth, the WAL's size, what the open recovered, and how many pages a
    `compact` would leave, exactly, without compacting. A sixth of what
    `check` costs. `trunkdb info` prints them.

64. **Explain with execution statistics** (§94): `explain_run(filter)`
    runs the query as `find` does and says how: the plan, each index
    read with the conditions that bound it, how many sort keys the index
    gave and how many were sorted in memory, and the index entries and
    documents read, matched and returned, and the time. `find` counts
    the same on every call, at no cost the benchmark shows.

65. **Only objects** (§95): every document is an object, so every
    document carries its id as `_id`. A write of a number, a string, an
    array, a bool or null fails with `Error::NotAnObject`, and a
    `Collection<i64>` with it at its first insert: put the value into a
    one-field struct. File format 12: an older file is checked once at
    its first open, every document's type byte read, and stamped 12; one
    that holds such a document fails the open with
    `FormatError::NotAnObject`, unchanged, and moves up through 0.19.0.

66. **Sorting by `_id` through the primary index** (§96): a sort by
    `_id` with a limit walks the primary index and stops at the limit,
    as a sort through a secondary index does. `gt("_id", last)` starts
    the walk after `last`, so keyset paging is a walk too. The first 50
    of 100,000 by id: 80 µs, where it read and sorted them all in
    189 ms.

67. **A walk that copies only what it hands out** (§97): reading an
    index in order no longer copies every key of a leaf before handing
    out the first. A sorted `find` with `limit(1)` takes 3 µs instead of
    18; the first 50 by id 70 µs instead of 82.

68. **A lock held by a starting process** (§98), documented: on Unix, a
    database dropped while another thread starts a process stays locked
    until that child's exec, and an open in that moment fails with
    `WouldBlock`. Keep or clone the handle instead of reopening, or
    retry briefly.

69. **Decoding only what a read needs** (§99): a candidate is checked on
    the fields its filter and sort name, decoded alone, and only the
    documents handed out are decoded whole, after sort, skip and limit.
    A typed read decodes straight into `T`, no `Document` in between,
    skipping the fields `T` lacks; the same values and errors as before.
    The first 50 by id: 37 µs instead of 69; a scan of 100,000 for a
    fifth of them 62 ms instead of 168; `count` with a filter reads no
    document whole.

70. **Index entries that answer the filter** (§100): where an index
    holds every field a filter names (or `_id`, which every key ends
    with), its entries decide the matches, and a document is read only
    to be handed out. `count` of 1,000 by index reads no document,
    0.17 ms instead of 0.9; page 50 of 20 reads 20, 0.35 ms instead of
    1.2; a skip passes over matches unread, in `find` and in a cursor.
    `IndexRead::covering` in `explain_run` says where entries decided.
    Not for `exists`, `size`, `elem_match` or multikey indexes. Where
    entries decide, a damaged index can change a result until
    `check` finds it.

71. **A cheaper walk through an index** (§101): an index walk lends
    its entries instead of copying them, decides a run of equal values
    once, and counts or skips a leaf inside its range whole where its
    first and last entries decide it. `count()` of 100,000 documents
    takes 0.08 ms instead of 7; `count` of 1,000 by index 0.02 ms
    instead of 0.17; page 50 of 20 26 µs instead of 350; 50 by id after
    skipping 5,000 38 µs instead of 560. Same results, same file.

72. **Paths resolved once per query** (§102): a filter's paths are
    parsed when a read starts, not on every document it checks; a field
    in an object of up to 8 is found by comparing names, without
    hashing; a sort in memory looks each document's keys up once, not
    on every comparison. A count over an index range takes 0.07 ms
    instead of 0.14; "tenant == x, first 20 by id" 0.39 ms instead of
    0.6. Same results.

73. **An `Eq`-fixed index walked in id order** (§103): a sort by `_id`
    with a limit, after `Eq`s that fix every field of the index the
    filter is read by, walks that index's range in id order, forward or
    backward, and stops at the limit, instead of sorting every match in
    memory. Within one value an index's keys differ only in the id they
    end with. `_id` comparisons narrow the walk, so paging by the last
    id works within one value too. "tenant == x, newest 20 by id" takes
    23 µs instead of 380; `explain` says `IndexOrder` for it where it
    said `Index`. Same results.

74. **Index entries matched on their bytes** (§104): where an index
    holds what a filter names, an entry is decided by comparing its
    key's bytes with the conditions' values, encoded the same way,
    instead of decoding it into a document; only `contains` on a string
    and a cut value equal to the condition's still decode. A leaf whose
    ends both match a filter that accepts one interval (comparisons, in
    ANDs, on the index's fields in order) is counted or skipped whole.
    A count over an index range of 1,000 different values takes 15 µs
    instead of 73; SQLite takes 21–30. Same results.

    Items 60–74: not released yet. File format 12, which still opens
    6–11 holding only objects; 0.19.0 refuses a file this build has
    opened. An older build refuses a WAL this one left after a crash.
    Code that caught a foreign or unreadable file as `Error::Io` with
    `InvalidData` now gets `Error::Format` instead. `Collection<T>` for
    a `T` that isn't a struct or a map no longer stores anything.

What's still open: [ROADMAP.md](ROADMAP.md).

## License

Licensed under the [MIT License](LICENSE).
