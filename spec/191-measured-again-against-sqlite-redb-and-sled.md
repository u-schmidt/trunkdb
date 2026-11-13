# 191. Measured again, against SQLite, redb and sled (`bench/`, `DESIGN.md`, `README.md`)

The tables in DESIGN and the README took trunkdb from §172 and the
other three from §153, each measured on its own day. §190 found 0.40.0
as fast as 0.34.0; this measures all four again, side by side, and
checks whether the machine still gives the numbers those tables have.
Nothing in the library changed.

## 191.1 How
Apple M1 Pro (8 performance and 2 efficiency cores, macOS), Rust
1.98.1, 2026-11-13, trunkdb 0.40.0 at `ef92a25`, SQLite 3.53.2
(rusqlite 0.40), redb 4.3, sled 0.34.7, as `bench/Cargo.lock` pins
them. Release builds, run one after the other, nothing else running:

- The comparison (§48): 100,000 tasks, all four stores, three runs.
- `reader_wait` (§58): trunkdb and redb, two readers, 10 s per case,
  three runs.
- `long_read` (§83), `select` (§106) and `aggregate` (§107, §126): once
  each.

## 191.2 The comparison
Medians of three runs:

| | trunkdb | SQLite | redb | sled |
|---|---:|---:|---:|---:|
| insert 100000, 1000 per commit | 43k/s | 40k/s | 44k/s | 29k/s |
| insert 1000, one per commit | 5008.8 µs | 5194.4 µs | 5690.2 µs | 10655.4 µs |
| get by id | 2.0 µs | 17.9 µs | 3.5 µs | 3.2 µs |
| find tenant == x (1000 docs) | 0.89 ms | 1.82 ms | 1.07 ms | 1.97 ms |
| find created in a range (1000 docs) | 1.02 ms | 2.02 ms | 1.12 ms | 2.18 ms |
| status == x, oldest 20 | 18.6 µs | 23.0 µs | 16.3 µs | 21.2 µs |
| status == x, newest 20 | 25.0 µs | 24.3 µs | 16.1 µs | 22.2 µs |
| first 50 by id | 36.3 µs | 38.1 µs | 26.1 µs | 28.9 µs |
| scan: tries > 7, unindexed | 38.61 ms | 44.83 ms | 53.42 ms | 69.24 ms |
| count tenant == x (1000 docs) | 0.01 ms | 0.03 ms | 0.04 ms | 0.27 ms |
| count created in a range (1000 docs) | 0.02 ms | 0.02 ms | 0.04 ms | 0.34 ms |
| count, no conditions (100000 docs) | 0.07 ms | 0.01 ms | 0.00 ms | 24.9 ms |
| status == x by created, page 50 of 20 | 21.1 µs | 53.4 µs | 43.2 µs | 251.4 µs |
| 50 by id after skipping 5000 | 35.8 µs | 125.6 µs | 226.3 µs | 924.7 µs |
| tenant == x by id, first 20 | 21.6 µs | 1275.1 µs | 20.7 µs | 25.2 µs |
| tenant in (x, y) by id, first 20 | 29.2 µs | 2526.0 µs | 19.7 µs | 26.1 µs |
| tenant in (x, y) by id, page 25 of 20 | 72.7 µs | 2829.4 µs | 68.0 µs | 290.7 µs |
| update 9528, 1000 per commit | 20k/s | 14k/s | 18k/s | 22k/s |
| delete 10000, 1000 per commit | 34k/s | 19k/s | 24k/s | 34k/s |
| file size | 24.6 MB | 38.4 MB | 67.4 MB | 108.9 MB |
| compact | 0.64 s | 0.18 s | 0.45 s | — |
| file size after compact | 18.0 MB | 22.2 MB | 29.9 MB | — |

trunkdb's `get by id` was 2.0, 2.0 and 6.5 µs, its first run's first
read the slow one; §190's twelve runs took 2.1–3.7 µs, so the tables
keep "2–4 µs". Its "first 50 by id" ran 31–44 µs.

## 191.3 Has the machine changed?
Hardly. Against §153's numbers for the other three, most rows moved
less than a tenth, both ways:

- **The same:** SQLite's inserts (40k/s), updates (14k/s), deletes
  (19k/s), reads by index, pages and skips; redb's and sled's walks,
  counts and scans; every file size.
- **A little slower, all three:** one commit, 5–14% (SQLite 4.7 →
  5.2 ms, redb 5.0 → 5.7, sled 10.1 → 10.7). The disk's flush, which
  every store pays alike.
- **Slower for redb and sled:** `get by id`, redb 2.0 → 3.5 µs, sled
  2.3 → 3.2. Every run, so not noise; sled's deletes 36k → 34k/s.
- **Faster:** redb's inserts, 41k → 44k/s.

