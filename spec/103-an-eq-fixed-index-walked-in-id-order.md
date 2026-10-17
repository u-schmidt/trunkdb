# 103. An `Eq`-fixed index walked in id order (`query.rs`, `collection.rs`)

"The newest 20 of tenant x", `eq("tenant", x).sort_desc("_id").limit(20)`,
is the list a document viewer shows most: one value of a field, the
latest first. Until §103 trunkdb answered it as it would without the
sort:
1. it read the tenant's whole range of the `tenant` index (1,000
   entries in the benchmark);
2. the entries decided the `Eq` (§100);
3. it kept one small document per match for the sort (§102.4);
4. it sorted all 1,000 in memory by `_id`;
5. it read the 20 it handed out.

That took 378–401 µs (§102.9). redb takes 19–20 µs, because it walks
its index, whose keys within one tenant are in id order already.

trunkdb's keys are too. A secondary key is the encoded values followed
by the 16-byte document id (`key::compound`, `key::secondary`). Within
one value the keys differ only in the id, so they're in id order. The
walk in sort order didn't use this: `id_order` (§96.2) gave up as soon as
an `Eq` on an indexed field found the documents by value, and the range
read took over.

§103 walks such a range in id order, forward or backward, and stops
after skip plus limit matches, as every walk does. It was the ROADMAP's
"a walk through an index for a sort by `_id` after an `Eq` on its
fields" (§100.9, §102.10).

## 103.1 Why the order is there
A range fixed by `Eq`s on every field of its index holds keys that all
start with the same value bytes, and the rest of each key is its id:
- **The encoding is prefix-free (§28.1).** A key in
  `KeyRange::prefixed(p)`, where `p` encodes one value per field of the
  index, has exactly `p` as its value part: no other value's encoding
  starts with `p`.
- **So the keys differ only in their last 16 bytes**, and their B-tree
  order is the ids' order, the primary index's (§96.1).
- **Values that share an encoding change nothing.** A string cut to the
  key budget, or an integer beyond 2^53, may share one encoding with
  other values (`part_is_exact`). Such documents are in the range too,
  under the same bytes, so the range is still in id order. They are
  only checked against the filter, not decided by their entries
  (§100.2).
- **A document is in the range once.** B-tree keys are unique, and the
  range's keys differ only in their id. A multikey index's keys are
  deduplicated per document when written (§42.2), so two elements
  equal to the `Eq`'s value, or sharing its encoding, are one entry.

The draft planned to drop a repeated id, in case a damaged index held
one document twice. It was left out: two keys of one id in the range
can't be neighbours unless they are the same key. A damaged key with
bytes between the value and the id is damage like any other (§55),
which `check` finds.

This holds only when every field of the index is fixed. On `(status,
created)` with only `status` fixed, the keys are ordered by `created`
first, and by id only among equal `created`.

## 103.2 When the walk is used
`Filter::id_order` (§96.2) keeps its rules: a sort whose first key is
`_id`, a limit, no `ids` (§61). Where the conditions don't find documents
by value, it walks the primary index, as before. Where they do, it asks
which range the range read would take (`index_ranges`), and walks that
range in id order (`Filter::fixed_whole`) if:
- it is **one range**, not a union of several (an OR, §36);
- the **`Eq`s fix every field of its index** (`equal_prefix`): a
  one-field index with an `Eq` on its field, a compound index with an
  `Eq` on each, a multikey index with an `Eq` on its path (`tags[*] ==
  "x"`).

Otherwise the range read takes over, as before: a compound index fixed
only in part, a range comparison without an `Eq`, an OR.

A sparse index (§44) needs no check of its own: `index_ranges` takes
one only where the conditions keep its nulls out. An `Eq` on null finds
nothing by value in it, so the primary index is walked.

The walk reads the range the range read would have read, the same
index under the same bounds, in another order, so it never reads more:
- **Without other conditions**, skip plus limit entries, and only the
  limit's documents.
- **With conditions the index doesn't hold**, documents until enough
  match: at worst the whole range, which the range read reads anyway.
- **Never another index than the range read's.** With `tenant == x` and
  `status == y` both indexed, the walk is of whichever range the range
  read picks (§28.4), not of a larger one picked for being in id order.

## 103.3 `_id` comparisons narrow it
After a fixed value part `p`, a key is `p` and an id. The `_id`
comparisons with an id become a range relative to `p`
(`key::primary_range`, exact for whole ids, §96.2), placed under `p`
with `KeyRange::under`, as the sort field's comparisons are placed under
the fixed fields (§43.3). Several are intersected.

