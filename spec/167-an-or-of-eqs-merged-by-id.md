# 167. An OR of `Eq`s merged by id (`query.rs`, `collection.rs`)

"The newest 20 of these two tenants", `is_in("tenant", ["a", "b"])
.sort_desc("_id").limit(20)`, is §103's list for more than one value:
a document viewer filtered by a few statuses, paged by skip and limit.
Until §167 trunkdb answered it as it would without the sort:
1. it read both tenants' ranges of the `tenant` index whole (2,000
   entries in the benchmark);
2. it read every match;
3. it sorted them all in memory by `_id`;
4. it handed out 20.

§103 walks one such range in id order and stops at the limit. It left
an OR out (§103.7): "a k-way merge by id with a heap. Possible, but ORs
sorted by `_id` with a limit are rarer, and it would be a second kind of
walk." A real program pages by skip and limit through such a list, so
§167 builds it. It was the ROADMAP's "a sort by `_id` after an OR of
`Eq`s".

## 167.1 Why a merge gives the order
- **Each range is in id order.** A range fixed by `Eq`s on every field
  of its index holds keys that differ only in their last 16 bytes, the
  id (§103.1). That holds for a one-field index, a compound one fixed
  whole and a multikey path.
- **So the next document in id order is the next entry of one of
  them.** The walk keeps each range's next entry, takes the least id
  among them (the greatest, backward), hands it out, and steps that
  range on. A binary heap holds the ranges by their next id.
- **A document in two ranges comes out of both at once.** Its id is the
  least in both, so the two entries are taken one after the other, and
  the second is passed over. That covers `tags[*] in ("a", "b")` for a
  document tagged both, values that share an encoding (two strings cut
  alike past the key budget, `5` and `5.0`), and the same value given
  twice.
- **Within one range a document is there once** (§103.1): only the
  entries of different ranges can name the same id.

## 167.2 When the merge is used
`Filter::merged_by_id` decides. It takes the ranges the range read
would take (`index_ranges`), and merges them if:
- the sort's first key is `_id`, with a limit, no `ids` (§61) and no key
  of the application's (§154), as for any walk;
- `index_order` doesn't walk already: one range is §103's;
- there are **two or more ranges**, an OR's or an `in`'s, with no ids
  looked up beside them (§164);
