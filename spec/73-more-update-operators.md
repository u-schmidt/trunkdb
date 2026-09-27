# 73. More update operators (`update.rs`)

`Update` had `set`, `unset` and `inc` (§68), and left the rest for
later, each a small addition since `Update` is built by methods. Six
more come now, the ones MongoDB apps reach for next: moving a field,
keeping a bound, and changing an array without replacing it.

```rust
use trunkdb::{DateTime, query::{Filter, Update}};

devices.update_fields(
    Filter::new().id(id),
    &Update::new()
        .add_to_set("tags", "online")
        .pull("tags", "offline")
        .max("last_seen", DateTime::now())
        .min("first_seen", DateTime::now()),
)?;
```

They go where `set` goes: `update_fields`, typed and untyped, one
batch, all or nothing, applied in the order they were added (§68.2).

## 73.1 `rename(from, to)`
The field at `from` moves to `to`: `unset(from)`, then `set(to, <its
value>)`. So it lands at the end of its new object, replaces whatever
`to` held, and objects missing on the way to `to` are made; anything
else on the way fails, as for `set`. Nothing at `from` is no change,
and `to` is left alone, as MongoDB does.

Refused as written, besides the paths `set` refuses: `from` equal to
`to`, and one inside the other (`a` to `a.b`, `a.b` to `a`), which
would move a field into what it's being taken out of.

## 73.2 `min(path, value)` and `max(path, value)`
`value` replaces what's at `path` if it's less (`min`) or greater
(`max`), or if the field is missing or null; equal is no change, so an
`Int` 5 isn't swapped for a `Float` 5.0. "The latest time seen" is
`max("last_seen", now)`, "the best score" `max("best", score)`.

- **Compared as filters compare** (§34.1): numbers by exact value,
  `Int` against `Float` too; strings, bools, ids and date-times among
  their own kind.
- **Something there that doesn't compare** with `value`, a string
  against a number, an array, a NaN, fails the update with
  `Error::Update`. MongoDB instead orders every kind against every
  other (numbers before strings), so `$min: 5` on a string field sets
  5; that's a typo's result more often than an intent.
- **Null is like missing**: an optional field (`Option<DateTime>`,
  stored as null when `None`) takes the first value it gets. MongoDB
  orders null lowest, so `$max` replaces it and `$min` never does;
  here both do, as "no value yet" is what a null there means.
- **`value`** must be one of the kinds that compare, and not NaN:
  refused as written otherwise, since it could never change a field
  that has a value.

## 73.3 `push`, `add_to_set` and `pull`
- **`push(path, value)`** adds `value` at the end of the array. Several
  values: several `push`es, applied in order.
- **`add_to_set(path, value)`** adds it unless an equal element is
  there already: tags, members, anything kept as a set. It doesn't
  remove duplicates already there.
- **`pull(path, value)`** removes every element equal to `value`; the
  others keep their order.
- **A missing field:** `push` and `add_to_set` make it `[value]`;
  `pull` leaves it missing. **Anything but an array** there, null
  included, fails the update, as MongoDB does; a null isn't an empty
  array.
- **Equal** means what a filter's `eq` means for single values (§32,
  §34.1): `1` and `1.0` are equal, a string and a number never, NaN
  equals nothing, not even NaN. Arrays, objects and binary, which no
  filter compares, are equal by their contents: arrays element by
  element in order, objects field by field in any order, as
  `Document`'s `==` sees them.

A multikey index on the array (`tags[*]`, §42) follows, as every index
follows every update.

Rejected:
- **`pull` by condition** (MongoDB's `$pull: {score: {$lt: 5}}`): it
  needs conditions on an element, as `elem_match` has (§46), in an
  update. Possible later; removing by value is the common case.
- **`push` of several values at once** (`$each`), positions, sorting
  and slicing: several `push`es say the first; the rest is rarely used.
- **`add_to_set` by exact type** (MongoDB tells `1` from `1.0`): a
  filter's `eq` doesn't, and an `add_to_set` then `eq` should agree.
- **`mul` and `pop`** now: nobody has asked; each is a small addition.

## 73.4 Tests
- `update.rs`: `min` and `max` on integers, floats and a mix, strings
  and date-times, on a missing and a null field, equal values left
  alone, and failing against a string, a bool, an array or a NaN;
  `rename` to a new field, over an existing one, between nested
  objects, of a missing field, and through a number; `push`,
  `add_to_set` and `pull` on arrays, missing fields and nested paths,
  `pull` of objects in another field order, arrays, binary, `1` and
  `1.0` together, and NaN, and all three failing on null, a string or
  an object; refused as written: `min` and `max` of null, NaN, an array
  or binary, a rename to itself or into or out of itself, and paths
  into `_id` or with brackets.
- `collection.rs`: typed devices, with tags kept as a set through a
  multikey index, the latest time seen through an index on it, a best
  score, and a rename into a field the struct has; a `push` onto a
  string failing the batch. And the random updates of §68.4 now draw
  from all nine operators, with a multikey index on `t[*]`, which must
  agree with a scan afterwards.
- Checked by breaking it on purpose, seventeen ways. Each of these
  fails a test:
  - a rename's `to` not checked; `min` and `max` not taking null as
    missing; `min` keeping the greater; replacing on any difference;
    no error for values that don't compare;
  - a rename copying instead of moving; renames to itself or out of
    itself allowed; NaN allowed as a bound;
  - `push` not starting a missing field; `add_to_set` adding
    duplicates; `pull` comparing as filters only, so no objects or
    arrays; `pull` of a non-array silently ignored;
  - arrays or objects of different lengths equal; binary compared as
    filters compare it; a removed field reordering the rest.
