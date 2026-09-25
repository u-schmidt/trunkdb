# 50. A page cache (`storage/cache.rs`, `storage/file.rs`, `database.rs`)

Real and measured: pages read from the file stay in memory, up to a
size the caller chooses (256 MiB by default). A lookup by id went from
30 µs to 6.7 µs in the benchmark (§48). That's faster than SQLite's
16 µs, and close to sled (5.7 µs) and redb (2.4 µs).

```rust
use trunkdb::{Database, OpenOptions};

let db = Database::open("app.trunkdb")?;                     // 256 MiB at most
let db = Database::open_with("big.trunkdb", OpenOptions::default().cache_size(1 << 30))?;
let db = Database::open_with("tiny.trunkdb", OpenOptions::default().cache_size(0))?;   // none
```

## 50.1 What it holds
- **Committed pages only**, checked, as they are in the file. A read
  looks in this order:
  1. the batch's staged pages, while a batch runs (§19.3);
  2. the cache;
  3. the file, with the checksum verified (§40), after which the page
     goes into the cache.

  A staged page therefore never reaches the cache before it's in the
  file, and a rolled-back batch leaves nothing behind.
- **After a write-back**, every page written goes into the cache,
  replacing its older copy. Those are the pages the next reads want: a
  B-tree's path, the current data page.
- **A write-back that fails halfway** clears the whole cache. The file
  then holds some new pages and some old, and the cache can't know
  which (§19.6). Crash recovery (`restore_pages`) clears it too, before
  it writes. Pages cut off the end of the file (§41) leave it.
- **Eviction is CLOCK** (second chance). A hand sweeps the slots, spares
  once each page read since it last passed, and replaces the first that
  wasn't read. A read only sets a flag, with no reordering and no list to
  maintain, which is close enough to least-recently-used for a page
  cache.
- **A mutex around it:** reads take `&self` and run in parallel under
  the read lock (§27), and a cache read changes the cache, both the flag
  and a page put in on a miss. It's held for a copy of one page.

## 50.2 What a read costs now
`read_page` copies the page out of the cache into a new `Vec`, because
`PageStore` hands out owned pages that `SlottedPage` may change. The
first version filled a zeroed 8 KB buffer and copied into it, and a
profile of two million lookups found `memset` second only to `memmove`.
Allocating straight from the cached bytes (`read_vec`) took a lookup
there from 4.6 to 3.8 µs. What's left is mostly that copy. Sharing
cached pages instead (an `Arc` handed out, and a copy only to change
it) would remove it; that changes `PageStore` and is left for later.

## 50.3 Damage and `check`
A page damaged on disk after it was cached keeps being read from the
cache: the checksum is checked once, when the page comes in. `check`
(§39) still finds the damage, because its damaged-page scan reads the
file itself (`read_disk_page`) and not through the cache. A test
damages a cached page on disk and checks both.

## 50.4 The size
- **`OpenOptions`** is new: `Database::open_with(path, options)`, with
  `open` using the default. It's `#[non_exhaustive]`, made with
  `OpenOptions::default()` and setters, so options added later break no
  caller.
- **The default is 256 MiB.** The cache fills only as pages are read,
  so a small file costs its own size and no more. redb and sled default
  to 1 GiB. SQLite defaults to 2 MB, but reads through the OS cache,
  with no checksum to verify.
- **The first default was 32 MiB**, and the benchmark showed it was too
  small for its own 45 MB file:

  | 100,000 documents | no cache | 32 MiB | 256 MiB |
  |---|---:|---:|---:|
  | get by id | 30 µs | 26 µs | 5–7 µs |
  | find tenant == x (1000 docs) | 4.6 ms | 3.2 ms | 2.2–2.4 ms |
  | status == x, oldest 20 | 92–126 µs | 118 µs | 46–47 µs |
  | scan, unindexed | 162–179 ms | 214 ms | 151–152 ms |

  Random lookups over a working set bigger than the cache mostly miss.
  A full scan got *slower* with the small cache: every page it passed
  was copied in, and replaced one a lookup would have wanted.

