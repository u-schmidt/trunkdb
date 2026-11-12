# trunkdb — changelog

Every step so far, earliest first, grouped by the release that shipped
it. Section numbers (§) refer to [the spec](spec/README.md), which
records why each step was taken; [ROADMAP.md](ROADMAP.md) has the same
releases as a table, and what's still open.

It began as 0.1.0, the v0 core (§6–§18): pages, the catalog, a B-tree
primary index, document encoding and the serde bridge, filter, sort and
limit, batches and a write-ahead log. Since then:

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

    Items 60–74 → 0.20.0: file format 12, which still opens 6–11
    holding only objects; 0.19.0 refuses a file 0.20.0 has opened, and
    a WAL 0.20.0 left after a crash. Breaking: `Collection<T>` for a `T`
    that isn't a struct or a map no longer stores anything (§95); code
    that caught a foreign or unreadable file as `Error::Io` with
    `InvalidData` gets `Error::Format` instead (§92). `explain` says
    `IndexOrder` where a sort by `_id` after an `Eq` now walks the index
    (§103). Exports as in 0.14.0.

75. **A differential fuzz target for queries** (§105): `fuzz/`'s
    `queries` target reads any bytes as a program of index changes,
    writes, compactions, reopens, snapshots and reads, and checks every
    read, through `find`, typed reads, the cursor, `find_one`, `count`
    and `explain_run`, against the filter applied in memory. A smoke run
    on stable runs it on random programs in CI. It found a leak: a
    current data page emptied by deletes, given up for a new one when a
    large document didn't fit beside its slot directory, was never freed
    (`check` reported it, `compact` reclaims it); now it is. And the
    other fuzz targets' starting database hadn't built since §95.

76. **Projection, a select of fields** (§106): `Filter::select(["name",
    "address.city"])` hands out only those fields and `_id`, in the
    order selected, nested as stored; the rest is skipped, not decoded.
    Where an index's entries decide the filter and hold every selected
    field, a match comes from its entry without its document being read:
    booleans, strings, ids and date-times; numbers and nulls are read
    from the document. `explain_run` counts those (`from_entries`) and
    names the selection. Writes ignore it, `update_many` too. Aggregates
    are no longer "not planned": §106.7 takes them up again.

77. **Aggregates** (§107): `Collection::aggregate(filter, &Aggregate)`,
    the documents the filter finds put in groups (`group_by`), with a
    `count`, `sum`, `min`, `max` and `avg` per group, then `having`, a
    row sort, `skip` and `limit`; `Aggregate::apply` the same in memory.
    Sums are exact in any order. Index entries answer `count`, `avg`,
    `min` and `max` where the index holds the fields, and a walk counts
    whole leaves of a group. `explain_aggregate` says how it ran, with
    the groups it made.

    Items 75–77 → 0.21.0: nothing breaking; `Filter::select`,
    `aggregate`, and the new fields of `Explained` and `ExecutionStats`,
    which are `#[non_exhaustive]`, are additions. Files and exports as
    in 0.20.0.
78. **Compaction in memory that doesn't grow with the file** (§108):
    the new image is built in a file beside the database,
    `<database>.compact`, logged to the WAL from there and copied into
    the database at the checkpoint, where it was held in memory. A
    compaction from 2.1 GB to 1.1 GB takes 0.1 GB of memory instead of
    1.25, in the same 11 s; with a 256 MiB cache, 0.4 GB instead of 1.5.
    The disk needs room for the new file twice more until it's done.
    No file is left behind, after a crash either.

79. **`check` in memory that doesn't grow with the file** (§109): it
    reads documents one at a time and checks each index entry against
    the document it names, where it held every document and every key.
    On a file of 1.1 GB: 0.3 GB instead of 1.75 with the default cache,
    in the same 2.5 s; 3 MB instead of 1.3 GB without one, in 5 s
    instead of 2.6. The same problems found.

80. **A fix to aggregates** (§110): grouped through an index whose
    group fields aren't its last, a row could show its group's number
    as an earlier match had it, `3` where the first document by id has
    `3.0`. Found by the `queries` fuzz target in CI.

81. **Filters as text** (§111): `text.parse::<Filter>()` and
    `filter.to_string()`, in a language like SQL's `WHERE`: `status =
    "Complete" and seen >= 10 order by started_at desc limit 1`, with
    `select`, `is null`, `exists(…)`, `size(…)`, `has (…)` for one
    element, `not`/`and`/`or`, values as JSON with `id("…")` and
    `date("…")`, and names in backticks. It says what the builders say
    and compiles to the same `Filter`. `Condition` has both too;
    mistakes come back as `ParseFilterError`, with line and column.

82. **`trunkdb find`** (§112): `trunkdb find <file> <collection>
    [filter]` prints what a filter in text (§111) finds, one JSON
    document per line, the count to stderr; read-only, streamed, quiet
    when its reader goes (`| head`). A filter's mistake is shown under
    its line with a `^`.

    Items 78–82 → 0.22.0: nothing breaking; `FromStr` and `Display`
    for `Filter` and `Condition`, and `ParseFilterError`, are additions.
    Files and exports as in 0.21.0.

83. **`trunkdb find --explain`** (§113): how a query ran instead of
    what it found: the plan, each index read with its bounds in the
    filter language, the sort, and `explain_run`'s counts and time.

84. **`#[trunkdb::document]`** (§114): with the `derive` feature,
    `#[id]` on a struct's id field instead of `#[serde(rename =
    "_id")]`, refused at compile time on a field that isn't a `DocId`
    or an `Option<DocId>`, or on a second field; a macro crate of its
    own, `trunkdb-derive`, as serde has `serde_derive`.

85. **`in`** (§115): `Filter::is_in("status", ["open", "blocked"])`
    and `Condition::is_in`, SQL's `IN`: the OR of an `eq` per value,
    read as one point of an index per value, by id on `_id`; `status
    in ("open", "blocked")` and `not in` in the text.

86. **A map of its own** (§116): `Document::Object` holds a
    `trunkdb::Map` instead of an `IndexMap<String, Document>`, so
    `indexmap` is no longer part of trunkdb's API and can change
    without breaking a program. The same methods and order: a field
    set again keeps its place, `remove` keeps the others' order.

87. **Variants that can grow** (§117): `Condition`'s `Compare`,
    `Exists`, `Size` and `ElemMatch` and `QueryPlan`'s `Index`,
    `IndexOrder` and `IndexUnion` are `#[non_exhaustive]`, so a field
    can be added to one without breaking a program.

88. **The public API, gone through** (§118): options and reports no
    longer `Copy` or `Eq`, so they can get any field in 1.x;
    `Document::nests_too_deep` and `Filter::apply_to` private; a
    `Document` no longer a serde `Deserializer`, `into_value` the one
    way to a `T`; the `fuzzing` feature declared outside semver.

89. **Unique indexes in the `queries` fuzz target** (§119), checked
    with the database's own filters. **Fixed**, found by it: an update
    that changed a long string only past where its key is cut (about
    1000 bytes in one field, less in a compound index) wasn't checked,
    and could store a duplicate in a unique index. `check` reports such
    a duplicate in an older file; changing or deleting one of the two
    documents fixes it.

