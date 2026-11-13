# 190. 0.40.0 measured against 0.34.0 (`bench/`)

The last measurement was §172.5, on the way to 0.34.0. The six releases
since changed paths the bench times: lock files and a gate at each
checkpoint (§187), a check on every page read for a process that let
go (§189), a WAL bound to its file (§183), checked writes (§184), leaf
splits (§186). Each should cost little; this checks that together they
cost nothing a user would see. Nothing in the library changed.

## 190.1 How
Apple M1 Pro (8 performance and 2 efficiency cores, macOS), Rust
1.98.1, 2026-11-13. The bench's code is the same in both releases
(only its `Cargo.lock` names another version), so the workload is
identical. 0.34.0 was built from its tag in a worktree outside the
repository, 0.40.0 from `main`; trunkdb alone (`BENCH_ONLY=trunkdb`).
The two ran one after the other, alternating, so the machine's load
fell on both alike.

- The comparison (§48): 100,000 tasks, three indexes, six runs each.
- `reader_wait` (§58): two readers, 10 s per case, five runs each.
- `reader_wait` with 1 and with 4 readers, 4 s per case, two runs each.
- A copy of `reader_wait`, outside the repository, whose cases were
  "no writer" three times, "10 commits a second", then "no writer"
  twice more, 5 s each, three runs each (190.3).

## 190.2 The comparison
Medians of six runs:

| | 0.34.0 | 0.40.0 |
|---|---:|---:|
| insert 100000, 1000 per commit | 45.5k/s | 45k/s |
| insert 1000, one per commit | 5094.5 µs | 5155.4 µs |
| get by id | 2.95 µs | 2.60 µs |
| find tenant == x (1000 docs) | 1.00 ms | 0.91 ms |
| find created in a range (1000 docs) | 0.96 ms | 0.94 ms |
| status == x, oldest 20 | 18.8 µs | 18.8 µs |
| status == x, newest 20 | 24.6 µs | 24.7 µs |
| first 50 by id | 33.7 µs | 32.3 µs |
| scan: tries > 7, unindexed | 40.19 ms | 38.97 ms |
| count tenant == x (1000 docs) | 0.01 ms | 0.01 ms |
| count created in a range (1000 docs) | 0.02 ms | 0.02 ms |
| count, no conditions (100000 docs) | 0.07 ms | 0.07 ms |
| status == x by created, page 50 of 20 | 21.4 µs | 21.6 µs |
| 50 by id after skipping 5000 | 36.4 µs | 36.3 µs |
| tenant == x by id, first 20 | 23.2 µs | 24.0 µs |
| tenant in (x, y) by id, first 20 | 29.1 µs | 30.3 µs |
| tenant in (x, y) by id, page 25 of 20 | 72.7 µs | 74.7 µs |
| update 9528, 1000 per commit | 20.5k/s | 21k/s |
| delete 10000, 1000 per commit | 35.5k/s | 34.5k/s |
| file size | 24.6 MB | 24.6 MB |
| compact | 0.66 s | 0.65 s |
| file size after compact | 18.0 MB | 18.0 MB |

Every row within 4% but `get by id` and `find tenant == x`, which got
faster, inside their spread. Single runs vary more than that: the scan
took 38.5–47.5 ms on 0.40.0, and after three runs looked 12% slower;
the next three put it level. Updates on 0.34.0 ran from 9.5k/s to
21k/s, as §48.3 saw before.

## 190.3 Readers
`reader_wait`, medians of five runs, reads in 10 s by two readers:

| writer | 0.34.0 | 0.40.0 | p50, both | p99, both |
|---|---:|---:|---:|---:|
| no writer | 10,749,491 | 8,855,668 | 1.5 µs; 2.2–2.5 µs (190.4) | 3.5–3.7 µs |
| 1 commit a second | 10,678,285 | 10,966,212 | 1.5 µs | 3.5–3.8 µs |
| 10 commits a second | 11,031,637 | 11,071,718 | 1.4–1.5 µs | 3.5–3.8 µs |
| single inserts, back to back | 11,064,229 | 11,070,740 | 1.4–1.5 µs | 3.5–3.8 µs |
| batches of 1000, back to back | 13,729,279 | 13,927,610 | 1.6–1.7 µs | 3.9–4.2 µs |

With 1 reader, both read about 3.0 million times in 4 s, p50 1.2 µs,
in every case; with 4 readers, about 5.6 million, p50 2.6–3.1 µs.

## 190.4 Two readers, sometimes slower
A case of two readers runs at a p50 of 1.5 µs or of 2.2–2.5 µs, with
nothing between, and the slow ones read about 20% less. Of 43 cases
with two readers, 9 ran slow on 0.40.0 and 2 on 0.34.0, in any case,
writer or not, first or later (the copy of 190.1). In the five runs
above, four of the five "no writer" cases on 0.40.0 happened to be
slow, which is what moved that row's median.

Not the code: a lookup in a process that writes takes the same path in
both, the gate's read lock, the current commit cloned, the pages
walked; the check of §189 is an `Option` that is `None` there. Profiles
of the two (`sample`, 10 s each) have the same functions in the same
proportions, `Pages::read_at`, `read_node`, `malloc`, and no thread
waiting on a lock. And 1 and 4 readers show nothing. Most likely it's
where macOS puts the two threads: both write the gate's count of
readers and the commit's reference count on every lookup (§65.2), and
that costs more between the M1 Pro's two clusters of performance cores
than within one. Why 0.40.0 lands there more often, perhaps how the
grown `Inner` lays those counters out, wasn't taken further.

## 190.5 Readers in parallel, since §65
§65.2 measured four readers doing 3.2 times the work of one, which
ROADMAP's 0.14.0 row repeats. Now four do about 1.9 times one, in
0.34.0 as in 0.40.0: one reader got faster, 2.8 µs at p50 then and
1.2 µs now, about 750,000 lookups a second against 327,000, while four
went from 1.03 to 1.4 million a second. The atomic writes every lookup
makes to shared memory (§65.2) are now a larger part of a shorter
lookup. Recorded here; the 0.14.0 row stays as it was measured.

## 190.6 Not measured
- An open, which makes and locks four files now (§182, §187).
- A reader in another process (§187–§189): the bench reads in the
  process that writes.
- Linux and Windows.