`gt("_id", last)` then starts the walk right after `last` within the
tenant, so keyset paging works within one value as it does over the
whole collection: "the 20 of tenant x after the last one shown" reads 20
entries. `ne`, and comparisons of `_id` with anything but an id, are
only checked, as in §96.2.

## 103.4 The walk
`walk_in_index_order` does it, with the shortcuts the primary walk had
(§96.2), now keyed on `OrderedRead::by_id` instead of the index being
the primary one:
- **Every key is a group of its own, and exact** (`Spans::by_id`): its
  id is unique in the walk. Each entry is handed out as it's read,
  without reading the next key to see its group end.
- **Backward too.** A descending `_id` sort walks the range from its
  end, in the direct mode of §101.5. Its condition, the walk's order
  being the order handed out, holds both ways here: no ties need putting
  in id order.
- **Nothing unordered to look for afterwards**, and nothing held back
  (`Held` is off for a walk by id): every document in the range has the
  fixed values.
- **Entries decide where the index covers the filter (§100).** One
  decision covers the whole range when the filter names only the fixed
  fields (§101.2), and a leaf whose matches the skip passes over is
  passed whole (§101.3). "Page 25 of 20 of tenant x" counts its 480
  skipped entries leaf by leaf.
- **Only the conditions' fields are decoded** from a document checked
  (§99.2): a walk by id gives the whole order, so the sort keys aren't
  looked at.

`OrderedRead` for it:
- `index: Some(index)`;
- the range of 103.2, narrowed by 103.3;
- `by`: the `Eq`s, then the `_id` comparisons;
- `fixed`: every field of the index;
- `served`: every sort key, since ids are unique and no two documents
  tie in the first;
- `by_id: true`, as for the primary index.

## 103.5 How it reports itself
- **`explain`:** `QueryPlan::IndexOrder { field: "tenant" }` where it
  said `QueryPlan::Index { field: "tenant" }`. There is no new variant:
  it is the walk `IndexOrder` names, and `field` names the index walked
  (§96.3).
- **`explain_run`:** one read of that index, bounded by its `Eq`s and
  the `_id` comparisons that narrowed it, `backward` for a descending
  sort, `covering` where the entries decide, and `SortWork { from_index:
  <every key>, in_memory: 0 }`.

## 103.6 What stays
- **Every result, count and order.** The order is `_id`'s, as the sort
  in memory gave it. A tie in `_id` can't happen, so later keys never
  order anything.
- **The API, the file format and every write.**
- **Every other plan.** A sort on another field, an `_id` sort without
  an `Eq` on an indexed field, any read without a limit, and a count
  are untouched.

## 103.7 Rejected
- **Walking whenever some `Eq` fixes an index**, whatever the range read
  would pick: it could walk a large tenant's range where `status == y`
  names ten documents. A rule-based planner (§28.4) has no counts to
  weigh that with; walking only the range the range read takes is never
  worse.
- **A merge of several id-ordered ranges**, for `tenant == a OR tenant
  == b` or a union over indexes (§36): a k-way merge by id with a heap.
  Possible, but ORs sorted by `_id` with a limit are rarer, and it would
  be a second kind of walk.
- **A compound index fixed in part**, walked by the next field and
  re-sorted: that order isn't id order.
- **Walking without a limit:** every match is read anyway, and the sort
  costs little beside that (§96.4).
- **Leaving out multikey indexes:** their keys are deduplicated per
  document (103.1), so the range holds each document once, in id order.
  Their entries never decide (§100.9), so they gain less, but they still
  stop at the limit.
- **Dropping a repeated id** in the walk (103.1).

## 103.8 Tests
- **Planner** (`an_eq_fixing_its_index_whole_is_walked_by_id`), both
  directions, with a second sort key:
  - an `Eq` on a one-field index: walked by id, one field fixed, both
    keys served;
  - `Eq`s on two indexed fields: the walk is of the range
    `index_ranges` reads;
  - a compound index fixed whole is walked; fixed in part, or with a
    range on its last field, not; `(status, created)` beating `status`
    in the range read with a range on `created`, not;
  - a multikey path by an `Eq` on it: walked;
  - an OR of `Eq`s: not;
  - a sparse index: an `Eq` on null walks the primary index, an `Eq` on
    a value the sparse one;
  - without a limit, with ids, or sorted by another field: no walk by
    id.
