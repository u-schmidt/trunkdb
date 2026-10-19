# trunkdb — roadmap

"v1" is a milestone name, not a semver promise: 0.x minor versions may
still break API and file format. 1.0.0 is reserved for a stable format
with a migration path — export/import (§30) is that path.

Section numbers (§) refer to the spec, one file per section in
[spec/](spec/README.md), which records why each step was taken. New
work gets a new spec section with the next number; this file only
lists it.

## Done
The first roadmap (2026-09-23) was ordered by priority: correctness and
the file format first, so real data never needs a migration; then what
the sync workload (§5.3) needs; then the large-document workload (§5.2).
All of it is done:

| Version | What | Sections |
|---|---|---|
| 0.1.0 | The v0 core: pages, catalog, B-tree primary index, document encoding, serde bridge, filter/sort/limit, batches, a WAL | §6–§18 |
| 0.3.0 | Block A, correctness and file format: page-image WAL, several documents per page, file lock and format version, three data-risk fixes | §19–§22 |
| 0.3.0 | Block B, the sync workload: `find_with_ids`, typed batches, `Contains` (planned as 0.2.0, never released on its own) | §23–§25 |
| 0.3.0 | Block C, the large-document workload: overflow pages, a thread-safe `Database`, secondary indexes, `find_one`/`count`/`upsert`/`cursor` | §26–§29 |
| 0.4.0 | Export/import, nested-field paths, null and missing fields, unique indexes, sorting through an index; file format 5 | §30–§34 |
| 0.5.0 | A filter builder; OR, NOT and nesting (`Condition` became a tree — breaking); `delete_many` and dropping a collection | §35–§37 |
| 0.6.0 | `update_many`; the `trunkdb` command and `Database::check`; page checksums, file format 6 | §38–§40 |
| 0.7.0 | Compaction, and index leaves that fill when keys come in order; array conditions and multikey indexes, file format 7 | §41–§42 |
| 0.8.0 | Compound indexes, sparse indexes, file format 8 | §43–§44 |
| 0.9.0 | `Exists` and array size; `Op` and `Condition` `#[non_exhaustive]`; `elem_match`; sorting by several fields (`Filter.sort` became a list — breaking) | §45–§47 |
| 0.10.0 | Benchmarks against SQLite, redb and sled; a lazy B-tree walk; a page cache (`Database::open_with`) | §48–§50 |
| 0.11.0 | One flush per commit (`Database::checkpoint`); B-tree pages changed in place; a configurable checkpoint threshold (`OpenOptions::checkpoint_pages`, and the `OpenOptions` fields private — breaking for code that read `cache_size`); a page's layout checked when it is read | §51–§54 |
| 0.12.0 | Fuzzing (`fuzz/`), and fourteen ways a damaged file crashed or hung trunkdb fixed; documents nest at most 64 levels; the concurrency model written down, snapshots deferred; how long readers wait, measured; a struct carries its own id (`#[serde(rename = "_id")] id: Option<DocId>`), and an insert keeps an id given in `_id` — a change from §18; the id stored once, in the cell, file format 9, which still opens format 8; `find_with_ids` and `find_one_with_id` deprecated; a smaller public API: the internals private, one `Batch` for typed and untyped collections (`write_batch` internal), `Filter` and `IndexOptions` built with methods, `ensure_unique_index` replaced by `IndexOptions::new().unique()`, `indexes()` returning `IndexInfo`, `Error::Txn` gone | §55–§60 |
| 0.13.0 | Filters on the id (`eq("_id", id)`, or an OR of ids) looked up in the primary index instead of scanned; `QueryPlan::ById`; an index key for ids, so an index on a reference (a `DocId` field) works, and ids sort after strings, by when they were made — file format 10, which still opens 6–9 and rebuilds, at the first open, an older index that holds ids; `Filter::id` and `Condition::id`; `DocId: FromStr`, with `ParseIdError` | §61–§63 |
| 0.14.0 | Shared pages: the page cache hands out its page (`Page`, an `Arc`) instead of a copy, and a change copies it first; the cache behind an `RwLock`, looked up under its read lock; one reader 30% faster, four 3.2 times one; a page's layout checked once, as it enters the cache or its batch commits; `write_page` takes the `Page`, so a change copies a page once, and compaction is 11% faster; update operators, `update_fields` with `Update::new().set(..).inc(..).unset(..)` on dotted paths, and `Error::Update`; a date-time type, `Document::DateTime`, what a `SystemTime` field stores, to the nanosecond, compared, sorted and indexed in time order, `{"$date": ...}` in export format 2 — file format 11, which still opens 6–10 | §64–§69 |
| 0.15.0 | `Document::DateTime` holds `trunkdb::DateTime` instead of a `SystemTime` (breaking), the same on every platform over an `i64` of seconds; `DateTime` fields, times before 1970 included; `ParseDateTimeError` | §70 |
| 0.16.0 | Documents as JSON text: `Display` and `FromStr` for `Document`, tagged JSON as an export writes it, and `ParseDocumentError`; `Filter::skip`, for paging with `limit`, read through an index only as far as skip plus limit; more update operators, `min`, `max`, `rename`, and `push`, `add_to_set` and `pull` on arrays | §71–§73 |
| 0.17.0 | `find_with_ids` and `find_one_with_id` removed, typed and untyped, and a `cursor` handing out `T` instead of `(DocId, T)` (breaking); the id from the type's `_id` field, a wrapper with `#[serde(flatten)]` for a type from another crate; a free-space map per collection, in memory, so inserts refill the room deletes and updates leave in older data pages | §74–§75 |
| 0.18.0 | A WAL size limit, `OpenOptions::checkpoint_wal_bytes`, so a large `checkpoint_pages` cannot grow the WAL without bound; a failed log taken back alone, so it no longer empties the WAL along with the batches committed before it (a fix to §51); snapshot reads: a batch is staged, logged, flushed and checkpointed beside the readers, which wait only for its publish, and a page's older versions stay in memory for the snapshots open, so a read keeps its commit to its end and commits wait for no read; `Database::snapshot` and `View<T>`, a snapshot a caller keeps, and a `cursor` that shows the moment it was made (breaking for code that relied on seeing later changes); `OpenOptions::snapshot_memory`, a limit on what snapshots keep, past which the oldest is ended with `Error::SnapshotTooOld`; `compact` takes the database alone and is refused with `Error::SnapshotOpen` while a snapshot is kept | §76–§83 |
| 0.19.0 | Damaged and hostile input: `trunkdb export` through a temporary file, never over the database or its WAL; damaged header fields recovered from the WAL; a limit on the line an import reads, 256 MiB unless `ImportOptions::max_line_bytes` says otherwise; a WAL header that can't grow the file past its pages; `drop_index` on `_id` refused. A read-only open, `OpenOptions::read_only`: a shared file lock, nothing written, the WAL's batches and the open's own changes kept in memory, every write refused with `Error::ReadOnly`; the `trunkdb` command's `info`, `check` and `export` open read-only; a `checkpoint_pages` past 2^50 no longer overflows the default WAL limit (a fix to §76) | §84–§89 |
| 0.20.0 | **Memory that doesn't grow with the file**, steps 1 and 2: the WAL record written from the pages, compaction in 1.2× the new file's size instead of 3.3×; a batch over 32 MiB logged in several records, crash recovery a record at a time, 37 MB where it took 2.5 GB; no batch size limit, where a batch over 4 GiB was logged with its length cut to 32 bits; WAL version 3, version 2 still recovered. **Diagnostics:** `Error::Format` for a file this build can't read at all (not a trunkdb file, a format or WAL version it doesn't read), `InvalidData` at an open meaning damage only; `Database::stats`, in `trunkdb info` too: pages by kind, sizes per collection and index, the WAL, recovery at open, and the pages a `compact` would leave; `explain_run`: the plan, each index read and its bounds, how the sort was done, entries and documents examined, matched and returned, the time. **Only objects** (breaking for `Collection<i64>` and the like): a document that isn't one refused with `Error::NotAnObject`, file format 12, an older file checked once at open. **Reads** — same results, same file: a sort by `_id` walks the primary index and stops at the limit, or the range an `Eq` fixes, `_id` comparisons narrowing either (`explain` says `IndexOrder` for the second, where it said `Index`); only what a read needs decoded, typed reads straight into `T`; index entries decide the filter without their documents, on their bytes, and a leaf inside the range counted or skipped whole; walks lend their entries instead of copying them; paths parsed once per query. On 100,000 documents: the first 50 by id 33 µs instead of 189 ms, a scan 64 ms instead of 168, a count by index 0.01 ms instead of 0.9, page 50 of 20 27 µs instead of 1.2 ms, a tenant's first 20 by id 23 µs instead of 0.6 ms. **Documented:** on Unix a database dropped while a process starts stays locked until that child's exec. | §90–§104 |
| 0.21.0 | A differential fuzz target for queries, `queries`: any bytes as a program of index changes, writes, compactions, reopens, snapshots and reads on a valid database, every read through every way of reading held against the filter applied in memory to a model of what is stored; a smoke run of it on stable, in CI on 400 random programs; it catches 33 of the 36 hand-made mutations of §100–§104 that change answers. A leak it found fixed: a current data page emptied by deletes and given up for a new one, when a large document didn't fit beside its slot directory, was left in the free-space map instead of freed. The other fuzz targets' starting database built again: a scratch collection of plain numbers, refused since §95, had left them panicking at their start, and CI now builds it. `compact` beside a snapshot documented as `Ok` where nothing would be rewritten. **Projection:** `Filter::select`, the fields a read hands out, `_id` always, in the order selected, nested as stored, a missing field left out; the rest skipped, not decoded; a match handed out from its index entry, its document never read, where the entries decide the filter and hold every selected field and its values are booleans, strings that fit the key, ids or date-times; `explain_run`'s `from_entries` and `select`; writes on whole documents, `update_many` too. Aggregates taken off "not planned". **Aggregates:** `Collection::aggregate(filter, &Aggregate)`, groups by `group_by` paths with `count`, `sum`, `min`, `max` and `avg`, then `having`, a row sort, skip and limit; a missing field grouped with null, `5` and `5.0` one group, a row's values its first document's by id; sums exact in a fixed-point integer, so every plan gives one answer; `count`, `avg`, `min` and `max` from index entries where an index holds the fields, a walk counting whole leaves of a group, without conditions an index walked only if it holds every document; `sum` from the documents; `explain_aggregate` and `ExecutionStats::groups` | §105–§107 |
| next | **Memory that doesn't grow with the file**, step 3: compaction's new image built in a file beside the database, `<database>.compact`, whose name is gone as soon as it's open, so a crash leaves nothing; logged to the WAL from there a record at a time, read from there until the checkpoint copies it in, a megabyte at a time. From 2.1 GB to 1.1 GB in 0.1 GB of memory instead of 1.25, in the same 11 s; the disk needs room for the new file twice more until it's done. **`check`** in memory that doesn't grow with the file: documents read one at a time, an index checked entry by entry against the documents its entries name, a second pass only to name what's missing; a page's owner in four bytes. On a file of 1.1 GB, 0.3 GB instead of 1.75 with the default cache, in the same time; 3 MB instead of 1.3 GB without one | §108–§109 |

