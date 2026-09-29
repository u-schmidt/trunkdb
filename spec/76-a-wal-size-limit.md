# 76. A WAL size limit (`database.rs`, `durability/`)

§53 made the checkpoint threshold a count of *distinct* waiting pages,
and §53.3 found what that leaves open: the WAL holds every commit's
dirty pages whole, so commits that change the same few pages again and
again add an image each time while the count stays low. A large
threshold, wanted for speed on batched writes, then meant a WAL of
1 GB beside a 45 MB file, all of it read back by the next `open` after
a crash. A commit now also writes back once the WAL itself is long
enough.

## 76.1 The rule
After a commit, `Database::transact` checkpoints if either
- `unwritten_pages() >= checkpoint_pages` (§53), which bounds memory, or
- `WalDurability::len() >= checkpoint_wal_bytes`, which bounds disk and
  the time to recover.

`Durability` gains `len`, the log's length in bytes as of the last `log`
or `checkpoint`, kept in the struct so a commit does not ask the file
system. At `open` it is what was read, before the checkpoint that
empties it.

## 76.2 The option and its default
`OpenOptions::checkpoint_wal_bytes(bytes)`. Unset, it is
`2 * checkpoint_pages * PAGE_SIZE`: 16 MB at the default 1,000 pages.
- The WAL holds at least one image of every waiting page, so a limit
  below `checkpoint_pages * PAGE_SIZE` would always fire first and make
  the page threshold meaningless. Twice that lets a spread-out workload
  reach the page threshold first and a concentrated one hit the byte
  limit after about two images per waiting page.
- The default follows `checkpoint_pages`, so raising the one raises the
  other; setting the limit itself overrides it. It is not validated, as
  in §53.1: `0` checkpoints after every commit.
- The limit is checked after a commit, so one large commit can take the
  WAL past it. Refusing it, or splitting it, would break its atomicity
  (§19).
- The new default is stricter than before for a workload that rewrites
  hot pages: at 1,000 pages the WAL now stops at 16 MB where it grew
  without bound. No file or WAL format changed.

## 76.3 Measurements
The benchmark of §48, trunkdb only, 100,000 documents, one run each
(`BENCH_TRUNKDB_CHECKPOINT_PAGES`, `BENCH_TRUNKDB_CHECKPOINT_WAL_MB`).
"File + WAL" is the size at the end of the run, so it shows the WAL's
peak:

| `checkpoint_pages` | WAL limit | insert, 1000 per commit | update | delete | one insert per commit | file + WAL |
|---|---|---:|---:|---:|---:|---:|
| 1,000 | 16 MB (default) | 19k/s | 7.8k/s | 12k/s | 347 µs | 42 MB |
| 16,000 | none | 22k/s | 7.5k/s | 11k/s | 372 µs | 1,043 MB |
| 16,000 | 256 MB | 26k/s | 8.2k/s | 17k/s | 279 µs | 263 MB |

Single runs vary by 10–20%, so only the last column is clearly
different: the limit keeps most of what 16,000 pages buy for inserts
and deletes and a quarter of its disk. This benchmark's 1,000-document
commits change many pages, so it shows the limit as a guard, not as a
gain; the hot-page case is covered by the tests. The default stays at
1,000 pages; raising it is now safe to do without also risking the WAL.

## 76.4 Tests
- `checkpoint_wal_bytes_bounds_the_wal_when_few_pages_change`: 200
  updates of one document leave 200 images with the limit off and
  never a WAL as long as a 20-image limit with it on.
- `checkpoint_wal_bytes_defaults_to_twice_the_page_threshold`: at 5
  pages the WAL stays under 40 images; with the limit raised it doesn't.
- The tests of §51 and §53 run unchanged.

## 76.5 Limits
- Fixed at `open_with`, like §53.5.
- The limit counts bytes on disk, not recovery time, which depends on
  the disk.