90. **Old files as fixtures** (§120): a database this release wrote,
    every kind of value, overflow pages, an index of each kind, free
    space and a dropped collection, kept in `tests/fixtures/0.23.0/`
    with what it holds; every build opens it, reads it as written,
    queries it through each index and writes to it. Each release adds
    its own. The minimum Rust version's policy written down in the
    README: it rises only when a dependency or a real need requires it,
    and that isn't breaking.

    Items 83–90 → 0.23.0. **Breaking:** `IndexMap<String,
    Document>` becomes `trunkdb::Map`, `IndexMap::new()` `Map::new()`,
    `shift_remove` `remove`; `IndexMap`-only methods (`entry`, `first`,
    `get_index`, …) are done with `get`, `get_mut`, `insert` and
    `iter`. Those seven variants are matched with `..` and built with
    `Condition`'s methods; a plan is checked with `matches!` instead of
    `==` against one built by hand. Options and reports are `.clone()`d
    where they were copied; `T::deserialize(document)` is
    `document.into_value::<T>()`. The `derive` feature is off unless asked for; `is_in` is an
    addition. Files and exports as in 0.22.0.

91. **`trunkdb fields <file> <collection> [filter]`** (§121): every
    path a collection's documents have, as a filter writes it
    (`address.city`, `tags[*].k`, `` `first name` ``), in how many
    documents, and as which kinds of value, most first: what a
    schema-less file looks like, for a person or an AI meeting it. A
    filter sums up only what it finds, so `limit 1000` is a sample.
    Read-only; at most 10,000 paths, and a notice past them.

92. **A skill for AI agents** (§122): `skills/trunkdb/`, an Agent
    Skill in the open format Claude Code, Codex and others read. It
    teaches an agent with a shell to look into a trunkdb file through
    the command: `info`, then `fields`, then `find` with `select` and
    `limit`, counts without the documents, `export` only as a backup;
    the filter language in a file of its own, read when needed. Not in
    the crate.

93. **Batched writes, a checkpoint less often** (§123): the default
    `checkpoint_pages` is 4,000 instead of 1,000, so the WAL limit that
    follows it is 65.5 MB instead of 16.4. A commit of 1,000 documents
    into large indexes leaves 800–1,250 pages waiting, and 59 commits
    of 100 paid a checkpoint, now 10. On the benchmark's 100,000
    documents: inserts about 1.35× faster, updates 1.35×, deletes 1.6×;
    single commits and reads unchanged. It costs up to 33 MB of waiting
    pages instead of 8, a WAL up to 65.5 MB, 79 ms of recovery after a
    crash instead of 34. A compaction's commit now writes it back at
    once whatever its size, as §108 had it: under the new threshold a
    small image would have waited in the WAL. A caller who set either
    option sees no change; no file or WAL format moved.

94. **A fix to `in` as text** (§115): an OR of `eq`s of one field whose
    branches came wrapped alone in an AND or OR printed as
    `a = "s0" or a = 0`, which reads back as `in` and prints so; the
    text didn't read back the same. It prints as `in` now, as an OR of
    bare `eq`s does. Found by the `queries` fuzz target.

95. **`=` and `!=` on arrays, objects and binary** (§124): compared by
    their contents, where before `=` never held for them and `!=`
    always did, even against a document holding exactly that value.
    Arrays element by element, objects field by field in any order,
    binary byte by byte, single values inside as `=` sees them (`[1]`
    equals `[1.0]`): the equality `add_to_set` and `pull` already used,
    now one function for both. `in`, `not in` and `has (@ = …)` with
    such values follow; `<`, `<=`, `>`, `>=` still never hold for them.
    **Changed answers:** a filter with `!=` and such a value finds fewer
    documents than before, and aggregates put equal ones into one group
    (`{"a": 1, "b": 2}` with `{"b": 2, "a": 1}`). Reads through a
    compound index now read the document where an entry holds such a
    value, which it can't tell apart from its key. Unique indexes still
    don't check these values. No file format or API change.

96. **The release that wrote a file** (§125): the header names the
    trunkdb release that created the file and the one that wrote it
    last, three `u16`s each after the fields that were there, from
    `Cargo.toml`'s version. A new file, an import's too, is this
    build's for both; the first page a batch of another release writes
    stamps "written by", once; `compact` keeps "created by". Unknown,
    `None`, for a file from before, and where an older build wrote the
    header since: it zeroes them. `FileInfo::created_by` and
    `written_by`, a new `Release` type and `trunkdb::RELEASE`; `trunkdb
    info` prints `written by 0.24.0, created by 0.24.0`. Files stay
    format 12, which older builds still open.

97. **A fix to `trunkdb info`**: its reader gone, as `| head` goes once
    it has its lines, it panicked (`failed printing to stdout: Broken
    pipe`); it ends quietly now, as `find` does (§112). Its lines are
    gathered and written at once, and what was gathered before a
    failure is still printed.

98. **Smaller fixtures** (§120.9): the generator writes 60 tasks
    instead of 300 and overflow documents just over a page, keeping
    every kind of value, every kind of index, a branch, free space and a
    dropped collection. The 0.23.0 fixture was written again by
    0.23.0's own code, 168 KB instead of 272, 9 KB compressed instead of
    32. A release adds a fixture only where it changes how files are
    written.

    Items 91–98 → 0.24.0: nothing breaking at compile time;
    `FileInfo::created_by` and `written_by`, `Release` and `RELEASE`
    are additions. **Changed answers:** `!=` with an array, object or
    binary value finds fewer documents than before, and aggregates put
    equal ones into one group (§124). The default checkpoint threshold
    is 4,000 pages (§123). Files stay format 12 and exports as in
    0.23.0, which opens what 0.24.0 writes; a header 0.23.0 writes again
    loses the releases (§125.3).

99. **A condition on an aggregate column** (§126): `filter` right after
    a column, SQL's `FILTER (WHERE …)`, so the column takes in only the
    group's documents that meet it, while the groups, the row's values
    and the other columns still see every document the filter finds.
    `.count("tasks").count("open_tasks").filter(Condition::eq("status",
    "open"))` gives each team both counts; a team without an open task
    is still a row, `0` there. Any column takes one, two are ANDed, and
    a `filter` before any column is refused. Index entries decide it
    where the index holds the fields it names; a leaf of counts is
    counted whole where its ends decide it. The docs of every column
    say that its string is its name in the row, not something matched.
    No file format change; aggregates without a condition answer and
    run as before.

100. **Fixed: a database opened through a symlink** (§127). The WAL
    was named after the path given, so an open through a link found a
    WAL beside the link, not the file's. After a crash it missed the
    batches still in the file's WAL, and writes through the link went
    into a second WAL, so a later open of the file could replay old
    pages over newer ones. `Database::open` now follows a symlink to
    the file it names before the lock, the WAL or compaction's file is
    opened; links to links too, a loop or more than 40 refused with
    `InvalidInput`. `trunkdb export` through a link refuses the file's
    WAL as well. A hard link to the database is documented as
    unsupported.

101. **Fixed: a failed log after a failed checkpoint** (§128, a fix to
    §81). A checkpoint that cut the WAL and then failed to flush kept
    the WAL's old length, and a log that failed next was taken back by
    growing the empty WAL to that length with zeros; the batches logged
    after them were committed, but the next open refused the WAL. The
    length is now 0 as soon as the cut succeeds, and an undo never
    grows the WAL past where the file ended before the failed log.

102. **Fixed: a slot directory past the page's end** (§129, a fix to
    §55). A page with 2,044 slots or more and its cells said to start
    after them got past the check that the directory ends before the
    cells, and the check then read its slots past the page's end and
    panicked. It takes a checksum that matches, so a crafted or
    miswritten file, not a bit flip. That the cells start inside the
    page is now checked first, so the open fails with `InvalidData`.
    The fuzz corpus has the page as an `open_raw` seed.

