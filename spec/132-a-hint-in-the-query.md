# 132. A hint in the query (`query.rs`, `collection.rs`, `query_text.rs`)

The planner follows fixed rules (§28.4, §36.3, §34.2), and the rules
don't look at the data. ROADMAP.md names the two places where that
costs a read: an `Eq` on a value nearly every document has
(`status == "done"`, 99% of them) beats a range that holds a few
(`created > yesterday`); and a walk in sort order with a limit, beside
a range filter, reads the whole index when the range's matches are
rare. Either costs at most about a scan. But the program knows its data
and can't say so.

Now it can, in the query:

```rust
let recent = Filter::new()
    .eq("status", "done")
    .gt("created", yesterday)
    .use_index("created");
```

```
status = "done" and created > 5 use index created
```

The plan stays a function of the query and the indexes, as everywhere
else: the hint is part of the query.

## 132.1 The API
- **`Filter::use_index(fields)`** reads the index on `fields`, as
  `ensure_index` takes them: `"created"`, `["status", "created"]`.
  `"_id"` is the primary index, which a lookup by id and a sort by
  `_id` read.
- **`Filter::no_index()`** scans: every document, read from the
  primary index in id order, as a filter no index can bound is read.
  No lookup by id either. It never fails.
- A second hint replaces the first, as a second `limit` does.
- **`Error::Hint { collection, index, reason }`** where the hint can't
  be followed (132.3).
- **`Explained::hinted`**: the plan is the one the hint asked for.
  `trunkdb find … --explain` says `plan: index on created, as hinted`.

The fields, not the name `Collection::indexes` prints, as ROADMAP.md
had it. `(status, created)` is a name for people. A program already
holds the fields it gave `ensure_index`, and a typo in the name of a
compound index would be one more way to miss it.

## 132.2 How a hint chooses
The hint narrows what the rules choose among. Nothing else changes.

- **`Filter::offered(indexes)`** is the hinted index alone, as a
  one-element slice of the catalog's, or nothing for `no_index` or the
  primary index. `index_ranges` and `index_order` choose among that.
  So does the aggregate's walk of a whole index (§107.5). A slice of
  the slice is the same slice, so narrowing twice is narrowing once,
  and the helpers they call need no change.
- **The primary index** is offered without a hint, or with
  `use_index("_id")`. `ids` returns `None` otherwise, and `id_order`
  doesn't walk it.
- **With nothing offered there's no range at all**, not even for an OR
  of no branches. Every index bounds that equally well, with no range.
  Without a hint it never gets that far: it names no ids, and `ids`
  takes it first.

So the hinted plan is the rules' plan on a collection that has that
one index. The same ranges, the same bounds from the same conditions,
the same walk where a sort with a limit allows one, the same deciding
from entries (§100). Every way of reading follows from those
functions: `find`, `cursor`, `count`, `aggregate`, the writes that
find (`update_many`, `update_fields`, `delete_many`, `upsert`), and the
snapshot views.

## 132.3 A hint that can't be followed
It is an error, not a quiet return to the rules. A plan that changes
without a word is what pinning plans in SQL Server or PostgreSQL tries
to stop. A program that pinned a read to an index and then dropped the
index should hear about it once, not find out from a slow page.

`follow_hint` refuses two cases, before anything is read:
- **No such index:** none on those fields. That includes a collection
  that doesn't exist. Fields in another order are another index.
- **Nothing to read it by:** offered that index alone, the rules would
  scan. There's no condition it can bound, and no sort with a limit
  whose order it gives. A sort with no limit is no walk (§34.2).

Every read checks, so every read refuses alike: `explain` too, so it
says what `find` will do. A write that finds (`delete_many`) rolls back
and writes nothing. `find_one` reads its filter with a limit of one, so
a hint a walk can follow only under a limit works there and fails for
`find`. That is the same rule on a different filter.

## 132.4 As text
```
… [limit <n>] [offset <n>] [use index <fields> | use no index]
```
Last, after `offset`. The fields are one path, or several in
parentheses: `use index (status, created)`. They print back the same
way. Keywords are read in any case, as everywhere in the language.

**Two words, so nothing is reserved.** `use` starts the hint only when
`index` or `no` follows it. A field named `use` is still a field:
`use = 1 use index use`. This is the "kept free" rule of §111.3: the
new words come where a field can't be.

`trunkdb find` takes a filter as text, so it takes hints too.

## 132.5 Rejected
- **Counting before choosing**, MySQL's index dives. They count the
  candidate ranges with `BTreeIndex::count` (§101.4), up to a cap,
  where the rules see a tie. No statistics are kept, nothing to go
  stale. But the same query would get another plan on other data,
  which is the one thing the rules promise not to do.
- **A quiet fall back** to the rules where the hint can't be followed.
  MySQL's `USE INDEX` and SQL Server's table hints differ here. SQL
  Server fails the query, and so does trunkdb.