trunkdb's batched inserts are 43k/s here and 45k/s in §190, against
48k/s in the tables. 0.34.0, built and run beside 0.40.0 the same day,
gave 45.5k/s (§190.2), so what moved is the day, not the releases
since §172. Its updates (22k → 20k/s) and deletes (36k → 34k/s) moved
with it.

## 191.4 Where trunkdb stands now
- **Reads ahead or level** everywhere but three rows: the count of all
  documents (trunkdb counts leaf by leaf, the others keep a total);
  "first 50 by id", 1.4× redb (1.2× before); "two tenants' first 20 by
  id", 1.5× redb (1.3× before). trunkdb's first 50 ran 31–44 µs, so
  that one is partly noise; the two tenants' 28–30 µs every run, so it
  isn't.
- **The scan**, 38.6 ms, is a little ahead of SQLite's 44.8.
- **Writes:** inserts 1.08× SQLite's (1.2× in §172's runs), level with
  redb; updates 1.4× SQLite's, ahead of redb, a little behind sled;
  deletes level with sled, ahead of the others.
- **Compaction** still behind: 0.64 s against SQLite's 0.18 and redb's
  0.45.

## 191.5 Readers beside a writer
`reader_wait`, two readers, medians of three runs, reads in 10 s:

| writer | trunkdb | p99 | p99.9 | redb | p99 | p99.9 |
|---|---:|---:|---:|---:|---:|---:|
| no writer | 10,536,997 | 3.6–3.7 µs | 8.1–17.7 µs | 11,700,259 | 6.0–6.6 µs | 26.0–30.8 µs |
| 1 commit a second | 9,463,210 | 3.5–3.8 µs | 12.6–17.7 µs | 11,001,640 | 6.1–7.2 µs | 28.5–30.0 µs |
| 10 commits a second | 10,964,282 | 3.5–3.6 µs | 8.0–8.4 µs | 11,870,413 | 5.8–6.5 µs | 25.5–28.6 µs |
| single inserts, back to back | 11,075,856 | 3.5–3.6 µs | 11.0–11.6 µs | 11,999,928 | 6.2–6.8 µs | 17.7–19.5 µs |
| batches of 1000, back to back | 14,188,511 | 3.8–4.0 µs | 13.6–18.5 µs | 15,121,311 | 8.4–9.1 µs | 25.9–28.2 µs |

redb's readers do 6–16% more lookups; trunkdb's slowest ones are
faster, a p99 about half of redb's and a p99.9 lower in every case. The
p50 is 1.4–1.5 µs for both, but in three of trunkdb's fifteen cases,
2.0–2.5 µs, the two-reader effect of §190.4. The writer doesn't slow
trunkdb's readers: back-to-back batches leave them more lookups than
no writer at all, in both stores; why wasn't taken further.

## 191.6 A snapshot beside single writes
`long_read`, 200,000 documents, then 15 s of single writes, with and
without one snapshot open all the run:

| analysis | passes | longest pass | commits | commit p50 | p99 | max | most kept |
|---|---:|---:|---:|---:|---:|---:|---:|
| none | 0 | — | 3,006 | 4.6 ms | 10.3 ms | 35.9 ms | 0 |
| one snapshot, all the run | 198 | 104 ms | 3,304 | 4.4 ms | 7.3 ms | 27.7 ms | 1.4 MiB |

The writer still doesn't notice the snapshot (§83.6). It kept 1.4 MiB,
not §83's 13 MiB: §83 ran on Linux, where a commit's flush took about
0.4 ms and 34,000 commits fit in 15 s; here a flush takes the M1 Pro's
4.4 ms, and 3,300 fit. Kept memory follows the commits.

## 191.7 Selections and aggregates
Once each, against the ranges §106 and §107 and §126 recorded:

- `select`: 1000 by index, two fields selected, 0.98 ms (§106: 1.05),
  and answered from index entries 0.72 ms (0.7); whole, 1.63 ms
  (1.6). Unchanged.
- `aggregate`: per status on `(status, created)` 0.44 ms (§107:
  0.67–0.69); per tenant on `(tenant, tries)` 14.8 ms (§126:
  14.8–17.1); per status, no index, 105 ms (§107: 94–95); per tenant,
  no index, 126 ms (117–119); three counts as `count`s 42 ms (§126:
  64.5–68.2). The scans' aggregates a tenth slower, the index ones as
  fast or faster; not taken further from one run.

## 191.8 What changed in the documents
- DESIGN §12 and the README: the comparison's table from 191.2, all
  four from these runs; the text after it from 191.4.
- DESIGN's snapshot memory: 1.4 MiB over 3,300 commits on the M1 Pro,
  13 MiB over 34,000 on Linux (191.6).
- DESIGN's readers in parallel: four did 3.2 times one at §65, 1.9
  times now that one is more than twice as fast (§190.5).
