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
| 0.24.0 | `trunkdb fields <file> <collection> [filter]`: every path a collection's documents have, written as a filter writes it, in how many documents, and as which kinds of value, most first; a filter sums up only what it finds, `limit` a sample; read-only, at most 10,000 paths; an Agent Skill, `skills/trunkdb/`, for AI agents using the command. **Batched writes:** the default checkpoint threshold 4,000 pages instead of 1,000, the WAL limit following to 65.5 MB; with 1,000 documents a commit, 59 commits of 100 checkpointed, now 10; on 100,000 documents inserts about 1.35× faster, updates 1.35×, deletes 1.6×, single commits and reads unchanged; recovery after a crash with a full WAL 79 ms; a compaction's commit writes it back at once whatever its size, as §108 had it. **Fixed:** an OR of `eq`s of one field whose branches came wrapped alone in an AND or OR printed as an OR but read back as `in`, so its text didn't read back the same (a fix to §115). **`=` and `!=` on arrays, objects and binary:** compared by their contents, as `add_to_set` and `pull` compare them (objects in any field order, `[1]` equal to `[1.0]`), where `=` never held and `!=` always did, even against the very value; `in`, `not in` and `has (@ = …)` with them; `<` and the rest still never hold; an index entry with such a value in a compound key read from its document; groups of equal ones merged; unique indexes unchanged, still not checking them. **The release that wrote a file:** the header names the release that created the file and the one that last wrote it, `FileInfo::created_by` and `written_by`, `Release` and `trunkdb::RELEASE`, in `trunkdb info` too; unknown for older files and where an older build wrote since; still format 12. **Smaller fixtures:** 60 tasks instead of 300, the 0.23.0 fixture written again by 0.23.0's code, 9 KB compressed instead of 32; a fixture only for a release that changes how files are written. **Fixed:** `trunkdb info` panicked when its reader went (`\| head`); it ends quietly, as `find` does | §120.9, §121–§125 |
| 0.25.0 | **A condition on an aggregate column:** `filter` after a column, SQL's `FILTER (WHERE …)`, the column taking in only the group's documents that meet it, the groups and other columns every match; any column, two ANDed, one before any column refused; decided from index entries where the index holds its fields, a leaf of counts at a time where its ends decide it; the column methods' docs saying their string is the column's name. **Fixed:** a database opened through a symlink found a WAL beside the link instead of the file's, so after a crash it missed committed batches, and a write through it could have the file's old WAL replayed over newer pages later; the link is now followed to the file before the WAL is named, a loop of links refused; `trunkdb export` through a link protects the file's WAL; a hard link documented as unsupported. **Fixed:** a checkpoint whose flush failed after it had cut the WAL left the WAL's length as before, so a failed log after it was taken back by growing the WAL with zeros, and the next open refused it; the length is 0 once the cut succeeds, and an undo never grows the WAL past its end before the failed log (a fix to §81). **Fixed:** a page whose slot directory ran past its end, the cells said to start further on and its checksum matching, panicked as it was checked, reading past the page; refused as damage before a slot is read, a seed for `open_raw` with it. **Fixed:** on Unix, a compaction opened `<database>.compact` emptying what was there, following a link: whatever file a link under that name pointed to was emptied, even where the compaction gained nothing; the file is now always a new one, as on Windows, under the first name free (a fix to §108). **Fixed:** `avg` rounded the exact total to a float before dividing it, so finite numbers whose total passed `f64::MAX` had an infinite mean, and other means could be a unit off; the exact total is now divided, then rounded once, never `-0.0` (a fix to §107) | §126–§131 |
| 0.26.0 | **A hint in the query:** `Filter::use_index(fields)` reads that index where the rules would read another, `Filter::no_index()` scans, `"_id"` the primary index; the rules choose the rest among it alone; a hint that can't be followed, no such index or nothing to read it by, is `Error::Hint` from every read, never a quiet fall back; `explain_run` says `hinted`; `use index …` and `use no index` last in a filter's text and `trunkdb find`; a panic on `false` under a hint, found by the `queries` fuzz target, fixed before release ; **the plans `explain_run` passed over:** `Explained::alternatives`, the filter hinted to each index, `_id` and a scan, each with the entries it would read, `Exactly` or `MoreThan`, whether it reads a document per entry, and whether that's fewer; ranges counted whole a leaf at a time up to 1,000 leaves, walks in sort order as far as the run read; `instead:` lines in `trunkdb find --explain`; **aggregate columns named last** (breaking): `count().named("n")`, `sum("tries").named("total")`, a column's `filter` before its name, an `UnnamedColumn` until it has one, so a column without a name or a `filter` before any column doesn't compile | §132–§134 |
| 0.27.0 | **A hook after each commit:** `Database::subscribe`, a `Changes` receiving a `Commit` for every batch committed after it, in commit order, once it is durable and new reads see it, never for a failed batch; each document once with the batch's net effect, `Inserted`, `Updated` or `Deleted`, `drop_collection` one `Dropped`; ids only; an unbounded `std` channel, so a writer never waits and a subscriber may write; nothing recorded without a subscriber; **a condition of the application's own:** `Condition::custom(fields, test)`, a closure `Fn(&Document) -> bool + Send + Sync` given a document of the fields it names, never more; it never bounds an index read nor is decided on an entry; a panic in it fails the call with `Error::ClosurePanicked`, a write with nothing written, the writer's lock not poisoned; no text form, the parser refusing `custom(…)`; readers on `Document`: `get(path)`, `as_bool`, `as_i64`, `as_f64`, `as_str` and the others; **a panic in `update_many`'s change** or in its `T`'s conversions, under the writer's lock, fails the call with `Error::ClosurePanicked`, nothing written, the lock not poisoned; **live queries:** `Collection::watch(filter)`, a `LiveQuery<T>` reporting first what matches, then for each commit the documents `Entered`, `Left` or `Changed`, with their id and the document as that commit left it; checked inside the commit against the documents it changed, so no transition is lost; a sort, a skip or a limit refused with `Error::NotLive`; a panic in a `custom` condition ending that query alone; **live aggregates:** `Collection::watch_aggregate(filter, &aggregate)`, a `LiveAggregate` reporting every row, then for each commit to the collection the rows `Entered`, `Left` or `Changed`, each with its group values; the aggregate run again whole inside the commit; **a window:** `Aggregate::window(path, duration)`, the documents within `duration` of the newest date-time among the matches, for `aggregate` and `apply` too, refused beside a filter's sort, skip or limit | §135–§139 |
| 0.28.0 | **`check` finding cells that share bytes**, which a page's own check on reading leaves out; **update operators** `mul`, `pop_first`, `pop_last` and `push_all`; **`except`**, every field but these, `select * except …` in the text; **the update operators in a `Batch`**, by id, and **`upsert_fields`**, inserting from the filter's `eq`s, with `set_on_insert`; **a live query with a sort and a limit**, "the ten fastest vans", each event with its place in the list, a skip still refused; **aggregate columns `count_distinct`, `first` and `last`**, the last two by id or `by` a field, "each van's latest position"; **`Aggregate::unwind`**, one document per element of an array, MongoDB's `$unwind`, read from documents; **rows of the application's own**, `aggregate_as::<R>` and `watch_aggregate_as::<R>`, a `LiveAggregate<R>` of `LiveRow<R>`, converted on the reader's thread; **faster batched writes**, each step profiled and measured: a binary search within B-tree pages, stepping past the tombstones older leaves may hold; a page changed again in its batch not copied; a page changed through `SlottedPage` keeping its layout mark; a grown document made room for in place, where 83–96% of updates compacted their whole page. Together, on 100,000 documents, 1,000 a commit: inserts 32k/s → 42k/s, updates 13k/s → 15k/s, deletes 27k/s → 31k/s, compaction 1.1 s → 0.62 s, reads unchanged | §141–§148, §150–§153 |
| 0.29.0 | **A sort by a key of the application's own:** `Filter::sort_by_key(fields, key)`, smallest first, and `sort_by_key_desc`, a closure `Fn(&Document) -> Option<f64>` given the fields it names, as `Condition::custom` is; `None` or NaN leaves a match out, for `count` too; equal keys by the `then_*` fields after it, then by id; with a limit, only the best `skip + limit` kept as the matches are read; never from an index, conditions an index serves narrowing what is read; in `find`, `count`, `cursor`, the writes, `aggregate` and `watch`, a live list of the ten nearest; a panic fails the call with `Error::ClosurePanicked`; written `order by key(lat, lon)`, which the parser refuses. "The ten restaurants nearest to this point", the nearest vectors among thousands, any score of the application's; **`trunkdb count` and `get`**, and `--json` for `info`, `check` and `fields`, for a program or an AI agent; **writing through the command:** `trunkdb insert`, `update` with MongoDB's operators as JSON and `--upsert`, `delete`, `index add`/`drop`, `drop`, each one batch; a write that would change every document asking for `--all`, `--dry-run` counting; **aggregates as text**, `count() as n, avg(hours) as mean group by team having n > 10`, `FromStr` and `Display` for `Aggregate`, and `trunkdb aggregate`; **the agent skill after a trial**, ten requests by an agent given only the skill, reads and writes, all right, what it had to work out alone added: which conditions an index serves, strings and prefixes, values of another type, two collections put together by id | §154–§157, §122.5 |
| 0.30.0 | **`starts with`:** `Filter::starts_with(field, prefix)`, `title starts with "Fix"`, a string beginning with a prefix, case counting as for `<`; read from an index as the keys beginning with the prefix, alone or after a compound index's `=`s, and decided on an entry where the key holds the prefix, so a count reads no document; **a test of a value's kind:** `Filter::is_kind(field, Kind::String)`, `hours is string` and `is not`, by the names `trunkdb fields` prints, `number` for either number, no null kind beside `is null`; `[*]` for elements; checked on the documents, never from an index; **an `in` on a compound index's field, a range per value:** `status in ("open", "late") and due < 50` on `(status, due)` reads each status's entries before 50, as `=` reads one status's, where it read each status whole; after the `=`s on the fields before it, with the `=`s and a range on the fields after it; chosen as `=` is; **the public API gone through again**, as in §118, for what came since: `Kind::ALL` a slice and `Kind::DateTime` for a date-time, so a date without a time can be added; the hook's `Commit` and `Change` no longer `Eq` (breaking), as reports aren't | §158–§161 |
| 0.31.0 | **A batch's preconditions:** `Batch::require(&collection, filter)`, at least one document matches, and `Batch::require_none`, none does, checked at `commit` at their place in the batch, after the ops before them and under the writer's lock, so no write comes between the check and the batch's own; a failed one writes nothing and returns `Error::PreconditionFailed { collection, op, matched }`; with `Filter::id`, one document held to what was read, a version the program keeps; read as `count` reads, through an index, stopping at the first match; a sort, skip, limit or selection refused **Parameters in a filter's text:** `:name` where a value or a count stands, `status = :status limit :n`, the values passed apart so none can change what the text says; `Template<Filter>`, `Template<Condition>` and `Template<Aggregate>` read and check a text once, `bind(&Params)` makes the filter, `params()` lists the names; a missing value, an unused one or one that doesn't fit a `BindError`; `trunkdb … --param name=value` for `find`, `fields`, `count`, `aggregate`, `update` and `delete` **An OR of ids and indexed conditions:** `_id in (…) or updated > t` reads the ids from the primary index and the rest from an index range, each document once, where it was a scan; `IndexUnion` with `_id` first among its `fields`; a branch neither naming ids nor indexed, or a hint, as before | §162–§164 |
| 0.32.0 | **Backups a user can trust:** an export ends with a line of its counts, export version 3, which an import checks, so one cut short fails instead of restoring in part; `Summary::ended`; `verify_export` and `trunkdb verify`, a restore into a file of its own, `check`ed, counted and deleted, a `Verified` report; a section of the guide on backups while running, restores, copying the file and its WAL, and moving between releases; 0.31.0 and older refuse version 3 **Binary in indexes, in the order of its bytes:** compared byte by byte as SQLite compares a BLOB, `<`, ranges and `starts with` a binary prefix, in the text as `{"$binary": …}`; sorted after date-times, where it came last with arrays and objects; an index key, so `=`, ranges, prefixes and sorts read just their entries, a count and a selection none, a unique index refusing the same bytes twice; file format 13, an older file's indexes holding binary rebuilt at its first open; on 100,000 file paths, binary instead of hex text a quarter of the file and a third of the index smaller **An OR of `Eq`s merged by id:** a sort by `_id` with a limit after an OR or an `in` whose ranges `Eq`s fix whole, one index or several, walks the ranges side by side and merges them by id, stopping at the limit, where it read every match and sorted them; `_id` comparisons narrow each range, so paging by the last id works through `tenant in (…)`; `QueryPlan::IndexUnion` says so in `in_order` | §165–§167 |
| 0.33.0 | **Writes by a filter in a batch:** `Batch::update_many(&collection, filter, &update)`, the operators made to what the filter finds at `commit`, and `Batch::delete_many`, at their place among the ops, under the writer's lock, sort, skip and limit counting; `commit` returns a `Written`, how many documents each op wrote; `Batch::dry_run`, every op run and rolled back, the counts or the error `commit` would give. **`trunkdb batch <file> <ops.jsonl \| ->`:** one op a line as JSON, `insert`, `update`, `delete`, `require` and `require_none`, committed whole or not at all, with `--param`, `--create` and `--dry-run`, each new id printed, what each line wrote, and the line a failure came from. **A regex:** `Filter::regex(field, Regex)`, `notes matches "time-?out"` in the text, the pattern a parameter too, through the `regex` crate, whose matching is linear in the text, behind the `regex` feature, on by default; a `Regex` is compiled once, when made, at most 4,096 bytes and 1 MiB compiled; a pattern beginning `^` and fixed text, `^Fix`, reads an index as `starts with` does, any other every document | §168–§169 |
| 0.34.0 | **Field names stored once, numbers as varints:** one dictionary of field names for the file, kept in the catalog, and a stored document's keys as numbers into it; ints, lengths and counts as varints; a name of at most 64 bytes, among the first 4,096, joins, any other key stays inline; schemaless still; `compact` keeps only the names in use; older documents read as they are and are stored anew when written. **Index leaves stored after an anchor:** each leaf keeps one anchor key, its middle one when built, and each entry the bytes it shares with it, then the rest, the location as varints; every entry reads on its own, so the binary search, walks, inserts and removes work as they did; older leaves packed at their first insert or by `compact`. File format 14, which 0.33.0 refuses; on the bench's 100,000 tasks the file 42% smaller, on 150,000 file paths 37%. **Walks that pass part of a leaf:** into the first leaf by a binary search, the range's end checked once a leaf, a key copied only where it differs; a count, and a skip through an index, pass the part of a leaf inside the range at once, so paging, skips and counts got faster than before | §170–§172 |
| 0.35.0 | **A date, and a time of day:** `Date`, a day in the calendar, and `Time`, a time of day to the nanosecond, as `Document::Date` and `Document::Time`, a struct's fields of those types stored as them; each compares only with its own kind, never with a date-time; keys in an index in their order, sorted after binary; `date("2026-11-07")`, `time("14:05")` and `datetime("…")` in a filter's text, `is date` and `is time`, `date` naming dates now where it named date-times; `{"$date": "2026-11-07"}` and `{"$time": …}` in JSON, export version 4; `DateTime::date()`, `time()` and `Date::at`; file format 15; **a window for each group:** `Aggregate::newest_per_group(path, n)`, each group's `n` newest by `path`, and `window_per_group(path, duration)`, each group's documents within `duration` of its own newest, so a group gone silent keeps its last ones; beside `window`, which comes first; live too; `window at 10 per group` and `window at 5m per group` in the text; **an upsert in a batch:** `Batch::upsert_fields`, at its place among the ops, `Written::upserted` saying whether it inserted and the id; an `upsert` line for `trunkdb batch`; **a sort with a limit kept small:** a sort by fields no index serves keeps only the best `skip + limit` as it reads, the index entries streamed, close to twice as fast, memory as small as the limit | §173–§176 |
| next | **Text a program passes, read by the library:** `Update` from MongoDB's operators as JSON, `Params` from an object, `BatchLines`, a batch's ops read from JSON lines with each mistake's line, the batch made, a failed commit's line found; the command reads through them; **the C ABI:** `trunkdb-ffi`, trunkdb as a C library and its header, 36 functions over documents as JSON and filters as text, opaque handles checked by a tag, error codes and the last error as text and JSON, no panic across; tested from C under AddressSanitizer; **the C# wrapper:** `Database`, `Collection<T>` written by the object, `Snapshot`, `Cursor<T>` over safe handles, documents as a program's classes through System.Text.Json, `DateOnly`, `TimeOnly`, `byte[]` and `DocId` as their tags, errors as exceptions | §177–§180 |

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

