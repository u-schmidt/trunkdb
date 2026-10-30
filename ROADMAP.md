# trunkdb — roadmap

trunkdb stays below 1.0. Its API and file format are kept stable by
practice rather than by a version number; [A settled API and
format](#a-settled-api-and-format) says how. Export and import (§30)
move a file between formats where needed.

Section numbers (§) refer to the spec, one file per section in
[spec/](spec/README.md), which records why each step was taken. New
work gets a new spec section with the next number; this file only
lists it. [CHANGELOG.md](CHANGELOG.md) has each step on its own.

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
| 0.22.0 | **Memory that doesn't grow with the file**, step 3: compaction's new image built in a file beside the database, `<database>.compact`, whose name is gone as soon as it's open, so a crash leaves nothing; logged to the WAL from there a record at a time, read from there until the checkpoint copies it in, a megabyte at a time. From 2.1 GB to 1.1 GB in 0.1 GB of memory instead of 1.25, in the same 11 s; the disk needs room for the new file twice more until it's done. **`check`** in memory that doesn't grow with the file: documents read one at a time, an index checked entry by entry against the documents its entries name, a second pass only to name what's missing; a page's owner in four bytes. On a file of 1.1 GB, 0.3 GB instead of 1.75 with the default cache, in the same time; 3 MB instead of 1.3 GB without one. **Fixed:** an aggregate from index entries showed a group's number as an earlier match had it (`3` for the first document's `3.0`) where its first document by id came late in the walk (a fix to §107). **Filters as text:** `FromStr` and `Display` for `Filter` and `Condition`, a language like SQL's `WHERE` with `select`, `order by`, `limit` and `offset`, `is null`, `exists(…)`, `size(…)`, `has (…)` with `@` for the element, values as JSON with `id("…")` and `date("…")`, names in backticks where they must be; it compiles to the same `Filter`; `ParseFilterError` with line and column; a fuzz target, `filter_text`, and the `queries` target reading through the text. **`trunkdb find <file> <collection> [filter]`**: what the filter finds, one JSON document per line, the count to stderr; read-only, streamed from a cursor, quiet when its reader goes; a filter's mistake shown under its line with a `^`, exit code 2 | §108–§112 |
| 0.23.0 | `trunkdb find … --explain`: how a query ran instead of what it found, a line each: the plan in words, every index read with its bounds in the filter language, its entries and whether they decide the filter, the sort, the counts, and the time last. `#[trunkdb::document]`, with the `derive` feature: `#[id]` on a struct's id field instead of `#[serde(rename = "_id")]`, refused at compile time on a type that isn't `DocId` or `Option<DocId>`, on a second field, or with arguments; a macro crate of its own, `trunkdb-derive`, re-exported as serde re-exports `serde_derive`. **`in`:** `Condition::is_in` and `Filter::is_in`, one of a list of values, SQL's `IN`, the OR of an `eq` per value, so an index reads a point per value and `_id` looks each up; `in (…)` and `not in (…)` in the text, where an OR of `eq`s of one field prints as `in`. **A map of its own:** `Document::Object` holds a `trunkdb::Map`, `IndexMap` inside it and out of the API, its iterators its own types in `trunkdb::map`, the same methods and order as before (`remove` keeping the others' order), equal in any order; breaking for code that named `IndexMap`. **Variants that can grow:** `Condition`'s and `QueryPlan`'s variants with named fields `#[non_exhaustive]`, matched with `..` outside trunkdb, built with `Condition`'s methods; compile-fail tests for each. **The public API gone through**, from rustdoc's JSON: options and reports without `Copy` and `Eq`, two helpers private, `Document` no `Deserializer` (`into_value` the way), the `fuzzing` feature outside semver. **Unique indexes in the `queries` target**, the model asking the database's own `Eq` whether a write makes a duplicate, a batch op by op; **fixed**, found by it: an update of a long string past where its key is cut, which kept its key, wasn't checked, and could store a duplicate **Old files as fixtures:** a database each release from 0.23.0 wrote, in `tests/fixtures/`, opened by every build, read as written, queried through each index and written to; it alone catches a stored type tag changed in encoder and decoder alike. The minimum Rust version's policy in the README | §113–§120 |
| 0.24.0 | `trunkdb fields <file> <collection> [filter]`: every path a collection's documents have, written as a filter writes it, in how many documents, and as which kinds of value, most first; a filter sums up only what it finds, `limit` a sample; read-only, at most 10,000 paths; an Agent Skill, `skills/trunkdb/`, for AI agents using the command. **Batched writes:** the default checkpoint threshold 4,000 pages instead of 1,000, the WAL limit following to 65.5 MB; with 1,000 documents a commit, 59 commits of 100 checkpointed, now 10; on 100,000 documents inserts about 1.35× faster, updates 1.35×, deletes 1.6×, single commits and reads unchanged; recovery after a crash with a full WAL 79 ms; a compaction's commit writes it back at once whatever its size, as §108 had it. **Fixed:** an OR of `eq`s of one field whose branches came wrapped alone in an AND or OR printed as an OR but read back as `in`, so its text didn't read back the same (a fix to §115). **`=` and `!=` on arrays, objects and binary:** compared by their contents, as `add_to_set` and `pull` compare them (objects in any field order, `[1]` equal to `[1.0]`), where `=` never held and `!=` always did, even against the very value; `in`, `not in` and `has (@ = …)` with them; `<` and the rest still never hold; an index entry with such a value in a compound key read from its document; groups of equal ones merged; unique indexes unchanged, still not checking them. **The release that wrote a file:** the header names the release that created the file and the one that last wrote it, `FileInfo::created_by` and `written_by`, `Release` and `trunkdb::RELEASE`, in `trunkdb info` too; unknown for older files and where an older build wrote since; still format 12. **Smaller fixtures:** 60 tasks instead of 300, the 0.23.0 fixture written again by 0.23.0's code, 9 KB compressed instead of 32; a fixture only for a release that changes how files are written. **Fixed:** `trunkdb info` panicked when its reader went (`| head`); it ends quietly, as `find` does | §120.9, §121–§125 |
| 0.25.0 | **A condition on an aggregate column:** `filter` after a column, SQL's `FILTER (WHERE …)`, the column taking in only the group's documents that meet it, the groups and other columns every match; any column, two ANDed, one before any column refused; decided from index entries where the index holds its fields, a leaf of counts at a time where its ends decide it; the column methods' docs saying their string is the column's name. **Fixed:** a database opened through a symlink found a WAL beside the link instead of the file's, so after a crash it missed committed batches, and a write through it could have the file's old WAL replayed over newer pages later; the link is now followed to the file before the WAL is named, a loop of links refused; `trunkdb export` through a link protects the file's WAL; a hard link documented as unsupported. **Fixed:** a checkpoint whose flush failed after it had cut the WAL left the WAL's length as before, so a failed log after it was taken back by growing the WAL with zeros, and the next open refused it; the length is 0 once the cut succeeds, and an undo never grows the WAL past its end before the failed log (a fix to §81). **Fixed:** a page whose slot directory ran past its end, the cells said to start further on and its checksum matching, panicked as it was checked, reading past the page; refused as damage before a slot is read, a seed for `open_raw` with it. **Fixed:** on Unix, a compaction opened `<database>.compact` emptying what was there, following a link: whatever file a link under that name pointed to was emptied, even where the compaction gained nothing; the file is now always a new one, as on Windows, under the first name free (a fix to §108). **Fixed:** `avg` rounded the exact total to a float before dividing it, so finite numbers whose total passed `f64::MAX` had an infinite mean, and other means could be a unit off; the exact total is now divided, then rounded once, never `-0.0` (a fix to §107) | §126–§131 |
| next | **A hint in the query:** `Filter::use_index(fields)` reads that index where the rules would read another, `Filter::no_index()` scans, `"_id"` the primary index; the rules choose the rest among it alone; a hint that can't be followed, no such index or nothing to read it by, is `Error::Hint` from every read, never a quiet fall back; `explain_run` says `hinted`; `use index …` and `use no index` last in a filter's text and `trunkdb find`; a panic on `false` under a hint, found by the `queries` fuzz target, fixed before release ; **the plans `explain_run` passed over:** `Explained::alternatives`, the filter hinted to each index, `_id` and a scan, each with the entries it would read, `Exactly` or `MoreThan`, whether it reads a document per entry, and whether that's fewer; ranges counted whole a leaf at a time up to 1,000 leaves, walks in sort order as far as the run read; `instead:` lines in `trunkdb find --explain`; **aggregate columns named last** (breaking): `count().named("n")`, `sum("tries").named("total")`, a column's `filter` before its name, an `UnnamedColumn` until it has one, so a column without a name or a `filter` before any column doesn't compile | §132–§134 |

## A settled API and format
trunkdb aims for an API and a file format that stay put. It doesn't
give formal guarantees yet: those wait until use in more than one
application shows what holds for a broader range of users. In practice:

- **Files:** a release opens the files earlier releases wrote, from
  0.23.0 on, and the fixtures (§120) check it. A file using something
  an older build lacks is refused by it with a clear error (§21.2,
  §33.4). Should a format ever not be readable in place, export and
  import (§30) move a file up.
- **Changes are deliberate:** each has its own spec section saying why,
  and the changelog points out anything a program has to adapt to.
- **The filter language** (§111) grows without taking names from
  fields: a new operator comes only where a field can't be (`in`,
  `has`, `contains`), and the eleven reserved words of §111.3 stay the
  only ones.