## Before 1.0
No date: 1.0 comes after months of real use in more than one
application, not after a list. This is what it would have to settle, so
nothing lands in the meantime that makes it harder.

**What 1.0 promises**
- Every 1.x build opens every file a 1.x build wrote; a file using
  something an older 1.x build lacks is refused by it, clearly, as now
  (§21.2, §33.4). Files from before 1.0 move up by export and import
  (§30).
- No breaking API change within 1.x.

**Format questions.** Index changes are cheap, since indexes are derived
data and can be rebuilt from the documents at open. Changes to
documents, data pages and the header are the expensive ones.
- Done in §75: a free-space map, in memory only, no format change. A
  persisted form can come in 1.x if real use asks for it: a file using
  it is refused by an older 1.x build, as 1.0 promises, so it needn't
  be decided before.
- Done in §69: a date-time type, `Document::DateTime`, file format 11.
- Done in §95: every document an object, file format 12.
- The WAL's format matters less: it's empty after a clean close, so a
  change needs only a checkpoint before upgrading.
- Settled in §77: snapshot reads live in memory only, and freed pages
  need no tag, so they don't touch the file format.

**API questions: cheap now, breaking after 1.0**
- Done in §74: `find_with_ids` and `find_one_with_id` removed, and
  `cursor` handing out `T`, like `find`; a type without an id field
  wrapped.
