# 47. Sorting by several fields (`query.rs`, `collection.rs`)

Real and tested: a sort with any number of keys, each ascending or
descending. It sorts by the first key, breaks ties with the second, and
so on; documents equal in every key come in id order. An index serves
as many of the leading keys as its fields allow, and the rest are
sorted in memory, but only among documents that tie on the keys the
index already ordered.

```rust
// By status, and within a status newest first:
tasks.find(Filter::new().sort_asc("status").then_desc("created").limit(20))?;
// By status, then oldest first: `(status, created)` gives both.
tasks.find(Filter::new().sort_asc("status").then_asc("created").limit(20))?;
// As a literal:
Filter { sort: vec![Sort::asc("status"), Sort::desc("created")], ..Filter::default() }
```

## 47.1 API
- `Filter.sort` changed from `Option<Sort>` to `Vec<Sort>`, most
  significant key first, where empty means unsorted. This breaks code
  that builds a `Filter` literal with `sort: Some(…)`. The fix is
  `sort: vec![Sort::desc("started_at")]`, using the new constructors
  `Sort::asc` and `Sort::desc`.
- `then_asc`, `then_desc` and `then_by` add a key after the ones
  before. `sort_asc`, `sort_desc` and `sort_by` still replace the whole
  sort, as their documentation always said. This is like LINQ's
  `OrderBy(…).ThenByDescending(…)`, and SQL's `ORDER BY status,
  created DESC`.
- `query::compare_by(keys, a, b)` holds the order: key by key, with
  `sort_order` (§34.1) for each, until one differs. `apply_to` sorts
  with it, stably after sorting by id, so full ties stay in id order as
  before.

Rejected:
- **`sort_asc` appending when called twice.** A second call used to
  replace the first, and code that relied on that would silently change
  meaning. The `then_` methods say what they do.
- **One sort direction for all keys.** "By status, newest first within
  it" needs two directions, and it is the case the SPEC's own examples
  keep coming back to.

## 47.2 Which index, serving how many keys
`index_order` still starts from the first key, looking for an index
whose field after its `Eq`-fixed fields is that key (§34.2, §43.3). It
now counts how many keys that index **serves**: how many of the
following sort keys are its next fields, in order and in the same
direction as the first. An index is read in one direction only, so a
key that goes the other way ends the run. `(status, created)` serves
two keys for `status asc, created asc` (or both `desc`), and one for
`status asc, created desc`.

Among the candidates, the one that fixes more fields by `Eq` wins, and
then the one that serves more keys. Fixed fields narrow what is read;
served keys only save sorting in memory.

`OrderedRead` tells the sorted read the index, the range, how many
fields are fixed and how many keys are served.

## 47.3 The sorted read
`read_in_index_order` groups the entries by the values of the served
keys, and only those, instead of by the first key alone:
- **An exact group with nothing left to sort** is read one document at a
  time until the limit, as before.
- **A group that needs sorting** is read whole and sorted by every key.
  That's the case when keys remain after the served ones, or when a
  value may stand for several (a long string cut to the key's share, a
  number beyond 2^53). The sort sees the true values, so ties among the
  served keys are ordered by the remaining keys.
- **Groups are ordered** value by value, each in the first key's
  direction, and for a compound index unordered values (`TAG_OTHER`)
  come last within their position, in both directions, as `sort_order`
  puts them. Before, the groups were reversed as a whole and the
  unordered ones moved to the end, which was only right for one key.
- **A group ends at the first value that may stand for several.** The
  randomized test found this: three `desc` keys on `(a, b, c)` with `b`
  a 1,200-byte string, cut to its share of a three-field key (§43.2). Grouped by
  all three values, the entries sharing a cut `b` were ordered by `c`,
  though their real `b`s differ past the cut and should decide first.
  Now such a group runs up to and including the cut value, and is
  sorted in memory by the real values.
- **The scan for unordered values** after a one-field index (§34.2) now
  sorts what it finds by the remaining keys. It still stops at the limit
  when there's only one key, since id order is their order then.

The cost of a key the index doesn't serve depends on its groups. In the
typed test, sorting by `created, tenant` through the index on `created`
reads 15 documents for 12 results: groups of five tasks with equal
`created`, each sorted by tenant. `status asc, created desc` through
`(status, created)` reads one whole status (1,000 of 3,000 tasks) for
20 results. That is still a third of the reads of a full scan, but it
shows why two directions are worth a separate index where it matters.

## 47.4 Tests
- `query.rs`:
  - The in-memory order for two keys in either direction, and for a
    `Filter` literal.
  - Which index serves how many keys: two or three keys in one
    direction, a later key the other way, a key that isn't the index's
    next field, fixed fields beating served keys, and `sort_desc`
    starting over after `then_asc`; an index fixing a field against one
    serving more keys.
- `collection.rs`:
  - The randomized compound test now sorts by zero to three of `a`, `b`
    and `c`, mostly in one direction and sometimes mixed. That covers
    long strings, huge numbers, nulls, unordered values and every index
    plan, sparse or not, through every kind of write and a reopen, with
    the sorted result compared to a sort in memory. It's the test that
    found the cut-value grouping above.
  - The nested-filter random test sorts by `v` descending and now
    sometimes then by `w`, so ties go through the in-memory part of the
    index read, the unordered scan and the cursor.
  - Typed tasks with ties in `created`: `status, created` in both
    directions reads exactly 20; mixed directions read one status; the
    index on `created` serves `created, tenant` with 15 reads. All
    compared to sorting everything.
  - Arrays in the indexed field, found by the scan after the index:
    sorted among themselves by the next key, the best of them taken,
    not the first by id.
- Checked by breaking it on purpose, fourteen ways. Each of these
  fails a test:
  - only the first key compared in memory;
  - an index serving keys whatever their direction, or whatever its
    fields;
  - ranking by fixed fields only, or by served keys before fixed ones;
  - `sort_asc` not starting over;
  - groups not ended at a cut value;
  - groups not put in id order when fields follow;
  - later keys never sorted in memory;
  - unordered groups not put last, or `Desc` groups not reversed;
  - the unordered values after the index taken in id order, or the scan
    for them stopped at the limit despite later keys;
  - only the first value of a group checked for exactness.

  Three passed at first: ranking by served keys before fixed fields,
  and the two about the scan after the index. No test had two indexes
  where the rules disagree, and the random filters rarely got past the
  index with more unordered matches than the limit. The two targeted
  tests above catch them now.

## 47.5 Limits
- **An index serves keys in one direction only.** Mixed directions need
  memory for the later keys, or a second index. Keys stored in
  descending order per field (SQL's `CREATE INDEX … (a, b DESC)`) would
  lift that; it isn't planned.
- **No sort by array elements** (§42.3), as before.
- **No sort without a limit through an index** (§34.2), as before.
