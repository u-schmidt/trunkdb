# 123. Batched writes: a checkpoint less often (`database.rs`, `storage/file.rs`, `storage/pages.rs`)

Batched writes were the one place where trunkdb was clearly behind. On
the benchmark's 100,000 tasks (§48), 1,000 documents a commit, the
median of three runs for 0.20.0:

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert | 22k/s | 42k/s | 37k/s | 28k/s |
| update | 9.1k/s | 12k/s | 17k/s | 21k/s |
| delete | 15k/s | 17k/s | 21k/s | 33k/s |

The ROADMAP blamed the checkpoint threshold: "nearly every batch of 1,000
crosses the default checkpoint threshold once the indexes are large".
§53 made the threshold an option, §76 bounded the WAL so a large one is
safe, and the default stayed at 1,000 pages "to be raised only with a
benchmark of more workloads than one". This section measures first, then
raises the default to 4,000 pages, and leaves the rest of the gap
described for later.

## 123.1 Where the time goes
A probe timed each commit of 100,000 tasks inserted 1,000 a commit, and
watched the WAL file:

| | 1,000 pages | 4,000 pages |
|---|---:|---:|
| commits that checkpoint | 59 of 100 | 10 of 100 |
| a commit that checkpoints | 57–58 ms | 59–61 ms |
| a commit that doesn't | 26–27 ms | 29–30 ms |
| WAL written per commit, at the end | 6.5 MB | 10.2 MB |
| inserts | 22k/s | 30–31k/s |

The same builds with every flush skipped (`--cfg fuzzing`, §55.1) insert
41–47k/s, SQLite's speed. The CPU work is level; what was behind is
waiting for the disk:
- **A checkpoint costs about 30 ms** on top of its commit: every waiting
  page written back, the file flushed, the WAL truncated and flushed
  again, three full flushes (`F_FULLFSYNC`) instead of one. At 1,000
  pages, 59 commits of 100 paid it.
- **The WAL gets every changed page whole** (§19.2): 1,000 inserts in
  random index order change 800–1,250 pages, mostly index leaves, so a
  commit of about 200 KB of documents writes 6–10 MB to the WAL and
  flushes it. SQLite does the same with 4 KB pages, half the bytes.
- **CPU**, from a stack sample of the run without flushes: writing the
  WAL record 17%, copying a page on its first change (`make_mut`) 6%,
  checking a node's layout on every read (`check_layout`) 6%, allocation
  about 15%; the rest the B-tree and encoding.

Why 59 of 100: a commit of 1,000 tasks leaves 800–1,250 distinct pages
waiting, so 1,000 was crossed after one or two commits. The threshold
was too small for a batch the size of the benchmark's.