103. **Fixed: compaction emptied what was under its file's name**
    (§130, a fix to §108). On Unix the image's file, `<database>.compact`,
    was opened to be emptied: a file left there was emptied, and so was
    any file a link under that name pointed to, even where the
    compaction then found nothing to gain. It is now always made new,
    as on Windows, under the first free name (`.compact.1` and so on
    after it), and only that name is removed. What was there stays.

104. **Fixed: `avg` rounded twice** (§131, a fix to §107). The exact
    total was rounded to a float and then divided by the count: two
    `1e308` averaged to infinity, the total being past `f64::MAX`, and
    other means could be a unit in the last place off. The exact total
    is now divided, then rounded once, and a mean is never `-0.0`.
    **Changed answers:** such means are now finite, and some differ
    from before in their last bit; means of integers below 2^53, and of
    floats whose total is exact, are as before.

    Items 99–104 → 0.25.0: nothing breaking at compile time;
    `Aggregate::filter` is an addition. **Changed answers:** some
    means differ from 0.24.0's in their last bit, and means whose
    total passed `f64::MAX` are finite (§131). **Moving up:** a
    database opened through a symlink now uses the WAL beside the file
    the link names (§127); one that 0.24.0 left after a crash, opened
    through a link, has its last batches in the WAL beside the link, so
    open it once with 0.24.0 and close it before moving up. A file left
    under `<database>.compact` stays where it was (§130). Files stay
    format 12 and exports as in 0.24.0, which opens what 0.25.0 writes;
    no new fixture, as nothing changes how files are written (§120.9).

105. **A hint in the query** (§132). `Filter::use_index(fields)` reads
    the index on those fields where the planner's rules would read
    another or none: an `Eq` on a value most documents have, beside a
    range that holds a few. `Filter::no_index()` scans, and
    `use_index("_id")` is the primary index. The rules choose the rest
    among that index alone. A hint that can't be followed, no such
    index or nothing to read it by, fails every read with the new
    `Error::Hint`, `explain` included; it is never dropped quietly.
    `Explained::hinted` says the plan was hinted. As text, last in a
    filter: `use index created`, `use index (status, created)`,
    `use no index`, also for `trunkdb find`. A field named `use` is
    still a field.

106. **The plans `explain_run` passed over** (§133). `Explained` has
    `alternatives`: the filter hinted to each index, to `_id` and to a
    scan, and for a hinted run the planner's own plan. Each says the
    index entries it would read (`EntryCount::Exactly` or `MoreThan`),
    whether it reads a document per entry, and whether that's fewer.
    Index ranges are counted whole, a leaf at a time without a
    document read, so a hinted read of 40 shows the 40,000 the rules'
    plan would read. Only past 1,000 leaves is it `MoreThan`. A walk
    in sort order is walked as far as the run read. The run's own
    numbers don't change. `trunkdb find --explain` adds an `instead:`
    line for each.

107. **Aggregate columns, named last** (§134), breaking. A column says
    what it computes, then its name: `count().named("n")`,
    `sum("tries").named("total")`, and the same for `min`, `max` and
    `avg`. `count("open")` read as what is counted, as SQL, LINQ, Spark
    and Polars write `count(x)`. Until `named`, a column is an
    `UnnamedColumn`, with `filter` for its condition (§126), given
    before the name: `count().filter(c).named("open")`.
    `Aggregate::filter` is gone. A column without a name, or a `filter`
    before any column, no longer compiles; the latter was refused when
    the aggregate ran. To adapt: `count("n")` becomes
    `count().named("n")`, `sum("a", "n")` becomes `sum("a").named("n")`,
    and a column's `.filter(c)` moves before its `.named(..)`. Files and
    results don't change.

    Items 105–107 → 0.26.0: **breaking** for every aggregate, its
    columns named last (§134); the changelog's item 107 says how to
    adapt, the same way at each column. Hints and the plans
    `explain_run` passed over are additions; a filter without a hint
    reads as before. Files stay format 12 and exports as in 0.25.0,
    which opens what 0.26.0 writes; no new fixture, as nothing changes
    how files are written (§120.9).

108. **A hook after each commit** (§135). `Database::subscribe` hands
    out a `Changes`, which receives a `Commit` for every batch committed
    after it, in commit order, sent once the batch is durable and new
    reads see it; a failed batch is never reported. Each document is
    listed once, with the batch's net effect: `Change::Inserted`,
    `Updated` or `Deleted`, with its collection and id;
    `drop_collection` is one `Dropped`. Ids only. `recv`, `try_recv`,
    `recv_timeout`, or a `for` loop, which ends when the database is
    dropped. The channel is `std`'s and unbounded: a writer never
    waits, so a subscriber may write. Nothing is recorded while no one
    subscribes.

109. **A condition of the application's own** (§136).
    `Condition::custom(fields, test)` takes the paths a closure reads
    and the closure, `Fn(&Document) -> bool + Send + Sync`, for a test
    no operator covers: a point within a fence, two fields compared.
    The closure gets a document of the fields it names, never more. It
    never bounds an index read; ANDed with conditions that do, it runs
    on what they leave. A panic in it fails the call with the new
    `Error::ClosurePanicked`: a write writes nothing, and the database
    goes on. A check in memory (`Filter::matches`) panics on. It has no
    text form: `Display` writes `custom(a, b)`, which the parser
    refuses. `Document` gets readers: `get(path)`, `is_null`,
    `as_bool`, `as_i64`, `as_f64`, `as_str`, `as_bytes`, `as_datetime`,
    `as_id`, `as_array`, `as_object`. `Condition` has a new variant,
    `Custom`; it is `#[non_exhaustive]`, so a `match` with a `_` arm
    still compiles.

110. **A panic in `update_many`'s change** (§137), or in the `Serialize`
    or `Deserialize` of a typed collection's `T` while `update_many` or
    `update_fields` writes, no longer poisons the database. It ran
    under the writer's lock, and every later write failed with
    `Error::Poisoned` until the database was opened again. Now the call
    fails with `Error::ClosurePanicked`, nothing is written, and the
    database goes on. That error is item 109's, named for both.

111. **Live queries** (§138): `Collection::watch(filter)` keeps a filter
    as a subscription. A `LiveQuery` first reports every document that
    matches as `Entered`, then, for each commit, the documents that
    came to match (`Entered`), no longer match (`Left`) or were written
    and still match (`Changed`), each with its id and the document as
    that commit left it. Each commit is checked as it commits, so a
    fence entered and left between two reads is reported both times.
    A sort, a skip or a limit is refused with the new
    `Error::NotLive`. New types: `Live`, `LiveQuery`.

112. **Live aggregates, and a window** (§139):
    `Collection::watch_aggregate(filter, &aggregate)` keeps an
    aggregate as a subscription. A `LiveAggregate` first reports every
    row as `Entered`, then, for each commit to the collection, the rows
    that came into the result, left it, or changed their values. With
    a `having` (`mean > 0.8`), crossing the limit is a row entering or
    leaving. `Aggregate::window(path, duration)` keeps only the
    documents within `duration` of the newest date-time at `path`,
    "the last five minutes", for one-off aggregates too; it is refused
    beside a filter's sort, skip or limit. New types: `LiveAggregate`,
    `LiveRow`.

    Items 108–112 → 0.27.0: additions only, nothing to adapt. A hook
    after each commit, `Condition::custom` with `Document`'s readers,
    live queries, live aggregates and `Aggregate::window` are new; a
    panic in `update_many`'s change no longer poisons the database. New
    errors, `ClosurePanicked` and `NotLive`, are variants of the
    `#[non_exhaustive]` `Error`. Files stay format 12 and exports as in
    0.26.0, which opens what 0.27.0 writes; no new fixture, as nothing
    changes how files are written (§120.9).

