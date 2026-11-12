# 189. A reader too long is asked to let go (`storage/lock_files.rs`, `database.rs`)

Step 5 of §185.8, and the last. A snapshot a reader in another process
keeps holds the readers' lock, and every checkpoint stays out for as
long as it does (§187.3): the WAL and the writer's memory grow, for good
if the snapshot is forgotten in a tool. §185.5 chose to let the WAL grow
first, and to end such a reader later, past a second limit, carrying
`Error::SnapshotTooOld` across processes. This is that.

## 189.1 The request
A fourth file beside the database, `<path>.letgo`, made by a writer's
open with the lock files and never locked: one byte, 1 while the writer
asks the readers to let go, 0 else. Never locked, so a reader on Windows
can read it while the writer has it open (§185.6).

**Asking** (`WriterLocks::readers_asked`): the writer shuts the gate,
writes 1, and waits for the readers' lock, as through the gate
(§185.4); then writes 0, and opens the gate, whether it had the lock or
not. A writer's open writes 0 too, should a writer have ended while it
asked.

**A request is the byte at 1 and the gate shut.** A writer that ended
while it asked has let go of the gate with its process, and left 1
behind: with the gate open, a reader takes it for no request. The gate
also decides who is asked: a reader that passed it before the writer
shut it is under way, and asked; one that comes after waits at the gate
until the writer is done, and the request is withdrawn by then.

## 189.2 The watcher
Every read-only open beside a writer, where the request's file is
there, starts a thread (`trunkdb-reader-watch`), trunkdb's first in a
library call that isn't asked for one. It sleeps while its process
holds no snapshot, woken when one takes the readers' lock; while one
does, it looks at the request every 20 ms (`LET_GO_LOOK`). Asked, it
lets go of the readers' lock for every snapshot of its process at once
(`ReaderLocks::let_go`), and counts that in `ReaderLocks::ended`. It
ends within a second of the open's last handle.

So a forgotten snapshot lets go though nothing in its process reads: the
watcher, not the snapshot, sees the request.

## 189.3 Ending what was let go
A snapshot is ended by the count moving past the one its hold was taken
at (`ReadersHold::ended_at`):
- **its hold** counts no more: dropped later, it doesn't let go of a
  lock taken again since;
- **its pages** (`Pages::ended`) refuse every read as too old, checked
  before each read, and again after a read of the file: the writer may
  have written it in between, and what was read is then thrown away. A
  page from memory needs no second look: the writer doesn't reach it.
  So a long scan ends at its next page, not only at the next call;
- **`Database::read_at`** refuses a kept snapshot, before a read that
  needs no page;
- **a snapshot being built** when the process lets go is built again,
  through the gate (`ReaderSide::holding`): what it read may be the
  writer's;
- **the kept store** (§188) is built anew, not caught up: its pages are
  ended with its snapshots.

Every read of an ended snapshot fails with `Error::SnapshotTooOld`, as
when the memory limit ends one (§83). A new read takes the database as
it is then.

## 189.4 When the writer asks
- **A checkpoint due** (§187.1) **with the WAL past
  `OpenOptions::let_go_wal_bytes`**: four times `checkpoint_wal_bytes`
  unless set, about 262 MB at the defaults; 0 never asks. It waits up
  to `checkpoint_wait`, or `LET_GO_WAIT` (1 s) if that is longer: many
  times the watcher's look. Readers let go, the checkpoint goes
  through; one still there, it is left as before, and the next ask comes
  10 seconds later at the earliest (`ASK_AGAIN_AFTER`). Counted in
  `SnapshotInfo::readers_asked`.
- **An open that has to write the file** (§185.4): it asks at once, and
  waits `wait_for_readers` as before.
- Not `Database::checkpoint`, nor the last handle's checkpoint: they go
  through the gate as before (§187.2, 3).

## 189.5 Decided
1. **A watcher thread, not only the snapshots that read** (asked
   2026-11-12). Without it a snapshot sees the request only when it
   reads, and the forgotten one, the case this is for, never does.
2. **The limit is the WAL's length, four times `checkpoint_wal_bytes`
   by default** (asked 2026-11-12): what a long reader costs is the WAL
   on disk and the writer's memory, not the time it holds.
3. **Every snapshot of every reader is ended,** not the oldest. The
   readers' lock is one lock (§185.4): the writer can't tell readers
   apart, and a process can't keep a share of a lock it let go of.
4. **An open that has to write asks:** an application that starts goes
   before a tool that looks. §185.10 left it to "the request to let go
   can then serve the open too". An open beside a reader that doesn't
   let go still fails after `wait_for_readers`.
5. **A byte and the gate, not a counter.** A counter read when a reader
   takes its lock may move between its look and its passing the gate,
   and end a reader that came after the writer was done. The byte is
   read while the lock is held, and only counts with the gate shut.