- **Public dependencies are chosen on purpose:** `serde` is one, as for
  nearly every crate; any other is a decision, not an accident.

This list settles early what would be expensive to change once
applications rely on trunkdb.

**Settled already.** The free-space map lives in memory, so a
persisted one can come later (§75); a date-time type, file format 11
(§69); every document an object, format 12 (§95); snapshot reads in
memory only, with nothing in the file (§77); `find_with_ids` removed and
`cursor` handing out `T` (§74); the internals private, and `Error`,
`Document` and the reports `#[non_exhaustive]` (§60). The WAL's format
matters less: it's empty after a clean close, so a change needs only a
checkpoint before upgrading.

**Must: expensive to change once applications use it**
- Done in §116: **`indexmap` out of the API.** `Document::Object`
  holds a `trunkdb::Map`, trunkdb's own type with the `IndexMap` inside
  it, so an `indexmap` 3 changes nothing a program sees.
- Done in §117: **variants with public fields `#[non_exhaustive]`**,
  `Condition`'s and `QueryPlan`'s, so a field can be added to one
  without breaking a `match`; `All`, `Any`, `Not` and the unit
  variants left as they are.
- Done in §118: **one pass over the whole public API**, listed from
  rustdoc's JSON: options and reports no longer `Copy` or `Eq`, two
  helpers made private, `Document` no longer a serde `Deserializer`,
  the `fuzzing` feature declared outside the API. Another pass, the
  same way, once the API settles, for what came since.
