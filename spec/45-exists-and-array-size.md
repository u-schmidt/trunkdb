# 45. `Exists` and array size (`query.rs`)

Real and tested: two new kinds of condition that look at a field's shape
rather than its value. One asks whether the field is there at all, and
the other asks how long the array in it is. Neither changes the file
format.

```rust
// Written before `team` existed, or with `None` skipped by serde:
users.find(Filter::new().missing("team"))?;
users.find(Filter::new().exists("nick"))?;          // a stored null counts
posts.find(Filter::new().size("tags", Op::Eq, 0))?; // no tags
posts.find(Filter::new().size("tags", Op::Gte, 3))?;
```

## 45.1 `Exists`
`Condition::Exists { field }` holds if the field is there, even when it
holds null. `Condition::missing(field)` is `!Condition::exists(field)`,
and `Filter` has `exists` and `missing` as builders. This separates the
two states that §32.1 folds into one for comparisons:

| document | `is_null` | `exists` | `missing` |
|---|---|---|---|
| `{"nick": null}` | true | true | false |
| `{}` | true | false | true |
| `{"nick": "Bob"}` | false | true | false |

Paths work as in comparisons, with a missing field giving nothing
instead of a null (`present_at`, which shares its walk with
`values_at`):
- `address.city` exists if both steps are there. A step into a string
  or a null finds nothing.
- `items[*].sku` exists if some element has `sku`.
- `tags[*]` exists if `tags` is an array with at least one element.
  `[null]` counts; `[]`, a scalar and a missing `tags` don't.

**Why, if §32.1 decided against telling them apart:** that decision
still holds for comparisons. `== null` finds both, so the typed path and
the queries agree. `Exists` is a separate question for the cases where
the difference matters: documents written before a field was added, and
fields a struct skips with `#[serde(skip_serializing_if =
"Option::is_none")]`.

**A catch on the typed path:** serde writes `None` as null. An `Option`
field without `skip_serializing_if` is therefore always there, and
`missing` finds only documents written before the field existed. The
typed test in §45.5 shows both cases.

Rejected: **`Op::Exists`**, as MongoDB spells it with `{$exists: true}`.
An `Op` compares a field with a value, and `Exists` has no value; its
`bool` would duplicate what `Not` already does. So it's a variant of
`Condition` without a value.

## 45.2 Array size
`Condition::Size { field, op, size }` holds if the field is an array
whose length `op`-compares true against `size`: `Eq` 0 for an empty
array, `Gte` 3 for three or more. Every comparison operator works;
MongoDB's `$size` has only equality.

Without an array there's no length, so only `Ne` holds. `Ne` is `Eq`
negated everywhere (§42.1), and it stays that way here: `size("tags", Ne,
0)` holds for a missing `tags`, as `tags != 5` does. The typed test uses
exactly this: a document written before `tags` existed is "not empty".

Through `[*]`, any element's array counts, as any element's value does
for a comparison (§42.1). `size("rows[*]", Eq, 0)` holds if some row is
an empty array. `compare_matches` and `size_matches` share one helper,
`any_matches`, so `Ne` means "none equal" for both.

Only arrays have a size here, not strings or objects. A string's length
would be a separate condition, and it would have to decide between
bytes and characters.

Rejected:
- **A path suffix** (`"tags.$size"`), which would clash with a field
  that has that name.
- **Only `Eq`**, as in MongoDB: "at least one" and "more than two" are
  the questions actually asked, and supporting them costs nothing.

## 45.3 The planner
No index holds either answer. A missing field is indexed as null (§32.2)
and a sparse index leaves out both null and missing (§44.1), so no index
can tell "there" from "not there". Array lengths aren't indexed at all.
So both conditions:
- **alone** scan;
- **next to an indexed comparison** in the same AND, filter what the
  index finds (the comparison bounds, as before);
- **in an OR** make the OR scan, since a branch that can't be bounded
  makes the union unbounded (§36.3);
- **don't rule out null** for a sparse index (§44.3). `exists` holds for
  a stored null, which a sparse index leaves out.

`bounds_for` returns `None` for both, next to `Not`.

## 45.4 `Op` and `Condition` are `#[non_exhaustive]`
Adding `Exists` and `Size` to `Condition` breaks any code outside the
crate that matches on it exhaustively, as `Condition` becoming a tree
did in §36. Both enums are now `#[non_exhaustive]`: a `match` outside
this crate needs a `_` arm, so the next operator or condition, such as
`$elemMatch` or a pattern, won't break anything. It works like
C#'s advice to always write a `default:` case in a `switch` on an enum
from another library, except that the compiler enforces it. Building
the values with `Condition::Compare { .. }` still works; only matching
changes.

This is a breaking change once, for 0.9.0, and it prevents the same
break later.

## 45.5 Tests
- `query.rs`:
  - `Exists` against a stored null, a missing field, a value and a
    non-object, next to `is_null`, which can't tell the first two
    apart; eleven paths, including a string's child, `[*]` on a scalar
    and `[]` against `[null]`.
  - `Size` with every operator against arrays of lengths 0, 1 and 3, a
    string, a null and a missing field, and through `[*]`, where it
    holds for any element's array.
  - The builders make the trees they name.
  - Neither condition bounds an index, alone, next to an index, or in an
    OR, and `exists` doesn't make a sparse index usable for a sort.
- `collection.rs`:
  - Typed profiles, with `nick` skipped when `None` and `team` written
    as null, plus an untyped document from "before" `team` and `tags`
    existed. It covers `missing`, `exists` and `is_null`, and the sizes
    of an empty list, a full one and none at all. An index lookup plus
    `missing` reads 1 document.
  - The randomized compound test's filters now include `exists`,
    `missing` and `size` on its fields, so they run beside every index
    plan, sparse and not, and must still find what a scan finds.
- Checked by breaking it on purpose, nine ways. Each of these fails a
  test:
  - `Exists` reading a missing field as null;
  - `present_at` walking as `values_at` does;
  - a step into a non-object giving a null;
  - `missing` built as `exists`, in `Condition` or in `Filter`;
  - `Ne` over several values as "any not equal";
  - lengths counted one too many;
  - a non-array counted as length 0;
  - `size` through `[*]` never holding.

## 45.6 Limits
- **Neither uses an index.** A sparse index could answer `exists` if it
  kept stored nulls and left out only missing fields. That's the
  MongoDB variant §44.1 rejected; it can come back as a third index
  option if the need shows up.
- **No length of a string or an object.**
- **Per-element conditions** (`$elemMatch`) came next, in §46.
