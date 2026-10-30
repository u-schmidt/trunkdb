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
