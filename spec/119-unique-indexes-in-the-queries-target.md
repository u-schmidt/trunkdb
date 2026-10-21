# 119. Unique indexes in the `queries` target (`fuzz/src/queries.rs`, `collection.rs`)

The `queries` target (§105) never made a unique index. §105.7 left them
out because the model would have to know when two values are equal —
across `Int` and `Float`, array elements, nulls — and a second notion of
equality, kept beside the database's, would disagree in the corners for
reasons that aren't bugs. So the promise of a unique index went
unfuzzed: that a write which would make a duplicate is refused, and one
that wouldn't isn't, through every kind of write.

Now a third of the indexes a program makes are unique, plain or sparse,
on one field, an array's elements or several fields. The first 400
programs found a bug that had been there since unique indexes could be
updated (§119.1).

## 119.1 Found: an update past where a key is cut
A key holds a string only up to a budget: about 1000 bytes in a key of
one field, less in a key of several (§28.1). Two strings that differ
only past it share a key. A unique index allows for that: `check_unique`
reads every document under the same key and compares the real values
(§33.1).

It was only asked, though, for a key the document didn't have before.
An update that kept a key — "only the document moved", or the field
unchanged — wasn't checked (§33.2). But a kept key isn't a kept value.
Change a string past its cut, and the key stays while the value
changes, possibly into another document's. That update was let
through, and the index then held a duplicate. `check` reported it
(§39), and the next write of either document was checked against it.

Now a value is passed over only if the old document held it already,
equal as `Eq` sees it: then it can't be a new duplicate. Every other
value is checked, kept key or not.

A file may hold such a duplicate from before. `check` names it; changing
or deleting one of the two documents fixes it.

## 119.2 The model, without a second equality
§33.1 says what a unique index means: `find(field == v)` never finds
two documents. The model asks exactly that, with the database's own
filters:
- for a document about to be written, each value it holds at the
  index's field (each element, for `tags[*]`) becomes an `Eq`, the AND
  of one per field for a compound index;
- `Filter::apply` runs it over the model's other documents;
- a match is a duplicate.

Arrays, objects, binary and `NaN` equal nothing under `Eq`, so they
collide with nothing, as the index holds them. One rule is the model's
own, because `Eq` can't say it: **a null collides with nothing** (§33.1),
and neither does a missing field, which a filter can't tell from null.

## 119.3 What each write must do
- **`insert`:** a stored id is `DuplicateId`. Otherwise a duplicate is
  `DuplicateValue`, naming the document written and one the model finds
  it colliding with.
- **`update` and `upsert`:** the same, against every document but the
  one replaced.
- **A batch:** the model goes op by op against the state the ops before
  left, as the database does (§33.2). The first op that fails decides
  the error, `DuplicateId`, `NotFound` or `DuplicateValue`, and nothing
  of the batch is kept. Before, any of the first two was accepted.
- **`ensure_index`, unique, over existing duplicates:** refused, naming
  two documents that collide, and no index made. The prelude makes its
  indexes before its documents, and leaves out a document a unique
  index would refuse, so that unique indexes are in most runs.
  Otherwise they'd hardly ever be made over the program's few, repeated
  values.
- **`update_fields`:** the model doesn't apply operators (§105.1), so it
  can't say which update makes a duplicate. On success, it checks that
  no document collides with another; on `DuplicateValue`, that the
  document named is one the update touched and the other is stored.

A failure prints each unique index's values in the two documents. That
is how the bug of §119.1 showed: two 990-byte strings, the same in both,
in a unique index on `(a, b)`.

## 119.4 What it costs
The model looks for each value written among the documents, a scan of
up to 400 per value: 400 programs take 39 s instead of 37. The check
after `update_fields` compares every document with every other, which
only runs after that step.

## 119.5 Tests
- `collection.rs`: an update past the cut in a one-field unique index,
  and past the cut of a field in a compound one, each refused, naming
  the index; the document's own values kept, and `5` for `5.0`, let
  through; `check` finding nothing wrong after.
- `collection.rs`: a compound index's second field differing past its
  cut, the first the same: no duplicate. A stored id inserted again,
  alone and in a batch: `DuplicateId`.
- The `queries` target: 400 programs in each of 10 seeds, without a
  failure. In 400 programs, about 250 unique indexes are made, and
  about 590 writes and index builds are refused as duplicates where the
  model says they must be.

## 119.6 Mutations
In the library's unique checks, each run against 400 programs of the
target and against the library's tests:
- **The bug of §119.1 back**, kept by key, not value: the target (in
  program 172, step 62) and the new test.
- **`check_unique` finding nothing:** both, 15 tests.
- **A null colliding:** both, 9 tests.
- **Only an array's first element checked** (§42.2): both, 4 tests.
- **A compound index with a null in it checked:** both, 1 test.
- **The document counted as its own duplicate:** the target, and none
  of the tests before. An update past the cut to a value no other
  document holds meets its own entry under the key, and the record
  there already holds the new value. Now a test does that update.
- **A compound index comparing its first field only:** the target, and
  none of the tests before. It shows only where the second field differs
  past its cut. Now a test has that.

Each caught. One changes nothing: every value checked, none passed over
as kept. A value the document held already can only meet the document
itself, which the check passes over; it costs a lookup.

## 119.7 Limits
- `update_fields` is checked only for what it lets through, and for
  naming a plausible pair when it refuses: whether a refusal was right
  would take the model applying operators.
- No unique index on `_id`, which the primary index keeps unique
  itself.
