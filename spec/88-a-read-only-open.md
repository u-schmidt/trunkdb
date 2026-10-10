# 88. A read-only open (`database.rs`, `storage/file.rs`, `durability/wal.rs`, `src/bin/trunkdb.rs`)

`trunkdb info` on a file whose program had crashed recovered the file:
it wrote the WAL's batches back, emptied the WAL, and on an old file
rebuilt indexes and stamped the format. A tool that only looks should
not do that: maybe the file is the evidence, or a backup on a mount
that can't be written, or the program will open it again and is the one
that should recover it. `OpenOptions::read_only(true)` opens a database
without writing to it, not even what an open does by itself.

## 88.1 What it does
- The file is opened for reading only, and never created: a missing one
  is `ErrorKind::NotFound`, not a new database. The WAL is read if it
  is there and created if it is not. The file only has to be readable.
- The lock is shared (`File::try_lock_shared`, stable since 1.89, the
  MSRV). Any number of read-only opens hold it together. An open that
  writes takes it exclusively as before (§21.1), so it fails beside a
  read-only one with the same `WouldBlock`, and a read-only one fails
  beside it.
- Every write fails with the new `Error::ReadOnly`, and nothing is
  changed. That includes `checkpoint` and `compact`, an empty
  `write_batch`, and an `ensure_index` of an index that exists: they all
  go through `Database::write`, which refuses before anything else. That
  is one check for every write path, `transact` included. Reads,
  snapshots, `check`, `file_info` and `export` work as ever.
- Dropping the last handle doesn't checkpoint.

## 88.2 Recovery in memory
An open that writes does three things before the database is usable:
it writes the WAL's batches back to the file (§19), bootstraps the
catalog page of an empty file (§22.1), and keys the ids an index of a
format 6–9 file lacks (§62). A read-only open does all three, but keeps
the result in memory:

- `durability::read_pending` reads the WAL's batches and keeps no
  handle. A torn tail is ignored and left where it is.
- `FileStore::restore_in_memory` checks them exactly as
  `restore_pages` does. Both call one `check_restorable`: damaged
  header fields need a header image in the WAL (§85), no page may lie
  past the end of the file (§55), and no header may count more pages
  than the file and the WAL can account for (§87). Then it hands the
  latest image of each page to `Pages::commit` as one commit. That is
  where committed pages wait for a checkpoint anyway (§51, §77), and
  readers find them there. The header becomes the WAL's image of it,
  if the WAL has one.
- The catalog loads in a batch, as before (`Database::load_catalog`,
  now shared by both opens). If the batch changed pages, they are
  committed to memory the same way, without logging them.

No checkpoint ever runs, so these pages are never written back.
`Pages::damaged` already skips pages that wait for a checkpoint, so
`check` reports none of them. The WAL is untouched, and the next open
that writes recovers from it as it would have.

So a read-only open shows what an open that writes would show, byte
for byte, and the file can't tell that it was opened.

The `trunkdb` command opens the file read-only for `info`, `check` and
`export`, and read-write for `compact` and `import`. Its usage text
says so.

## 88.3 Rejected
- **Refusing to open when the WAL holds batches**, with an error
  saying "open it read-write first". It's simpler, but it fails exactly
  when a tool is needed most: right after a crash, before anything is
  touched.
- **Refusing format 6–9 files with indexes**, instead of keying them in
  memory. Also simpler, but such a file reads wrongly only through the
  index, and an inspection tool shouldn't send you to a write first.
- **Reading the file without any lock.** A program that writes could
  be checkpointing beneath the reader, so its pages would be half old
  and half new. The shared lock costs nothing here: the reader can't
  open the file while a writer has it, and the writer can't while a
  reader does.
- **`checkpoint` returning `Ok(())` on a read-only database.** It
  promises that the file is complete without its WAL, which it can't
  make true here, so it fails like every other write.
- **A flag on `Shared` beside the WAL**: a missing WAL (`Writer::durability`
  is `None`) is what makes a database read-only, so there is one fact,
  not two that could disagree.

## 88.4 Tests
`database.rs`:
- Nothing written. For a clean file, for each of three crash points
  (after the log, in the middle of the write-back, before the
  checkpoint), for a torn WAL tail, and for damaged header fields
  restored from the WAL, a read-only open reads everything there is to
  read (`file_info`, `check`, `export`, every collection, a snapshot,
  a clone, a second read-only open). The file's and the WAL's bytes
  stay the same, before the drop and after it. The WAL's batches can be
  read, and the next open that writes recovers them and empties the
  WAL.
- A missing file gives `NotFound`, and nothing is created in the
  directory. An empty file opens as an empty database and stays an
  empty file, with no WAL.
- Thirteen writes all fail with `Error::ReadOnly`: `write_batch`, an
  empty one, `insert`, `update`, `delete`, `delete_many`, an
  `ensure_index` that exists, `drop_index`, `drop_collection`, a
  `Batch`, `import`, `compact` and `checkpoint`. The bytes stay the
  same.
- Locks: read-only beside a writer fails, two read-only opens go
  together, and a writer fails beside either of them.
- Unix: a file of mode 444 in a directory of mode 555 opens read-only,
  and an open that writes fails on it.

`collection.rs`: a format 9 file whose index lacks id keys, opened
read-only twice. It is keyed in memory both times, the index finds the
documents, `check` passes, and the file stays byte for byte the same.

`storage/file.rs`: `restore_in_memory` refuses a page past the end and
a header over the bound, restores a grown file to memory (the latest of
two images of a page wins), and writes nothing.

`tests/cli.rs`: `info`, `check` and `export` work beside a read-only
open, and `compact` is refused as "in use".

Mutations, each caught: the exclusive lock instead of the shared one
(six tests); the file opened for writing too (the mode-444 test);
`restore_pages` instead of `restore_in_memory` (two); the WAL opened
and created by `WalDurability::open` (two); no `ReadOnly` check in
`write` (the write test); a `Drop` that checkpoints (the `expect` in
`wal` panics in the drop, which aborts the test binary); the header not
taken from the WAL (three); `restore_in_memory` without the checks, and
with the first image winning instead of the latest (the store test);
the open's own batch rolled back instead of committed (the empty-file
and old-format tests).

## 88.5 Limits
- The locks are advisory and don't wait. A tool that has the file open
  keeps the program that writes it from opening it, and a program that
  has it open keeps the tool out. Reading beside a running program is
  what a snapshot in the same process is for (§82), not this.
- The WAL's pages are held in memory for as long as the database is
  open. That's bounded by the WAL's size: about 16 MB at the
  `checkpoint_wal_bytes` default (§76), more if the program raised it
  or crashed in the middle of one large commit.
- An old file's indexes are keyed again at every read-only open, and
  `file_info` reports format 11, which is the format in memory, not
  the one on disk.
- The file is opened read-only, but a file system that changes the
  access time on reads still does so; that is metadata, not content.
