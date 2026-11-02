# 150. A binary search within B-tree pages (`index/btree.rs`)

§149 found the CPU, not the WAL's bytes, to be most of a batched
commit. A profile of the benchmark's inserts (§48) with every flush
skipped (`--cfg fuzzing`, §55.1), 100,000 tasks, 1,000 a commit, three
indexes, sampled with macOS's `sample` for 15 s (12,224 samples):

| | share of the CPU |
|---|---:|
| B-tree inserts, the primary index and three | 48% |
| ↳ finding the slot: `get_cell` and `memcmp` under `insert_into` | 12% |
| ↳ copying a page on change (`make_mut`) | 11% |
| ↳ checking a node's layout as it is read (`check_layout`) | 7% |
| ↳ inserting the cell (`insert_cell_at`) | 10% |
| writing the WAL record | 13% |
| the documents into data pages | 9% |
| checkpoints | 4% |

Every search within a B-tree page went through its cells in order:
`find_child` for the descent, `insert_into_leaf` and
`insert_into_branch` for the slot an entry goes in, `lookup` and
`remove`. A leaf of the benchmark holds about 200 entries, so finding a
slot compared about 100 keys, where a binary search compares 8.

## 150.1 The change
- **`first_slot_where(page, meets)`:** the first live slot whose cell
  meets `meets`, by a binary search over the slots. A page's live
  cells are in key order, so a bound on the key (`key >= x`, `key > x`)
  fails for every cell before that slot and holds from it on.
- **Tombstones:** a leaf written before §52 may hold tombstoned slots,
  which have no key to compare, the reason §97.3 kept the walk's start
  linear. A probe that lands on one steps on to the next live slot;
  the slots it passes are tombstones, so the answer is below the probe
  or is the live slot found. A run of tombstones is passed over one by
  one, which only such old leaves pay.
- **Used by** `find_child`, `insert_into_leaf` (`>= key`, as before),
  `insert_into_branch` (`> key`), and `lookup` and `remove` through
  `slot_of`, the first slot at or above the key if it is the key.
- **Not changed:** the walk's start (`Walk::next_in_leaf`), which §97.3
  measured at under a microsecond for a full leaf, and `Walk::descend`,
  which collects every child of a branch anyway for a walk backward.

No file format, no API, no change to what any call returns.

## 150.2 Measured
On the machine of §123, the build before the change (`f0420ce`) and
after, one after the other.

Inserts alone with flushes skipped, the profile's program, two rounds
of 100,000 in each of three runs:

| | before | after |
|---|---:|---:|
| inserts | 47.3–50.0k/s | 56.2–57.7k/s |
| CPU a commit of 1,000 | 20.6 ms | 17.6 ms |

1.17×, a little more than the 12% the profile gave the search: fewer
keys compared also means fewer cache lines read.

The §48 benchmark, trunkdb alone, three runs of each:

| | before | after |
|---|---|---|
| insert, 1000 per commit | 32k/s, 32k/s, 26k/s | 36k/s, 37k/s, 36k/s |
| update, 1000 per commit | 13k/s, 12k/s, 13k/s | 14k/s, 13k/s, 14k/s |
| delete, 1000 per commit | 26k/s, 27k/s, 27k/s | 28k/s, 28k/s, 28k/s |
| compact | 1.13 s, 1.08 s, 1.14 s | 0.79 s, 0.78 s, 0.80 s |
| one insert a commit | 5.0–5.2 ms | 5.0–5.2 ms |
| get by id | 2.6–2.8 µs | 2.0–3.4 µs |

- **Inserts 1.14×:** the CPU's part of a commit is 3 ms less of about
  32. The 26k/s run before was one slow run; the rest agree.
- **Compaction 1.4×:** it inserts every entry again, one by one
  (§48.4), each descent through full pages.
- **Reads within noise:** a lookup searches three or four pages, and a
  cached page's search was never most of it. Single commits are the
  flush.

The full benchmark, all four stores, three runs: inserts 36k/s against
SQLite's 41k/s on the same day, 0.88× (0.73× in §123.6); the README's
and DESIGN's tables are these runs. The other stores measured slower
than in §123.6 (SQLite's inserts 41k/s against 44k/s, its `get` 16 µs
against 6.1): the same machine, on another day.

## 150.3 Rejected
- **A search on the bytes of the slot directory** (offsets read
  without `get_cell`'s bounds check): `get_cell` checks one slot per
  probe, eight a search; nothing measured says it costs.
- **Interpolation search** (guessing where a key falls by its value):
  keys are bytes of any length, compound and multikey, and a guess
  needs them to be evenly spread.
- **The walk's start by the same search:** §97.3's reason is gone, but
  the cost it would save is under a microsecond; left as it is until a
  measurement says otherwise.

## 150.4 Tests
- `the_search_finds_what_a_scan_finds_around_tombstones`: a leaf and a
  branch of 60 entries, with tombstones nowhere, every third slot, a
  run at the start, in the middle and at the end, everywhere, and
  everywhere but one slot; for 123 keys below, at, between and above
  every key, `first_slot_where` on `>=` (leaf) and `>` (branch), and
  `slot_of`, give the slot the linear scan gives.
- `leaves_with_tombstones_from_older_versions_take_lookups_and_removes`:
  a leaf with every third slot tombstoned, as an older version left
  it; every live key is found where it is, none of the tombstoned
  ones, and every live key is removed. Inserts into such a leaf were
  tested already (`leaves_with_tombstones_from_older_versions_take_inserts`).
- The full test suite, clippy, the MSRV build (1.89).

## 150.5 Mutations
Each run against the index tests:
- a probe on a tombstone taken as not meeting, and as meeting, without
  stepping on: caught;
- the slot found not kept, and the search going on past it: caught;
- `slot_of` without its check that the slot holds the key: caught;
- a branch routing at `>=` instead of `>`, in `find_child` and in
  `insert_into_branch`: caught;
- a leaf insert at `> key` instead of `>= key`: **survives, and is
  equivalent.** Keys are unique, the primary key a document's id and
  every secondary key ending in it, so a leaf never holds the key being
  inserted, and both bounds find the same slot. The scan before had
  `>=` too.
