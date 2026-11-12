# 187. Readers beside a writer, built (`storage/lock_files.rs`, `database.rs`)

Steps 1 and 2 of §185.8, built together, since format 17 is worth a
release only with the feature it makes room for. §185 is the design;
this says what was built, and where the building decided what §185
left open or changed it.

## 187.1 What was built
**The lock files** (`storage/lock_files.rs`): `<path>.writer`,
`<path>.readers` and `<path>.gate`, made with `open_or_create`, so a
new one has its directory synced (§182), and never removed. A reader
in a directory it may not write opens them for reading only, where
they are there. A bounded wait is `try_lock` again and again, the pause
doubling from 0.2 ms to 10 ms.

**Format 17.** No page changes. `FORMAT_VERSION` is 17, and 16 is among
the formats opened as they are. A writer's open stamps it, through the
same batch that stamps every older format (§59), so it is in the file
itself before the open lets go of the file. A read-only open stamps
nothing, as ever (§166.3).

**A writer's open,** in the order of §185.6: `.writer` exclusively,
then the file exclusively as before, then the other two lock files.
The readers' lock is taken through the gate before the first write of
the file, and only if there is one: recovery, a fresh file's catalog,
an older file's keys or its stamp. At the end the open lets go of the
file (`FileStore::unlock_file`).

**A read-only open** reads the header's format without a lock
(`FileStore::peek_format`). At 17, with the lock files there or made,
it goes by them. Else it holds the file shared, as §88 has it, and
having it, looks once more: at 17 with lock files by now, it lets go
and goes by them.

**A reader's snapshot** (`ReaderSide` in `database.rs`): each read, and
each `snapshot` or cursor, passes the gate, holds the readers' lock,
opens the file without a lock and builds a store as a read-only open
does (§88.2): the WAL's whole batches over the file, the catalog
loaded, all in memory. The snapshot carries those pages and its share
of the lock (`Committed::own`, `_hold`); `Committed::reading` reads its
own pages where it has them. Nothing else in the read paths changed:
they all go through `Database::read`, `read_at` and `pin`.

**A checkpoint** (`Writer::checkpoint`) takes a `Turn`:
- `Due`, from a commit: tries the readers' lock at once; if a reader
  holds it, the checkpoint is left and the next one goes through the
  gate, waiting `OpenOptions::checkpoint_wait` (50 ms unless set).
- `Asked`, from `Database::checkpoint` and the last handle's drop:
  through the gate at once. If a reader stays, `Database::checkpoint`
  fails with `WouldBlock`; the drop leaves the WAL for the next open.
- `Held`, for `compact`, which takes the readers' lock itself.

Every checkpoint left is counted in `SnapshotInfo::checkpoints_skipped`,
with the longest WAL one left in `largest_wal_skipped` (§185.5).

**`compact`** tries the readers' lock beside its check for kept
snapshots, and is refused with `Error::SnapshotOpen` the same way. It
holds the lock until its checkpoint has copied the image in.

## 187.2 Decided while building
1. **No field for the lock protocol** (§185.6 left it to now). A later
   change to the protocol has to keep this release out as 17 keeps
   older ones out, and raising the format does that. A field would
   only spare a bump for a change older protocols could take part in
   unchanged, and then nothing needs to be marked at all.
2. **A commit waits at the gate at most once a second.** §185.4 has
   the commit after a left checkpoint go through the gate. Beside a
   kept snapshot every commit has a checkpoint due, and each would wait
   `checkpoint_wait`. So after a wait at the gate that found a reader
   still there, a due checkpoint only tries, for a second
   (`GATE_AGAIN_AFTER`): a kept snapshot slows the writer by 50 ms a
   second at most, and readers that read per call still get the gate
   every second.
3. **`Database::checkpoint` beside a reader fails** with `WouldBlock`,
   after `checkpoint_wait`. Its docs promise a file complete on its own
   afterwards; a checkpoint quietly left would break that.
4. **A reader's memory holds the WAL's page images,** the latest of each
   page, as `restore_in_memory` does (§88.2), not places in the WAL as
   §185.2 has it. It is as large as the writer's own pages waiting for
   a checkpoint, which grow the same way beside a long reader (§185.5),
   and each call reads the whole WAL twice, once to check it and once
   for the pages. Step 3, the reader that catches up, is where places
   pay: it reads only what is new, and keeps what it had.
5. **On Windows, a reader whose reads stay refused gives up the call,
   not only its snapshot.** §185.6 has it begin the call again. A read
   refused for a second (`REFUSED_FOR`) fails with
   `Error::SnapshotTooOld`; the call returns, its share of the lock with
   it, and the open waiting for readers goes on. Beginning the call
   again inside trunkdb would mean a retry around each of the twenty
   places that read. While the snapshot is still being built, it is
   begun again: the reader lets go and waits at the gate the open has
   shut, for up to 30 seconds (`BUILD_REFUSED_FOR`).
6. **A reader reads again three times** (`READ_AGAIN`) before it
   reports damage: any error but a refused read, from the file or the
   WAL.
7. **A file replaced under a reader:** a snapshot built from a file
   that no longer says 17 fails, saying so, rather than reading a file
   an older release may write.