A third pass over the whole public API, as §118's and §161's, comes
before 1.0.

## Open
In three groups: what is planned, what is deferred until a reason
comes, and what is not planned. Unordered within each group; each line
says where the need or the limit is described. None of it changes a
file today's build writes, or what a program or query means today.

Not decided: whether trunkdb goes on crates.io, with its docs on
docs.rs, or stays on GitHub alone, as now.

### Planned
- **A C ABI,** `trunkdb-ffi`, with a header made by cbindgen: the way
  in for every language that calls C, C# through P/Invoke, Swift, Go,
  Java, Node, Dart. Handles for a database, a cursor and a snapshot;
  documents in and out as tagged JSON, filters and aggregates as text
  with parameters (§163), updates as JSON operators, so about 35
  functions carry the whole query language; errors as codes and a last
  error's text; no panic across the boundary. Closures (custom
  conditions, a sort by key, `update_many`'s change) have no text and
  stay out. A thin wrapper per language on top, C# first. Built on one
  embedding API inside trunkdb that the PyO3 bindings share. Designed
  in §177, before the code; the library and its header built (§178,
  §179), and the C# wrapper (§180). Next: the files on each GitHub
  release (§177.12).
- **PyO3 bindings,** for Python apps: `pip install` from wheels,
  documents as dicts, errors as exceptions, on the embedding API the C
  ABI uses.
