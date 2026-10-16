# 101. A cheaper walk through an index (`index/btree.rs`, `storage/slotted.rs`, `collection.rs`, `query.rs`)

Since §100 an index entry decides a filter without reading its document.
What was left was the walk itself, and it cost far more than it should.
Measured on a probe built like the benchmark (§48), with 100,000 tasks
and the same three indexes, each query run in a loop:

| | time | per entry |
|---|---:|---:|
| `count`, no conditions (100,000) | 7.1 ms | 71 ns |
| `count` of `tenant == x` (1,000) | 0.18 ms | 180 ns |
| `count` of created in a range (1,000) | 0.22 ms | 220 ns |
| `status == x` by created, page 50 of 20 (1,000 entries) | 0.36 ms | 360 ns |
| the first 50 by id after skipping 5,000 | 0.56 ms | 110 ns |

SQLite and redb counted 1,000 entries in 0.03–0.04 ms, and served page
50 in 0.04–0.05 ms.

A profile (`sample`, 5 s per query) put the time here:

| | allocating and freeing | grouping | deciding | path search |
|---|---:|---:|---:|---:|
| page 50 of 20 | 56% | 7% | 8% | 4% |
| `count` of `tenant == x` | 38% | — | 14% | 9% |
| 50 by id after 5,000 | 71% | — | 2% | — |

The B-tree walk itself (`Walk::next`, `get_cell`, the key comparisons,
`read_node`) was about 20 ns an entry. Everything else was overhead
around it:
- **Every entry was copied out of its leaf** (`key.to_vec()`), even one
  decided and dropped at once.
- **The walk in index order allocated about five times per entry:**
  - a `Vec` per group, though nearly every group is one entry;
  - `vec![group]` from `Held::pass`, even when nothing is held;
  - two `Vec`s in `Spans::values`, to check exactness;
  - and `Spans::group` ran twice per entry.
- **The skipped matches were kept:** `skip + limit` candidates were
  collected and the skipped ones drained by `find_in`. Freeing the
  1,000 of page 50 was 6% of its time.
- **`count` collected before it counted:** every key was copied into a
  `Vec`, and every match became a candidate.
- **A count without conditions** copied all 100,000 primary keys to
  take their number.
- **Every entry was decided on its own,** though most ranges hold long
  runs of one value: `tenant == x` made the same decision 1,000 times.

§101 removes the copies and allocations, decides a run of equal values
once, and counts or skips a leaf whose ends decide it without looking at
its entries. It was the ROADMAP's "a cheaper walk through an index"
(§100.9).

## 101.1 A walk that lends its entries
`Walk::step` returns the next entry with its key lent from the leaf,
which is the cache's (§97), not a copy. The key lives until the next
step.

The `Iterator` stays, built on `step`, copying each key. `range()`,
`scan()` and the tests keep using it, and `KEYS_COPIED` (§97) counts its
copies. The reads below use `step`, and copy a key only where they keep
it past the next entry: a group collected whole (101.5).

## 101.2 One decision per run of equal values
A `Covering` decides on the values the filter names (§100.2). Its
**deciding bytes** (`Covering::deciding_len`) are a key's encoded values
up to the end of the last named one. Two keys that agree on those
bytes tell of documents the filter can't tell apart.

A `Decider` (`Covering::decider`) keeps the last decision and its
deciding bytes in one reused buffer. An entry that agrees with them
takes that decision: a byte comparison, with no value decoded and no
condition checked.

This holds only where:
- **The filter doesn't name `_id`.** Every key ends with a different id,
  so `deciding_len` is `None` and nothing is reused.
- **The decision was made.** An entry with an inexact named value never
  decides (§100.2), so it leaves nothing to reuse.

A filter that names no value at all (only `sort`, or no conditions)
has zero deciding bytes: one decision covers everything.

## 101.3 A leaf decided whole
A leaf's keys are in order, and every value's encoding ends where it
ends, whatever follows it (§28.1). So if a leaf's lowest and highest
keys agree on their deciding bytes, every key between them does too.
`Decider::decide_all` then makes one decision for the leaf.

`Walk::whole_leaf` describes the leaf the walk is about to enter: its
lowest and highest keys, and how many live entries it holds
(`SlottedPage::live_cells`, read from the slot directory alone,
tombstones left out). It does so only for an untouched leaf that lies
all inside the walk's range. That rules out the leaves at the range's
ends, which are walked entry by entry as before, and an empty leaf.
`Walk::pass_leaf` passes over it.

`BTreeIndex::count(range)` uses this for every leaf, without deciding
anything: each inner leaf is counted by its slot directory, and only
the two leaves at the range's ends are stepped through.

**Index cells are checked once (`storage/slotted.rs`).** `read_node`
checked every cell of every node it read for the minimum entry length
(§55). That check was most of the cost of a counted leaf. It now lives
in the page's layout check, which the cache runs once as a page comes
in and marks (§66). `read_node` adds the page id to the error. A
damaged cell is still an `InvalidData` error from every path that reads
the page: "index page N: slot S is B bytes, shorter than any entry".