- Done in §134: **aggregate columns, the name last.** `count("n")` was
  read as what is counted, as SQL, LINQ, Spark and Polars write
  `count(x)`. Now `count().named("n")`, `sum("tries").named("total")`,
  a column's `filter` before its name; a column is an `UnnamedColumn`
  until it has one, so a column without a name, or a `filter` before
  any column, doesn't compile.

**Should: cheap, and checked instead of kept by care**
- Done in §120: **old files as fixtures**, a small database written by
  each release from 0.23.0 that changes how files are written (§120.9),
  kept in `tests/fixtures/`, and a test that
  opens every one, reads it as written and writes to it. It catches a
  change to how values are stored that no other test does.
- Done: **every public item documented**, and the `missing_docs` lint
  on in `src/lib.rs`, so CI fails on a new item without docs. The MSRV
  policy is written down, in the README.
- Done in §119: **unique indexes in the `queries` target**, which
  found an update that could store a duplicate.

**Optional: where trunkdb is published**
- **On crates.io**, with its docs on docs.rs, or GitHub alone, as now:
  not decided. Use in more than one application is what settles the
  API, and a git dependency pinned to a commit serves that as well as
  `cargo add trunkdb`; neither changes a file, a program or a query.
  Nothing is published on crates.io.

**Later: additive, in any release**
- **An index rebuilt at open, where its key format changed:** not
  needed now after all. Every open reads the file's format version, so
  the release that changes a key — the `Int`/`Float` bit and missing
  mark `sum` from index entries wants (§106.8) — bumps it and rebuilds
  the indexes of older files then; nothing need be in today's files for
  it. What a read-only open does then, and how a long first open shows
  its progress, is decided with that change.