6. **A request file never locked,** not a byte in a lock file: on
   Windows a locked file can't be read by others (§185.11).
7. **Asking again after 10 seconds,** not at every commit: a reader that
   doesn't let go, such as one whose process hangs, costs the writer a
   second in ten at most. A checkpoint between asks goes through the
   gate as before (§187.2, 2).
8. **No format change.** A writer of this release always makes the
   request's file; a reader without it, in a directory it may not
   write before a writer has been there, has no watcher and reads as
   §187 has it.

## 189.6 Tests
In `storage/lock_files.rs`:
- `a_reader_asked_to_let_go_lets_go`: two holds of one process, ended
  within half the bound; the request withdrawn; a new hold takes the
  lock anew, and the old ones dropped don't let go of it.
- `a_request_left_behind_is_no_request`: the byte at 1 with the gate
  open ends nobody; the next writer's open withdraws it.

In `storage/file.rs`, `a_readers_pages_refuse_every_read_once_it_let_go`:
a page from the cache refused, and one read from the file while the
process let go.

In `database.rs`:
- `a_forgotten_snapshot_is_ended_past_the_second_limit`: with 0 never
  asked; past the limit asked once, the WAL checkpointed, the kept
  snapshot `SnapshotTooOld`, the reader's next read the newest.
- `an_open_that_has_to_write_asks_readers_to_let_go`: a kept snapshot
  ended, the open recovers, the reader reads what it recovered.
- `a_reader_that_does_not_let_go_is_asked_once_in_a_while`: beside one
  that holds the lock without a watcher, four commits past the limit
  ask once; the reader that let go reads on.
- `an_open_that_has_to_write_waits_for_readers_then_fails`, changed:
  beside a reader that doesn't let go, the open fails as before, and a
  reader that did, with nothing new to read, reads on from a store
  built anew.

Changed: `a_kept_snapshot_keeps_checkpoints_out_and_the_wal_grows` sets
`let_go_wal_bytes(0)`, as its `checkpoint_pages(1)` makes the default
limit small; a new database's directory is synced six times, for the
request's file.

Mutations, each caught: the watcher never letting go; a request left
behind taken for one; the request not withdrawn; the open not
withdrawing it; letting go without counting; an ended hold still
counted at its drop; the watcher not woken when a snapshot takes the
lock (caught once the test let the watcher fall asleep first); pages
not refusing reads before, or after a read of the file; `read_at` not
refusing an ended snapshot; the kept store caught up after letting go
(caught only beside a request with nothing new after it, where an
open asks and fails: with a new batch, catching up fails as too old,
and the next try starts again anyway); the writer never asking; 0 not
meaning never; the open not asking; asks not counted; asking again at
every commit.

Not caught, by its nature: a snapshot whose building the process let go
under not built again. It takes the moment of the build, which no test
here can hold; its pages refuse every read anyway, so a snapshot built
so fails as too old, where it would be built again.

## 189.7 Corrected after a review, 2026-11-12
1. **The watcher looks at the request again before it lets go,** under
   the lock where a snapshot counts itself in
   (`ReaderLocks::let_go_if_asked`). It looked once, then let go: a
   writer that had waited in vain could withdraw the request and open
   the gate in between, and a snapshot that came after was ended, though
   189.1 has it not asked. Now a snapshot counted in while the watcher
   looks again passed the gate before the writer shut it.
2. **The scan for damaged pages stops once its process let go**
   (`Pages::damaged`): it reads the file itself, past the checks of
   189.3, and a page a writer in another process wrote meanwhile could
   be read half old, half new, and called damaged. Each page read is
   followed by the check `read_at` makes, and the scan fails as too old.
3. **A check of a snapshot ended while it checks is
   `Error::SnapshotTooOld`,** not a report: it collected every read that
   failed as a problem, so a snapshot ended by letting go, or by the
   memory limit (§83), gave `Ok` with problems the file doesn't have.
   `Database::check_of` asks the snapshot at the end
   (`SnapshotStore::is_ended`). A writer writes only once the reader
   has counted its letting go, so a page called damaged because it was
   half written can't come from a snapshot that isn't ended.

Tests: `a_request_withdrawn_before_the_watcher_lets_go_ends_nobody`
(`lock_files.rs`), with a hook between the watcher's two looks;
`a_readers_scan_for_damage_stops_once_it_let_go` (`file.rs`);
`a_check_whose_reader_lets_go_is_too_old` and
`a_check_whose_snapshot_the_memory_limit_ends_is_too_old`
(`database.rs`), each ended at the check's first read of the file.

Mutations, each caught: no second look; no check after a page the scan
read; no check at the end of `check_of`; `is_ended` without letting go,
or without the memory limit.