8. **A read-only open may make the reader's two lock files.** §88 wrote
   nothing at all; now nothing is written to the database or its WAL,
   and the lock files are made where they aren't there and can be.
9. **`verify_export`** makes the lock files of the database it restores
   into with `create_new`, as it does that file and its WAL, and removes
   them with the rest (§182.2). Nobody else opens a database made to be
   removed.
10. **A writer's open of another program's file** leaves `<path>.writer`
    behind: it is made before the file is looked at, and never removed.
    The file itself is untouched, as before.

## 187.3 Step 4 came along
`Database::snapshot` on a reader beside a writer, and a cursor, take
their snapshot through `pin`, which builds one as a read does. So a
snapshot a reader keeps works already, as §82 has it in one process:
it holds the readers' lock for as long as it is kept, and keeps every
checkpoint out meanwhile (§185.5, 1). On Windows a kept snapshot that
reads while a writer's open holds the file fails with
`Error::SnapshotTooOld`, as §185.6 says. What is left of §185 is step 3,
the reader that catches up, and step 5, a reader too long ended.

## 187.4 What changed for callers
- A read-only open goes beside a writer of a file at 17, where it
  failed with `WouldBlock`.
- An open that writes beside a reader of a file at 17 succeeds, where
  it failed. One that has to write the file waits for the readers up to
  `OpenOptions::wait_for_readers`, 5 seconds unless set, then fails with
  `WouldBlock`, having written nothing.
- A second writer is refused at `.writer`, with `WouldBlock`, as before.
- `Database::checkpoint` can fail with `WouldBlock` beside a reader.
- `SnapshotInfo` has two fields more; it is `#[non_exhaustive]`.
- `OpenOptions` has `checkpoint_wait` and `wait_for_readers`.
- The `trunkdb` command's `info`, `check`, `export`, `find` and the
  rest go beside the application, and its writes beside a reader.
- Releases up to 0.39.0 refuse a file this release has opened to write:
  back is by export and import (§30).

## 187.5 Tests
In `storage/lock_files.rs`: one writer at a time; the last of a
process's snapshots lets the readers' lock go; through the gate, a
reader is waited for a bounded time and the gate opens again either
way; a new snapshot waits at a shut gate though its process holds the
readers' lock; a reader that may not write the directory takes part
with the lock files a writer made, and without them gets none.

In `database.rs`:
- `a_file_at_16_is_read_as_before_until_a_writer_stamps_17`: read-only
  opens of a file at 16 go together and keep a writer out; the writer
  alone stamps 17 into the file, and a reader goes beside it.
- `a_reader_beside_a_writer_reads_what_was_committed_before_each_call`:
  from the WAL, from the file after a checkpoint, after the writer is
  gone; a second writer refused; the lock files made at the open.
- `a_kept_snapshot_keeps_checkpoints_out_and_the_wal_grows`: eight
  checkpoints left and counted, the WAL's length recorded; the snapshot
  reads its own state, a new read the newest; the next commit after it
  is dropped checkpoints.
- `a_checkpoint_asked_for_and_compact_are_refused_beside_a_reader`.
- `an_open_that_has_to_write_waits_for_readers_then_fails`: the last
  handle's checkpoint left for a reader; the next open waits, fails
  with `WouldBlock` and writes nothing; the reader reads on; once it is
  done the open recovers.
- `readers_in_turn_let_the_checkpoint_through_the_gate`: two threads
  reading in a loop, one call after another, beside twenty commits that
  each have a checkpoint due: at most half are left, and no read sees an
  older state than the one before it.
- `a_reader_without_lock_files_it_can_make_holds_the_file` (Unix).

And in `lock_files.rs`, `no_new_reader_begins_while_the_writer_waits_at_the_gate`,
added for the one mutation below the tests first missed.

Tests that said otherwise were changed: a reader beside a writer now
opens (`damage_and_a_held_file_are_not_format_errors`, and the
command's `a_file_in_use_is_named_as_such`,
`looking_into_a_file_opens_it_read_only`,
`writes_refuse_a_file_in_use`, which now holds a writer); a new file
syncs its directory five times, for the lock files; 17 is the format
a file is stamped with, and 18 the newer one refused.

Mutations, each caught: a checkpoint that ignores readers; recovery
that doesn't wait for them; a snapshot that skips the gate; the first
of a process's snapshots letting the lock go; `compact` ignoring
readers; a reader's snapshot reading the open's pages instead of its
own; an asked checkpoint left quietly; no writer's lock; the gate not
shut while the writer waits (caught only by the test added for it);
a reader of a file at 16 going by the lock files; the open not
stamping 17; skips not counted; `verify_export` leaving the lock files.

Not caught on Linux and macOS, by its nature: the open not letting go
of the file at its end, which only Windows' locks show: there a
reader's reads would be refused for as long as the writer runs, which
the tests above should meet on Windows' CI. Not tried.

Not tested here: the Windows paths of 187.2, 5 (a read refused while a
writer's open holds the file), which only Windows' CI reaches, and
there only in the tests above, where no open waits beside a reading
snapshot.
