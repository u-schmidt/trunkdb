# 193. Readers in parallel past four (`storage/pages.rs`, `database.rs`)

§65 let readers look pages up together, under a read lock, and four
did 3.2 times the work of one; eight did no more than four (§65.3).
§190.5 found four at 1.9 times one now that one is faster, and the
roadmap kept "readers that run in parallel past four" deferred until a
measurement showed the cost. Measured now: eight readers did fewer
lookups than four.

## 193.1 What the readers shared
A lookup by id takes three locks, each a `std::sync::RwLock`:

- **the gate** (`Shared::gate`, §80), shared for as long as the call
  reads;
- **the last commit** (`Shared::current`, §78), to clone it;
- **the pages in memory** (`Pages::memory`, §65), once for every page
  read: the primary index's root, a leaf, the data page.

Taking a read lock and letting it go each change a count in the lock,
and every reader changes the same one. With eight threads on eight
cores, the cache line that holds it moves from core to core: a lookup
by id changes those counts ten times, twice for each lock and twice
more for each page after the first. A profile of eight readers
(`sample`, 5 s) had 12,790 samples at the top of the stack in
`Pages::read_at`, where the lock is taken, against 1,579 in
`read_node`, the next of trunkdb's own.

## 193.2 The change
The three are now `crossbeam_utils::sync::ShardedLock`: eight
`RwLock`s, each in a cache line of its own. A thread reading takes the
one its thread index picks; a thread writing takes all eight, in order.
Readers on different threads change different counts, so the line
stays with its core. What each lock protects, and when it is taken, is
as before; so is poisoning, which `ShardedLock` reports as `RwLock`
does.

- **The writer pays eight locks for one**, none of them contended
  unless a reader holds it: a commit takes `current`'s once and the
  pages' a few times, nanoseconds beside its flush's milliseconds. A
  read that misses the cache takes the pages' to put the page in, eight
  too, beside a `pread` and a checksum.
- **`crossbeam-utils`** is the one new dependency. It has none of its
  own, and builds with Rust 1.89.

Rejected:
- **A sharded lock of trunkdb's own**, some sixty lines: it needs
  `unsafe`, an `UnsafeCell` and a `Sync` promised by hand, where
  trunkdb's only `unsafe` is the CRC's processor instructions (§40).
- **The pages' lock taken once per operation**, not once per page
  (§65.3): a `find` reading thousands of pages would keep commits out
  for its whole time, and a second read of the lock on the same thread
  waits behind a writer that waits for the first.
- **Pages borrowed from the cache instead of counted** (§65.3): every
  read that holds a page past the lock would need its lifetime tied to
  it; a larger change, for what is left (193.4).

## 193.3 Measured
Apple M1 Pro (8 performance and 2 efficiency cores), macOS, release
builds, 2026-11-13; `main` at `e1a1fb7` against this change, run one
after the other, alternating.

**Readers alone.** A scratch program, outside the repository: the 100,000
documents of `reader_wait` (§58), looked up by id from 1 to 8 threads,
4 s each, three runs each, lookups a second:

| readers | before | after | after, against one |
|---:|---:|---:|---:|
| 1 | 762k–770k | 686k–764k | 1 |
| 2 | 1,076k–1,101k | 1,057k–1,249k | 1.6 |
| 4 | 1,408k–1,468k | 2,069k–2,076k | 2.7 |
| 8 | 960k–1,004k | 2,494k–2,734k | 3.5 |

Before, four did 1.9 times one and eight 1.3 times; after, 2.7 and 3.5.
One reader is as before (one run of three at 686k).

**Readers beside a writer**, `reader_wait`, 4 s a case, two runs each,
reads in 4 s:

| readers | writer | before | after | p99 before | p99 after |
|---:|---|---:|---:|---:|---:|
| 2 | none | 4.0–4.4M | 5.0M | 3.7–4.0 µs | 3.0 µs |
| 2 | batches of 1000 | 8.4–8.5M | 9.6–9.7M | 3.8–3.9 µs | 3.0 µs |
| 4 | none | 5.8–5.9M | 8.5–8.6M | 4.5 µs | 2.9 µs |
| 4 | batches of 1000 | 10.6–11.1M | 15.6–16.0M | 6.0–6.2 µs | 3.2 µs |
| 8 | none | 3.9–4.0M | 11.4–12.1M | 19.2–19.7 µs | 5.9–6.2 µs |
| 8 | batches of 1000 | 8.3–8.8M | 24.8–25.1M | 26.5–28.6 µs | 6.1–6.2 µs |

The other writer cases (1 and 10 commits a second, single inserts)
moved as "none" did. The writer got as many commits through as before,
or more: with four readers 440 batches against 374–390, with eight
328–340 against 219–224.

**The comparison** (§48), trunkdb alone, four runs each: every row
within the runs' spread, inserts 43–46k/s against 43–45k/s, updates
20–21k/s against 19–21k/s, compaction 0.65–0.70 s against 0.65–0.70.

## 193.4 What is left
- **Eight do 3.5 times one, not eight.** Two shared writes are left on
  every lookup: the reference count of the last commit, cloned for the
  call (§80), and of the index's root page, which every lookup reads.
  A profile of eight now has no one function above the rest; the time
  is spread over the page reads, `malloc` and decoding.
- **More than eight threads share the locks again**, two threads to a
  lock past eight, and so on. A thread's index is crossbeam's, handed
  out as threads first take any `ShardedLock`, and reused when one
  ends.
- **Two readers** gain least, and vary most from run to run, as §190.4
  found.

## 193.5 Tests
What the locks do is as before, so the tests that hold them to it stay
as they are and pass against `ShardedLock`:

- `storage/file.rs`, `readers_look_pages_up_together` (§65.4): while
  one thread holds the pages to read, another's read goes through.
- `storage/file.rs`, new, `a_change_to_the_pages_waits_for_readers_on_every_thread`:
  three threads, each holding the pages to read; a change on a fourth
  waits until the last of them lets go. With a lock of each thread's
  own, the writer would go through beside them.
- `database.rs`, `compact_is_refused_by_a_kept_snapshot_and_waits_for_a_read`
  (§80.7): `compact`, on another thread than the read, waits for it at
  the gate.
- The concurrent tests of §79.8 and the model of §80.7.

## 193.6 Mutations
| Mutation | Test |
|---|---|
| a write that takes only its own thread's lock (crossbeam patched) | caught: the new test, and ten others that read beside a commit or a checkpoint |
| the three locks put back as `RwLock` | passes: how fast readers are is not tested, it is measured (193.3) |
