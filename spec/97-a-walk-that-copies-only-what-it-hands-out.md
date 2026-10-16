# 97. A walk that copies only what it hands out (`index/btree.rs`)

§49 made reading an index in order lazy *between* leaves: a walk reads
the next leaf only when the current one is used up. Within a leaf it
was eager. Before it handed out the first entry, `Walk::read_next_leaf`
copied every entry of the leaf that lay in the range into `pending`, a
`Vec<(Vec<u8>, RecordLocation)>`, one heap allocation per key. §49.5
named it as a limit.

A walk that stops at a limit paid for the whole leaf anyway. On the
benchmark's 100,000 documents (§48), a leaf of the primary index holds
about 269 entries, of `created` 193, of `tenant` 121 and of
`(status, created)` 97. Copying 400 16-byte keys into `Vec`s takes
18 µs. A `find` sorted by `_id` with `limit(1)` took 17.6 µs, where a
`get`, which descends the same tree and reads one document, takes
2.2 µs.

The question came from §96: "first 50 by id" took 80 µs, where redb
takes 25 µs and SQLite 36 µs. A probe found two causes. This is the
first. The second is decoding into a `Document` before a `T`, about
1 µs per document, which needs a section of its own.

## 97.1 The walk keeps its leaf
`Walk` holds the current leaf's page and a cursor in place of
`pending`: forward the next slot to look at, backward the slot after
it. `next_in_leaf`:
- steps the cursor one slot in the walk's direction, past a tombstoned
  slot (a leaf written before §52 may hold them, as `iter_cells` skips
  them);
- decodes the entry in place (`decode_index_entry`, a borrow of the
  page);
- checks it against the range;
- copies the key out only to hand the entry out.

**The range** is checked one entry at a time, with the comparisons
`read_next_leaf` made over the whole leaf before:
- **forward:** a key before `range.start` is skipped (only the first
  leaf has any); a key at or after `range.end` ends the walk;
- **backward:** a key at or after `range.end` is skipped (only the first
  leaf has any); a key before `range.start` ends the walk.

Once the walk is done it doesn't look at the leaf again.

**At the end of a leaf** the walk reads the next one as before: by the
sibling link forward, by the path of branches backward (§49.1). The
descent, the path, empty leaves and the check for sibling links that go
round a loop (§55) are unchanged.

The page the walk holds is a `Page`, bytes shared with the cache
(`Arc<[u8]>`), not copied. Holding it keeps nothing from being written:
a write makes a new version of the page, and the walk runs inside the
caller's read (§27) or snapshot (§77) as before. §49.1's "holds no page
past its leaf" now reads "holds its current leaf".

**Damage (§55)** is found where it was. `read_node` checks every cell of
a leaf before the walk uses it: a cell too short for an entry fails the
read, so `decode_index_entry` can still trust the cell. An error ends
the walk after it's handed out.

## 97.2 What stays
- **The item type:** `io::Result<(Vec<u8>, RecordLocation)>`, one
  allocation per entry handed out. `read_in_index_order` keeps keys in
  its groups and compares them after reading the next key (§47, §49.2),
  so it needs owned keys. 50 copies cost about 2 µs.
- **`Index::range` and `scan`** collect the walk as before (§49.1).
  They copy every key anyway.
- **The counts of `explain_run` (§94):** an entry is counted when it is
  handed out, as before.

## 97.3 Rejected
- **Handing out borrowed keys** (a lending iterator, or a key type that
  borrows the page): it would save the last 2 µs of a 50-entry walk,
  but `Walk` would stop being an `Iterator`, every caller would change
  with it, and `read_in_index_order` would copy the keys it holds
  anyway.
- **A binary search for the first key of the range:** the slots are in
  key order, but a leaf from before §52 may hold tombstoned slots,
  which have no key to compare. Skipping the keys before the start
  compares bytes without copying them, less than a microsecond for a
  full leaf. If a measurement ever says otherwise, a search can skip
  tombstones the way the cursor does.
- **Smaller copies, such as `SmallVec` keys:** cheaper copies, not fewer
  of them, and a compound key may be up to 1 KiB (`MAX_KEY_LEN`).

## 97.4 Tests
- **The walk tests from before** pass unchanged:
  `walks_match_a_btreemap_both_ways`,
  `walks_climb_as_far_as_needed_in_a_tall_tree`,
  `a_walk_stopped_early_reads_only_what_it_handed_out` (which counts
  pages), and the damage tests of §55.
- **Copies counted:** `KEYS_COPIED`, a thread-local counter in tests
  only, like `RECORDS_READ`. In `a_walk_copies_only_the_keys_it_hands_out`:
  - from a range that starts and ends mid-leaf, 5 entries copy 5 keys,
    forward and backward;
  - the last entry, backward, copies 1;
  - 400 entries across leaves copy 400;
  - the whole range copies its 406.
- **Stopped anywhere** (`a_walk_stopped_anywhere_copied_what_it_handed_out`):
  - 4,000 random keys, a stretch and every fifth removed, so some
    leaves are empty;
  - every third leaf with every fourth slot tombstoned, as versions
    before §52 left them;
  - 400 random ranges in random directions, stopped after a random
    count. What was handed out equals the start of the range in that
    direction, and as many keys were copied as handed out.
- **`explain_run`'s counts** in the §94 and §96 tests are unchanged.

Eight hand-made mutations, all caught:
- a tombstone ending the leaf instead of being skipped;
- forward, the key at `range.end` handed out;
- backward, keys before `range.start` handed out;
- backward, the walk not ending at `range.start`, caught by the page
  count;
- forward, the first leaf's keys before the start handed out;
- a key copied before it's checked against the range;
- backward, the cursor starting one slot short;
- forward, the cursor starting one slot late.

Benchmark (§48), trunkdb alone, before and after run back to back twice
on 100,000 documents:

| | before | after |
|---|---:|---:|
| `limit(1)` by id | 17.5–18.7 µs | 3.2–3.5 µs |
| first 50 by id | 82–83 µs | 69–71 µs |
| status == x, oldest 20 | 43–46 µs | 39–42 µs |
| status == x, newest 20 | 44–46 µs | 43–48 µs |

`limit(1)` falls to about what a `get` costs, and the first 50 by id
lose the 12 µs the leaf copy took. Oldest 20 gains less: its leaf holds
97 entries, not 269. Newest 20 shows no gain beyond the noise.

Full suite, clippy `-D warnings` with and without all features, fmt,
rustdoc, the fuzz and bench crates.

## 97.5 Limits
- One allocation per entry handed out stays.
- An index range read whole (`candidate_entries`, `check`, `compact`)
  still copies every key; it needs every one.
- Most of the remaining gap to the other stores is decoding: about
  1 µs per document into a `Document` and then a `T`, where serde_json
  takes 0.4–0.5 µs straight into the struct.
