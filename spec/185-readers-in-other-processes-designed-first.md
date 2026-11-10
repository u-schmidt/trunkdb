# 185. Readers in other processes, designed first (`storage/file.rs`, `durability/wal.rs`, `database.rs`)

**A draft, 2026-11-11: designed, not built.** Its four questions are
decided (185.4, 185.5, 185.6; listed in 185.7).

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
  snapshot in a reader is a length of the WAL: every whole record
  before it.
- **A change counter.** The `session` of §183 is new after every
  checkpoint, in the file's header and in the WAL's. A reader that
  finds the same session and a longer WAL reads the new records only; a
  reader that finds another session knows a checkpoint ran, drops what
  it built, and starts again from the file.
- **The catalog is in pages,** so a reader loads it from its snapshot
  like any page. The free-space map (§75) is the writer's alone; a
  reader needs none.

So no shared memory is needed to start with: each reader builds its own
index of the WAL, which the WAL's limit (§76, 65.5 MB by default) keeps
small. A shared index can come later if many readers each building one
measures as a cost.

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
Two locks, besides what §21 and §88 take:

- **The writer's lock:** exclusive, held by the process that writes for
  as long as it has the database open. It replaces the exclusive lock on
  the file itself (185.6).
- **The readers' lock:** shared, held by a reader for as long as a
  snapshot of its own is open, and only then.

The rules:

- **A checkpoint** first tries the readers' lock exclusively, without
  waiting. If it gets it, nobody reads: it checkpoints as today and lets
  go. If not, it leaves the pages waiting and the WAL as it is, and the
  next commit tries again.
- **`compact`** takes the readers' lock exclusively too, and fails at
  once beside a reader, as beside a kept snapshot (§77,
  `Error::SnapshotOpen`).
- **An open that has to write the file waits for the readers,** for a
  bounded time (decided 2026-11-11, 185.7.4). It has to when the WAL
  isn't empty: after a crash, and also after a clean close whose last
  checkpoint was skipped for a reader (185.5). And once in a file's
  life, to stamp format 17 (185.6). It takes the readers' lock
  exclusively, waiting at most `OpenOptions::wait_for_readers`, 5 seconds
  unless set. Then recovery runs as it does today: written back, the
  WAL emptied, nothing new. If the readers are still there, the open
  fails with `WouldBlock`, having written nothing, and its message
  says why: the database has to be recovered, and another process is
  reading it. A retry succeeds once the reader is done. An open whose
  WAL is empty, in a file already at 17, waits for nobody.
- **A reader** takes the readers' lock shared, and waits for it: what
  holds it exclusively is a checkpoint, tens of milliseconds (§123.1).
  Then it reads the WAL's header and every whole record, and the file's
  header, and has its snapshot. While it holds the lock nothing writes
  the file, so the file and the WAL's records it read agree for as long
  as it reads.
- **A writer's commit** goes on as ever: it appends a record and
  flushes it. Readers that took their snapshot before it don't see it;
  the next snapshot does.

Since the readers' lock is one lock, not one per reader, the writer
can't tell how old the oldest reader is, only that someone reads. That
is enough for the rule above. Read marks as SQLite's would let the
checkpoint write back everything older than the oldest reader; they can
come with a shared index (185.2).

## 185.5 A reader too long
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
  commits for as long as it reads, the opposite of what §83 chose.
- **1 to start with.** A reader that takes a fresh snapshot per call,
  as a plain `find` already does in one process (§79), holds the lock
  for one call only. Admin views and agents mostly read that way, and
  the next commit after the call checkpoints what waited. What grows
  meanwhile is not only the WAL: the pages waiting for a checkpoint
  stay in the writer's memory (§51, §77), the very thing
  `checkpoint_pages` bounds (§76). That is the known limit of this
  step, and it is made visible: a counter of checkpoints skipped for a
  reader, and of the largest WAL that left, beside §83's counters.
- **3 later,** as the clean end: it carries `Error::SnapshotTooOld`
  (§83) across processes. Past a second, higher limit the writer asks
  readers to let go, through a byte in the lock file (185.6) or a third
  lock, as SQLite's PENDING; it waits for that a bounded time, and goes
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

**Two lock files, with std's locks:** `<path>.writer` and
`<path>.readers`, one lock each, as std's locks are whole-file. A third
may come with 185.5's request to let go.
- **Rejected: one file with byte ranges,** through `fcntl` and
  `LockFileEx` directly. It needs `libc` and `windows-sys`, or `extern`
  declarations and `unsafe`; trunkdb has only pure Rust dependencies,
  and §183 chose `uuid` to add none. And `fcntl`'s locks belong to the
  process: a second open in the same process would get the lock too,
  where §21 refuses it. Linux's OFD locks fix that; macOS, as far as
  known, has none.
- **Made by whoever needs one first,** opened with `create` and never
  truncated, its name synced as §182 does.
- **Never removed,** by anyone. A lock file removed while another
  process opens it leaves two files, and two "exclusive" locks.

**Releases up to 0.38.0 kept out by the format.** They don't know the
lock files, and the new opens don't hold the database file. What every
older release checks before it writes anything is the format version,
right after its lock (§21.2): it refuses a higher one, writing and
read-only alike. So the release that brings this raises the format to
17, once. The pages stay as they are; 17 means "whoever opens this file
has to know the lock files". A new writer opens:
1. the database file, exclusively, as today: an older process is kept
   out while
2. it takes `<path>.writer` exclusively, and
3. stamps the header to 17 if it was lower, in the file itself, not
   only in the WAL: an older process would otherwise find 16 on disk
   and replay the WAL itself. Writing the file waits for the readers
   (185.4);
4. then lets go of the database file, so readers can read it on Windows.

An older release that comes after that finds 17 and refuses. A file
once stamped opens only in this release or a newer one; back is by
export and import (§30), as for every format before. A new release
opens 16 and 17.

**A reader that can't make the lock files,** in a directory it can't
write, falls back to §88: it takes the database file shared, and a new
writer's step 1 fails beside it with `WouldBlock`, as today. Safe, and
without the parallel reads.

**Considered with it:** a field in the header naming the lock protocol,
from 17 on, so that a later change to the protocol needn't raise the
format again. Older releases ignore such a field, so it can't replace
the bump now. To decide when step 1 is built.

## 185.7 The questions, and where they were decided
1. ~~A reader too long~~: decided in 185.5, the WAL grows first, the
   reader is ended later.
2. ~~Lock files~~: decided in 185.6, two files with std's locks.
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
| 1 | Windows' lock behaviour tested on CI; locks moved to the two lock files; format 17, stamped by a writer's open | older releases refuse a stamped file |
| 2 | A read-only open beside a writer: a snapshot per call, built from the file and the WAL's records under the readers' lock; checkpoints skipped while a reader holds it, and counted; an open that has to write the file waits `wait_for_readers` for readers | the feature, with the WAL and the writer's memory growing past a long reader |
| 3 | The reader that catches up: same session, the new records only; another session, start again | readers cheap to refresh |
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