113. **`check` finding cells that share bytes** (§141): two cells of a
    page whose bytes overlap, one document's or entry's read as part
    of another's, are now a problem `Database::check` and `trunkdb
    check` report, by page, owner and slots. A page's own check on
    reading still leaves them out (§54.2). No measurable cost.

114. **`mul`, `pop` and `push_all`** (§142): three more update
    operators for `update_fields`. `mul(path, by)` multiplies a number,
    a missing field becoming zero, as MongoDB's `$mul`;
    `pop_first(path)` and `pop_last(path)` take an element from an end
    of an array, a queue's next item; `push_all(path, values)` pushes
    each value in order, `$push` with `$each`.

115. **`except`** (§143): `Filter::except(paths)` hands out every field
    but those, BigQuery's `SELECT * EXCEPT`: for a large field a list
    doesn't show. The rest keeps its order, `_id` stays, and the
    excepted fields are skipped, not decoded. After a `select` it takes
    out of what the selection hands out. In the text, `select * except
    body`; `except` isn't a reserved word. `Explained::except` says
    what was excepted.
116. **Operators in a batch and an upsert** (§144):
    `Batch::update_fields(&collection, &id, &update)` makes the
    operators to one document at the commit, as the ops before leave
    it: a transfer between two accounts without reading either.
    `Collection::upsert_fields(filter, &update)` updates the one match,
    or inserts a document made of the filter's `eq`s and the operators,
    MongoDB's `upsert: true`: a counter per key. `Update::set_on_insert`
    gives a field only where it inserts. A batch now commits op by op
    under the writer's lock, no longer through the `TransactionManager`.
117. **A live sort and limit** (§145): `watch` takes a filter with a
    sort, and a limit or none, "the ten fastest vans": a list in
    `find`'s order, kept as each commit changes it. `Live::Entered`
    gains `at`, `Left` `from`, `Changed` both, each its place in the
    list as the events before left it, `None` without a sort; `Left`
    also means pushed out by one that came before. Where a full list
    loses a member to a document no commit wrote, the filter is read
    again. A skip, and a limit without a sort, are still refused with
    `Error::NotLive`, whose `reason` now says so.
118. **`count_distinct`, `first` and `last`** (§146): three more
    aggregate columns. `count_distinct(path)` counts the different
    values as a filter's `=` tells them apart, `null` and missing not
    counted, from index entries where an index holds the path.
    `first(path)` and `last(path)` take the value of the group's first
    or last document, by id, or `by(field)` and then by id:
    `.last("pos").by("at")` is each van's position at its newest `at`,
    pings sent late included. `by` on any other column is refused.
119. **Unwind** (§147): `Aggregate::unwind(path)` makes each document
    the filter finds one per element of the array at `path`, the
    element in its place, MongoDB's `$unwind`: sales per sku with
    `unwind("items").group_by("items.sku").sum("items.qty")`, fields of
    one element together. An empty array, `null` or a missing field
    leaves the document out; a value that isn't an array is one
    element; equal elements each count. Unwinds follow one another,
    into an element's own arrays. Read from documents; live aggregates
    and `apply` unwind alike.
120. **Rows of the application's own** (§148):
    `Collection::aggregate_as::<R>` and `View::aggregate_as::<R>`
    convert each row to a struct, a row that doesn't failing the call
    with `Error::Document`; `watch_aggregate_as::<R>` gives a
    `LiveAggregate<R>`, whose `LiveRow<R>` holds `R` rows, converted on
    the reader's thread, a row that doesn't convert an `Err` for that
    event while the live aggregate goes on. `LiveRow` and
    `LiveAggregate` default to `Document`, so code naming them is
    unchanged.
121. **A binary search within B-tree pages** (§150): finding a key in a
    leaf or a branch, for inserts, lookups, removes and the descent,
    went through the page's cells in order, about 100 compared in a
    leaf of 200; a binary search compares 8, stepping past the
    tombstones a leaf from before §52 may hold. Batched inserts about
    1.14× faster, compaction 1.4×, reads unchanged; nothing else
    changes.
122. **A page changed again in its batch is not copied** (§151): every
    change to a page copied all of it first, as the batch held a copy
    a read shared, though only its first change in a batch has to;
    now the store lets go of its copy before a change, and the page is
    changed in place. B-tree inserts and removes, documents put into a
    page or deleted from one, and a compaction's image. Of about
    500,000 page changes in 100 batches of 1,000 inserts, 81,000 copy
    instead of all. Batched inserts about 1.11× faster, deletes 1.11×,
    compaction 1.05×, updates and reads unchanged; nothing else
    changes.
123. **A page changed through `SlottedPage` keeps its mark** (§152): a
    page's layout is checked once, as it goes into the cache, but any
    change took the mark away, so every read of a page its batch had
    changed checked it again. A change through `SlottedPage` keeps a
    valid layout valid, so it now keeps the mark; a cell too short for
    an index entry still loses it, and a debug build checks every page
    it marks. Batched inserts about 1.1× faster, compaction 1.15×,
    reads unchanged; nothing else changes.
124. **A grown cell made room for in place** (§153): a document that
    grew in an update needed its whole length free on its page, which
    a page filled by inserts doesn't have, so 83–96% of the
    benchmark's updates compacted the whole page, each live cell
    copied into an allocation of its own and back. Now the cells below
    it move down by the bytes it grew, where the page has those free;
    a compaction moves each cell in place, highest first, with no
    allocation per cell. Batched updates about 1.15× faster, the rest
    unchanged; nothing else changes.

    Items 113–124 → 0.28.0: additions only, nothing to adapt. `check`
    finds cells that share bytes; the update operators `mul`,
    `pop_first`, `pop_last` and `push_all`; `except`; the operators in
    a `Batch` and `upsert_fields`, with `set_on_insert`; a live query
    with a sort and a limit; the aggregate columns `count_distinct`,
    `first` and `last`; `Aggregate::unwind`; rows of the application's
    own. New names: `FromDocument` and `Upserted`; `Live`'s variants
    gain fields, and as they are `#[non_exhaustive]` no match changes;
    `LiveRow` and `LiveAggregate` take a type that defaults to
    `Document`. Batched writes faster, each step profiled: inserts
    about 1.3× (32k/s to 42k/s on the benchmark), updates 1.15×,
    deletes 1.15×, compaction 1.8×, reads unchanged. Files stay format
    12 and exports as in 0.27.0, which opens what 0.28.0 writes: cells
    may sit elsewhere in a page than before, in the layout every
    release reads. No new fixture, as nothing changes how files are
    written (§120.9).

125. **A sort by a key of the application's own** (§154):
    `Filter::sort_by_key(fields, key)`, smallest first, and
    `sort_by_key_desc` sort by a number the application's closure
    gives each match, from the fields it names: a distance, a
    similarity, a score. `None` or NaN leaves the match out, for
    `count` too; equal keys go by the `then_*` fields after it, then
    by id. With a limit only the best `skip + limit` are kept as the
    matches are read, so "the ten nearest" holds ten, not every match.
    It works in `find`, `count`, `cursor`, the writes, `aggregate` and
    `watch`; a panic fails the call with `Error::ClosurePanicked`. No
    text form: `order by key(lat, lon)`, which the parser refuses.
