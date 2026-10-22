# 115. A list of values: `in` (`query.rs`, `query_text.rs`)

"The status is open or blocked" was an OR of two `eq`s on one field,
written out:

```rust
Filter::new().any_of([Condition::eq("status", "open"), Condition::eq("status", "blocked")])
```

It is most of what an OR is used for, and a form of conditions — a list
of rows, all to hold — can't say it at all. Now it has its own words,
SQL's `IN`:

```rust
let waiting = Filter::new().is_in("status", ["open", "blocked"]);
let waiting: Filter = r#"status in ("open", "blocked")"#.parse()?;
let others: Filter = r#"status not in ("open", "blocked")"#.parse()?;
```

## 115.1 What it is
`Condition::is_in(field, values)` and `Filter::is_in` make **the OR of
an `eq` per value**, nothing new: `Condition::Any` of `Compare`s. So
everything an OR of `eq`s does, it does, unchanged:
- the planner reads **one point of an index per value**, a union of
  them (§36.3); on `_id`, each value a lookup by id (§61); `explain`
  shows the `eq`s;
- a value against an array path (`tags[*]`), a sparse index, null
  (`in (1, null)` holds where the field is null or missing, as `eq`
  with null does, §32), numbers (`5` and `5.0` equal): all as for `eq`;
- **no values: nothing matches**, as an OR of none (§36).

`!Condition::is_in(..)` is `NOT IN`: two-valued, as every NOT (§32),
so it holds where the field is missing, unless null is among the
values. SQL's `NOT IN` is unknown, so
false, when a value is null; here `a not in (1, null)` holds where `a`
is neither 1 nor null nor missing.

`is_in`, because `in` is a Rust keyword; `is_null` is its neighbor.

## 115.2 As text
`field in (value, …)` and `field not in (value, …)`, after a path as
the comparisons are, any values of §111.4, `()` for none. `in` isn't
reserved: a field named `in` is compared as `in = 1`, listed as
`` `in` in (1) ``.

Printed, every OR of two or more `eq`s of one field is `in`, however
it was written, and its NOT `not in`: `a = 1 or a = 2` prints as `a
in (1, 2)`. An `in` of one value prints as the `eq` it is, `a = 1`;
of none, as `false`. Read again, each prints the same (§111.6).

## 115.3 Rejected
- **A condition of its own** (`Condition::In`): the planner, sparse
  and multikey indexes, covering reads (§100), `explain` and the fuzz
  model would each need it taught, for what they all know already as
  an OR.
- **`any_of` of `eq`s left as the way**: it repeats the field per
  value, and a tool with a form of conditions has to build the OR
  itself.
- **`Filter::is_in` taking `&[V]`**: an iterator takes arrays, `Vec`s
  and iterators alike, as `select` does.

## 115.4 Tests
- `collection.rs`: `is_in` on an indexed field with three values, one
  of them stored nowhere: the plan a union of three points, 200
  documents read for 200 matches, 200 keys examined; `NOT IN` a scan
  finding the rest; on `_id`, by id, sorted; no values, nothing.
- `query_text.rs`: `in` and `not in` read as the builders make them;
  `IN` in any case; `()`; a field named `in`; printing an OR of `eq`s
  as `in`, its NOT as `not in`, inside an AND, beside another
  condition; an OR of two fields, or of `=` and `>`, left as it is; one
  value as `=`, none as `false`; arrays, objects and ids as values; the
  mistakes. The 3,000 random filters (§111.9) now include `in` lists of
  0 to 3 values of every type.
- The `queries` fuzz target (§105) makes `in` of 0 to 3 values on
  plain, nested, array and `_id` paths: 400 random programs pass. The
  `filter_text` target ran 5.3 million inputs for 2 minutes, `in`
  among its seeds, without a failure.

The random filters found one thing: a NOT of an AND of one `in`
printed as `not a in (…)`, and read back as `a not in (…)`. A NOT now
looks through ANDs and ORs of one condition, which print as it does.

**Fixed later**, found by the `queries` target (seed 123123, after
0.23.0): an OR whose `eq` branches came wrapped alone in an AND or OR,
`Any([All([a = "s0"]), a = 0])`, printed `a = "s0" or a = 0`, since the
printer drops such wrappers, but wasn't recognized as a list. Its text
read back as `in`, and printed so: the text didn't read back the same.
The list now looks at its branches, and at itself, past such wrappers,
as the NOT already did. The values' two types had nothing to do with
it. Tested in `a_list_of_values_reads_as_an_or_of_eqs_and_prints_as_in`
(the OR, its NOT, and both wrapped once more, each read back to the
same text); the check removed, it fails.

## 115.5 Mutations
Each caught: `is_in` as an AND (2 tests), of `ne`s (2); `Filter::is_in`
negated (2); `not in` read as `in` (2); a list keeping its first value
only (2); `()` refused (1); `in` printed for one value (1), for two
fields (2), for other comparisons (1); `not in` never printed (1); a
NOT looking through no AND of one (1), or through one only (1).
