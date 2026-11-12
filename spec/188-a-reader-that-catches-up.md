# 188. A reader that catches up (`database.rs`, `durability/wal.rs`)

Step 3 of §185.8. Until now a reader beside a writer (§187) built each
call's snapshot anew: the file opened, the whole WAL read twice, the
catalog loaded, and its cache of the file empty again. Now it keeps the
store it built, and the next call reads only the batches the WAL gained
since, as one more commit over those before. Where it can't, it starts
again as before.

## 188.1 What the reader keeps
`ReaderSide::kept` (`database.rs`), behind a mutex, from one snapshot
to the next:
- **the store:** the file, the cache of what was read of it, and the
  WAL's batches read so far, as commits in memory;
- **the catalog** of its last commit;
- **where it stopped in the WAL** (`WalPlace`): the WAL's header as it
  was, where the last whole batch read ends, and that batch's last
  record, where it starts and its first 8 bytes, its length and
  checksum (§185.4);
- **the file's id and session,** as the header on disk said when the
  store was built (§183);
- **the snapshots handed out,** weak, as `Writer::past` (§80).

Each snapshot holds the readers' lock as before (§187.1), and the
store's pages; the mutex is held only while a snapshot is made.

## 188.2 Catching up, or starting again
Under the readers' lock, so nothing writes the file meanwhile:

1. **The file:** its header on disk read again. Every checkpoint writes
   the header first, with the session of the batches it writes back
   (§183), so another session there means the file is not what the
   store read of it, and its cache is stale: start again. A checkpoint
   that failed halfway has written the header too.
2. **The WAL** (`durability::read_pending_since`): start again if it is
   shorter than where the reader stopped, its header says another
   version or binding, or the last record read has other bytes now.
   Else its whole batches past that point are read, and nothing before.
3. **The new batches** (`FileStore::catch_up_in_memory`): checked as a
   recovery checks them, every page within the file as the header
   before it counts them, the store's header at first; then the latest
   image of each page made the next commit, the header the newest image
   of it. For the snapshots still open, the pages as they were are kept,
   as a writer's commit keeps them (§80): those the WAL had already are
   versions in memory; one from the file is read before the commit.
4. **The catalog** loaded again, if anything new was read.

A reader that read nothing of the WAL, because it was empty or had no
whole batch, has no record to look at again: then the WAL is read whole,
and step 1 is what makes sure that no checkpoint came in between.

**Starting again** is §187's build: the file opened, the whole WAL read,
the catalog loaded, a new store. The snapshots of the old one keep it
for as long as they live. Any error while catching up also drops the
kept store, so the next try, through the gate as before (§187.2, 5 and
6), starts again.

## 188.3 Decided while building
1. **Page images in memory, not places in the WAL.** §185.2 and
   §187.2, 4 had step 3 keep where each page is in the WAL and read it
   from there. Kept as images: the latest of each page, as a recovery
   into memory keeps them (§88.2), now read once each, not every call.
   The reader's memory for them is as large as the writer's own for the
   same pages, which wait in memory for a checkpoint in the same way
   (§185.5). Places would mean a version kind of their own in `Pages`,
   read from the WAL under the readers' lock; worth it only if a reader
   beside a large WAL shows the memory matters.
2. **The file checked by its header's session,** not by the WAL alone.
   §185.4 has "same sessions and its last record as it was". The WAL's
   session says no checkpoint came between two batches read; it says
   nothing where the reader had read no batch. The header on disk says
   it in both cases, for one page read per call.
3. **A reader's snapshots share one store,** so the snapshot memory
   limit applies to them as to snapshots in one process (§83): a reader
   that keeps a snapshot while the WAL grows past `snapshot_memory` of
   older versions ends it with `Error::SnapshotTooOld`. Each had a store
   of its own before, which never ended, and held the whole WAL each.
4. **One snapshot made at a time** in a process: the mutex is held from
   the readers' lock taken to the snapshot made. Threads that come
   together wait for each other's catching up, which is short, rather
   than each read the same batches.
5. **A file replaced under a reader** is noticed when its WAL changes,
   which a writer of the new file does at its first batch: the WAL then
   names another file, and the reader starts again, where §187.2, 7
   checks the format. Until then it reads the file it opened, as it was.

## 188.4 Tests
- `a_reader_catches_up_with_the_new_batches_only` (`database.rs`): a
  reader built on an empty WAL catches up into the same store, batch by
  batch; a snapshot it keeps reads its own state meanwhile, a page from
  the file that a later batch changes included, and a collection made
  since missing; after a checkpoint the reader starts again.
- `a_reader_starts_again_after_a_checkpoint_it_could_not_see_in_the_wal`:
  a reader that read no batch, its cache holding a page the checkpoint
  then wrote, reads the new state, not the page it cached.
- `a_reader_reads_on_only_in_the_wal_it_read` (`wal.rs`): only what is
  past the place; nothing new read as nothing; a record read and taken
  back, the WAL shorter, then another in its place; a header naming another file; a WAL
  cut, and as long again with other records under the same header; a
  WAL gone.

The tests of §187 run against the reader that catches up as they stand.

Mutations, each caught: no check of the file's header; never catching
up; no check of the last record; no check of the WAL's length (caught only
once the test had a WAL cut back by a log taken back); no check of its
header; no pages as they were for open snapshots; no snapshots
counted as open; a snapshot not registered; the catalog not loaded
again; the new batches read from the WAL's start; the last record not
carried over a read with nothing new; the header not taken from a new
batch's image.

Not caught, by its nature: the place not moved on after reading. The
batches are then read again next time, the same images over the same
pages, which costs only time.
