# 91. A batch in several records, and recovery a record at a time (`durability/wal.rs`, `storage/file.rs`, `storage/pages.rs`)

The second of the three steps of §90. After the first, the open after a
crash still took 2.1× the WAL's size in memory: `WalDurability::open`
read the whole file with `read_to_end`, and `decode_pending` copied
every page out of it into a list. Measured on a WAL of 1,194 MiB, a
compaction killed right after its log, that was 2.52 GB. And no batch
could be over 4 GiB of pages (§90.2), because a batch was one record
and a record's length is a `u32`.

Both come from the same rule: one batch, one record. A record is only
valid once its CRC has been checked, which needs all of it, so recovery
can't hold less than the largest record.

## 91.1 A batch may take several records
`log` puts at most `RECORD_PAGES` pages into a record: 4,096 pages,
32 MiB. A larger batch goes out in several records. Each record's body
gains a `u32` of flags after the page count, under the CRC, and the
flag `MORE` says that the batch goes on in the next record. The last
record of a batch doesn't have it.

Recovery restores a batch whose last record it finds whole, with every
page of every record in it. A batch that ends before its last record,
or whose last record is torn, is left out entirely, intact earlier
records included. So a batch is still restored whole or not at all.

**The flush between records.** `log` flushes the file after every
record of a batch before it writes the next one, and once more after
the last. That keeps the rule of §19 sound: a crash tears the tail of
the WAL and nothing before it. Without it, the last record of a batch
could reach the disk before an earlier one. The batch would look whole
and have a damaged record in its middle, and recovery could not tell
that from real damage. With it, a damaged record that has another one
behind it is damage, inside a batch as between batches, and is an
error as before.

A batch of up to 32 MiB is one record, written with one write and
flushed once, exactly as before. A larger one pays one flush per
32 MiB.

Each record is built in one buffer that `log` reuses, so `log` holds
one record beside the pages, whatever the batch's size. That replaces
the two passes and the 1 MiB buffer of §90.1. The refusal of §90.2 is
gone with its reason: no record is ever over 32 MiB, and a batch has no
limit.

## 91.2 The format: WAL version 3
The header says version 3. A record is
`[u32 body_len][u32 crc32(body)][body]` as before, and the body is now
`[u32 page_count][u32 flags]` and the entries.

- A flag this build doesn't know is an error, not ignored: a later
  build meant something by it.
- **Version 2 is still read**: a record a batch, no flags. So a WAL
  that a crash left under an older build is recovered by this one.
- A version 2 file is never added to. `log` refuses to append to one,
  and also behind a header a crash cut short. `Database::open` always
  checkpoints, which empties the file, before it logs anything, so only
  a caller of `WalDurability` that skips the checkpoint meets that
  refusal.
- An older build refuses a version 3 WAL with its existing
  "unsupported WAL version" error. That needs a downgrade right after
  a crash; after a clean close the WAL is empty.
- The database file's format is unchanged, 11.

## 91.3 Recovery, a record at a time
`Records` reads a WAL record by record through a buffered reader and
holds one record. Recovery goes through the file twice.

1. **`Pending::scan`**, at open. It checks every record's CRC, finds a
   torn tail, and finds where the last whole batch ends. It keeps no
   pages: only the page id of every image in the whole batches (8 bytes
   a page, 4 MB for a WAL of 4 GiB) and the images of the header page,
   one per batch that changed the header.
2. **`PendingPages::pages`**, at the restore. It reads the file again,
   up to the end of the last whole batch, and hands out each page as it
   comes. It fails if it finds fewer pages than `scan` did.

