# 185. Readers in other processes, designed first (`storage/file.rs`, `durability/wal.rs`, `database.rs`)

**A draft, 2026-11-11: designed, not built.** Its four questions are
decided (185.4, 185.5, 185.6; listed in 185.7). Reviewed against the
code the same day, and corrected: 185.10 lists what changed.

A database file has one process at a time (§21). A read-only open goes
beside other read-only opens, but not beside a program that writes
(§88), and §88.5 sends a reader that wants the running program's data
to a snapshot in that program (§82). That leaves out the case that
matters most for tools: the application is running, and an admin tool
or an agent wants to look into the same file at the same time, from a
process of its own.

This section designs that case: **one process that writes, and any
number of processes that read beside it.** Several processes that
write stay out of scope (ROADMAP.md, "Not planned").

## 185.1 How SQLite does it
SQLite's WAL mode was the model, and the comparison shows what trunkdb
already has:

- **Writers** append page images to `<db>-wal`. One writer at a time,
  across processes, by a lock on one byte of `<db>-shm`.
- **Readers** take a snapshot: the last committed frame of the WAL when
  their transaction starts, kept in one of a few *read marks* in
  `<db>-shm`, each with a lock of its own. A page is read from the WAL
  if a frame at or before the snapshot holds it, else from the file.
- **`<db>-shm`** is mapped into every process. It holds the WAL index,
  a hash from page to its newest frame, so no process scans the WAL,
  and a header kept twice, with a checksum, rebuilt from the WAL when
  the two disagree after a crash.
- **A checkpoint** copies frames back into the file only up to the
  oldest read mark still held, and starts the WAL again only when no
  reader uses it. A long reader holds the WAL back, and it grows.
- **Every process checks its cache** against a counter in the header
  before it trusts it.

Its limits: one machine (shared memory), no network file systems, and
POSIX `fcntl` locks that a process loses when it closes *any*
descriptor of the file, which SQLite works around with a table of open
files per inode.

## 185.2 What trunkdb has already
- **A reader can rebuild the state on its own.** A read-only open reads
  the file and the WAL and keeps the latest image of each page in
  memory, writing nothing (§88.2). That is what a reader beside a
  writer does too, again and again.
- **The WAL only grows between checkpoints.** Each batch is one record,
  with its length and a CRC (§19.3). A record the writer is still
  appending reads as a torn tail, which is ignored (§19.3, §88.2). So a
  snapshot in a reader is a length of the WAL: every whole batch
  before it. Two things a reader has to allow for, both in 185.4: the
  WAL is cut back when a log fails (§128), and a record is in the file
  before it is flushed.
- **A change counter.** The `session` of §183 is new with the first
  batch logged after the WAL was empty: in the WAL's header at once, in
  the file's header on disk at the checkpoint after. So what a reader
  compares is three things: the session in the file's header on disk,
  the session in the WAL's header (none, for an empty WAL), and how far
  it read. The same two sessions and a longer WAL: it reads the new
  records only (185.4 says what it checks first). Anything else, an
  empty WAL where there was one among them: a checkpoint ran, and it
  drops what it built and starts again from the file.
- **The catalog is in pages,** so a reader loads it from its snapshot
  like any page. The free-space map (§75) is the writer's alone; a
  reader needs none.

So no shared memory is needed to start with: each reader builds its own
index of the WAL. The WAL's limit (§76, 65.5 MB by default) keeps that
small only while checkpoints run; beside readers the WAL grows past it
(185.5). So a reader's index holds where each page's newest image is
in the WAL, 16 bytes a page, as `Pending` keeps the ids today, and not
the images, as `restore_in_memory` does (§88.2): its memory then
doesn't grow with the WAL's bytes. A shared index can come later if
many readers each building one measures as a cost.

## 185.3 What can't stay: the checkpoint beneath a reader
A checkpoint writes pages back over the file's, in place, and then
empties the WAL (§51). In one process that is safe because older
versions stay in memory for the snapshots open (§80). A reader in
another process has no such memory: it would read some pages before the
write-back and some after, the mix §88.3 rejected locking for.

Recovery at an open that writes, and `compact`, write the file too.
Appending to the WAL does not; it is the only write the writer makes
while readers read.

So the writer has to know whether anyone reads.

## 185.4 Locks
Three locks, besides what §21 and §88 take:

- **The writer's lock:** exclusive, held by the process that writes for
  as long as it has the database open. It replaces the exclusive lock on
  the file itself (185.6).
- **The readers' lock:** shared, held by a reader for as long as a
  snapshot of its own is open, and only then. One lock for the process:
  its snapshots and threads count who holds it, and the last lets go.
- **The gate:** what a reader passes before it takes the readers' lock,
  and what the writer shuts to get a turn. Without it readers that each
  read for milliseconds, one overlapping the next, keep the readers'
  lock held for good, and the checkpoint out with it. And they would
  overlap more and more: every snapshot reads the WAL, which grows
  while the checkpoint is kept out.