- the **`Eq`s a range comes from fix every field of its index**
  (`equal_prefix` on the range's `by`).

Otherwise the range read takes over and sorts in memory, as before.

It reads the ranges the range read would read, under the same bounds,
so it never reads more:
- **Without other conditions,** skip plus limit entries and one more in
  each range, and only the limit's documents.
- **With conditions the indexes don't hold,** documents until enough
  match: at worst every entry, which the range read reads anyway.
- **Never another index than the range read's.** With `tenant in (…)`
  beside `status == y`, both indexed, the read is of whichever the
  rules pick (§28.4).

The ranges may be of different indexes, `tenant == a OR status == b`:
each is in id order on its own, so the merge holds.

## 167.3 `_id` comparisons narrow each range
The `_id` comparisons with an id among the filter's conditions hold
for every match, so each range is narrowed by them under its own fixed
values, as §103.3 narrows one: `gt("_id", last)` starts every range
right after `last`. "The 20 of these tenants after the last one shown"
reads about 20 entries. The comparisons join each range's `by`, after
its `Eq`s.

## 167.4 The walk
`walk_merged_by_id` does it:
- **One entry at a time,** decided or read and checked as §103.4 hands
  out an entry of a walk by id: the per-entry step of
  `walk_in_index_order`, now `HandOut::one`, which both share.
- **Entries decide only where every range is of one index**, and it
  covers the filter (§100): a decider reads a key as its index lays it
  out. `tenant in (…)` decides on its entries; `tenant == a OR status
  == b` reads each document to check it.
- **Each range keeps its own last decision** (`LastDecision`). A
  decider takes its last decision again where the next key's deciding
  bytes agree with it (§101.2), as they do throughout a range fixed by
  `Eq`s: one decision for a range. Two ranges' entries interleave, so
  one decider's last decision would almost never be the next entry's,
  and every entry would be decided again from its values. The merge
  has one decider, and swaps a range's last decision into it while
  that range's entry is decided (`Decider::swap_last`). Page 25 of 20
  through two tenants decides four times at most, where it decided
  about 500 times.
- **Backward too:** each range walked from its end, the greatest id on
  top.
- **No leaf passed whole** (§101.3): the next entry may be in another
  range, so a skip counts entry by entry. Its documents still aren't
  read where the entries decide.
- **Only the conditions' fields are decoded** from a document checked
  (§99.2), as for every walk by id.
- **Each range's next key is copied** while it waits in the heap: the
  leaf it is lent from moves on when that range steps.

## 167.5 How it reports itself
- **`explain`:** `QueryPlan::IndexUnion { fields, in_order: true }`.
  `in_order` is new; it is `false` where the ranges are read whole and
  sorted in memory. The variants are `#[non_exhaustive]` (§117), so a
  program that matches `IndexUnion { fields, .. }` still compiles.
  Merged, it is still the union of those ranges, each document once;
  `in_order` says how it is read.
- **`explain_run`:** one read per range, bounded by its `Eq`s and the
  `_id` comparisons, `backward` for a descending sort, `covering` where
  the entries decide, and `SortWork { from_index: <every key>,
  in_memory: 0 }`.
- **Alternatives** (§133): a merge is counted by running it under the
  budget, as a walk in sort order is (§133.4); it reads documents where
  its ranges are of several indexes, or its one index doesn't cover the
  filter.

## 167.6 What stays
- **Every result, count and order.** The order is `_id`'s, as the sort
  in memory gave it.
- **The file format and every write.**
- **The API, but for `in_order`.**
- **Every other plan.** A sort on another field, an OR with a branch an
  `Eq` doesn't fix whole, ids beside the ranges, any read without a
  limit, and a count are untouched. An unsorted cursor still streams.

## 167.7 Rejected
- **A compound index fixed only in part,** which the ROADMAP's entry
  named too: on `(status, created)` with only `status` fixed, the keys
  are in the order of `created`, and in id order only among equal ones
  (§103.1). A merge would need one input per value of `created`, which
  only reading the range tells, so the sort in memory reads no more. It
  moved to the ROADMAP's Not planned.
- **Ids beside the ranges** (§164), `_id in (…) or status == a`: the ids
  are sorted, and could be one more input. Left out until a program
  asks: their lookups are few, and the union reads them anyway.
- **A new plan variant,** `IndexMerge`: a program matching
  `IndexUnion` would miss the merge, which is the same union read
  another way. A field says it, as §103 kept `IndexOrder` for its walk.
- **Merging only ranges of one index:** ranges of several indexes are
  each in id order just the same; only the entries' decisions need one
  index.
- **A decider per range,** for each range's last decision: building a
  decider compiles the whole filter (§101.2, §104), whose OR has a
  branch per range, so an `in` of k values built k deciders of k
  branches each. Measured, an `in` of 500 tenants, newest 20, took
  34 ms against 2.4 ms with one decider; 43 ms before §167. A last
  decision kept per range costs nothing to set up.
- **A linear search for the least id** instead of a heap: as fast for
  two or three ranges, but an `in` of hundreds of values would search
  them all for each entry.
- **Passing whole leaves within one range** while the others wait: the
  page's next entries interleave across ranges, so a leaf is whole to
  pass only where no other range's next id falls inside it. Possible,
  but deep pages through an OR are rarer still, and their documents
  aren't read anyway where the entries decide.

## 167.8 Tests
- **Results** (`an_or_of_eqs_merged_by_id_gives_what_a_sort_in_memory_gives`):
  3,000 documents with indexes on `t`, `(t, u)` and `tags[*]`, values
  chosen to share encodings (two strings cut alike past the key budget,
  2^60 and 2^60 + 1, `Int(5)` and `Float(5.0)`), nulls and missing
  fields. 150 random filters each round: an `in` of two to four values
  on `t` or on `tags[*]`, values repeated now and then, or an OR of two
  to four branches each an `Eq` on `t`, on `t` and `u`, or on `tags[*]`;
  now and then one or two `_id` comparisons of every kind and a
  condition on an unindexed field; `_id` in both orders, now and then a
  second key; skips up to 400 and limits of 1, 7 and 50. Each gives what
  `apply_to` over every document gives, untyped, typed and streamed,
  with a selection and an aggregate: after the inserts, more inserts,
  updates, deletes and an `update_fields`, and after reopening. Over 500
  of the filters are merged.
- **Work done** (`an_or_of_eqs_merged_by_id_reads_only_its_page`), 5,000
  documents of five tenants:
  - "tenant in (t1, t3), newest 20": `IndexUnion { in_order: true }`
    on `tenant` twice, both reads backward, covering, each bounded by
    its `Eq`; at most 22 entries, 20 documents; with a second sort key,
    both served;
  - page 25 of 20: the page's 20 documents read, at most 502 entries,
    four decisions at most;
  - after the 600th id: each read bounded by its `Eq` and the
    comparison, at most 22 entries, 20 documents, the 601st to the
    620th; `lte` backward from it;
  - `tenant == t1 OR status == open`: merged across two indexes, not
    covering, 20 documents read, as a sort in memory gives;
  - a compound index fixed whole by each branch: merged; `tenant in
    (…)` hinted to `(tenant, tries)`, fixed in part: not merged, every
    entry read, the same 20;
  - without a limit, or sorted by `tries` first: not merged, the same
    documents as a sort in memory;
  - an id beside a range: not merged;
  - hinted to `_id`: the merge among the alternatives, counted as the
    run reads it, fewer, without documents.
- **The planner's own checks** (`explained`), which every filter of
  the model tests passes through: a merge reads exactly its plan's
  fields, stops at skip plus limit, and serves every sort key.
- **Updated:** §96's `finding_by_id_or_by_value_beats_the_walk_by_id`
  now expects `IndexUnion { in_order: true }` for `group == 1 OR n ==
  2` sorted by `_id`.
- **The corpora** (§47, §72, §96, §100, §101, §102, §103, compound,
  sparse, multikey) pass unchanged.

Eighteen hand-made mutations, all caught:
- a range fixed in part taken as fixed whole;
- the `_id` comparisons not narrowing the ranges;
- the `_id` range placed outside the prefix;
- the `_id` comparisons left out of `by`;
- a merge without a limit;
- a merge for a sort on another field;
- a document in two ranges handed out twice;
- the heap's order reversed for a forward walk;
- the ranges walked forward for a descending sort;
- entries deciding where the ranges are of several indexes;
- the reads not marked `covering`;
- the merge planned but not run;
- the plan not saying `in_order`;
- the sort said to be done in memory;
- an alternative merge said to read documents;
- an alternative merge counted as its ranges whole;
- a range's next entry read before the one taken is handed out;
- a range's last decision not swapped in (caught by the deep page's
  count of decisions).