- **Longer fuzzing runs,** with AddressSanitizer too, and short ones in
  CI on nightly (§55.6).

### Deferred
Each waits for what its line names: a program that needs it, or a
measurement that shows the cost. "When asked" means a program that
uses trunkdb asking for it.

**Queries**
- Leaves decided whole for filters that accept several intervals of
  keys (`ne`, an OR, a NOT), each leaf checked against the intervals'
  bounds; they decide entry by entry, on bytes, since §104 (§104.6).
- A scan, 1.3× SQLite's: each document's filtered fields are decoded
  into a small object to be checked (§99); checking them as they are
  decoded would save that.

**Changes as they happen** ([§140](spec/140-changes-as-they-happen.md))
- A window counted back from the clock, with a timer to run the
  aggregate again as readings age out: a second source of events
  (§139.2).
- A window per group read through an index on `(group, path)`, each
  group's newest sought: until a measurement asks for it, as every
  match is read now (§174.4).
- The newest `n` of each group as documents, in `find`, a filter's
  limit per group: when asked; aggregates have it (§174.6).

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
- Batched writes: inserts level with SQLite's (43k/s against 39k/s,
  §152.2), updates too (15k/s against 14k/s, §153), still behind redb
  and sled: a batch of 1,000 updates writes about 2,160 whole pages.
  Next, the WAL record written from the pages instead of copied into
  one buffer (4–5%); then a checkpoint without the WAL's own flush, a
  generation number in its header and records. WAL records of a page's
  changed bytes (§149), designed as WAL version 4, gave 4.2× fewer WAL
  bytes but only about 15% speed; whether to build them is open
  (§149.8).

