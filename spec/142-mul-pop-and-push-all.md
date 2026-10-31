# 142. `mul`, `pop` and `push_all` (`update.rs`)

§73 left three of MongoDB's update operators for when they were asked
for: `$mul`, `$pop`, and `$push` of several values at once. Each is a
small addition, since `Update` is built by methods, and the roadmap
held them as one line; they come together now. No file format changed.

```rust
use trunkdb::query::{Filter, Update};

workers.update_fields(Filter::new().id(id), &Update::new().push_all("jobs", ["a", "b"]))?;
workers.update_fields(Filter::new().id(id), &Update::new().pop_first("jobs"))?; // take the next
prices.update_fields(Filter::new().eq("sale", true), &Update::new().mul("price", 0.9))?;
```

They go where the others go: `update_fields`, typed and untyped, one
batch, all or nothing, in the order they were added (§68.2), and every
index follows, a multikey one on the array (§42) too.

## 142.1 `mul(path, by)`
The number at `path` multiplied by `by`, as `inc` adds:
- **Two integers stay an integer;** past `i64` the update fails, as for
  `inc`. Anything with a float is a float.
- **A missing field becomes zero,** `0` or `0.0` as `by` is: MongoDB's
  `$mul` does so, and it is what multiplying nothing gives. `inc` makes
  it `by` instead, which is what adding to nothing gives.
- **Anything but a number there** fails the update, null included.
- **`by`** must be a number, refused as written otherwise, as `inc`'s.

## 142.2 `pop_first(path)` and `pop_last(path)`
The first or last element of the array is removed: a queue's next item,
or a list kept to its newest entries with `push` and `pop_first`.
- **Two methods, not a sign:** MongoDB's `$pop` takes `-1` or `1`, which
  says nothing at the call; `pop_first` and `pop_last` say which end.
- **An empty array, or nothing at `path`,** is no change, as for `pull`.
- **Anything but an array there** fails the update, null included, as
  for the other array operators (§73.3).
- **The element isn't handed back:** `update_fields` returns how many
  documents changed. A program that needs it reads the document first,
  or `update_many` with a closure (§38).

## 142.3 `push_all(path, values)`
`push` of every value, in order, MongoDB's `$push` with `$each`. Several
`push`es said the same (§73.3), but a program holding a list wrote a
loop to say it. A missing field becomes an array of the values, `[]`
if there are none; anything but an array there fails the update, even
with no values, as `push` would. One `push` is a `push_all` of one
inside: one change, one way to apply it.

## 142.4 Rejected
- **`$pop` with a number:** 142.2.
- **`add_to_set` of several values** (`$addToSet` with `$each`): not
  asked for; several `add_to_set`s say it.
- **`$push`'s positions, sorting and slicing** (§73.3): still rarely
  used. A list kept to its last ten is `push` then `pop_first` while
  it's longer, which the application knows.
- **`pull` by a condition on the element:** still open (ROADMAP.md).

## 142.5 Tests
- `update.rs`:
  - `mul` of integers, negatives, floats and a mix; a missing field
    `0` or `0.0` as `by` is, nested too; overflow, and its edge not
    failing; null, a string and an array failing; refused as written
    for a string or null;
  - `push_all` in order, duplicates kept, nested, a missing field, no
    values on a missing field and on an array; `pop_first`, `pop_last`,
    both in one update, nested to empty; an empty array and missing
    fields no change; all three failing on null, a string or an object;
    paths refused as for the others.
- `collection.rs`: typed workers, jobs queued with `push_all`, taken
  with `pop_first`, dropped with `pop_last`, through a multikey index;
  popping empty arrays changes no document; a rate multiplied, through
  an index on it; a `pop_first` on a string failing the batch, nothing
  written. The random updates of §68.4 draw from all thirteen ways.
- **The `queries` fuzz target** (§105): its `update_fields` step draws
  `mul`, `push_all`, `pop_first` and `pop_last` too. 1,000 programs
  ran 2,748 of those steps, 2,000 of seed 7 5,529, all as the model
  says. It draws differently, so a seed no longer gives the program it
  gave.

## 142.6 Mutations
| Mutation | Caught by |
|---|---|
| `mul` adding | `mul` |
| `mul` overflow not checked | `mul` |
| A missing field made `by` | `mul` |
| A missing field always `0` | `mul` |
| `mul` by a string allowed | refused as written |
| `push_all` keeping the last value only | `push_all and pop`, workers |
| `push_all` in reverse | `push_all and pop`, workers |
| `pop_first` taking the last | `push_all and pop`, workers |
| `pop_last` taking the first | `push_all and pop`, workers |
| `pop` of a non-array ignored | `push_all and pop`, workers |
