# 38. `update_many` (`collection.rs`)

Real and tested: the documents a filter finds can be changed in one
call, by a closure.

```rust
tasks.update_many(Filter::new().eq("status", "Queued"), |task| {
    task.status = "Running".to_string();
})?;   // -> how many changed

// Untyped: the closure gets the Document.
docs.update_many(Filter::new().lt("seen", 0), |doc| { /* ... */ })?;
```

## 38.1 A closure, not update operators
The change is a closure: `FnMut(&mut T)` on the typed path, `FnMut(&mut
Document)` on the untyped one. It's LiteDB's `UpdateMany(x => ...,
predicate)`, and in Rust it's the obvious shape — the compiler checks
the field names, and any change the language can express works.

Rejected for now: MongoDB-style operators (`$set`, `$inc`, `$unset`, on
dotted paths). They'd work without deserializing and read declaratively,
but they are a small language to design, and for a typed caller strictly
less than a closure. They can come later, on the untyped path, if a need
shows up.

## 38.2 What it changes
Like `delete_many` (§37.1): exactly what `find(filter)` would return —
sort and limit included — through the same code and plans. `change` is
called on each match in `find`'s order, so with a sort it sees them
sorted. Everything happens in one batch under one write lock: all
changes land or none. A unique index refusing one change (§33) rolls
back every one; so does a match that doesn't convert to `T`.

It returns how many documents changed — matches `change` left as they
were aren't written or counted. "Unchanged" is decided by the encoded
bytes, not `==`: a NaN isn't equal to itself, so a document holding one
never compared equal — the randomized test found exactly that. On the
typed path it's decided after the round trip through `T`, so a document
written before a field was added to `T` counts as changed: it gets the
field. An `_id` can't change: whatever `change` puts there, the document
keeps its own.

`change` runs while the database is locked for writing, so it must not
use this database — that would deadlock. The same rule as for `export`'s
writer (§30.4).

## 38.3 Tests
- `update_many_changes_what_find_would_return`: typed tasks — the five
  lowest-ranked of a status by sort and limit, seen by `change` in that
  order; the index follows; unchanged matches not counted; a unique
  index refusing one change rolls back all; `_id` unchangeable
  (untyped); a match that doesn't convert to `T` fails the batch.
- `update_many_matches_a_model_through_random_filters`: 30 rounds of
  inserts and a random nested filter, sometimes sorted and limited; the
  change gives `v` a random new value or leaves the document alone. The
  count and every document must match a model; at the end the indexes
  agree with a scan on every plan. It caught the NaN case above.
- Checked by breaking it on purpose, four ways: no "unchanged" check,
  the limit ignored, matches in reverse order, and comparing without the
  `_id` put back. Each fails a test.

## 38.4 Limits
- No update operators (§38.1).
- One big batch, like `delete_many` (§37.4).
