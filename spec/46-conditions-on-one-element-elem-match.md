# 46. Conditions on one element: `elem_match` (`query.rs`)

Real and tested: a condition that one element of an array must meet as
a whole. It is MongoDB's `$elemMatch`, with the multikey indexes of §42
bounding it.

```rust
// An order with a line of 9 or more of A1 — not an A1 line and some
// other line of 9:
orders.find(Filter::new().elem_match(
    "lines",
    Condition::eq("sku", "A1") & Condition::gte("qty", 9),
))?;
// A score in the 80s — not one above 80 and another below 90:
students.find(Filter::new().elem_match(
    "scores",
    Condition::gte("", 80) & Condition::lt("", 90),
))?;
```

## 46.1 What it means
`Condition::ElemMatch { field, condition }` holds if some element of
the array at `field` meets `condition`, evaluated with the element as
its document. The difference from §42's `[*]` paths is that each
comparison on `lines[*].sku` or `lines[*].qty` may be met by a
different line (§42.1). `elem_match` needs one line that meets all of
them.

- **Paths inside start at the element.** On `lines`, `sku` is a line's
  `sku`, and `address.city` is its `address.city`.
- **`""` is the element itself**, for arrays of scalars: `gte("", 80)`
  on a score. A path starting with `[*]` starts there too, so
  `eq("[*]", 2)` on `rows` means "a row containing 2" when the rows
  are arrays. `field_value` and `values_at` read the empty path as the
  document, so any condition works on an element: comparisons,
  `exists`, `size`, a nested `elem_match`, `Not`, `Any`.
- **`field` is an ordinary path.** `orders[*].lines` reaches every line
  of every order, the same as `elem_match("orders", elem_match("lines",
  …))`.
- **No array, no element:** a missing field, a scalar or a null never
  matches, and `!elem_match(…)` always does. Negations inside are
  about the one element: `elem_match("lines", ne("sku", "A1"))` means
  "a line that isn't A1", where `ne("lines[*].sku", "A1")` means "no
  line is A1" (§42.1).

Rejected:
- **An operator form for scalar elements**, like MongoDB's
  `{scores: {$elemMatch: {$gte: 80, $lt: 90}}}`, which puts operators
  where fields would be. The empty path does the same with the
  conditions that exist, so there's nothing new to learn.
- **`$` as the element** (`gte("$", 80)`). A field may be named `$`,
  but not `""` (§31.4 refuses empty path parts for indexes; a key `""`
  was reachable in a filter before, and now isn't).

## 46.2 API
- `Condition::elem_match(field, condition)` and
  `Filter::elem_match(field, condition)`.
- `Condition` gained the variant `ElemMatch`. It is `#[non_exhaustive]`
  since §45.4, so that breaks nothing.

## 46.3 The planner
If an element meets `sku == "A1"`, the document meets `lines[*].sku ==
"A1"`. So an `ElemMatch` is bounded like its condition with every path
moved out to the elements (`within`): `sku` becomes `lines[*].sku`,
`""` becomes `lines[*]`, and `[*]` becomes `lines[*][*]`. An index on
`lines[*].sku` then reads the orders with an A1 line, and checking each
one picks those whose A1 line has the quantity.

The rewritten condition is used only for bounds, never for matching,
because it doesn't mean the same. `ne("sku", "A1")` becomes "no line is
A1", and a `Not` becomes the negation of an any-element test. Neither
ever bounds an index (§36.3), and everything that does bound one (an
`Eq`, a range, an OR whose every branch is bounded) holds for the
document whenever it holds for one element. So the bounds hold
everything that matches, as bounds must (§28.3).

Everything else is as for `[*]` paths (§42.3):
- Two ranges on the same element path aren't intersected, even inside
  an `ElemMatch`. `gte("", 80) & lt("", 90)` reads one of the two
  ranges, which is still correct because the check sorts it out.
- A multikey index is never read in sort order.

Measured by the typed test: 2,000 orders of one to three lines each, 20
skus. The `elem_match` for a line of A1 with a quantity of 9 or more
reads 195 orders, the ones with an A1 line, and 38 of them match. The
two comparisons on `lines[*]` paths would find 78 instead: every order
with an A1 line and some line of 9.

## 46.4 Tests
- `query.rs`:
  - The semantics, against one order-like document: an A1 line and a
    large line that aren't the same line; scores between bounds with
    `""`; arrays of arrays with `[*]` and `size`; nested and through a
    `[*]` path; a missing field, strings and numbers as "arrays", and
    `Not`, `Ne`, empty `All` and `Any` inside.
  - The planner: the rewritten paths for a field, for `""` and for
    `[*]`, with the range read; an OR inside bounded by union; none
    from a `Ne`, a `Not`, an unindexed field, an OR with an unbounded
    branch, or an index on another path.
- `collection.rs`:
  - The multikey randomized test now also runs 300 random `elem_match`
    filters, before and after a reopen. They mix `items` with
    conditions on `n` and `tags` with conditions on the element itself,
    in ANDs, ORs and NOTs of every comparison, with null and array
    values and sometimes an indexed comparison beside them. Each must
    find what a scan finds, and both index plans and a scan must run.
  - The typed orders test above: the same orders as a scan, the reads
    equal to the orders with an A1 line, fewer than the loose version
    finds.
- Checked by breaking it on purpose, eight ways. Each of these fails a
  test:
  - every element having to meet it;
  - a scalar taken as an array of itself;
  - an `ElemMatch` never bounding an index;
  - bounding by the inner condition with its paths unchanged;
  - `""` rewritten as `lines[*].`;
  - an OR inside rewritten as an AND;
  - a path's first empty step read as a field `""`, in `values_at`, or
    in `field_value`.

## 46.5 Limits
- **An element condition can't use a compound index.** Compound indexes
  have no `[*]` fields (§43.1), so "sku and qty of one line" is always
  bounded by one field and checked for the rest.
- **No positional result.** MongoDB can return only the matching
  element (`$`); here a query returns whole documents.
