# 53. A configurable checkpoint threshold (`database.rs`)

§52.4 left the threshold open, since a larger one buys speed with
memory and WAL size, which is the caller's trade. It is now an option:
`OpenOptions::checkpoint_pages(n)`, default 1,000 as before. No file or
WAL format changed.

## 53.1 The option
- A commit writes its pages back once `n` or more committed pages wait
  (§51). `0` and `1` therefore mean after every commit: two flushes each,
  as before §51, and a file that is always complete on its own.
- The value is kept in `State`, next to the code that applies it
  (`Database::transact`). `FileStore` only holds the waiting pages and
  writes them when told (`unwritten_pages`, `checkpoint`); when to do it
  is the commit protocol's decision, not the store's. (`cache_size` is
  in the store because the cache lives there.)
- **Not validated.** Small values are slow and correct, and a test
  wants `0` to force a checkpoint. Large ones are covered in §53.3.
  Rejected: refusing small values; letting `0` mean "the default",
  which would silently give a caller who asked for the most frequent
  checkpoints the least frequent one. SQLite's `wal_autocheckpoint`
  also takes 0.

## 53.2 Measurements
The benchmark of §48, trunkdb only, 100,000 documents, one run each on
the same machine (`BENCH_TRUNKDB_CHECKPOINT_PAGES`):

| `checkpoint_pages` | insert, 1000 per commit | update | delete | one insert per commit | file + WAL at the end |
|---|---:|---:|---:|---:|---:|
| 1,000 (default) | 19k/s | 8.2k/s | 13k/s | 5.1 ms | 45 MB |
| 4,000 | 20k/s | 8.7k/s | 21k/s | 5.2 ms | 72 MB |
| 16,000 | 27k/s | 15k/s | 18k/s | 5.2 ms | 1,032 MB |

Compaction is 1.1–1.2 s throughout. Single runs vary by 10–20%, so only
the 16,000 row is clearly different. The 4,000 row's gain is smaller
than in §52.4's scratch program (22k/s to 30k/s); the delete figure
is the only one that moved clearly.

## 53.3 The WAL grows faster than the threshold says
§52.4 named "a larger WAL" as a cost; the last column shows how much.
The threshold counts *distinct* waiting pages (`unwritten` holds the
newest image of each), so it bounds memory: 16,000 pages are 128 MB.
The WAL is not bounded by it. It gets every commit's dirty pages whole
(§19.2), so a page changed by 100 commits is 100 images in the WAL until
the next checkpoint, and 1,000 random-order inserts change over 1,000
pages, most of them index leaves. At 16,000, 100 commits of 1,000
documents left a WAL of nearly 1 GB beside a 45 MB file, all of which
the next `open` reads back after a crash.

So a large threshold is safe for memory and costly for disk and
recovery time. The option's doc comment says so, and the default stays
at 1,000. A second trigger, the WAL's size, would make a large
threshold reasonable; it is listed as open in ROADMAP.md.

## 53.4 Tests
- `checkpoint_pages_sets_when_a_commit_writes_back`: after one small
  commit, nothing waits with `0` or `1`, and some pages do with the
  default. With the threshold equal to the number of pages that commit
  leaves, nothing waits; one more, and all of them do.
- The tests of §51 run with the default and are unchanged, apart from
  the constant's new name.
- Checked by breaking it on purpose, three ways. Each fails a test:
  the option ignored (always 1,000); every commit checkpointing;
  `>` instead of `>=`. That last one survived the first version of the
  test, which had no case at the boundary.

## 53.5 Limits
- The value is fixed at `open_with`; nothing changes it on an open
  database (`Database::checkpoint` still forces one).
- No upper bound and no WAL-size trigger (§53.3).