The rules:

- **A checkpoint** first tries the readers' lock exclusively, without
  waiting. If it gets it, nobody reads: it checkpoints as today and lets
  go. If not, it leaves the pages waiting and the WAL as it is, and the
  next commit tries again, this time **through the gate**: it takes the
  gate exclusively, so no new reader begins, and waits for the readers'
  lock for a short bound (`OpenOptions::checkpoint_wait`; its default
  set in step 2, tens of milliseconds). Readers that read per call are
  done within it, and the checkpoint runs. If one isn't, a kept
  snapshot, the writer opens the gate and goes on without the
  checkpoint (185.5). A commit so waits at most the bound, once per
  checkpoint due, never for as long as a reader reads. The writer never
  waits for the gate while it holds the readers' lock, so the two can't
  hold each other.
- **`compact`** takes the readers' lock exclusively too, and fails at
  once beside a reader, as beside a kept snapshot (§77,
  `Error::SnapshotOpen`).
- **An open that has to write the file waits for the readers,** for a
  bounded time (decided 2026-11-11, 185.7.4). It has to when the WAL
  isn't empty: after a crash, and also after a clean close whose last
  checkpoint was skipped for a reader (185.5). And once in a file's
  life, to stamp format 17 (185.6). It shuts the gate and takes the
  readers' lock exclusively, waiting at most
  `OpenOptions::wait_for_readers`, 5 seconds unless set. Then recovery runs as it does today: written back, the
  WAL emptied, nothing new. If the readers are still there, the open
  fails with `WouldBlock`, having written nothing, and its message
  says why: the database has to be recovered, and another process is
  reading it. A retry succeeds once the reader is done. An open whose
  WAL is empty, in a file already at 17, waits for nobody. A WAL that
  holds only a torn tail is cut at open as today, without the readers'
  lock: no reader read anything from it.
- **A reader** takes the gate shared, and waits for it; then the
  readers' lock shared, and waits for that too: what holds it
  exclusively is a checkpoint, tens of milliseconds (§123.1). Then it
  lets go of the gate, reads the WAL's header and every whole batch,
  and the file's header, and has its snapshot. While it holds the
  readers' lock nothing writes the file, so the file and the WAL's
  batches it read agree for as long as it reads.
- **A writer's commit** goes on as ever: it appends a record and
  flushes it. Readers that took their snapshot before it don't see it;
  the next snapshot does.

**Reading a WAL that is being written** differs in three ways from
reading one at an open, where nobody writes it (§19.3, §91):