126. **`trunkdb count`, `get`, and JSON** (§155): `trunkdb count <file>
    <collection> [filter]` prints how many documents a filter finds,
    from index entries where they can tell; `trunkdb get <file>
    <collection> <id>` prints the document with an id, exit code 1 if
    there is none. `info`, `check` and `fields` take `--json`, for a
    program or an AI agent to read. The agent skill uses them.
127. **Writing through the `trunkdb` command** (§156): `insert` one
    document as JSON, or one per line from standard input; `update`
    with MongoDB's operators written as JSON (`$set`, `$inc`, `$push`
    with `$each`, `$pop` and the others), `--upsert` too; `delete`;
    `index add` and `index drop`; `drop` a collection. Each is one
    atomic batch. An update or delete that would change every document
    of a collection asks for `--all`; `--dry-run` says how many it
    would change; `insert` into a collection that isn't there asks for
    `--create`. The agent skill says how an agent writes safely.
128. **Aggregates as text** (§157): `"count() as n, avg(hours) as mean
    group by team having n > 10 order by n desc".parse::<Aggregate>()`,
    and `Display` writing one back, as a filter has its text: every
    column (`count(distinct p)`, `last(p order by q)`, `filter (where
    …)`), `unwind`, `window at 5m`. `trunkdb aggregate <file>
    <collection> <filter> <aggregate>` prints the rows, `--explain`
    how it ran. The filter's language is unchanged: the aggregate's
    words mean something only in its own text.
129. **The agent skill, after a trial** (§122.5): an agent given only
    the skill did ten requests on a test file, reads and writes, all
    right; what it had to work out alone is now in the skill: which
    conditions an index serves and how to make a query fast, how
    strings compare and a prefix as a range, values of another type
    that comparisons and `sum` pass over, two collections put together
    by their ids, and what an export holds.

    Items 125–129 → 0.29.0: additions only, nothing to adapt.
    `Filter::sort_by_key` and `sort_by_key_desc`; `FromStr` and
    `Display` for `Aggregate`; the `trunkdb` command gains `count`,
    `get`, `insert`, `update`, `delete`, `index add`/`drop`, `drop` and
    `aggregate`, and `--json` for `info`, `check` and `fields`; the
    earlier commands work as before. The agent skill covers
    them. Files stay format 12 and exports as in 0.28.0, which opens
    what 0.29.0 writes. No new fixture, as nothing changes how files
    are written (§120.9).

130. **`starts with`** (§158): `Filter::starts_with("title", "Fix")`,
    or `title starts with "Fix"` in a filter's text, finds strings
    beginning with a prefix, case counting as it does for `<`. An index
    on the field reads only the keys beginning with it, and decides
    each one from the key, so a count reads no document.
131. **A test of a value's kind** (§159): `Filter::is_kind("hours",
    Kind::String)`, or `hours is string` in a filter's text, finds the
    documents whose field holds a value of one kind, where others hold
    another; `is not string` its NOT. The kinds are the ones `trunkdb
    fields` prints, `bool`, `int`, `float`, `string`, `binary`, `array`,
    `object`, `id` and `date`, and `number` for either number.
132. **An `in` on a compound index's field, a range per value** (§160):
    `status in ("open", "late") and due < 50`, with an index on
    `(status, due)`, reads each status's entries before 50, as `status
    = "open" and due < 50` reads one status's, where it read every
    entry of each status. The same after `=`s on the index's first
    fields, and with `=`s on the fields after the `in`'s.
133. **The public API gone through again** (§161), as in §118, for what
    came since. **Breaking:** the hook's `Commit` and `Change` are
    `PartialEq` but no longer `Eq`, as reports aren't, so a field that
    holds a float can be added; `==` works as before, but neither can
    be a `HashMap` key. `Kind::ALL` is a slice and a date-time's kind
    `Kind::DateTime`, both before any release had them.

    Items 130–133 → 0.30.0. **One thing to adapt:** the hook's `Commit`
    and `Change` are no longer `Eq`, so neither can be a `HashMap` or
    `HashSet` key; `==` works as before, and a set of what changed is
    one of ids, from `Change::id`. Otherwise additions:
    `Filter::starts_with` and `Op::StartsWith`; `Filter::is_kind`,
    `Condition::is_kind`, `Condition::IsKind` and `Kind`; `starts
    with`, `is <kind>` and `is not <kind>` in a filter's text. An `in`
    on a compound index's field reads a range per value, so queries
    find the same documents, faster. Files stay format 12 and exports
    as in 0.29.0, which opens what 0.30.0 writes. No new fixture, as
    nothing changes how files are written (§120.9).
134. **A batch's preconditions** (§162): `Batch::require(&collection,
    filter)` must find a document matching the filter, and
    `require_none` must find none, when `commit` reaches them: after
    the ops before them, under the writer's lock, so nothing is written
    between the check and the batch. A failed one writes nothing and
    returns `Error::PreconditionFailed`, naming the op. A transfer only
    while the balance covers it, a registration only while the email is
    free among active users, a write held to the version it read.
135. **Parameters in a filter's text** (§163): `:name` where a value or
    a count stands, `status = :status limit :n`, the values passed
    apart, so nothing in one can change what the text says.
    `Template<Filter>` (and of a `Condition` or an `Aggregate`) reads
    and checks the text once; `bind(&Params)` makes the filter, any
    number of times; `params()` lists the names. A missing value, an
    unused one, or one that doesn't fit is a `BindError`. `trunkdb`
    takes the values as `--param name=value` for `find`, `fields`,
    `count`, `aggregate`, `update` and `delete`.
136. **An OR of ids and indexed conditions** (§164): `_id in (…) or
    updated > :since`, "these ids, or what changed since", reads the
    ids from the primary index and the rest from an index range, each
    document once, where it read every document of the collection: on
    100,000 documents, ten ids and the hundred changed since, 0.17 ms
    instead of 76. `explain` names it `IndexUnion` with `_id` first
    among its `fields`. Every branch must name ids or have an index, or
    the OR is a scan as before; a hinted filter reads as before.

    Items 134–136 → 0.31.0. **Nothing to adapt:** additions only.
    `Batch::require` and `Batch::require_none`, and
    `Error::PreconditionFailed`, a new variant of an `Error` that was
    `#[non_exhaustive]` already; `Template`, `Params` and `BindError`
    in `trunkdb::query`; `--param` for the command's filters and
    aggregates. Two things read differently without breaking: a
    filter's text with `:name` in it, an error before, now names the
    template to read it as; an OR of ids and indexed conditions is an
    `IndexUnion` where it was a `Scan`, finding the same documents.
    Files stay format 12 and exports as in 0.30.0, which opens what
    0.31.0 writes. No new fixture, as nothing changes how files are
    written (§120.9).
137. **Backups a user can trust** (§165): an export ends with a line
    saying how many collections and documents came before it, export
    version 3, and an import of one checks it, so an export cut short,
    by a full disk or a crash while it was written, fails to import
    instead of restoring in part; `Summary::ended` says whether there
    was one. `trunkdb::verify_export` and `trunkdb verify` restore an
    export into a file of their own, `check` it, count it against the
    export, and delete it: a `Verified` report, exit code 1 and why on
    the command line. A section of the guide on backups while the
    application runs, restores, copying the file and its WAL, and moving
    between releases. 0.31.0 and older refuse an export of version 3;
    the guide says how to make one version 2.
