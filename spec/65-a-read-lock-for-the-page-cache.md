# 65. A read lock for the page cache (`storage/cache.rs`, `storage/file.rs`)

Shared pages (§64) shortened the time a reader holds the page cache's
mutex to a lookup, but four readers still did only 10% more than two:
every page read took the mutex, and with four threads taking it
millions of times a second, two met often, and macOS put one to sleep
in the kernel each time (§64.2).

A lookup needn't exclude other lookups. Now it doesn't: the cache sits
behind an `RwLock`, and readers look pages up under its read lock,
together.

## 65.1 What a lookup changes
The only thing a lookup changed was CLOCK's reference flag (§50): "read
since the hand last passed". It became an `AtomicBool`, so
`PageCache::get` takes `&self`, and a shared reference is what a read
lock hands out. Everything else in a lookup, the hash map and the
slot's `Page`, is only read.

- **`get` sets the flag only if it isn't set.** The pages every lookup
  reads, the B-tree's root first, would otherwise have their flag
  written by every reader, and each write takes the cache line from
  the other cores. The flag is a hint for eviction, so `Relaxed` is
  enough: a flag set a moment late costs at most a page evicted a
  sweep early.
- **`put`, `retain`, `resize` and `clear` take the write lock**, as
  before they took the mutex. A read that misses takes it to put the
  page in; after the first reads, with the default 256 MiB, lookups
  hardly ever miss.
- **`FileStore::cache` is the read lock and `cache_mut` the write
  lock.** A poisoned lock is still taken over, for the same reason as
  before (§50).
- **The counters tests read** (`hits`, `misses`) are atomics too, and
  exist only in test builds.

Rejected:
- **Keeping the mutex and sharding the cache** (§64.1 said why: every
  lookup reads the same hot pages).
- **A lock-free map** (a crate like `dashmap`, or a concurrent hash map
  of our own): a new dependency or a lot of subtle code, for a
  structure that changes only when a page comes in from the file.
- **Dropping the flag on reads**, so a lookup writes nothing at all:
  CLOCK would degrade to FIFO, and the pages every lookup needs would
  be evicted as readily as a page one scan touched once.

## 65.2 Measured
M1 Pro (8 performance and 2 efficiency cores), `reader_wait` (§58)
with trunkdb alone, 3 s per case, run one after the other: before
§64, after it, and after this change. Reads in 3 s, with no writer:

| readers | before §64 | after §64 | after §65 | p50 after §65 |
|---:|---:|---:|---:|---:|
| 1 | 732,869 | 970,503 | 980,485 | 2.8 µs |
| 2 | 702,943 | 1,494,563 | 1,798,432 | 3.2 µs |
| 4 | 751,869 | 1,741,679 | 3,096,131 | 3.8 µs |
| 8 | 597,425 | 955,888 | 3,125,606 | 6.9 µs |

- **Four readers do 3.2 times the work of one**, and p99 falls from
  51 µs to 6 µs: a reader rarely waits at all.
- **With a writer doing batches of 1,000 back to back**, four readers
  read 3,984,486 times, against 963,018 before §64.
- **Eight readers do no more than four.** A profile of eight has 33
  samples waiting in the kernel, against 6,651 for four readers before
  this change: nobody sleeps on the lock anymore. The time goes to
  `read_current` itself, then to checking and reading pages
  (`SlottedPage::from_bytes`, `get_cell`, §64.3). Most likely it's the
  atomic writes each page read makes to memory every core shares: the
  lock's count of readers, and the page's reference count, both taken
  and given back for every page of every lookup. Not taken further:
  keeping the reference count in an allocation of its own
  (`Arc<Vec<u8>>`), away from the page's first bytes, made no
  difference, so it isn't false sharing with the page header.
- **One reader is unchanged:** an uncontended read lock costs what an
  uncontended mutex did.

## 65.3 Limits
- **Eight readers are no faster than four** (65.2). Taking the cache's
  read lock once per operation instead of once per page, or pages
  that don't need a reference count to be read (the cache's own,
  borrowed while its lock is held), would cut the shared writes; both
  change more than this step should.
- **A read that misses waits for the write lock**, and so does every
  reader behind it. After a cold start, or with a cache smaller than
  the working set, readers take turns again while pages come in.
  `std`'s `RwLock` makes no promise about fairness between readers and
  a waiting writer.
- **Readers still wait for a whole commit** (§57, §58.3): this is the
  cache's lock, not the database's.

## 65.4 Tests
- `file.rs`: while one thread holds the cache's read lock, another
  thread's read of a cached page goes through; with a mutex, or a
  lookup under the write lock, it waits, and the test fails after ten
  seconds instead of hanging.
- `cache.rs`: the CLOCK tests look pages up through `&self` now; a
  page put in again counts as used and is spared once (a gap from §50
  the mutations below found).
- Checked by breaking it on purpose, three ways. Each of these fails a
  test:
  - lookups under the write lock;
  - a lookup that never sets the flag;
  - a page put in again not marked as used.
