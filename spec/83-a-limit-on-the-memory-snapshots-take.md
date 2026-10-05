# 83. A limit on the memory snapshots take (`storage/pages.rs`, `database.rs`, `snapshot.rs`, `lib.rs`)

The last step of §77's plan. An open snapshot keeps every page changed
since as it was (§80), and since §82 a caller can keep one for as long
as it likes. Now there is a limit on what those versions take. Past
it the oldest snapshot is ended: its next read fails, and the writer
goes on. With it come counters to see how much is kept, and a
benchmark of the workload §77.1 named.

## 83.1 What is counted
The versions kept for a snapshot's sake only: every version of a page
but its newest (`Memory::older`). The newest is the page itself. It
waits for a checkpoint (§51) or is in the file, and would be there
with no snapshot open.

The count is kept as it changes, not walked for: a commit adds what its
pages' chains gained and takes off what `prune` dropped; `forget`
counts again as it goes through every chain anyway.

## 83.2 The limit
`OpenOptions::snapshot_memory(bytes)`, in whole pages. Default 256 MiB,
`DEFAULT_VERSION_LIMIT`: as much again as the page cache's (§50). It
fills only while a snapshot is open beside writes. `usize::MAX` is no
limit.

After a commit has put its versions in, `Memory::keep_within_limit`:
while more are kept than the limit, the oldest open snapshot's commit
is ended, and what only it read is dropped. One commit at a time, so
no snapshot goes that didn't have to.

- **Past the limit, not at it.** A limit of n pages keeps n.
- **The oldest first.** It is the one holding the most, and dropping a
  newer one frees only the versions between it and the next.
- **Several snapshots of one commit end together**: they read the same
  versions.
- **0 ends a snapshot at the first page changed beside it.**
- **A lower limit set later** counts from the next commit.

## 83.3 Ended
`Memory::ended_before`: every snapshot of a commit before it is ended.
It only grows. One number is enough because snapshots end oldest
first.

- **`read_at` refuses** a commit before it, under the lock the versions
  were dropped under: a read gets its version or the error, never a
  newer page. This is the "snapshot too old" §78.2 left a place for.
- **The writer stops counting it** (`Writer::live`): its versions
  aren't kept for it, and it doesn't refuse `compact` (§82) any more.
  It is of no use to its holder, so it shouldn't cost anybody else.
- **A read that needs no page** is refused as well (`read_at(&Committed)`
  looks first): `collections` on an ended snapshot would otherwise
  answer from the catalog while `find` failed.
- **It stays ended.** Nothing brings the versions back; the holder
  takes a new snapshot.

## 83.4 The error
`Error::SnapshotTooOld`. A page read can only say `io::Error`
(`PageStore`), so the storage layer puts a marker, `TooOld`, inside
one, and `From<io::Error> for Error`, written by hand now, turns it
into the variant. Nothing between the two had to change.

Who can meet it: a `Snapshot`, a `View`, a `Cursor`, and any one read
that runs long enough, since each reads a snapshot of its own (§80):
an `export` during which more than the limit is rewritten fails. A
write never does: a batch reads the newest pages, which no limit
touches.

## 83.5 Counters
`Database::snapshot_info()`, a `SnapshotInfo`:
- `kept_pages`, `kept_bytes`: the older versions in memory now;
- `limit_bytes`: the setting;
- `ended`: how many times the limit has ended snapshots since `open`,
  counted by their commits.

No count of open snapshots: the writer's list (§80.3) holds reads
under way too, and a number that flickers with every `get` says little.

## 83.6 Measured
`bench/src/bin/long_read.rs`: 200,000 documents, then single writes
back to back for 15 seconds, every twentieth an update of a document
somewhere in the file, the rest inserts. Beside it, or not, one thread
with one snapshot for the whole run, counting and listing all of it
again and again and checking that both stay what they were. Linux, two
cores, release build:

| analysis | passes | longest pass | commits | commit p50 | p99 | max | most kept |
|---|---:|---:|---:|---:|---:|---:|---:|
| none | — | — | 26,871 | 484 µs | 1.5 ms | 33 ms | 0 |
| one snapshot, 15 s | 40 | 507 ms | 34,015 | 383 µs | 1.0 ms | 65 ms | 13.2 MiB |

- **The writer doesn't notice the snapshot.** The difference between
  the rows is the machine's, not the reader's (the second is the
  faster one). Before §80 this was 12 commits in 132 seconds (§80.6).
- **13 MiB for 34,000 commits**, a twentieth of the default. Inserts
  change the same few pages again and again, and a page is kept once
  per snapshot, not once per commit (§77.2); the updates, spread over
  the file, are most of it.
- Every pass found 200,000 documents and the same sum.

## 83.7 Tests
- `file.rs`:
  - past the limit the oldest snapshot is ended and the next one reads
    on; at the limit none is;
  - a limit of 0 ends a snapshot at the first change.
- `snapshot.rs`:
  - the default is 256 MiB, and `snapshot_info` reports the setting;
  - a snapshot kept past the limit is `SnapshotTooOld` in every read
    it has, and a new one reads; the writer never failed;
  - of two snapshots the older goes first;
  - a long read (an export held open) past the limit fails, and the
    writes beside it don't;
  - an ended snapshot doesn't refuse `compact`;
  - on threads: a reader ended by the limit gets the error and never a
    document from a later commit.
- `database.rs`: the model test of §80 (every open snapshot reads
  exactly its commit through random writes, checkpoints and
  compactions) runs with limits now, 300 steps in each of seven
  settings: a snapshot either reads its commit or is too old, and is
  too old only if the limit was passed.
- Checked by breaking it on purpose, ten ways, each failing a test: a
  read as of an ended snapshot let through; the limit not kept; the
  newest snapshot ended first; a commit's versions not counted; what
  is forgotten not taken off the count; an ended snapshot still
  counted as open by the writer; an ended snapshot read where no page
  is needed; too old coming out as an I/O error; the option not passed
  on; a snapshot at the limit, not past it, ended.

## 83.8 Limits
- **The limit is looked at when a commit ends**, so one large batch
  can take the count past it for the length of that commit.
- **Pages, not bytes in use**: a version counts 8 KB whatever it holds.
- **Nothing warns before the limit.** `snapshot_info` has to be asked.
- **A read can't ask for more time.** A reader that must not fail
  needs a higher limit, or shorter snapshots.
- **The cost on the read path of §80.6** is as it was.
- The benchmark is one machine, two cores, and a reader of half a
  second per pass; the 15 seconds are the snapshot's.