- New types — a date without a time, a time without a date, a decimal:
  `Document` is `#[non_exhaustive]`, and a file holding one is refused
  by an older build, as above.
- Everything under **Open** below: new operators, parameters (`:`
  reads nowhere now), `serde_json` behind a feature, repairing what
  `check` finds, the planner, performance, the PyO3 bindings. None changes a file today's build
  writes or what a program or query means today.

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
- Rules are enough while every plan is bounded by one pass over one
  collection; where they are blind to the data, a hint (§132) and the
  plans `explain_run` passed over (§133) cover it. With joins, or
  lookups into another collection, that bound is gone and the choice
  of a rule-based planner wants a second look. The alternatives for
  `explain_aggregate`'s reads from entries wait until asked for
  (§133.5).
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

**Changes as they happen**

The time-series workload (§5.1) asks when a position enters or leaves
a geofence; the application works it out itself today. Readings sent
every few seconds, such as a server's load, ask the same of a mean
over a window. Four steps, each useful alone, in this order:
- A hook after each commit: the collection and the ids of the
  documents a batch inserted, changed or deleted, handed out once the
  batch is durable, never for one that failed. SQLite's
  `sqlite3_update_hook` and commit hook, MongoDB's change streams.
- Conditions on places: a point within a circle or a polygon, as
  MongoDB's `$geoWithin`, needed apart from the rest. With it an index
  on points, if a scan of the fences is too slow; with a few fences it
  isn't. Geometry runs deep (sphere or plane, the antimeridian,
  polygons with holes), and SQLite and PostgreSQL keep it in extensions
  (R*Tree, PostGIS). The alternative: a condition of the application's
  own, a closure on the document (`Condition::custom`), so trunkdb
  knows nothing of places and the application brings its geometry
  crate; it serves `find`, the hook and live queries alike, and any
  other test no operator covers. It never bounds an index and has no
  text form (§111), which a few fences don't miss. Decided once the
  hook is in use.
- Live queries: a `Filter` kept as a subscription, which reports the
  documents that came to match it and those that no longer do. With a
  position kept in one document of state, updated with each ping,
  entering a fence is that document coming to match, leaving it the
  document no longer matching: no state of its own. Each commit checks
  the live queries against the documents it changed; at a ping every
  second or two that costs nothing worth counting, so no incremental
  aggregates (DBSP, TimescaleDB's continuous aggregates) are needed.
  Left to the application: hysteresis at a fence's edge (GPS noise
  flips a position in and out), and order: pings a tracker sends late,
  after being offline, are applied in the order of their time, not of
  their arrival, or a fence is entered after it was left.