## 101.4 `count`
`count` streams where it can (`count_in`), and counts without
collecting:
- **No conditions:** `BTreeIndex::count` over the whole primary index.
- **One range of one index that covers the filter (§100),** or the
  primary index for a filter on `_id` alone. Each inner leaf is decided
  whole where its ends agree (101.3). Every other entry is stepped
  through and decided by run (101.2), and only an entry that can't
  decide has its document read.
- **Anything else** counts as before, through `read_candidates`: a
  lookup by id, a union of ranges (which needs its dedup set), a
  multikey index, or a filter the entries don't cover.

`read_candidates` and the unsorted cursor decide through a `Decider`
too, so they get the reuse by run.

## 101.5 The walk in index order
`walk_in_index_order` does what it did. What changes is how much it
holds.

**Entries handed out as they're walked.** The walk is **direct** where
the walk's order is the order handed out:
- forward, or on the primary index;
- every index field fixed or served (`Spans::end` is the field count),
  so the bytes after a group's values are the id, and ties are in key
  order;
- no sort key left for memory, and nothing held (`Held`, a backward
  walk of a compound index).

In a direct walk, an entry of an exact group (`Spans::exact`, which
iterates the parts without building a `Vec`) is decided and handed out
on its own, its key lent. It doesn't wait for the entry after it to
end its group. Only an inexact group, or any group of a walk that isn't
direct, is collected with its keys copied, and handed out as before.
`Held::pass` is called only when it is active.

**The skip is the walk's.** Matches are handed to a `Page`, which counts
the first `skip` and keeps the rest, up to `limit`. A decided match the
skip passes over is never read. An undecided one is read to be
checked, then dropped. `find_in` gets the page alone, so nothing is
drained. This holds in every walk, direct or not, since groups are
handed out in output order either way. It also holds for the scan
after the walk for what the index lacks (§34.2).

**Whole leaves, in a direct walk with no group being collected:**
- **Rejected whole:** passed over.
- **Matched whole, where the skip still covers all of the leaf:**
  counted as skipped and passed over. Its last key's group must also be
  exact. That group may go on past the leaf, and an inexact one is
  sorted in memory, which could mix this leaf's entries with the next
  leaf's.

A backward walk of a secondary index is never direct. It sorts each
group of ties by id (§49), so a group that straddles a leaf boundary
may come out in a different order from the walk's. Those walks skip
entry by entry, at the new, lower cost.

## 101.6 What explain_run says
`keys_examined` and each `IndexRead::entries` keep their meaning: the
entries of the range the read passed. A leaf counted or passed whole
adds its live entries. Nothing is added to the API.

One number changes. A direct walk no longer reads the entry that ends
the last group. "The 5 youngest" on an age index with two people per
age examines 5 entries, where it examined 7: three groups of two and
the entry that ended the third (`explain.rs`, §94).

## 101.7 What stays, and what changes in kind
- **Every result, count and order.**
- **The file format and every write.**
- **The API.** `Walk::step`, `whole_leaf`, `pass_leaf` and
  `BTreeIndex::count` are crate-internal (`index::btree` is private).

**What changes in kind:** a leaf decided whole takes its ends' word for
the keys between them. A leaf whose keys are out of order (damage, §55)
can now change a count or a page by more than its damaged entries.
§100 already trusts an entry for its own document. `check` (§39) walks
every index and checks that its keys are in ascending order, so it
finds such a leaf.

## 101.8 Rejected
- **Counts kept in branch pages** (an order-statistic B-tree), for a
  `count` or a `skip` in O(log n): a new file format, and every insert
  and delete would rewrite the counts up its path. Counting leaves gets
  most of the gain for none of that.
- **A count per collection in the catalog,** for the unfiltered total:
  every insert and delete would write the catalog page.
- **An arena for the copied keys:** it makes the copies cheaper but
  keeps them. 101.1 removes them.
- **Prefix compression in leaves:** more entries per leaf, but a new
  file format, and the walk is not limited by leaves read.
- **Whole leaves on backward walks of secondary indexes:** correct only
  with care at every straddling group. It would gain little, since
  backward deep pages are rarer and still walk entry by entry at the
  new, lower cost.
- **The path search per match** (`value_or_null`, `slot`: `.` and `[*]`
  looked for in the path on every document): 4–9% of these profiles,
  and most of what is left of a range count. It is a change to `Filter`,
  not the walk, so it gets a section of its own.

## 101.9 Tests
- **The walk** (`index/btree.rs`):
  - `walks_match_a_btreemap_both_ways`, on 6,000 random keys with
    stretches removed (emptied leaves) and 300 random ranges, now also
    checks `BTreeIndex::count`. It also checks a walk that passes every
    leaf it can whole (`by_leaves`), both ways: each passed leaf lies
    inside the range, the stepped entries are in it, and together they
    are all of it;
  - `a_count_looks_only_at_the_keys_of_the_leaves_at_its_ends`: 3,000
    of 5,000 keys, over 2,000 passed whole, and fewer than two leaves'
    worth stepped through;
  - `leaves_with_tombstones_from_older_versions_take_inserts`: a leaf
    tombstoned as before §52 counts its live entries only;
  - `a_cell_too_short_for_its_entry_is_an_error`, unchanged: the check
    moved into the layout check, and every path still fails with
    "shorter than any entry".
