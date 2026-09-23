# trunkdb

An embedded, single-file, schema-less document database — a learning-first,
from-scratch reimplementation of the [LiteDB](https://www.litedb.org/) idea
in Rust. No tables, no joins, no schema migrations: open a file, get
collections of documents.

**Status: v0 core is real and usable.** Page storage, the catalog, a real
B-tree primary index, document encoding (both the untyped `Document` path
and a serde bridge so any `T: Serialize + DeserializeOwned` works),
scan/filter/sort/limit queries (`find`, `find_one`, `count`, a streaming
`cursor`), `upsert`, single-field secondary indexes, overflow pages for documents larger
than a page, a page-image write-ahead log making
every write crash-safe, and atomic multi-collection batch writes
(typed and untyped) with real rollback are all real and tested — not stubs. Still deliberately out of scope for v0: secondary
indexes beyond single fields, a cost-based query planner, and concurrent writers (`Database` is a
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
├── storage/              PageStore + FileStore + SlottedPage (real, tested)
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
├── cursor.rs                        Cursor — streaming find
├── batch.rs                         Batch — typed atomic multi-op writes
└── database.rs                      Database — opens the file, recovers from
                                       the WAL, wires layers, write_batch
```

## Building

```
cargo build
cargo test
```

## Roadmap (short version)

Done so far (see SPEC.md §30 for the full list and reasoning):

1. **Correctness and file format**: a page-image WAL (SPEC §19),
   several documents per data page (§20), a file lock and format
   version (§21), and three data-risk fixes found in review (§22).
2. **A small app replacing MongoDB**: `find` returning ids (§23), a
   typed batch API (§24), case-insensitive substring search (§25).
3. **A desktop app with large documents**: overflow pages (§26), a
   thread-safe `Database` handle (§27), secondary indexes (§28), and
   `find_one`/`count`/`upsert`/`cursor` (§29) → 0.3.0.

Next: export/import as JSON Lines, the migration path between file
format versions; then an unordered list of further features.

## License

Licensed under the [MIT License](LICENSE).