- Done in §95: documents that aren't objects refused, so every
  document has its `_id` and `Collection<T>` takes a struct or a map.
- Done in §60: the internals private, `Error`, `Document` and the
  reports `#[non_exhaustive]`, `IndexOptions` with builder methods.

**What only use can tell:** whether the API holds up in two or three
applications, and whether the format has anything that hurts over
months of real data.

## Open
Unordered within each group; each line says where the need or the
limit is described.

**Data types**
- A date without a time, and a time without a date, as variants of
  their own; until then, noon UTC of the day serves (§69.1).
- `chrono`, `time` and `jiff` recognized on the way in, behind a
  feature each, as date-times rather than the strings they serialize to
  (§69.5); conversions between their types and `trunkdb::DateTime`.

**Queries**
- A pattern language: regex or `LIKE`-style wildcards (§25.4).
- A sort by `_id` after an OR of `Eq`s, or after a compound index fixed
  only in part: a merge of id-ordered ranges, where §103 walks one
  (§103.7).
- Leaves decided whole for filters that accept several intervals of
  keys (`ne`, an OR, a NOT), each leaf checked against the intervals'
  bounds; they decide entry by entry, on bytes, since §104 (§104.6).
- A scan, 1.3× SQLite's: each document's filtered fields are decoded
  into a small object to be checked (§99); checking them as they are
  decoded would save that.
