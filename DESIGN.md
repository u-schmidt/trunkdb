# trunkdb — design

How trunkdb works today, one subsystem at a time. This file describes
the current state; the reasons, the rejected alternatives and the
measurements are in [the spec](spec/README.md), one numbered section per
step, linked from each part below. What's done and what's open is in
[ROADMAP.md](ROADMAP.md), and every step by release in
[CHANGELOG.md](CHANGELOG.md).

When this file and a spec section disagree, the newer spec section
describes the code. This file should be updated with every change that
alters what it says.

## 1. What it is

An embedded, single-file, schema-less document database: open a file,
get collections of documents, no tables, joins or schema migrations. It
takes the idea of [LiteDB](https://www.litedb.org/) outside .NET, as an
independent design rather than a port [§1](spec/01-motivation.md).

It began as a learning project first and a personal tool second [§2](spec/02-feasibility-read.md),
with a deliberately small v0 [§6](spec/06-v0-feature-set.md). Every layer has since been
replaced by a real implementation, and the engine has grown well past
that v0: secondary indexes of every common kind, snapshot reads beside
a writer, projection and aggregates, a text query language, explain
with execution statistics, and reads level with SQLite, redb and sled
in its benchmark. Learning is still how it grows: each step is a spec
section with its reasons and its measurements.

Three real workloads shaped what it needs: a time-series workload of
many small documents queried by time, a large-document desktop
workload, and a small sync app that replaced MongoDB with it [§5](spec/05-reference-workloads.md).

Deliberately not there: a cost-based planner (the planner follows
fixed rules, `explain` says which, and a hint in the query picks the
index where the rules can't know better), joins, several writers at once,
several processes on one file, encryption and schema validation [§6](spec/06-v0-feature-set.md).
Aggregates were on that list; they came back in [§107](spec/107-aggregates.md).

## 2. Layers

Every layer sits behind a small trait, so a naive version could be
swapped for a real one without reshaping what's above it [§3](spec/03-architecture-layers-and-whats-real-vs-fakeable.md). All of
them are real now; `InMemoryIndex` remains, for tests.

| Layer | Trait | Implementation | Code |
|---|---|---|---|
| Pages | `PageStore` | `FileStore`: the writer's staged pages and header, over `Pages`: the file, checksums, the page cache and the committed pages [§77](spec/77-the-committed-pages-apart-from-the-writers.md) | `storage/` |
| Documents | — | the `Document` enum, its byte encoding, the serde bridge, typed reads from the encoding, tagged JSON | `document.rs`, `serde_bridge.rs`, `serde_stored.rs`, `json.rs`, `decode.rs` |
| Ids | `IdGenerator` | `UuidV7Generator` | `id.rs` |
| Indexes | `Index` | `BTreeIndex`, for the primary index and every secondary one | `index/` |
| Transactions | `TransactionManager` | `GlobalLockTxnManager`: one batch at a time, all or nothing | `txn/` |
| Durability | `Durability` | `WalDurability`: a page-image write-ahead log | `durability/` |
| Queries | — | `Filter`, `Condition`, `Sort`; a fixed-rule planner | `query.rs` |
| API | — | `Database`, `Collection<T>`, `Batch`, `Cursor`, `Snapshot`, `View<T>` | `database.rs`, `collection.rs`, `batch.rs`, `cursor.rs`, `snapshot.rs` |

`PageStore` is the one seam that isn't meant to be faked: everything
above it works in pages, never in file offsets. Index and data code
take `&mut dyn PageStore` and `&mut Catalog` as plain parameters and
hold no handles of their own, so the database's lock is the one place
they come from [§8](spec/08-ownership-one-refcell-boundary-not-one-per-component.md) [§27](spec/27-a-thread-safe-cloneable-database-handle.md).

## 3. Files on disk

| File | What |
|---|---|
| `app.trunkdb` | the database: pages of 8 KB |
| `app.trunkdb.wal` | the write-ahead log: whole page images, one record per commit, several for a commit over 32 MiB |

Both are named after the database file. An open through a symlink
follows it to the file first, so every name for it finds one WAL; a
hard link is a second name the open can't tell apart, and isn't
supported [§127](spec/127-a-database-opened-through-a-symlink.md).

The database file holds an OS advisory lock (`flock`, `LockFileEx`)
while it's open, so a second `open` fails at once instead of reading a
file another process is writing [§21](spec/21-an-exclusive-file-lock-and-a-format-version.md). Within one process, a handle is
cloned, not opened twice. On Unix a process the application starts
holds a copy of the file from its fork to its exec, and with it the
lock: a database dropped in that moment is still held until the exec [§98](spec/98-a-lock-held-by-a-starting-process.md). A read-only open (`OpenOptions::read_only`)
takes the lock shared: read-only opens go together, and none goes
beside an open that writes [§88](spec/88-a-read-only-open.md).

Between checkpoints the newest pages are only in the WAL. The main file
is complete on its own after `db.checkpoint()`, and after the last
handle is dropped [§51](spec/51-one-flush-per-commit.md).

## 4. Pages

Every page is 8,192 bytes, and its last 4 bytes are a CRC-32C of the
page's id and the rest of its bytes [§7](spec/07-page-layout.md) [§40](spec/40-page-checksums.md). A read checks it, so a
damaged page is an error, not garbage; the checksum over the id also
catches a page written to the wrong place. Above `FileStore`, a page is
8,188 bytes: nothing else ever sees the checksum.

| Page | Layout |
|---|---|
| 0: header | magic `TRUNKDB1`, page size, page count, head of the free list, format version; the releases that created the file and last wrote it [§125](spec/125-the-release-that-wrote-a-file.md) |
| 1: catalog | slotted: one cell per collection and per index |
| data | slotted: documents of one collection |
| index leaf, index branch | slotted: B-tree entries, in key order |
| overflow | one run of a large document's bytes, and the next page |
| free | the next free page |

**Slotted pages** have a 13-byte header (type, next page, slot count,
start of the cell area), then a directory of slots growing forward, 4
bytes each, then the cells packed backward from the end [§7](spec/07-page-layout.md) [§20.2](spec/20-several-documents-per-data-page.md).
A cell is named by its slot, so it can move within its page without
anything that points at it changing. On data pages a slot is an
identity and a deleted one is reused; on B-tree pages slot order is key
order, and a cell is inserted or removed in place by moving the
directory [§52](spec/52-b-tree-pages-changed-in-place.md). A page's layout is checked when it's read: a slot
outside the cell area is an error saying what is wrong, not a panic [§54](spec/54-checking-a-page-when-it-is-read.md),
and so is a slot directory past the page's end, refused before a slot is read [§129](spec/129-a-slot-directory-past-the-pages-end.md).
A page is checked once, as it goes into the cache or its batch
commits, and marked; reads of a marked page skip the check [§66](spec/66-a-page-checked-once.md).
The same holds for everything decoded from a page: bytes that run out,
a count past what's left, a link back to a page already seen or past
the file's end are `InvalidData` errors. Decoders read through
`decode.rs`, and B-tree pages through one function that checks every
cell is long enough for its entry [§55](spec/55-fuzzing.md).

**Freed pages** go onto a free list threaded through the pages
themselves, and allocation takes from it first [§7](spec/07-page-layout.md). The file doesn't
shrink until a compaction [§41](spec/41-compaction.md).

**The format version** is 12 [§21.2](spec/21-an-exclusive-file-lock-and-a-format-version.md) [§59](spec/59-a-struct-carries-its-own-id.md) [§62](spec/62-an-index-key-for-ids.md) [§69](spec/69-a-date-time-type.md) [§95](spec/95-only-objects.md). A build opens its own format and
the older ones that are valid files of it (6 to 11), and stamps them
with the current one at open; it refuses newer ones, and older ones
that need converting. Opening a 6–9 file with secondary indexes checks
them once and rebuilds any that a document holds an id in, since ids
had no key before 10 [§62](spec/62-an-index-key-for-ids.md). Opening a 6–11 file checks that every document
is an object, by its type byte, and refuses the file, unchanged, if one
isn't [§95](spec/95-only-objects.md). Moving between formats is
export and import [§30](spec/30-export-and-import-as-json-lines.md). The history is in the comment on
`FORMAT_VERSION` in `storage/file.rs`.

## 5. Durability: WAL, commit and checkpoint

A write batch goes through five steps under the writer's lock, which
readers don't take (`Database::transact`) [§19.3](spec/19-durability-take-two-a-page-image-wal.md) [§51](spec/51-one-flush-per-commit.md):

1. **Stage.** Every page write stays in memory, in the store's staging
   area.
2. **Apply** the batch. On any error, the staged pages and the catalog
   cache are dropped: nothing reached the file, so nothing needs
   undoing [§19.5](spec/19-durability-take-two-a-page-image-wal.md).
3. **Log** every changed page, whole, to the WAL as one record, and
   `fsync` (`F_FULLFSYNC` on macOS). This is the one flush a commit
   waits for.
4. **Commit.** The pages become the newest committed ones. Reads find
   them in memory; the main file isn't touched.
5. **Checkpoint**, once `checkpoint_pages` or more committed pages
   wait (default 4,000, an `OpenOptions` setting) [§53](spec/53-a-configurable-checkpoint-threshold.md), [§123](spec/123-batched-writes-checkpoint-less-often.md), or the WAL
   reaches `checkpoint_wal_bytes` (default twice that many pages' bytes) [§76](spec/76-a-wal-size-limit.md), or a
   compaction's image waits [§108](spec/108-compaction-built-in-a-file.md): write them to
   the main file, `fsync`, truncate the WAL.

A crash before step 3 finishes leaves the state before the batch; a
crash after it leaves the batch whole in the WAL, which the next `open`
writes back before anything reads the file. A batch over 32 MiB is
logged in several records, each flushed before the next, and restored
only if its last one is there; recovery reads the WAL a record at a
time, so it takes one record's memory whatever the WAL's size
[§91](spec/91-a-batch-in-several-records.md). Page images are
idempotent, so writing one back twice is harmless [§19](spec/19-durability-take-two-a-page-image-wal.md). A WAL page
past the file's end, as the header before it has it, is refused before
anything is written [§55](spec/55-fuzzing.md). A checkpoint
that fails keeps its pages waiting, and the next one tries again.

A batch whose log fails is taken back alone: the WAL is cut to where
it was before, which keeps the batches committed earlier and not yet
written back [§81](spec/81-a-failed-log-taken-back-alone.md). Where it
was is never past the file's end as the log starts, and a checkpoint that cut
the WAL counts it empty even if its flush then failed [§128](spec/128-a-checkpoint-that-fails-after-cutting-the-wal.md). Only if that fails too is the
batch's fate unknown; the database then refuses every call with
`Error::Poisoned` until it's reopened, which recovers from the WAL
[§19](spec/19-durability-take-two-a-page-image-wal.md) [§27](spec/27-a-thread-safe-cloneable-database-handle.md). A panic in the application's code inside a batch
doesn't poison: it is caught, and the batch fails as any batch does
[§136](spec/136-a-condition-of-the-applications-own.md) [§137](spec/137-a-panic-in-update-many-s-change.md).

Costs: memory for the waiting pages (8 MB at the default), and a WAL
that grows with every commit, which the next open after a crash reads
back whole [§53.3](spec/53-a-configurable-checkpoint-threshold.md). The first WAL logged operations, not pages. It was
replaced because a crash between the several page writes of one
operation, a B-tree split say, left a file that replaying the operation
couldn't repair; page images need no structure-specific recovery at all
[§16](spec/16-durability-a-real-write-ahead-log.md) [§19.1](spec/19-durability-take-two-a-page-image-wal.md).

A read-only open recovers in memory: the WAL's batches, checked as for a
restore, become one commit that is never written back. The same goes for
whatever the open would change itself (an empty file's catalog page, an
old file's index keys). The file and the WAL stay as they are, and the
next open that writes recovers from the WAL. Every write fails with
`Error::ReadOnly` [§88](spec/88-a-read-only-open.md).

**Reported after it commits** [§135](spec/135-a-hook-after-each-commit.md): while a
`Database::subscribe` is open, a batch records in its own catalog what
it does to each document, as `apply_write_op` applies it, and
`drop_collection` the collection it drops. Taken out before step 4, the
record is sent after it, still under the writer's lock, to each
subscriber's `std` channel: one `Commit` per batch, in commit order,
each document once with the batch's net effect. A failed batch drops
its record with its catalog. The channel is unbounded, so a writer
never waits for a subscriber, and a subscriber may write.

**Live queries, checked as it commits** [§138](spec/138-live-queries.md): the writer keeps each
`Collection::watch` with its filter and the set of ids that match. A
batch records its changes while one is open, as for a subscriber, and
right after step 4, before the reports, each document it changed in a
watched collection is read from the snapshot just published and
checked against each filter there. Against the set, that is
`Entered`, `Left` or `Changed`, sent to the live query's own unbounded
channel. A panic in a `custom` condition ends that query alone.
Beside them the writer keeps each `watch_aggregate` with the rows of
its result, and after the live queries runs it again, whole, on the
same snapshot, for each commit that changed its collection; rows known
by their canonical group values are compared with those before
[§139](spec/139-live-aggregates-and-a-window.md).

## 6. Documents and data pages

**The value type** is `Document`: `Null`, `Bool`, `Int`, `Float`,
`String`, `Binary`, `Array`, `Object`, `Id` [§4.1](spec/04-key-decisions-and-rationale.md) [§11](spec/11-document-encoding.md), and `DateTime`, an
instant in UTC to the nanosecond, what a `SystemTime` or a
`trunkdb::DateTime` field stores [§69](spec/69-a-date-time-type.md). It holds trunkdb's `DateTime`, not a
`SystemTime`, which differs between platforms: the same everywhere,
over the whole range of an `i64` of seconds [§70](spec/70-a-portable-date-time.md). It's what
everything below the API works in, like LiteDB's `BsonValue`.
`Collection<T>` converts any `T: Serialize + DeserializeOwned` to and
from it through serde, and delegates to `Collection<Document>`: one
code path, not two [§13](spec/13-the-serde-bridge.md). Each stored object gets its id as an `_id`
field [§18](spec/18-merging-id-into-the-untyped-path.md), and a struct takes it with `#[serde(rename = "_id")] id:
Option<DocId>`: filled in on every read, used on insert if `Some`, made
if `None` [§59](spec/59-a-struct-carries-its-own-id.md); with the `derive` feature, `#[id]` under
`#[trunkdb::document]` writes that rename, and refuses another type at
compile time [§114](spec/114-the-document-attribute.md). The id is stored once, in the cell: an object is
encoded without its `_id`, and every read puts the cell's id there,
first, so there's no second copy that could say something else. `DocId`
serializes as a marked newtype the bridge stores as `Document::Id`, and
other formats as a UUID string, and `str::parse` reads that string back
into a `DocId` [§63](spec/63-an-id-filter-and-parsing-ids.md). A `Document` has a text form too: `to_string` writes
it as the tagged JSON an export uses (`{:#}` indented), and
`str::parse` reads it back, for tools that work in JSON rather than
structs; text, not `serde_json::Value`, so `serde_json` stays out of the
public API [§71](spec/71-documents-as-json-text.md). `find_with_ids` [§23](spec/23-find-with-ids.md) is gone, and a `cursor`
hands out `T`, as `find` does: a type that wants its id declares the
field; one from another crate is wrapped, its fields flattened [§74](spec/74-find-with-ids-removed.md).
Every document is an object, so every one has its `_id`: a write of
anything else fails with `Error::NotAnObject`, and `T` is a struct or a
map [§95](spec/95-only-objects.md).

**Nesting** is limited to 64 levels, counted as MongoDB counts (the
document is the first, each object or array inside adds one): deeper
writes are refused, so a damaged document can't take the decoder off
the end of the stack, and every export imports again [§56](spec/56-a-limit-on-nesting.md).

**Ids** are UUIDv7 by default: 16 bytes, starting with a timestamp, so
ids made in sequence land at the end of the primary index instead of
all over it [§4.2](spec/04-key-decisions-and-rationale.md).

**Data pages** hold one collection's documents, several per page, each
cell being `[flags][id][encoded document]` [§20](spec/20-several-documents-per-data-page.md). The id is stored in
the cell because nothing else maps a location back to its document, and
a scan or a rebuild needs that. A collection's next insert tries its
current data page first, then the fullest of its older pages with room
enough, and allocates a new one only when none has it. The pages with
room are in a **free-space map** per collection, by their exact free
bytes: in memory only, learned from writes (a page gets in with a
quarter of it free, and stays until less than 256 bytes are left), and
rolled back with the catalog it lives in. It starts empty at open, so
space freed in an earlier session comes back by a compaction [§75](spec/75-a-free-space-map.md).
A page left with no documents goes back to the file's free list, unless
it is the current page; one that stops being current is freed then, if
it holds nothing [§105](spec/105-a-differential-fuzz-target-for-queries.md).

**Large documents** keep only a pointer in their cell: the length and
the first of a chain of overflow pages [§26](spec/26-overflow-pages-and-u32-lengths.md). Lengths are `u32`.

**The catalog** (page 1) holds a cell per collection (its primary
index root, its current data page, its name) and one per index (its
fields, root and flags: unique, sparse) [§9](spec/09-catalog.md) [§28](spec/28-secondary-indexes.md) [§44](spec/44-sparse-indexes.md). It's read at
open and kept in memory, one per commit: a batch works on a copy of
its own, which becomes the readers' at commit and is dropped if the
batch fails [§78](spec/78-reads-through-a-snapshot.md).

## 7. Indexes

**One B-tree** implements every index [§10](spec/10-index.md). Keys are byte strings
compared byte by byte, so the tree knows nothing about documents
[§28.1](spec/28-secondary-indexes.md). Leaves are linked, so a range read walks along them, and a
read in sort order is a lazy walk, forward or backward, that stops when
the caller does [§49](spec/49-a-lazy-b-tree-walk.md), and copies only the entries it hands out [§97](spec/97-a-walk-that-copies-only-what-it-hands-out.md). Inserts change a page in place, and only a full
page is rebuilt when it splits; a split is by bytes, since keys differ
in length [§28.2](spec/28-secondary-indexes.md) [§52](spec/52-b-tree-pages-changed-in-place.md). Keys arriving in order fill their leaves
instead of leaving them half empty [§41](spec/41-compaction.md). A key is at most a quarter
page, so every page holds at least four.

**The primary index** maps each id, 16 bytes, to its document's
location (page and slot).

**Secondary indexes** (`ensure_index`) map an encoded value followed by
the id, so every key is unique and removing a document names exactly
one entry [§28](spec/28-secondary-indexes.md). The value encoding sorts in the same order `Filter`
compares values in [§34.1](spec/34-sorting-through-an-index.md). They cover:

- **nested paths:** `address.city` [§31](spec/31-nested-field-paths.md);
- **null and missing fields**, held as null, so `x == null` finds both
  through the index [§32](spec/32-null-and-missing-fields.md);
- **unique:** a second equal value fails the whole batch with
  `DuplicateValue`; nulls never count [§33](spec/33-unique-indexes.md);
- **multikey paths with `[*]`:** `tags[*]` holds a document once per
  element [§42](spec/42-array-conditions-and-multikey-indexes.md);
- **compound:** `(status, created)` sorts by the first value, then the
  second, and so on; the encoding is prefix-free [§43](spec/43-compound-indexes.md);
- **sparse:** no entry for null or missing values, for fields few
  documents have [§44](spec/44-sparse-indexes.md);
- **references:** a `DocId` field has a key, in the order ids were
  made, and sorts after strings [§62](spec/62-an-index-key-for-ids.md);
- **date-times:** a key in time order, sorting after ids [§69](spec/69-a-date-time-type.md).

An index is built and kept up to date in the same batch as the
documents it covers, so it can't disagree with them after a crash.
`Database::check` confirms it anyway [§39](spec/39-the-trunkdb-command-and-database-check.md).

## 8. Queries

A `Filter` holds a tree of conditions, a list of sort keys, a skip, a
limit and a selection, applied in that order [§72](spec/72-skip-for-paging.md)
[§35](spec/35-a-filter-builder.md) [§36](spec/36-or-not-and-nesting.md) [§47](spec/47-sorting-by-several-fields.md):

- **comparisons** (`eq`, `ne`, `lt`, `lte`, `gt`, `gte`) and a
  case-insensitive `contains` [§25](spec/25-op-contains-case-insensitive-substring-match.md); `is_in`, one of a list of
  values, an OR of `eq`s read as a point of an index per value [§115](spec/115-a-list-of-values-in.md);
- **`all`, `any_of` and `not`**, also as `&`, `|` and `!` [§36](spec/36-or-not-and-nesting.md);
- **`exists` and `missing`**, telling a stored null from an absent field;
  `size` for array lengths [§45](spec/45-exists-and-array-size.md);
- **conditions on array elements:** any element with `[*]` [§42](spec/42-array-conditions-and-multikey-indexes.md), or
  one element meeting several conditions with `elem_match` [§46](spec/46-conditions-on-one-element-elem-match.md).

**As text**, a filter reads and prints in a language like SQL's
`WHERE`, `status = "Complete" and seen >= 10 order by started_at desc
limit 1`, which says what the methods say and compiles to the same
`Filter`; values are JSON as a `Document` reads it, so their types
survive the round trip [§111](spec/111-filters-as-text.md).

Values of different types never compare equal or ordered, except
numbers: an `Int` and a `Float` compare by exact value [§34](spec/34-sorting-through-an-index.md). Arrays,
objects and binary have no order; `=` and `!=` compare them by their
contents, objects in any field order, `[1]` equal to `[1.0]` [§124](spec/124-equal-arrays-objects-and-binary.md).

**The planner** follows fixed rules, not costs [§28.4](spec/28-secondary-indexes.md). `explain` shows
what it picked (`QueryPlan`):

- **`Scan`:** read every document of the collection and check it.
- **`ById`:** the filter names its documents by id: `eq("_id", id)`
  (or `Filter::id`, which spares spelling `_id` [§63](spec/63-an-id-filter-and-parsing-ids.md)), or an OR of
  such. Look each up in the primary index, as `get` does.
  Beats every other plan [§61](spec/61-filters-on-the-id-through-the-primary-index.md).
- **`Index`:** read one index range, then check each document against
  the whole filter. An `Eq` beats an OR of indexed branches, which beats
  a range; bounds on more fields beat fewer [§43.3](spec/43-compound-indexes.md). `ne`, `contains`
  and `not` never use an index. A sparse index is used only when the
  filter rules nulls out [§44](spec/44-sparse-indexes.md).
- **`IndexUnion`:** one range per branch of an OR, each document read
  once [§36.3](spec/36-or-not-and-nesting.md).
- **`IndexOrder`:** with a sort and a limit, walk the index on the
  first sort key and stop at the limit, past any skip [§34.2](spec/34-sorting-through-an-index.md) [§49](spec/49-a-lazy-b-tree-walk.md) [§72](spec/72-skip-for-paging.md). A compound index
  whose leading fields an `Eq` fixes gives both the filter and the order
  (`status == "Queued"`, newest first, on `(status, created)`). Sort
  keys the index doesn't serve are sorted in memory within each run of
  equal served keys [§47](spec/47-sorting-by-several-fields.md). A sort by `_id` walks the primary index,
  whose order is `_id`'s, and `_id` comparisons with an id narrow the
  walk exactly [§96](spec/96-sorting-by-id-through-the-primary-index.md).

**A hint** names the index where the rules, blind to the data, pick
the wrong one [§132](spec/132-a-hint-in-the-query.md): `use_index(fields)`, or `no_index()` to scan.
The rules then choose among that index alone, so the plan is still a
function of the query and the indexes. A hint the index can't follow,
no such index or nothing to read it by, fails every read with
`Error::Hint`; it is never dropped quietly. `explain_run` lists the
plans it passed over, the filter hinted to each index, `_id` and a scan,
with the index entries each would read [§133](spec/133-the-plans-explain-run-passed-over.md): ranges counted whole a
leaf at a time, without a document read, up to 1,000 leaves; a walk in
sort order as far as the run read.

A document from an index range is checked against the whole filter,
since a range may hold more than its conditions match. Where the index
holds every field the filter names, its entries decide instead
[§100](spec/100-index-entries-that-answer-the-filter.md): an entry's values, decoded from the key with its id,
are checked, and the document is read only to be handed out, or where
a value stands for several (a cut string, a number past 2^53). `count`
then reads no document, a deep page only the page, and a sort by `_id`
after an index range only what it hands out. `Exists`, `Size`,
`ElemMatch` and multikey indexes never decide this way. Where entries
decide, the result follows the index: `check` is what finds an index
that disagrees with its documents. A walk decides a run of equal
values once, and a leaf inside its range at once where its first and
last entries agree on what the filter names: counted or skipped by its
slot directory, its keys never looked at [§101](spec/101-a-cheaper-walk-through-an-index.md).

A read decodes only what it needs [§99](spec/99-decoding-only-what-a-read-needs.md). A candidate is checked on the
top-level fields its filter and sort name, decoded alone, every other
field skipped with the same checks; it is carried as it is stored, its
page shared with the cache, and decoded whole only if it is handed out,
after sort, skip and limit. A typed read decodes straight into `T`
(`serde_stored.rs`), giving it exactly what the serde bridge would. An
untyped read whose candidates come from an index range or by id decodes
each whole once and checks that, as they nearly all match.

A selection (`select`) cuts what a read hands out down to `_id` and the
paths it names, in the order named, nested as stored, after the filter,
sort, skip and limit have run on whole documents
[§106](spec/106-projection-a-select-of-fields.md). It is decoded from the
stored document in one walk, the rest skipped with the same checks
(`select.rs`). Where an index's entries decide the filter and hold every
selected path, a match is built from its entry and its document never
read, if each selected value is held as stored: a bool, a string that
fits the key, an id, a date-time. A number is keyed as a float and a
missing field as null, so those are read from the document. Writes work
on whole documents, whatever is selected.

**Aggregates** (`aggregate`, with an `Aggregate`) put the documents a
filter finds in groups by the values of some paths, and per group count,
sum, average, or find the smallest and largest value
[§107](spec/107-aggregates.md). A missing field groups with null, values a
filter finds equal are one group, and sums are exact (a fixed-point
integer as wide as every float), so the plan doesn't change the answer.
A mean is that exact total divided, then rounded once [§131](spec/131-a-mean-rounded-once.md).
Where an index holds every path the aggregate reads, its entries are
aggregated without their documents for `count`, `avg`, `min` and `max`,
a document read only for a value the key doesn't hold as stored (a
group's or an extreme's number); a walk counts whole leaves of a group.
Without conditions, an index is walked only if it holds every document.
A column is named last, `count().named("n")`, so the string in
`sum("tries")` is always the field read [§134](spec/134-aggregate-columns-named-last.md).
A window, `Aggregate::window(path, duration)`, keeps the documents
within `duration` of the newest date-time at `path` among the matches:
read first, as the first match sorted newest first, then a condition
ANDed with the filter's, so an index on `path` bounds both reads
[§139](spec/139-live-aggregates-and-a-window.md).
A column can take a condition of its own, `filter` before its name, SQL's
`FILTER (WHERE …)` [§126](spec/126-a-condition-on-a-column.md): it takes
in only the group's documents that meet it, while the groups and the
other columns see every match. Index entries decide it where the index
holds what it reads, a leaf of counts at a time where its ends agree.

`explain_run` runs a query and says how it went: the plan, each index
read with the conditions that bound it and whether its entries decided
the filter, how many sort keys the index gave, and the entries and
documents examined, matched and returned, and how many results came
from index entries
[§94](spec/94-explain-with-execution-statistics.md). It is `find` with its counters read out: every read
counts, and only `explain_run` keeps the counts.

## 9. Concurrency

`Database` is a cheap, cloneable, `Send + Sync` handle; every clone,
`Collection` and `Batch` shares one open database [§27](spec/27-a-thread-safe-cloneable-database-handle.md). Three things
are shared, each with a lock of its own [§79](spec/79-writers-beside-the-readers.md):

- **the writer**: the batch being staged and the WAL, behind a mutex.
  A batch holds it from staging to its checkpoint; readers never take
  it. One writer at a time, and one process per file;
- **the last commit**: its catalog and its commit number [§78](spec/78-reads-through-a-snapshot.md), behind
  a read-write lock held only to take it or to replace it. A read
  (`get`, `find`, `count`, `cursor`, `export`, `check`) takes it when
  it begins and keeps it, a *snapshot*, to its end;
- **the committed pages**: the file, the page cache [§50](spec/50-a-page-cache.md) and the
  versions of pages kept in memory [§77](spec/77-the-committed-pages-apart-from-the-writers.md) [§80](spec/80-older-versions-kept-for-open-snapshots.md), behind one read-write
  lock. A page read takes it shared for the lookup, since a lookup
  changes only an atomic flag [§65](spec/65-a-read-lock-for-the-page-cache.md); a page coming in from the file, a
  commit and the two ends of a checkpoint take it alone, briefly. Pages
  are shared, not copied: a read gets the page itself (`Page`, an
  `Arc`), and a change copies it first [§64](spec/64-shared-pages.md).

**Nobody waits for anybody**, but for two short locks and `compact`:
- a batch is staged, logged and flushed beside the readers, and
  published by putting its pages into a map and swapping the snapshot.
  Reads see a batch entirely or not at all;
- a read goes on reading its own commit whatever is committed
  meanwhile, and a commit doesn't wait for it. An export of some
  seconds holds up no write;
- a checkpoint writes the file beside all of them.

**Versions** make that work [§80](spec/80-older-versions-kept-for-open-snapshots.md). A page read names its commit and
gets the newest version at or before it:
- a page has versions in memory from its commit until a checkpoint has
  written the newest to the file, and beyond that for as long as an
  open snapshot would read an older one. Otherwise the file's page is
  the only version, and everyone reads that;
- the first commit to change a page keeps the page as it was, if a
  snapshot is open; without one, nothing older is kept;
- of a page's versions, the newest stays and each one an open snapshot
  reads; the rest go at the next commit of that page or the next
  checkpoint. One snapshot beside many commits costs one older copy of
  each page changed, not one per commit;
- a checkpoint always writes the newest version. A reader that went to
  the file for a page just as a checkpoint wrote it sees that one
  began, and looks again;
- a freed page needs no tag: freeing and reusing it are newer versions
  of that page.

All of it is in memory: commit numbers start at 0 at every open, and
the file and the WAL are as they were.

**`compact` takes the database alone**: it rewrites every page, so none
can be kept as it was. It waits for the reads under way, lets none
begin until it is committed, and fails with `Error::SnapshotOpen` if a
snapshot is kept open beyond a read.

**A snapshot to keep** [§82](spec/82-a-snapshot-to-keep.md): `Database::snapshot` hands a commit
out for as long as the caller keeps it. Reads through it, a `View<T>`
per collection, are all of that moment. A cursor keeps the commit of
its creation the same way. Both cost what §80 says an open snapshot
costs: the pages changed since, in memory, and no `compact`.

**A limit on that memory** [§83](spec/83-a-limit-on-the-memory-snapshots-take.md): `OpenOptions::snapshot_memory`,
256 MiB unless set. Past it the oldest open snapshot is ended: its
reads fail with `Error::SnapshotTooOld`, and the writer goes on.
`Database::snapshot_info` says how much is kept. That ends the plan of
[§77](spec/77-the-committed-pages-apart-from-the-writers.md): §57 deferred snapshots until a workload asked, and many small
writes beside reads of up to 15 seconds did. Measured for it
(`bench/`, `long_read`): single writes back to back take the same time
with a snapshot open for 15 seconds as with none, and it kept 13 MiB.

**Measured** [§58](spec/58-measuring-reader-waits.md) (`bench/`, `reader_wait`): a writer committing
once or ten times a second costs readers nothing measurable. One that
commits back to back made them wait tens of milliseconds, up to half a
second; since §79 they do 18 to 31 times the reads beside it, with a
p99.9 under 100 µs [§79.7](spec/79-writers-beside-the-readers.md). A writer beside an export that never
stops waited up to 132 s for a commit; since §80, 24 ms, as fast as
without the export [§80.6](spec/80-older-versions-kept-for-open-snapshots.md). The price: two readers do a tenth fewer
short reads than before §80. Readers run in parallel since pages are shared [§64](spec/64-shared-pages.md) and looked up
under a read lock [§65](spec/65-a-read-lock-for-the-page-cache.md): four do 3.2 times the work of one. Eight do no
more than four.

## 10. The API

- `Database::open(path)`, or `open_with(path, OpenOptions)` with
  `cache_size` [§50](spec/50-a-page-cache.md) and `checkpoint_pages` [§53](spec/53-a-configurable-checkpoint-threshold.md) `checkpoint_wal_bytes` [§76](spec/76-a-wal-size-limit.md), `snapshot_memory` [§83](spec/83-a-limit-on-the-memory-snapshots-take.md) and `read_only` [§88](spec/88-a-read-only-open.md).
- `db.collection::<T>(name)`: `insert`, `get`, `update`, `upsert`,
  `delete`, `find`, `find_one`, `count`, `cursor`, `explain` [§12](spec/12-wiring-collection-document.md) [§29](spec/29-api-rounding-out-find-one-count-upsert-cursor.md),
  `explain_run` [§94](spec/94-explain-with-execution-statistics.md);
  `delete_many` and `update_many` with a closure [§37](spec/37-delete-many-and-dropping-a-collection.md) [§38](spec/38-update-many.md), whose
  panic fails the call with `Error::ClosurePanicked` [§137](spec/137-a-panic-in-update-many-s-change.md);
  `update_fields` with operators, `Update::new().set(..).inc(..).unset(..)`
  on dotted paths [§68](spec/68-update-operators.md), and `min`, `max`, `rename`, and `push`,
  `add_to_set` and `pull` on arrays [§73](spec/73-more-update-operators.md);
  `ensure_index` and `ensure_index_with` (`IndexOptions::new().unique()`,
  `.sparse()`), `indexes()` listing each index's fields and options
  [§28](spec/28-secondary-indexes.md) [§33](spec/33-unique-indexes.md) [§44](spec/44-sparse-indexes.md) [§60](spec/60-a-smaller-public-api.md).
- **Snapshots:** `db.snapshot()`, the database at one moment for as
  many reads as wanted; `snapshot.collection::<T>(name)` is a `View<T>`
  with `get`, `find`, `find_one`, `count`, `cursor`, `explain`,
  `explain_run` and `indexes` and nothing that writes; `collections`, `export`, `check`
  and `file_info` on the snapshot too. A `cursor` shows the moment it
  was made [§82](spec/82-a-snapshot-to-keep.md).
- **Changes:** `db.subscribe()`, a `Changes` receiving a `Commit` for
  each batch committed after it, its documents `Inserted`, `Updated`
  or `Deleted` and its collections `Dropped` [§135](spec/135-a-hook-after-each-commit.md).
- **Live queries:** `collection.watch(filter)`, a `LiveQuery<T>`
  reporting what matches, then each document that `Entered`, `Left` or
  `Changed`; a sort, a skip or a limit refused with `Error::NotLive`
  [§138](spec/138-live-queries.md); `collection.watch_aggregate(filter,
  &aggregate)`, a `LiveAggregate` reporting its rows `Entered`, `Left`
  or `Changed`, and `Aggregate::window` [§139](spec/139-live-aggregates-and-a-window.md).
- **Batches:** `db.batch()` across collections, typed and untyped alike
  [§24](spec/24-typed-batches.md) [§60](spec/60-a-smaller-public-api.md); all or nothing.
- **Filters** through `Filter`'s methods; `limit` takes an `Option`
  too [§60](spec/60-a-smaller-public-api.md); or from text, `text.parse::<Filter>()`, and back with
  `to_string()` [§111](spec/111-filters-as-text.md); `select` for the fields a read hands out [§106](spec/106-projection-a-select-of-fields.md);
  `aggregate` and `explain_aggregate` with an `Aggregate` [§107](spec/107-aggregates.md), a
  column's own condition with `filter` [§126](spec/126-a-condition-on-a-column.md); a condition of the
  application's own, `Condition::custom(fields, test)`, a closure given
  the fields it names, which mustn't use the database, and whose panic
  fails the call with `Error::ClosurePanicked` [§136](spec/136-a-condition-of-the-applications-own.md).
- **Reading a document:** `get(path)`, `as_i64`, `as_f64`, `as_str` and
  the other readers, each `None` for another type [§136](spec/136-a-condition-of-the-applications-own.md).
- **Conversions:** `Document::from_value(&t)` and `doc.into_value::<T>()`.
- **Limits** sit on the types they limit: `Document::MAX_NESTING`, and
  on `Database` the rest; its documentation lists them all.
- **What's public** is only this: `Database`, `Collection`, `Batch`,
  `Cursor`, `Snapshot`, `View`, `Changes`, `Document`, `DocId`, `Map`, the options, reports and errors, and
  `query`. Types a caller passes in are built with methods; types it
  gets back are `#[non_exhaustive]`, like `Error` and `Document` [§60](spec/60-a-smaller-public-api.md), and so is every
  variant with named fields, matched with `..` [§117](spec/117-variants-that-can-grow.md). Options and reports
  are `Clone` and `PartialEq` but not `Copy` or `Eq`, so a field of any
  kind can join them; a document becomes a `T` only through `into_value`
  [§118](spec/118-the-public-api-gone-through.md).
  No other crate's type is in it but `serde`'s traits and `std`'s: an
  object's fields are a `Map` of trunkdb's own, the `IndexMap` inside it
  out of sight [§116](spec/116-a-map-of-its-own.md).
- **Upkeep:** `export`/`import` as JSON Lines [§30](spec/30-export-and-import-as-json-lines.md), `check` [§39](spec/39-the-trunkdb-command-and-database-check.md),
  `compact` [§41](spec/41-compaction.md), `checkpoint` [§51](spec/51-one-flush-per-commit.md), `drop_collection` [§37](spec/37-delete-many-and-dropping-a-collection.md),
  `stats`: pages by kind, each collection's and index's size, the WAL's,
  and the pages a `compact` would leave, worked out exactly from the
  sizes without compacting [§93](spec/93-storage-statistics.md).
- **Errors:** one `Error` enum. A failed batch is rolled back and says
  why: `DuplicateId`, `DuplicateValue`, `NotFound`, and so on. An open
  says why it failed in types, not words: `Format` for a file or WAL
  this build can't read at all, `Io` of kind `WouldBlock` for a file
  another open holds, `Io` of kind `InvalidData` for damage [§92](spec/92-typed-format-errors.md).
  Code of the application's that panics inside a call, a
  `Condition::custom`, `update_many`'s change or a `T`'s conversions
  while it writes, is caught: `ClosurePanicked`, the batch rolled back,
  the writer's lock not poisoned [§136](spec/136-a-condition-of-the-applications-own.md) [§137](spec/137-a-panic-in-update-many-s-change.md).

## 11. Tools

- **`trunkdb` command:** `info`, `check`, `compact`, `export`, `import`
  [§39](spec/39-the-trunkdb-command-and-database-check.md), and `find`, a filter's matches as JSON lines [§112](spec/112-trunkdb-find.md), or with `--explain` how it ran [§113](spec/113-trunkdb-find-explain.md), and `fields`, every path a collection's documents have, in how many and as which kinds [§121](spec/121-trunkdb-fields.md); `info`, `check`, `export`, `find` and `fields` open the file read-only [§88](spec/88-a-read-only-open.md).
- **An Agent Skill**, `skills/trunkdb/`: how an AI agent uses the command on a file it meets, in the order that keeps its context small [§122](spec/122-a-skill-for-ai-agents.md).
- **`Database::check`** reads the whole file and reports what doesn't
  add up: damaged pages, pages neither used nor free, index entries
  without their document and the other way round [§39](spec/39-the-trunkdb-command-and-database-check.md) [§40](spec/40-page-checksums.md). It reads
  documents one at a time and checks an index entry by entry against
  the documents its entries name, so its memory doesn't grow with the
  file: four bytes a page, for its owner [§109](spec/109-check-in-memory-that-does-not-grow-with-the-file.md).
- **Compaction** rebuilds the collections into a fresh image with full
  pages, logs it through the WAL like any batch, copies it in and
  truncates the file [§41](spec/41-compaction.md). The image is built in a file beside the
  database, `<database>.compact`, whose name is gone as soon as it's
  open; the WAL's records are written from it, 32 MiB at a time, and
  reads find the new pages there until the checkpoint has copied them
  in. Memory stays at about 100 MB whatever the file's size, besides
  each index's keys while they are sorted [§90](spec/90-a-wal-record-written-from-the-pages.md) [§91](spec/91-a-batch-in-several-records.md) [§108](spec/108-compaction-built-in-a-file.md).
- **Export and import** write and read JSON Lines, with the types JSON
  lacks kept by tags (`{"$id": ...}`, `{"$binary": ...}`, `{"$date":
  "<RFC 3339>"}`), so an export is a readable backup and the path
  between file formats [§30](spec/30-export-and-import-as-json-lines.md). Exports are format 2 since `$date`; an
  import reads format 1 too [§69](spec/69-a-date-time-type.md).

## 12. Performance

`bench/` runs one workload against trunkdb, SQLite, redb and sled, with
the same documents and indexes and a durable commit everywhere, and
checks that all four give the same answers [§48](spec/48-benchmarks-against-sqlite-redb-and-sled.md).

Reads are level with SQLite, redb and sled or ahead of them; inserts, a
full scan and compaction are still behind. On an M1 Pro with 100,000
documents, the median of three runs (0.23.0 with §123):

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert, 1000 per commit | 32k/s | 44k/s | 45k/s | 30k/s |
| one commit | 4.7 ms | 4.7 ms | 4.9 ms | 10.2 ms |
| get by id | 2.8 µs | 6.1 µs | 2.0 µs | 2.6 µs |
| 1000 documents by index | 0.96 ms | 1.7 ms | 1.0 ms | 1.7 ms |
| count of 1000 by index | 0.01 ms | 0.02 ms | 0.03 ms | 0.26 ms |
| count of a range of 1000 | 0.01 ms | 0.02 ms | 0.03 ms | 0.34 ms |
| count of all 100,000 | 0.08 ms | 0.01 ms | 0.00 ms | 24 ms |
| oldest 20 of a status | 17.5 µs | 22.5 µs | 16.2 µs | 20.8 µs |
| page 50 of 20 of a status | 25 µs | 53 µs | 40 µs | 253 µs |
| first 50 by id | 32 µs | 37 µs | 23 µs | 28 µs |
| 50 by id after skipping 5000 | 38 µs | 120 µs | 143 µs | 0.95 ms |
| a tenant's first 20 by id | 22 µs | 1.2 ms | 20 µs | 24 µs |
| scan, 100,000 for a fifth | 61 ms | 43 ms | 50 ms | 68 ms |
| update, 1000 per commit | 13k/s | 16k/s | 19k/s | 24k/s |
| delete, 1000 per commit | 26k/s | 20k/s | 20k/s | 37k/s |
| compact | 1.2 s | 0.16 s | 0.42 s | — |
| file size after compact | 31 MB | 22 MB | 30 MB | — |

Both counts by index take about 15 µs (SPEC §104.8). Where reads stand:
- **Through an index, reads are fast:** a count reads no document, and
  counts a leaf at a time where its ends decide it (§100, §101, §104).
  A page by index reads only the page, and a deep one skips leaves
  whole. A sort by `_id` walks the primary index, or the range an `Eq`
  fixes (§96, §103).
- **Still behind:**
  - the count of all documents, which trunkdb counts leaf by leaf where
    the others keep a total;
  - a scan, 1.4× SQLite: each document's filtered fields are decoded
    into a small object to be checked;
  - "first 50 by id", 1.4× redb.
- **Writes:** inserts 0.7× SQLite's, updates 0.8×, deletes ahead, since
  the checkpoint threshold was raised to 4,000 pages ([§123](spec/123-batched-writes-checkpoint-less-often.md)): before, 59
  batches of 1,000 in 100 paid a checkpoint. Without flushes inserts
  run at SQLite's speed; the rest is the WAL's whole pages, 10 MB a
  commit (§123.5). Compaction inserts its entries one by one (§48.4),
  and its file is 1.4× SQLite's.

```
cd bench && cargo run --release
cd bench && cargo run --release --bin reader_wait   # readers while a writer commits
cd bench && cargo run --release --bin long_read     # one long snapshot beside single writes
cd bench && cargo run --release --bin select        # reads with a selection (§106)
cd bench && cargo run --release --bin aggregate     # aggregates by group (§107, §126)
```

Each gap had a measured cause, fixed one at a time:

| What | Before | Fix |
|---|---|---|
| "Oldest 20" by status | 19 ms | a lazy walk that stops at the limit [§49](spec/49-a-lazy-b-tree-walk.md) |
| Lookup by id | 32 µs | a page cache, 256 MiB by default, CLOCK eviction [§50](spec/50-a-page-cache.md) |
| One commit | 16 ms | one flush per commit; write-back at checkpoints [§51](spec/51-one-flush-per-commit.md) |
| Batched inserts | 7.5k/s | B-tree pages changed in place [§52](spec/52-b-tree-pages-changed-in-place.md) |
| Batched inserts | 22k/s | a checkpoint once 4,000 pages wait, not 1,000 [§123](spec/123-batched-writes-checkpoint-less-often.md) |
| First 50 by id | 189 ms | a walk of the primary index [§96](spec/96-sorting-by-id-through-the-primary-index.md), copying only what it hands out [§97](spec/97-a-walk-that-copies-only-what-it-hands-out.md) |
| First 50 by id, scans | 70 µs, 168 ms | decoding only what a read needs [§99](spec/99-decoding-only-what-a-read-needs.md) |
| Count of 1000 by index, page 50 of 20 | 0.9 ms, 1.2 ms | index entries that answer the filter [§100](spec/100-index-entries-that-answer-the-filter.md) |
| Count of all, of 1000 by index, page 50 of 20 | 7 ms, 0.17 ms, 0.35 ms | a walk that lends its entries and decides whole leaves [§101](spec/101-a-cheaper-walk-through-an-index.md) |
| Count of a range, tenant's first 20 by id | 0.14 ms, 0.6 ms | paths parsed once per query, small objects searched without hashing, sort keys looked up once [§102](spec/102-paths-resolved-once-per-query.md) |
| Tenant's first 20 by id | 0.38 ms | the `Eq`-fixed range walked in id order, which its keys are in [§103](spec/103-an-eq-fixed-index-walked-in-id-order.md) |
| Count of a range | 0.07 ms | entries matched on their bytes, leaves inside the interval counted whole [§104](spec/104-index-entries-matched-on-their-bytes.md) |
| Two fields of 1000 by index, untyped | 1.6 ms | a selection decoded alone, 1.05 ms, or answered from index entries, 0.7 ms [§106](spec/106-projection-a-select-of-fields.md) |

Still behind: in a scan, the small object each document's filtered
fields are decoded into, and whole 8 KB page images in the WAL for
every changed page. The copy per page read is gone [§64](spec/64-shared-pages.md), and a
changed page is copied once, not twice: `write_page` takes the `Page` [§67](spec/67-write-page-takes-the-page.md).

## 13. How it's tested

- **Unit and integration tests** for every section, most against a real
  file. Where an algorithm has a simple model (the B-tree against a
  `BTreeMap`, the planner against a scan), randomized tests compare the
  two.
- **Crash tests** inject a failure after any number of page writes, in
  the WAL, the write-back or the checkpoint, and check that the next
  open has the state before or after the batch, and nothing in between
  [§19](spec/19-durability-take-two-a-page-image-wal.md) [§51](spec/51-one-flush-per-commit.md).
- **Fuzzing** (`fuzz/`): damaged databases, WALs and exports, millions
  of them, and filters as text [§111](spec/111-filters-as-text.md), where anything but a panic or a hang is a correct answer. Each
  crash it found became a test in the library [§55](spec/55-fuzzing.md).
- **Differential fuzzing** (`fuzz/`, the `queries` target): any bytes as
  a program of index changes, writes, compactions, reopens, snapshots
  and reads on a valid database, every read through every way of reading
  held against the filter applied in memory to a model of what is
  stored. It catches 33 of the 36 hand-made mutations of §100–§104 that
  change answers [§105](spec/105-a-differential-fuzz-target-for-queries.md). Its writes meet unique indexes too, each
  refusal held against the database's own `Eq` over the model; that
  found an update that could store a duplicate [§119](spec/119-unique-indexes-in-the-queries-target.md).
- **Old files** (`tests/old_files.rs`): a database each release from
  0.23.0 wrote, kept in `tests/fixtures/` with what it holds, opened by
  every build, read as written and written to. It's the one test that
  sees a change to how a value is stored, made in the encoder and
  decoder alike [§120](spec/120-old-files-as-fixtures.md).
- **Mutation checks:** each section breaks its own code on purpose in a
  handful of ways and lists them; every one has to fail a test.
- **CI** runs the tests on Linux, macOS and Windows, clippy with
  warnings as errors, fmt, the minimum Rust version (1.89), a short
  benchmark run, and the fuzz crate: compiling, building the targets'
  database, and the `queries` target on 400 random programs from a new
  seed each run (guided fuzzing runs by hand).

## 14. Where the code is

```
DESIGN.md              how it works today, one subsystem at a time
spec/                  why: one numbered section per step (SPEC §N)
ROADMAP.md             what's done, what's open
GUIDE.md               how to use it, by example
CHANGELOG.md           every step so far, by release
src/
├── lib.rs             public re-exports, the crate's Error type
├── database.rs        Database: open, recovery, the commit protocol,
│                        checkpoints, OpenOptions
├── collection.rs      Collection<T>: CRUD, find, indexes, update_many,
│                        update_fields
├── batch.rs           Batch: typed atomic multi-op writes
├── changes.rs         Changes, Commit, Change: what each commit changed
├── cursor.rs          Cursor: streaming find, of one moment
├── snapshot.rs        Snapshot and View<T>: the database at one
│                        moment, to keep reading
├── query.rs           Filter, Condition, Sort; the planner; index
│                        entries deciding a filter
├── query_text.rs      a filter as text, read and printed
├── path.rs            field paths, parsed once per query
├── select.rs          projection: the fields a read hands out
├── aggregate.rs       Aggregate: count, sum, min, max, avg by group, a condition per column
├── explain.rs         explain and explain_run: how a query ran
├── stats.rs           Database::stats: where a file's space goes
├── update.rs          Update: operators on dotted paths
├── document.rs        Document and DocId: the schema-less value type
├── datetime.rs        DateTime, what Document::DateTime holds
├── serde_bridge.rs    any T: Serialize + DeserializeOwned <-> Document
├── serde_stored.rs    a stored document straight into T, for typed reads
├── json.rs            tagged JSON <-> Document, lossless
├── id.rs              IdGenerator + UuidV7Generator
├── catalog.rs         collections and their indexes (page 1)
├── data.rs            documents on data pages, overflow chains
├── free_space.rs      FreeSpace: data pages with room, by free bytes
├── storage/
│   ├── mod.rs         PageStore, page types
│   ├── file.rs        FileStore: the writer's staged pages, the
│   │                    header, the file lock
│   ├── pages.rs       Pages: the file, checksums, the committed
│   │                    pages, shared by readers and the writer
│   ├── page.rs        Page: a page's bytes, shared, copied on change
│   ├── snapshot.rs    SnapshotStore: the pages as of one commit
│   ├── slotted.rs     SlottedPage: slot directory + cells
│   ├── cache.rs       PageCache: CLOCK eviction
│   └── scratch.rs     ScratchStore: a compaction's image, in a file
├── index/
│   ├── mod.rs         Index trait
│   ├── btree.rs       BTreeIndex: every index, primary and secondary
│   ├── key.rs         index key encoding, KeyRange
│   ├── leaf.rs        leaf entry encoding
│   ├── branch.rs      branch entry encoding
│   └── in_memory.rs   InMemoryIndex: a fake, for tests
├── durability/
│   ├── mod.rs         Durability trait
│   └── wal.rs         WalDurability: the page-image write-ahead log
├── txn/
│   ├── mod.rs         TransactionManager trait, WriteOp
│   └── global_lock.rs GlobalLockTxnManager: one batch at a time
├── decode.rs          reading bytes from disk: running out is an error
├── crc32.rs           CRC-32C, in hardware where the CPU has it
├── export.rs          Database::export/import (JSON Lines)
├── check.rs           Database::check: consistency check
├── compact.rs         Database::compact: rebuild, file shrinks
├── fuzzing.rs         hooks for fuzz/, behind a feature, hidden
└── bin/trunkdb.rs     the `trunkdb` command (info, check, compact,
                         export, import, find, fields)
tests/                 the command line, and a time-series-shaped
                         workload through the public API; ui/: what
                         #[trunkdb::document] refuses, compiled
trunkdb-derive/        #[trunkdb::document], the `derive` feature's
                         macro (its own crate)
bench/                 against SQLite, redb and sled (its own crate)
fuzz/                  fuzz targets: damaged files, WALs, exports and
                         filter texts, and queries against a model (its own crate,
                         nightly; a smoke run on stable)
skills/trunkdb/        an Agent Skill: the command, for an AI agent
```