Rejected:
- **A default the size of the file, or no limit.** An embedded
  database shouldn't grow to its file's size in memory without being
  asked.
- **Memory-mapping the file**, as LMDB does. Reads would cost almost
  nothing, but a failing disk would become a `SIGBUS` instead of an
  error, and each page's checksum would have to be verified on some
  first use rather than when it's read. The positional reads and writes
  work the same on every platform.

## 50.5 Measured
100,000 documents, all four stores, on the same machine as §48.3:

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert 100000, 1000 per commit | 6800/s | 43k/s | 44k/s | 27k/s |
| insert 1000, one per commit | 16158 µs | 4990 µs | 5611 µs | 11073 µs |
| get by id | 6.7 µs | 16.0 µs | 2.4 µs | 5.7 µs |
| find tenant == x (1000 docs) | 2.21 ms | 1.61 ms | 0.96 ms | 1.68 ms |
| find created in a range (1000 docs) | 2.19 ms | 1.71 ms | 1.57 ms | 2.11 ms |
| status == x, oldest 20 | 46.6 µs | 22.5 µs | 15.4 µs | 21.0 µs |
| status == x, newest 20 | 49.5 µs | 22.9 µs | 15.4 µs | 21.6 µs |
| scan: tries > 7, unindexed | 151 ms | 44 ms | 49 ms | 67 ms |
| update 9528, 1000 per commit | 5852/s | 15k/s | 20k/s | 24k/s |
| delete 10000, 1000 per commit | 11k/s | 21k/s | 25k/s | 37k/s |

Reads are now within 1.3–3× of the others, and lookups by id beat
SQLite's. The unindexed scan (3×) spends its time decoding documents,
not reading pages. Writes haven't changed: they're bound by three
flushes per commit, which is the next item (§48.4).

`BENCH_TRUNKDB_CACHE_MB` sets trunkdb's cache size in the benchmark;
the others run with their defaults.

## 50.6 Tests
- `storage/cache.rs`:
  - pages kept and replaced;
  - a full cache evicting the page nobody read, and sparing the ones
    read;
  - `retain`, `clear` and `resize`, with pages still found after slots
    moved;
  - a cache of size 0 keeping nothing.
- `storage/file.rs`, one test for each way a stale page could appear:
  - a page read twice comes from the file once (hits and misses
    counted);
  - size 0 reads the file every time;
  - a rolled-back staged page isn't read back, and a written-back one
    is, from the cache;
  - a write-back that failed halfway leaves reads saying what the file
    says;
  - restored pages replace cached ones;
  - pages cut off the end leave the cache;
  - a page damaged after it was cached is still found by the scan.
- `database.rs`: `open_with` with no cache, three pages and 1 MiB reads
  the same 500 documents, twice each; the default is 256 MiB.
- Every other test runs with the cache on, as a database opens by
  default.
- Checked by breaking it on purpose, fourteen ways. Each of these fails
  a test:
  - a page read from the file not put in;
  - written-back pages not put in, leaving an older copy;
  - a failed write-back, or a recovery, not clearing it;
  - cut-off pages kept;
  - a page written outside a batch not updated in it;
  - the cache asked before the batch's staged pages;
  - a page from the file not checked;
  - CLOCK ignoring the flag, or new pages starting as read;
  - an older copy not replaced;
  - slots moved without their index;
  - `resize` not shrinking, or never called.

## 50.7 Limits
- **Not scan-resistant.** A scan over more pages than the cache holds
  pushes out everything else. Databases often keep scanned pages out of
  the cache, or put them in a probationary part of it; it wasn't needed
  at 256 MiB.
- **A copy per page read** (§50.2).
- **One mutex.** Many threads reading at once contend on it; a sharded
  cache would spread them.
