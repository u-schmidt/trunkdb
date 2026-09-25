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
| next | Fuzzing (`fuzz/`), and fourteen ways a damaged file crashed or hung trunkdb fixed | §55 |

## Open
Unordered within each group; each line says where the need or the
limit is described.

**Queries**
- A pattern language: regex or `LIKE`-style wildcards (§25.4).

**Storage and durability**
- Scan resistance for the page cache, and shared pages instead of a
  copy per read (§50.7).
- A free-space map, so inserts refill half-empty data pages between
  compactions (§20.1, §41.5).
- Compaction without holding the whole new file in memory: a streamed
  WAL record (§41.5). Leaves built from sorted entries instead of
  inserted one by one would make it faster still (§48.4), though 1 s for
  100,000 documents (§52.3) makes that less pressing.
- Batched writes, still about 2× behind: nearly every batch of 1,000
  crosses the default checkpoint threshold once the indexes are large.
  `OpenOptions::checkpoint_pages` can raise it (§53), but the WAL then
  grows with every commit, not with the threshold: a second trigger, on
  the WAL's size, would make a large value reasonable (§53.3). A staged
  page is also still copied whole on every read and write.

**Concurrency**
- Readers that don't wait for a write batch: MVCC or pre-batch page
  snapshots (§27). It would also stop an export from blocking writers
  (§30.6).

**API**
- Update operators (`$set`, `$inc`, `$unset` on paths) next to
  `update_many`'s closure (§38.1).
- A struct with its own id field (`#[serde(rename = "_id")]`, §13.5,
  §18) — `find_with_ids` (§23) covers most needs; unscheduled.

**Tooling and reach**
- Repairing what `check` finds, beyond export and import (§39.5).
- `check` finding overlapping cells on a page (§54.2).
- A limit on how deeply documents nest, so a damaged one can't
  overflow the stack when it's decoded; it would also refuse inserting
  one that deep (§55.6).
- Longer fuzzing runs, with AddressSanitizer too, and short ones in CI
  on nightly (§55.6).
- PyO3 bindings, for Python apps.

Not planned: a cost-based query planner, joins, aggregates, multiple
writers, encryption, schema validation (§6).
