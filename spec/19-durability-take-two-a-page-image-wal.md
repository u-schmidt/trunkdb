# 19. Durability, take two: a page-image WAL (`storage/file.rs`, `durability/wal.rs`, `database.rs`)

Real and tested: the WAL logs the *pages* a batch changed, not the ops
that changed them. It replaces §16's op-level log, closes a real
crash-safety hole in it, and makes rollback real (closing §17.3).

## 19.1 Why the op-level WAL wasn't enough
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

## 19.2 Staging lives in `FileStore`
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

## 19.3 The commit protocol (`Database::write_batch`)
1. **stage** — `store.begin()`.
2. **apply** each op (`apply_write_op`). On error: `rollback()`, restore
   the catalog snapshot (§19.5), return the error.
3. **log** — every dirty page, as one WAL record, `fsync`.
4. **write back** — the same pages to the main file, `fsync`.
5. **checkpoint** — truncate the WAL.

(Since §51, steps 4 and 5 happen at a checkpoint, once 1,000 pages have
been committed, not in every commit.)

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

## 19.4 Recovery
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

## 19.5 Rollback, finally real
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

## 19.6 Poisoning after a failed write-back
Once the WAL record is durable, the batch is committed — but if writing
it back fails in-process (e.g. a full disk), the main file may be
half-written while the process keeps running. `write_back` keeps the
dirty set on error, so `write_batch` retries once from it (the same
images the WAL holds). If that fails too, the `Database` poisons itself:
every call — reads included — returns `Error::Poisoned` until it's
reopened, and reopening restores the batch from the WAL. The same idea
as `Mutex` poisoning, or SQLite going read-only after I/O errors.

(§51 replaced this part: a write-back now happens at a checkpoint, and
one that fails keeps its pages in memory, readable, for the next
checkpoint — no poisoning needed.)

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

## 19.7 The fresh-file bootstrap goes through the WAL too
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

## 19.8 The regression test for the original bug
`a_crash_mid_b_tree_split_loses_nothing` (`database.rs`) reproduces
§19.1's numbers exactly — 1088 committed entries, then an insert that
splits a leaf and changes 5 pages (6 since §20: `Int` data cells and
leaf entries are both 30 bytes with their slot, so the leaf and the data
page fill up on the same insert) — and cuts its write-back after every
possible page count. Without recovery (reading the file raw), 3 of
the 4 cuts damage the tree; with recovery, all 1088 documents plus the
new one survive every cut. The test asserts both halves, so it fails if
a later change makes the scenario silently stop reproducing the damage.

## 19.9 Cost and limits
Each dirty page is written twice (WAL, then main file): a single insert
is 2 pages (data page, leaf) ≈ 16 KB plus the `fsync`s — 3–4 when it
opens a new data page (plus the header and the catalog page). A page rewritten many times
within one batch is logged once (the dirty set is keyed by id), so large
batches amortize well. A batch's dirty pages live in memory until commit
— a very large batch costs RAM; documented, not solved. Still not
covered: bugs in the tree logic itself (a wrong tree is written
atomically and durably — tests catch that, a WAL can't), bit rot in the
main file (needs per-page checksums), and disks that lie about `fsync`.