138. **Binary in indexes, in the order of its bytes** (§166): binary
    compares byte by byte, as SQLite compares a BLOB, with `<`, `>`,
    ranges and `starts with` a binary prefix, written `{"$binary": …}`
    in a filter's text; a sort puts it after date-times, by its bytes,
    where it came last with arrays and objects. It has an index key,
    so `=`, ranges, prefixes and sorts on a binary field read just
    their entries, a count and a selection of the field none, and a
    unique index refuses the same bytes twice. File format 13: the
    first open of an older file rebuilds the indexes that hold binary,
    in one batch, and 0.31.0 refuses a file this build has written.
    A read-only open, as `trunkdb info` makes, no longer stamps the
    format in memory, so it reports what the file says.
    Kept as binary instead of hex text, 100,000 file paths take a
    quarter less file and a third less index.
139. **An OR of `Eq`s merged by id** (§167): a sort by `_id` with a
    limit after an OR or an `in` whose ranges `Eq`s fix whole, "the
    newest 20 of these tenants", walks the ranges side by side and
    merges them by id, stopping at the limit, where it read every match
    and sorted them in memory. The ranges may be of one index or
    several, a multikey path or a compound index fixed whole; a
    document in two of them comes out once, and where the entries
    decide, each range is decided once, so a deep page by skip reads
    only its own documents. `_id` comparisons narrow
    each range, so paging by the last id works through `tenant in (…)`.
    `QueryPlan::IndexUnion` gains `in_order`, `true` for such a merge.
    A sort by `_id` through a compound index fixed only in part moved
    to the ROADMAP's Not planned: its keys aren't in id order.

    Items 137–139 → 0.32.0: nothing breaking at compile time;
    `verify_export`, `Verified`, `Summary::ended`, `trunkdb verify` and
    `IndexUnion`'s `in_order` are additions, to types that were
    `#[non_exhaustive]` already. **Changed answers:** binary compares
    by its bytes, so `<` and ranges on it find documents they didn't,
    and a sort puts it after date-times instead of last. **File format
    13:** the first open of an older file rebuilds the indexes that
    hold binary; 0.31.0 and older refuse a file 0.32.0 has written, and
    an export of version 3, so moving back goes through an export made
    version 2, as the guide says. With its fixture (§120.9), which
    adds a binary index to the generator.
140. **Writes by a filter in a batch, and a batch through the command**
    (§168): `Batch::update_many(&collection, filter, &update)` makes the
    operators to every document the filter finds at `commit`, and
    `Batch::delete_many` deletes them, each at its place among the ops,
    under the writer's lock, so nothing writes between finding the
    documents and changing them; the filter's sort, skip and limit
    count, as for `Collection::update_fields` and `delete_many`.
    `Batch::dry_run` runs every op and rolls it back, saying what
    `commit` would write or how it would fail. `trunkdb batch <file>
    <ops.jsonl | ->` reads one op a line as JSON, `insert`, `update`,
    `delete`, `require` and `require_none`, and commits them as one
    batch or not at all, with `--param`, `--create` and `--dry-run`; it
    prints each new id and what each line wrote, and names the line a
    failure came from. An update or delete with an empty filter asks
    for `"all": true`. **One thing to adapt:** `Batch::commit` returns
    a `Written`, how many documents each op wrote, where it returned
    `()`. `batch.commit()?;` compiles as before; a function returning
    `batch.commit()` as a `Result<()>` adds `.map(drop)`, and a `match`
    on `Ok(())` reads `Ok(_)`.
141. **A regex** (§169): `Filter::regex(field, Regex::new(pattern)?)`,
    and `notes matches "time-?out"` in a filter's text, the pattern a
    `:name` parameter too; the `regex` crate's syntax, `(?i)` to ignore
    case, matching anywhere in a string unless `^` or `$` anchor it, in
    time linear in the string, whatever the pattern. `Regex` compiles
    once, when made, refusing a pattern over 4,096 bytes or over 1 MiB
    compiled with a `RegexError` in one line. A pattern beginning `^`
    and fixed text, `^Fix`, reads an index on the field as `starts
    with "Fix"` does; any other reads every document, as `contains`
    does. Behind the `regex` feature, on by default, which brings the
    `regex` and `regex-syntax` crates; `default-features = false`
    leaves them out, and `matches` is then refused, saying why. A
    filter's text error lists `matches` among the comparisons, and
    `BindError` gains `NotAPattern`, for a value after `matches` that
    isn't a string or a regex; it was `#[non_exhaustive]` already.
142. **A sparse index for an AND group or an OR** (§169.9): a sparse
    index is read only where the filter rules out null in its fields
    (§44), and that was seen only among the filter's top-level
    conditions. Inside an AND group, or an OR whose every branch rules
    it out, an OR of one branch among them, it is seen now, so a filter
    and its text, which reads back flat, plan alike, and a hint to use
    the index is followed by both. The fuzz program found them planning
    apart; the answers were the same.

    Items 140–142 → 0.33.0: **one thing to adapt:** `Batch::commit`
    returns a `Written` where it returned `()`, so a `match` on
    `Ok(())`, or a function returning `batch.commit()` as a
    `Result<()>`, changes as item 140 says; `batch.commit()?;`
    compiles as before. `Condition::Regex`, `BindError::NotAPattern`
    and the `regex` feature are additions, to enums that were
    `#[non_exhaustive]` already; the feature is on by default, so a
    build gains the `regex` and `regex-syntax` crates unless it turns
    default features off. No file format changed, so no fixture
    (§120.9): 0.32.0 and 0.33.0 read each other's files.
143. **Field names stored once, numbers as varints** (§170): a file
    keeps one dictionary of field names, in the catalog, and a stored
    document's keys are small numbers into it, where each key was its
    4-byte length and its bytes; ints are zigzag varints, and lengths
    and counts varints, where they were 8 and 4 bytes. A name of at
    most 64 bytes joins the dictionary in the batch that first stores
    it, and goes with that batch if it fails; past 4,096 names, and for
    a longer one, the key is stored inline. Documents stay schemaless,
    and every read gives what was written. `compact` keeps only the
    names still in use. On the bench's 100,000 tasks, 1,233 data pages
    where there were 2,223.
144. **Index leaves stored after an anchor** (§171): each index leaf
    keeps one anchor key, its middle entry's when the page is built,
    and each entry the number of bytes it shares with the anchor, the
    rest of its key, and its location as varints. Every entry still
    reads on its own, so the binary search within a page, walks,
    inserts and removes work as they did; a full leaf is rebuilt
    around a new anchor before it splits. On the bench, 1,654 index
    pages where there were 2,742; on 150,000 file paths indexed by run
    and path, 4,970 where there were 9,250.
145. **Walks that pass part of a leaf** (§172): a walk enters its first
    leaf by a binary search instead of stepping over the keys before
    its range, checks the range's end once a leaf, decodes each entry
    once and copies only the part of a key that changed. A count, and
    a skip through an index, pass the part of a leaf inside the range
    at once, where they stepped through the leaves at the range's
    ends. Page 50 of 20 by an index takes 21 µs where it took 26, a
    skip of 5,000 by id 35 µs where it took 38.
