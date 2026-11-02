# 160. An `in` on a compound index's field, a range per value (`query.rs`)

An agent asked for the overdue tasks of two statuses wrote `status in
("open", "late") and due < …`, added an index on `(status, due)`, and
found with `--explain` that it read every entry of both statuses
(§122.5). `status = "open" and due < …` reads only that status's
entries before the date: an `=` on the first field and a range on the
second is what a compound index is for (§43.3). An `in` is an OR of
`=`s (§115), and the planner bounded the OR on its own, one range per
value of `status` and nothing of `due`, then checked `due` on each
document. The skill told agents to write the OR out with the range in
each branch; this reads the `in` as they would have.

## 160.1 The ranges
Where the field of a compound index after the ones an `=` fixes has an
`in` (`compound_bounds`, `in_bounds`):

- **One range per value,** each the one `=` with that value would read:
  the `=`s' values before it, that value, the `=`s on the fields after
  it, and a range on the field after those. `team = "b" and status in
  ("open", "done") and due < 100` on `(team, status, due)` reads `"b",
  "open"` before 100 and `"b", "done"` before 100.
- **Bounds by value, as `=`'s are,** with every field they narrow
  counted: so the rules choose between them and another index as they
  would for the same query with `=` (§43.3): more fields beat fewer,
  and an `=` on another index is weighed as against one `=` here.
- **Not for an `in` alone on the first field,** with nothing after it:
  its ranges are the OR's own (§36.3), chosen as before. Nor where a
  value has no key (an array, NaN), nor for an `in ()`, which matches
  nothing anyway.
- **Each range says what bounds it,** for `explain` (§94.2): the `=`s
  before, its own value as an `=`, and the conditions after.
- A document is read once, however many ranges hold it, as for any
  union (§36.3); and every document is still checked against the whole
  filter, so the ranges only narrow what is read.

## 160.2 Measured

On the machine of §123: 100,000 tasks, four statuses and 1,000 due days
spread evenly, indexes on `(status, due)` and `(team, status, due)`;
each the mean of 20 runs, release build, two rounds of each (the time
of a `find` varies by up to 4× between rounds; the keys read don't):

| | found | keys read, 0.29.0 → now | `find`, ms | `count`, ms |
|---|---:|---:|---:|---:|
| `status in ("open", "late") and due < 10` | 506 | 50,006 → 545 | 8.4–11 → 0.5–2.1 | 7.8–9.8 → 0.1–0.2 |
| `status in ("open", "late") and due < 100` | 5,006 | 50,006 → 5,056 | 13–18 → 5.5–9.5 | 8.1–10 → 0.8–0.9 |
| three statuses, `due` 500 to 509 | 751 | 75,005 → 808 | 14–17 → 0.7–1.0 | 14–16 → 0.14–0.16 |
| `team = "a"`, two statuses, `due < 10` | 234 | 50,000 → 254 | 5.4–6.6 → 0.24–0.26 | 1.6–1.9 → 0.06–0.07 |

- **The keys read are now about what matches:** the few over are each
  range's bound, read as every range reads it (§28.1).
- **`count` gains most,** 50 to 100×: it decides on the entries
  (§101), so it reads only keys, and now only the ones that match.
  `find` gains less where it reads many documents, which it reads
  either way.

## 160.3 Rejected
- **Each branch of any OR planned with the conditions beside it:** tried
  first. It reads the `in` the same, and also an OR whose branches have
  conditions of their own, but planned once per branch with everything
  else, and a union of ranges of other indexes for each branch where
  they don't narrow it: guarding that made it a rule of its own beside
  §43.3, and an `=` on the first field still beat the narrower union.
  The `in` is the case §122.5 found.
- **A skip-scan,** reading the index once and jumping from one value of
  `status` to the next: the values are known, so a range each is the
  same reads with no new walk.
- **An `in` on more than one field,** `status in (…) and prio in (…)`,
  as the product of their values: the second is checked on the
  documents; the product grows fast, and nothing asked for it.

## 160.4 Tests
- `tests/or_with_a_range.rs`, 800 tasks with four statuses, 200 due
  days, three priorities and two teams:
  `each_value_of_an_in_reads_only_its_range`: an `in` with a range
  after it, two ranges, a range before it in the text, read as a union,
  as many keys and documents as the `=`s of its values summed; the OR
  written out read the same.
  `an_eq_before_it_and_a_range_after_narrow_every_value`, on `(team,
  status, due)`: `=` before, the `in` first with `=` and a range after,
  and the bounds `explain` gives each range.
  `where_no_branch_gains_the_plan_stays`: indexes on one field each,
  and on `due` alone, read as before; beside an `=` on another index,
  the index chosen for the same query with `=`.
  `random_ors_beside_ranges_find_what_a_scan_finds`: 300 filters of
  `in`s, ranges, an `=`, a `not`, sorts and limits, against a scan, and
  `count` against `find`.
- The randomized index tests of `collection.rs`, which compare every
  plan with a scan, and the `queries` fuzz target, whose `in`s and
  compound indexes now reach these ranges.
- The full test suite, clippy, the MSRV build (1.89).

## 160.5 Mutations
Each run against `tests/or_with_a_range.rs` and the library's tests of
`query` and `collection`:

- not tried at all: caught;
- taken for an `in` alone on the first field (caught once a test
  pinned the plan between two ORs), or only beside three fields: caught;
- the range after it dropped; the `=`s after it, or before it, left out
  of the key: caught;
- the `=`s after it not counted among the fields (caught once a test
  pinned the choice between a three-field and a two-field index); bound
  by range rather than by value: caught;
- its value's `=` not among the bounds `explain` gives: caught;
- only the first value read: caught;
- an OR of `=`s of another field taken for the values (caught once the
  random filters had no `in` on `status` at times): caught;
- one field counted fixed too many after the `in`: caught.
