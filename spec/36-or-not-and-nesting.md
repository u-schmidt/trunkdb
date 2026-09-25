# 36. OR, NOT and nesting (`query.rs`, `collection.rs`)

Real and tested: conditions can be combined with OR, AND and NOT, nested
as deep as needed, and an OR whose branches can use indexes reads just
those index ranges.

```rust
runs.find(
    Filter::new()
        .any_of([Condition::eq("status", "Queued"), Condition::eq("status", "Running")])
        .and(!Condition::lt("seen", 30))
        .sort_desc("started_at"),
)?;   // explain: IndexUnion { fields: ["status", "status"] }

// The same with operators — `&` binds tighter than `|`, as in Rust:
let c = (Condition::eq("status", "Queued") | Condition::eq("status", "Running"))
    & !Condition::lt("seen", 30);
```

## 36.1 `Condition` is a tree
```rust
pub enum Condition {
    Compare { field: String, op: Op, value: Document },
    All(Vec<Condition>),     // AND — true if empty
    Any(Vec<Condition>),     // OR — false if empty
    Not(Box<Condition>),
}
```
`Filter.conditions` stays a list that must all hold — the top level is
an AND, as before — but each entry can now be a group. What was the
struct `Condition { field, op, value }` is the variant
`Condition::Compare { field, op, value }`: a breaking change for code
that built conditions by hand, which the builder (§35) mostly replaced.

Rejected: a separate expression type next to a flat `Condition` struct
(`Filter.conditions: Vec<Expr>`, `Expr::Is(Condition)`). It keeps the
old struct but adds a wrapper at every use, and two names for "a
condition". Rejected: SQL's three-valued logic. `NOT` is plain negation
of "matches": `!(x == 5)` is true where `x` is missing, like `x != 5`
(§32). A missing field is never "unknown" here, only null.

## 36.2 Building
`Condition::eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `contains`, `is_null`,
`is_not_null`, `compare`, and `Condition::any([...])`/`all([...])` for
groups built from lists. `Filter::and(condition)` adds any condition,
`Filter::any_of([...])` an OR.

The operators `|`, `&` and `!` build the same trees. A chain flattens —
`a | b | c` is one OR of three — and Rust's precedence applies (`&`
before `|`). Rejected: a method `Condition::not(c)`: clippy flags it as
confusable with `std::ops::Not::not`, and implementing the trait is the
idiomatic way anyway; `!c` is what a C# or Rust reader expects. Rejected:
`.or(...)` on `Filter` itself: in a chain like
`f.eq(a).or(g).eq(b)` the grouping would depend on call order, which
reads ambiguously. `any_of` says exactly what's grouped.

## 36.3 Indexes: bounds and unions
Planning is one recursive function, `bounds_for`: for a condition, index
ranges whose **union** holds every document it can match — or `None` if
no index can bound it.
- A comparison on an indexed field: its range (§28.4), with all the
  comparisons on that field in the same AND intersected.
- An AND (the filter's list, or a nested `All`): the best bounds of any
  one part — every match must satisfy that part anyway.
- An OR: the union of every branch's bounds — but only if **every**
  branch has some. One branch no index can bound can match documents
  outside any range, so then there's no bound, and the other conditions
  or a scan decide.
- A NOT: none. The complement of a range is two ranges, but the rest of
  the NOT's semantics (missing fields, values of other kinds) make it
  more than that; not worth it yet.

Choosing, rule-based like §28.4: an `Eq` beats an OR's union, which
beats a range — "found by value" first. An OR of `Eq`s is how "status is
one of these" is written, so it ranks right after a single `Eq`. The
same rule decides against reading in sort order (§34.2): if the
conditions *other than those on the sort field* can find documents by
value — an `Eq` or a union — they're used instead. That rule no longer
depends on the order conditions were added in.

A union reads each range and keeps each document once — ranges of
different indexes, or overlapping ones of the same, can hold the same
document. `explain` says `IndexUnion { fields }`, one field per range.
An OR of nothing matches nothing and reads no range at all.

## 36.4 Tests
- `nested_filters_find_what_a_scan_finds_on_every_plan`: 400 random
  filters, conditions nested three deep — ANDs, ORs (empty ones
  included), NOTs, every operator, on two indexed fields, an unindexed
  one and a field no document has — some sorted with a limit. `find`,
  `cursor` and `count` must each give exactly what checking every
  document gives, before and after a reopen, and all four plans (scan,
  index, union, order) must run.
- `index_ranges_union_the_branches_of_an_or`: every planning rule —
  unions, ANDs inside branches, nested ORs flattening, an empty OR, an
  unbounded branch or a NOT falling back, `Eq` over union over range in
  any order, and reading in order giving way to a union.
- `an_or_of_indexed_values_reads_just_those`: typed, with the read
  counter (§34.3): `status` in two of three values reads 200 of 300
  documents.
- Matching: OR, AND, NOT, empty groups, nesting, NOT against missing
  fields; the builder's and the operators' trees, precedence included.
- Checked by breaking it on purpose, four ways: an OR skipping a branch
  with no bounds, a NOT using its inner bounds, no de-duplication in a
  union, a union of the first branch only. Each fails a test.

## 36.5 Limits
- A NOT never uses an index (§36.3).
- An AND uses one part's bounds, never the intersection of several
  indexes' ranges.
- Still no pattern language, no conditions on array elements (ROADMAP.md).