## 123.2 Thresholds measured
The §48 benchmark, trunkdb only, two runs each, before the change
(`BENCH_TRUNKDB_CHECKPOINT_PAGES`; the WAL limit at its default, twice
the pages' bytes, unless given):

| pages | WAL limit | insert | update | delete | one insert a commit | get by id | file + WAL at the end |
|---|---|---:|---:|---:|---:|---:|---:|
| 1,000 | 16.4 MB | 23–24k/s | 8.7–10k/s | 15–17k/s | 4.6 ms | 2.5 µs | 42 MB |
| 2,000 | 32.8 MB | 26–31k/s | 7–11k/s | 19–22k/s | 4.5–4.7 ms | 2.9 µs | 56 MB |
| **4,000** | **65.5 MB** | **30–33k/s** | **12–13k/s** | **26–27k/s** | 4.4–4.7 ms | 2.5–3.2 µs | 42 MB |
| 8,000 | 131 MB | 34k/s | 15–18k/s | 26–28k/s | 4.4–4.6 ms | 2.2–3.2 µs | 150 MB |
| 16,000 | 256 MB (given) | 34–35k/s | 16k/s | 33k/s | — | — | 263 MB |
| 64,000 | 1 GB (given) | 36k/s | 19k/s | 31–32k/s | — | — | 1,043 MB |

- **4,000 takes most of the gain:** inserts 1.35×, updates 1.35×,
  deletes 1.6×. Past it the gain flattens while the WAL grows: 8,000
  adds a few percent to inserts for twice the WAL.
- **Single commits and reads don't move:** a commit of one document
  never comes near either threshold.

First written on 2026-10-18 against `b11f7fe` as §108; that number went
to compaction built in a file. Every figure above was measured again on
2026-10-22 against 0.23.0, with the same results.

## 123.3 The change
- **`DEFAULT_CHECKPOINT_PAGES` is 4,000**, 32.8 MB of waiting pages. The
  WAL limit follows it (§76.2): twice their bytes, 65.5 MB. Both stay
  options (`OpenOptions::checkpoint_pages`, `checkpoint_wal_bytes`); a
  caller who set them sees no change. No file or WAL format, no API.
- **A compaction's commit writes back at once**, however small its
  image. §108.3 says it does: its pages count as waiting, "so the
  checkpoint follows the commit at once". That held because any image of
  more than 1,000 pages crossed the threshold. At 4,000 an image under
  32.8 MB didn't, and waited in the WAL: the benchmark's file plus WAL
  after `compact` measured 73.4 MB instead of 31.0, the shrunk file only
  reaching the disk at a later commit or the close. Now a commit also
  checkpoints while a replacement waits (`FileStore::replacement_waits`),
  as it does past either threshold: a failed write-back is no error for
  the commit, and the next one tries again.

## 123.4 What it costs
- **Memory:** waiting pages are versions held beside the cache (§80), up
  to 32.8 MB instead of 8.2, beside the 256 MiB cache (§50). Peak RSS of
  the trunkdb-only benchmark was 387–542 MB at 4,000 and 420–532 MB at
  1,000, three runs each: the cache and the allocator move it more than
  the threshold does.
- **Disk:** the WAL up to 65.5 MB plus one commit, from 16.4 MB. It is
  empty after a clean close.
- **Recovery after a crash:** the open writes the WAL back. With the WAL
  at its largest, 16.3 MB took 33–34 ms at 1,000 pages and 65.5 MB
  79 ms at 4,000 (260 MB took 363 ms at 16,000).
- **A read-only open of a file a crash left** (§88) keeps the WAL's
  latest image of each page in memory instead of writing it back: up to
  about 33 MB instead of 8. The `trunkdb` command (`find`, `fields`,
  `info`, `check`, `export`) and the Agent Skill (§122) open read-only;
  after a clean close there is nothing to hold.
- **A checkpoint's commit** takes about as long as before (59–61 ms
  against 57–58) and comes once in ten commits instead of six in ten.
- **Snapshots** (§80, §83): versions kept for open snapshots are counted
  and limited apart from the waiting pages. Readers beside a writer
  (`reader_wait`) and a long snapshot beside single writes (`long_read`)
  measured the same before and after (123.6).

## 123.5 What stays for later
What remains of the gap, in the order it would likely pay:
- **WAL records of what changed, not whole pages.** 10 MB written and
  flushed per commit for 200 KB of documents. The first image of a page
  after a checkpoint has to stay whole: a checkpoint can tear a page in
  the file, and only a whole image repairs it (PostgreSQL's
  `full_page_writes`). Later images in the same interval could be the
  changed byte ranges; at 4,000 pages a hot index leaf is logged about
  nine times an interval. WAL version 4, and recovery applying ranges
  onto pages: a spec of its own.