146. **A header counting pages the file lacks is refused** (§55.2):
    a file whose header counts more pages than it holds opened, and
    `check` sized a map by the count, so a damaged count ran it out
    of memory. Open now refuses it as damage, after the WAL's
    recovery. Found by fuzzing; 0.33.0 and older have it too.
147. **A page whose cells add up to more than it holds is refused**
    (§55.2): slots on the same bytes could add up past the page, and a
    delete on it panicked. Its check refuses it as damage now. Found by
    fuzzing; 0.33.0 and older have it too.

    Items 143–147 → 0.34.0: nothing to adapt in a program. **File
    format 14,** which 0.33.0 and older refuse once 0.34.0 has opened
    a file for writing; keep an export or a copy from before to go
    back. Opening an older file converts nothing: its documents and
    index leaves read as they are and are stored anew as they are
    written, all of them by `compact`. With its fixture (§120.9),
    which adds a key past 64 bytes to the generator, so the fixture
    holds a key stored in full.
148. **A date, and a time of day** (§173): `Date`, a day in the
    calendar without a time or a zone, and `Time`, a time of day
    without a date, to the nanosecond, as `Document::Date` and
    `Document::Time`. A struct's `Date` and `Time` fields are stored
    as them; `DateTime::date()` and `time()` take a moment apart in
    UTC, and `Date::at(time)` puts it back together. Each compares
    only with its own kind, never with a date-time, midnight or not:
    filters, sorts after binary, keys in an index, `min` and `max`.
    In a filter's text `date("2026-11-07")` and `time("14:05")`, and
    `datetime("…")` for a moment; `Kind::Date` and `Kind::Time`, `is
    date` and `is time`; in JSON `{"$date": "2026-11-07"}` and
    `{"$time": "14:05:00"}`.
149. **A window for each group** (§174):
    `Aggregate::newest_per_group(path, n)`, each group's `n` documents
    with the largest values at `path`, and `window_per_group(path,
    duration)`, each group's documents within `duration` of its own
    newest date-time: "the last ten readings of each server", SQL's
    `row_number() over (partition by …)`. A group that stops reporting
    keeps its last documents. Beside `window`, which comes first; for
    `aggregate`, `apply`, snapshots and live aggregates; refused
    beside a filter's sort, skip or limit, as `window` is. In the
    text, `window at 10 per group` and `window at 5m per group`.
150. **An upsert in a batch** (§175): `Batch::upsert_fields(&collection,
    filter, &update)`, `Collection::upsert_fields` as an op, at its
    place among the batch's ops: a counter and its total made or added
    to in one commit. `Written::upserted(op)` says whether it inserted
    or updated, and the id. Several matches fail the batch; a filter
    with a sort, skip, limit or selection is refused when added. An
    `upsert` line for `trunkdb batch`, its id printed with the inserts'.
151. **A sort with a limit kept small** (§176): a sort by fields that no
    index serves, with a limit, keeps only the best `skip + limit` as
    the matches are read, as a key sort does (§154), instead of
    gathering and sorting them all. Ten of 100,000 by a field without
    an index: 87 ms before, 48 ms now; memory as small as the limit,
    the index entries read now handed on as the walk reaches them
    rather than gathered first. Results are the same.

    Items 148–151 → 0.35.0. **Changed answers:** `Kind::DateTime` is
    named `datetime` in the text now, as `date` names dates, so
    `hours is date` finds dates where it found date-times; `trunkdb
    fields` prints `datetime` for them; and a filter prints a
    date-time as `datetime("…")`. `date("…")` with a time in it still
    reads as a date-time, so filters written so far mean what they
    did. `Date`, `Time`, `Document::Date`, `Document::Time`,
    `Kind::Date` and `Kind::Time` are additions, to enums that were
    `#[non_exhaustive]` already; so is `Written::upserted`. **File
    format 15,** which 0.34.0 and older refuse once 0.35.0 has opened
    a file for writing; keep an export or a copy from before to go
    back. An older file opens as it is, nothing rebuilt. **Export
    version 4,** which 0.34.0 and older refuse; an import reads 1 to
    3 as before. With its fixture (§120.9), which adds dates and
    times of day to the generator, as values and as index keys.
152. **Text a program passes, read by the library** (§178), the first
    step of the C ABI (§177): `Update` from MongoDB's operators as JSON,
    `TryFrom<Document>` and `FromStr`; `Params` from an object of
    values, with `names()` and `only(names)` for texts that share them;
    `BatchLines`, a batch's ops read from JSON lines as `trunkdb batch`
    reads them, each mistake naming its line, `batch(&db)` making the
    `Batch`, and `failed_line` finding the line a commit failed at.
    `trunkdb update` and `batch` read through them now, printing what
    they printed; of several unused `--param`s, the first by name is
    named.
153. **The C ABI** (§179): `trunkdb-ffi`, trunkdb as a C library,
    `libtrunkdb.so`, `libtrunkdb.dylib`, `trunkdb.dll` and `libtrunkdb.a`,
    with `include/trunkdb.h`, made by cbindgen and checked in. 36
    functions: open, close, checkpoint, compact, check, stats,
    collections, drop, export and import; indexes; `get`, `find`,
    `find_one`, `count`, `aggregate` and `explain`, of the database or
    a snapshot; `insert`, `replace`, `delete`, `update`, `delete_many`,
    `upsert`; a batch as JSON lines; snapshots and cursors. Documents as
    tagged JSON, filters and aggregates as text with their parameters
    as a JSON object; an error code from each call, the thread's last
    error as text and as JSON with what it knows; handles checked by a
    tag; no panic across. Tested through raw pointers, and from a C
    program under AddressSanitizer in CI. In the library for it:
    `Display` for `Explained` and `QueryPlan`, the lines `find
    --explain` prints, and `Database::export_to_file`, as `trunkdb
    export` wrote a file.
154. **The C# wrapper** (§180), `bindings/dotnet/`, over the C ABI:
    `Database.Open`, `Snapshot` and `Cursor<T>`, each `IDisposable`
    over a `SafeHandle`; `db.Collection<T>(name)`, as in Rust, with
    `Insert(task)` setting its id, `Update(task)` and `Delete(task)` by
    it, `InsertMany`, `UpdateMany` and `DeleteMany` by a filter,
    `Upsert` and indexes; the reads, `Get`, `Find`, `FindOne`, `Count`,
    `Aggregate`, `Explain` and `OpenCursor`, of the collection now or
    as a snapshot was taken; batches, check, stats, compact, export
    and import on the database. Documents are a program's own classes through
    System.Text.Json, a converter for each tag: `DocId`, `DateTime`,
    `DateOnly`, `TimeOnly`, `byte[]`, and floats kept floats; a
    property `Id` is `_id`. Errors are a `TrunkDbException` with the
    code and details, and a subclass for a parse error, a duplicate id
    or value, a document not found, a failed precondition and a
    snapshot too old. .NET 8; tested in CI on the three systems.

    Items 152–154 → 0.36.0. Additions only: nothing a program has to
    change. The C ABI, `trunkdb-ffi`, carries the same version as
    trunkdb, and so does the C# wrapper, which checks the ABI's version
    when it opens a database. No file format changed: format 15 and
    export version 4, as in 0.35.0, so no fixture.
