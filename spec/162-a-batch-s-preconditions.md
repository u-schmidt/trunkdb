# 162. A batch's preconditions (`batch.rs`, `collection.rs`, `lib.rs`)

A batch commits all its ops or none (§19.3), but what it writes was
decided on something the program read before, outside the writer's
lock. Another write can come in between, and the batch commits anyway.
Two things go wrong that way:

- **A rule breaks.** Two transfers both read a balance of 80 and both
  take 50 from it; two registrations both find an email free and both
  take it; two location updates both find a device outside a geofence
  and both record that it entered.
- **An update is lost.** A program reads a document, changes it, and
  writes it back whole, over a change another write made meanwhile.

A precondition is an op that writes nothing but must hold when the
commit reaches it, or the whole batch writes nothing. Checked under the
same lock as the writes, nothing can come between the check and them.
No file format changed.

```rust
let mut batch = db.batch();
batch.require(&accounts, Filter::new().id(from).gte("balance", 50))?;
batch.update_fields(&accounts, &from, &Update::new().inc("balance", -50))?;
batch.update_fields(&accounts, &to, &Update::new().inc("balance", 50))?;
match batch.commit() {
    Ok(()) => {}
    Err(Error::PreconditionFailed { .. }) => { /* not enough: nothing moved */ }
    Err(other) => return Err(other),
}

let mut batch = db.batch();
batch.require_none(&users, Filter::new().eq("email", email).eq("active", true))?;
batch.insert(&users, user)?;
batch.commit()?;
```

## 162.1 `require` and `require_none`
- **`Batch::require(&collection, filter)`:** at least one document of
  the collection matches the filter's conditions. **`require_none`:**
  none does. Both take any collection handle of the batch's database,
  typed or not, as the other ops do.
- **Checked at `commit`, at their place:** after the ops added before
  them, on the catalog and pages as those ops leave them, and before
  the ops added after them. So a `require_none` after an insert sees the
  inserted document; a `require` after a delete doesn't find the deleted
  one; one before an insert doesn't see it yet. This is how a batch's
  `update_fields` reads (§144.1).
- **A failed one** returns `Error::PreconditionFailed { collection, op,
  matched }` and the batch is rolled back whole, as for any op's error
  (§22.2). `op` is its place in the batch, counting every op added
  before it from 0, preconditions included, as `Batch::len` counts
  them; `matched` is `true` for a `require_none` that found a document,
  `false` for a `require` that found none. It is a variant of its own
  so a program can tell "the world changed, decide again" from a
  failure.
- **A precondition is an op:** `len` counts it, an empty batch is one
  without any. A batch of preconditions alone writes nothing and commits
  as a check: useful as one, though `find` says more.
- **A missing collection** has no documents: `require` fails,
  `require_none` holds, and neither creates it.

## 162.2 One document, held to what was read
With `Filter::id`, `require` is about one document: it exists, and
matches the rest of the filter. A program that keeps a `version` field
and raises it on each write holds a read-modify-write to the version it
read:

```rust
batch.require(&contacts, Filter::new().id(id).eq("version", read_version))?;
batch.update(&contacts, &id, contact_with_the_next_version)?;
```

If another write came in between, the batch fails, and the program
reads again. That is optimistic concurrency, as a SQL `UPDATE … WHERE
version = ?` that checks the rows it changed. A failure doesn't say
whether the document is gone or changed: the read that follows does.

## 162.3 Reading
`any_match_in` reads as `count` does (§101.4), through `each_candidate`:
by id where the filter names ids, through the range of an index where
one bounds the conditions, an entry that decides the conditions (§100,
§104) counting without its document, and the planner's choice or the
filter's hint (§132) as for any read. It stops at the first match,
where `count` reads on: a check by id reads one document; one an
index's entries decide, at most a leaf. A precondition no index bounds
reads the collection, and does so with the writer's lock held, so every
other write waits for it: as for `update_many` with such a filter, an
index is the cure.

A `Condition::custom` runs as in any read (§136): one that panics fails
the batch with `Error::ClosurePanicked`, nothing written, the database
usable.