- Live aggregates: an `Aggregate` (§107) kept as a subscription, as a
  `Filter` above: "the mean load of the last five minutes", reported as
  it changes, while a process reports the load every few seconds. A
  limit is a `having` (`mean > 0.8`), and crossing it is a row coming
  to match or no longer matching, as a fence is entered or left. Run
  again after each commit that touches the collection, over the
  window's range of an index on the time; five minutes of a reading
  every five seconds is 60 documents, so incremental maintenance isn't
  needed here either. "The last five minutes" counted back from the
  newest reading, not from the clock: then only a commit changes the
  result, and the same readings give the same answer. Counted back from
  the clock, readings age out with nothing written, and a timer would
  have to run the aggregate again: a second source of events, decided
  only if asked for. Hysteresis at the limit, as at a fence's edge, is
  the application's.

What the hook, live queries and live aggregates share:
- **Kept in memory, never in the file.** A subscription is registered
  by the program, as an event handler is, and ends when it is dropped
  or the program does; the program registers it again at its start.
  What reacts to a change is the application's code, which a file
  can't hold, so a stored query would have no one listening after a
  restart anyway.
- **The state first, then the changes.** A live query or aggregate
  reports at its registration what matches now, then only what comes
  to match and what no longer does, as Realm's do: a position already
  in a fence at the start is reported so. The state that decides it,
  the position in its document, is in the file, so the answer is
  rebuilt after a restart, not remembered.
- **Nothing missed while the program is down:** the file is locked to
  one writing process (§21), so with the program gone nothing changes.
  The one gap is a crash between a commit and its report. Where a
  report must not be lost (a message sent once), the application
  writes the event as a document in the same batch as the change that
  caused it, and works those off apart (the transactional outbox, as
  .NET's MassTransit and NServiceBus have it): both or neither.
- **Through a channel, not a callback.** A report handed to a channel
  is read on the application's own thread, which may then read and
  write freely. A callback run inside the commit that writes to the
  database would wait for the writer it runs in.

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
- Batched writes, inserts about 0.7× SQLite's since §123 raised the
  checkpoint threshold: the CPU work is level (inserts without flushes
  run at SQLite's speed), the rest is the disk. What would pay, in order
  (§123.5): WAL records of a page's changed bytes after its first whole
  image in a checkpoint interval, 10 MB a commit of 1,000 documents now
  (WAL version 4); a checkpoint without the WAL's own flush, a
  generation number in its header and records; `check_layout` skipped
  for a node checked when it entered the cache.

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
- A leaf decided whole for a column's range after the group fields
  (§126.4, §126.9): per status on `(status, created)`, those since
  Monday, where a leaf's ends agree on their group values and both meet
  the range, so every entry between does. A new case in `decide_all`;
  until then such leaves are walked entry by entry, from their entries.
  If §126's measurements show the walk costs enough.
- Counts of everything with conditions as one `count` per column
  (§126.9), each through its index, where one pass reads every
  document.
- Projection beyond paths, when asked (§106.8): arrays trimmed to a
  field of their elements (`items[*].sku`, which now selects `items`
  whole), and fields excluded instead of selected.
- Operators in a `Batch` and in `upsert` (§68.3).
- More update operators, when asked for: `pull` by a condition on the
  element, `push` of several values at once, `mul`, `pop` (§73.3).
- An OR mixing ids and other conditions, served by lookups and index
  ranges together; now one branch without ids means no lookups (§61.4).

**Tooling and reach**
- Repairing what `check` finds, beyond export and import (§39.5).
- `check` finding overlapping cells on a page (§54.2).
- Longer fuzzing runs, with AddressSanitizer too, and short ones in CI
  on nightly (§55.6).
- Parameters in a filter's text, named, `status = :status`, values
  passed apart (§111.8), with the PyO3 bindings at the latest;
  aggregates and updates as text (§111.11). Aggregates in SQL's
  order, `group by team select count() as tasks, avg(hours) as
  mean_hours having tasks > 1`, the form most who'd want it know,
  documented with what goes in and what comes out; a column's
  condition in it, `count() filter (where …) as n` (§126.11).
- PyO3 bindings, for Python apps.

Not planned: a cost-based query planner, joins, multiple writers,
encryption, schema validation (§6). Aggregates were on this list until
§106.7, and came in §107.
