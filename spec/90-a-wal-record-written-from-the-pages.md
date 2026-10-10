# 90. A WAL record written from the pages (`durability/wal.rs`)

The first of three steps towards compaction, and crash recovery, in
memory that doesn't grow with the file (ROADMAP.md, "Storage and
durability"). Measured before it, on a file of 2,251 MiB that compacts
to 1,193 MiB (517,000 documents of 2–6 KB, one index, every other one
deleted; release build, each step its own process, peak memory as
macOS reports it):

| | Peak memory | × the new file |
|---|---|---|
| `compact`, no page cache | 3.89 GB | 3.3 |
| `compact`, 256 MiB cache | 4.18 GB | 3.5 |
| `open` after a kill right after compaction's log | 2.52 GB | 2.1 |

§41.5 counted the image twice: in the `MemoryStore`, and in the WAL
record. It was three times. `encode_record` built the record's body,
then copied the body into a second buffer to put the length and the CRC
in front of it, and `log` wrote that buffer. A file that compacts to
4 GB would have needed about 13 GB of memory to compact.

Reading the code for this found a worse fault. The record's length is a
`u32`, and `encode_record` wrote `body.len() as u32`, which cuts the
length to its low 32 bits without a word. A batch of more than 524,032
pages (4 GiB of them), such as the compaction of a file that is still
over 4 GiB afterwards, logged a record with the wrong length. Nothing
showed as long as nothing crashed, because the checkpoint empties the
WAL. After a crash between the log and that checkpoint, the record read
as torn or damaged. Either recovery refused the WAL, or it ignored the
record over a main file the write-back had left half done.

## 90.1 Written from the pages
`log` no longer builds the record. `write_record` works out the CRC in
a first pass over the pages, which are in memory anyway: the CRC heads
the record, so it has to be known before the body is written. A second
pass writes the length, the CRC, the count and each page straight to
the file, through a buffer of 1 MiB (`WRITE_BUFFER`). A large batch
takes one system call per megabyte, and a small one takes a single
write, as before.

The format is unchanged, byte for byte: `encode_record` remains for the
fuzz targets and the tests, and it writes through the same
`write_record`.

The buffer is flushed explicitly (`write_buffered`), not by its drop,
which ignores an error. A last write that failed, on a full disk say,
would otherwise have had `log` report the record durable when it isn't
whole.

## 90.2 Records over 4 GiB refused
`body_len` works out the body's length and refuses one that doesn't fit
the record's `u32`, with "a batch of N pages is too large for one WAL
record: at most 524032 pages", as `ErrorKind::InvalidInput`. `log`
checks this before anything goes into the buffer. The buffer's drop
would write what it holds, so a refused batch leaves the WAL exactly
as it was, not even a header added. `transact` rolls the batch back as
it does for any failed log. A `compact` of a file that would still be
over 4 GiB afterwards fails with that error and changes nothing, where
before it seemed to succeed while risking the file. Ordinary batches
are nowhere near the limit.

Lifting the limit belongs to step 2: recovery that reads the WAL record
by record also needs a batch to be able to span several records, marked
as one unit. That is a change to the WAL's format, which is cheap
because the WAL is empty after a clean close.

## 90.3 Measured after
The same file, the same steps:

| | Before | After |
|---|---|---|
| `compact`, no page cache | 3.89 GB | 1.40 GB (1.18×) |
| `compact`, 256 MiB cache | 4.18 GB | 1.70 GB |
| `open` after the kill | 2.52 GB | 2.52 GB (step 2) |

What remains for `compact` is the image itself, plus the page cache
when it has one. Time was dominated by a nearly full disk. In three
runs of each build, alternating, the old one took 6.3–13.6 s and the
new one 6.6–7.0 s, so there is no sign of a cost. The open after the
kill recovered every document and left the compacted file.

## 90.4 Rejected
- **Leaving the CRC at the end of the record**, so that one pass would
  do. That changes the format, the decoder and every test of a torn
  tail, for a pass over memory that costs little next to the write.
- **Raising the length to a `u64`** here: it changes the format too,
  and step 2 changes it anyway, for a better reason.
- **Writing every piece unbuffered**: two system calls a page, a
  million for a compaction of 4 GiB.

## 90.5 Tests
`durability/wal.rs`:
- A batch of 300 pages, over twice the buffer, is logged exactly as
  `encode_record` encodes it, with a batch after it. The WAL's length
  is counted right, and recovery returns both batches.
- `body_len` takes 524,032 pages and refuses one more. `log` refuses
  that many with `InvalidInput`, and the WAL is unchanged byte for byte:
  after a batch, and also on an empty WAL, which doesn't even get its
  header.
- A write that fails one byte before the end of a record fails
  `write_buffered`, for a batch smaller than the buffer and for one
  larger (`FullAfter`, a writer with room for so many bytes).

Mutations, each caught: no check before the buffer (the refusal test);
the length cut to 32 bits again (the same test); the CRC without the
page count (20 recovery tests); the WAL's length without its header
(three); no explicit flush (the full-disk test); a limit one page lower
(the refusal test).

## 90.6 Limits
- The batch's pages are still all in memory, and for `compact` that is
  the whole new image: step 3 builds it in a file instead.
- Recovery still reads the whole WAL, and then copies its pages: about
  2× the WAL (step 2).
- No batch, `compact` included, can be over 4 GiB of pages until step 2.