- A native backup while the application runs, `Database::backup_to(path)`:
  a snapshot written as one compacted file without a WAL, as SQLite's
  `VACUUM INTO`, restored by opening it. The export stays the portable
  backup, as a native file opens only in the same build or a newer one;
  when asked.

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
- An unwind read from a multikey index (§147.5), "tasks per tag" on
  `tags[*]` without the documents: not exact as the index stands, which
  holds no entry for a value that isn't an array or an element of a
  kind no index holds, and one for a value repeated. With a marker entry
  for each document whose entries don't stand for its array whole, file
  a format bump and multikey indexes rebuilt at the first open, every
  column would be: if a collection makes per-element aggregates slow.
- A document as `serde_json::Value`, beside its text form (§71), if a
  tool needs it: behind a feature named `serde_json-1`, so `serde_json`
  stays out of the default public API (§71.4).
- Aggregates beyond §107, §146 and §147, when asked (§107.8): `push`, percentiles; `sum` from index entries, which wants an
  `Int`/`Float` bit and a missing mark in every key, a format bump
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
- Arrays trimmed to a field of their elements in a projection, when
  asked (§106.8): `items[*].sku`, which now selects `items` whole.
- `pull` by a condition on the element, when asked for (§73.3).
- A batch's precondition on a count, `require_count(filter, n)`, beside
  `require` and `require_none` (§162.5), when a program needs more than
  "at least one" or "none".