- **Agreement** (`whole_leaves_decide_as_the_documents_would`): 6,000
  documents on `g` (three values, runs far longer than a leaf, and now
  and then a cut string or a number past 2^60 inside a run), `n` (ties
  of three, which leaves split) and `(g, n)`, with a stretch deleted to
  leave empty leaves. 300 random filters, from none at all to `Eq`,
  `Ne`, ranges and combinations, under six sorts (by `n` and by `_id`
  both ways, by `g`, none), skips from 0 to 5,000 and limits. `count`,
  `find` and typed `find` agree with the filter applied to every
  document, `explain_run`'s invariants hold (`explained`), and `check`
  passes.
- **An inexact group across leaves**
  (`a_skip_keeps_an_inexact_group_across_leaves_in_its_order`): 900
  numbers past 2^60, inserted against their order, share three
  encodings, so each group spans several leaves and is sorted in memory.
  Every seventh skip through them gives the order the documents give.
- **Work done** (`a_walk_copies_and_decides_only_what_it_must`, 5,000
  tasks): `count` of `tenant == x` copies no key and decides at most once
  per ten entries; `count()` copies no key and decides nothing; page 51
  of 20 on `(status, created)`, and 20 by `_id` backward after 3,000,
  copy no key. `KEYS_COPIED` (§97) now counts a key copied into a
  collected group too, and `DECISIONS` counts the decisions a `Decider`
  makes on values.
- **The corpora before** (§47, §72, §96, §100, compound, sparse,
  multikey) pass unchanged.
- **One count changed in an older test:** "the 5 youngest" examines 5
  entries, not 7 (101.6).

Thirteen hand-made mutations, all caught (the last one as a hang):
- a leaf decided whole whose ends differ;
- the deciding bytes ending before the last named value;
- a decision reused where the filter names `_id`;
- a leaf skipped whole whose last group is inexact;
- a leaf at a range's end passed whole;
- tombstoned slots counted;
- a backward walk of a secondary index made direct;
- the skip off by one at a leaf's end;
- a skipped match kept;
- an undecided entry counted unread;
- index cells not checked for length;
- a collected exact group not sorted by id;
- a backward `pass_leaf` leaving the walk at the leaf's end.

Benchmark (§48), with a new row for all four stores: `count` with no
conditions. trunkdb before (`53c6ad1`, the new row copied in) and after,
back to back, three runs each, on 100,000 documents:

| | before | after |
|---|---:|---:|
| `count`, no conditions (100,000) | 6.75–8.04 ms | 0.08–0.09 ms |
| `count` tenant == x (1,000) | 0.17–0.19 ms | 0.01–0.02 ms |
| `count` created in a range (1,000) | 0.22 ms | 0.14–0.15 ms |
| status == x by created, page 50 of 20 | 350–377 µs | 25.5–25.9 µs |
| 50 by id after skipping 5,000 | 541–593 µs | 37.7–40.3 µs |
| tenant == x by id, first 20 | 645–688 µs | 555–599 µs |
| get by id | 3.3–3.4 µs | 2.3–2.6 µs |
| status == x, oldest 20 | 24.0–30.9 µs | 17.9–18.5 µs |
| first 50 by id | 35.1–38.0 µs | 31.6–32.9 µs |

The other stores, in the same runs: SQLite counts all 100,000 in
0.01 ms, redb reads its table's stored length, and sled walks every
entry in 24–25 ms. For a count of 1,000 by index they take 0.03, 0.03–0.04
and 0.26 ms; for page 50 of 20, 54–58, 42 and 248–273 µs; for 50 by id
after 5,000, 119–128, 150–156 and 958–992 µs. So trunkdb is now the
fastest of the four on a deep page and level with them on a count by
value. On a range count it is 5× theirs: the path search (101.8).

Lookups by id and the walks that stop early got faster too, though
§101 doesn't target them. `read_node` no longer checks every cell of a
cached page (101.3), and every lookup descends through it. Inserts,
updates, deletes and the scan move within their noise.

## 101.10 Limits
- **A range of all-different values** (created in a range) still
  decides entry by entry. It allocates nothing now, but it decodes and
  checks every value, and the path search dominates (101.8).
- **A filter naming `_id`** decides entry by entry.
- **A union of ranges, a multikey index, and a filter that doesn't
  cover** count through the candidate list, as before.
- **Backward deep pages on a secondary index** walk every skipped
  entry.
- **The unfiltered total is O(leaves),** about 330 for 100,000
  documents. Only counts in branch pages (101.8) would make it
  O(log n).
- **"tenant == x by id"** still sorts the tenant's matches in memory
  (ROADMAP, a walk of an `Eq`-fixed index for an `_id` sort).