155. **Documents an index leaves out, counted** (§181): an index on one
    field, without `[*]`, keeps in its catalog cell how many documents
    have a value there that nothing orders, an array, an object or
    NaN, which it holds no entry for. A sorted page read through the
    index that the walk doesn't fill scanned every document for those
    afterwards; now only when the count isn't 0. Found through the
    admin app: `category != "Salad" order by category limit 50` on six
    recipes read 12 entries and 11 documents, now 6 and 5. The count
    changes only when a document moves in or out of it, a cell
    rewritten in place; `ensure_index` and `compact` count it,
    `check` recounts it. **File format 16,** which 0.36.0 refuses; a
    format 15 file opens as it is, its indexes scanning as before
    until `compact` counts them.

    Item 155 → 0.37.0. Nothing a program has to change; queries
    answer as before. **File format 16,** which 0.36.0 and older
    refuse once 0.37.0 has opened a file for writing; keep an export
    or a copy from before to go back. A format 15 file opens as it is,
    its indexes counted at its next `compact`. Export version 4, as in
    0.35.0. With its fixture (§120.9), whose generator adds an index
    on `kinds.v`, every kind as a key, five documents left out of it.
156. **Files made new, and their names synced** (§182), found in a
    review of writes, recovery and backups: `export_to_file`'s
    temporary file, `<out>.tmp<pid>`, was opened emptying whatever was
    under that name, through a link too, and two exports to one file
    from one process shared it; now `<out>.tmp<pid>-<n>`, always a new
    file. `verify_export` checked that its files weren't there, then
    opened them, and removed them afterwards, so another open in
    between could lose its files; now it creates them, and removes
    only those. A new database's file and WAL, and an export renamed
    into place, now have their directory synced, so a power cut can't
    lose their names (Unix). A WAL replayed into a file it doesn't
    belong to is §183, next.
157. **A WAL bound to its file** (§183), the review's fourth finding:
    the WAL said nothing of the file it belonged to, so one a crash
    left beside an older copy of the file, restored from a backup say,
    or beside another database, or beside a new file where the old
    one was deleted, was replayed into it, writing its pages over the
    file's before anything checked. Now the file's header holds a
    random id and a session, new each time the WAL starts again from
    empty, and the WAL's header, version 4, the id and both sessions.
    An open replays a WAL only into the file it names, at either
    session, and refuses any other with
    `FormatError::WalNotThisFiles`, before writing anything; its
    message says to move the WAL aside, or put back the copy it
    belongs to. Versions 2 and 3 are still replayed, unchecked. File
    format 16 still: the fields sit in bytes older releases leave zero.
158. **Writes that do what they say** (§184), found in a review of
    updates, deletes, batches and the .NET entry points. The C#
    wrapper passed text with a NUL to the library, which read it up to
    the NUL: a filter lost its last conditions, a name or a path named
    another collection or file. Every call that takes text now refuses
    one with `ArgumentException` first. `trunkdb update` and `delete`
    with `--dry-run` counted the filter's matches, so a preview
    succeeded where the write would fail, counted documents the write
    would leave, and for `--upsert` with an `offset` said "insert"
    where the run updated. Both now run as one op of a batch, its
    `dry_run` or its `commit`; an upsert whose filter has a sort,
    `limit` or `offset` is refused, as a batch's is. The typed
    `update_many` stores each match as its type, dropping the fields
    the type lacks: now said in its documentation and GUIDE.md, which
    point to `update_fields`.

    Items 156–158 → 0.38.0, fixes from two reviews. **WAL version 4,**
    which 0.37.0 and older refuse: a WAL a crash left under 0.38.0
    needs 0.38.0 to recover it; after a clean close the WAL is empty,
    and going back works. File format 16 and export version 4, as in
    0.37.0, so no fixture: the file's id and session sit in header
    bytes older releases leave zero. A WAL that isn't its file's is
    refused with the new `FormatError::WalNotThisFiles` (the enum is
    non-exhaustive). `trunkdb update --upsert` refuses a filter with a
    sort, `limit` or `offset`, which the run ignored before. In .NET,
    text with a NUL throws `ArgumentException`.
159. **A leaf split around its old anchor** (§186), found by the
    `queries` fuzz target's random programs on CI. An insert into a
    full index leaf could fail with "index leaf can't be split so that
    each half fits", refusing the batch, and every later insert into
    that leaf with it. Each half of a split took its middle key as its
    anchor (§171), which can pack it much worse than the leaf's anchor
    did: in a leaf of short keys among long ones that share long
    prefixes, no point fit. A half that doesn't fit around its middle
    key is now built around the old anchor too, which always finds a
    point. Nothing was ever damaged. Since 0.34.0, whose leaves are
    stored after an anchor; it takes keys of hundreds of bytes sharing
    long prefixes, with short ones between them.

    Item 159 → 0.39.0, the fix alone. File format 16, WAL version 4
    and export version 4, as in 0.38.0, so no fixture.
160. **Readers in other processes** (§185, §187): one process writes,
    and read-only opens in other processes read beside it, an admin
    tool or an agent while the application runs. The locks move to
    three files beside the database, `<path>.writer`, `<path>.readers`
    and `<path>.gate`, since on Windows a lock on the database or its
    WAL would keep the others from reading or writing them. File
    format 17, which a writer's open stamps and 0.39.0 and older
    refuse; no page changes. A reader builds each call's snapshot from
    the file and the WAL, holding the readers' lock while it reads; a
    `Snapshot` or a cursor keeps its own. A checkpoint beside a reader
    is left for later and counted in `SnapshotInfo`, the next tried
    through the gate, which no new read passes, waiting
    `OpenOptions::checkpoint_wait`. `Database::checkpoint` beside a
    reader fails with `WouldBlock`, `compact` with `SnapshotOpen`. An
    open that has to write the file waits `wait_for_readers`, then
    fails with `WouldBlock`. A file at 16, or one without lock files
    a reader can make, is read as before.
161. **A reader that catches up** (§188): a reader beside a writer
    keeps the store it built, its cache of the file included, and each
    call reads only the batches the WAL gained since, as one more
    commit. It starts again from the file where a checkpoint has
    written it, or where the WAL isn't the one it read. A reader's
    snapshots share that store, and the snapshot memory limit ends the
    oldest of them as in one process.
162. **A reader too long is asked to let go** (§189): past a second
    limit, `OpenOptions::let_go_wal_bytes` (four times
    `checkpoint_wal_bytes` unless set, 0 never), a commit's checkpoint
    shuts the gate and asks the readers to let go, through a fourth
    file beside the database, `<path>.letgo`. A watcher thread in each
    reading process lets go for all its snapshots within milliseconds,
    a forgotten one too; their reads fail with `Error::SnapshotTooOld`,
    a long scan at its next page. An open that has to write the file
    asks as well. `SnapshotInfo::readers_asked` counts the asks.
    `check` of a snapshot ended while it checks, by this or by the
    memory limit, fails with `Error::SnapshotTooOld`, where it reported
    the reads that failed as problems (§189.7).

    Items 160–162 → 0.40.0. Nothing a program has to change; one
    changed answer, `check` of a snapshot ended while it checks (162).
    **File format 17,** which 0.39.0 and older refuse once
    0.40.0 has opened a file for writing; keep an export or a copy
    from before to go back. A format 16 file opens as it is, stamped
    17 by its first writer's open; read-only, it is read as before.
    Beside the database there are now four lock files, `.writer`,
    `.readers`, `.gate` and `.letgo`, made where missing and never
    removed. A read-only open beside a writer reads, where it was
    refused before, and starts a thread of its own; an open that has
    to write beside readers waits up to `wait_for_readers`. WAL
    version 4 and export version 4, as in 0.39.0. With its fixture
    (§120.9), its generator as before.
