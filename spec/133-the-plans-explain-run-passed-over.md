# 133. The plans `explain_run` passed over (`collection.rs`, `explain.rs`, `index/btree.rs`)

A hint (§132) chooses the index where the planner's rules can't know
better. But to know where a hint pays, and by how much, a program had
to run the query once per index and compare `keys_examined` by hand.
Now `explain_run` does that once, on the same snapshot, and lists each
plan it passed over with what that plan would have read:

```
plan: index on status
read: status, where status = "done", 190 entries
instead: index on created, 10 entries, fewer
instead: scan of every document, 200 entries
```

The other way round shows how much a hint gains, not only that it
does. Hinted to `created`, the run reads 10, and the rules' plan it
passed over shows its whole 190, not "more than 10".

## 133.1 The API
- **`Explained::alternatives: Vec<Alternative>`**, filled by
  `explain_run`, empty from `explain_aggregate`.
- **`Alternative`** (`#[non_exhaustive]`):
  - `plan`: the plan, as `explain` names it for that hint.
  - `entries`: an `EntryCount`.
  - `reads_documents`: whether it reads a document for each entry to
    check it, or its entries decide (§100).
  - `fewer`: it reads fewer entries than the run did.
- **`EntryCount`** (`#[non_exhaustive]`): `Exactly(n)` or
  `MoreThan(n)`, and `at_least()` for the number either way.
- **`trunkdb find --explain`** prints an `instead:` line for each,
  after the sort: the plan in the words of the `plan:` line, the
  entries, `its entries decide the filter` where they do, and `fewer`.

## 133.2 Which plans
The filter hinted to each index of the collection, then to `_id`, then
to a scan (§132.1). For a hinted run, the filter without its hint comes
first: the rules' plan, so the run says whether the hint still pays.

Each one is the planner's own plan for that hint, as `explain` would
name it (§132.2). Nothing about choosing a plan is written a second
time, so an alternative can't drift from what a hint really does.

Left out are:
- the hints that can't be followed (§132.3), which have no plan to
  read;
- the plans already listed, the run's own first. A hint to the index
  the rules chose gives the run's plan again. A hint to `_id` for a
  filter that names ids gives the lookup the rules chose.

A filter only a scan can read lists nothing: every hint to an index is
refused, and `no_index` gives the scan it ran.

## 133.3 Counting a range, whole
An alternative that reads index ranges is counted with
`BTreeIndex::count_at_most`. That is `count` (§101.4), stopping after
so many leaves. A leaf all inside the range is counted from its slot
directory, its keys never decoded. Only the leaves at the range's ends
are stepped through. No document is read. A union's ranges are counted
one after the other, their leaves together. A scan counts the primary
index whole. Lookups by id count one each, as the run counts them.

So the count is the run's own `keys_examined` under that hint, to the
entry. The fuzz target holds it to that (133.6).

**A safety cap, not a budget:** 1,000 leaves, `ALTERNATIVE_LEAVES`,
8 MB of pages and some 100,000 entries or more. Past it, the entries
counted so far are `MoreThan`. It is far above what a query that a hint
would improve reads, so the gap shows whole. It is there so that an
`explain_run` on a large collection can't take minutes counting a scan
it didn't run.

`count_at_most` checks the cap before it enters a leaf. A cap already
used up by a union's earlier ranges counts nothing further. At the cap
it looks one entry ahead: a range that ends right on the last leaf
allowed is `Exactly`.

**Rejected: stopping at what the run read**, as first planned. It would
bound the diagnosis by one query's worth per index. But a hinted run
that reads 40 would show the rules' plan as "more than 40", not the
40,000 it would read. How much a hint gains is what the diagnosis is
for. Counting a range costs a page per leaf and no document, and
40,000 entries are a few hundred leaves.

## 133.4 A walk in sort order, as far as the run read
A walk with a limit (§34.2) reads until it has `limit` matches. How far
that is depends on where the matches are, which only checking them
tells, and that means reading documents. So an alternative that walks
is run: `find_in` under its hint, on a `Trace` with a budget of the
run's `keys_examined`. The walk stops as soon as it has read more than
that (`Trace::spent`), at the top of each step and after the scan for
values the index doesn't hold. It then reports `MoreThan(budget)`.
Otherwise it reports the entries it read, exactly.

A walk is the one alternative that reads documents, at most one query's
worth. It is never `fewer` when it runs out of budget, which is right:
it read at least as much as the run.

**Rejected:** an upper bound for a walk, the size of its range. It
can't say "fewer". A walk that finds its 5 matches in the first 42 of
4,000 entries would be listed as 4,000.

## 133.5 Rest of the design
- **Comparing entries only.** A document read per entry is the other
  half of a plan's cost. It is a flag here, not a number. Counting
  them would mean reading them, or guessing.
- **`fewer` only for `Exactly`.** A range past the cap may still read
  fewer than a run that read more than the cap. That isn't known, so
  it isn't claimed.
- **After the run.** `elapsed` is taken first, and the alternatives are
  counted on traces of their own. `stats` and `reads` are the run's
  alone, as they were.
