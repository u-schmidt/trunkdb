# 172. Walks that pass part of a leaf (`index/btree.rs`, `index/leaf.rs`, `collection.rs`, `decode.rs`, `storage/slotted.rs`)

With packed leaves (§171), a leaf holds two to three times the entries
it did. A walk through an index pays per entry it steps through, and
the leaves at a range's ends, which it steps through (§101.3), now held
most of a short range. So the bench's walks got slower with the smaller
file:

| | 0.33.0 | packed leaves |
|---|---|---|
| status == x by created, page 50 of 20 | 26 µs | 43 µs |
| 50 by id after skipping 5000 | 38 µs | 46 µs |
| tenant in (x, y) by id, page 25 of 20 | 66 µs | 98 µs |
| count tenant == x (1,000 docs) | 0.01 ms | 0.03 ms |

Profiled, the time went to stepping over entries that were never
handed out, and to the work of each step. Four changes take it back,
and further:

## 172.1 Into the first leaf by a binary search
A walk descended to the leaf where its range begins and then stepped
from the leaf's first entry, comparing each key with the range's start
until one was inside it: half a leaf on average, decoded and compared
for nothing. Now `read_next_leaf` puts the cursor of the first leaf
where the range begins, by the binary search within a page (§150):
forward the first key from `start` on, backward the first from `end`
on, the walk taking the keys before it. No later leaf can hold a key
before the range, so the start is never compared again forward, nor the
end backward.

## 172.2 The far end checked once a leaf
Each step compared its key with the range's far end. Now, as a leaf is
entered, its last live key in the walk's direction is compared once:
if it lies inside the range, so does every key of the leaf, and no step
on it compares (`Walk::clear`). Only the leaf the range ends in is
compared entry by entry, and the step that leaves the range ends the
walk, as before.

## 172.3 Each entry decoded once, its key copied in part
`step` decoded an entry to compare it, and again to hand it out, and
then copied the whole key into the walk's buffer. Now the entry found
is decoded once and written into the buffer by `KeyParts::write_over`,
which knows how many of the buffer's first bytes are already the
anchor's (`Walk::kept`) and copies only what differs: on a packed leaf,
mostly the rest after the shared bytes. A plain leaf's key is copied
whole, as before. `take_varint` reads a one-byte varint, the most
common, without its loop.

## 172.4 A run of a leaf passed at once
§101.3 passed a leaf whole only if the walk was about to enter it,
untouched, and all of it lay inside the range: never the leaves at a
range's ends. Now `Walk::run(most)` describes the **live entries of the
current leaf from the cursor on, in the walk's direction, that lie
inside the range, at most `most` of them,** the nearest: their lowest
and highest keys and how many. It finds them by binary searches, the
range's far end in the leaf and the slot of the `most`th entry, counts
them by the slot directory (`SlottedPage::live_in`), and decodes only
the two keys at their ends. `pass_leaf` passes over them.

Where a run is offered and the caller steps instead, no other run is
offered on that leaf (`Walk::offered`), so a walk going entry by entry
doesn't search at every step.

Its callers:
- **`count`** of a range (`BTreeIndex::count_at_most`), of an index
  (`index_entries`), and of conditions decided by the entries
  (`count_in`, §101.4): every run, each leaf at the range's ends too,
  counted at once where its ends decide it (§101.2). A count of a range
  inside one leaf decodes two keys.
- **The walk in index order** (§101.5): a run all rejected is passed
  whole; one all matching is passed whole if the skip covers it, and
  if the skip ends inside it, the part the skip covers is offered as a
  run of its own, decided at its own ends, and passed. So a skip of
  5,000 passes the leaf it ends in up to its last skipped entry, where
  it stepped through it.

**The aggregate** (§107.5) still passes only whole, untouched leaves
(`whole_leaf`), as §101.3 has it: a group counted from a run that
begins after entries already added would need the group's order
merged from both, which a whole leaf never does. Its counts are of
groups, usually spanning leaves, so the gain would be small.

## 172.5 Measured
Release build, the bench's 100,000 tasks with three indexes, the
median of three runs each:

| | 0.33.0 | packed leaves | now |
|---|---|---|---|
| status == x by created, page 50 of 20 | 26.4 µs | 42.8 µs | 21.4 µs |
| 50 by id after skipping 5000 | 38 µs | 45.7 µs | 35.2 µs |
| tenant in (x, y) by id, first 20 | 36 µs | 37.6 µs | 28.9 µs |
| tenant in (x, y) by id, page 25 of 20 | 65.8 µs | 98 µs | 69 µs |
| count tenant == x (1,000 docs) | 0.01 ms | 0.03 ms | 0.01 ms |
| scan: tries > 7, unindexed | 40.1 ms | 45.1 ms | 41.1 ms |

The rest moved within noise. 172.1 to 172.3 would have made 0.33.0's
walks faster too: part of the gain is theirs, not the format's.

## 172.6 Rejected
- **Handing out `KeyParts` instead of a key:** a step would copy
  nothing, but every caller that takes a key, the deciders, groups,
  spans and the key module's parts, would read a key in two pieces.
  The copy of what differs costs less than the change.
- **Runs for the aggregate** (172.4).

## 172.7 Tests
- `index/btree.rs`: walks stopped anywhere, over leaves emptied by
  removals and plain leaves with tombstones mixed among packed ones,
  now also walked by runs of at most 1, 2, 7 or every entry, some
  declined and stepped through instead, both ways over random ranges:
  each run the next entries in the walk's direction, its lowest and
  highest keys theirs, all of them in all. The tests of §101 and §150
  run on the new walk: random ranges against a `BTreeMap` both ways,
  counts by leaf, the keys copied only for what's handed out.
- `collection.rs`: the tests of counts, skips and pages through indexes
  (§101, §133) answer as before, with the same reads.
- `decode.rs`: varints of one byte and longer read back (§170.9).

## 172.8 Fuzzed and mutated, with §170 and §171
**Fuzzing** (§55), 15 minutes per target on format 14, with a key
stored inline added to the base: `import`, `recover_wal` and `queries`
found nothing. `open_file` and `open_raw` found two bugs that 0.33.0
has too (§55.2, 15 and 16): a header counting pages the file lacks ran
`check` out of memory, and cells adding up to more than their page made
a delete panic. Both fixed, then 15 minutes more of each, clean.

**Mutations,** 34 by hand over the three sections: 30 caught, 4 of
them by a test that no longer ends. The tests added for those that
weren't: a key of exactly 64 bytes joins the dictionary; name cells
overlapping, with a gap or a name twice are damage (§170.9); an entry
sharing one byte more than its anchor and a slot of 65,536 are refused
(§171.8). Left standing:
- a varint read for 11 bytes: the tenth may be only 0 or 1 and never
  continues, so no input tells it apart;
- the anchor at a leaf's first key, no split at the entry count's
  midpoint, a run offered again after a decline: they cost size or
  speed (§171.5, §172.5), not an answer.
