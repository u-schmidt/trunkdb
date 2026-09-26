# 58. Measuring how long readers wait (`bench/src/bin/reader_wait.rs`)

§57 deferred snapshot reads until something measured asks for them, and
named the measurement: how long readers wait while a writer commits.
This is it. It found what §57 expected for paced writers, something
worse for writers that never pause, and something §57 didn't expect at
all: trunkdb's readers don't run in parallel.

## 58.1 The measurement
`cargo run --release --bin reader_wait -- [seconds per case]` from
`bench/`. 100,000 small documents; two threads look them up by id as
fast as they can, and each lookup is timed; a third writes, case by
case:
- not at all;
- one document per commit, one commit a second (a location tracker, say),
  or ten a second;
- one document per commit, back to back;
- batches of 1,000, back to back.

The same with redb (§48), whose readers read a snapshot: what snapshots
in trunkdb would give. `BENCH_ONLY` picks a store, `BENCH_READERS` the
number of reading threads.

## 58.2 Measured
M1 Pro, 10 s per case, durable commits everywhere (trunkdb flushes with
`F_FULLFSYNC`, about 5 ms):

| store | writer | commits | reads | p50 | p99 | p99.9 | max |
|---|---|---:|---:|---:|---:|---:|---:|
| trunkdb | no writer | 0 | 2,278,711 | 7.6 µs | 38.3 µs | 65.3 µs | 7.9 ms |
| trunkdb | 1 commit a second | 11 | 2,257,342 | 7.7 µs | 33.5 µs | 64.9 µs | 11.9 ms |
| trunkdb | 10 commits a second | 101 | 2,165,407 | 7.8 µs | 32.2 µs | 63.1 µs | 9.6 ms |
| trunkdb | single inserts, back to back | 1,929 | 10,794 | 13.1 µs | 60.8 ms | 208.8 ms | 473.4 ms |
| trunkdb | batches of 1000, back to back | 833 | 912,726 | 7.8 µs | 50.5 µs | 11.1 ms | 34.3 ms |
| redb | no writer | 0 | 14,170,301 | 1.2 µs | 4.5 µs | 13.2 µs | 14.4 ms |
| redb | 1 commit a second | 11 | 14,021,902 | 1.2 µs | 4.6 µs | 14.1 µs | 2.9 ms |
| redb | 10 commits a second | 101 | 14,132,661 | 1.2 µs | 4.6 µs | 13.7 µs | 2.1 ms |
| redb | single inserts, back to back | 2,130 | 13,583,251 | 1.2 µs | 5.1 µs | 11.8 µs | 3.9 ms |
| redb | batches of 1000, back to back | 1,630 | 17,988,604 | 1.3 µs | 5.7 µs | 15.1 µs | 5.3 ms |

The maxima of about 10 ms are the operating system's: redb's readers,
which never wait for anything, show 14 ms with no writer at all.

## 58.3 What it shows
**Paced writers cost readers nothing measurable.** At one or ten
commits a second, p99.9 is as without a writer, and the longest wait
is one commit, inside the noise. The tracker case of §57.2 is fine as
trunkdb is.

**A writer that never pauses is a different matter.**
- Batches back to back: a reader that arrives during a batch waits for
  all of it, p99.9 11 ms, 34 ms at a checkpoint (§51), and readers get
  40% of their reads done.
- Single documents back to back: a commit holds the write lock for its
  flush, about 5 ms, and the writer takes it again at once. Readers get
  in between commits only now and then: 0.5% of their reads, p99 61 ms,
  one waiting almost half a second. Rust's `RwLock` doesn't promise a
  waiting reader a turn before the next writer.

That is §57's trigger (a p99 in the tens of milliseconds), under a
workload none of the current ones has: an import, a sync catching up, a
logger writing every event as its own commit. Option B (§57.2) is what
fits it: the flush, which is most of a commit, would happen outside the
lock, and readers would wait only for the publish.

**Readers don't run in parallel.** With no writer at all:

| readers | trunkdb p50 | trunkdb reads in 3 s | redb p50 | redb reads in 3 s |
|---:|---:|---:|---:|---:|
| 1 | 3.5 µs | 826,274 | 0.9 µs | 3,152,435 |
| 2 | 7.5 µs | 690,215 | 1.2 µs | 3,997,736 |
| 4 | 12.2 µs | 747,275 | 1.6 µs | 4,186,689 |

More readers make each lookup slower and all of them together no
faster. A profile of four readers (macOS `sample`, as in §52) has 5,827
samples waiting in `__psynch_mutexwait` against about 1,500 doing work,
all of them under `FileStore::read_vec`: the page cache's mutex (§50).
Every page read takes it and holds it while copying 8 KB out. The read
lock of §27 lets readers in together; the cache lets them through one
at a time.

## 58.4 What it changes
- **§57's decision stands for snapshots (C).** Nothing measured needs a
  reader to pin a state across calls.
- **The cache's mutex is the first thing to fix**, before B or C:
  cheaper than either, and it holds back every application with more
  than one reading thread, writer or not. The candidates are §50.7's
  shared pages (the cache hands out a reference-counted page, and the
  copy, if any, happens after the lock), or a cache split into shards by
  page id. Open (ROADMAP.md).
- **B has a case now**: writers that commit back to back. Still not
  scheduled, since no current workload writes that way; the numbers are
  here for when one does.

## 58.5 Limits
- One machine, macOS, where a flush is `F_FULLFSYNC`; on Linux a flush
  is usually shorter, and so is every wait for one.
- Lookups by id only. A `find` holds the read lock longer and would
  make writers wait longer; not measured.
- Not in CI: it takes minutes, and its numbers only mean something on
  one quiet machine. CI builds and lints it with the rest of `bench/`.