- **A checkpoint without the WAL's own flush.** The truncation is
  flushed so that an old record can't be replayed after a newer one. A
  generation number in the WAL header and every record (SQLite's salt)
  would make old records unreadable instead, saving one flush per
  checkpoint: about 1% at 4,000 pages, so only together with the above.
- **CPU on the write path:** `check_layout` on every read of a node
  already checked when it entered the cache, and the page copy on first
  change, 6% each without flushes.

## 123.6 Measured after
The §48 benchmark, all four stores, median of three runs:

| | before (0.20.0) | after | SQLite | redb | sled |
|---|---:|---:|---:|---:|---:|
| insert, 1000 per commit | 22k/s | 32k/s | 44k/s | 45k/s | 30k/s |
| update, 1000 per commit | 9.1k/s | 13k/s | 16k/s | 19k/s | 24k/s |
| delete, 1000 per commit | 15k/s | 26k/s | 20k/s | 20k/s | 37k/s |
| one commit | 4.7 ms | 4.7 ms | 4.7 ms | 4.9 ms | 10.2 ms |
| get by id | 2.5 µs | 2.8 µs | 6.1 µs | 2.0 µs | 2.6 µs |
| compact | 1.1 s | 1.2 s | 0.16 s | 0.42 s | — |
| file size after compact | 31 MB | 31 MB | 22 MB | 30 MB | — |

The three runs of the after column: inserts 32, 33 and 32k/s, updates
13k/s each time, deletes 26, 26 and 25k/s. The other stores measured
faster on this day than in 0.20.0's table (SQLite inserts 44k against
42k): the same machine, under other load. Against them, inserts are
0.73× SQLite's (from 0.52×), updates 0.81× (from 0.76×), and deletes
1.3× (from 0.88×). Reads and single commits moved only within run-to-run
noise; the README's and DESIGN's tables are these runs.

Beside a writer (`reader_wait`, two runs each), reads took the same
before and after: medians 1.9–2.5 µs, p99 4.2–6.0 µs throughout, the
largest outliers single scheduling stalls on a loaded machine either
way. A long snapshot beside 15 s of single writes (`long_read`): commit
p50 4.3 ms and p99 7.1–9.1 ms before and after, 1.4 MiB kept.

## 123.7 Rejected
- **A threshold that adapts** (to the cache, the batch, the WAL's
  growth): harder to predict and to test, for a gain one fixed value
  mostly takes.
- **8,000 or more by default:** a few percent more inserts for twice the
  WAL and recovery time. A caller who wants it sets it.
- **A checkpoint in a background thread:** it would take the 30 ms off
  the commit, but needs the WAL to go on while its older part is
  written back (two WAL files, or one never truncated). Nothing measured
  says commit latency, not throughput, is what callers lack.
- **A smaller page size** (4 KiB, as SQLite): half the WAL bytes, but a
  file format change and deeper B-trees for large documents.
- **Leaving a compaction to the next checkpoint** (123.3): `compact` is
  asked for to shrink the file, and §108.3 promised it at once.

## 123.8 Tests
- `the_defaults_are_4000_pages_and_a_wal_of_twice_their_bytes`: a
  database opened without options has 4,000 and 65,536,000.
- `a_commit_checkpoints_once_enough_pages_wait`: commits of about 50
  pages at the default; the one that checkpoints left fewer than 4,000
  waiting before it and at least 3,940, so neither an earlier nor a
  later threshold passes. It now names 4,000 as a number, where it read
  the constant and moved with it.
- `a_small_compaction_is_in_the_file_when_it_returns`: a compaction of
  112 pages, far under the threshold, leaves the file at its new length,
  the WAL empty, nothing waiting and the image's file gone, without a
  `checkpoint` call.
- Two tests that held a compaction in the WAL by the threshold now hold
  it with a failed write-back: `a_batch_in_several_records_is_recovered_whole_or_not_at_all`
  (the compaction's own checkpoint fails before its first page) and
  `a_compaction_cut_short_is_completed_by_the_next_open` (its own
  checkpoint and the one at drop fail, as its comment always said).
- **Mutations**, each caught: the default back to 1,000 (by both
  tests above on the default); the WAL limit fixed at 16 MB instead of
  following the pages (by the default's test and two of the WAL
  limit's); a
  compaction's commit not writing back (by the compaction test).
- The full test suite, clippy, and 2,000 programs of the `queries`
  smoke run (seed 123124). Seed 123123 found a filter whose text doesn't
  read back the same (an `in` of values of two types); it fails without
  this change too, and is left to its own fix.
