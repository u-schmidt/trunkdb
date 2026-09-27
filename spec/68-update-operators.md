# 68. Update operators (`update.rs`, `collection.rs`)

`update_many` changes what a filter finds with a closure (§38), which
§38.1 chose over MongoDB-style operators and left those for later. They
come now, next to it, for three things a closure doesn't do well: an
untyped collection, where a closure has to dig through `Document`s by
hand; a change that's data rather than code, built at run time; and
saying what changes, not how.

```rust
use trunkdb::query::{Filter, Update};

let started = jobs.update_fields(
    Filter::new().eq("status", "Queued"),
    &Update::new().set("status", "Running").inc("tries", 1).unset("error"),
)?;   // -> how many changed
```

## 68.1 The operators
`Update` is a list of changes, built like a `Filter`, applied in the
order they were added, each to what the one before left. Paths are
dotted, as everywhere (§31); values convert as in `Filter`'s builder.

- **`set(path, value)`**: the field becomes `value`, replaced where it
  is or added at the end. Objects missing on the way are created, as
  MongoDB does; anything else on the way, a number, a string, null, an
  array, fails the update.
- **`unset(path)`**: the field is removed, the others keep their order.
  Nothing there, or no object on the way, is no change.
- **`inc(path, by)`**, `by` an integer or a float: two integers stay an
  integer and fail on overflow instead of wrapping; anything with a
  float is a float. A missing field becomes `by`, as MongoDB does; a
  null, a string, anything but a number fails.

Refused before anything is read, as an `InvalidInput` error, like an
index on a bad path (§31): a path with an empty step; brackets, `[*]`
included, since an update names one field and a `[*]` would name any
number (§42); a path into `_id`, which no update changes (§18); an
`inc` by something that isn't a number.

## 68.2 `update_fields`
`Collection::update_fields(filter, &update)`, on both paths, is
`update_many` (§38.2) with the operators in place of the closure: it
changes exactly what `find(filter)` would return, `sort` and `limit`
included; in one batch, all or nothing; unchanged matches, judged by
their encoding, aren't written or counted; the indexes follow.

- **An operator that can't be made** to one of the documents fails the
  whole batch with a new error, `Error::Update { collection, id,
  message }`: which document, and what went wrong at which path.
  Nothing lands, including the documents changed before it.
- **On the typed path**, each changed document must still convert to
  `T`, or the batch fails with the conversion's error: a field set to a
  value of the wrong type is caught by the update, not by the next
  read. That costs a conversion per changed document, which the
  untyped path doesn't pay.
- **The update is taken by reference**, so one `Update` can be applied
  again, to another filter or collection.

Rejected:
- **One `update_many` for both, generic over a trait closures and
  `Update` both implement:** Rust infers a closure's argument types
  only from an `Fn` bound it's passed to directly. Behind a trait of
  our own, every `|task| ...` would need `|task: &mut Task|`.
- **Refusing two changes to the same or overlapping paths**, as
  MongoDB does: applied in order, their meaning is clear, and the order
  is what the caller wrote.
- **More operators now** (`push`, `pull`, `min`, `max`, `rename`,
  `mul`): the three the roadmap named cover the common changes, and
  each more is a small addition later, `Update` being built by methods.
- **Skipping the conversion check on the typed path:** a typo in a
  field name still slips through, since serde ignores unknown fields,
  but a value of the wrong type would otherwise only fail the next
  read of that document, far from the update that caused it.

## 68.3 Limits
- **No operators in a `Batch`** or in `upsert`: `update_fields` is its
  own batch, like `update_many`.
- **A misspelled path adds a field** instead of failing, as in
  MongoDB. On the typed path the new field is ignored by serde, so the
  update seems to do nothing.
- **Arrays can't be changed element by element**: `set` replaces the
  whole array.
- **Every match is read and decoded**, as for `update_many`; the
  operators save the typed path's closure, not the reads.

## 68.4 Tests
- `update.rs`: `set` replacing, adding, and making the objects on its
  way, and failing through a number, null, an array, a string or a
  document that isn't an object; `unset` removing a field, keeping the
  others' order, and ignoring a missing field or path; `inc` on
  integers and floats and a mix, starting a missing field, failing on
  overflow and on null, a string, a bool, an array; changes applied in
  order; the paths and increments refused before anything is read.
- `collection.rs`:
  - typed tasks: the first five by rank changed, sort and limit
    counted, the index on `tries` following; unchanged matches not
    counted; a string set into an `i64` field failing the batch; an
    `inc` of null failing on the sixth match with that document's id,
    after five others changed, and none of it landing; `_id` and `[*]`
    refused;
  - 40 rounds of random inserts, filters and updates (`set`, `unset`,
    `inc` on `v`, `w` and `o.x`, the last creating an object), some of
    which fail on a match: the count and every document must match a
    model, a failed update must leave every document as it was, and
    afterwards the index on each path must agree with a scan.
- Checked by breaking it on purpose, twelve ways. Each of these fails
  a test:
  - changes applied in reverse; `unset` reordering the fields; no
    objects made on the way; `inc` of a missing field starting at 0; an
    overflow wrapping; an integer plus a float dropping the float;
  - only `[*]` refused, not other brackets; `_id.x` allowed; `inc` by a
    string allowed; nothing checked up front;
  - the typed result not converted; the error naming no document.