The first run left two alive, a merge without a limit and one for a
sort on another field: no test asked for either. The work-done test
now does.

## 167.9 Measurements
Benchmark (§48), a new row: "tenant in (x, y) by id, first 20", 100
pairs of tenants of 1,000 tasks each. Each store answers it its own
way: SQLite with `tenant IN (?, ?) ORDER BY id LIMIT ?`, redb and sled
by taking each tenant's first 20 ids from their index, which hold the
first 20 of both, then sorting those and reading 20 documents.
trunkdb before (`fc56e4a`) and after, back to back, three runs each,
on 100,000 documents:

| | before | after |
|---|---:|---:|
| tenant in (x, y) by id, first 20 | 1.10–1.18 ms | 29–33 µs |

36–37× faster, beside redb's 17.8–19.6 µs and sled's 24.8–29.8 µs in
the same runs; SQLite takes 2.47–2.90 ms. One tenant by id, the same
walk over one range (§103), took 21.7–26.2 µs in them: the second range
and the heap cost some 7 µs.

Every other row stays within its noise, before and after alike: no
other plan changed.

A second row, "tenant in (x, y) by id, page 25 of 20", since paging is
by skip and limit: the same pairs, skipping 480. Three builds, three
runs each, back to back: before the merge (`fc56e4a`), the merge with
one decider and its one last decision, and with each range's own
(167.4):

| | before | one last decision | one per range |
|---|---:|---:|---:|
| tenant in (x, y) by id, first 20 | 1.04–1.11 ms | 27.7–29.2 µs | 27.8–32.6 µs |
| tenant in (x, y) by id, page 25 of 20 | 1.06–1.17 ms | 77–87 µs | 66–68 µs |

The deep page is level with redb's 65–68 µs, and ahead of sled's
287–311 µs and SQLite's 2.7–3.4 ms. The first page doesn't change: it
decides about 20 entries either way.

An `in` of many values, 100,000 documents of 1,000 tenants, newest 20
and page 25 of 20, by a program of its own, two runs each:

| values | before | one last decision | one per range |
|---:|---:|---:|---:|
| 2 | 121 µs, 97 µs | 22–23 µs, 28–29 µs | 22–24 µs, 24–25 µs |
| 10 | 535 µs, 530 µs | 57–60 µs, 146–148 µs | 57–62 µs, 106–116 µs |
| 100 | 7.1 ms, 7.0 ms | 0.47 ms, 0.79–0.81 ms | 0.46–0.49 ms, 0.60–0.65 ms |
| 500 | 43 ms, 43 ms | 2.4 ms, 3.9 ms | 2.4 ms, 3.9 ms |

Most of what an `in` of hundreds costs is opening its ranges, a walk
down the tree each. A decider built per range instead took 34 ms for
500 (167.7).

## 167.10 Limits
- **Only ranges fixed whole:** a branch with a range, `tenant == a OR
  created > t`, or a compound index fixed in part, still sorts in
  memory (167.7).
- **Ids beside the ranges** still sort in memory (167.7).
- **No leaf passed whole** in a skip (167.4).
- **Without a limit**, every match is read and sorted, as for every
  walk (§96.4).
