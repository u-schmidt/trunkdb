# 79. Writers beside the readers (`database.rs`, `storage/pages.rs`)

The third step of §77's plan, and option B of §57.2. The one lock is
gone. A batch is staged, logged and flushed under a lock only writers
take, and readers go on meanwhile; they wait only while a commit is
published, which is putting its pages into a map. A checkpoint writes
the file beside them too. §58.3's starved readers are the measure: with
a writer committing back to back, they now do 18 to 31 times the
reads.

A reader still reads the last commit, one call at a time, and a commit
still waits for the reads under way. Reading an older commit while
newer ones come is §80.

## 79.1 Three places instead of one lock
`Shared` held `RwLock<State>`. Now:

| What | Behind | Who takes it, and for how long |
|---|---|---|
| `writer: Writer`, the staged batch (`FileStore`) and the WAL | a `Mutex` | a batch, from staging to its checkpoint; never a reader |
| `current: Snapshot`, the last commit (§78) | an `RwLock`, the *gate* | a read, shared, for the call; a commit, alone, to publish |
| `pages: Pages`, the committed pages (§77) | its own `RwLock`, inside | a page read, shared, for the lookup; a commit and the end of a checkpoint, alone, briefly |

`FileStore` and `Shared` hold the same `Pages`, in an `Arc`.

## 79.2 A commit
`transact`, all of it under the writer's lock:

1. **Stage and apply**, as before: the dirty pages and the batch's own
   catalog (§78.3) are the writer's. Readers go on.
2. **Log and flush** (§51). Readers go on: this was the 5 ms they
   waited for (§57.1).
3. **Check the staged pages' layout** (§66), which `commit` used to do,
   so it isn't done at the gate.
4. **Publish**, with the gate held alone: the pages go into `Pages`
   under the next commit number, and `current` becomes the new
   snapshot. Taking the gate waits for the reads under way. The old
   snapshot is dropped after the gate is open again.
5. **Checkpoint**, when due (§53, §76). Readers go on (79.4).

A reader that starts during 1, 2 or 5 reads the commit before; one
that starts after `transact` returned reads the new one.

## 79.3 Why a commit waits for the reads under way
`Pages` keeps one version of a page (§77.4). A reader in the middle of
a `find` when a commit's pages went in would read some pages as of its
own commit and get "snapshot too old" for the others (§78.2). The gate
rules that out, as the one lock did: no reader is on an older commit
when a newer one comes.

So one thing has not changed: **a long read still holds up commits**,
now at their publish, where it used to be at their start. And while a
commit waits there, new reads wait behind it, as `std`'s `RwLock` puts
a waiting writer first. An export of some seconds stops writers for
those seconds, and readers with them (§30.6). §80 is what ends that:
with the versions a reader needs kept, a read holds the gate only to
take its snapshot.

## 79.4 A checkpoint beside the readers
`Pages::checkpoint` took `&mut self`, and the write lock with it. Now:

1. the waiting pages are listed, under the read lock;
2. each is written to the file, the file cut to its page count and
   flushed, with no lock held;
3. under the write lock, at once, the versions are forgotten and their
   pages put into the cache.

Nobody reads the file where it changes, by two facts together:
- **only pages with a version waiting are written**, and a page with a
  version is read from that version, not from the file;
- **a page gets a version only by a commit**, and a commit waits for
  the reads under way (79.3). So a reader that found no version for a
  page, and is reading it from the file, can't have a checkpoint
  writing that page beside it.

Step 3 is one step so that a reader finds each page in the versions or
in the cache: with the versions forgotten first, a reader would go to
the cache, miss, and read the file, correctly but for nothing.

This holds with the gate. §80, where a reader no longer holds it,
needs its own answer for a page read from the file while a commit and
a checkpoint of that page pass by; §77's plan has one.

## 79.5 One lock inside `Pages`
The cache had its own `RwLock` (§65), and the committed pages not
written back sat under the database's lock. Both are under one lock in
`Pages` now, `Memory`: the versions, the cache, the commit number. A
page read takes it once, shared, as it took the cache's before, and
looks in the versions and then the cache. Two locks would have meant
two for every page read, on the path §65.3 found too heavy already.
Reading a page from the file happens outside it, as before.

## 79.6 Poisoning, as before
§27.3's two ways still hold, checked by `read` and `write`:
- **A panic in the middle of a batch** poisons the writer's lock and
  may leave the store staging. Writes are refused. Reads are refused
  too, as they were, though the commit they would read is whole now:
  the gate isn't poisoned by a writer that never reached its publish.
  Letting reads go on would be a change of behavior to decide on its
  own; `Database::read` asks `Mutex::is_poisoned`, without taking the
  lock, to keep it.