- **Narrowing** (`id_comparisons_narrow_a_walk_under_its_fixed_values`):
  `gt`, `gte`, `lt`, `lte` and two together hold exactly the ids they
  should of nine; `ne` and a comparison with a string hold all nine;
  none ever holds another tenant's key (`w`, `xx`, `y`, the empty
  string), on a one-field and a compound index. `by` is the `Eq`, then
  the comparison.
- **Results** (`a_walk_by_id_after_an_eq_gives_what_a_sort_in_memory_gives`):
  3,000 documents with indexes on `t`, `(t, u)` and `tags[*]`, values
  chosen to share encodings (two strings cut alike past the key budget,
  2^60 and 2^60 + 1, `Int(5)` and `Float(5.0)`), nulls and missing
  fields. 150 random filters each round: an `Eq` on `t`, on `t` and `u`,
  or on `tags[*]`; now and then an `_id` comparison of every kind and a
  condition on an unindexed field; `_id` in both orders, now and then a
  second key; skips up to 400 and limits of 1, 7 and 50. Each gives what
  `apply_to` over every document gives, untyped, typed and streamed:
  after the inserts, more inserts, updates, deletes and an
  `update_fields`, and after reopening. Over 300 of the filters are
  walked on a secondary index.
- **Work done** (`a_walk_by_id_after_an_eq_reads_only_its_page`), 5,000
  documents of five tenants:
  - "tenant == t3, newest 20": `IndexOrder` on `tenant`, backward,
    covering, bounded by the `Eq`; 20 entries, 20 documents read;
  - page 25 of 20 both ways, by `tenant`: 20 documents read, no key
    copied, at most 10 decisions; and on `(tenant, tries)` fixed whole,
    backward: no key copied;
  - after the 600th id: bounded by two conditions, 20 entries, 20
    documents, the 601st to the 620th; `lte` backward from it;
  - with a condition the index lacks: not covering, under 1,000
    documents read for 20.
- **The corpora** (§47, §72, §96, §100, §101, §102, compound, sparse,
  multikey) pass unchanged. §96's
  `finding_by_id_or_by_value_beats_the_walk_by_id` now expects
  `IndexOrder` on `group` for `group == 3` sorted by `_id`.

Ten hand-made mutations, nine caught:
- a compound index fixed in part taken as fixed whole;
- a walk on another index than the range read's;
- the `_id` range placed outside the prefix;
- `gt` on `_id` keeping its boundary id;
- the `_id` comparisons left out of `by`;
- a backward walk kept out of the direct mode;
- a backward compound walk by id held back (`Held`);
- only the first sort key served;
- the range walked not marked `by_id`.

The tenth, an OR's ranges taken as one, is equivalent: `index_ranges`
picks a union only where no top-level `Eq` on an indexed field exists,
and then `equal_prefix` fixes no field. The one-range check stays, as
what the rule says.

## 103.9 Measurements
Benchmark (§48), no new rows. trunkdb before (`05174f1`) and after, back
to back, three runs each, on 100,000 documents:

| | before | after |
|---|---:|---:|
| tenant == x by id, first 20 | 369–404 µs | 22–25 µs |

16–17× faster, beside redb's 18.7–18.8 µs and sled's 24–40 µs in the
same runs; SQLite takes 1.26–1.30 ms. The aim was 20–35 µs, from
"status == x, oldest 20" (18 µs), the same kind of walk.

Every other row stays within its noise. "count tenant == x" printed
0.01 ms before and 0.02 after: measured in µs, it is 14.1–15.5 before
and 13.0–13.8 after, so that was rounding; a count never reaches the
planner's walk.

## 103.10 Limits
- **Only the range the range read would take** is walked. With two
  `Eq`s on indexed fields, it is whichever index §28.4 picks.
- **An OR, or a union over indexes**, still sorts in memory (103.7).
- **A compound index fixed in part** still sorts in memory, by `_id`
  among the range's entries.
- **Without a limit**, every match is read and sorted, as for every
  walk (§96.4).
- **An `_id` comparison in the filter** keeps the decider from deciding
  whole leaves: the filter names `_id`, which every key holds a
  different one of (§101.2). Each entry is decided on its own, without
  its document. The range already enforces the comparison exactly
  (103.3), so it could be left out of the decision; with paging by id
  there is nothing to pass whole anyway.
