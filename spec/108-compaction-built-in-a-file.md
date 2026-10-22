# 108. Compaction built in a file (`compact.rs`, `storage/scratch.rs`, `storage/pages.rs`, `storage/file.rs`, `durability/wal.rs`)

The last of the three steps of §90. After the first two, crash recovery
took a few megabytes whatever the WAL's size, but `compact` still held
its whole new image in memory, 1.2× the new file: `rebuild` built it in
a `MemoryStore`, `replace_all` staged every page of it, and the commit
handed them to `Pages`, where they stayed until the checkpoint. A file
that compacts to 4 GB needed about 5 GB of memory.

## 108.1 The image in a file
`rebuild` builds into a `ScratchStore` instead: a `PageStore` over a
file beside the database, `<database>.compact`. The file is laid out as
a database file is, each page at its id's place with its checksum, and
page 0 left for the header, which `FileStore` writes. So the image goes
into the database by copying its bytes, and a page read back from it
is checked as a page of the database is.

A compaction changes the same page many times: the data page it fills,
the leaf an index grows at, the branch above it. A `ScratchStore` holds
the pages written lately, up to 2,048 of them, 16 MiB, and when it
reaches that writes the older half out, in id order, and lets them go.
Data pages and leaves fill one after another, so the pages written
again are the last few, and nearly every page reaches the file once.
`finish` writes what is still held and hands the image on as a
`Scratch`.

The file isn't flushed: nothing relies on it surviving a crash. The WAL
does, as before.

**No file left behind.** `ScratchStore::create` removes the file's name
as soon as it has opened it: the file lives as long as its handle, and
a crash leaves nothing. Windows can't remove the name of an open file,
so there the file is opened to be deleted when it's closed
(`FILE_FLAG_DELETE_ON_CLOSE`), which a crash does too. A file left
under that name some other way is emptied and reused.

**On Windows, a name nobody holds.** An image is read from until its
checkpoint, and in a small database that can be many commits after
its compaction (§108.3). A compaction in between makes its image under
the same name. Where the first image's name is gone, that is a new
file. On Windows it isn't gone: the second open was the first image,
emptied (`truncate`), and reads of the first image's pages failed at
the end of the file. CI's Windows tests found it, in the first build
of this section. So on Windows the file is made new (`create_new`)
under the first name nobody holds: `<database>.compact`, then
`.compact.1`, `.compact.2` and so on.

## 108.2 Logged from the file
`FileStore::replace_all` takes the `Scratch` and stages it: the header
with the image's page count and no free list in the dirty pages, the
image beside them. Whatever else the batch changed goes: the image
replaces it. `staged_pages` hands out the dirty pages and then the
image's, read from its file one at a time, and `transact` logs them
through `WalDurability::log_each`. It takes the pages as an iterator
with their count, gathers one record's pages (4,096, §91), writes the
record, flushes, and goes on. `log` is now `log_each` over a slice, and
writes the same bytes as before.

`log_each` fails if the pages don't come as counted, or a read fails.
`transact` then takes back what it wrote, as for any failed log
(§81), and the database is as it was.

## 108.3 Committed without its pages in memory
`Pages` gains a replacement (`Memory::replacement`), the image a
compaction committed. `commit_replacing` puts it in place of every page
past the header, and the header's new version in the versions as any
commit's. Everything else in memory is of the file being replaced, and
goes:
- the versions, waiting or not. A batch committed before the
  compaction and not yet written back is replaced along with the rest.
  The WAL holds both, in order, so recovery comes to the same file;
- the cache, whose pages are the old file's.

No snapshot is open and no read under way: `transact` made sure of both
before the log (§80.5), so there are no older versions to keep.

A read looks in the versions first, then in the replacement, then in
the cache and the file. It reads the replacement outside `Pages`' lock,
with a handle of its own: the image never changes, and the handle keeps
the file open whether or not a checkpoint has finished with it
meanwhile.

**The checkpoint** copies the image into the database a megabyte at a
time, checksums and all, then writes the header, cuts the file to its
page count and flushes. Then it lets the replacement go, and with it
the file. Until then the pages count as waiting (`unwritten`), so the
checkpoint follows the commit at once, as it did when the pages were in
memory. (That held while any image crossed the 1,000-page threshold;
since §123 raised it, a commit checkpoints while a replacement waits,
whatever its size.) A batch committed after the compaction and before the
checkpoint is read over the replacement and written after it.

**A failed copy** leaves the file half written and the replacement in
place: reads go on finding the image, and the next checkpoint copies
all of it again. That is what §51 does with pages in memory. `check`'s
scan of the file skips every page while a replacement waits, as it
skips any page waiting to be written.

## 108.4 Measured
A file of 2,064 MiB that compacts to 1,053 MiB: 517,000 documents of
2–6 KB, one index, every other one deleted. Release build, each step
its own process, peak memory footprint as macOS reports it, three runs
of each build alternating; the times are the compaction's own.

| | Before | Now |
|---|---|---|
| `compact`, no page cache | 1.25–1.26 GB, 11.2–13.0 s | 0.09–0.11 GB, 11.1–11.8 s |
| `compact`, 256 MiB cache | 1.54–1.55 GB, 11.0–11.5 s | 0.38–0.46 GB, 10.7–12.2 s |
| `open` after a kill right after compaction's log | 37 MB | 37 MB |

No sign of a cost in time: the image is written twice more than before
(to its file, and from it into the database), but in order, and the
pages that were copied in memory are now copied by the file system.

