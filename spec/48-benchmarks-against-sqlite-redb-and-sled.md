# 48. Benchmarks against SQLite, redb and sled (`bench/`)

Real and measured: one document workload run against trunkdb and three
other embedded stores. It measures how far trunkdb is from them and,
more usefully, where its time goes. The remaining roadmap is mostly
performance work, and this is what orders it (§48.5).

```
cd bench
cargo run --release              # 100,000 documents, about two minutes
cargo run --release -- 20000     # smaller
BENCH_ONLY=trunkdb,redb cargo run --release
```

`bench/` is its own crate and its own workspace, excluded from the
package. The comparison databases (bundled SQLite, redb, sled) never
become part of trunkdb's build, tests or CI test runs. A separate CI
job builds it, lints it and runs it with 20,000 documents. Its times
don't count there; the job exists so the harness can't rot, and so the
four stores are checked to give the same answers on every push.

## 48.1 The workload
- **The documents:** tasks with `status` (three values), `created` (each
  value once, not in insertion order), `tenant` (100 values), a title
  of 20–60 characters, `tries` and zero to two tags.
- **The indexes:** `tenant`, `created` and `(status, created)`.
- **The steps, in order, on one fresh store each:**
  1. insert N documents, 1,000 per commit;
  2. insert 1,000 more, one commit each;
  3. 10,000 lookups by id;
  4. 100 × all tasks of one tenant (N/100 each);
  5. 100 × a range of 1,000 `created` values;
  6. 2,000 × the oldest 20 tasks of one status (`Eq` plus sort plus
     limit);
  7. 5 × count `tries > 7`, a field no index covers;
  8. about 10,000 updates that change `status`, `created` and the
     title, 1,000 per commit;
  9. 10,000 deletes, 1,000 per commit;
  10. the file size, compaction, and the size after it.

Every query decodes the documents it returns into the `Task` struct,
in every store.

## 48.2 Keeping it fair
- **Same data, same ids.** Every store gets the same documents under
  the same 16-byte UUIDv7 ids, and the random picks come from one seeded
  generator.
- **Same answers.** Each store's results (documents found, the created
  values of the last "oldest 20", the count) must equal trunkdb's, or
  the run fails. That caught nothing so far, but it's what makes the
  times comparable.
- **Durable commits everywhere.** trunkdb and redb are durable at
  commit by default, and sled is flushed after every commit. SQLite
  runs in WAL mode with `synchronous = FULL`, **and `fullfsync = ON`**.
  On macOS a plain `fsync` doesn't make data durable; Rust's
  `sync_all`/`sync_data` issue `F_FULLFSYNC`, and SQLite only does with
  that pragma. Without it SQLite committed in 70 µs, against about
  5 ms for a real flush on this machine.
- **What a user of each would write:**
  - SQLite gets a table with the indexed fields as columns, the
    document as JSON, and three SQL indexes. The unindexed count uses
    its own `json_extract`.
  - redb and sled get a table of JSON documents by id, plus one table
    of keys per index (the indexed values encoded to sort, then the
    id), kept up to date in the same transaction. That's what trunkdb
    does internally.
  - The others use JSON (serde_json), while trunkdb uses its own
    binary encoding. A binary format would make them faster still.
- **One thread**, one process, and warm OS caches, since each file has
  just been written.

## 48.3 Results
100,000 documents on an Apple M1 Pro (10 cores, macOS), Rust 1.98.1,
trunkdb 0.9.0, SQLite 3.53.2 (rusqlite 0.40), redb 4.3, sled 0.34.7,
on 2026-09-24:

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert 100000, 1000 per commit | 6267/s | 41k/s | 41k/s | 29k/s |
| insert 1000, one per commit | 16157.0 µs | 5290.7 µs | 5803.0 µs | 10314.2 µs |
| get by id | 31.9 µs | 18.4 µs | 2.7 µs | 3.6 µs |
| find tenant == x (1000 docs) | 4.68 ms | 1.92 ms | 1.06 ms | 1.87 ms |
| find created in a range (1000 docs) | 4.75 ms | 2.02 ms | 1.08 ms | 2.46 ms |
| status == x, oldest 20 | 19246.7 µs | 23.3 µs | 15.7 µs | 21.1 µs |
| scan: tries > 7, unindexed | 166.40 ms | 45.95 ms | 47.91 ms | 67.55 ms |
| update 9528, 1000 per commit | 5952/s | 13k/s | 18k/s | 24k/s |
| delete 10000, 1000 per commit | 11k/s | 21k/s | 23k/s | 32k/s |
| file size | 44.9 MB | 38.4 MB | 67.4 MB | 111.1 MB |
| compact | 6.99 s | 0.17 s | 0.45 s | — |
| file size after compact | 33.2 MB | 22.2 MB | 29.9 MB | — |

