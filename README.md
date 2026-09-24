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
cloneable, thread-safe handle; reads run in parallel, writes one at a
time). See [SPEC.md](SPEC.md)
for the full design rationale and current progress.

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
  see [SPEC.md](SPEC.md) for the full list and why each one is deferred
  rather than missing.
- **Own decisions, not a translation.** LiteDB (and the Rust embedded DBs
  `sled`, `redb`, `PoloDB`) are references for calibration, not code to
  port.

## Project layout

```
src/
├── lib.rs           public re-exports, crate-level Error/Result
├── document.rs        Document enum + DocId — the schema-less value type
├── id.rs               IdGenerator trait + UuidV7Generator
├── storage/              PageStore + FileStore (page checksums) +
│                            SlottedPage + MemoryStore (real, tested)
├── crc32.rs               CRC-32C, in hardware where the CPU has it
├── index/                 Index trait + BTreeIndex (real, persisted B-tree,
│                            primary and secondary indexes) + key encoding +
│                            InMemoryIndex (fake, for tests)
├── txn/                     TransactionManager trait + GlobalLockTxnManager,
│                              wired via Database::write_batch
├── durability/                Durability trait + WalDurability (real WAL) +
│                                NoopDurability (fake, for tests)
├── query.rs                     Filter/Condition/Sort — scan-based matching,
│                                  comparisons + case-insensitive Contains
├── collection.rs                  Collection<T> — public CRUD API (real)
├── json.rs                          tagged JSON <-> Document, lossless
├── export.rs                        Database::export/import (JSON Lines)
├── check.rs                         Database::check — consistency check
├── compact.rs                       Database::compact — rebuild, file shrinks
├── bin/trunkdb.rs                   the `trunkdb` command (info, check,
│                                      compact, export, import)
├── cursor.rs                        Cursor — streaming find
├── batch.rs                         Batch — typed atomic multi-op writes
└── database.rs                      Database — opens the file, recovers from
                                       the WAL, wires layers, write_batch
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

## Roadmap (short version)

Done so far (see SPEC.md §46 for the full list and reasoning):

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

What's still open: SPEC.md §46.2.

## License

Licensed under the [MIT License](LICENSE).
