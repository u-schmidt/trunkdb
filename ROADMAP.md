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
| next | A read-only open, `OpenOptions::read_only`: a shared file lock, nothing written, the WAL's batches and the open's own changes kept in memory, every write refused with `Error::ReadOnly`; the `trunkdb` command's `info`, `check` and `export` open read-only | §88 |

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
- The WAL's format matters less: it's empty after a clean close, so a
  change needs only a checkpoint before upgrading.
- Settled in §77: snapshot reads live in memory only, and freed pages
  need no tag, so they don't touch the file format.

**API questions: cheap now, breaking after 1.0**
- Done in §74: `find_with_ids` and `find_one_with_id` removed, and
  `cursor` handing out `T`, like `find`; a type without an id field
  wrapped, a document that isn't an object read by `get`.
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
- Index-only reads, what SQL calls a covering index or an index-only
  scan and MongoDB a covered query: where the index answers the whole
  filter, every condition on its fields and exact, its entries alone
  decide. Two places would gain: a skip that passes over index entries
  without reading their documents, where now a deep page reads every
  document it skips (§72.3); and `count` with conditions, which now
  reads every candidate document even when the index already says it
  matches. Not for returning documents: that needs projection (only
  some fields back), which trunkdb doesn't have, and a `Collection<T>`
  wants the whole `T` anyway. Multikey indexes can't answer this way
  (an entry is one element, not the array), as in MongoDB.
- An explain that runs the query, as MongoDB's `executionStats`: the
  results with the plan, index bounds, keys and documents examined,
  documents returned and time taken; now `explain` only names the kind
  of plan (`QueryPlan`) without running it.

**Storage and durability**
- Scan resistance for the page cache (§50.7).
- A free-space map that survives a restart: now it's in memory only,
  so space freed in an earlier session comes back only by `compact`
  (§75.4). If restarts leave too much behind: persisted as hints, as
  PostgreSQL's, one byte per data page, rounded down to 32 bytes, in map
  pages of their own, read at open into the exact map; one byte so most
  commits write no map page (§75.4).
- Compaction without holding the whole new file in memory: a streamed
  WAL record (§41.5). Leaves built from sorted entries instead of
  inserted one by one would make it faster still (§48.4), though 1 s for
  100,000 documents (§52.3) makes that less pressing.
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
- Projection, a `select` of fields (`Filter::new().select(["name",
  "team"])`), for `Collection<Document>` and the `trunkdb` command,
  which have no smaller struct to read into (a typed collection already
  can: the README's "Reading only some fields"). It saves decoding
  and memory, not reads: a document is stored in one piece, overflow
  pages included (§26), and its encoding records no byte sizes, so
  reaching a field still walks those before it (§11.1), though without
  allocating them. Projection is also what an index-only `find` would
  need (see index-only reads under Queries). When a workload asks for
  it.
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
- Done in §88: a read-only open (`OpenOptions::read_only`), a shared
  file lock and nothing written, recovery kept in memory.
- Repairing what `check` finds, beyond export and import (§39.5).
- `check` finding overlapping cells on a page (§54.2).
- Longer fuzzing runs, with AddressSanitizer too, and short ones in CI
  on nightly (§55.6).
- PyO3 bindings, for Python apps.

Not planned: a cost-based query planner, joins, aggregates, multiple
writers, encryption, schema validation (§6).
