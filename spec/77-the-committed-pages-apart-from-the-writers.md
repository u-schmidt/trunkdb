# 77. The committed pages, apart from the writer's (`storage/pages.rs`, `storage/file.rs`)

§57 deferred snapshot reads until a workload asked for them. One does
now, and this section reopens the decision, records the plan, and takes
its first step: `FileStore` is split into the pages every reader reads
and the batch only the writer sees. Nothing behaves differently yet.

## 77.1 Why now
§57.2 named three triggers. A workload now has two of them:
- **many small writes beside long reads:** an analysis reads for up to
  15 seconds, and every write waits for it; mostly inserts, few updates;
- **one state across several calls:** a count and the `find` after it
  have to agree, whatever is written between them.

A single call already sees one state (§57.1). Several calls don't, and
a cursor reads each item at its own moment (§29.4).

## 77.2 The plan
Page versions in memory, one writer as before. A reader pins the commit
it started at; a page read takes the newest version of the page at or
before that commit. No change to the file or the WAL: old versions are
only needed while the process runs.

Rejected:
- **A copy-on-write B-tree** (LMDB, redb), old versions in the file: a
  new file format, every change copying its path to the root, which
  undoes §52, and a WAL (§19, §51) left with nothing to do.
- **Versions per document** (PostgreSQL): version fields in every cell
  and index entry, and a vacuum. It buys several writers at once, which
  §6 rules out.
- **Checkpoints held back by the oldest reader**, as SQLite's and as
  §57.3's rule 3 has it: one long reader stops the write-back, and the
  WAL grows without bound, which §76 just fixed. Instead a checkpoint
  always writes the newest version, and the versions older readers need
  stay in memory. The memory is the same either way.

Rule 1 of §57.3 needs nothing: freeing a page and using it again are
two more versions of that page, and an older reader finds the one
before them. No tags on freed pages, no change to the free list or the
free-space map (§75). That answers the format question ROADMAP.md left
open.

Six steps, a section each (§81 is not one of them: a fix found on the
way):

| Section | What | Seen from outside |
|---|---|---|
| §77 | `FileStore` split: the committed pages, and the writer's staging; committed pages numbered by commit | nothing |
| §78 | every read through a snapshot, inside, under the one lock | nothing |
| §79 | option B of §57.2: a lock for writers only; readers take a snapshot per call; log, flush and checkpoint beside them | readers don't wait for commits |
| §80 | older versions kept while a snapshot needs them, and dropped after | nothing |
| §82 | `Database::snapshot`, `Snapshot`, `View<T>`; `export`, `check` and `cursor` on a snapshot | the feature |
| §83 | a limit on the memory versions take, counters, a benchmark of the workload above | a forgotten snapshot can't take all memory |

Decided with it:
- **A version is kept only if an open snapshot would read it**, or it's
  the newest: one long snapshot beside many commits costs one old copy
  per page changed, not one per commit.
- **A cursor shows one moment**, a change from §29.4.
- **`compact` with a snapshot open is an error**, at once: waiting
  would deadlock a thread holding one (rule 6 of §57.3).
- **At the memory limit the oldest snapshot fails** on its next read; a
  forgotten reader doesn't stop writes.
- **A plain read never waits.** One that runs while another thread
  commits sees the state before; one that starts after `commit`
  returned sees it.
- **Not planned:** a read and a write as one unit. The analysis writes
  its results as an ordinary batch.

## 77.3 The split
`FileStore` held everything: the file, the cache, the committed pages
waiting for a checkpoint, the header, and the batch being staged. A
reader got the committed pages only because `read_current` found no
staging while the reader held the lock. Now the two are apart:

- **`Pages`** (`storage/pages.rs`): the file, the page cache (§50), and
  the committed pages not written back (§51). `read` looks in those,
  then the cache, then the file. It changes at three moments only:
  `commit`, `checkpoint`, `restore`. Reading and writing pages in the
  file, and their checksums (§40), moved here with it.
- **`FileStore`** keeps what is the writer's: the header as the batch
  has it, and `Staging`, its dirty pages. `read_current` is the dirty
  set, then `Pages::read`. `commit` hands the dirty pages over;
  `rollback` drops them, and `Pages` never saw them. It still
  implements `PageStore`, and nothing above it changed.

The header page is a page like any other in `Pages`: the committed one
there, the staged one in the dirty set. `FileStore::header`, the decoded
one, is the writer's.

Not done here: `Pages` isn't shared yet. It sits inside `FileStore`,
inside `State`, behind the one lock (§27). §79 takes it out.

## 77.4 Commit numbers
`Pages::commit` counts: the first commit after `open` is 1. Each page
waiting for a checkpoint carries the number of the commit that wrote it
(`versions`, which was `unwritten`), still one version per page, the
newest. Nothing reads the number yet but an assertion that a version
only ever replaces an older one; §78 reads by it.

- **In memory only.** It starts at 0 at every open. The file and the
  WAL know nothing of it, so no format changed.
- **A rollback takes no number**, and neither does a batch that changed
  nothing (§19.3).
- **A checkpoint doesn't start the count again:** a snapshot's number
  has to stay comparable across one.

## 77.5 Tests
- `file.rs`:
  - a staged batch is read by the store and is not among the committed
    pages, its header page included, until `commit`;
  - a rollback leaves the committed pages and the count as they were;
  - commits count from 1, a waiting page carries the number of the last
    commit that changed it, a checkpoint keeps the count, a new open
    starts at 0.
- Every other test runs unchanged, but for where a test reaches in:
  the injected write-back failures are fields of `Pages` now
  (`store.pages.failing_write_backs`).
- Checked by breaking it on purpose, ten ways, each failing a test: the
  writer's dirty set not read; the committed versions not read; a
  staged write put among the committed pages; commits not counted; a
  version given the number before its commit's; a checkpoint starting
  the count again; a rollback keeping the staged header; a checkpoint
  not cutting the file; recovery leaving the cache as it was; an older
  version kept over a newer.

## 77.6 Limits
- No measurement: a read takes the same steps as before, through one
  more function.
- `Pages` is `pub(crate)` with a `pub(crate)` field in `FileStore`, for
  the tests above the storage layer that inject failures. §79 gives it
  a place of its own.
