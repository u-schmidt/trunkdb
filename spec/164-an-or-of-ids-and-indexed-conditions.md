# 164. An OR of ids and indexed conditions (`query.rs`, `collection.rs`)

"The documents the other side asked for by id, and everything changed
since the last sync": a sync's question, and a filter's.

```rust
let wanted = Filter::new().any_of([
    Condition::is_in("_id", asked_for),
    Condition::gt("updated", last_sync),
]);
```

```
_id in (id("…"), id("…")) or updated > date("2026-11-01T00:00:00Z")
```

Each half alone has a plan. An OR of ids alone is looked up in the
primary index (`ById`, §61); an OR whose every branch an index bounds
reads a range per branch, each document once (`IndexUnion`, §36.3).
Together they have none: a branch that only names ids has no index
range, so the OR is unbounded, and a filter with nothing else is a scan
of the whole collection (§61.4). On 100,000 documents, ten ids and the
hundred changed since: every document read, for 110 that match.

This section reads such an OR as a union of both kinds: the ids looked
up in the primary index, and each other branch's index range read, each
document once, then checked against the whole filter, as `IndexUnion`
does. No file format changed, and no filter finds anything other than
before: only fewer documents are read.

## 164.1 Which OR
- **An OR whose every branch either names ids or is bounded by an
  index.** A branch names ids as §61 says: an `Eq` on `_id` with an id,
  an `in` of ids (an OR of such), an AND of which one condition names
  ids. A branch is bounded as §36.3 and §43 say, by an `Eq`, a range,
  `starts with`, an `in`, a compound index's fixed fields, or an OR
  within it bounded the same way.
- **A branch that names ids is looked up,** even where an index would
  bound it too: a few ids beat any index, as §61 has it for a whole
  filter.
- **One branch neither,** `_id = x or n != 5`, or a branch on a field
  without an index: the OR stays unbounded, as now.
- **Nested:** inside an AND, the OR is one of the conditions
  `bounds_for_all` weighs, of kind `Union` as any OR is (§43.3):
  `tenant = "a" and (_id in (…) or updated > t)` reads the index on
  `tenant` if it has one, as for any OR beside an `Eq`; without it, the
  union. Inside another OR, its branches are the outer OR's.
- **Every branch naming ids** stays `ById`, the whole filter's ids
  looked up (§61): this section adds the mixed case only.

## 164.2 How it reads
- **The ids first:** the ids of every branch that names some, together,
  sorted, each once, looked up in the primary index as `ById` looks
  them up; an id with no document is passed over.
- **Then each other branch's range,** in the order of the branches, as
  `IndexUnion` reads them.
- **Each document once:** a document found by id and again in a range,
  or in two ranges, is a candidate once, by the id its key ends with
  (`key::doc_id`, which is the whole key for the primary index).
- **Every candidate checked against the whole filter:** a looked-up id
  whose document fails another condition of the AND around the OR is no
  match, as with `ById`. No entry decides the filter alone: candidates
  come from several indexes, as in any union over several (§100.3).
- **The order of candidates** is no promise, as for any union; a sort
  sorts them.

## 164.3 Sort, limit and the walk in order
`index_order` walks an index in sort order instead of finding by value
only where nothing else finds by value (`found_by_value`, §34.2): an
OR this section bounds counts as finding by value, as any union does.
So `_id in (…) or updated > t order by updated limit 20` reads the
union and sorts it in memory, where it walked the index on `updated`
from its start; with `order by _id limit 20`, likewise, where it walked
the primary index (§96). A sort through a merge of the union's ranges
in order is §103.7's planned merge, and isn't part of this.

## 164.4 What it says
- **`QueryPlan::IndexUnion { fields }`**, with `_id` among the
  `fields`, first: the primary index, named as `IndexOrder` names it
  (§96). No new variant, so no program matching plans breaks, and a
  program that looks for `ById` sees what it saw before for a filter of
  ids alone.
- **`explain_run`** has a read of `_id` with the ids as its bounds,
  `_id in (…)`, its entries the documents found, as `ById` reports
  them; then a read per range. `ExecutionStats` counts each document
  examined once.