- **A forced plan kind** (`IndexOrder` on `created`). A hint names an
  index, and the rules choose how to read it. A plan kind would be a
  second thing to keep stable, for a choice the rules make well once
  the index is fixed.
- **`no_index` keeping lookups by id.** A scan that still looks up ids
  would be a hint that doesn't do what it says. A baseline for
  measuring wants every document read.
- **A hint on a condition**, in an aggregate column's `filter` or
  inside an OR. The plan is the whole filter's.
- **`QueryPlan::Hinted(Box<QueryPlan>)`:** every `match` on a plan
  would miss hinted ones. `Explained::hinted` says it beside the plan.

## 132.6 Tests
- `tests/hints.rs`:
  - The two ROADMAP.md cases. The skewed `Eq` read through the range:
    10 entries instead of 190, the same 10 tasks. A range read and
    sorted in memory instead of a walk, and back, hinted to the sort
    field.
  - `no_index` scans, by id too, and never fails. `use_index("_id")`
    is a lookup by id or a walk in `_id` order. Hinted to a secondary
    index, a sort by `_id` doesn't walk the primary one.
  - A compound index by its fields: the rules' choice, and another
    index instead. The fields in another order are refused.
  - Refused hints, an unknown index and an index with nothing to bound
    it, fail on every read: `find`, `find_one`, `count`, `cursor`
    sorted and not, `explain`, `explain_run`, `aggregate` four ways,
    `explain_aggregate`, `delete_many`, `update_many`, `update_fields`,
    `upsert`, and a snapshot's view. Afterwards nothing is written.
  - A collection that doesn't exist: refused, and `no_index` finds
    nothing.
  - Aggregates and `count` under hints: the same rows.
  - Seven filters under seven hints: the same ids in the same order,
    the same count, the same number from a cursor, or refused by
    `explain` too.
  - Text goes through the same plans. A later hint replaces an earlier.
  - `false` under every hint finds nothing, and its plan without a hint
    is unchanged.
- `a_hint_reads_as_use_index_and_no_index` (`query_text.rs`): the
  forms, any case, backticks, round trips, a field named `use`, and the
  errors.
- `find_explain_says_how_it_found_them` (`tests/cli.rs`): `as hinted`,
  and an unknown index is exit code 1.
- **The `queries` fuzz target** (§105) reads every query a second time
  under a random hint: no index, `_id`, or any of its nine indexes, made
  or not. The same answers from `find`, the typed `find` through the
  text, `cursor`, `find_one` and `count`. Or `Error::Hint` from all of
  them alike, with `find_one` judged by its own limited filter. 3,000
  programs of the smoke run pass. Every hinted plan kind comes up, and
  so do refusals.

The fuzz target found two things on its first run. A `find_one` that
follows a hint `find` refuses (132.3) was the harness's mistake. And
`false` under a hint panicked in `candidate_entries`: it took the
first of no ranges, where the comment above it said there could be
none. The OR of no branches had only ever reached it as a lookup of no
ids. Both cases are now in the tests.

## 132.7 Mutations
Each line of the hint broken alone; each caught by the tests above
(the fuzz target not counted):

| Mutation | Caught by |
|---|---|
| The hinted index offered with every other one | the skewed case |
| `no_index` offering every index | `no_index` scans |
| An unknown index offering every index | refused hints |
| `no_index` keeping the primary index | `no_index` scans, by id |
| Any one-field hint counted as `_id` | a sort by `_id` hinted elsewhere |
| `ids` not asking the hint | `no_index` by id |
| `id_order` walking the primary index under any hint | a sort by `_id` hinted elsewhere |
| `index_order` choosing among every index | the walk replaced by a range |
| A range with nothing offered | `false` under every hint |
| `use_index` doing nothing | every hinted plan |
| No "no such index" refusal | refused hints |
| No refusal of a scan | refused hints |
| `follow_hint` left out of `find_in`, `explain`, `upsert`, `cursor`, `count` | refused hints, each read |
| `follow_hint` left out of aggregates | an aggregate's walk of a whole index, hinted |
| The aggregate's walk choosing among every index | an aggregate follows the hint |
| `hinted` not set, `explain_run` or `explain_aggregate` | the skewed case; the aggregate |
| The hint not parsed, not ending a condition, not printed, no space after `offset` | the text tests |
| `use` alone starting a hint | a field named `use` |
| `as hinted` not printed | the CLI test |

## 132.8 Limits
- **An aggregate with no conditions** may walk a whole index that holds
  what it reads (§107.5). A hint to that index is refused anyway, as
  `explain` would scan: `follow_hint` asks what `find` would do. Leave
  the hint out there; the rules take the index.
- **`count` with a sort and a limit** follows the hint the walk allows,
  and counts as it always does, without walking (§72).
- **The diagnosis ROADMAP.md plans with the hint** isn't here:
  `explain_run` counting the ranges the rules passed over, and saying
  which would have read fewer. It stays open.