- The count of all documents, 0.09 ms for 100,000 where SQLite and redb
  keep a total: counted leaf by leaf since §101. A total kept in the
  catalog would make it free, at a write to the catalog page in every
  batch that inserts or deletes.

**Storage and durability**
- Scan resistance for the page cache (§50.7).
- A free-space map that survives a restart: now it's in memory only,
  so space freed in an earlier session comes back only by `compact`
  (§75.4). If restarts leave too much behind: persisted as hints, as
  PostgreSQL's, one byte per data page, rounded down to 32 bytes, in map
  pages of their own, read at open into the exact map; one byte so most
  commits write no map page (§75.4).
- Leaves built from sorted entries instead of inserted one by one,
  which would make compaction faster (§48.4), though 1 s for 100,000
  documents (§52.3) makes that less pressing. Compaction and crash
  recovery now take memory that doesn't grow with the file (§90, §91,
  §108), but for each index's keys, sorted in memory while it is
  rebuilt: an external sort, if real files ask for it (§108.8).
- Batched writes, still about 2× behind: nearly every batch of 1,000
  crosses the default checkpoint threshold once the indexes are large.
  `OpenOptions::checkpoint_pages` can raise it (§53); the WAL's size
  is bounded by `checkpoint_wal_bytes` (§76), so a large value is now
  reasonable. Its default is still 1,000 pages, to be raised only with
  a benchmark of more workloads than one.

**Concurrency**
- Readers that run in parallel past four: four do 3.2 times the work
  of one, eight no more than four, and nobody waits in the kernel. Most
  likely the atomic writes every page read makes to shared memory (the
  cache's read lock, the page's reference count): the lock taken once
  per operation instead of once per page, or cached pages borrowed
  instead of counted (§65.3).
- The full `reader_wait` (§58) with redb beside it, on the machine of
  §58, for the snapshot build: §79.7 and §80.6 measured a cut-down one
  on two cores.
- Short reads on several threads: a tenth fewer with two readers since
  §80, each read now writing to three shared places instead of one
  (§80.6). With §82's kept snapshots counted, `compact` could do
  without the gate on the read path (§80.8).

**API**
- A document as `serde_json::Value`, beside its text form (§71), if a
  tool needs it: behind a feature named `serde_json-1`, so `serde_json`
  stays out of the default public API (§71.4).
- Aggregates beyond §107, when asked (§107.8): grouping by array
  elements (MongoDB's `$unwind`); `count_distinct`, `first`, `last`,
  `push`, percentiles; `sum` from index entries, which wants an
  `Int`/`Float` bit and a missing mark in every key, file format 13
  (§106.8); `min` and `max` that seek a group's ends instead of walking
  its entries; rows handed out as they are made rather than collected.
- Projection beyond paths, when asked (§106.8): arrays trimmed to a
  field of their elements (`items[*].sku`, which now selects `items`
  whole), and fields excluded instead of selected.
- Operators in a `Batch` and in `upsert` (§68.3).
- More update operators, when asked for: `pull` by a condition on the
  element, `push` of several values at once, `mul`, `pop` (§73.3).
- An OR mixing ids and other conditions, served by lookups and index
  ranges together; now one branch without ids means no lookups (§61.4).
- Maybe later, as sugar: `#[trunkdb::document]` on a struct and `#[id]`
  on its id field, so a consumer says "this is a document, that is its
  id" and needn't know about `_id` or serde's `rename` (§59). Without
  it, `eq("id", ...)` on a struct's id field still finds nothing,
  silently, in code that doesn't use `Filter::id` (§63.3). An attribute
  macro that adds `#[serde(rename = "_id")]` to the marked field before
  serde's derive runs, and refuses at compile time a field
  that isn't `DocId` or `Option<DocId>`. It needs a `proc-macro` crate of
  its own (`trunkdb-derive`, with `syn` and `quote`), re-exported behind
  a feature as serde does with `derive`. Purely additive: nothing breaks
  without it. A good piece for learning how Rust generates code at
  compile time.

**Tooling and reach**
- Repairing what `check` finds, beyond export and import (§39.5).
- `check` finding overlapping cells on a page (§54.2).
- Longer fuzzing runs, with AddressSanitizer too, and short ones in CI
  on nightly (§55.6).
- Unique indexes in the `queries` target, the model asking the
  database's own filters whether two values collide (§105.7).
- A query on the command line, `trunkdb find`: the command has none yet
  (§39), and one needs a filter syntax in text, a decision of its own
  (§106.8).
- PyO3 bindings, for Python apps.

Not planned: a cost-based query planner, joins, multiple writers,
encryption, schema validation (§6). Aggregates were on this list until
§106.7, and came in §107.
