# 57. Concurrency: one writer, and snapshots kept possible

The review after §52 asked for a decision instead of a default: will
readers ever stop waiting for writers? This section records the answer.
Snapshot reads (MVCC) are **deferred, not rejected**. The model stays as
it is. And the storage layer gets rules that keep snapshots possible
later, because that's where they would otherwise become expensive: the
free-space map (ROADMAP.md) is the first change the rules apply to.

## 57.1 The model, as it is
One lock, `RwLock<State>` in `Database` (§27):

| Call | Lock | A caller sees |
|---|---|---|
| `get`, `find`, `count`, `explain` | read, for the call | one committed state: a batch wholly or not at all |
| `cursor` | read, briefly, per item (§29.4) | each item from one committed state; items from different ones |
| `export` | read, for the whole export (§30) | one committed state; writers wait for it |
| `write_batch`, `insert`, ... | write: stage, apply, log and flush, commit, checkpoint (§19.3, §51) | — |
| `import` | write, once per chunk (§30.3) | not atomic |
| `compact` | write, for all of it (§41) | — |

Reads run in parallel. Writes run one at a time, and readers wait for a
write's whole commit: its WAL flush (about 5 ms here), and now and then
a checkpoint's write-back. A long read (an export, a large sorted
`find`) makes writers wait in turn. One process per file (§21.1).

## 57.2 The options, and the decision
- **A. Keep it**, as a decision. Chosen for now.
- **B. Writers work beside readers, and publish under the lock.** Stage
  privately, log and flush without the write lock, then take it only to
  publish: the staged pages become committed, the catalog is swapped.
  Readers stop waiting for a write's I/O; they still see only the
  latest committed state, one per call. No format change. The work is
  splitting `FileStore` into a shared committed part and a writer's
  private staging. It's the first half of C. Not scheduled: nothing
  measured asks for it yet.
- **C. Snapshots.** A reader pins the committed state it started with and
  keeps it across calls, while writers commit newer ones: an export that
  doesn't block writers, a cursor that sees one moment. SQLite's WAL
  mode, LiteDB 5, redb, LMDB and sled work this way; SQLite's default
  rollback-journal mode doesn't, and blocks like trunkdb. **Deferred.**

Why defer C:
- **Nothing measured needs it.** The workloads so far read and write
  little; once a desktop app has its data on screen, the database is
  idle. A writer at one commit a second (a location tracker, say) holds
  the lock about 5 ms of each second: a reader waits at most that, and
  rarely.
- **It's the largest change on the roadmap**, in the layer every other
  one rests on: page versions, the cache, checkpoints, compaction.
- **Deferring it doesn't make it more expensive**, if 57.3 holds.
  Indexes, data pages, the catalog and queries read only through
  `&dyn PageStore` and `&Catalog` (§3, §8). A snapshot can be one more
  `PageStore`, "the pages as of commit N", with nothing above it
  changing. What would make C expensive are storage changes that assume
  every reader sees the newest commit.

**When to reopen it:** a real workload where readers measurably wait (a
p99 in the tens of milliseconds), an export blocking writers in use, or
a need to read one consistent state across several calls. The
measurement to watch is reader latency while a writer commits:
measured in §58 — paced writers cost readers nothing, writers that
commit back to back do, and readers turned out to serialize on the page
cache.

## 57.3 Rules for the storage layer
None of these costs anything today, because no reader overlaps a
commit. Each is what a later snapshot, or B, would need. A change to the
storage layer states in its spec section how it keeps them.

1. **A freed page isn't reused while a reader could still need its old
   contents.** Today a page freed by a commit is on the free list at
   once, and the next allocation, even in the same batch, may take it:
   harmless, since no reader is running. With snapshots, an older
   reader may still read that page as it was. The usual fix is to
   remember which commit freed a page, and to reuse it only once every
   reader started after that commit (LMDB tags free pages with the
   transaction that freed them). **The free-space map is where this
   applies first:** knowing which pages have room is fine; reusing space
   a reader may still see is not, unless it's the writer's own staged
   pages.
2. **A committed page is never changed in place where a reader can see
   it.** Every change goes through staging (§19.2) and becomes visible
   only at commit, as a whole page. §52's in-place inserts change staged
   copies, not committed pages, so they keep this.
3. **The file only gets a page's new version once no reader needs the
   old one.** A checkpoint (§51) overwrites pages in the file. Today the
   newest version is also the only one anyone reads. With snapshots, a
   checkpoint may write back only up to the oldest reader's commit, as
   SQLite's does.
4. **Everything a reader sees comes through `PageStore` and `Catalog`.**
   No query, index or data code keeps page contents or catalog state of
   its own between calls. The catalog is cloned per batch already
   (§19.5); a snapshot would keep the clone of its commit.
5. **The page cache holds committed pages, keyed by page id** (§50).
   With versions, it holds the newest; older ones come from wherever the
   snapshot keeps them.
6. **Operations that rewrite everything may take the database alone.**
   Compaction (§41) holds the write lock for all of it; with snapshots,
   it would also wait for readers to finish. Rare operations don't need
   to be concurrent.

## 57.4 What changes now
Nothing in the code's behavior. The rules are in this section, in
DESIGN.md, beside the free-space map in ROADMAP.md, and in comments on
`FileStore::allocate_page` and `free_page`, where the free-space map
will change them.