- **Always on.** `explain_run` is for diagnosis. The cost is bounded:
  one count per index, capped, and walks within the run's own reads.
  An option to turn it off would be a type for one flag.
- **Not for `explain_aggregate`.** An aggregate reads from index
  entries in its own way (§107.5), and its plans would need their own
  alternatives.
- **Rejected: choosing by the counts.** The diagnosis tells the
  program, which may add a hint. The plan stays a function of the
  query and the indexes (§132.5).

## 133.6 Tests
- `tests/alternatives.rs`:
  - The skewed case: the rules read 190 entries for `status`, the
    `created` range is listed with exactly 10 and `fewer`, and a scan
    with 200. The indexes the filter can't be hinted to aren't listed.
  - Hinted to `created`, the rules' plan shows its whole 190, not
    fewer.
  - A walk passed over is walked as far as the run read: `MoreThan(20)`
    where it needs 42 entries for 5 matches, exactly 2 with a limit of
    one. A walk the rules chose lists the range at 20, fewer.
  - Lookups by id, 3 entries. A union of two ranges, 40 entries,
    whose entries decide. A union across two indexes, which no hint
    gives, listed for a hinted run as the rules' plan, a document per
    entry. A walk in `_id` order, which only the `_id` hint gives. Six
    more filters with sorts, skips, limits and a selection.
  - For every alternative listed: run with its hint, it has the plan
    the alternative names. It reads exactly the entries counted, or
    more than `MoreThan` says. `fewer` agrees with the run's
    `keys_examined`, and `reads_documents` with whether its reads
    decide.
  - The run's own reads and counts don't change. `explain_aggregate`
    lists nothing, and neither does a filter only a scan reads, or a
    collection that doesn't exist.
- `an_alternative_is_counted_through_so_many_leaves`
  (`collection.rs`), with `ALTERNATIVE_LEAVES` at 4 in the unit tests:
  1,500 entries past the cap are `MoreThan`, a short range is exact.
  Two ranges of 400 that fit alone are `MoreThan` together.
- `a_walk_passed_over_reads_no_more_than_the_run` (`collection.rs`):
  the run reads the 30 documents of `c = 99`. The walk down `b` it
  passed over, which needs some 400 for its 5, is `MoreThan(30)`, and
  the whole `explain_run` reads at most twice the run's documents. It
  read 431 without the stop.
- `a_count_at_most_stops_after_its_leaves` (`btree.rs`): with as many
  leaves as the range has it is whole, with one fewer it isn't. One
  leaf's worth for a cap of one, none for a cap of none. A range with
  no end is whole at its own number of leaves, by the look ahead. A
  range inside a leaf is whole in one.
- `find_explain_says_how_it_found_them` (`tests/cli.rs`): the
  `instead:` lines, and a hinted scan listing the rules' index as
  fewer.
- **The `queries` fuzz target** (§105) picks one alternative of every
  read at random and runs it under its hint. The plan named, the
  entries counted exactly or more than, and `fewer` must all hold. In
  the 3,000 programs of the smoke run, both `Exactly` and `MoreThan`
  came up and held.

The unit test found a bug before it shipped. `count_at_most` with a
cap of none counted the first leaf anyway, because it checked the cap
after a leaf, not before. A union whose earlier ranges used up the cap
could meet that.

## 133.7 Mutations
| Mutation | Caught by |
|---|---|
| `fewer` for as many as the run read | the CLI test; the predictions |
| A plan listed twice | the skewed case |
| The run's own plan listed | the predictions (`assert_ne`) |
| No rules' plan for a hinted run | the union across two indexes |
| No `_id` alternative | the walk in `_id` order |
| No scan alternative | the skewed case |
| Ids counted as none | lookups by id |
| A scan never reading documents | the predictions |
| A walk out of budget reported exact | a walk passed over |
| A walk with no budget | a walk passed over |
| The walk not stopping at its budget | the documents a walk passed over reads |
| A union's leaves not added up | the leaf cap, a union |
| The cap never giving `MoreThan` | the leaf cap |
| A range always reading documents | the union whose entries decide |
| A union never reading documents | the union across two indexes |
| No alternatives at all | every test above |
| The cap one leaf late | `count_at_most` |
| A count at the cap never whole | `count_at_most`, a range with no end |
| A budget spent at exactly the run's reads | a walk passed over |
| `fewer` not printed, or `its entries decide` | the CLI test |

The walk's stop after its scan for values the index doesn't hold
survives the tests: it needs a walk that runs out of index before its
limit while over budget, which the fixtures don't make. The fuzz
target's documents hold such values.

## 133.8 Limits
- A walk's `MoreThan` says only that it would read more than the run.
- Past 1,000 leaves an index range is `MoreThan`. A run that itself
  read more than that sees such an alternative as not fewer, not as
  unknown.
- `reads_documents` is a flag. A plan that reads fewer entries but a
  document for each may still cost more than one whose entries decide.
  Comparing those is the program's call.
- An alternative for an aggregate's own reads from entries isn't
  listed (133.5).
