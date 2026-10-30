# 134. Aggregate columns, named last (`aggregate.rs`)

§107 put a column's name in the call that makes it: `count("n")`,
`sum("tries", "total")`. A reader took `count("open")` for what is
counted, the documents that have an `open` field or that are open, not
for a name. That isn't one reader's mistake. It is how nearly every
query tool reads it:

| Where | `count(x)` means |
|---|---|
| SQL | `COUNT(open)`: the rows where `open` isn't null |
| C#'s LINQ | `Count(t => t.Open)`: the ones that match |
| Spark, Polars | `count("open")`: the values of `open` |
| pandas | `.count()`: the values that aren't null, per column |
| MongoDB | the `$count: "n"` stage: names the result |

§107's draft came from MongoDB. Comments at every column (§107.1,
§126.1) said what the string is. Now the API says it:

```rust
Aggregate::new()
    .group_by("team")
    .count().named("tasks")
    .count().filter(Condition::eq("status", "open")).named("open_tasks")
    .avg("hours").named("mean_hours")
```

SQL writes it in the same order: `count(*) FILTER (WHERE status =
'open') AS open_tasks`. So does the text form ROADMAP.md plans,
`count() as tasks`.

## 134.1 The API
- **`Aggregate::count()`, `sum(path)`, `min(path)`, `max(path)`,
  `avg(path)`** each hand out an `UnnamedColumn`, which holds the
  aggregate.
- **`UnnamedColumn`** (in `query`, `#[must_use]`) has two methods:
  - `filter(condition)`: the column takes in only the group's
    documents that meet it (§126). Called again, the conditions are
    ANDed, as before.
  - `named(name)`: adds the column under `name` and hands the
    `Aggregate` back.
- **`Aggregate::filter` is gone.** A condition belongs to its column,
  given before its name.

What a column computes, how it is checked (§107.2) and what an
aggregate returns don't change.

## 134.2 What the compiler checks now
Two mistakes the run had to refuse, or couldn't see, no longer compile:
- **A column without a name.** An `UnnamedColumn` isn't an
  `Aggregate`: it has no `count`, `group_by` or `having`, and
  `Collection::aggregate` doesn't take one. `.count().sum("hours")`
  fails with "no method named `sum`".
- **A condition before any column.** `Aggregate` has no `filter`, so
  `Aggregate::new().filter(…)` doesn't compile. §126.2 refused it when
  the aggregate ran; that refusal and the flag it read are gone.

A condition after a column's name, which §126 gave to that column, now
fails the same way: `named` hands out an `Aggregate`, which has no
`filter`. The condition moves before the name.

This is the typestate pattern: the type says how far a value is built,
and each step's method is only on the type that allows it.

## 134.3 Breaking, the same way each time
Every aggregate written so far changes, and there's no way to keep the
old methods beside the new. Rust has no overloading, so `count("n")`
and `count()` can't both be there, nor `sum("a", "n")` and `sum("a")`.
A deprecation would need new names, and the new names are the point.

The change is mechanical:

| Before | Now |
|---|---|
| `count("n")` | `count().named("n")` |
| `sum("a", "n")` (and `min`, `max`, `avg`) | `sum("a").named("n")` |
| `count("n").filter(c)` | `count().filter(c).named("n")` |

Files, exports and query results don't change.

## 134.4 Rejected
- **`count_as("n")`, `sum_as("tries", "total")`:** a rename, with
  `as` saying which string names. It still takes two strings in a row
  and keeps `filter` a call on the aggregate, with its refusal at run
  time.
- **A default name**, `count` or the path read: two counts in one
  aggregate would share it, and a column used by `having` or the sort
  needs a name anyway. SQL's default names differ by database.
- **`named` optional, checked when the aggregate runs:** a mistake the
  type can catch at compile time.
- **The name first, `.column("n", Column::count())`:** MongoDB's order
  again, and a second type for the reader to know.
- **Keeping §107's form**, documented: the comments were there, and
  still it was read the other way.
- **`named` anywhere in the chain**, the condition after it too: the
  `UnnamedColumn` would have to stay the type after `named`, so a
  column without one would compile again.

## 134.5 Tests
- **Compile-fail doctests** on `UnnamedColumn`, each pinned to
  `E0599`, no such method: a column without a name before the next
  column, and `filter` on an `Aggregate`. An unrelated error, a missing
  import, wouldn't pass for them.
- **Every aggregate test, rewritten** (`aggregate.rs`, `collection.rs`,
  `tests/hints.rs`, `tests/alternatives.rs`,
  `tests/equal_whole_values.rs`), passing unchanged otherwise. Among
  them §126's: a column with two conditions ANDed, conditions on every
  kind of column, from documents and from entries.
- **§126's test of two conditions ANDed was too weak**, before this
  section too: `status = "open"` and `hours > 5` picked the same
  document, so a second `filter` that replaced the first counted the
  same. It is `hours < 5` now, which counts 3 alone where both count 1.
- The test of §126.2's refusal of a `filter` before any column is
  gone, along with the refusal itself. The compile-fail doctest holds
  it.
- The aggregate generators of the `queries` fuzz target and of
  `collection.rs`'s randomized tests build their columns through
  `UnnamedColumn`, drawing from the program in the same order as
  before, so a seed gives the program it gave.
- The bench (`bench/src/bin/aggregate.rs`) and GUIDE's examples are
  written the new way.

## 134.6 Mutations
| Mutation | Caught by |
|---|---|
| `filter` replacing an earlier condition instead of ANDing | §126's two conditions on one column, made stronger (134.5) |
| `filter` keeping the earlier condition only | the same |
| `named` dropping the column's condition | §126's tests |
| `named` keeping the path of another kind, or no path | every `sum`, `min`, `max`, `avg` test |
| `min` building a `max` column | the `min` and `max` tests |
| `named` adding the column twice | "two columns of one name" |

## 134.7 Limits
- An `UnnamedColumn` can be kept in a variable, filtered in a loop and
  named later, as the generators do. Dropped without `named`, the
  aggregate it holds is gone too: `#[must_use]` warns of it.
- The second pass over the whole public API, ROADMAP.md's, is apart
  from this.
