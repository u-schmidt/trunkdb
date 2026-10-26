# 128. A checkpoint that fails after cutting the WAL (`durability/wal.rs`, `database.rs`)

A fix to §81. `undo_failed_log` cuts the WAL back to `len`, which
81.2 defines as the file's length after the last `log` that
succeeded, or the last checkpoint. A checkpoint does three things:
1. it cuts the file to nothing;
2. it moves to the start;
3. it flushes.

It set `len` to 0 only after all three. If the cut succeeded and the
move or the flush then failed, the file was empty but `len` still gave
the old length. `Writer::checkpoint` reports no error from the WAL
(the pages are already in the file), so writes went on.

A robustness review found it (2026-10-26) and showed what came next:
1. A batch's `log` fails, a full disk say: the case §81 is for.
2. `undo_failed_log` sets the file's length to the stale `len`, which
   grows the empty file back to its old size, filled with zeros.
3. The next batch logs fine, after the zeros.
4. The next open fails on the zeros ("WAL batch record too short for
   its page count"), and every batch after them, though each was
   reported as committed, is behind a WAL the open refuses to read.

It takes two failures in a row: the checkpoint's flush, then the next
log. The database then doesn't open, rather than opening without
those batches.

`log` itself was never affected: it starts at the file's end, read
from the file, and sets `len` from that when it succeeds.

## 128.1 The fix
- **`checkpoint` sets `len` to 0, and `appendable`, as soon as the cut
  succeeds**, before the move and the flush. The file is empty from
  then on, whatever fails next. If the cut itself fails, nothing
  changed and `len` is still right.
- **`log` keeps `len` within the file's end before it writes**, the
  end it already reads: `len` becomes the smaller of the two. A failed
  log leaves it there, so `undo_failed_log` never grows the file,
  whatever an earlier call recorded. Where `len` is the smaller, bytes
  past it are a failed log's not yet taken back, and are cut as
  before: `log_each_fails_on_pages_that_dont_come_as_counted` makes
  three failed logs and one undo, and takes all three back.

Either change alone fixes the case found. The first keeps `len` right
for what else reads it: the WAL size limit (§76) and `stats` (§93).
The second keeps undo from growing the file if the length kept
between calls is ever too long again in some other way.

A first version set `len` to the file's end outright. It took back
only the last of several failed logs, which that existing test
caught.

## 128.2 An unflushed cut
A cut whose flush failed may not have reached the disk. After a crash
the WAL can come back with its old records. They are page images
already written back to the file and flushed before the checkpoint
began (§51), so replaying them changes nothing. A batch logged after
the cut flushes the file, its new length included, so its record
can't sit behind an old one.

So treating the WAL as empty from the cut on is safe, and the
checkpoint's error is still not reported to the caller (§51).

## 128.3 Rejected
- **Poisoning after a checkpoint that fails:** the state is known after
  every step (128.1), and a full disk would stop every write until a
  reopen.
- **Returning the error from `Database::checkpoint`:** the pages are in
  the file, which is what it promises. A caller couldn't do anything
  with the error, and the WAL is emptied again at the next one.

## 128.4 Tests
- `a_checkpoint_that_fails_after_cutting_the_file_counts_it_cut`
  (`durability/wal.rs`): a three-page batch, then a checkpoint that cuts
  and fails. `len` is 0. A log that fails after it is taken back to
  the empty file, and the next log is what an open recovers. This is
  the review's case.
- `an_undo_cuts_back_to_where_the_failed_batch_began`: a `len` set
  past the file's end by hand. A failed log is still taken back to the
  bytes before it.
- `log_each_fails_on_pages_that_dont_come_as_counted`, as it was:
  three failed logs, one undo, all three gone.
- `a_failed_log_after_a_failed_checkpoint_loses_no_batch`
  (`database.rs`): the same through `Database`. A batch, a failed
  checkpoint, a batch whose log fails, then one that succeeds. After a
  crash the first and third are there and the second isn't, and
  `check` passes.

The new test hook is `failing_checkpoints`: a checkpoint cuts the
file, then fails as if its flush had. It stands for a failed move too:
both come after the cut, and `len` is 0 before either.

## 128.5 Mutations
| Mutation | Caught by |
|---|---|
| `len` set to 0 only after the flush, as before | `a_checkpoint_that_fails_after_cutting_the_file_counts_it_cut` |
| `log` doesn't keep `len` within the file's end | `an_undo_cuts_back_to_where_the_failed_batch_began` |
| `len` set to the file's end outright, not the smaller | `log_each_fails_on_pages_that_dont_come_as_counted` |
| Both: the code before | all three |

## 128.6 Limits
- No real failed flush was tested, only the injected one.
- A cut whose flush failed may come back after a crash; that is
  harmless (128.2).
