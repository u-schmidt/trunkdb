# 81. A failed log, taken back alone (`durability/`, `database.rs`)

A fix to §51. When a batch's `log` failed, `transact` emptied the WAL,
to make sure the failed batch couldn't be restored by the next `open`
(§19.6). Since §51 the WAL holds more than that batch: every batch
committed since the last checkpoint, and it is the only durable place
they are in. Emptying it took them along. They were still in memory,
and the next checkpoint wrote them to the file; a crash before it lost
them, though each had been reported as committed.

It took two things at once, a failed write to the WAL (a full disk,
say) and a crash before the next checkpoint. Now the log is cut back
to where it was before the failed call, and no further.

The number is not the next one: §77 to §80 are taken, by work on
another branch, where this fix was first made. It is the same change
there, in code that has been rewritten around it, so merging the two
conflicts in `transact`, the tests' place in `database.rs`, this file
and the list in `spec/README.md`: in each, the branch's side is the
one to keep.

## 81.1 Why it was right once
§19 wrote a batch to the WAL, then to the file, then emptied the WAL,
all in one commit. Between commits the WAL was empty, so "truncate the
log" and "take the failed record back" were the same thing. §51 moved
the write-back to a checkpoint, a thousand pages later by default
(§53), and left this path as it was: 51.3 says poisoning remains "for
a failed `log` that can't be undone", and the undoing still emptied
everything.

## 81.2 The fix
`Durability` gains `undo_failed_log`: cut the log to what it held
before the last `log`, if that call failed.

- `WalDurability::len` is the file's length as of the last `log` that
  succeeded, or the last checkpoint (§76 added it, for the size limit).
  A failed `log` doesn't move it. `undo_failed_log` sets the file's
  length to it and flushes.
- After a failed first log since a checkpoint that is 0: the header,
  written with the first record, goes too, and the next log writes it
  again.
- `transact` calls it where it called `checkpoint`. If it fails too,
  the batch's fate is unknown and the database is poisoned, as before
  (§19.6).

Rejected: writing the waiting pages back first and then emptying the
WAL, a checkpoint in the middle of handling an I/O error, on a disk
that has just refused a write.

## 81.3 Tests
The WAL takes a test-only fault: a `log` that writes its record whole
and then fails, as a failed flush would leave it; and an
`undo_failed_log` that fails.

- `wal.rs`:
  - after two good logs, a failed one leaves three complete records in
    the file; taken back, the file is byte for byte what it was, and a
    batch logged after it is recovered behind the two;
  - a failed first log is taken back to an empty file, and the next log
    is recovered;
  - after a good log, taking back changes nothing.
- `database.rs`:
  - two batches committed and not written back, then one whose log
    fails: the WAL is as long as before it, the failed batch is rolled
    back, and a copy of the file and the WAL as they are on disk, opened
    as after a crash, has both batches and not the third. A batch
    committed afterwards is there too after the next crash;
  - a failed log that can't be taken back: the next write and the next
    read are refused.
- Checked by breaking it on purpose, five ways, each failing a test:
  the log emptied, as before this section (the WAL goes from 49,212
  bytes to 0 on the failed log); nothing cut; the failed log not taken
  back; no poisoning when taking back fails; a failed record counted
  into the log's length.

## 81.4 Limits
- **A failed flush of the WAL may mean more than one lost record.** On
  some systems a failed `fsync` drops what it couldn't write and
  reports the error once. Records flushed before are safe; this
  section is about them.
- **Not seen in use:** no report of lost data led here, only reading
  the code.