- **Damage may be the writer at work.** At an open, a bad checksum
  before the tail is an error, and so is a record that doesn't decode.
  Beside a writer, the end the reader took may lie in a record half
  written; on Windows a file's new length may show before its bytes,
  which read as zeros (to be confirmed in step 1's test), and a prefix
  of zeros passes the checksum of an empty body. And the WAL may be cut
  under the reader: by a failed log taken back, or by an open's cut of
  a torn tail. So a reader that finds damage, or the file shorter than
  the length it took, takes the length again and reads again, a few
  times, before it reports damage.
- **A batch may be seen before it is durable.** `log` writes a record
  and then flushes it; if the flush fails, the record is cut off again
  (§128), and the batch fails. A reader between the two sees a batch
  that never committed. For a snapshot per call that is a state no
  later snapshot shows, and it is said so in the docs. It is not made
  impossible: that would need the writer to say how far it flushed,
  somewhere readers see, which is the shared index of 185.2.
- **A reader that catches up (step 3) checks where it stopped.** Same
  sessions and a longer WAL is not enough: a batch taken back and
  another logged in its place leave both. The reader keeps the 8 bytes
  before the last record it read, its length and checksum, and reads
  them again first. If they changed, or the WAL is shorter than what
  it read, it starts again from the file.

Since the readers' lock is one lock, not one per reader, the writer
can't tell how old the oldest reader is, only that someone reads. That
is enough for the rule above. Read marks as SQLite's would let the
checkpoint write back everything older than the oldest reader; they can
come with a shared index (185.2).

## 185.5 A reader too long
Readers that read for milliseconds are the gate's (185.4): the
checkpoint gets its turn between them. This is about the reader the
gate's bound doesn't outwait.

A reader that keeps its snapshot keeps every checkpoint out, and the
WAL grows past its limit (§76). §77.2 rejected exactly that for readers
in the same process, and chose to keep old versions instead, ending the
oldest snapshot at a memory limit (§83). Across processes the writer
can't keep the reader's versions, so one of three things gives:

1. **The WAL grows** until the reader lets go, as in SQLite.
2. **The writer waits** at the WAL's limit, for as long as the reader
   reads.
3. **The reader is ended:** past the limit the writer asks readers to
   let go, through a flag both can see; a reader checks it before each
   read, lets go of the lock, and fails with
   `Error::SnapshotTooOld`. The writer waits for that, up to a bound.

**Decided, 2026-11-11: 1 first, 3 later; never 2.**

- **2 is out:** a tool that only looks would stop the application's
  commits for as long as it reads, the opposite of what §83 chose. The
  gate's wait is not 2: it is bounded, and short.
- **1 to start with.** A reader that takes a fresh snapshot per call,
  as a plain `find` already does in one process (§79), holds the lock
  for one call only. Admin views and agents mostly read that way, and
  the next commit checkpoints what waited, through the gate if readers
  follow one another closely. What grows
  meanwhile is not only the WAL: the pages waiting for a checkpoint
  stay in the writer's memory (§51, §77), the very thing
  `checkpoint_pages` bounds (§76). That is the known limit of this
  step, and it is made visible: a counter of checkpoints skipped for a
  reader, and of the largest WAL that left, beside §83's counters.
- **3 later,** as the clean end: it carries `Error::SnapshotTooOld`
  (§83) across processes. Past a second, higher limit the writer asks
  readers to let go, through a byte in a lock file (185.6) or a lock
  more; it waits for that a bounded time with the gate shut, and goes
  on without the checkpoint rather than hang. A reader checks the
  request every so many pages or milliseconds, not at every page, so a
  single long scan ends too, not only the next call. Built when a
  real reader shows how long readers get.

## 185.6 Where the locks live
**Decided, 2026-11-11.**

**No lock stays on a file anyone reads or writes.** On Windows a lock
taken with `LockFileEx` is mandatory, and std's `File::lock` covers the
whole file: an exclusive lock keeps other processes from reading and
writing it, and a shared one keeps every process from writing it. A
writer holding the database file, as today, would keep readers from
reading it; readers holding it shared would keep the writer's
checkpoint from writing it; a readers' lock on the WAL would keep the
writer from appending. (To be confirmed by a test on Windows CI first,
step 1 of 185.8.)

**Three lock files, with std's locks:** `<path>.writer`,
`<path>.readers` and `<path>.gate`, one lock each, as std's locks are
whole-file. Another may come with 185.5's request to let go.
- **Rejected: one file with byte ranges,** through `fcntl` and
  `LockFileEx` directly. It needs `libc` and `windows-sys`, or `extern`
  declarations and `unsafe`; trunkdb has only pure Rust dependencies,
  and §183 chose `uuid` to add none. And `fcntl`'s locks belong to the
  process: a second open in the same process would get the lock too,
  where §21 refuses it. Linux's OFD locks fix that; macOS, as far as
  known, has none.
- **Made by whoever needs one first,** opened with `create` and never
  truncated, its name synced as §182 does. A writer makes all three at
  its open, before it lets go of the database file (below): where a
  writer has been, they are there.
- **Opened for reading only by a reader that may not write them.** A
  lock needs a handle, not the right to write (`flock`; `LockFileEx`
  wants read or write access, in step 1's test). So a reader in a
  directory it can't write still takes part, once the files are there.
- **Never removed,** by anyone. A lock file removed while another
  process opens it leaves two files, and two "exclusive" locks.

**Releases up to 0.38.0 kept out by the format.** They don't know the
lock files, and the new opens don't hold the database file. What every
older release checks before it writes anything is the format version,
right after its lock (§21.2): it refuses a higher one, writing and
read-only alike. So the release that brings this raises the format to
17, once. The pages stay as they are; 17 means "whoever opens this file
has to know the lock files". A new writer opens:
1. `<path>.writer`, exclusively, first: a second writer of this
   release is refused here, and never touches the database file, where
   on Windows its lock would fail the running writer's checkpoint and
   the readers' reads;
2. the database file, exclusively, as today, in a file at 17 too: an
   older process is kept out, and a reader that holds the file itself
   (below) refuses this open, while
3. it makes the other two lock files, and stamps the header to 17 if
   it was lower, in the file itself, not only in the WAL: an older
   process would otherwise find 16 on disk and replay the WAL itself.
   Writing the file waits for the readers (185.4);
4. then lets go of the database file, so readers can read it on Windows.

Nothing is written before 1 and 2 are both held, so their order is
free, and this one keeps a refused open from disturbing anyone. Between
2 and 4 the file is locked on Windows, and a reader in the middle of a
snapshot has its reads fail there for that moment: **a reader on
Windows tries a read again that failed with a lock violation,** for a
bounded time (in step 1's test).

An older release that comes after that finds 17 and refuses: the format
is checked before the header's checksum and before the WAL is opened
(`Header::decode`), so even a header read while a checkpoint writes it
is refused. A file once stamped opens only in this release or a newer
one; back is by export and import (§30), as for every format before. A
new release opens 16 and 17.

**A reader of a file still at 16** opens as §88 does: it takes the
database file shared, for as long as it is open. A release before this
one may be writing that file, holding only the file itself, and its
checkpoints would go beneath a reader that held only the readers' lock.
Having the lock, it reads the header again: at 17 by now, it lets go
and goes by the lock files.

**A reader that finds no `<path>.readers` and can't make it,** in a
directory it can't write, opens as §88 does too: it takes the database
file shared, and a new writer's step 2 fails beside it with
`WouldBlock`, as today. Having the lock, it looks for `<path>.readers`
again: there by now, a writer came between, and it lets go and goes by
the lock files. Without that second look a writer that had opened
before, and let go of the file in step 4, would checkpoint beneath it.
With it: a writer that got to step 2 first has made the lock files
before the reader gets the file; one that comes later fails at step 2.
Safe, and without the parallel reads.

**Considered with it:** a field in the header naming the lock protocol,
from 17 on, so that a later change to the protocol needn't raise the
format again. Older releases ignore such a field, so it can't replace
the bump now. To decide when step 1 is built.

## 185.7 The questions, and where they were decided
1. ~~A reader too long~~: decided in 185.5, the WAL grows first, the
   reader is ended later.
2. ~~Lock files~~: decided in 185.6, three files with std's locks.
3. ~~Older releases~~: decided in 185.6, kept out by format 17.
4. ~~An open that writes beside readers, after a crash~~: decided in
   185.4, it waits a bounded time, then fails with `WouldBlock`.
   Readers that take a snapshot per call hold the lock for
   milliseconds, so the wait is short in practice; long only beside a
   kept snapshot (step 4 of 185.8), and step 5's request to let go can
   then serve the open too.

   **Later, if needed:** recovery into memory instead, as a read-only
   open does it (§88.2): the torn tail cut, the WAL's session continued
   (§183 starts a new one only on an empty WAL; to be checked in the
   code), and the pages written back at the first checkpoint that
   finds no reader. It never waits, but it rebuilds a recovery that is
   well tested today, and makes a writer continue a WAL that already
   holds records, which no open does now. If applications fail to
   start because readers keep them waiting.

## 185.8 Steps, a section each
| Step | What | Seen from outside |
|---|---|---|
| 1 | Windows' lock behaviour tested on CI (what a lock keeps out, a lock through a read-only handle, a read that meets a lock, a length before its bytes); locks moved to the three lock files, the writer's first; format 17, stamped by a writer's open; a read-only open of a file at 16, or without lock files it can make, as §88 | older releases refuse a stamped file |
| 2 | A read-only open beside a writer: a snapshot per call, through the gate, built from the file and the WAL's batches under the readers' lock, read again where the writer was at work; checkpoints skipped while a reader holds it, tried again through the gate, and counted; an open that has to write the file waits `wait_for_readers` for readers | the feature, with the WAL and the writer's memory growing past a long reader |
| 3 | The reader that catches up: same sessions and its last record as it was, the new records only; else start again | readers cheap to refresh |
| 4 | A snapshot a reader keeps (`Database::snapshot` on a read-only open beside a writer) | as §82, across processes |
| 5 | A reader too long is ended (185.5, 3): a second limit, the request to let go, `Error::SnapshotTooOld` in the reader | a forgotten reader doesn't hold the WAL |

## 185.9 What it doesn't change
- Several processes that write: still one at a time, refused at open.
- Network file systems: locks there are as unreliable as §21.1 says,
  more so with two processes depending on them.
- The pages and the WAL format. The format number goes to 17 (185.6),
  but no layout changes with it.
- A process with the database open in both modes: it clones its handle
  (§21), as now.

## 185.10 Corrected after the review, 2026-11-11
The design read against the code as it is at 0.38.0. Its core held: a
snapshot as a length of the WAL, appending as the writer's only write
between checkpoints, a crash mid-checkpoint covered by the WAL, and
format 17 against older releases. What changed:

1. **The reader without lock files was not safe** beside a writer that
   had opened before it: it looks again for the lock files, and a
   writer makes them all at its open (185.6).
2. **A reader of a file at 16 was not said:** as §88, since an older
   release may be writing it (185.6).
3. **The writer's lock comes before the database file,** and a reader
   on Windows tries a locked read again (185.6).
4. **Short readers could keep the checkpoint out** as well as a long
   one: the gate, a third lock file (185.4, 185.5).
5. **A batch taken back** could stay in a reader that catches up: it
   checks its last record first. That a reader may see a batch before
   it is durable is now said (185.4).
6. **The WAL read beside a writer** reads again before it reports
   damage (185.4).
7. **The session** is new with the first batch after an empty WAL, not
   after the checkpoint; a reader's index holds places, not images
   (185.2); the readers' lock is counted within a process (185.4).
