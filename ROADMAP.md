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
| next | Documents as JSON text: `Display` and `FromStr` for `Document`, tagged JSON as an export writes it, and `ParseDocumentError`; `Filter::skip`, for paging with `limit`, read through an index only as far as skip plus limit | §71–§72 |

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
- A free-space map: persisted (a new page type, a format change) or
  rebuilt in memory at open (none, but a slower open). Either way it
  keeps §57.3.
- Done in §69: a date-time type, `Document::DateTime`, file format 11.
- The WAL's format matters less: it's empty after a clean close, so a
  change needs only a checkpoint before upgrading.
- Snapshot reads (§57) would most likely live in memory only, as in
  SQLite's WAL mode; how freed pages are tagged decides whether they
  touch the file.

**API questions: cheap now, breaking after 1.0**
- **Remove `find_with_ids` and `find_one_with_id`**, deprecated since
  0.12.0 (§59.4), and let `cursor` hand out `T` instead of `(DocId, T)`,
  like `find`. Document how a type without an id field, or a document
  that isn't an object, gets its id then (a wrapping struct; `get`).
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
- A skip that passes over index entries without reading their
  documents, where the index answers the whole filter; now a deep page
  reads every document it skips (§72.3).
- An explain that runs the query, as MongoDB's `executionStats`: the
  results with the plan, index bounds, keys and documents examined,
  documents returned and time taken; now `explain` only names the kind
  of plan (`QueryPlan`) without running it.

**Storage and durability**
- Scan resistance for the page cache (§50.7).
- A free-space map, so inserts refill half-empty data pages between
  compactions (§20.1, §41.5). It must keep the storage rules of §57.3:
  above all, space freed by a commit isn't reused while a reader could
  still need it.
- Compaction without holding the whole new file in memory: a streamed
  WAL record (§41.5). Leaves built from sorted entries instead of
  inserted one by one would make it faster still (§48.4), though 1 s for
  100,000 documents (§52.3) makes that less pressing.
- Batched writes, still about 2× behind: nearly every batch of 1,000
  crosses the default checkpoint threshold once the indexes are large.
  `OpenOptions::checkpoint_pages` can raise it (§53), but the WAL then
  grows with every commit, not with the threshold: a second trigger, on
  the WAL's size, would make a large value reasonable (§53.3).

**Concurrency**
- Readers that run in parallel past four: four do 3.2 times the work
  of one, eight no more than four, and nobody waits in the kernel. Most
  likely the atomic writes every page read makes to shared memory (the
  cache's read lock, the page's reference count): the lock taken once
  per operation instead of once per page, or cached pages borrowed
  instead of counted (§65.3).
- Writers that stage and flush beside the readers, taking the lock only
  to publish (§57.2, option B). A writer committing back to back
  starves readers now (§58.3); no current workload does. Not
  scheduled.
- Snapshot reads (MVCC): deferred, not rejected (§57). They would also
  stop an export from blocking writers (§30.6).

**API**
- A document as `serde_json::Value`, beside its text form (§71), if a
  tool needs it: behind a feature named `serde_json-1`, so `serde_json`
  stays out of the default public API (§71.4).
- More update operators: `push` and `pull` on arrays, `min`, `max`,
  `rename`; operators in a `Batch` and in `upsert` (§68.3).
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
- PyO3 bindings, for Python apps.

Not planned: a cost-based query planner, joins, aggregates, multiple
writers, encryption, schema validation (§6).
