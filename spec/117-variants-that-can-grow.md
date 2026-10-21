# 117. Variants that can grow (`query.rs`)

`Condition` and `QueryPlan` were `#[non_exhaustive]` (§45.4, §60): a new
kind of condition or plan breaks no program, which has a `_` arm for it
already. Their variants weren't. Outside trunkdb, anyone could build
`Condition::Compare { field, op, value }` by hand, or match all of its
fields, `Condition::Compare { field, op, value } => …`. Either breaks
the day a variant gets a field — a path parsed ahead in a `Compare`, a
direction in an `IndexOrder` — so after 1.0 no variant could get one
(ROADMAP.md, "Before 1.0").

Now each variant with named fields is `#[non_exhaustive]` too:
- `Condition`: `Compare`, `Exists`, `Size`, `ElemMatch`;
- `QueryPlan`: `Index`, `IndexOrder`, `IndexUnion`.

Outside trunkdb, such a variant can't be built with its fields, and a
match on it ends in `..`:

```rust
match condition {
    Condition::Compare { field, op, value, .. } => …,
    Condition::All(all) => …,
    _ => …,
}
```

## 117.1 Which, and which not
- **Variants with named fields:** these may grow, so they're marked.
- **`All`, `Any`, `Not`:** a list of conditions, or one in a box. There
  is nothing to add to them that wouldn't be a new variant. Marked, they
  could only be matched as `Condition::All { 0: all, .. }`, which no one
  should have to write. Left as they are.
- **`Scan`, `ById`:** no fields to add. Left as they are.
- **`Error`'s variants with fields:** marked since §60.
- **The reports** (`Explained`, `IndexRead`, `ExecutionStats`, …):
  structs, marked since §94.

## 117.2 What it costs
- **Building a condition goes through the methods,** which exist for
  every variant: `Condition::compare`, `eq` and the like, `exists`,
  `size`, `elem_match`.
- **A plan can no longer be built to compare with `==`.** A test
  writes `matches!(plan, QueryPlan::Index { field, .. } if field ==
  "ts")` where it wrote `assert_eq!(plan, QueryPlan::Index { field:
  "ts".into() })`. One test in this repository did, and so did the
  example in `explain_run`'s documentation; the `trunkdb` command
  matched three plans without `..`.
- **Inside trunkdb nothing changes:** `#[non_exhaustive]` binds only
  other crates.

It breaks code that built or matched such a variant, in 0.x, where
that's allowed.

## 117.3 Rejected
- **Constructors for plans** (`QueryPlan::index("ts")`), to keep
  `assert_eq!` working: a constructor is a promise about the fields as
  much as the variant was.
- **`Condition` opaque**, a struct with methods to ask what it is: it
  would take away a match that tools, like a form that shows a filter as
  rows, need.
- **All variants marked**, for uniformity: §117.1.

## 117.4 Tests
Two compile-fail tests in `tests/ui` (run by `tests/document_macro.rs`
with the `derive` feature, on stable and on the MSRV in CI):
- `variant_built_by_hand`: a `Condition::Compare` built with its fields
  doesn't compile.
- `variant_matched_without_rest`: each of the seven variants matched
  without `..`: seven errors, one each.

Written on one line each: the gutter of a span over several lines
differs between Rust 1.89 and stable, and trybuild compares the
messages exactly.

## 117.5 Mutations
The mark taken off each of the seven variants in turn: each caught by
the second test, `Compare` by both.