`FileStore` takes what it restores as a `PendingPages`, a trait in
`storage`, so that storage doesn't depend on the WAL: a count, an
`outline` (every image's page, with the image where it's the header's)
and `pages`. `check_restorable` runs the checks of §55, §85 and §87 on
the outline, before anything is written, as before. `restore_pages`
then writes each page as `pages` hands it out. For tests, a list of
`PageImage`s is a `PendingPages` too.

The read-only open (§88) uses the same reader. It still keeps the
restored pages in memory, which is what it's for, but only the latest
image of each page, where it held the file, the decoded list and the
map.

## 91.4 Measured
The file of §90: 2,251 MiB, compacting to 1,193 MiB. Release build,
each step its own process, peak memory as macOS reports it.

| | §90 | Now |
|---|---|---|
| `open` after a kill right after compaction's log (WAL 1,194 MiB) | 2.52 GB | 37 MB |
| `open` after a kill a third of the way into the log (WAL 384 MiB) | – | 36 MB |
| `compact`, no page cache | 1.40 GB | 1.41 GB |
| `compact`, 256 MiB cache | 1.70 GB | 1.71 GB |

The open after the whole log recovered every document and left the
compacted file. The open after the partial log left the file as it was
before the compaction, with every document. Compaction's log now takes
38 records and 38 flushes. Its time stayed within the noise of a nearly
full disk: 7.1, 7.4 and 15.0 s in three runs without the cache, against
6.6 to 7.0 s before.

## 91.5 Rejected
- **No flush between records, and a damaged record inside the last
  batch treated as a torn tail.** A record whose CRC fails has a length
  that can't be trusted either, so what follows it can't be found: the
  reader couldn't tell "inside the last batch" from "before a later
  one".
- **A checksum over the whole batch in its last record.** It detects
  the reordering but needs the same guess about where the batch ends.
- **One pass, restoring as it reads.** The checks of §55, §85 and §87
  come before any write, and a batch must be known whole before any
  page of it is restored. Both need the WAL read to its end first.
- **Keeping version 2 unreadable.** Cheap to read, and the alternative
  is a database that won't open after an upgrade that followed a crash.

## 91.6 Tests
`durability/wal.rs`, with a test-only record size of three pages:
- A batch of one record costs one flush. One of eight pages goes out in
  three records, flagged `MORE`, `MORE`, none, with three flushes; the
  file is exactly header and batches as `encode_batch` encodes them;
  it is recovered whole with the batches before and after it, and the
  outline shows every page and the header's image alone.
- That batch cut at five places (inside the first record, after it,
  inside the second, after the second, one byte short), and with its
  last byte damaged: none of it is recovered, the batch before it is,
  and its header image is not in the outline.
- A damaged record of a batch with another behind it is an error.
- A version 2 WAL of two batches and a torn third is recovered. `log`
  refuses to add to it and leaves it byte for byte; after a checkpoint
  the file starts with a version 3 header. Versions 1 and 4 are
  refused.
- A torn header holds nothing and isn't written behind.
- A record with an unknown flag and a valid CRC is an error.

`database.rs`: a compaction logged two pages a record, more than 20
records. From the whole WAL the next open has the compacted file. From
the WAL cut at five places it has the file as before the compaction,
with every document, and `check` passes.

`storage/file.rs`: the tests of §55, §85, §87 and §88 pass unchanged,
through `PendingPages`.

Fuzzing: `recover_wal` now also builds its batch in records of one to
seven pages, and the seed corpus has one in two records. Three minutes,
3,903 inputs, found no crash.

Mutations, each caught: no flush between records; `MORE` never set, and
set on the last record too; a batch without its last record restored;
the ids of a torn batch kept; unknown flags ignored; a version 2 file
added to; the restore reading past the last whole batch; the record
size not respected. One changes nothing that can be seen: keeping the
header images of a torn batch. The outline shows a header image only at
a place within the ids, and a torn batch's ids are cut off, so dropping
those images only frees their memory.

## 91.7 Limits
- Compaction still holds its new image in memory, 1.2× the new file
  (step 3).
- The read-only open keeps the WAL's pages in memory (§88), once each.
- A version 2 record is read whole, however large: only a WAL from an
  older build has one over 32 MiB.
- A WAL of several GiB is read twice at the open after a crash.
- A batch over 32 MiB costs one flush per 32 MiB.
