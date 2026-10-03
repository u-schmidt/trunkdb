# 78. Reads through a snapshot (`storage/snapshot.rs`, `storage/pages.rs`, `database.rs`)

The second step of §77's plan. Every read now names the commit it
reads: it gets that commit's catalog, and its pages through a
`PageStore` of their own. The one lock is still there (§27), so the
commit is always the last one and nothing behaves differently. What
changes is what a read can reach: the last commit, and not the store
where a batch may be staged.

## 78.1 A commit, and its pages
- **`Commit`**: a commit's number (§77.4) and its header. The number
  says which version of each page to read; the header says how many
  pages there were and where the free list started.
  `FileStore::last_commit` gives the current one, and refuses while a
  batch is staged, when the store's header is the batch's.
- **`SnapshotStore`**: `Pages` and a `Commit`, as a `PageStore`: what
  §57.2 called "one more `PageStore`, the pages as of commit N".
  `read_page` checks the id against the commit's page count and reads
  the page as of the commit's number. It can't write: `allocate_page`,
  `write_page` and `free_page` are errors. It also has what `check` and
  `file_info` read from the store: `page_count`, `format_version`,
  `free_pages` and `damaged_pages`.
- **`Pages::read_at(id, seq)`**: the version waiting for a checkpoint
  if commit `seq` or an earlier one wrote it, otherwise the file's
  page. `Pages::read`, the newest whatever its number, stays for the
  writer.

Walking the free list is one function now, `free_list`, over a header
and a way to read a page: the writer walks its staged list, a snapshot
its commit's.

## 78.2 A version from a later commit
`Pages` keeps one version of a page, the newest (§77.4). A snapshot
that needs an older one finds a version with a higher number than its
own, and gets an error, "snapshot too old", not the newer page. Under
the lock no read meets it: a reader's commit is the last one. §80
keeps the versions a snapshot needs, and the error then stays for the
snapshot that §82's memory limit ends.

What this can't catch yet: a checkpoint moves a page into the file and
forgets its number, so an older snapshot reading it afterwards would
get the newer page silently. Also unreachable under the lock, and also
§80's to close: a page keeps a version for as long as an open snapshot
is older than it.

## 78.3 The snapshot, and the catalog
`State` held the catalog, and a batch changed it in place, with a clone
to put back on failure (§19.5). Now:

- **`Snapshot`**: a `Commit` and the catalog as that commit left it,
  behind an `Arc`. `State::current` is the last one. Rule 4 of §57.3:
  a snapshot keeps the catalog of its commit.
- **A batch works on a catalog of its own**, cloned from the
  snapshot's: one clone per batch, as before. At commit it becomes the
  new snapshot's, with `FileStore::last_commit`. A batch that fails
  drops it; so does one that changed no page (§19.3), since every
  catalog change writes one. The snapshot there was stays, the same
  `Arc`.
- **The free-space map** (§75) travels in the catalog as before: the
  next batch clones it from the snapshot. Readers carry it unused; it
  can be split off if the clone ever shows.

## 78.4 What a read can reach
`Database::read` returned the lock's guard, and with it the whole
`State`. It returns a `ReadGuard` now, which gives out two things:
`snapshot()`, a `Reading` (the catalog and a `SnapshotStore`), and
`catalog()` for a read that needs no pages. The store isn't reachable
through it. `get`, `find`, `count`, `cursor`, `explain`, `indexes`,
`collections`, `export`, `check` and `file_info` all read this way.

Why a type and not a test: under the lock, a read of the store and a
read of the snapshot give the same bytes, so no test could tell a read
that goes around the snapshot. With §79 it would be a reader seeing
half a batch. Now it doesn't compile.

Writers are as they were: `transact` hands the batch `&mut FileStore`,
which reads its own staged pages first (§77.3). `compact`'s rebuild
reads through it too.

## 78.5 Tests
- `file.rs`:
  - a snapshot store reads its commit's pages, page count and free
    list while a batch is staged that changes all three, and the
    writer reads its own; after the commit, the new commit's snapshot
    reads the batch;
  - a page written by a later commit is "snapshot too old" for an
    earlier one, through `read_page` and `try_read_page`; pages the
    later commit left alone are read by both;
  - a snapshot store refuses to allocate, write and free, and changes
    nothing.
- `database.rs`:
  - with a batch staged, the snapshot has neither its documents nor
    the collection it made, and the batch's own catalog and store have
    both;
  - a commit replaces the snapshot, with the next number, and the one
    before keeps its catalog; a failed batch and one that changes
    nothing leave the same snapshot.
- Every other test runs unchanged, but for where a test reached into
  `State` through `read()`: it takes `snapshot()` now, or the test-only
  `state()`.
- Checked by breaking it on purpose, eight ways, each failing a test:
  a snapshot handed the newest version whatever its commit; a commit's
  own pages refused to its snapshot; `read_page` and `try_read_page`
  past the snapshot's page count; a snapshot that writes; a commit not
  replacing the snapshot; a batch that changed nothing replacing it;
  the snapshot given the commit before. A ninth, `get` reading the
  store instead of the snapshot, passed every test, which is what led
  to `ReadGuard` (78.4): it no longer compiles.

## 78.6 Limits
- Not measured. A read copies a `Commit` (a number and a header) and
  compares one number per page waiting for a checkpoint; the
  benchmarks of §48 and §58 are to be run again with §79, where the
  numbers are expected to move.
- An older snapshot isn't safe to read yet (78.2). Nothing can hold
  one: `Snapshot` is private to `database.rs` and replaced under the
  write lock.
- `damaged_pages` is of the file as it is now, not as of the commit: a
  page's older bytes aren't on disk any more.