- **A batch whose fate is unknown** (§19): the flag is an `AtomicBool`
  in `Shared`.

## 79.7 Measured
A cut-down `reader_wait` (§58), trunkdb only, the build before this
section and the one after: 20,000 small documents, two threads looking
them up by id, a third writing, 4 s per case, durable commits. Linux,
x86-64, **two cores**, a virtual disk: not the M1 Pro of §58, so only
before and after compare.

| writer | build | commits | reads | p50 | p99 | p99.9 | max |
|---|---|---:|---:|---:|---:|---:|---:|
| none | before | 0 | 2,877,661 | 2.5 µs | 5.9 µs | 24.6 µs | 6.4 ms |
| none | after | 0 | 2,877,058 | 2.6 µs | 5.2 µs | 22.1 µs | 4.0 ms |
| 10 commits a second | before | 39 | 2,895,532 | 2.5 µs | 5.4 µs | 22.5 µs | 17.8 ms |
| 10 commits a second | after | 40 | 2,803,881 | 2.6 µs | 5.6 µs | 22.2 µs | 7.2 ms |
| single inserts, back to back | before | 15,172 | 62,007 | 2.9 µs | 975 µs | 32.6 ms | 118.9 ms |
| single inserts, back to back | after | 10,384 | 1,932,848 | 2.8 µs | 31.2 µs | 94.5 µs | 40.5 ms |
| batches of 1000, back to back | before | 389 | 99,046 | 3.1 µs | 12.0 µs | 20.2 ms | 52.0 ms |
| batches of 1000, back to back | after | 205 | 1,818,534 | 3.0 µs | 7.5 µs | 36.7 µs | 10.3 ms |

- **Readers beside a writer that never pauses** do 31 and 18 times the
  reads, and p99.9 falls from tens of milliseconds to under 100 µs.
  What is left of the maximum is about what it is with no writer.
- **Readers alone** are unchanged: one lock per page read, as before.
- **The writer commits less often with two readers beside it**, a
  third to a half less. That is the two cores: three busy threads on
  them, where before the readers were asleep and the writer had a core
  to itself. With one reader, so a core each, the writer does as many
  commits after as before (15,523 and 13,744 single inserts, 373 and
  368 batches), and alone as well (16,496 and 12,576; 372 and 397),
  within what a rerun changes.

A second run of each gave the same picture. The full `reader_wait`,
with redb beside it, wasn't run: its dependencies couldn't be fetched
where this was written. To run on the machine of §58.

## 79.8 Tests
- `database.rs`:
  - a read on another thread comes back while a batch is staged, and
    again while it is logged and flushed and not yet published, both
    through `transact` itself, with the commit before; it waited for
    all of it until now;
  - a commit doesn't publish while a read is under way, and that read
    sees its commit to the end, reading the very pages the batch
    changed;
  - four threads writing at once: every batch lands whole;
  - three readers beside a writer that commits back to back,
    checkpoints after every commit, frees and takes pages and compacts,
    with a cache of no pages, of three, and the default: every `find`
    sums to the same total, the index and the ids agree, `check` finds
    nothing. Run a hundred times over, debug and release, without a
    failure;
  - a database whose batch's fate is unknown refuses reads, writes and
    checkpoints, and reopens with what was committed.
- The tests of §19, §27, §51, §77 and §78 run unchanged, but for where
  one reaches in: `Database::state()` hands out the writer and the last
  commit, and the injected write-back failures are set through
  `Pages::fail_write_backs`.
- Checked by breaking it on purpose, nine ways, each failing a test:
  the gate held from staging on, and from the log on; the pages
  committed before the gate; a checkpoint forgetting the versions
  before it writes the file, and before its flush; a checkpoint keeping
  the versions; a writer's panic not refusing reads; the snapshot not
  replaced at the publish; the unknown-fate flag not refusing writes.

## 79.9 Limits
- **A long read holds up commits, and the reads behind them** (79.3):
  for §80.
- **`compact`** rebuilds beside the readers now, and they wait only for
  its publish; writers wait for all of it, as before (§41).
- **A reader's panic** poisons nothing: read guards don't. A panic in a
  publish poisons the gate and the writer's lock together.
- **Not on Windows or macOS by measurement**, only by the tests in CI.