## 162.4 What a filter may have
Only its conditions are checked. A sort (by fields, or by a key of the
application's, §154), a skip, a limit, a `select` or an `except` would
mean nothing here, and a program that wrote one meant something else:
"at least five" is not `limit(5)`. They are refused when the
precondition is added, as a write's invalid input (`Io` of kind
`InvalidInput`), leaving the batch as it was. A hint is kept: it says
how to read, and a hint that can't be followed fails the commit with
`Error::Hint`, as it fails any read.

## 162.5 Left out
- **A count:** `require_count(filter, n)`, "exactly one", "fewer than
  ten". `require` with `Filter::id` covers "exactly this one"; a unique
  index makes any other `require` exactly one. A count is a
  `count_matches` where `any_match_in` stops early, and can come when a
  program needs it (ROADMAP.md, Deferred).
- **Versions kept by trunkdb:** a version or an etag on every document,
  raised by every write, as SQL Server's `rowversion` or Cosmos DB's
  `_etag`. It would spare the program its own field, but it is a
  field in every stored document or its record, so a format change,
  and every write pays for it whether a program checks it or not
  (ROADMAP.md, Deferred).
- **Text and the command line:** the text language reads a filter or
  an aggregate, not a batch, and the CLI writes one op per call; a
  precondition there has nothing to guard.
- **A guard on one op,** `update_fields_if(id, condition, update)`:
  the same thing for one document, but one method per kind of write,
  and no rule about another document or the absence of one. A
  precondition composes with every op.
- **A transaction that reads:** a closure run under the writer's lock,
  reading and writing as it likes. More general, but it runs the
  application's code with every other write waiting, as long as it
  takes; §136.7 and §137 keep that code short and outside. A
  precondition is a read trunkdb does itself, and as short as its
  filter allows.

## 162.6 Tests
In `tests/preconditions.rs`:
- **The transfer:** a batch writes while the balance covers it, fails
  with op 0 and writes nothing when it doesn't.
- **`require_none`** holds a rule a unique index can't, an email unique
  among active users only; the failed op is 1, after an insert.
- **Order:** a precondition sees an insert, an update and a delete
  before it, and not an insert after it; a failed one rolls back a
  delete before it.
- **Refused:** a sort by fields and by key, a skip, a limit, a select,
  an except, each with its name in the error and the batch left empty;
  a hint followed where the index exists and failing the commit with
  `Error::Hint` where it doesn't.
- **Preconditions alone:** a batch of two commits as a check, fails at
  the second where it doesn't hold, and is no commit a subscriber hears
  of (§135).
- **A missing collection, a panicking custom condition, a typed
  collection** (rooms booked by the hour).
- **A model:** 400 random batches of up to six inserts, updates,
  deletes and preconditions, `require` and `require_none` on a field, a
  range, two fields, a multikey path and an id, against a model applying
  them in order: the same outcome, the same failed op and `matched`,
  the same documents after, for four sets of indexes (none, `k`,
  `(k, v)`, `tags[*]` and `v`), and both outcomes over 50 times in each.
- **Threads, a rule:** four threads move random amounts between three
  accounts, each only while the balance it read covers it, 1,200 moves:
  no balance below zero and the sum kept.
- **Threads, a version:** four threads count one document up 100 times
  each by reading it and writing it back whole, held to the version
  they read and reading again on a failure: 400 at the end, none lost.
  A thousand failures in a row fail the test, as each failure is
  another thread's success: a check that never holds fails it rather
  than hanging.

## 162.7 Mutations
Each made by hand and the tests run (`tests/preconditions.rs`):

- **No check at all:** caught, the transfer and the missing collection
  among others.
- **`require` and `require_none` swapped:** caught; the version test,
  whose every attempt then fails, by its bound on attempts, where it
  hung before it had one.
- **The op counted from 1,** and **`matched` reported the other way:**
  caught by every test that reads the error.
- **An entry that decides taken as undecided,** and **every entry taken
  as deciding:** caught, the first by the hint read through an index on
  `v`, the second by the batch of preconditions alone.
- **The hint not followed:** caught.
- **A limit, or a sort by a key, let through:** caught by the refusals.
- **A panic in the matcher taken as no match:** caught by the custom
  condition that panics.

Not tested: that the walk stops at the first match. It changes how much
is read, not what a check says, and a batch has no `explain`.