- A regex decided on an index's entries, where the key holds the whole
  string (§169.3): a count or a selection of `_id` by a regex would read
  no document. Each is checked on its document now, as `contains` is.
  If a program counts by a regex over many documents.

**Tooling and reach**
- Repairing what `check` finds, beyond export and import (§39.5).

### Not planned
Decided against, each for the reason given; the scope was first drawn
in §6.

- **A cost-based query planner:** fixed rules, a hint where they are
  blind to the data (§132), and `explain_run`'s alternatives (§133)
  cover plans bounded by one pass over one collection.
- **Joins:** a document holds what belongs together, and a reference
  (a `DocId` field, §62) is looked up by the application.
- **Multiple writers:** batches are serialized (§27); readers run
  beside the writer on snapshots (§77), and one process owns the file
  (§21).
- **Encryption:** left to the disk or the operating system.
- **Schema validation:** the application's own types are the schema,
  checked as documents convert to and from them.
- **Vector search:** done well, an index for nearest neighbours over
  millions of vectors would be a feature as large as the rest of
  trunkdb, if not its main one, and stores built for it exist (pgvector
  and others). trunkdb is not a vector store; a field of floats is
  stored as any other field. A sort by a key of the application's own
  (§154) finds the nearest few thousand exactly, by a scan.
