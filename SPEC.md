# trunkdb — design spec

This document captures the reasoning behind trunkdb, not just its current
state: why it exists, what was decided and why, and what was deliberately
left out of v0. Treat it as the record to come back to when a later
decision needs to be checked against the original intent.

## 1. Motivation

Background: an experienced C# developer, very little prior Rust experience.
On Windows, small personal projects used [LiteDB](https://www.litedb.org/)
— a single-file, embedded, document-oriented database (much closer to
MongoDB than to a relational DB: no tables, no joins, no schema versions).
LiteDB was mostly dormant for a while and has since become active again,
but it's .NET-only, and .NET is rarely used now.

Two motivations, not one:
1. Get back the *easy embedded-document-store* experience LiteDB provided,
   in a language actually used day to day.
2. Learn database internals — page storage, indexing, transactions,
   durability — which is currently a knowledge gap, not just a language gap.

Explicitly **not** a goal: translating or porting LiteDB's C# implementation
into Rust. LiteDB is a reference for calibrating scope and comparing
decisions against, not a source to copy from. Own design decisions,
including ones that turn out different from LiteDB's, are the point.

## 2. Feasibility read

Stacking "learn Rust" and "learn database internals" simultaneously is the
real risk — not raw difficulty. Precedent that this scope is solo-feasible:
SQLite itself started as a solo project; `sled`, `redb`, and especially
[PoloDB](https://github.com/PoloDB/PoloDB) (a MongoDB/LiteDB-shaped,
single-file, embedded document database, in Rust, built solo) are direct
existence proofs for the exact shape of this project.

Decision: treat this explicitly as a learning project first, a personal
tool second, and "maybe publish to crates.io later" as a real but
non-blocking stretch goal — confirmed: the crate name `trunkdb` is
currently unclaimed on crates.io.

## 3. Architecture: layers and what's real vs. fakeable

An embedded document database breaks into layers. Each sits behind a small
trait so a naive v0 implementation can be swapped for a real one later
without reshaping anything above it — **except** the storage/page layer and
the document format, where the *contract* needs to be right early because
everything else is expressed in terms of it (even though the storage
layer's own implementation is still incomplete in v0).

| Layer | Trait | v0 implementation | Real from day one? |
|---|---|---|---|
| Storage/pages | `PageStore` | `FileStore` (real, tested — see §7) | Yes |
| Document format | — (`Document` enum) | real enum + byte encode/decode (§11) + serde bridge (§13) + tagged JSON (§30.1) | Yes |
| Id generation | `IdGenerator` | `UuidV7Generator` | Yes |
| Indexing | `Index` | `BTreeIndex` (persisted B-tree, O(log n), byte-string keys; primary `_id` index §10, secondary indexes §28) + `InMemoryIndex` (fake, tests only) | Yes |
| Transactions | `TransactionManager` | `GlobalLockTxnManager` (one global lock), wired via `Database::write_batch` (§17); rollback via staged pages (§19.5) | Yes — atomicity and rollback; no isolation |
| Durability | `Durability` | `WalDurability` (page-image write-ahead log, §19; op-level before that, §16) + `NoopDurability` (fake, tests only) | Yes |
| Query execution | — (`Filter`) | comparisons combined by AND, OR and NOT (§36), built with a builder (§35); a full scan, one secondary index's range (§28.4), a union of ranges for an OR (§36.3), or an index read in sort order up to the limit (§34.2), by a fixed rule | No — deliberately minimal |
| Public API | `Collection<T>`, `Database` | Both `Collection<Document>` (§12) and typed `Collection<T>` (§13.3) real | Yes |

## 4. Key decisions and rationale

### 4.1 Document model
Own `Document` enum (`Null`, `Bool`, `Int`, `Float`, `String`, `Binary`,
`Array`, `Object`, `Id`) — the schema-less value type everything else is
expressed in. Equivalent in spirit to LiteDB's `BsonValue` / MongoDB's BSON
/ `serde_json::Value`.

`Collection<T>` converts `T: Serialize + DeserializeOwned` to/from
`Document` by implementing serde's `Serializer`/`Deserializer` traits
*against* `Document` (the same technique the `bson` and `serde_json` crates
use) — real and tested, see §13. Built in the opposite order from how this
section originally imagined it: `Collection<Document>` (the untyped path)
was wired to real storage first (§12), and typed `Collection<T>` second,
as a thin layer converting to/from `Document` and then delegating to
`Collection<Document>` for the actual storage work (§13.3) — not two
separate code paths, matching the original intent here, just arrived at
from the other direction.

### 4.2 Id generation: UUIDv7, not UUIDv4
Two reasons this isn't just cosmetic:
- **Size**: 16 bytes either way (ids duplicate into every index entry and
  every reference, so size matters), but the sortability point below is the
  bigger one.
- **B-tree insert locality**: UUIDv7 embeds a timestamp prefix, so ids
  created in sequence sort close together. Now that `BTreeIndex` (§10) is
  the real index, sequential-ish ids insert near the "end" of the tree;
  fully random UUIDv4 ids would scatter inserts randomly, causing more
  page splits and worse locality as data grows. UUIDv7 (RFC 9562) gets
  this property while staying a standard UUID, avoiding a hand-rolled id
  format like LiteDB/Mongo's ObjectId.

Id generation is pluggable via `IdGenerator`, in case a caller-supplied
scheme is ever wanted.

### 4.3 Query capability: richer than "equality only"
Originally scoped as single-field equality only. Widened after checking
real reference workloads (§5): v0 supports comparison operators (`=`, `!=`,
`<`, `<=`, `>`, `>=`) and a case-insensitive substring match
(`Op::Contains`, §25) ANDed together, plus sort-by-field and limit (real
and tested, §14) — still scan-based, still no
OR/nesting/planner/joins/aggregates.

### 4.4 Transactions: atomic batch writes are v0, not a stretch goal
Originally scoped as "single global lock, no multi-op atomicity." Widened
after reading a desktop app's storage code (§5.2): **atomic multi-document
batch write** (all-or-nothing across several documents) is a first-class
v0 requirement, because that app's file-based writer only has
best-effort rollback (re-writing snapshotted file contents on failure) and
its own code says so directly — a real transaction primitive is a concrete
improvement, not a nice-to-have.

`GlobalLockTxnManager` gives atomicity (all ops in a batch succeed or none
do, enforced by the caller's `apply` closure returning early on error) but
no isolation. Now wired to a real, public entry point —
`Database::write_batch` — and durable, via the same WAL as every other
write. See §17. A batch that fails partway is rolled back completely:
every page write is staged in memory until the batch commits, so nothing
of a failed batch ever reaches the file (§19.5).

### 4.5 Durability: no longer a no-op
Originally `NoopDurability` (a crash mid-write could corrupt the file —
documented, deliberate, not an oversight). Now real: `WalDurability`, a
write-ahead log that makes every batch crash-safe. First built as an
op-level log (§16), which turned out not to protect writes spanning
several pages; replaced by a page-image log (§19).

### 4.6 Concurrency
v0 is single-process only: a database file is exclusively locked by
whoever opens it (§21.1), so a second process gets a clear error instead
of corrupting the file. Within the process, `Database` is a cloneable,
thread-safe handle (§27): readers run in parallel, write batches one at
a time, and a reader waits while a batch commits.

## 5. Reference workloads

Three workload shapes, taken from real applications, calibrate what
trunkdb has to do. v0 was built standalone against synthetic data shaped
like these. The sync workload (§5.3) is the first planned live use, the
large-document workload (§5.2) the second; see §39 for the roadmap.

### 5.1 Time-series workload
A data-shape and query-pattern reference, not a planned integration:
- A mix of high-frequency, append-only data (location pings, event logs)
  and small reference tables (geofences, a singleton state row).
- Query patterns: time-range filters (`tst >= ? AND tst <= ?`) combined
  with sort + limit; several optional filters ANDed together dynamically;
  "most recent row" (`ORDER BY id DESC LIMIT 1`); one unique constraint
  (`zone_key`); one aggregate (`LEFT JOIN` + `GROUP BY` + `COUNT`) that a
  document DB should *not* try to replicate — that becomes application
  code instead.
- In a relational store, such data ends up as JSON blobs in TEXT
  columns — exactly the pattern a document store removes.

### 5.2 Large-document workload (a desktop app)
A Rust desktop app that keeps its data as a directory of files, one per
entity. Integration plan: trunkdb becomes an *alternative* `Storage`
backend behind a trait, selectable against the file-based one.

What its storage code showed:
- About ten entity kinds sharing common fields — a document-shaped data
  model, arguably a better fit than a relational one. Some entities hold
  long-form text: tens to hundreds of KB per document.
- Every lookup rescans and re-parses the whole directory, including from
  inside write operations — get-by-id is effectively O(all files).
- Relationships between entities are hand-maintained linked lists of
  string ids, with manual re-pointing and cycle detection — a real index
  would replace much of that logic.
- Multi-entity writes get best-effort atomicity by snapshotting and
  restoring file contents on failure; there is no real transaction
  protocol for loose files. This directly motivated making atomic batch
  writes a v0 feature (§4.4).

**Planned integration sequencing**: build trunkdb standalone and solid
first → refactor the app to define a `Storage` trait that its existing
file-based writer implements unchanged (pure refactor, no behavior
change) → only then write a second `Storage` impl backed by trunkdb.
Two moving targets (a new storage engine and a live app's integration)
are deliberately not built at the same time.

### 5.3 Sync workload (a small app replacing MongoDB)
A small Rust desktop app, low thousands of documents in one main
collection, currently on MongoDB — genuine overkill here, and the app's
only reason to need a running database server. Its whole database
surface is three operations:
- **Sync**: a periodic run looks every record up by the app's own
  business key (independent of `_id`), inserts new ones and updates the
  rest (at least a `last_seen` field) — ideally as one atomic batch.
- **Query**: a dynamic AND of optional exact-match filters, a
  case-insensitive substring search on two text fields, and a dynamic
  sort field/direction plus limit.
- **Partial update** of a few fields, which becomes read-modify-write
  against `Collection::update`'s whole-document replace — an
  application-level detail, not a gap.

A prototype ran this workload against trunkdb 0.1.0, on a few hundred
real documents:
- **Document size**: ~500 B min / ~900 B avg / 3 KB max — well within
  one page (no overflow involved, §26.2).
- **Space**: one 8 KB page per document (§11.4), ~9× overhead. Harmless
  at this scale; still the reason to fix the data-page format before
  real data lands — since done (§20).
- **`find()` didn't return `DocId`s** (typed path): the prototype kept a
  business-key → `DocId` map from `insert`, which is gone after a
  restart. The sync looks records up by key and then updates them, so
  this was the one real functional blocker — resolved by
  `find_with_ids` (§23).
- **Substring search** was done app-side after a full scan — works at
  this scale, but belongs in `Filter` (since §25: `Op::Contains`).
- **Every write was its own batch** (two `fsync`s each), while a sync
  touches every record — it wants one typed, atomic batch per run
  (since §24).
- **Threading**: `Database` was `Send`, so `Mutex<Database>` in the
  app's shared state worked; since §27 `Database` itself goes there.
- Secondary indexes aren't needed: at low thousands of documents a full
  scan takes milliseconds.

## 6. v0 feature set

**In scope, real (no faking):**
- Single-file database, page-based storage
- `Document` value type with typed (`Collection<T>`) and dynamic
  (`Collection<Document>`) access via one serde-based mechanism
- UUIDv7 ids, pluggable generator
- CRUD: insert (auto-id), get-by-id, update-by-id (full replace),
  delete-by-id, iterate-all — later joined by `find_one`, `count`,
  `upsert` and a streaming `cursor` (§29)
- Filter: comparison operators ANDed together, sort-by-field, limit
- Atomic multi-document, multi-collection batch write (§17) — all ops
  land or none do: a failing op rolls the whole batch back, and a crash
  leaves either the pre- or the post-batch state (§19)
- Crash survival via a page-image write-ahead log (§19)
- A persisted B-tree primary (`_id`) index (§10)
- Documents larger than a page, via overflow pages (§26)

Originally planned as fakes behind a trait (`Index` as linear scan,
`Durability` as no-op, `TransactionManager` unwired) — all three have
since been replaced by real implementations; §3's table is the current
state. The fakes (`InMemoryIndex`, `NoopDurability`) remain, for tests
only.

**Explicitly out of v0:**
- A real (cost-based) query planner; secondary indexes were added
  later (§28), with a fixed rule for when to use one
- Joins, aggregates (GROUP BY/COUNT-style — left to application code,
  per §5.1); OR/nested conditions were added later (§36)
- Concurrent multi-process access; concurrent writers (batches are
  serialized, §27)
- Compaction/vacuum, encryption, schema validation; online backups
  (export, §30, is a snapshot, but writers wait for it)

## 7. Page layout (`storage/`)

`FileStore` (implementing `PageStore`) and the generic `SlottedPage`
primitive are now real and tested — the first genuinely finished layer.

### 7.1 Page size: 8192 bytes
Matches LiteDB's own page size, deliberately — it keeps numbers comparable
to the reference implementation later. Also a better fit than the
originally-considered 4096 for a document DB specifically: documents are
whole JSON-ish objects (hundreds of bytes to a few KB), so fewer, larger
pages means less fragmentation across page boundaries; and once a real
B-tree exists, larger pages hold more keys per node, giving higher fanout
and a shallower tree.

### 7.2 Header page (page 0) and the free list
Page 0 is reserved and positionally identified (never tagged). It holds an
8-byte magic (`TRUNKDB1`, catches opening a corrupt or foreign file early),
`page_size` (validated against the build's `PAGE_SIZE` constant on open),
`page_count`, `free_list_head`, and — since §21.2 — a `u32` format
version.

Freed pages form an on-disk linked list — the same technique SQLite's
freelist trunk pages use: a freed page's own first bytes hold the next
free page's id (`0` doubles safely as "no free page," since page 0 is
never itself freeable). `allocate_page` pops the free-list head if one
exists, otherwise grows the file by one page. Content of a freshly
allocated page — whether reused from the free list or newly grown — is
unspecified until the caller writes to it; allocation does not zero reused
pages (matching how most page allocators behave).

`write_page`/`free_page` on the header page (id 0) are rejected through
the generic `PageStore` interface — the header is cached in memory by
`FileStore` and managed internally, so writing it through the generic path
would desync that cache from disk.

### 7.3 Page type tags and `SlottedPage`
Every page except the header carries a 1-byte type tag as its first byte
(`Free`, `Catalog`, `Data`, `IndexLeaf`, `IndexBranch`, `Overflow` —
the last added in §26) — cheap corruption detection (e.g.
`allocate_page` verifies a popped free-list page is actually tagged
`Free`), and lets `read_page`'s raw bytes be self-describing.

`SlottedPage` is one generic primitive — a slot directory growing forward
from a small header, cells packed backward from the end of the page, slots
identified by index rather than byte offset — that backs three different
page kinds:
- **Catalog** pages (§9, real and tested): cells are collection registry
  entries.
- **IndexLeaf** pages (§10, real and tested): cells are `(key,
  RecordLocation)` entries.
- **Data** pages (§11, §20, real and tested): cells are a serialized
  document prefixed with a flags byte and its own `DocId`, as many per
  page as fit.

This is the same layout PostgreSQL heap pages and SQLite B-tree pages use,
and it's *why* `RecordLocation` is `{ page, slot }` rather than `{ page,
offset }` — a cell can be relocated within its page later (a future
compaction pass) without invalidating anything that references it, since
only the slot directory entry changes.

### 7.4 Why a persisted index instead of rebuild-on-scan
Originally considered: keep the index purely in memory (as `InMemoryIndex`
already is) and rebuild it by scanning all data pages on `Database::open`.
Rejected in favor of persisting the index for real, because of what it
saves later: a real B-tree, when it replaces the naive linear index, would
otherwise have to invent its own page-persistence and catalog-wiring from
scratch. Building that plumbing once now — even though v0's index stays
algorithmically naive (a flat, linked chain of `IndexLeaf` pages, O(n)
lookup) — means the *leaf page format* doesn't change when real B-tree
branch nodes are added on top later. Concretely, what's fixed now vs. what
changes later:
- **Fixed now, unchanged later**: the `IndexLeaf` page format itself (via
  `SlottedPage`), the `Index` trait's signature (`insert`/`remove`/
  `lookup`/`scan` — a flat leaf chain and a real multi-level tree both
  implement the same four operations), and the catalog storing a single
  `root_page: PageId` per collection (today it always happens to point
  directly at a leaf page — a degenerate, height-1 tree; later it
  sometimes points at a branch page instead).
- **Added later, not yet built**: branch/internal node pages (`(key,
  child_page_pointer)` cells, using the same `SlottedPage` primitive with a
  different cell payload), split-on-overflow logic, and O(log n)
  navigation. Leaf pages stay linked to each other via `next_page` even
  after branch nodes exist — real B+trees (e.g. InnoDB) keep this, since it
  makes range scans fast without re-descending the tree.

One consequence: because the index is the persisted source of truth for
"which documents exist," a delete must be a real tombstone/removal on the
data page — not just dropped from the index — or a future rebuild-style
operation would resurrect it.

Update: the branch/internal node pages and split-on-overflow logic
anticipated above are now built — see §10.

## 8. Ownership: one `RefCell` boundary, not one per component

*Updated by §27: the boundary is now one `RwLock` inside an `Arc`, and
`Collection` holds a clone of the `Database` handle instead of a borrow.
The reasoning below — one narrow interior-mutability boundary, plain
`&mut dyn PageStore` parameters beneath it — is unchanged.*

Decided and implemented: `Index`/`Collection` don't hold their own
`Rc<RefCell<_>>` handles into storage. Instead, `Database` holds the
crate's one interior-mutability boundary — `store: RefCell<FileStore>` —
and everything below it (`Index` trait methods, `Collection`'s method
bodies) receives `&mut dyn PageStore` (or `&dyn PageStore`) as a plain
parameter for the duration of a single call, the same way
`TransactionManager::apply_batch` already receives its `apply` closure
rather than owning callback state.

Why not `Rc<RefCell<_>>` per component: `RefCell`'s aliasing rule (one
mutable borrow, or any number of shared borrows, never both) is checked at
*runtime* — a conflicting borrow panics instead of failing to compile.
Spreading it through every internal type gives up exactly what Rust's
compile-time borrow checker is for. The mature pattern is the opposite
instinct: localize interior mutability to one deliberately narrow
boundary, and keep everything else ordinary, compile-time-checked
ownership.

Why *some* interior mutability is still needed, rather than none: with a
plain `store: FileStore` field and no `RefCell`, `Database::collection()`
would need `&mut self` to let `Collection` reach in and call `FileStore`'s
`&mut self` methods — and holding one `Collection` handle would then block
getting a second one (e.g. `"users"` and `"posts"` open at the same time),
since Rust disallows two overlapping mutable borrows of the same value.
The single `RefCell` absorbs that: `Database::collection()` stays a
shared-`&self` method, so multiple lightweight `Collection` handles can
coexist, while the controlled, single-point-of-truth mutation still
happens underneath. `database::tests::two_collections_coexist` is the
compiled proof of this.

Concrete consequence: the `Index` trait's methods (`insert`/`remove`/
`lookup`/`scan`) were widened to take a store parameter — a deliberate
signature change made now, while `InMemoryIndex` (which ignores the
parameter) was still the only implementation, rather than something a real
persisted index would have forced retroactively.

## 9. Catalog (`catalog.rs`)

*Updated by §28: every catalog cell starts with a kind byte, and the
catalog also records secondary indexes (§28.5).*

Real and tested: `Catalog` maps collection name → `CollectionMeta { index_root:
PageId }`, backed by a chain of `SlottedPage(Catalog)` pages starting at the
fixed, well-known page 1. `CollectionMeta` first held just the one
field — no `current_data_page` (deferred: v0 allocated a fresh data page
per document; it did prove wasteful, and §20 added the field) — and no
`name` (redundant — it's already the `HashMap` key, and only needs to
exist in the *on-disk cell*, not the in-memory struct).

### 9.1 `PageStore::try_read_page`: `Option`, not `Err`, for "not allocated yet"
Added because "this page hasn't been created yet" is an expected, ordinary
outcome (a fresh database) — using `Err` for it conflated *failure* with
*absence*. `try_read_page` returns `Ok(None)` for a never-allocated page id
and `Ok(Some(bytes))` otherwise; genuine I/O errors still surface as `Err`.
It only distinguishes "never allocated" from "allocated" — it does not
check free-list membership, so it isn't safe in general for a page that
*could* have been freed. Fine for the catalog root specifically, since
nothing ever frees it.

### 9.2 Corruption check: page-type tag, not slot count
A freed page's bytes coincidentally read as "0 slots" at the same offset a
genuinely-empty `SlottedPage(Catalog)` would — `free_page` only writes the
type tag and free-list-next pointer, leaving the rest zeroed, same as a
fresh page. Slot count alone can't tell them apart. The page's type tag
can: `Catalog::load` checks `page.page_type() == PageType::Catalog`
after a successful read, and treats a mismatch as corruption (a real
`Err`), not as "must be empty." This reuses the page-type-tag mechanism
built in §7.3, rather than requiring new free-list-walking machinery.

### 9.3 Bootstrap-ordering assumption: an `assert`, not a stronger type
`Catalog::load` assumes the catalog page is the *first* page ever
allocated in a fresh file (so `allocate_page()` is guaranteed to hand back
id 1). This is enforced with `assert_eq!`, not a typestate-style API that
would make violating it a compile error. Typestate was considered and
rejected: the assumption's entire blast radius is a few lines inside one
function (`Catalog::load` itself), never touched by external callers — the
cost of a second type and a consuming transition method wasn't worth it
for a risk that's local, not spread across a broad API surface. The assert
still turns a silent, corrupting violation into a loud, immediate panic.

### 9.4 Naming: `load`, not `open`
Considered `open` (to signal "creates if missing, reads if present,"
matching `FileStore::open`'s own behavior) but kept `load` — a deliberate
judgment call, not an oversight: "open" fits establishing a live handle to
an external resource (a database, a file); "load" fits populating a
structure from a source, which is what a catalog fundamentally is, even
though this one instance also happens to create-on-miss. Compensated by
being explicit in `load`'s doc comment that it creates the catalog page
when necessary, since the name alone hints at that less strongly than
"open" would have.

### 9.5 `create_collection`
Allocates a page to serve as the new collection's `index_root`, encodes
`(name, CollectionMeta)` into cell bytes (fixed 8-byte `index_root` first,
then the name's raw UTF-8 bytes last — no length prefix needed, since
`SlottedPage`'s own slot directory already tracks each cell's length),
walks the catalog page chain for room (extending it via `next_page` if
every page is full), writes the cell, and updates the in-memory map to
match. The allocated `index_root` page is also written immediately as an
empty `SlottedPage(IndexLeaf)` — added after the index (§10) turned out
to assume a readable, correctly-typed page at `root` from its very first
call, which a merely-*allocated*-but-never-written page isn't. Names are
limited to 255 bytes since §22.3.

`SlottedPage` gained `#[derive(Clone)]` for this: writing a page's bytes to
disk via `into_bytes(self)` consumes it, but the in-memory value is
sometimes still needed afterward (e.g. a freshly-allocated chained page,
written once immediately, then kept as the page subsequent code continues
operating on) — cloning (a cheap `Vec<u8>` copy) resolves that without
resorting to re-parsing bytes back into a second `SlottedPage`.

## 10. Index (`index/btree.rs`, `index/branch.rs`, `index/leaf.rs`, `index/in_memory.rs`)

*Updated by §28: keys are byte strings of any length up to 1024 bytes,
not `DocId`s, and pages split by bytes, not entry count (§28.2). The
primary index's pages are byte for byte what they were.*

Real and tested: `BTreeIndex` is the disk-backed `Index` implementation
for the primary `_id` index — a real B-tree with linked leaves, replacing
the earlier `LinearIndex` (a flat, unordered chain of `IndexLeaf` pages,
O(n) for everything; deleted, fully superseded). `insert`/`lookup`/
`remove` are O(log n): a page holds ~270 leaf entries or ~290 branch
entries at 8192 bytes, so even a few hundred thousand documents keep the
tree at 3-4 levels. `InMemoryIndex` is untouched — still the fast,
non-durable fake for tests that don't want real I/O.

### 10.1 Entry format (`leaf.rs`): fixed 26 bytes — unchanged, as forecast
A leaf entry is still `[DocId: 16 bytes][RecordLocation: 8-byte page +
2-byte slot]`, exactly as `LinearIndex` used it. §7.4 predicted this
format would survive a real B-tree unchanged, since only the node
hierarchy above leaves was expected to change — that held.

### 10.2 Branch pages: a new page type, no changes to `SlottedPage`
`PageType::IndexBranch` is new; branch cells (`branch.rs`) are
`(separator_key: DocId, child: PageId)` — 24 fixed bytes, the same
fixed-size property leaf entries have (see §10.4 for why that matters).
No change to `SlottedPage` itself was needed — branch pages are just
slotted pages whose cells mean something else, the same reuse `Catalog`/
`Data`/`IndexLeaf` already share.

One field does double duty: `SlottedPage::next_page` means "next leaf
sibling" (the scan chain) on an `IndexLeaf` page, but "rightmost child
pointer" on an `IndexBranch` page — the classic "n separator keys route
to n+1 children" scheme, where a branch's cells `[(k0,c0), (k1,c1)]` plus
rightmost `R` means `c0` handles keys `< k0`, `c1` handles `k0 <= keys <
k1`, and `R` handles `keys >= k1`. No new page field, just a
page-type-dependent meaning for an existing one.

### 10.3 The root page id is permanent; its role isn't
`CollectionMeta.index_root` is handed out once and never reassigned. A
tree still needs to grow taller over time — root starts as a leaf,
becomes a branch, later a taller branch. When the root overflows and
splits, its current content (already rewritten in place as the "left
half" by the split that bubbled up to it) is relocated to a *freshly
allocated* page, and the root's own page id is overwritten with a new
branch page pointing at (separator → relocated page) with rightmost = the
split's new sibling (`grow_new_root`). Every other split just allocates a
fresh page for the new sibling and leaves the original id as "left" — the
root split is the one case where the *original content* has to move,
because its id is the one thing that can't.

### 10.4 Splitting: fixed-size entries make it foolproof
Leaf and branch entries are always exactly 26 or 24 bytes. So if `n+1`
entries overflow a page that held `n` just fine, splitting those `n+1`
into two roughly-equal halves always leaves each half fitting — a
guarantee from the arithmetic (`(n+1)/2 <= n` for `n >= 1`), not something
asserted defensively per split. Leaf and branch splits differ in one way,
matching standard B-tree/B+-tree semantics: a **leaf split** copies the
smallest key of the right half up as the separator (leaves hold real
data, so nothing is removed); a **branch split** promotes *and removes*
the middle entry's key (branch keys are pure routing information, never
duplicated).

### 10.5 Deliberately not built: rebalancing on delete
`remove` descends to the right leaf and tombstones the cell — no merging
or redistributing underfull nodes afterward. This isn't a new gap the
B-tree opened; it matches the rest of the codebase's existing stance
(tombstones never reclaim space anywhere, no compaction pass exists yet).

### 10.6 A side effect: `scan` is now sorted
Leaves chain left-to-right in key order, so `scan()` — used by every
`Collection::find` — returns documents in ascending `_id` order for free,
where `LinearIndex` returned arbitrary insertion order. Tested
(`scan_after_many_inserts_is_sorted_and_complete`, 2000 entries inserted
in shuffled order) but not yet promised as part of the `Index` trait's
contract — worth revisiting if something later wants to rely on it.

### 10.7 Two bugs surfaced while building the original `LinearIndex`
(kept for history — both fixes carried forward unchanged into
`BTreeIndex`, since they live in `SlottedPage`/`Catalog`, not the index
itself.)
- `SlottedPage::has_room_for(data_len)` replaces a hand-rolled
  `free_space() < data_len` check that forgot to account for the 4-byte
  slot-directory entry every cell also costs. `catalog.rs`'s
  `create_collection` had the identical bug already (never triggered,
  since no test happened to land in that 4-byte gap). Both now use
  `has_room_for`, which encapsulates the correct comparison instead of
  requiring every caller to know about `SlottedPage`'s internal
  `SLOT_LEN`.
- `create_collection` allocated `index_root` but never wrote anything to
  it (see updated §9.5) — the index's first read of `root` would have
  misread whatever bytes were already there as a bogus page. Fixed by
  initializing it as an empty `IndexLeaf` page at allocation time.

## 11. Document encoding (`document.rs`, `data.rs`)

*Updated by §26: lengths and counts are `u32` now, and the cell format is
`[u8 flags][DocId][payload]` (§20.5) with overflow cells (§26.2).*
Real and tested: `document::encode_document`/`decode_document` convert a
`Document` to and from bytes — one type tag, then a tag-shaped payload
(fixed-width for scalars, `u16` length + bytes for `String`/`Binary`, `u16`
count + that many encoded elements for `Array`, count + `(key length, key
bytes, encoded value)` triples for `Object`). `u16` is enough for every
length here since a whole document has to fit inside one 8192-byte page's
cell budget regardless (§7.1). `data::encode_record`/`decode_record` sit on
top: a document's own `DocId`, prefixed onto its encoded bytes — this is
the actual `Data` page cell format referenced in §7.3. The `DocId` has to
be stored explicitly rather than assumed to live inside the document's own
fields, because nothing else records it once persisted: the index maps
`id -> location`, never the reverse.

### 11.1 Recursive encode/decode: why `decode_document` returns a remainder
`Array`/`Object` nest arbitrarily, and neither the encoder nor decoder
knows ahead of time how many bytes a nested element occupies — only
`Array`/`Object` store an element *count*, never a total byte size. Encode
handles this for free (it just keeps appending to one growing buffer,
recursing into `write_document` for nested elements). Decode can't do the
equivalent so easily, since it's handed one flat slice and has to work out
where each value stops: `decode_document(bytes) -> (Document, &[u8])`
returns not just the parsed value but everything *after* it, and a
recursive caller (decoding an `Array`/`Object`'s elements) feeds that
remainder back in as the start of the next element. Every level of nesting
trusts its recursive call's remainder without needing to know anything
about what's inside it — this is what lets arbitrarily deep nesting work
without the format needing to record total byte sizes anywhere.

### 11.2 Errors: only for real corruption, not truncation
`decode_document` returns `Err` for exactly two things: an unrecognized
type tag, and invalid UTF-8 in a string or object key — both realistic
disk-corruption symptoms, mirroring how `PageType::from_u8` and the
catalog's name-decoding already behave. It does *not* guard against a
truncated buffer; that panics via ordinary out-of-bounds slicing instead
of a graceful error. Same trust level as the rest of the crate's cell
decoders: nothing ever reads a cell's bytes except code that wrote them,
so "the buffer is shorter than the format says it should be" isn't a
reachable state in practice.

### 11.3 Considered and rejected: two-pass size-then-write encoding
A first pass to compute the exact encoded size (so `encode_document` could
`Vec::with_capacity` the precise total up front, avoiding the buffer's
internal reallocations) was considered and rejected for v0. `Vec`'s growth
is already amortized (roughly doubling), so the total bytes ever copied
across every regrow is bounded by about twice the final size — for
anything capped at 8192 bytes, that's at most ~16KB of `memmove`, once,
per document; not a measured or plausible bottleneck next to the disk
write that follows it. A size-computing pre-pass would have to mirror
`write_document`'s entire recursive shape without writing anything — real
duplicated logic that has to stay in sync with the encoder forever, and an
`Vec::with_capacity` under-guess wouldn't even fail loudly (it's a hint,
not a hard limit, so it just silently falls back to normal reallocation),
making the two versions drifting apart a plausible, quiet bug. Worth
revisiting only if profiling ever shows this encoding path actually
dominating insert time in practice.

### 11.4 `Data` page management (`data.rs`): no chaining, one document per page
*Superseded by §20: documents are packed several per page now, updates
can move a document, and a page is freed only once it's empty.*
Real and tested: `insert_record`/`get_record`/`update_record`/
`delete_record`. Unlike `Catalog`/`IndexLeaf`, `Data` pages are never
chained — each document gets its own freshly-allocated page (the v0
simplification already recorded in §9: no `current_data_page`, revisit
only if this proves wasteful). One consequence worth naming: every live
`RecordLocation` this module hands out has `slot == 0`, since it's always
the first and only cell ever inserted into that page — asserted at the top
of `update_record`/`delete_record` rather than left implicit.

No chaining also means no page-walking loop, so this module is just four
plain functions, not a struct — unlike `Catalog`/`BTreeIndex`, there's no
root page or cache to hold between calls, so a wrapper type would hold
nothing.

`update_record` rebuilds the target page from scratch (`SlottedPage` has
no "replace a cell in place" operation, only append and tombstone) and
writes it back to the *same* page id — `loc.page` never changes on update,
so nothing that references the location (the index) needs touching for a
same-collection update. `delete_record` frees the whole page via
`store.free_page`, not just the one cell in it, since a page is never
shared between documents — freeing the page *is* freeing the document.

`insert_record`/`update_record` return a real `Err` (not `.expect()`) when
a document doesn't fit in one page — unlike every other `.expect()` in
this codebase, which is only ever safe because a `has_room_for` check
immediately before it guards an invariant the code itself controls, here
the document's size comes from the caller, so "too large" is a reachable,
legitimate outcome, not a bug. `insert_record` checks this against a
throwaway empty page *before* calling `allocate_page`, so a
too-large document never wastes (or has to roll back) a real allocation.

## 12. Wiring `Collection<Document>` (`collection.rs`)

Real and tested: `insert`/`get`/`update`/`delete`/`find` all now do real
work for `Collection<Document>` — the untyped path. Every method follows
the same shape: look up the collection's `CollectionMeta` via a shared
`meta` helper (creating it, via `Catalog::create_collection`, only on
`insert`'s first call), build a `BTreeIndex` from `meta.index_root` (free
— it's one `PageId`, nothing to load), then combine the index and `data.rs`:
`insert` writes the record then indexes it; `get`/`find` look the location
up (or scan) then read it; `update` rewrites the data page, and
re-points the index only if the document had to move (§20.3); `delete`
removes the data cell *and* the index entry — both, since either alone
would leave the other side pointing at nothing.

Two additions to `Database` were needed to make this possible:
`catalog`'s field visibility widened from private to `pub(crate)` (same
reasoning as `store`'s — `Collection` needs direct access from its method
bodies), and a new `pub(crate) id_gen: UuidV7Generator` field — plain, no
`RefCell`, since generating an id never mutates the generator's own state.
(Since §27 these live in `State` behind the lock, `id_gen` outside it.)

### 12.1 A second `impl` block, not a filled-in generic one
At the time this was built, `Collection<T>`'s existing generic methods
(bounded on `T: Serialize + DeserializeOwned`) were still `todo!()` — they
needed the serde bridge (§13) to convert an arbitrary `T` to/from
`Document`, which didn't exist yet. `Document` needs no such conversion;
it already *is* the storage representation. So the real implementation
went into a second, concrete `impl<'db> Collection<'db, Document>` block
instead of the generic one. Rust allowed both blocks to coexist without
ambiguity, since `Document` didn't (at that point) derive
`Serialize`/`DeserializeOwned`, so the generic bound never applied to it —
the two blocks never competed for the same call. This turned out to be
more than a stopgap: once the serde bridge did land, `Collection<T>`'s
generic methods (§13.3) were wired as a thin layer that converts and then
*calls this same concrete impl* rather than duplicating its logic — so
this "second impl block" is the permanent shape, not a temporary one.

## 13. The serde bridge (`serde_bridge.rs`)

Real and tested: `to_document`/`from_document` convert any `T: Serialize +
DeserializeOwned` to/from `Document`, using the same technique
`serde_json` uses for `serde_json::Value` and `bson` uses for
`bson::Bson` — implement serde's `Serializer` trait so its *output* is
`Document` instead of bytes/text, and implement `Deserializer` *for*
`Document` itself so it can feed derive-generated code directly. This
closes out the one item §4.1 originally flagged as "real, nontrivial
work, deliberately not rushed."

### 13.1 Enum representation: externally tagged, matching `serde_json`'s default
A unit variant serializes as a bare string (`Shape::Point` →
`Document::String("Point")`); every other variant kind serializes as a
single-entry object (`Shape::Circle(2.0)` → `{"Circle": 2.0}`,
`Shape::Rect{w,h}` → `{"Rect": {"w":..,"h":..}}`). Not invented for this
project — it's the same "externally tagged" default `serde_json` and most
other value-type bridges use, chosen deliberately so the behavior is
already familiar rather than a new convention to learn.

### 13.2 Why `deserialize_any` does most of the work
`Document` is *self-describing* — unlike a byte-stream `Deserializer`,
where `deserialize_i64` means "read exactly 8 bytes here," `Document`
already knows what it is regardless of which method serde calls. So
`deserialize_any` is the one method that actually inspects `self` and
calls the matching `visitor.visit_*`; nearly every other required method
(`deserialize_bool`, `deserialize_struct`, `deserialize_seq`, ...) just
forwards to it via `serde::forward_to_deserialize_any!`. Only
`deserialize_option` (`Null` → none, else some) and `deserialize_enum`
(reconstructing the bare-string/single-entry-object shape from §13.1)
needed real logic.

### 13.3 Wiring `Collection<T>`: convert, then delegate
`Collection<T>`'s methods (`insert`/`get`/`update`/`delete`/`find`) are
thin: convert via `to_document`/`from_document`, then delegate to a
freshly-built `Collection<'db, Document>` (same `db`, a cloned `name` —
cheap, consistent with `Collection` being a handle constructed freely
rather than held onto) for the actual catalog/index/data-page work. The
storage logic exists in exactly one place (§12) regardless of whether the
caller used the typed or untyped path.

### 13.4 A real gotcha: `IntoDeserializer` isn't automatic
Expected serde to provide a blanket `impl<T: Deserializer> IntoDeserializer
for T`, needed so `serde::de::value::SeqDeserializer`/`MapDeserializer`
(serde's built-in "visit this iterator as a seq/map" helpers, used to
implement `Array`/`Object` deserialization without hand-writing
`SeqAccess`/`MapAccess`) could wrap iterators of `Document`. That blanket
impl doesn't exist — every value-type bridge, `serde_json::Value`
included, writes the one-line `impl IntoDeserializer for Document { fn
into_deserializer(self) -> Self { self } }` itself. First compile attempt
failed on exactly this.

### 13.5 `DocId` has no special wire representation
A stored `Document::Id` decodes as its plain string form (`id.to_string()`)
when flowing into an arbitrary `T` field — there's no dedicated "this was
an id" signal preserved through the bridge (that would need a sentinel
newtype-name technique, e.g. what `serde_bytes` does for `Vec<u8>` vs.
`serde_bytes::Bytes` — real, but disproportionate complexity for v0).
`DocId` itself also has no hand-written `Serialize`/`Deserialize` impl
yet, so a `T` struct can't yet have a field of type `DocId` directly.

Partially revisited (§18): the untyped path now merges `_id` into every
stored `Object` document, so `Collection<Document>` no longer has this
gap. The typed path still does — a `T` struct still can't declare its own
`id: DocId` field and have it populated, since that needs both the
`Serialize`/`Deserialize` impl mentioned above and a decision about which
field name/convention `insert` would populate. Worth revisiting only if a
real struct actually wants an embedded id field — less likely since
`find_with_ids` (§23) hands ids out alongside `T`.

## 14. Query: sort and limit (`query.rs`)

Real and tested: `Filter` gained `sort: Option<Sort>` and `limit:
Option<usize>`, and a new `Filter::apply(docs)` method — the single place
query semantics now live. `Collection::find` no longer filters
documents itself; it just gathers every candidate (via `index.scan()` +
`data::get_record`) and hands the whole set to `apply`, which filters,
then sorts, then limits, in that order (matching SQL's `WHERE` -> `ORDER
BY` -> `LIMIT` — sorting an already-filtered set is both correct and
cheaper than sorting everything first). This was the concrete gap
the time-series workload (§5.1) was expected to surface: its query
patterns need "most recent row" (`ORDER BY tst DESC LIMIT 1`), which
`Filter` couldn't express before this.

A document missing the sort field, or one whose field type doesn't
compare against the other side's, doesn't make `apply` error — it
compares as `Equal`, so it just doesn't move relative to whatever it's
being compared against (`sort_by`'s stability preserves the rest of the
original order). Matches the project's general stance of trusting the
caller rather than inventing validation for a case a schema-less document
store can't really define as "wrong" anyway. (Since §34 there's a fixed
order across kinds instead: null and missing first, values nothing
orders last.)

Since §23, `apply` delegates to `apply_to(items, doc_of)`, the same
pipeline for items that *carry* a document — `find_with_ids` runs it
over `(DocId, Document)` pairs.

## 15. Synthetic time-series data (`tests/timeseries_shape.rs`)

The validation step the old "Open work" item 1 called for: a new
integration test (`tests/`, not `src/`) that only uses trunkdb's public
API — `Database`, `Collection<T>`, `query::Filter` — the way an actual
application would, rather than reaching into crate internals.
`LocationPing` and `Geofence` mirror the time-series workload's data
(§5.1); `Collection<LocationPing>` and
`Collection<Geofence>` are real, independent, typed collections in the
same database file.

It exercises exactly the query patterns §5.1 identified as real and not
yet provable before `Filter::apply` existed (§14):
- a time-range filter (`tst >= ? AND tst <= ?`) combined with an ascending
  sort,
- "most recent row" (`ORDER BY tst DESC LIMIT 1`),
- several optional filters built up dynamically and ANDed together (the
  shape a real query endpoint uses when not every filter param is always
  supplied).

Deliberately not exercised, because §5.1 already scoped them out of v0
rather than leaving them as an oversight: the `zone_key` unique
constraint (v0 has no constraint system at all) and the `LEFT JOIN` +
`GROUP BY` + `COUNT` aggregate (that stays application-level code on top
of `find`, not something `Filter` should grow support for). All 4 new
tests pass against the real on-disk storage stack, no mocking.

## 16. Durability: a real write-ahead log (`durability/wal.rs`, `txn.rs`)

*History: this op-level WAL was replaced by a page-image WAL in §19,
after it turned out replay couldn't repair a structure a crash had left
half-written. Kept for the reasoning; §16.1–§16.4 describe code that no
longer exists in this form.*

Real and tested: `WalDurability` replaces `NoopDurability` as what
`Database` actually uses. Every `Collection<Document>` write (`insert`/
`update`/`delete` — `get`/`find` need none of this) now goes through
`Collection::write`'s four-step protocol: **log** the op (a `WriteOp`) to
a separate append-only file (`<path>.wal`), `fsync`ed; **apply** it for
real (the same `data.rs`/`BTreeIndex` calls as before); **sync** the main
file (one `fsync` after all of the operation's page writes, not one per
write — a single `fsync` flushes every dirty page for that file
regardless of how many separate writes produced them); **checkpoint**
(truncate the WAL to empty, now that the op is durably reflected in the
main file). `Database::open` reads back whatever a prior run's crash left
pending and replays it — via `apply_write_op` (§16.2), the same function
`Collection::write` itself calls — before returning a usable `Database`.

### 16.1 Why replay has to be idempotent
A crash can land anywhere in the four-step protocol, including *after*
the main file was fully synced but *before* `checkpoint` finished
truncating the WAL — so recovery may replay an op that's already durably
applied. `apply_write_op` is written to tolerate that: `Insert` skips if
the id already exists, `Update`/`Delete` skip if it's already gone —
either way a no-op, never a duplicate or an error. Two tests
(`database.rs`) inject a WAL record directly, bypassing `Collection`, to
exercise both crash points: one where the op was only ever logged, one
where it was already applied — confirming recovery produces the same end
state either way.

### 16.2 One apply function, shared by the live path and recovery
`apply_write_op(catalog, store, collection, op)` (`collection.rs`) is the
single place a `WriteOp` actually gets applied to data/index pages — used
by both `Collection::write` and `Database::open`'s recovery loop, so the
two can't drift apart. `get_or_create_meta` (the former `Collection::meta`
method) became a free function for the same reason: recovery has no
`Collection` instance to call a method on.

### 16.3 A real conflict with `decode_document`, and how it's resolved
§11.2 documents `decode_document` as trusting its input enough to panic
on a truncated buffer rather than erroring — reasonable everywhere else
in the crate, since nothing reads a cell's bytes except code that wrote
them. A WAL's tail is exactly the place truncation legitimately happens
(a crash mid-append), and handing that buffer to a function documented to
panic on truncation would crash `Database::open` itself — the opposite of
durability.

Fix: the WAL's own framing keeps `decode_document` from ever seeing a
possibly-torn buffer. Each record carries its own `u32` length prefix
(`durability/wal.rs`); recovery checks "are there `record_len` more bytes
here?" *before* slicing that record out and decoding it. A torn tail
(fewer bytes than promised) is caught at that check and silently
discarded. A record that *is* length-complete but still fails to decode
is a different, stronger signal — real corruption, not a crash artifact —
and errors hard instead. Tested directly: a test truncates a real WAL
file mid-record and confirms recovery drops it rather than erroring.

### 16.4 Smaller decisions
- `FileStore::sync` is a new inherent method (`file.sync_all()`), not
  added to the `PageStore` trait — §7's "one load-bearing seam... not
  meant to be faked" stance argues for leaving that trait alone;
  `Collection::write` calls `sync()` directly on the concrete `FileStore`
  it already holds.
- `Durability::log` gained a `collection: &str` parameter. `WriteOp`
  itself wasn't changed to carry it, since within one `Collection`'s
  method the collection name is already available from `self.name` — but
  recovery has no such context, so the WAL's on-disk record needs the
  name explicitly even though the in-memory `WriteOp` doesn't.
- `WriteOp`'s byte codec (`encode_write_op`/`decode_write_op`, `txn.rs`)
  reuses `document::encode_document`/`decode_document` for the payload,
  the same pattern `data.rs`'s record codec already used.

### 16.5 Deliberately still out of scope, at the time this was built
`TransactionManager`/`GlobalLockTxnManager` (§4.4, multi-op batch
atomicity) was still completely unwired when this milestone landed —
`Durability::log` took a slice because it was designed to compose with
batches later, but `Collection` only ever passed single-op slices.
Wired up next, §17. Concurrency (§4.6) remains untouched.

### 16.6 One WAL record per batch, not per op (a bug fix)
As first built (§16, carried into §17), `log` framed each op as its own
`[u32 len][op]` record. That was fine while every batch had one op, but
once §17 made multi-op batches real it broke their atomicity: a crash
*during* `log` could leave the first ops' records complete on disk and
the last one torn. Recovery would discard the torn record and replay the
rest — surfacing part of a batch whose `write_batch` never returned, and
which was never applied at all.

Fix: one record per batch — `[u32 body_len][u32 crc32(body)][body]`,
with `body` = `[u32 op_count]` + `op_count` × `[u32 op_len][op]`. The
record is either wholly intact (replay every op) or torn (replay none).
Recovery still hands `Database::open` a flat `Vec<WriteOp>`; batch
boundaries don't need to survive past decoding, since §16.1's per-op
idempotency makes replaying whole batches in log order correct.

The CRC closes a second gap the length prefix alone couldn't: a record
whose length is complete but whose bytes didn't all reach the disk. A
bad CRC on the *last* record is treated as a torn tail (ignored); a bad
CRC with more records after it can't be a crash artifact, so it's a
hard `InvalidData` error. CRC-32 is implemented inline (bitwise, ~10
lines) rather than pulled in as a crate — it runs once per batch.

Tested at both levels: `a_batch_torn_between_its_ops_recovers_none_of_them`
(`wal.rs`) and `a_batch_torn_while_being_logged_is_not_partially_recovered`
(`database.rs`, fails against the old framing). The framing and CRC
carried over into §19's page-image records unchanged.

## 17. Batch writes: wiring `TransactionManager` (`database.rs`, `txn.rs`)

Real and tested: `Database::write_batch(ops: Vec<WriteOp>)` is the first
real, public entry point for `TransactionManager`/`GlobalLockTxnManager`
— both existed as dead code (never called by anything) before this. Ops
may target different collections in one call, which is the actual point:
a multi-entity atomic update (§4.4's requirement) needs
exactly that, and a sequence of separate `Collection::insert`/`update`/
`delete` calls can't give it, since each of those is its own
independently-durable unit.

`Collection<Document>`'s own `insert`/`update`/`delete` now delegate to
`write_batch(vec![op])` — collapsing what would otherwise be two
near-identical copies of the log/apply/sync/checkpoint protocol (§16)
into one. A side effect: single-document writes now also go through
`GlobalLockTxnManager`'s lock, which they didn't before.

### 17.1 The prerequisite: giving `WriteOp` its own collection name
`Durability::log`'s signature had grown a bolted-on `collection: &str`
parameter during §16, since at the time every `log` call came from one
`Collection` instance operating on one collection. That doesn't work for
a batch spanning several collections. Fix: move the collection name into
`WriteOp` itself (`Insert(String, DocId, Document)`, etc.) — which let
`Durability::log` drop the parameter and go back to its original,
cleaner `log(&mut self, ops: &[WriteOp])` shape, and let
`durability/wal.rs`'s record encoding drop a whole layer of
now-redundant name-handling (each op already carries it).

### 17.2 Partial batch application: recovery replays the whole batch, not just what's missing
*Superseded by §19: recovery now restores page images, not ops.*
A batch is logged as one WAL unit but still applied op-by-op, so a crash
partway through the apply loop can leave some ops durably on the main
file and others not, even though the whole batch is still sitting in the
(not yet checkpointed) WAL. No batch-specific recovery logic was needed
for this: replaying every op in the batch again and relying on §16.1's
per-op idempotency (insert skips if present, update/delete skip if
absent) produces the correct end state regardless of how far the crash
got. Tested directly (`recovers_a_batch_where_only_some_ops_were_already_applied`,
`database.rs`): manually applies only the first op of a two-op batch,
then confirms `Database::open` recovers both correctly.

### 17.3 No rollback — now finally exercisable, still unchanged
*Superseded by §19.5: rollback is real now; the test described here
became `a_failed_batch_leaves_no_trace`.*
`GlobalLockTxnManager`'s doc comment documented "no rollback of
already-applied ops on partial failure" before any of this was wired up.
Tested directly now that there's a real way to trigger it
(`a_failed_batch_does_not_roll_back_ops_already_applied`): a batch where
op 1 succeeds and op 2 is an oversized document `data::insert_record`
rejects — `write_batch` returns an error, but op 1's effect is still
visible afterward. Real rollback needs staged writes in the storage
layer; still not built.

## 18. Merging `_id` into the untyped path (`collection.rs`)

Real and tested: a document read back from `Collection<Document>` now
reports its own `_id`, LiteDB/Mongo convention — not a bug fix, a
deliberate v0 gap (§13.5) closed on request once it was actually hit in
practice. `apply_write_op`'s `Insert`/`Update` branches run the document
through a new `with_id(doc, id)` before storing: if it's an `Object`, it
merges (overwriting any existing one) an `"_id"` key set to
`Document::Id(id)` — the real id `insert`/`update` were actually called
with, never whatever a caller may have already put there. Non-`Object`
documents (a bare `Document::Int`, `String`, ...) pass through unchanged
— there's no field to attach an id to.

Enforced in `apply_write_op`, not in `get`/`find`/`insert`/`update`
directly, for the same reason that function exists at all (§16.2): it's
the one path both live writes and crash-recovery replay share, so the
merge happens exactly once and the *stored bytes* are canonical from the
moment of creation. `get`/`find` needed no changes — they just return
whatever's on disk, which now already has the right `_id`.

The typed path (`Collection<T>`) is unaffected on purpose: `from_document`
deserializes permissively (unknown map keys ignored, same as
`serde_json`'s default), so the extra `_id` key now present in the
underlying `Document::Object` is silently dropped for a `T` that has no
matching field. Giving a typed struct its own populated id field is a
separate, bigger question (§13.5) — not part of this.

## 19. Durability, take two: a page-image WAL (`storage/file.rs`, `durability/wal.rs`, `database.rs`)

Real and tested: the WAL logs the *pages* a batch changed, not the ops
that changed them. It replaces §16's op-level log, closes a real
crash-safety hole in it, and makes rollback real (closing §17.3).

### 19.1 Why the op-level WAL wasn't enough
§16's WAL logged ops ("insert id X") and relied on replaying them to
repair a crash. Replay only works if the structures it runs against are
intact — and they may not be: one op writes several pages (data page,
index leaf, header; for a B-tree split also the new sibling and the
parent), and `FileStore` handed each `write_page` to the OS immediately.
A crash between those writes left a structurally broken file that replay
couldn't fix: replaying "insert X" knows nothing about X's neighbors.

Found while reviewing the B-tree code, and demonstrated with a throwaway
test: 1088 committed `_id`s, then a crash after the first of a leaf
split's page writes — 136 previously committed entries vanished from
both `lookup` and `scan`, permanently (the left half pointed at a
never-written right sibling, and the parent never learned of the split).
The same window existed for data page + index, free-list allocation, and
the catalog bootstrap.

The fix is what LiteDB's WAL, SQLite's rollback journal and Postgres'
full-page writes all do in some form: log page images. Every layer above
`PageStore` then gets crash safety without being crash-aware — no
split-specific (or any structure-specific) recovery logic exists or is
needed.

### 19.2 Staging lives in `FileStore`
`begin()` snapshots the header and starts a dirty set (`BTreeMap<PageId,
page>`). While staging, every page change lands there instead of the
file — `write_page`, `free_page`'s free-list link, and the header updates
`allocate_page`/`free_page` make (the header is simply page 0 in the
set). Reads check the set first, so a batch sees its own writes.
`rollback()` restores the header snapshot and drops the set;
`dirty_pages()` lists it in ascending id order; `write_back()` writes it
out, `fsync`s, and ends staging.

A wrapper `PageStore` was considered and rejected: the header is managed
inside `FileStore`, bypassing `write_page`, so a wrapper would have had
to duplicate the allocation logic to see it. The `PageStore` trait is
unchanged; nothing above it knows staging exists.

### 19.3 The commit protocol (`Database::write_batch`)
1. **stage** — `store.begin()`.
2. **apply** each op (`apply_write_op`). On error: `rollback()`, restore
   the catalog snapshot (§19.5), return the error.
3. **log** — every dirty page, as one WAL record, `fsync`.
4. **write back** — the same pages to the main file, `fsync`.
5. **checkpoint** — truncate the WAL.

A crash before step 3 completes leaves the pre-batch state (the file was
never touched); a crash after it leaves a complete WAL record that
recovery writes back, giving the post-batch state. Never anything in
between. A batch that dirties no page just ends staging (defensive:
since §22.2 every op either changes a page or fails the batch).

The WAL record keeps §16.6's framing — one record per batch, `[u32
body_len][u32 crc32][body]`, torn or bad-CRC tail ignored, bad CRC
mid-file an error — with a new body: `[u32 page_count]` + `page_count` ×
`[u64 page id][8192 page bytes]`. The WAL file now starts with a header
(`TRUNKWAL` + `u32` version), written together with the first record
after each checkpoint, so a leftover op-level WAL (or any other file) is
a clear `InvalidData` error instead of a misread. `encode_write_op`/
`decode_write_op` are gone — only the WAL used them; `WriteOp` stays as
`write_batch`'s API type.

### 19.4 Recovery
`Database::open` opens the WAL *first*, then the store; pending page
images are written straight to the file (`FileStore::restore_pages`, in
log order, so a page logged twice ends at its latest image), the header
is re-read from disk (its own image may have been among them), and the
file is `fsync`ed — all before `Catalog::load` reads anything. Rewriting
a page image is idempotent by nature, so recovering a batch that had
already fully reached the main file is harmless; §16.1's per-op
idempotency argument is no longer load-bearing (`apply_write_op` kept
its skip-if-present/absent checks as API behavior until §22.2 turned
them into errors).

`open` then checkpoints *unconditionally*. Found while building this, a
bug the op-level WAL had too: after a crash mid-`log`, recovery found
nothing complete to restore and left the torn bytes in place; the next
batch was appended after them, and the open after *that* hit a bad CRC
mid-file — a hard error. Tested
(`a_torn_wal_tail_does_not_break_later_batches`).

### 19.5 Rollback, finally real
Since no page reaches the file before step 3, undoing a failed batch is
just `rollback()` — no undo log needed. One subtlety: `Catalog` caches its
collections in a `HashMap`, and `get_or_create_meta` can create a
collection mid-batch. Rolling back the pages without the cache would
leave it pointing at a never-written index root. So `Catalog` is `Clone`,
and `write_batch` snapshots it before applying and restores it on
rollback. Tested (`a_failed_batch_leaves_no_trace`): op 1 creates a new
collection, op 2 fails; afterwards neither the document nor the
collection exists, in the file or the cache, and the database keeps
working.

### 19.6 Poisoning after a failed write-back
Once the WAL record is durable, the batch is committed — but if writing
it back fails in-process (e.g. a full disk), the main file may be
half-written while the process keeps running. `write_back` keeps the
dirty set on error, so `write_batch` retries once from it (the same
images the WAL holds). If that fails too, the `Database` poisons itself:
every call — reads included — returns `Error::Poisoned` until it's
reopened, and reopening restores the batch from the WAL. The same idea
as `Mutex` poisoning, or SQLite going read-only after I/O errors.

A failed `log` is handled too: it may have left a complete record behind
(the write landed, the `fsync` failed), which the next `open` would
restore for a batch this call reported as failed. So `write_batch`
rolls back and truncates the WAL; only if *that* fails is the batch's
fate unknown, and the database poisons itself. A failed *checkpoint*
after a successful write-back is deliberately not an error: the batch is
complete in the file, and a leftover record is harmlessly written back
again at the next `open`.

Tested with test-only fault injection in `FileStore`
(`failing_write_backs`/`write_back_fails_after`: fail after N pages),
which leaves the file genuinely half-written.

### 19.7 The fresh-file bootstrap goes through the WAL too
`Catalog::load` on a fresh file allocates and writes the first catalog
page — before this, two unlogged writes with a crash window between
them ("header, but no catalog page" would fail every later open).
`Database::open` now runs it staged and commits it like any batch. That
needed one change in `FileStore::open`: it no longer writes a fresh
file's header eagerly — the header reaches the file with the bootstrap
batch, so a crash before the log leaves an empty file, still fresh next
time. A file shorter than one page is also treated as fresh: that can
only be a first write-back cut short, whose batch is in the WAL — if it
starts with the magic; since §22.1 anything else is rejected.
Tested at every cut point of the bootstrap.

### 19.8 The regression test for the original bug
`a_crash_mid_b_tree_split_loses_nothing` (`database.rs`) reproduces
§19.1's numbers exactly — 1088 committed entries, then an insert that
splits a leaf and changes 5 pages (6 since §20: `Int` data cells and
leaf entries are both 30 bytes with their slot, so the leaf and the data
page fill up on the same insert) — and cuts its write-back after every
possible page count. Without recovery (reading the file raw), 3 of
the 4 cuts damage the tree; with recovery, all 1088 documents plus the
new one survive every cut. The test asserts both halves, so it fails if
a later change makes the scenario silently stop reproducing the damage.

### 19.9 Cost and limits
Each dirty page is written twice (WAL, then main file): a single insert
is 2 pages (data page, leaf) ≈ 16 KB plus the `fsync`s — 3–4 when it
opens a new data page (plus the header and the catalog page). A page rewritten many times
within one batch is logged once (the dirty set is keyed by id), so large
batches amortize well. A batch's dirty pages live in memory until commit
— a very large batch costs RAM; documented, not solved. Still not
covered: bugs in the tree logic itself (a wrong tree is written
atomically and durably — tests catch that, a WAL can't), bit rot in the
main file (needs per-page checksums), and disks that lie about `fsync`.

## 20. Several documents per data page (`data.rs`, `storage/slotted.rs`, `catalog.rs`)

Real and tested: `Data` pages hold as many documents as fit, replacing
§11.4's one-document-per-page layout. 1000 small documents (a few
fields each) now take a 20-page file in total, index included, instead
of over 1000 pages (`small_documents_are_packed_into_few_pages`); on
the sync workload's data (§5.3, ~900 B per document) that's about eight
documents per page instead of one.

### 20.1 Where inserts go: a current data page per collection
`CollectionMeta` gained `current_data_page` (`0` = none yet) — the page a
collection's next insert tries first. If the document fits there (after
compacting the page, if needed), it goes there; otherwise a fresh page
is allocated and becomes current. The catalog cell grew to `[u64
index_root][u64 current_data_page][name]`; `Catalog::set_current_data_page`
rewrites it in place (same length) — once per filled page, not per
insert.

`data.rs` stays catalog-agnostic: `insert_record`/`update_record` take
the current page as `&mut PageId` and update it when they allocate;
`apply_write_op` persists a change through the catalog. The catalog
cache changing mid-batch is covered by §19.5's snapshot/restore already.

Considered and rejected:
- **In memory only** (forget the current page at `open`): simpler, no
  catalog change, but every open would abandon a partly filled page — for
  an app that opens the file once per run, a steady leak.
- **A free-space map** (which pages have how much room, like Postgres'
  FSM): would also reuse the holes in older pages, but it's a new
  on-disk structure to keep consistent. Not needed until deletes are
  common; see §20.4.

One collection per page, never mixed: a scan of one collection then
touches only its own pages, and dropping a collection (§37.2) frees
whole pages.

### 20.2 `SlottedPage`: compaction, slot reuse, in-place update
Three new operations, all keeping a cell's slot number — the reason
`RecordLocation` is `{ page, slot }` (§7.3) finally pays off:
- `compact` repacks the live cells against the page end, so the dead
  bytes of deleted or shrunk cells become free space; tombstoned slots
  at the end of the directory are dropped (nothing references them).
- `insert_cell_reusing_slot` fills a tombstoned slot before growing the
  directory, compacting first if the space exists but is fragmented.
  Only for `Data` pages: B-tree pages rebuild themselves in key order
  and rely on slot order, which reuse would break — so plain
  `insert_cell` is unchanged.
- `update_cell` replaces a cell's bytes: in place if they didn't grow,
  otherwise by compacting around them; `false` (page untouched) if they
  don't fit even then.

A bug the unit tests caught: `update_cell` and `insert_cell_reusing_slot`
compact while their target slot is a tombstone, and `compact` trimmed it
as a trailing tombstone — the cell was then written to a slot beyond
`slot_count`, invisible. The internal `compact_keeping(min_slots)` keeps
it.

### 20.3 Updates can move a document
§11.4's "`loc.page` never changes on update" no longer holds: a document
that grows past what its page can hold, even compacted, is deleted from
its page and inserted like a new one (usually into the current page).
`update_record` returns the new location, and `apply_write_op` re-points
the index (`remove` + `insert`; the `Index` trait needs no new method).
All staged in one batch, so a crash can't separate the move from the
index change.

A forwarding pointer at the old location (Postgres' HOT chains, MySQL's
row migration) was considered: it spares the index update, but every
later read of the moved document pays an extra page read, and a moved
document can move again. With one index, updating it is cheap and
simpler.

### 20.4 Deletes: tombstone, free the page once empty
A delete tombstones the cell. A page whose last document goes is freed
(back to the free list, for any page type) — unless it's the current
page, which the next insert will use anyway. Known limitation: a partly
emptied page that isn't current only gets its space back through
updates of its own documents; inserts don't look there (no free-space
map, §20.1). Churn-heavy workloads can leave pages half empty until a
future vacuum (§39.2).

### 20.5 Room for overflow pages
Every data cell now starts with a flags byte: `[u8 flags][16-byte
DocId][document]`. `0` means the whole document is in the cell — the
only kind written at the time. `1` was reserved for overflow, since in use (§26): the
cell will hold `[u32 total length][u64 first Overflow page]` and as much
of the document as fits. Reading it today is an `InvalidData` error, not
a misread. The page type tag `Overflow = 6` is reserved alongside it, so
adding overflow pages needs no format migration. *Done in §26 — without
the "as much as fits" part (§26.3). It did need a format version bump
after all, for §26.1's wider lengths.*

A flags byte rather than an always-present 8-byte pointer field: one
byte per document instead of nine, and room for other per-record
variants later (e.g. compression).

### 20.6 A file-format change without migration
Data cells and catalog cells both changed, so files written by 0.1.0
can't be read — they either fail to decode or decode as garbage. No
real data exists yet (that's why this is in Block A); the format
version added right after (§21.2) makes such files a clear error, and
every later format change too.

### 20.7 Test changes
`a_crash_mid_b_tree_split_loses_nothing` (§19.8) found its splitting
insert by "the dirty set grew by more than one page", which packing
broke (a plain insert now usually adds no page, but a new data page adds
two). It now counts dirty `IndexLeaf` pages. It also has to restore the
catalog cache after its trial rollback, like `write_batch` does, since
trial inserts can move `current_data_page`.

## 21. An exclusive file lock and a format version (`storage/file.rs`, `database.rs`)

Real and tested: a database file can only be open once at a time, and
its header says which on-disk format it's in. Both turn what used to be
silent corruption or a misread into a clear error.

### 21.1 The lock
`FileStore::open` takes an exclusive lock on the file with std's
`File::try_lock` (stable since Rust 1.89, hence `rust-version = "1.89"`
in `Cargo.toml`) and holds it until the `FileStore` is dropped. A second
open fails right away with `ErrorKind::WouldBlock` — from another
process, and also from a second handle in the same process: each
`Database` caches its own header and catalog, so two of them writing
one file corrupts it either way.

It's the OS's advisory lock (`flock` on Unix, `LockFileEx` on Windows):
it stops other trunkdb opens, not arbitrary programs writing the file,
and the OS drops it when the process dies — no stale lock file to clean
up after a crash, unlike a `.lock` sidecar file with a PID in it.
Known limits: advisory locks are unreliable on some network file
systems, and a platform without file locking fails the open (with
`Unsupported`) rather than silently skipping the lock.

Non-blocking on purpose: waiting for another process that holds a
database open for its whole lifetime would just hang the caller.

`Database::open` now opens the store *before* the WAL, so a refused
second open never reads the WAL either — the running instance may be
halfway through writing it. (The old order, WAL first, was already safe
from truncating that WAL, since the checkpoint only runs after the store
opened; this just makes "nothing happens before the lock" hold without
that argument.) Tested: `a_database_in_use_cannot_be_opened_again` logs
a record through the first instance, then checks the refused second
open left the WAL untouched.

The staging tests in `file.rs` inspect the file through a second
`FileStore` while the first is still open; they use a test-only
`open_with_lock(path, false)`.

### 21.2 The format version
The header gained `[28..32) format_version: u32`, first `1` — the
format of §20 (packed data pages, flags byte, catalog cells with
`current_data_page`), `2` since §26 (`u32` document lengths,
overflow pages), `3` since §28 (catalog cells with a kind byte,
index entries), `4` since §32 (indexes hold null and missing
fields), and `5` since §33 (unique indexes; format 4 is still read as
it is, §33.4). `Header::decode` checks it right after the magic,
before anything else in the header (another version may lay it out
differently), and only its own version is accepted. The error says
which case it is:
- `0` — the bytes every header had before the field existed: trunkdb
  0.1.0, or a development build between 0.1.0 and this change;
- higher than this build's — open it with a newer trunkdb;
- lower — an older format: export it with the version that wrote it,
  import it with this one (§30; the message said "no migration" until
  §32).

The magic stays `TRUNKDB1`: it answers "is this a trunkdb file at all",
the version answers "which layout". The version is bumped whenever a
page or cell layout changes; a file moves from one version to the next
by export and import (§30). The WAL keeps its own version (§19.3) — its
record framing is independent of the page layout inside the images it
carries.

## 22. Block A addendum: three data-risk fixes (`storage/file.rs`, `collection.rs`, `catalog.rs`, `txn/`)

After Block A, a review of the code for anything that could damage or
silently lose data turned up three problems — each confirmed with a
throwaway test before fixing, each now covered by a real one.

### 22.1 `open` no longer overwrites small foreign files
`FileStore::open` treated every file shorter than one page as fresh
(§19.7), so `Database::open("notes.txt")` on a 2 KB text file bootstrapped
a database *over it* — the one way trunkdb could destroy data that
wasn't its own. A non-empty short file is now fresh only if it starts
with the magic (or a prefix of it, for a cut inside the first 8 bytes):
a first write-back cut short always does, because the header page is
written first. Anything else is "not a trunkdb file (bad magic)",
left untouched. Since §21.1 opens the store before the WAL, no stray
`<path>.wal` is created either (`opening_a_foreign_file_changes_nothing`).

### 22.2 Duplicate and missing ids fail the batch
Inside `write_batch`, an `Insert` of an id that already existed, and an
`Update`/`Delete` of one that didn't (or of a collection that didn't),
were skipped — and the batch returned `Ok`. A leftover of op replay
(§16.1), which page-image recovery (§19.4) no longer needs; for a
caller, "saved" when nothing was saved. Block B's typed batch API (§24,
the sync workload's batch) would have inherited it.

Now `apply_write_op` returns `Error::DuplicateId { collection, id }` or
`Error::NotFound { collection, id }`, and the whole batch rolls back.
For those to be matchable, `TransactionManager::apply_batch` passes
the op's `crate::Error` through instead of flattening it into
`TxnError::Failed(String)` — which also keeps every other op error
(e.g. `InvalidInput` for a too-large document) intact. `TxnError`
remains for the manager's own failures (a poisoned lock).

The single-op API keeps its shape: `Collection::update`/`delete` still
return `Ok(false)` for a missing id — for one op that's an ordinary
answer, not an error. They now map `Error::NotFound` to `false` instead
of checking existence first, which saves an index lookup.

### 22.3 Collection names are limited to 255 bytes
A catalog cell must fit in one page. For a name that didn't fit even
an empty catalog page, `create_collection` chained new catalog pages
forever — staged, so the file was safe, but `insert` hung while memory
grew. Names over `MAX_COLLECTION_NAME_LEN` (255 UTF-8 bytes) are now an
`InvalidInput` error; 255 is generous for a name and keeps catalog
pages holding dozens of entries.

### 22.4 Reviewed, deliberately not changed
- **A panic mid-batch** (only a bug or a corrupt file can cause one)
  leaves `FileStore` staging: the file stays safe, but a caller that
  catches the panic would read uncommitted pages in-process. Poisoning
  on unwind would close it; not worth it before there's a known path.
  *Closed by §27.3, for free: the panic poisons the lock.*
- **The header is decoded before WAL recovery** (§19.4). It would only
  matter if a power loss tore the header page, and all of the header's
  fields sit in its first 512 bytes, which disks write atomically.
- **`u16` lengths** in the document encoding can't wrap today, since a
  whole document must fit in one page; overflow pages (§26) had to widen
  them, as already planned. *Done in §26.1.*
- **Bit rot** stays undetected until per-page checksums (§39.2).

## 23. `find_with_ids` (`collection.rs`, `query.rs`)

Real and tested: `Collection<T>::find_with_ids(filter) -> Vec<(DocId,
T)>`, and the same on `Collection<Document>`. It closes the sync
workload prototype's one real blocker (§5.3): a typed caller could get a
document's id only from `insert`, since a `T` has no field to carry it
(§13.5) — so after a restart there was no way to find a document by a
field and then `update`/`delete` it. Tested as exactly that
(`typed_find_with_ids_finds_updatable_ids_after_reopen`).

The ids come from the data cells, which store them anyway (§11) —
`data::get_record` already returned `(DocId, Document)`, and `find` used
to drop the id. To keep each id paired with its document through
filter, sort and limit, `Filter` gained `apply_to(items, doc_of)`: the
same pipeline as `apply`, for any item that carries a document; `apply`
now delegates to it. `find` is `find_with_ids` minus the ids, on both
paths, so there's one query path, not two.

On the untyped path, an `Object` document already carries its id as
`_id` (§18); `find_with_ids` also covers non-`Object` documents, and it
is what the typed version builds on.

Considered and rejected: a `Found<T> { id, doc }` struct instead of a
tuple — more self-describing, but a pair destructures directly (`for
(id, entry) in ...`) and matches the roadmap's signature; a struct can
come later if more per-result metadata appears.

## 24. Typed batches (`batch.rs`)

Real and tested: `Database::batch()` returns a `Batch`, which collects
typed inserts, updates and deletes against any of the database's
collections and `commit`s them as one `write_batch` — atomic and
durable, all or nothing (§19.3). Until now only the untyped
`write_batch(Vec<WriteOp>)` could do that, so a typed caller had to
build `Document`s by hand. The sync workload (§5.3) is the
target: one batch per run, find-then-update via `find_with_ids` (§23).

```rust
let entries = db.collection::<Entry>("entries");
let mut batch = db.batch();
let id = batch.insert(&entries, entry)?;   // id generated now
batch.update(&entries, &known_id, changed)?;
batch.delete(&entries, &gone_id);
batch.commit()?;
```

### 24.1 Ops take collection handles
Each op takes the `Collection<T>` it targets: `T` comes from the handle
(no turbofish, no mismatched types), and so does the name (no string
repeated per op). One batch can still span collections — the point of
batches since §4.4. A handle from a *different* `Database` would write
to a same-named collection of this one; `Batch` compares the handle's
database pointer and panics on a mismatch — a caller bug, not a runtime
condition. That needed `Collection::name()` (public) and a crate-internal
`db()` accessor.

Considered and rejected:
- **Collection names as strings** (`batch.insert::<Entry>("entries",
  x)`): no link between the name and `T`, so a typo or a wrong type only
  shows up as bad data.
- **A closure API** (`db.transaction(|tx| { ... })`): would allow reads
  inside a transaction later, but nothing reads mid-batch yet, and a
  builder is easier to fill from a loop and to inspect (`len`).

### 24.2 When errors surface
- **Adding an op** converts the value to a `Document` right away, so a
  value the serde bridge can't represent (e.g. `u64` above `i64::MAX`)
  fails there, and the batch is left as it was.
- **`commit`** reports what depends on stored state — `DuplicateId`,
  `NotFound` (§22.2), a too-large document — and rolls back everything.
  A missing id is an error here even for `update`/`delete`, where
  `Collection::update`/`delete` return `Ok(false)`: in a batch, a missing
  id means the batch's assumptions are wrong, and applying the rest
  would be a partial sync.

### 24.3 Ids, ordering, and what the batch doesn't see
`insert` generates the id when the op is added and returns it, so later
ops in the same batch can target it (insert, then update or delete — ops
apply in order, against staged pages). The id names a stored document
only once `commit` succeeds. Nothing is read before `commit`: reads in
the meantime (`get`, `find`) see the state before the batch — no
read-your-own-writes, which would need reads through the staged pages
and is out of scope until something needs it. A `Batch` is
`#[must_use]`; dropping one discards its ops.

### 24.4 Untyped callers
`Batch` covers `Collection<T>` for `T: Serialize`. `Document` doesn't
implement `Serialize` (§12.1), so `insert`/`update` don't accept a
`Collection<Document>` — untyped callers keep `write_batch(Vec<WriteOp>)`.
(`delete` needs no conversion and works with either.) A shared
conversion trait, implemented for every `T: Serialize` and for
`Document`, would lift that; not needed by anything yet.

## 25. `Op::Contains`: case-insensitive substring match (`query.rs`)

Real and tested: `Op::Contains` matches when the field is a string that
contains the condition's string value, ignoring case — what the sync
workload's search does with MongoDB's `{"$regex": ..., "$options": "i"}`
(§5.3). It composes with the other conditions (flat AND), sort and
limit like any `Op`.

### 25.1 Case-insensitive by definition, not by a flag
There is one `Contains`, and it ignores case — no case-sensitive variant
and no options field on `Condition`. Every known use (search fields) is
case-insensitive; LiteDB's string comparisons ignore case by default
too. A case-sensitive variant can be added as its own `Op` if something
needs it, without changing this one. The other ops (`Eq`, `Lt`, …) stay
case-sensitive, as before.

### 25.2 What "ignoring case" means
Both sides go through `fold_case`: Rust's Unicode `to_lowercase`, then
`ß` → `ss`. Lowercasing alone handles umlauts (`MÜLLER` ~ `müller`) but
leaves `ß` alone, so "Straße" wouldn't contain "STRASSE" — a likely
search in German text. Full Unicode case folding (the `CaseFolding.txt`
table) would cover the remaining rare cases, but needs a dependency;
not worth it yet. Folding isn't accent-stripping: `muller` doesn't match
`Müller`, which is what a user typing the umlaut expects.

### 25.3 Edge cases
- Anything but a string on either side (a number field, a numeric
  needle) doesn't match — no implicit conversion, consistent with how
  `compare` treats mismatched types.
- A missing field doesn't match, as for every other op.
- An empty needle matches every string field (standard `contains`
  semantics) — so an empty search box can be passed through unchanged,
  though skipping the condition is cheaper.

### 25.4 Deliberately not regex
A pattern language (regex, `LIKE` wildcards) is still open (§39.2). A
plain substring covers the search boxes, has no syntax to escape user
input for, and can't be made pathologically slow by a pattern.
Performance is a scan anyway (§4.3): each candidate's field is folded
per query — an allocation per document, negligible at the sync workload's size.

## 26. Overflow pages and `u32` lengths (`data.rs`, `document.rs`)

Real and tested: a document can be larger than a page — up to 4 GiB
encoded, in practice bounded by memory, since it's encoded, logged and
decoded as a whole. Documents with ~200 KB of
text (the large-document workload, §5.2) insert, update, turn up in a
`Contains` search and survive a reopen
(`documents_larger_than_a_page_work_end_to_end`). Documents that fit a page
are stored exactly as before; the sync workload's data never touches an overflow
page. Format version 2 (§21.2).

### 26.1 `u32` lengths in the document encoding
Every length and count in `encode_document` — `String`/`Binary` lengths,
`Array`/`Object` counts, object key lengths — went from `u16` to `u32`.
With documents capped at a page, `u16` could never wrap; past a page,
`as u16` would have truncated a 70 KB string's length silently and
misdecoded everything after it (§22.4 flagged this).

The `as u32` casts can't truncate: every inner length is at most the
encoding's total, and `data.rs` refuses a total past `u32::MAX` with
`InvalidInput` before writing anything — the one "too large" left. Keys
got `u32` too, for one rule instead of two; 64 KB keys are silly, but a
second width would mean a second error path in an encoder that has none.

Cost: two bytes per string, key, array and object — for the sync workload's
~900 B documents with ~20 fields, roughly 5–10 %.

Considered and rejected: **variable-length integers** (LEB128, as in
protobuf and SQLite): a length under 128 would take one byte, smaller
than even `u16`. But every length becomes a loop instead of a
`split_at(4)`, and "how long is this prefix" stops being a constant —
a real complexity cost in a format whose virtue so far is being readable
in a hex dump. The space it saves is small next to §20's packing; the
format version (§21.2) leaves the door open if measurements ever say
otherwise.

### 26.2 When a document overflows
Purely by size: a document stays inline if its cell (`[flags][id]
[document]`) fits on an empty data page — at most 8175 bytes — and
overflows otherwise. So:
- nothing changes for documents that fit, and there's no threshold to
  tune;
- the representation follows from the document alone, so an update
  switches it either way when the size crosses the line (inline → an
  overflow cell, and back, freeing the chain).

The cutoff sits at the edge, not lower as in SQLite (which overflows
cells past about a quarter page, to keep ≥ 4 cells per B-tree page):
trunkdb's data pages aren't B-tree nodes — nothing searches inside them,
so one large inline document on its own page costs nothing extra, while
overflowing it would waste the unused tail of its last overflow page.

### 26.3 The format
An overflow cell is `[u8 flags = 1][16-byte DocId][u32 length][u64 first
page]` — 29 bytes, packed onto the collection's data pages like any
other cell (§20.1). The encoded document lives in a chain of `Overflow`
pages:

```
[0]      page type tag (Overflow = 6)
[1..9)   next page id (0 = last)
[9..)    the next 8183 bytes of the document; on the last page only the
         remainder, the rest zeroed
```

No `SlottedPage` (one run of bytes needs no slot directory), and no
per-page length: the cell's total length says how much each page
holds — all of it, except on the last.

Rejected: keeping **"as much of the document as fits"** in the cell
itself, which §20.5 had sketched. It would save at most one page per
large document, but the cell would then fill its data page — no other
document could share it, and the cell size would depend on where it's
written. The pointer-only cell is fixed-size and always packs.

Also rejected: a **B-tree of blob pages** or extent allocation
(contiguous page runs, like SQL Server's LOB storage). Both are about
large-object random access and fragmentation; trunkdb reads and writes a
document as a whole, and a linked list is the simplest structure that
does that.

### 26.4 Writing, updating, deleting
- **Insert**: encode, write the chain (all pages allocated first, since
  each stores its successor), then place the 29-byte cell (§20.1).
- **Update**: free the old chain, write the new one, then update the cell
  — which, the cell being small, almost always stays in place. No
  in-place chain rewrite: the free list is LIFO, so the new chain gets
  the old pages back anyway, and one path is simpler than "reuse, then
  extend or trim".
- **Delete**: free the chain, then tombstone the cell (§20.4).

All of it is staged like any other page write (§19.2), so a batch that
fails rolls back the chain pages too; `a_failed_batch_leaves_no_trace`
now includes a large document and checks the file didn't grow.

### 26.5 Reading, and corrupt chains
`get_record` follows the chain, collecting the bytes, and decodes the
result. Scans (`find`) go through `get_record` for every document
already, so they need no change — and do read every chain, which is
fine for "search the prose" and wasteful for "filter on a small field
of a large document". Field-level lazy reading would need a different
encoding (offsets to fields); not before real use asks for it.

The walker trusts the cell's length, not the `next` pointers, to decide
where a chain ends, so corruption can't loop it forever: a chain that
ends early, one whose last page still links onwards, and a page in it
not tagged `Overflow` are all `InvalidData`. `delete`/`update` walk the
whole chain before freeing any of it, so a corrupt chain fails the batch
instead of freeing pages that belong to something else.

### 26.6 Cost
A large document is written twice like everything else (§19.9) — a
1 MB document is ~128 pages in the WAL and again in the main file — and
it is held in memory whole: as a `Document`, its encoding, and the
staged page images until commit. Fine for the large-document workload (tens to hundreds of KB);
a streaming blob API would be a different feature.

## 27. A thread-safe, cloneable `Database` handle (`database.rs`, `collection.rs`, `batch.rs`)

Real and tested: `Database` is a handle — `Clone`, `Send`, `Sync` —
and so are `Collection<T>` and `Batch`, which no longer borrow the
database. A handle can go into an app's shared state or a `static`, be
moved into a spawned thread, or be stored in a struct next to the
collections it hands out.

```rust
let db = Database::open("app.trunkdb")?;
let articles = db.collection::<Article>("articles"); // no 'db lifetime
std::thread::spawn(move || articles.insert(article)); // needs 'static: fine now
```

### 27.1 The shape: `Arc<Shared>`, one `RwLock<State>` inside
```rust
pub struct Database { inner: Arc<Shared> }            // #[derive(Clone)]
struct Shared { state: RwLock<State>, id_gen, txn }   // what clones share
pub(crate) struct State { store, catalog, durability, poisoned }
```
Cloning is an atomic reference-count increment (C#'s mental model: a
class reference, where every copy points at one object — `Arc` is what
makes that explicit, and thread-safe, in Rust). The file closes, and its
lock (§21.1) is released, when the last handle is dropped — including
the ones inside `Collection`s and `Batch`es. `Collection<T>` holds a
`Database` clone and its name; `Batch` holds a clone and its ops;
`Batch`'s "same database?" check became `Arc::ptr_eq`.

§8's design carries over unchanged, only the boundary type changed: the
three `RefCell`s and the `Cell<bool>` became one `RwLock` around all
the mutable state, and everything beneath it still receives `&mut dyn
PageStore` / `&mut Catalog` as plain parameters.

One lock rather than one per field (`store`, `catalog`, `durability`):
a batch needs all of them together, and reads need `store` and
`catalog` together — separate locks would only add a lock order to get
right, with no parallelism to gain.

`Collection<T>`'s marker became `PhantomData<fn() -> T>`: a plain
`PhantomData<T>` would make the handle `Send`/`Sync` only if `T` is,
though the handle never holds a `T`. `Clone` is implemented by hand for
the same reason — deriving it would require `T: Clone`.

### 27.2 `RwLock`, not `Mutex`
Reads (`get`, `find`, `find_with_ids`) take the lock shared and run in
parallel; `write_batch` takes it exclusively for the whole commit,
stage to checkpoint (§19.3). That is exactly what keeps readers from
seeing staged pages: while a batch is in flight, no reader holds the
lock, so a reader sees a batch entirely or not at all
(`readers_never_see_half_a_batch`). `FileStore` reads through `&self`
with positioned reads (`pread`), so parallel readers don't fight over a
file cursor.

A `Mutex` would have been equally correct and serialized readers too; it
costs nothing to take the better of the two, since reads already only
need `&` access. What this still isn't: readers during a write. A batch
blocks all readers until it has `fsync`ed twice — tens of milliseconds.
Truly concurrent readers need MVCC or a snapshot of the pre-batch pages
(§39.2).

`find` drops the lock before filtering and sorting: the candidates are
owned copies by then. No user code (serde conversion, filter closures)
ever runs under the lock, so a handle used inside one can't deadlock on
it — `std`'s `RwLock` isn't reentrant.

The `GlobalLockTxnManager`'s own `Mutex` is now always uncontended — the
write lock already serializes batches. It stays: it's the
`TransactionManager` implementation's own guarantee (§17), and a
different implementation behind that trait (e.g. MVCC) will need the
write lock to go away, not the trait's lock.

### 27.3 Poisoning, by lock or by flag
Two ways a database becomes unusable until reopened (`Error::Poisoned`),
both checked by the one `read()`/`write()` accessor every public entry
point goes through:
- the `poisoned` flag, for a batch that was logged but couldn't be
  written back (§19.6) — now a plain `bool` inside `State`;
- a **poisoned lock**: a thread that panics while holding the write lock
  (a bug, or a corrupt file hitting an `expect`) poisons it, and every
  handle reports `Error::Poisoned` from then on. This closes §22.4's
  "panic mid-batch leaves the store staging" for free: nobody reads
  those staged pages (`a_panic_mid_write_poisons_every_handle`). A panic
  under a *read* lock can't leave anything half-done, and `std` doesn't
  poison on it.

Recovery is what it was: reopen. With handles spread across threads
that now means dropping all of them first — the file lock is held until
the last one goes.

### 27.4 Considered and rejected
- **Keeping `Collection<'db, T>`** and making only `Database` `Sync`:
  `thread::scope` would work, `thread::spawn` and `AppState` storage
  wouldn't — they need `'static`. The borrow was the actual obstacle.
- **`Arc` inside, but `Collection` still borrowing**: the worst of both.
- **Asking callers to wrap it** (`Mutex<Database>`, §5.3's interim
  answer): works, but serializes reads, and every caller writes
  the same boilerplate.
- **A `parking_lot` `RwLock`** (no poisoning, fairer to writers): a new
  dependency, and poisoning is a feature here (§27.3). `std`'s lock may
  let a steady stream of readers delay a writer on some platforms —
  acceptable at trunkdb's scale.

### 27.5 API break
`Collection<'_, T>` in signatures becomes `Collection<T>`; `Batch<'_>`
becomes `Batch`. Nothing else in the public API changed. Tests use the
new freedom: a collection handle outliving the `Database` variable it
came from, writers on four spawned threads, a reader running against a
writer, and a compile-time `Send + Sync + 'static` check.

## 28. Secondary indexes (`index/`, `catalog.rs`, `collection.rs`, `query.rs`)

Real and tested: a collection can have B-tree indexes on top-level
fields, and `find` reads through one when a condition allows.

```rust
pings.ensure_index("tst")?;   // true: built now; false: it existed
pings.find(filter)?;          // tst >= a AND tst <= b reads just that range
pings.explain(&filter)?;      // QueryPlan::Index { field: "tst" }
pings.indexes()?;             // ["tst"]
pings.drop_index("tst")?;     // frees its pages
```

`ensure_index` builds the index from every existing document in one
atomic batch, and creates the collection if needed, so an app can
declare its indexes at startup. From then on every insert, update and
delete keeps each index current, inside the same batch as the document
change. Indexes persist. Nested fields (`a.b`) came later (§31); there's
still no index on several fields at once.

### 28.1 Keys are byte strings
The B-tree no longer knows what it indexes. Its keys are byte strings
compared byte by byte (`[u8]`'s `Ord`), and `index/key.rs` builds them:
- **primary index**: the 16-byte `DocId` — so its leaf and branch cells
  are byte for byte what they were (`[key][location]`, `[key][child]`,
  the key's length implied by the cell's);
- **secondary index**: the encoded field value, then the `DocId`. The id
  makes every key unique even when thousands of documents share a
  value, and makes removing one document's entry an exact-key delete.
  The index stores the document's location, like the primary index, so
  a lookup reads the data page directly, not through the primary index.

The value encoding is built so byte order agrees with `Filter`'s
comparisons (`query::compare`):
- **numbers**: `Int` and `Float` share one type, as they do in `compare`
  (an `Int` compared with a `Float` converts to `f64`): the `f64`'s bits,
  big-endian, sign bit flipped for positives and all bits for negatives,
  the standard trick for making bytes sort numerically. `-0.0` becomes
  `0.0`; `NaN` isn't indexed.
- **strings**: UTF-8 bytes with `0x00` escaped as `0x00 0xFF` and
  `0x00 0x00` as terminator. `compare` orders strings by bytes, and the
  terminator makes the encoding prefix-free: without it, `"ab"` + id
  could sort after `"abc"` + id.
- **bools**: one byte.
- A type tag comes first, so each type is one contiguous stretch of the
  index — a range never crosses into another type, matching `compare`,
  which never orders values of different types.

Everything else (`Null`, arrays, objects, binary, ids, missing fields)
has no key: no `Eq`/`Lt`/`Lte`/`Gt`/`Gte` condition can match it, so an
index without those documents answers those conditions correctly.
(Since §32, `Null` and missing fields do have a key: `== null` matches
them.)

Rejected: a **typed key** (compare decoded `Document`s in the tree).
Every comparison would decode, and the B-tree would depend on the
document model; byte keys keep it a plain ordered map, which the
`InMemoryIndex` fake now literally is (a `BTreeMap<Vec<u8>, _>`), and
the tree tests check the real one against it.

### 28.2 Splitting by bytes, and why each half still fits
§10.4's guarantee rested on fixed-size entries: `n + 1` entries that
overflow a page holding `n` split into two halves of about `n / 2`,
each of which fits. With keys from 0 to 1024 bytes, "half the entries"
can be most of the bytes, so leaves and branches now split at the
**byte** midpoint: the first entry at which the running total reaches
half. The guarantee comes back through a size cap:

Let `C` be a page's room for cells and slots (8179 bytes) and `m` the
largest entry. Keys are capped at `MAX_KEY_LEN = 1024` bytes, so
`m ≤ 1038 < C / 4`. An overflowing page holds `T` bytes with
`C < T ≤ C + m`. Splitting where the running total first reaches `T / 2`:
- left `< T / 2 + m ≤ (C + m) / 2 + m < 5C/8 + C/4 < C`;
- right `≤ T / 2 ≤ (C + m) / 2 < 5C/8 < C`.

Both halves also keep at least two entries, since two entries can't
reach `T / 2 > C / 2`. That matters for a branch split, which promotes
one entry and still needs one on each side.

The cap costs nothing in practice: string values are **cut** to fit,
about 1000 bytes (§28.3), so no document is ever rejected for an
overlong indexed value.

### 28.3 Candidates are always rechecked
Several different values can share a key prefix:
- `Int`s past 2^53 round to the same `f64`;
- strings that agree for their first ~1000 bytes are cut to the same
  key.

So an index range is a **superset**: `find` reads the documents in it
and applies the whole filter to them, exactly as it does for a full
scan. The ranges are built for that (`key::range_for`):
- `Eq v`: every key starting with `v`'s encoding;
- `Gt`/`Gte v`: from `v`'s encoding to the end of its type;
- `Lt`/`Lte v`: from the start of its type through `v`'s encoding.

Bounds include `v`'s own encoding even for the strict ops, since a
different value may share it; the recheck drops the extras. The
encoding only has to be monotone (`a < b` ⇒ `key(a) ≤ key(b)`), not
strictly — which is what makes cutting strings and rounding big `Int`s
safe.

Rejected: rejecting long values (an error on insert, as the old
`too_large` did for documents) or hashing them (loses order, so no
range queries).

### 28.4 Choosing an index: a fixed rule
`Filter::index_range` picks:
1. the first indexed field with an `Eq` condition, otherwise the first
   with any range condition;
2. then intersects **all** of that field's range conditions, so
   `tst >= a AND tst <= b` reads just `a..=b`, the time-series
   workload's range query (§5.1).

`Ne` and `Contains` never use an index: neither is a range. Conditions
on other fields are still applied, in the recheck. There's no cost
model — no statistics, no choosing between two indexes by selectivity.
With one or two indexes per collection that's the right amount of
planner. `explain` exposes the choice, for tests and for users.

Without a `sort`, `find`'s order is now unspecified: by `_id` for a
scan, by index key for an index range. §10.6 had already noted it wasn't
promised.

### 28.5 Catalog: a kind byte, and index cells
Indexes are catalog cells of their own, not a list inside the
collection's cell. Creating or dropping one appends or tombstones a
cell; the collection's cell keeps its fixed length, so
`set_current_data_page` still rewrites it in place.
- collection cell: `[u8 kind = 0][u64 index_root][u64 current_data_page][name]`
- index cell: `[u8 kind = 1][u64 root][u8 name length][collection name][field name]`
  (kind `2`, same layout: a unique index, §33.3)

Field names are capped at 255 bytes like collection names (§22.3), so
an index cell always fits. `CollectionMeta` stays a small `Copy` value;
the indexes live beside it in `Catalog` (`indexes(collection) ->
&[IndexMeta]`). An index cell whose collection doesn't exist is reported
as corruption at `load`. This changed the catalog cell format, so the
format version is now 3 (§21.2).

### 28.6 Writes keep indexes current
`apply_write_op` handles every op the same way through one function,
`update_secondary_indexes(old, new)`, where each side is a
`(document, location)` or nothing: insert is `(none, new)`, delete is
`(old, none)`. For each index it computes the key before and after,
and does nothing if both key and location are unchanged. Otherwise it
removes the old entry and inserts the new one. The location matters:
a document that moves (§20.3) needs its index entries re-pointed even
when the value didn't change. An update or delete reads the old
document first, but only when the collection has secondary indexes.

Building, maintaining and dropping all run through the batch protocol,
so a failed batch rolls back its index entries with everything else. To
share that protocol, `write_batch`'s body became `Database::transact`,
which takes a closure over `(&mut Catalog, &mut FileStore)`;
`write_batch`, `ensure_index` and `drop_index` all use it.
`BTreeIndex::free_all` reads the whole tree before freeing any page of
it, as `free_chain` does (§26.5).

### 28.7 Tests
- `variable_length_keys_match_a_btreemap` (`btree.rs`): 3000 random
  inserts and removes of keys from 0 to 1024 bytes over a four-letter
  alphabet, including `0x00` and `0xFF`, so long shared prefixes and
  uneven splits are common. Every lookup, the full scan and 400 random
  ranges must match `InMemoryIndex`.
- `indexed_finds_match_full_scans_through_every_kind_of_write`
  (`collection.rs`): random documents cover every value type, duplicate
  values, `Int`s past 2^53, strings that differ only past the cut,
  `NaN`, `-0.0` and missing fields. Padding of very different sizes
  makes updates move documents. The index is built over existing
  documents, then maintained through inserts, updates (half of which
  keep the indexed value, so only the move touches it) and deletes.
  Then 300 random one- and two-condition filters must find through the
  index exactly what checking every document finds — before and after
  a reopen. About 90 % of the filters use the index, and about 60 %
  match something.
- **Checked by breaking the code on purpose:** three bugs were
  introduced one at a time and each made that test fail — not removing
  old keys, making `Lt` bounds exclusive, and not re-pointing a moved
  document's entries. The last one first went unnoticed, which is why
  the padding and the value-keeping updates exist.
- Plus: idempotent `ensure_index`/`drop_index`, a dropped index's pages
  being reused, declaring an index before the collection exists,
  rejecting `_id` and overlong field names, a failed batch leaving no
  index entries, and the time-series range query (§15) with an index.

### 28.8 Cost and limits
- Each index costs one B-tree insert per insert, and a remove + insert
  per update that changes the value or moves the document. Updates and
  deletes also read the old document.
- `ensure_index` on a large collection is one big batch: every index
  page is staged in memory and written to the WAL (§19.9).
- One field per index (top-level at first; dotted paths since §31);
  no compound, unique, or sparse/partial
  options; no index-ordered `sort` (results are still sorted in
  memory — until §34). Each is a natural next step, none is needed by the reference workloads yet.

## 29. API rounding-out: `find_one`, `count`, `upsert`, `cursor` (`collection.rs`, `cursor.rs`)

Real and tested, on both the typed and the untyped path:

```rust
users.find_one(filter)?;           // Option<T>; find_one_with_id: Option<(DocId, T)>
users.count(filter)?;              // usize, no document converted to T
users.upsert(filter, user)?;       // Upserted::Inserted(id) | Upserted::Updated(id)
for item in users.cursor(filter)? { let (id, user) = item?; }
```

All four share the candidate step `find` already had, now factored out:
`candidate_entries` (one secondary index's range, or the whole primary
index, §28.4) and `read_candidates` (the documents behind them, not yet
checked against the filter). A candidate's id comes from its key, which
ends with the `DocId` in both kinds of index (`key::doc_id`).

### 29.1 `find_one`
The first item of a `cursor` (§29.4): without a `sort` it stops reading
at the first match, rather than reading everything and dropping all
but one. With a `sort`, "first" means first in that order, which needs
every match anyway — unless an index on the sort field gives the order
(§34.2). `find_one_with_id` exists for the same reason as
`find_with_ids` (§23): a typed caller that wants to update what it
found needs the id.

### 29.2 `count`
Counts matches of the filter's conditions, capped at its `limit`;
`sort` doesn't matter. Without conditions it counts the primary index's
entries and reads no document at all. With conditions it reads the
candidates (through an index when one applies) and checks each, but
never converts one to `T` — the typed `count` delegates to the untyped
one. A count kept in the catalog was rejected: every write would have
to maintain it for a query that's cheap enough without.

### 29.3 `upsert(filter, doc)`
Replaces the one document matching the filter's conditions (keeping its
id), inserts `doc` with a new id if none matches, and fails with
`Error::MultipleMatches` — writing nothing — if several do. Returns
`Upserted::Inserted(id)` or `Upserted::Updated(id)`.

**By filter, not by id**: ids are generated by trunkdb, so a caller
rarely knows the id of a document that may not exist yet. What it does
know is its own key — a business key or a slug —
which is a filter, ideally over a secondary index on that field.

**Atomic**: the lookup and the write run inside one `transact` (§28.6),
under the write lock, against the staged pages. Two threads upserting
the same key can't both see "missing" and both insert
(`concurrent_upserts_of_one_key_insert_it_once`). A version that calls
`find_one_with_id` and then `insert`/`update` separately fails that
test every time — checked.

**Several matches are an error**, not "update the first" as in MongoDB:
there's no meaningful first without a sort, and a key the caller thought
unique but isn't is a bug worth surfacing. A unique index (§33) catches
it at insert instead.

Not in `Batch`: a batch op's outcome (insert or update, which id) would
only be known at `commit`, unlike `Batch::insert`'s id (§24.3). Add it
when a caller needs it.

### 29.4 `cursor`: streaming without holding the lock
`cursor(filter)` returns a `Cursor<T>`, an `Iterator<Item =
Result<(DocId, T)>>`. When it's created, it takes the **ids** of all
candidates — 16 bytes each, no documents. Each `next` then:
1. looks the next id up in the primary index;
2. reads that document under a short read lock;
3. checks it against the filter.

It stops at the filter's `limit`. Memory stays at one document plus the
id list — the point for the large-document workload, whose documents
can be hundreds of KB each
(§26).

Between items the cursor holds **no lock**, so the caller can write
while iterating, even in the loop body. What the cursor then sees:
- a document deleted meanwhile is skipped;
- one updated meanwhile is checked in its new state;
- one inserted meanwhile isn't seen.

Each item is one consistent document; different items can come from
different moments. That's "read committed" per document, not a
snapshot.

Considered and rejected:
- **Holding the read lock for the cursor's lifetime**: a true snapshot,
  but a forgotten cursor blocks every writer, and writing in the loop
  body would deadlock (§27.2: `std`'s `RwLock` isn't reentrant).
  Lifetimes make it awkward too: the guard borrows from the `Database`
  the cursor would also have to own, which Rust can't express without
  a self-referential struct.
- **Snapshotting record locations instead of ids** (one lookup less per
  item): after a delete, a location's slot can be reused by a
  *different* document (§20.2) — the cursor would hand out the wrong
  document with the right-looking id. Going through the id is one
  primary index lookup (O(log n)) per item, and always right.

With a `sort` nothing can stream: which document comes first isn't
known until all are read. Such a cursor runs the whole `find` when
created and hands out its results — same API, `find`'s memory profile.

A cursor that hits an error returns it as an item and can continue with
the next id; `Error::Poisoned` (§27.3) mid-iteration is such an item.

### 29.5 Tests
- `find_one` with and without a match and a sort.
- `count` without conditions, with conditions, up to a limit, through
  an index where the recheck has to drop a candidate, and on a missing
  collection.
- `a_cursor_streams_and_sees_writes_made_meanwhile`: while a cursor is
  open, the test deletes, updates and inserts. It would deadlock if the
  cursor held the lock, and it checks each of the effects listed in
  §29.4.
- A sorted cursor yields in order.
- `upsert` updating, inserting, and refusing several matches without
  writing.
- Four threads racing to upsert the same five keys leave exactly five
  documents.

## 30. Export and import as JSON Lines (`json.rs`, `export.rs`)

Real and tested:

```rust
db.export(File::create("backup.jsonl")?)?;       // Summary { collections, documents }
new_db.import(File::open("backup.jsonl")?)?;     // same ids, same indexes
db.collections()?;                               // ["pings", "users"]
```

The whole database becomes one text file that doesn't depend on the page
layout. That makes it the migration path between file format versions
(§21.2): export with the old trunkdb, import with the new one, and
trunkdb never has to read an old format itself — which 1.0.0 needs
(§39). It's also a backup that can be read and `diff`ed, and a way to
bring data in from elsewhere.

### 30.1 Tagged JSON (`json.rs`)
`serde_json::Value` via the serde bridge (§13) loses information:
`Binary` would come back as an array of numbers, an `Id` as a string,
and JSON has no `NaN` or infinity. So `json.rs` converts `Document`
directly, and marks what plain JSON can't hold with a one-key object
whose key is a tag, like MongoDB's Extended JSON:
- `Id` → `{"$id": "<uuid>"}`;
- `Binary` → `{"$binary": "<base64>"}` (standard alphabet, padded; a
  dozen lines in `json.rs` rather than a dependency);
- `NaN`/`±inf` → `{"$float": "NaN"}`, `"Infinity"`, `"-Infinity"`;
- an `Object` that *is* a one-key object with a tag name →
  `{"$object": {...}}`, so it can't be mistaken for a tag.

Everything else is plain JSON. `Int` and `Float` stay apart because
`serde_json` always writes a float with a fraction or exponent (`3.0`,
`1e300`) and reads such a number back as a float. So plain JSON is
tagged JSON: a hand-written file needs no tags, and an object with an
unknown `$` key (MongoDB's `$date`) is just an object. Integers outside
`i64` are an error, not a rounded float.

Two `serde_json` features are needed:
- `preserve_order`: a `Value` object otherwise sorts its keys, and
  field order is part of a document;
- `float_roundtrip`: the default float parser is faster but can be one
  bit off. The round-trip test found it: `4.1946076254797075e17` came
  back as `4.194607625479707e17`.

`serde_json` is now a regular dependency (it was only used by tests).
A Cargo feature to make it optional was rejected for now: it's small,
nearly every Rust app already has it, and a feature flag would split
every build and test run in two. Easy to add if someone minds.

### 30.2 The file
```text
{"$trunkdb_export":1}
{"$collection":"users","$indexes":["age"]}
{"name":"Ada","age":36,"_id":{"$id":"0199…"}}
{"$collection":"pings","$indexes":[]}
…
```
- **Header line** with the export format's own version — independent
  of the file format version, which is the point.
- **A collection line** per collection (name, indexed fields — a unique
  index as `{"field": ..., "unique": true}`, §33.5), then its
  documents, one per line. Collections come in name order, documents in
  id order, so exporting the same data twice gives the same bytes.
- **A document line** is the document itself: an `Object` already
  carries its `_id` (§18). Any other document (a bare `Int`, an array)
  has nowhere to put an id, so it's wrapped:
  `{"_id": {"$id": ...}, "$value": 5}`. An `Object` whose only field
  besides `_id` has a tag name goes whole into `$object`, with the
  `_id` repeated outside — so the wrapper stays unambiguous, and field
  order is kept.
- A collection line is recognized by `$collection` **and no `_id`**:
  every exported document has an `_id`, so a document with a
  `$collection` field isn't misread.

One file per database, not one per collection: a backup or migration is
one thing to move, and the collection lines already separate the parts.

### 30.3 Import: ids kept, chunked, not atomic
Every document keeps its id — documents that refer to others by id
would otherwise point at nothing. This needed no new internal step:
`WriteOp::Insert` has always carried the id, and `write_batch` is
public. A document line without `_id` (hand-written) gets a new one;
an `_id` that isn't `{"$id": ...}` is an error, not a guess.

Documents are written in batches of 1000, or about 8 MB of JSON,
whichever comes first: a batch holds every page it changes in memory
until it commits (§19.9), so one batch for a whole import would need
the database's size in memory. The price is that **an import isn't
atomic**. It stops at the first bad line (`Error::Import { line,
message }`) or existing id (`Error::DuplicateId`), and every batch
before that stays. The documented safe way: import into a new file and
switch over only if it succeeds.

Each collection's indexes are built with `ensure_index` once its
documents are in — one pass over the collection, instead of maintaining
every index on every insert. A collection line with no documents still
creates the collection, so empty collections survive the round trip.

### 30.4 Export: one snapshot under the read lock
The export holds the read lock from the first line to the last. So it
is a consistent snapshot of the whole database: a batch lands entirely
before or entirely after it, and documents in different collections
that refer to each other agree. Readers go on in parallel; writers wait.

That's the opposite of `cursor` (§29.4), which holds no lock between
items, and the first roadmap sketch had planned to export through a
cursor.
Rejected, because an export is a backup: read-committed per document
would let a batch that updates two collections appear half-applied. The
reasons the cursor avoids the lock don't apply here — the export is one
function call, so there's no forgotten guard and no lifetime problem,
and nobody writes to the database from inside it. The one rule: the
`Write` the export writes to must not write to the same database, which
would deadlock. Memory stays at one document plus the ids of one
collection.

### 30.5 Tests
- `export_then_import_reproduces_every_document_id_and_index`: 2500
  documents of random shapes — every value type nested three deep, keys
  that look like tags (`$id`, `$value`, `$object`, `$collection`, `_id`
  inside nested objects), top-level documents that aren't objects — plus
  a 100 KB document in overflow pages, an empty collection and an index
  on an empty one. The import must reproduce every collection, index,
  id and document byte for byte, also after a reopen; the rebuilt index
  must answer a query; and exporting the copy must give the same text.
- `floats_come_back_bit_for_bit` (`json.rs`): 200,000 random floats
  through text and back. Fails without `float_roundtrip` — checked.
- Tagged JSON: every value kind, `Int`/`Float` separation, plain JSON
  as input, malformed tags rejected, document lines with and without
  ids, base64 against RFC 4648's test vectors.
- A hand-written file (no ids, no `$indexes` on one collection) imports.
- Ten kinds of bad input each report their line number.
- An existing id in the second batch: the first batch stays, the second
  is rolled back entirely.
- `export_is_a_snapshot_writers_wait_for`: halfway through an export,
  another thread updates the last document. The export must contain the
  old version, and the update must land afterwards.

### 30.6 Limits
- Not atomic on import (§30.3).
- Writers wait for the whole export. For a large database that's
  seconds; a snapshot that doesn't block writers needs MVCC (§39.2).
- Reading `mongoexport` output directly (`$oid` is 12 bytes, not 16;
  `$date`, `$numberLong`) is left out: its `$oid` values aren't
  `DocId`s, so a migration from MongoDB needs decisions only the app
  can make. A small program using `import`'s format can do it.

## 31. Nested-field paths (`query.rs`, `collection.rs`)

Real and tested: a condition, a sort and an index can name a field
inside nested objects with a dotted path.

```rust
people.ensure_index("address.city")?;   // like LiteDB's "$.Address.City"
let in_berlin = Filter {
    conditions: vec![Condition {
        field: "address.city".into(),
        op: Op::Eq,
        value: Document::String("Berlin".into()),
    }],
    sort: Some(Sort { field: "address.zip".into(), order: SortOrder::Asc }),
    ..Filter::default()
};
people.find(in_berlin)?;                 // reads the index, sorts by zip
```

On the typed path a nested struct serializes to a nested object, so
`address.city` is simply the `city` field of a `Person`'s `address`.

### 31.1 One lookup for filters, sorts and index keys
`query::field_value` walks the path, one object per dot. It was already
the single place where conditions, sorts and index keys read a field;
now it follows a path instead of reading one key. So a filter and an
index can't disagree about where a document's value is — which matters
because every index candidate is rechecked against the filter (§28.3):
if the two looked in different places, documents would silently go
missing from indexed finds.

No file-format change: the catalog already stores an index's field as a
string (§28.5), and a path is just a string with dots in it. The key
encoding, the key-size cap and "one entry per document per index" stay
as they are.

### 31.2 A dot always separates
`a.b` always means "field `b` of the object in field `a`", never a key
literally named `a.b`. Such keys can still be stored and read back; a
path just can't reach them. That's MongoDB's rule too.

Rejected: trying the literal key first and falling back to the path. A
path would then have two possible answers, and which one counts would
depend on each document — adding an unrelated `"a.b"` key would
silently change what a document is indexed under. Rejected: an escape
syntax (`a\.b`). It adds a small language for a case that barely comes
up: serde field names can't contain dots unless renamed on purpose.

### 31.3 Arrays aren't walked into
A step that lands on an array stops the walk: `items.name` finds
nothing in `{"items": [{"name": …}]}`, and neither does `items.0.name`.

Rejected: numeric steps into arrays (`items.0.name`). They're rarely what
you want, and they'd suggest `items.name` should mean "any element",
which it doesn't. Deferred: that "any element" meaning (MongoDB's
multikey indexes). An index would need one entry per element, so one
document several entries — breaking the "one entry per document per
index" rule the write path relies on (§28.6). It belongs with array
conditions in filters, which don't exist yet either.

### 31.4 What `ensure_index` rejects
A path with an empty part (`a..b`, `.a`, `a.`, the empty string), and
`_id` or anything below it — `_id` is the primary key, and an id has no
fields. Both mistakes would otherwise create an index where every
document sits under null (a missing field, §32). Filters don't check
paths: `matches` has no error to return, and a malformed path finds no
field, so it behaves like a missing one — null (§32).

### 31.5 Files from 0.3.0
In 0.3.0, a field name containing a dot meant a top-level key with that
literal name. An index created then on such a name was filled from those
keys. Now the name is a path, so writes look elsewhere: removing an old
entry finds nothing to remove (a no-op, §28.6), stale entries stay, and
indexed finds can miss documents. Nothing in the file tells the two
meanings apart. The fix is to rebuild the index — `drop_index` and
`ensure_index`, or an export and import (§30), which rebuilds every
index. Indexes on names without a dot, i.e. all normal ones, are
unaffected. (Since §32 the format version is 4, so a 0.3.0 file goes
through export and import anyway, and this can't happen.)

### 31.6 Tests
- `nested_path_indexes_match_full_scans_through_every_kind_of_write`:
  an index on `a.b.c` over documents that put the value at that path, one
  level short, inside an array, under keys with dots (`"a.b.c"`,
  `"b.c"`), or nowhere. It is built from existing documents, then kept
  through inserts, updates that move values in and out of the path's
  reach, and deletes; after all of them and again after a reopen, 300
  random filters must find through the index exactly what a full scan
  finds. Checked by breaking it on purpose: index keys read the old
  way (top-level key only) while filters walk the path — it fails.
- `typed_nested_fields_are_filtered_indexed_and_sorted_by_path`: nested
  structs, an index on `address.city`, a sort by `address.zip`, and an
  update that moves a document to another city and out of the result.
- Path semantics in `query.rs`: nested matches, missing and non-object
  steps, no deep search, arrays not entered, dotted keys not reached,
  malformed paths matching nothing, sort by a nested field.
- `ensure_index` rejects `_id`, `_id.x` and five malformed paths.
- The export round trip (§30.5) now includes an index on a nested path.

### 31.7 Cost and limits
- A lookup splits the path as it walks — no allocation — so a top-level
  field costs what it did before.
- No array traversal (§31.3), no escaping of dots (§31.2).
- Still one field per index: compound indexes are still open (§39.2);
  unique ones came in §33.

## 32. Null and missing fields (`query.rs`, `index/key.rs`, `collection.rs`)

Real and tested: a condition against `Null` finds documents where the
field holds null *and* documents that don't have the field at all.

```rust
#[derive(Serialize, Deserialize)]
struct Member { name: String, nick: Option<String> }   // C#: string? Nick

members.ensure_index("nick")?;
members.find(Filter { conditions: vec![Condition {
    field: "nick".into(), op: Op::Eq, value: Document::Null,
}], ..Filter::default() })?;   // every Member whose nick is None — via the index
```

Storing null always worked: `Document::Null`, and `Option::None` on the
typed path. Querying for it didn't: `Null` compared as "not
comparable", so `nick == null` matched nothing, and `nick != null`
matched every document that had the field — including the ones where
it was null.

### 32.1 Missing reads as null
A condition or a sort that looks at a field the document doesn't have
sees `Null` (`query::value_or_null`). That includes a path that can't be
followed (`address.city` when `address` is missing or a string, §31),
and a document that isn't an object. And `Null` compares equal to
`Null`. Together:

| condition | matches |
|---|---|
| `x == null`, `x <= null`, `x >= null` | `x` is null or missing |
| `x != null` | `x` is there and not null |
| `x < null`, `x > null` | nothing |
| `x == 5`, `x > 5`, `x contains "a"` | as before; never a null or missing `x` |
| `x != 5` | also a null or missing `x` |

The last row is the one behavior change beyond null itself: `x != 5`
always matched a *stored* null, but not a missing field. Now both.

Why equate them: on the typed path both come back as the same `None`
— serde fills a missing `Option` field with `None` — so a query that
told them apart would disagree with what the app reads back. And a field
added to a struct later is missing from every older document; if
`== null` skipped those, the query would silently return too few
results, the classic schema-less bug. SQL has no "missing" at all, and
LiteDB (a missing field reads as `BsonValue.Null`) and MongoDB (`{x:
null}` matches missing) equate them too.

Rejected: keeping them apart. What that buys — telling "cleared" from
"never set" — is rarely needed, and an `Exists` operator can add it
later without changing anything here.

### 32.2 Indexes hold null, and missing as null
`Null` became an indexed type with the lowest tag (`0`), one value,
and a missing field is indexed as null. So `== null`, `<= null` and
`>= null` read a range of the index, like any other value (§28.4), and
the existing range logic needed no change: a tag of its own keeps every
other type's range free of nulls.

The cost: every document now has an entry in every index — before, a
document without the field had none. For an index on a field most
documents lack, that's an entry per document that used to be free.
MongoDB makes the same trade by default. Rejected for now: *sparse*
indexes (skip missing fields, as an option). Such an index couldn't
answer `== null`, so it would need its own planner rule; it's worth
adding only when an index on a rare field turns out to be too big.

### 32.3 Format version 4
An index built by format 3 has no entry for a null or missing field, so
it would silently miss documents for `== null`. That's a change in what
an index contains, so the format version is now 4 (§21.2), and a
format-3 file is refused. The error now says how to move it: export
with the trunkdb version that wrote it, import with this one (§30) —
which rebuilds every index.

Rejected: upgrading format-3 files in place, by rebuilding their
indexes on open. It would be trunkdb's first automatic migration — an
open that writes, plus a header change in the same batch — for files
that only exist in development so far. Export and import is the
documented path (§21.2) and already tested.

### 32.4 Tests
- `a_missing_field_is_null` (`query.rs`): every operator against null
  and against a value, on a null field, a missing field, a document
  that isn't an object, and a set field; plus a path whose parent is
  missing.
- `null_and_missing_fields_are_found_alike_through_the_index`: typed,
  with `Option` fields stored as null, left out by
  `skip_serializing_if`, and missing because the document was written
  with an older struct. `== null` and `!= null` find the right ones,
  `== null` through the index, and an update that clears a field moves
  its entry.
- The two index-versus-scan tests (§28.7, §31.6) already had null
  values in their random filters and documents without the field;
  they now check those through the index too. Checked by breaking it on
  purpose: with missing fields left out of the index again, both fail,
  and so does the typed test.
- Key encoding: null's key, and that `Eq null` covers it while number
  ranges don't.

### 32.5 Limits
- No `Exists` operator: "null or missing" is one state for queries.
- A sort still leaves null and missing where they are relative to other
  values (not first or last): `compare` orders only values of one kind.
  (§34.1 fixed that: they sort first.)
- Every index grows by one entry per document without the field (§32.2).

## 33. Unique indexes (`collection.rs`, `catalog.rs`, `storage/file.rs`, `export.rs`)

Real and tested: an index can also be a constraint — no two documents
may have equal values in its field.

```rust
users.ensure_unique_index("email")?;   // like LiteDB's EnsureIndex(x => x.Email, true)
users.insert(ada)?;                     // ok
users.insert(also_ada)?;                // Err(Error::DuplicateValue { collection, field, id, existing })
users.unique_indexes()?;                // ["email"]
```

A unique index is an ordinary secondary index (§28) plus a check: it
answers the same queries the same way, and `indexes()` lists it too.

### 33.1 What counts as a duplicate
Two values are duplicates if a filter's `Eq` says they're equal
(`query::equal`): `1` and `1.0` collide, `0.0` and `-0.0` too; `"a"` and
`"A"` don't. So "no duplicates" means exactly "`find(field == v)` never
returns two documents" — the index and the filter agree on what equal
means.

**Null and missing values are exempt**: any number of documents may lack
the field or hold null. That's what SQL's `UNIQUE` does (Postgres,
SQLite; SQL Server allows one `NULL`). MongoDB counts missing as a
duplicate null, so an optional field there needs a partial index as
well. Here an optional field like `email` can be unique as it is.
Values no condition can match as equal — arrays, objects, binary, ids,
`NaN` — aren't in the index (§28.1) and aren't checked either.

Keys alone can't decide it. Different values can share a key's value
part: `Int`s beyond 2^53 that round to one `f64`, strings cut to the
key budget (§28.1). So `check_unique` reads every document under the
same value part and compares the real values — normally there are none,
so the check costs one B-tree range lookup.

### 33.2 When it's checked
Right before a unique index gets a new entry, in
`update_secondary_indexes`: on every insert, and on every update that
changes the field's value. An update that keeps its value (even if the
document moves, §20.3) isn't checked. A failure is
`Error::DuplicateValue`, naming both documents, and like any failed op
it rolls back the whole batch (§17.3, §22.2).

The check runs per op, against the state after the batch's earlier ops.
So a batch that swaps two documents' values fails, even though the end
state would be fine — MongoDB behaves the same. The workaround is three
ops through a temporary value. Rejected: checking once at the end of
the batch. It would need every changed key collected across the batch
and a second pass — for a case that barely comes up.

`ensure_unique_index` over existing documents checks each one as it
goes into the new index: the first duplicate fails, names two documents
that share a value, and nothing is created.

Asking for the other kind of index on a field that has one is an error
(`InvalidInput`), not a match and not a conversion — `ensure_*` means
"make sure this exists as declared", and a declaration that disagrees
with the file is a bug to surface. Dropping and recreating it is one
line. LiteDB also refuses a different definition for an existing index.

### 33.3 Catalog: a new cell kind
A unique index is a catalog cell of kind `2`, laid out like kind `1`
(§28.5). Rejected: a flags byte in the index cell. That changes the
layout of every existing index cell; a new kind leaves them alone.

### 33.4 Format 5, and reading format 4 as it is
An older build would read a kind-`2` cell as corruption ("unknown
catalog entry kind 2 — file may be corrupt") — wrong, and alarming. So
the format version is now 5, and a format-5 file is refused by older
builds with the clear "newer than this build" error.

But a format-4 file needs no migration: it *is* a valid format-5 file,
one without unique indexes. So this build opens format 4 as it is
(`COMPATIBLE_OLDER_FORMATS`). The header gets stamped 5 on its next
write — every header write stamps the current version, and the header
is written whenever a page is allocated or freed. Creating a unique
index always allocates its root page, so the batch that adds the first
kind-`2` cell also writes version 5, atomically. A file that uses
something new always says so; a file that doesn't may keep saying 4,
which is true. The first time a format change doesn't need export and
import (§21.2).

### 33.5 Export and import
`$indexes` lists a unique index as an object instead of a string:

```text
{"$collection":"users","$indexes":["age",{"field":"email","unique":true}]}
```

Files without unique indexes are unchanged, so the export format stays
version 1. An older build reading the object form stops with an error
naming the line. On import, the indexes are built after the
collection's documents (§30.3), so a file whose data breaks a unique
index fails after its documents are in — `Error::DuplicateValue`,
naming the two, and without that index.

### 33.6 Tests
- `a_unique_index_refuses_exactly_what_a_scan_finds_taken`: 300 random
  single-op batches (insert, update, delete; values of every type, with
  frequent duplicates, large ints that share a key, long strings cut to
  the same key, nulls, missing fields, documents that grow and move).
  Each must fail exactly when a scan finds another document with an
  equal non-null value, and a failed one must change nothing; at the end
  the index must still agree with a scan.
- The rules one by one: case-sensitive strings, `1` vs `1.0`, `0.0` vs
  `-0.0`, same key but different values, `NaN` and arrays unchecked,
  nulls and missing fields exempt; a batch whose second insert fails
  loses its first; an update onto a taken value; keeping a value while
  moving; freeing a value by delete; all of it again after a reopen.
- `ensure_unique_index` over duplicates creates nothing and names them;
  the other kind on an indexed field is refused.
- A format-4 file opens, stays 4 through a write that allocates nothing,
  and becomes 5 in the batch that creates its first unique index; the
  same for the header alone (`storage/file.rs`). Format 3 is still
  refused.
- Export and import keep the unique flag; a hand-written file declares
  one; a duplicate in the data fails the import; malformed `$indexes`
  entries name their line; the catalog keeps the kind across a reopen
  and drops a kind-`2` cell.
- Checked by breaking it on purpose, three ways: duplicates decided by
  key instead of value, nulls not exempt, no check on writes (only when
  building). Each fails two or three of these tests.

### 33.7 Limits
- One field per unique index; no compound uniqueness (`(tenant, email)`).
- A value swap inside one batch fails (§33.2).
- Case-insensitive uniqueness would need a case-folded key; it isn't
  there. An app that wants it stores a folded copy and indexes that.

## 34. Sorting through an index (`query.rs`, `collection.rs`, `index/key.rs`)

Real and tested: a `find` with a `sort` and a `limit` on an indexed
field reads the index in order and stops after `limit` matches — the
documents after that are never read.

```rust
runs.ensure_index("started_at")?;
runs.find(Filter {
    sort: Some(Sort { field: "started_at".into(), order: SortOrder::Desc }),
    limit: Some(20),
    ..Filter::default()
})?;   // reads 20 documents, not every run; explain: IndexOrder { field: "started_at" }
runs.find_one(newest_first)?;   // reads one
```

### 34.1 One sort order: the index's
Reading an index gives its keys' order. For that to be a way of
sorting rather than a different result, in-memory sorting had to use
the same order, and before this it had none worth copying: values that
don't compare (a number and a string, a null and anything) counted as
equal. That isn't a total order — `1 = null = 0`, yet `1 > 0` — and
Rust's `sort_by` may panic on a comparison that isn't one, or return an
order that depends on the input's. Now `query::sort_order` is total:

- null (and missing, §32) before bools before numbers before strings —
  the index's type tags (§28.1, §32.2) — each kind by value, all of it
  reversed for `Desc`;
- values nothing orders (arrays, objects, binary, ids, NaN) after all of
  those in **both** directions, as equals — no index holds them;
- equal values in id order, on every plan: ids are UUIDv7, so that's
  insertion order. The in-memory path sorts its candidates by id before
  its stable sort; an index keeps equal values in id order anyway.

Nulls come first ascending, like SQLite and MongoDB (Postgres puts them
last). Rejected: nulls last in both directions — the index has them at
the low end, so a descending walk would have to find them separately.

Numbers now compare by exact value, `Int` against `Float` too. Before,
an `Int` was cast to `f64`, which rounds beyond 2^53: `2^53 + 1` equaled
the float `2^53` but was greater than the int `2^53` — no total order
survives that. This also changes filters, for numbers beyond 2^53 only:
`Int(2^53 + 1) == Float(2^53)` is now false. Index keys stay valid:
rounding keeps order, so `a < b` still implies `key(a) <= key(b)`.

### 34.2 When and how the index is read in order
`Filter::index_order` picks it when the filter has a `sort` and a
`limit`, the sort field has an index, and no `Eq` condition could be
answered by another index — a few documents found by value beat a walk
in order. Range conditions on the sort field itself narrow the walk.
`explain` says `IndexOrder { field }`. Rule-based like §28.4: a very
selective range condition on another field loses to the walk.

`read_in_index_order` walks the index's range, grouping entries by
their key's value part:
- a group whose values are all equal (`key::is_exact` — everything but
  numbers beyond 2^53 and strings cut to the key budget) is already in
  id order; documents are read one by one, and reading stops at the
  limit;
- the rare group that isn't is read whole and sorted exactly;
- for `Desc`, the groups are walked in reverse, each still in id order.

If the index runs out before the limit, the values no index holds may
still be missing: they sort last. A scan then finds them — unless a
range condition on the sort field is there, which none of them can match.

`find_one` asks for `limit: 1`, so a sorted `find_one` reads one entry's
document instead of every match. A sorted `cursor` runs `find` (§29.4),
so it benefits the same way.

Rejected: using the index without a limit. Every match is read either
way, plus a possible scan for unordered values; sorting in memory costs
little next to the reads. Rejected for now: a lazy B-tree walk. `range`
still collects the range's entries (keys and locations, no documents),
a few hundred per leaf page — the documents are what's expensive. A
lazy walk, with backward leaf links for `Desc`, is a later step.

### 34.3 Tests
- `sorting_through_an_index_matches_sorting_in_memory`: every kind of
  value (huge numbers sharing a key, strings cut to one key, nulls,
  missing fields, arrays, NaN) through inserts, updates that move
  documents, deletes, and a reopen. 300 random sorted filters, each
  compared element by element — same documents, same order — with a
  full scan sorted in memory. Sorts by two indexed fields, with and
  without limits, with range, `Eq`, `Ne` and unindexed conditions, so
  every plan runs (`IndexOrder` on either field, `Index`, `Scan`).
- `a_limit_stops_the_reading_when_an_index_gives_the_order`: with a
  test-only counter of documents read (`data::RECORDS_READ`): 1000 read
  without an index, 5 with it for `limit 5`, 1 for a sorted `find_one`,
  11 for a range of ten (the bound is included, §28.4) — and an array
  value sorted after everything else.
- `sort_orders_every_kind_of_value_totally`: the order itself, both
  directions, with ties keeping input order.
- `ints_and_floats_compare_by_exact_value`, and which keys are exact.
- Checked by breaking it on purpose, four ways: every group treated as
  exact; `Desc` reversing entries instead of groups (ties in reverse id
  order); no scan for unordered values; no id pre-sort in memory. Each
  fails a test.

### 34.4 Limits
- One sort field; no index for a sort on several.
- `range` collects a range's entries before the walk (§34.2).
- Sorting by `_id` gives id order in both directions: ids are among the
  values nothing orders (§34.1), so they all tie.
- `count` ignores `sort`, as before.

## 35. A filter builder (`query.rs`, `document.rs`)

Real and tested: a `Filter` can be built one call at a time.

```rust
Filter::new()
    .eq("status", "Complete")
    .gte("seen", 10)
    .sort_desc("started_at")
    .limit(1)
```

What it replaces, still valid (the fields stay public):

```rust
Filter {
    conditions: vec![
        Condition { field: "status".into(), op: Op::Eq, value: Document::String("Complete".into()) },
        Condition { field: "seen".into(), op: Op::Gte, value: Document::Int(10) },
    ],
    sort: Some(Sort { field: "started_at".into(), order: SortOrder::Desc }),
    limit: Some(1),
}
```

### 35.1 Methods, starting from `Filter::new()`
`eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `contains`, `is_null`,
`is_not_null`, and `condition(field, op, value)` for any `Op`; each adds
one condition, ANDed with the rest. `sort_asc`/`sort_desc`/`sort_by` and
`limit` replace what was there — there's one sort and one limit.
Fields are any `impl Into<String>`, dotted paths included (§31). Each
method takes `self` and returns it, so building conditionally is
`filter = filter.eq(...)` inside an `if`.

Rejected: starting points like `Filter::eq("status", ...)` next to the
chaining `.eq(...)`. A type can't have an associated function and a
method of the same name, so it would take a second set of names
(`Filter::where_eq`) or free functions (LiteDB's `Query.EQ`, which
builds a query object, with `Query.And` to combine). One starting point
and one set of names is less to learn; `Filter::new()` costs one call.

Rejected: a borrowing builder (`&mut self -> &mut Self`). It chains the
same way, but `let f = Filter::new().eq(...)` would then borrow a
temporary that is dropped at the end of the statement; the consuming
form just works, and a `Filter` is cheap to move.

### 35.2 Plain values as `Document`s
`From` implementations turn plain values into `Document`s, so a value
is `36` or `"Berlin"` rather than `Document::Int(36)`: `bool`; `i8`–`i64`
and `u8`–`u32`; `f32`, `f64`; `&str`, `String`; `DocId`; and `Option`
of any of these, with `None` as `Null` — so a typed `Option` field's
value goes into a filter as it is, and `None` finds null and missing
(§32). A `Document` can always be passed directly.

Left out on purpose: `u64` and `usize` (they can exceed `i64`; a
`TryFrom` would turn every filter into a `Result`), and `Vec<T>` (is
`Vec<u8>` an `Array` or `Binary`?). Values of an app's own types — a
timestamp struct, an enum — go through `serde_bridge::to_document`.

### 35.3 Tests
- The builder produces exactly what the struct literal spells out —
  every method, a dotted path, a replaced sort and limit.
- Every `From` conversion, including an unsuffixed literal (`i32`) and
  nested `Option`s.
- End to end through a typed collection: conditions, sort and limit,
  an `Option` value, and a built filter reading an index in order
  (§34).
- The time-series integration test (§15) now builds its filters this
  way — including one assembled from optional parameters — and needs
  only `Filter` from `query` for it.
- The doc example on `Filter`'s builder `impl` runs as a doc test.

### 35.4 Limits
- Still AND only — OR and nesting came next, in §36 (`any_of`, `and`,
  and `|`, `&`, `!` on `Condition`).
- No compile-time check of field names — they're strings, as in
  MongoDB's drivers (a closure like LiteDB's `x => x.Name` can't be
  inspected in Rust).

## 36. OR, NOT and nesting (`query.rs`, `collection.rs`)

Real and tested: conditions can be combined with OR, AND and NOT, nested
as deep as needed, and an OR whose branches can use indexes reads just
those index ranges.

```rust
runs.find(
    Filter::new()
        .any_of([Condition::eq("status", "Queued"), Condition::eq("status", "Running")])
        .and(!Condition::lt("seen", 30))
        .sort_desc("started_at"),
)?;   // explain: IndexUnion { fields: ["status", "status"] }

// The same with operators — `&` binds tighter than `|`, as in Rust:
let c = (Condition::eq("status", "Queued") | Condition::eq("status", "Running"))
    & !Condition::lt("seen", 30);
```

### 36.1 `Condition` is a tree
```rust
pub enum Condition {
    Compare { field: String, op: Op, value: Document },
    All(Vec<Condition>),     // AND — true if empty
    Any(Vec<Condition>),     // OR — false if empty
    Not(Box<Condition>),
}
```
`Filter.conditions` stays a list that must all hold — the top level is
an AND, as before — but each entry can now be a group. What was the
struct `Condition { field, op, value }` is the variant
`Condition::Compare { field, op, value }`: a breaking change for code
that built conditions by hand, which the builder (§35) mostly replaced.

Rejected: a separate expression type next to a flat `Condition` struct
(`Filter.conditions: Vec<Expr>`, `Expr::Is(Condition)`). It keeps the
old struct but adds a wrapper at every use, and two names for "a
condition". Rejected: SQL's three-valued logic. `NOT` is plain negation
of "matches": `!(x == 5)` is true where `x` is missing, like `x != 5`
(§32). A missing field is never "unknown" here, only null.

### 36.2 Building
`Condition::eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `contains`, `is_null`,
`is_not_null`, `compare`, and `Condition::any([...])`/`all([...])` for
groups built from lists. `Filter::and(condition)` adds any condition,
`Filter::any_of([...])` an OR.

The operators `|`, `&` and `!` build the same trees. A chain flattens —
`a | b | c` is one OR of three — and Rust's precedence applies (`&`
before `|`). Rejected: a method `Condition::not(c)`: clippy flags it as
confusable with `std::ops::Not::not`, and implementing the trait is the
idiomatic way anyway; `!c` is what a C# or Rust reader expects. Rejected:
`.or(...)` on `Filter` itself: in a chain like
`f.eq(a).or(g).eq(b)` the grouping would depend on call order, which
reads ambiguously. `any_of` says exactly what's grouped.

### 36.3 Indexes: bounds and unions
Planning is one recursive function, `bounds_for`: for a condition, index
ranges whose **union** holds every document it can match — or `None` if
no index can bound it.
- A comparison on an indexed field: its range (§28.4), with all the
  comparisons on that field in the same AND intersected.
- An AND (the filter's list, or a nested `All`): the best bounds of any
  one part — every match must satisfy that part anyway.
- An OR: the union of every branch's bounds — but only if **every**
  branch has some. One branch no index can bound can match documents
  outside any range, so then there's no bound, and the other conditions
  or a scan decide.
- A NOT: none. The complement of a range is two ranges, but the rest of
  the NOT's semantics (missing fields, values of other kinds) make it
  more than that; not worth it yet.

Choosing, rule-based like §28.4: an `Eq` beats an OR's union, which
beats a range — "found by value" first. An OR of `Eq`s is how "status is
one of these" is written, so it ranks right after a single `Eq`. The
same rule decides against reading in sort order (§34.2): if the
conditions *other than those on the sort field* can find documents by
value — an `Eq` or a union — they're used instead. That rule no longer
depends on the order conditions were added in.

A union reads each range and keeps each document once — ranges of
different indexes, or overlapping ones of the same, can hold the same
document. `explain` says `IndexUnion { fields }`, one field per range.
An OR of nothing matches nothing and reads no range at all.

### 36.4 Tests
- `nested_filters_find_what_a_scan_finds_on_every_plan`: 400 random
  filters, conditions nested three deep — ANDs, ORs (empty ones
  included), NOTs, every operator, on two indexed fields, an unindexed
  one and a field no document has — some sorted with a limit. `find`,
  `cursor` and `count` must each give exactly what checking every
  document gives, before and after a reopen, and all four plans (scan,
  index, union, order) must run.
- `index_ranges_union_the_branches_of_an_or`: every planning rule —
  unions, ANDs inside branches, nested ORs flattening, an empty OR, an
  unbounded branch or a NOT falling back, `Eq` over union over range in
  any order, and reading in order giving way to a union.
- `an_or_of_indexed_values_reads_just_those`: typed, with the read
  counter (§34.3): `status` in two of three values reads 200 of 300
  documents.
- Matching: OR, AND, NOT, empty groups, nesting, NOT against missing
  fields; the builder's and the operators' trees, precedence included.
- Checked by breaking it on purpose, four ways: an OR skipping a branch
  with no bounds, a NOT using its inner bounds, no de-duplication in a
  union, a union of the first branch only. Each fails a test.

### 36.5 Limits
- A NOT never uses an index (§36.3).
- An AND uses one part's bounds, never the intersection of several
  indexes' ranges.
- Still no pattern language, no conditions on array elements (§39.2).

## 37. `delete_many` and dropping a collection (`collection.rs`, `database.rs`, `catalog.rs`, `data.rs`)

Real and tested:

```rust
runs.delete_many(Filter::new().lt("started_at", cutoff))?;              // -> how many
runs.delete_many(Filter::new().sort_asc("started_at").limit(100))?;   // the oldest 100
db.drop_collection("runs")?;                                          // -> false if there was none
```

### 37.1 `delete_many`: what `find` would return
It deletes exactly the documents `find(filter)` would return — sort and
limit included, so a retention rule like "the oldest 100" is one call —
and returns how many. It uses the same code as `find` (`find_in`) and
every plan with it: an index range, a union for an OR (§36.3), or a
walk in sort order that stops at the limit (§34.2).

Lookup and deletes run in one batch under one write lock, like `upsert`
(§29.3): all of them go or none, and no write can slip in between
finding a document and deleting it. The alternative a caller had —
`find_with_ids`, then a batch of deletes — could delete a document that
an update in between had made no longer match. A collection that doesn't
exist has nothing to delete and isn't created.

Rejected: ignoring `sort` and `limit`, as MongoDB's `deleteMany` does. A
filter meaning one thing to `find` and another to `delete_many` is a
trap; and the limited form is useful.

`find` now also filters under its read lock — before, it dropped the
lock before checking candidates. That's CPU work only, and sharing one
function is worth more.

### 37.2 Dropping a collection
`Database::drop_collection(name)` deletes the collection's documents,
indexes and catalog entries, and frees all their pages for reuse, in one
atomic batch. A collection owns:
- its data pages — a data page never holds two collections (§20), so
  they're all its own: every page one of its documents is on, plus its
  current data page, which stays even when empty (§20) and so may hold
  none of them;
- the overflow chains of its large documents (§26);
- its primary index tree and each secondary index tree (§28).

All of it is read before anything is freed, since freeing overwrites a
page (`data::free_collection_pages`, `BTreeIndex::free_all`). The
catalog drops the collection's cell and its index cells
(`Catalog::drop_collection`); catalog pages themselves stay, like after
`drop_index`.

Rejected: deleting the documents one by one (`delete_many` of
everything, then the empty collection). Each delete rewrites its page
and every index; dropping only reads the pages it frees. Rejected:
`Collection::drop()` instead — `drop` is what Rust calls a destructor,
and on a handle it reads like releasing the handle. LiteDB puts it on
the database too (`DropCollection`).

A `Collection` handle to a dropped collection stays usable: it only
holds a name, and the next write creates the collection again, empty.

### 37.3 Tests
- `dropping_a_collection_frees_every_page_it_had`: three collections
  interleaved in the file, the middle one with 300 documents of very
  different sizes, one in overflow pages, a plain and a unique index,
  and deletes that empty some pages — the current one included. After
  dropping it, filling a new collection with the same data must not grow
  the file by a single page; the other two must be unchanged, also after
  a reopen, and the new one must answer through its indexes. Checked by
  breaking it on purpose, four ways: not freeing the empty current page,
  the overflow chains, the secondary index trees, or the primary tree —
  each fails it. (The current page's case failed to fail at first: the
  test data left documents on it. The test now empties it.)
- `delete_many_deletes_what_find_would_return`: a range, the youngest
  five by sort and limit, an OR, nothing matching, a missing collection
  (not created); unique values free again afterwards, index queries
  consistent.
- `delete_many_matches_find_through_random_filters`: 40 rounds of
  inserts and a random nested filter, sometimes sorted and limited:
  each call deletes exactly what `find` returned just before; at the
  end the indexes agree with a scan on every plan (§36.4). Checked by
  making `delete_many` ignore the limit — both tests fail.

### 37.4 Limits
- A large `delete_many` or drop is one big batch: every changed page is
  staged in memory and written to the WAL (§19.9), like `ensure_index`.
- Freed pages are reused, but the file doesn't shrink (compaction,
  §39.2).
- No `update_many` yet — it came next (§38).

## 38. `update_many` (`collection.rs`)

Real and tested: the documents a filter finds can be changed in one
call, by a closure.

```rust
tasks.update_many(Filter::new().eq("status", "Queued"), |task| {
    task.status = "Running".to_string();
})?;   // -> how many changed

// Untyped: the closure gets the Document.
docs.update_many(Filter::new().lt("seen", 0), |doc| { /* ... */ })?;
```

### 38.1 A closure, not update operators
The change is a closure: `FnMut(&mut T)` on the typed path, `FnMut(&mut
Document)` on the untyped one. It's LiteDB's `UpdateMany(x => ...,
predicate)`, and in Rust it's the obvious shape — the compiler checks
the field names, and any change the language can express works.

Rejected for now: MongoDB-style operators (`$set`, `$inc`, `$unset`, on
dotted paths). They'd work without deserializing and read declaratively,
but they are a small language to design, and for a typed caller strictly
less than a closure. They can come later, on the untyped path, if a need
shows up.

### 38.2 What it changes
Like `delete_many` (§37.1): exactly what `find(filter)` would return —
sort and limit included — through the same code and plans. `change` is
called on each match in `find`'s order, so with a sort it sees them
sorted. Everything happens in one batch under one write lock: all
changes land or none. A unique index refusing one change (§33) rolls
back every one; so does a match that doesn't convert to `T`.

It returns how many documents changed — matches `change` left as they
were aren't written or counted. "Unchanged" is decided by the encoded
bytes, not `==`: a NaN isn't equal to itself, so a document holding one
never compared equal — the randomized test found exactly that. On the
typed path it's decided after the round trip through `T`, so a document
written before a field was added to `T` counts as changed: it gets the
field. An `_id` can't change: whatever `change` puts there, the document
keeps its own.

`change` runs while the database is locked for writing, so it must not
use this database — that would deadlock. The same rule as for `export`'s
writer (§30.4).

### 38.3 Tests
- `update_many_changes_what_find_would_return`: typed tasks — the five
  lowest-ranked of a status by sort and limit, seen by `change` in that
  order; the index follows; unchanged matches not counted; a unique
  index refusing one change rolls back all; `_id` unchangeable
  (untyped); a match that doesn't convert to `T` fails the batch.
- `update_many_matches_a_model_through_random_filters`: 30 rounds of
  inserts and a random nested filter, sometimes sorted and limited; the
  change gives `v` a random new value or leaves the document alone. The
  count and every document must match a model; at the end the indexes
  agree with a scan on every plan. It caught the NaN case above.
- Checked by breaking it on purpose, four ways: no "unchanged" check,
  the limit ignored, matches in reverse order, and comparing without the
  `_id` put back. Each fails a test.

### 38.4 Limits
- No update operators (§38.1).
- One big batch, like `delete_many` (§37.4).

## 39. Roadmap

"v1" is a milestone name, not a semver promise: 0.x minor versions may
still break API and file format. 1.0.0 is reserved for a stable format
with a migration path — export/import (§30) is that path.

### 39.1 Done
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
| next | `update_many` | §38 |

### 39.2 Open
Unordered within each group; each line says where the need or the
limit is described.

**Queries**
- A pattern language: regex or `LIKE`-style wildcards (§25.4).
- Conditions on array elements (`tags` contains `x`), with multikey
  indexes (§31.3).
- An `Exists` operator, to tell a missing field from a null one (§32.5).

**Indexes**
- Compound indexes: several fields in one key, also unique across
  several (`(tenant, email)`, §33.7).
- Sparse indexes, skipping missing fields, for fields few documents
  have (§32.2).
- A lazy B-tree walk, with backward leaf links for `Desc` (§34.2).

**Storage and durability**
- Compaction/vacuum: half-empty data pages are only refilled by updates
  of their own documents (§20), and the file never shrinks.
- Per-page checksums, to detect a damaged page instead of misreading it
  (a format change).

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
- A CLI: inspect and check a file, run export/import.
- PyO3 bindings, for Python apps.
- Benchmarks against SQLite, redb and sled.

Not planned: a cost-based query planner, joins, aggregates, multiple
writers, encryption, schema validation (§6).
