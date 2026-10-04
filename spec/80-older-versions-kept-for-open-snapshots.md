# 80. Older versions, kept for the snapshots open (`storage/pages.rs`, `storage/file.rs`, `database.rs`)

The fourth step of §77's plan, and the one its workload was waiting
for. A page now keeps its older versions in memory for as long as a
snapshot would read them. So a read takes its snapshot and lets go of
every lock: commits don't wait for it, and it goes on reading its own
commit whatever is committed, checkpointed or freed meanwhile. With an
export running again and again beside single inserts, a commit waited
132 seconds at worst before this section; now 24 ms.

No public API yet: a snapshot still lives for one call. §82 lets a
caller keep one.

## 80.1 What a read holds
`ReadGuard` held the gate (§79.3), and read `current` through it. Now
it holds a clone of the `Snapshot`, taken under `current`'s lock, which
is let go at once. A commit takes that lock only to replace the
snapshot, and waits for nobody.

Which snapshots are open is known without a registry. A snapshot is an
`Arc`, and it can only be taken from `current`:
- at a commit, under `current`'s lock, the snapshot about to be
  replaced is still held by somebody, or it never will be again. If it
  is, the writer keeps a `Weak` to it (`Writer::past`);
- `Writer::live` gives the commit numbers of those still open, and
  forgets the ones that have ended.

So the read path pays for no list, and a snapshot ends when its last
reader lets go.

## 80.2 Versions
`Pages::versions` was the newest version of each page waiting for a
checkpoint (§77.4). It is a list per page now, oldest first, each with
its commit number and whether the file holds it:
- **the newest**, from its commit until a checkpoint has written it;
- **older ones**, and a newest one already written, for as long as an
  open snapshot would read one of them.

A page with no list has one version, the file's, and everyone reads
that. The invariant: **a page has a list for as long as the file holds
anything but what every open snapshot should see.**

`read_at(id, seq)` takes the newest version at or before `seq`.

**The page as the batch found it.** The first commit to change a page
that has no list must leave the file's version for the snapshots open:
the file is about to get the new one. `FileStore`'s staging keeps it,
`before`: the committed page, taken when the batch first writes it.
`Pages::commit` puts it at the head of the new list, under commit
number 0, if a snapshot is open; otherwise it's dropped, and a commit
costs what it did. 0 is right for all of them: a snapshot older than
the file's version of that page can't be open, or the page would have
a list (the invariant).

Not kept: a page past the end of the file as it was, which no open
snapshot can read; a page that can't be read, which a snapshot couldn't
read either.

**Rule 1 of §57.3 needs nothing**, as §77.2 said: `free_page` and the
allocation that takes the page again are two writes of that page, and
an open snapshot reads the version before them. The free list and the
free-space map (§75) are untouched.

## 80.3 What goes
`prune` keeps, of a page's versions, the newest, and each one an open
snapshot reads: the newest at or before that snapshot's commit. The
rest go. One snapshot beside a thousand commits of a page keeps two
versions of it (§77.2).

- `commit` prunes the lists of the pages it changes;
- `checkpoint` prunes every list (`forget`), and a list left with one
  version that is in the file goes altogether.

So versions kept for a snapshot that has ended go at the next commit
of their page or the next checkpoint, not at once: nothing runs when a
reader lets go.

## 80.4 A checkpoint, and a reader at the file
A checkpoint writes the newest versions to the file whoever is reading
(§77.2, not rule 3 of §57.3), marks them written, puts them in the
cache, and prunes with the open snapshots. The WAL is emptied as
before.

§79.4 kept readers off the file where it changes by two facts, and the
second (a commit waits for the reads under way) is gone. What can
happen now: a reader finds no list and no cached page, and goes to the
file; before it has read, a commit changes the page and a checkpoint
writes it. The reader would get the newer page, or half of it.

`Memory::file_epoch` counts the checkpoints begun, bumped under the
lock together with listing what to write. A reader notes it at its
lookup, reads the file, and takes the lock to put the page in the
cache: if the count has moved, it throws the page away and looks
again. By then the page has a list, with the reader's version in it.
If the count hasn't moved, no checkpoint began, so nothing that the
reader's lookup didn't see was written. The scan for damaged pages
(§40) reads the file the same way.

Rejected: reading the file under the lock, which would hold it for a
disk read, with every commit and every other cold read behind it; a
lock around the file itself, held alone by a checkpoint for its
writes, which stops cold reads for a whole write-back.

## 80.5 `compact`, alone
A compaction writes every page and cuts the file (§41). Keeping each
as it was would be keeping the old file in memory, and the pages cut
off have no newer version to keep them under. Rule 6 of §57.3: it
takes the database alone.

- `Shared::gate` is back for this one purpose: every read holds it
  shared, a batch for which `FileStore::replaces_all` holds takes it
  alone, from before its log to after its publish. It waits for the
  reads under way, and none begins meanwhile. The rebuild itself runs
  beside the readers; no other batch touches the gate.
- **A snapshot kept beyond a read refuses it**: `Error::SnapshotOpen`,
  at once, before the log, with nothing changed (§77.2). Waiting would
  deadlock a thread that holds one. Nothing public can keep one yet.