- **A sort by `_id` through a compound index fixed only in part:** on
  `(status, created)` with only `status` fixed, the keys are in the
  order of `created`, and in id order only among equal ones (§103.1).
  A merge would need a range per value of `created`, which only reading
  the range tells; the sort in memory reads no more (§167.7).
- **Lookups into another collection, and a planner for them:** joins
  are not planned, so plans stay bounded by one pass over one
  collection, which rules serve.
- **Conditions on places:** a point within a circle or a polygon, as
  MongoDB's `$geoWithin`, with an index on points. Geometry runs deep
  (sphere or plane, the antimeridian, polygons with holes), and SQLite
  and PostgreSQL keep it in extensions (R*Tree, PostGIS).
  `Condition::custom` with a coarse key indexed, or the fences in the
  application's own memory, serves (§140.6).
- **A word index,** full-text search: words, languages and stemming
  are a product of their own, as vector search is. A regex (§169)
  searches text, reading each document (§169.6).
- **A version on every document, kept by trunkdb:** a format change and
  a cost to every write, where a program's own `version` field and
  `require` (§162.5) do the same.
- **A closure in a batch's `update_many`:** it would have to be
  `Send + 'static` and run with every write waiting (§168.1). A read by
  id and a write held by `require` serve.
- **Updates as text** (§111.11): updates are MongoDB's operators in
  JSON (§156), in the command and through the C ABI; a third form adds
  nothing.
- **A count of all documents kept in the catalog:** counted leaf by
  leaf it takes 0.09 ms for 100,000 (§101), and a total would cost a
  write to the catalog page in every batch that inserts or deletes.
- **`chrono`, `time` and `jiff` types recognized as times:** they
  serialize as strings (§69.5, §173.5), so each field would still need
  an annotation, and the public API would be tied to three crates, one
  of them before 1.0, for little gain. Store trunkdb's `DateTime`,
  `Date` and `Time`, and convert through their text (GUIDE.md, "With
  `chrono` or `jiff`"; `tests/other_date_crates.rs`).

Aggregates were on this list until §106.7, and came in §107.