A second run stayed within about 10% everywhere, except trunkdb's
updates (3,841/s against 5,952/s). sled has no compaction.

## 48.4 What it shows
trunkdb is slower at everything except file size before compaction,
and by very different factors. Each factor has a cause in the code:

- **Oldest 20 by status, about 1,000× slower (19 ms against 16–23
  µs).** `BTreeIndex::range` collects every entry in the range before
  the sorted read looks at the first one (§34.2). "The oldest 20
  queued" therefore costs as much as all 33,000 queued tasks' index
  entries, and grows with the collection, not the limit. The lazy walk
  that §34.2 deferred is the fix. Measured against the 0.8.0 build with
  20,000 documents (3.2 ms against 3.7 ms), the group sort of §47 adds
  about 15% on top.
- **One commit, 3× slower than SQLite and redb (16 ms against 5–6
  ms).** A commit flushes three times: the WAL, the write-back to the
  main file, and the WAL's truncation (§19.3). Each is an `F_FULLFSYNC`
  of about 5 ms here, and a profile of the first 12 seconds (the
  inserts) found two thirds of the samples in `fcntl`. The third flush looks unnecessary:
  replaying a complete WAL record again only rewrites pages the
  write-back already wrote, and the next commit's flush makes the
  truncation durable anyway. That needs a crash test before it
  changes. Getting down to one flush means not writing back on every
  commit: readers find the newest pages in the WAL, and a checkpoint
  writes them back later, as SQLite's WAL mode and redb do.
- **Lookups by id, 10× slower than redb (32 µs against 3 µs).** There
  is no page cache. Every page read is a `pread`, a checksum over 8 KB
  (§40) and an 8 KB allocation, and a lookup reads the primary index's
  path plus the data page. The index and range queries (2.5–4× slower)
  and the full scan (3.5× slower) pay the same per page.
- **Batched writes, 4–7× slower.** The flushes explain only 16 ms of a
  160 ms batch. The rest is page work: each of the four B-tree inserts
  per document reads its path from the file (again, no cache), and
  every changed page is logged whole (8 KB) to the WAL and written
  back. With `created` in random order, a batch of 1,000 changes
  hundreds of pages. (A profile later found the cost elsewhere: every
  insert rebuilt its leaf from copies of all its keys. §52 fixed that.)
- **Compaction, 15–40× slower (7 s).** It rebuilds through ordinary
  B-tree inserts into memory, then logs the whole new image to the WAL
  and writes it back (§41).
  (Since §52, which made those inserts cheap: 1 s.)
- **Size:** smaller than redb and sled before compaction, 17% larger
  than SQLite. After compaction it's about redb's size, and SQLite's
  file is a third smaller. Where trunkdb's extra third goes (document
  encoding, cells, index keys) isn't measured yet.

## 48.5 What it changes on the roadmap
Three new items, and one old one moved to the front, in order of
measured gap and effort:
1. **The lazy B-tree walk** (§34.2, done in §49): 1,000× on the one query compound
   indexes were built for (§43), and a contained change in `btree.rs`
   and the sorted read.
2. **A page cache** (done in §50), which is new: about 10× on lookups and 2.5–4× on
   every other read. It holds decoded or checked pages in memory, with
   dirty pages already staged per batch (§19.3). Its size and eviction
   are the design questions.
3. **Fewer flushes per commit** (done in §51, straight to one), also new: the first step (three to
   two) is small, and the second (one, with the WAL read at lookup)
   builds on the page cache.
4. **Faster compaction**, new: building leaves directly from sorted
   entries instead of inserting them one by one.

Rejected: optimizing before measuring again. Each of these gets its
number from this benchmark before and after.

## 48.6 Limits
- **One machine, one run per figure**, two runs compared. Linux's
  `fsync` costs differently, and CI runs the benchmark on Linux without
  looking at the times.
- **One thread.** Nothing measures readers during a write batch, which
  is where MVCC (ROADMAP.md) would show.
- **Warm caches only**; a cold start (first read after a reboot) isn't
  measured.
- **JSON for the others**, where a binary format would be faster.
- **sled 0.34** is the last stable release, and 1.0 has been in alpha
  for years.