## 80.6 Measured
Linux, x86-64, two cores, a virtual disk, as §79.7; the build before
this section and the one after.

**A long read beside small writes**, the workload of §77.1: 200,000
documents, one thread exporting all of them again and again (about
0.4 s each), another inserting single documents back to back, for 5 s.

| build | reader | exports | commits | commit p50 | p99 | max |
|---|---|---:|---:|---:|---:|---:|
| before | none | 0 | 11,400 | 397 µs | 769 µs | 17.6 ms |
| before | exporting | 459 | 12 | 397 µs | 267 ms | 132 s |
| after | none | 0 | 14,035 | 329 µs | 610 µs | 16.8 ms |
| after | exporting | 15 | 14,949 | 298 µs | 619 µs | 23.7 ms |

Before, the writer got in between two exports twelve times, and the run
took 132 s instead of 5 because one insert waited that long. After, the
writer is as fast with the export as without it.

**Short reads**, the `reader_wait` of §79.7, 4 s per case, three runs:
- one reader, no writer: the same, 1.39 and 1.45 million reads;
- **two readers, no writer: about a tenth fewer reads** (2.35 to 2.52
  million before, 2.00 to 2.42 after), p50 2.7 µs to 3.2 µs. A read
  now takes the gate, takes `current`'s lock and counts the snapshot's
  `Arc` up and down: three places two threads write, where there was
  one. It's the cost §65.3 names, made larger;
- beside a writer, the runs differ from each other by more than the
  builds do.

## 80.7 Tests
- `pages.rs`: `prune` over open snapshots before, between, at and
  after the versions' commits.
- `file.rs`:
  - an open snapshot reads its commit through two commits of the same
    pages, a checkpoint and a commit after it, while the file and a
    later snapshot have the newest;
  - versions go as snapshots close: by a commit for the pages it
    changes, by a checkpoint for all, down to none in memory;
  - one snapshot beside fifty commits of a page: two versions of it;
  - with no snapshot open, as many versions as pages waiting;
  - a page freed and taken again is the old page to an open snapshot,
    and the free list the old one;
  - a page committed and written back between a reader's lookup and
    its file read is read again, from its version, and the cache has
    the newest;
  - a checkpoint beside the scan for damaged pages finds no damage;
  - a batch that replaces the file panics if committed beside a
    snapshot.
- `database.rs`:
  - a read open across an update, an insert, a new collection, a
    dropped one, a new index, a checkpoint and a large document, on the
    same thread, still reads its commit: documents, catalog, page
    count, free list;
  - versions kept for a read: two per page beside twenty commits,
    still there after a checkpoint, gone after the read ends and the
    next checkpoint;
  - `compact` is refused by a kept snapshot, at the last commit or an
    earlier one, with nothing changed and writes going on; and waits
    for a read under way;
  - **a model**: 400 random steps, five settings of cache and
    checkpoint threshold: batches of inserts, updates and deletes of
    documents from a few bytes to several pages, up to four snapshots
    taken and let go, checkpoints, compactions. After every step each
    open snapshot reads exactly the copy of the database made when it
    was taken, through the primary index and a secondary one;
  - the readers of §79.8's stress test also keep a read open over many
    commits and find the same documents at its end.
- `export.rs`: an update made while an export runs is done before the
  export is, and isn't in it. This test used to show the opposite.
- The concurrent tests ran a hundred times over, debug and release,
  without a failure.
- Checked by breaking it on purpose, fifteen ways, each failing a test:
  no version kept of the page as found; that version under the
  commit's own number; `prune` keeping only the newest, keeping
  everything, and keeping a version for a snapshot at the next one's
  commit; a snapshot handed the oldest version at or before it; a file
  read not looked at again; a checkpoint not counted; a checkpoint
  forgetting what open snapshots read; a page with one version, in the
  file, kept in memory; a page changed again not counted as waiting;
  the replaced snapshot not remembered; an ended snapshot still
  counted; `compact` not waiting for reads; `compact` not refused by a
  kept snapshot.
- One breakage passes every test: the scan for damaged pages not
  looking again. A half-written page under a read can't be made to
  happen on demand.

## 80.8 Limits
- **No bound on the memory versions take**: §83. A read is as long as
  one call, so it's the pages changed during that call.
- **Versions outlive their reader** until the next commit of their page
  or the next checkpoint (80.3): at most the pages changed since the
  last checkpoint, once over.
- **Every first write of a page in a batch reads the committed page**,
  for `before`: from memory, unless the batch writes a page it never
  read, as `free_page` does, and the cache doesn't hold it.
- **Two readers do a tenth fewer short reads** (80.6). With §82's
  kept snapshots counted, `compact` could tell them from reads under
  way without the gate, which would take one of the three places off
  the read path.
- **`compact` behind a long read** makes new reads wait as long: a
  waiting writer goes first on `std`'s `RwLock`.
- **"Snapshot too old"** (§78.2) is left for a page whose `before`
  couldn't be read, and for §83.