- **`trunkdb find --explain`** words the plan as "ids looked up, and
  the indexes on updated, together"; a union without ids as before.
- **The plans passed over** (§133) count the lookups among the entries
  a plan would read.

## 164.5 Hints
A hint offers one index (§132.2), and this plan reads two, the primary
and another, so a hinted filter never gets it: `use index updated` reads
the OR as before, bounded only if `updated` bounds every branch, and
`use index _id` likewise. A hint the plan can't follow fails with
`Error::Hint` as before. A union of the primary and a named index
hinted at once would need a hint of several indexes, which no program
has asked for.

## 164.6 Left out
- **A range of ids in a branch,** `_id > id("…")`, "created since":
  ids sort by when they were made, so the primary index could read it as
  a range; it isn't one of the branches §61 names ids by, and stays
  unbounded in an OR. When asked.
- **Ids weighed against the ranges by size:** a branch naming 50,000
  ids is looked up even where an index would read fewer; rules, as
  everywhere (§28.4).
- **Counting from entries:** `count` reads every candidate, as for any
  union.

## 164.7 Tests
In `tests/or_with_ids.rs`, on 300 documents with indexes on `updated`,
`(tenant, updated)`, `title`, `tags[*]` and a sparse one on `due`, a
deleted document's id and an id that never had one among those asked
for:
- **The plan:** `IndexUnion` with `_id` first for ids beside a range,
  an `Eq` on a compound index's first field, `starts with`, a multikey
  branch, a sparse index's branch that rules out null, and an OR of two
  indexed branches; `ById` where every branch names ids, one of them an
  AND with an id; a scan where a branch is on a field without an index,
  or a `!=`. Each finds what the filter hinted to no index finds.
- **Each document once:** of ids 3, 290 and 295 and `updated > 280`, 20
  found and 21 examined, 290 and 295 not again from the range, which
  reads 280 too, as any `>` range does; the lookups one read of 5
  entries, the range 20.
- **The AND around it:** without an index on its other condition, the
  union, a looked-up document that fails it no match; with one on
  `tenant`, that index.
- **Sort and limit:** by `updated` and by `_id`, either way, with a
  skip: the union, sorted, the same documents in the same order as the
  scan, 22 examined at most.
- **The other reads:** `count`, `aggregate` and `explain_aggregate` (8
  examined), `update_fields`, `delete_many`, a batch's precondition;
  `explain_run` with a scan among the plans passed over, and run as a
  scan, the union passed over with its 3 lookups and 5 range entries
  counted.
- **Hints:** `use index updated` and `use index _id` fail with
  `Error::Hint`, as they did: neither bounds the OR alone.
- **Measured,** a test run by hand in release: 100,000 documents, ten
  ids and the 100 changed since, 110 examined, 0.17 ms, where the scan
  took 76 ms.

In `tests/cli.rs`: `trunkdb find … --explain` says "ids looked up, and
the index on age, together".

In `collection.rs`, the test that entries decide as documents would
(§100) counts a union with lookups among those over several indexes,
whose entries never decide.

Fuzzing: the `queries` target (§105) built `_id` comparisons, but in
2,000 programs of its smoke run not one OR this section reads. Its OR
of `eq`s on two paths now draws `_id` too, an id of a stored document
or not: in 1,000 programs, the union with `_id` first beside each of
the eight indexes, every read agreeing with the model and every other
plan. Its saved corpus, 2,045 programs, replayed by the guided target
(8.5 minutes, 3,003 runs): nothing found; at about 6 programs a second,
the time went to the replay, not to new programs, so a longer run by
hand remains.

## 164.8 Mutations
Each made by hand and the tests run:
- **The lookups left out of the union,** **a document found by id and
  in a range handed out twice,** **a branch naming ids read as
  unbounded,** **a union with lookups taken as one index's,** whose
  entries then decided, **`found_by_value` not counting the mixed OR,**
  and **`_id` left out of the plan's fields:** caught by
  `tests/or_with_ids.rs`.
- **The lookups not counted among the entries of a plan passed over:**
  survived, as no test ran the filter another way and read the union's
  count; caught once the test ran it as a scan.
