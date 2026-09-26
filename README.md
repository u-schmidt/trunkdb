# trunkdb

[![CI](https://github.com/u-schmidt/trunkdb/actions/workflows/ci.yml/badge.svg)](https://github.com/u-schmidt/trunkdb/actions/workflows/ci.yml)

An embedded, single-file, schema-less document database — a learning-first,
from-scratch reimplementation of the [LiteDB](https://www.litedb.org/) idea
in Rust. No tables, no joins, no schema migrations: open a file, get
collections of documents.

**Status: v0 core is real and usable.** Page storage, the catalog, a real
B-tree primary index, document encoding (both the untyped `Document` path
and a serde bridge so any `T: Serialize + DeserializeOwned` works),
scan/filter/sort/limit queries (`find`, `find_one`, `count`, a streaming
`cursor`), `upsert`, secondary indexes (also unique, on nested paths like
`address.city`, multikey on array elements like `tags[*]`, compound on
several fields like `(status, created)`, and sparse for fields few documents
have),
conditions on array elements and on whether a field exists, overflow pages for documents larger
than a page, export/import as JSON Lines, a checksum on every page so a
damaged one is an error instead of garbage, compaction that rebuilds the file
into as few pages as it needs, a page-image write-ahead log making
every write crash-safe, and atomic multi-collection batch writes
(typed and untyped) with real rollback are all real and tested — not stubs. Still deliberately out of scope for v0:
a cost-based query planner, and concurrent writers (`Database` is a
cloneable, thread-safe handle; reads share a lock, writes take it one
at a time — though the page cache still lets readers through one at a
time, SPEC §58). See [DESIGN.md](DESIGN.md) for how it works,
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
├── collection.rs      Collection<T>: CRUD, find, indexes, update_many
├── batch.rs           Batch: typed atomic multi-op writes
├── cursor.rs          Cursor: streaming find
├── query.rs           Filter, Condition, Sort; the planner
├── document.rs        Document and DocId: the schema-less value type
├── serde_bridge.rs    any T: Serialize + DeserializeOwned <-> Document
├── json.rs            tagged JSON <-> Document, lossless
├── id.rs              IdGenerator + UuidV7Generator
├── catalog.rs         collections and their indexes (page 1)
├── data.rs            documents on data pages, overflow chains
├── storage/
│   ├── mod.rs         PageStore, page types
│   ├── file.rs        FileStore: the file, checksums, staged and
│   │                    committed pages, the file lock
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
│   ├── wal.rs         WalDurability: the page-image write-ahead log
│   └── noop.rs        NoopDurability: a fake, for tests
├── txn/
│   ├── mod.rs         TransactionManager trait, WriteOp
│   └── global_lock.rs GlobalLockTxnManager: one batch at a time
├── decode.rs          reading bytes from disk: running out is an error
├── crc32.rs           CRC-32C, in hardware where the CPU has it
├── export.rs          Database::export/import (JSON Lines)
├── check.rs           Database::check: consistency check
├── compact.rs         Database::compact: rebuild, file shrinks
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
use trunkdb::{Database, IndexOptions, query::Filter};

#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    email: String,
    team: String,
    age: i64,
    nick: Option<String>,
}

let db = Database::open("app.trunkdb")?;
let users = db.collection::<User>("users");
users.ensure_unique_index("email")?; // once at startup; a no-op after
users.ensure_index("age")?;
users.ensure_index(["team", "age"])?; // compound: by team, in age order
let sparse = IndexOptions { sparse: true, ..IndexOptions::default() };
users.ensure_index_with("nick", sparse)?; // no entries for users without a nick

users.insert(User { name: "Ada".into(), email: "ada@example.com".into(), team: "core".into(), age: 36, nick: None })?;

let oldest_adults = users.find(Filter::new().gte("age", 18).sort_desc("age").limit(10))?;
let youngest_in_core = users.find(Filter::new().eq("team", "core").sort_asc("age").limit(5))?;
let by_team_then_age = users.find(Filter::new().sort_asc("team").then_asc("age").limit(20))?;
```

## Building

```
cargo build
cargo test
```

## Command line

```
cargo install --path .
trunkdb info app.trunkdb                 # format, pages, collections, indexes
trunkdb check app.trunkdb                # consistency check; exit code 1 on problems
trunkdb compact app.trunkdb              # rebuild into as few pages as needed
trunkdb export app.trunkdb backup.jsonl  # JSON Lines, or to standard output
trunkdb import copy.trunkdb backup.jsonl # `-` reads standard input
```

## Performance

Not fast yet, and measured so it can get there. `bench/` runs one
document workload against trunkdb, SQLite, redb and sled: same
documents and ids, same three indexes, durable commits everywhere, and
the same answers checked across all four. On an M1 Pro with 100,000
documents:

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert, 1000 per commit | 20k/s | 35k/s | 37k/s | 28k/s |
| one commit | 4.6 ms | 4.6 ms | 4.8 ms | 9.5 ms |
| get by id | 3.9 µs | 4.4 µs | 1.8 µs | 2.2 µs |
| 1000 documents by index | 2.4 ms | 1.7 ms | 1.0 ms | 2.4 ms |
| oldest 20 of a status | 47 µs | 23 µs | 22 µs | 20 µs |
| file size after compact | 33 MB | 22 MB | 30 MB | — |

Each gap has a known cause (SPEC §48, in [spec/](spec/README.md)), and the first four are
fixed: "oldest 20" took 19 ms before the lazy B-tree walk (§49), a
lookup by id 32 µs before the page cache (§50), a commit 16 ms before it
flushed once instead of three times (§51), and batched inserts ran at
7.5k/s before index pages were changed in place (§52). What's left:
checkpoints in large batches, decoding in scans, and a copy per page
read.

```
cd bench && cargo run --release
cd bench && cargo run --release --bin reader_wait   # readers while a writer commits
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

What's still open: [ROADMAP.md](ROADMAP.md).

## License

Licensed under the [MIT License](LICENSE).