What remains without a cache is about 100 MB. One record of the WAL is
built at a time beside its pages, 64 MiB together; the image holds up
to 16 MiB; and each index's keys are sorted in memory, here 258,500 of
them.

Killed at five moments, with `kill -9`, then opened: while the image
was being built and while it was being logged, the open had the file
as it was; after the log, during the copy, it had the compacted file.
Every document was there each time, `check` passed, and no
`.compact` file was left.

## 108.5 Rejected
- **Building the new file beside the database and renaming it over
  the old one.** It writes the data once instead of three times, and
  needs no WAL record. But a database that replaces its own file loses
  what the old one had: other hard links to it, its owner and its
  permissions where another user runs the compaction, its ACLs. On
  Windows, renaming over a file that is open and locked is unreliable,
  and the lock would have to pass from the old file to the new one
  with nobody in between. The WAL keeps one way of replacing pages,
  tested at every crash point, on every platform.
- **Building the image in the WAL directly.** A page of the image is
  written many times while it's built, and the WAL is append-only,
  each record checked whole: the image has to be finished before it
  can be logged.
- **Copying into the database from the WAL** instead of from the
  image's file. As many bytes, but the WAL's entries carry their ids
  between pages, so it can't be a copy of whole megabytes; and the
  image's file is needed anyway, to build it.
- **Building the image at the database file's end**, past its pages,
  so that no other file is needed. The image's pages must end up at
  ids 1 and up, where the old file's pages are, so each would have to
  be moved twice.
- **A larger image in memory, bounded by the page cache's size.** The
  cache is for reads; a compaction gets no faster from holding more of
  its image, as its pages are written once each anyway.

## 108.6 Tests
`storage/scratch.rs`:
- Pages are allocated from 1 and read back as written, shared while
  held (§67); `finish` hands out every page in id order. Page 0, a
  page past the end, a page of the wrong size and a free are refused.
- Five times as many pages as are held, each written twice, the second
  time long after it left memory: at most 2,047 are held at any time,
  and every page reads back as last written, before `finish` and after.
- The name is gone at once (on Windows, once the image is dropped),
  and nothing is left in the directory.
- Three images made under one name and kept open at once each keep
  their pages, on Windows too; a file another program left under the
  name is emptied, or, on Windows, passed by and left as it was.
- `copy_into` puts pages 1 and up at their places with their
  checksums, more than a megabyte of them, and leaves page 0 and what's
  past the image alone. Stopped after 0, 1, 128 and all but one pages,
  it has written exactly those.

`storage/file.rs`:
- A committed replacement is read from its image while the file still
  has the old pages, and three pages wait; after the checkpoint the
  file has the image, cut to its length, and reads don't find the old
  pages the cache held.
- A copy that fails after one page leaves reads finding the image, and
  the next checkpoint finishes it.
- A batch after a replacement is read over it and written after it; a
  later checkpoint doesn't copy the image again.
- A batch waiting before a replacement is replaced along with the rest.
- `check`'s scan skips a waiting replacement, even one that grows the
  file.
- The tests of `replace_all` (§41.4), now with an image from a file.
  The restore test logs the image as `staged_pages` hands it out.

`durability/wal.rs`: `log_each` writes what `log` writes, byte for
byte, in one record and in several; more or fewer pages than counted,
or a page that fails to come, fail it, and `undo_failed_log` leaves the
WAL as it was, the batch before it recovered.

`compact.rs`: no `.compact` file after a compaction and its checkpoint,
nor after one refused for a duplicate; a compaction whose file can't
be made (a failure the test injects) fails with `Error::Io` and
changes nothing, on disk or after the next open. The other tests of §41 pass unchanged,
among them the compaction whose write-back fails twice, now read from
its image meanwhile, and completed by the next open.

Fuzzing: the `queries` target, which compacts among its operations,
2,000 random programs on stable.

## 108.7 Mutations
Each caught: reads that skip the replacement (16 tests); a replacement
committed beside the old versions (11), and beside the old cache (5);
a checkpoint that keeps the replacement after copying it (4), and one
that doesn't copy it (12); `check`'s scan reading a waiting
replacement (5); a replacement not counted as waiting (3); the writer's
own reads skipping the image while it's staged (1); the image left out
of the pages logged (16), and of their count (15); `finish` forgetting
the pages held (27); a spill that writes nothing (1); a copy that
stops a page late (1), and one that skips the last megabyte (13);
`log_each` taking more pages than counted (1), and marking its last
record `MORE` (25).

Two of these first hung a test instead of failing it: the readers of
`readers_see_whole_commits_beside_checkpoints` read until the writer
says it's done, and a writer whose `compact` failed panicked without
saying so. It says so on any end now, a panic included.

One changes nothing that can be seen: a spill that writes out the
newer half instead of the older. It only changes which pages are
written to the file more than once; every page is written before it
is let go, so every read is the same.

## 108.8 Limits
- **Disk:** until the end, the new file twice more beside the
  database: the image's file and the WAL. A file that compacts to 4 GB
  needs 8 GB free, besides itself.
- **Each index's keys** are sorted in memory, one index at a time: tens
  of megabytes for a few hundred thousand documents. An external sort
  would bound that too, if real files ask for it.
- **One WAL record** is built beside its pages: 64 MiB at most.
- **The write lock** is held throughout, as before (§41.5).
- `check` took 1.7–1.8 GB on the file above, before this step and
  after. Not part of compaction, but the next place found where memory
  grows with the file.
