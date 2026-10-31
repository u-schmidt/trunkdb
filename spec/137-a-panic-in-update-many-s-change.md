# 137. A panic in `update_many`'s change (`collection.rs`)

§136 kept a panic in a `Condition::custom` apart from trunkdb: the call
fails, and the database goes on. Its limits (§136.7) named one more
closure of the application's that runs under the writer's lock,
uncaught: the change `update_many` makes to each document. A panic
there unwound through the batch, poisoned the writer's lock, and every
later write failed with `Error::Poisoned` until the database was
opened again. A bug in one closure cost every write after it.

## 137.1 What is caught
Code of the application's that runs while the writer's lock is held:
- `update_many`'s `change`, untyped and typed;
- a typed collection's `T`, converted in that batch: its `Deserialize`,
  of each match `update_many` hands to `change`, and of each document
  `update_fields` changed, which must still convert (§68); its
  `Serialize`, of what `change` made.

A panic there fails the call with **`Error::ClosurePanicked`**, carrying
the panic's message. The batch fails as any batch does: its catalog is
dropped, nothing is written, the lock is let go, not poisoned. Matches
changed before the one that panicked are dropped with it: all or none,
as `update_many` promised already.

The rest of a batch is trunkdb's own code, which a panic of trunkdb's
should poison: it may have left the batch half done.

## 137.2 One error for both
§136's error was `ConditionPanicked`. It now covers more than
conditions, so it is **`ClosurePanicked`**, for both: renamed before
either was released. Its message says "a closure of the application's
panicked".

`caught(run)` in `query.rs` is the one way to call such code:
`catch_unwind`, the panic made the error, which the batch's `?` passes
on.

## 137.3 Not caught
- **Reads of a `T`:** `find` and the others hold no writer's lock. A
  panic in `Deserialize` there goes on into the program, as it did, and
  leaves nothing poisoned.
- **A typed `insert` or `upsert`** converts its `T` before the batch
  begins, outside the lock.
- **`Batch`:** its writes are converted as they are added, before
  `commit` takes the lock.

## 137.4 Tests
`tests/update_panics.rs`:
- an untyped `change` that panics at the third match: the error with
  its message, the two before it not written either;
- typed: a `change` that panics, a `Serialize` that panics on what the
  change made, a `Deserialize` that panics on a match, and on what
  `update_fields` set; nothing written;
- after each, the database goes on: an insert, an `update_many`, a
  `delete_many`, a `check`;
- a typed read whose `Deserialize` panics panics on, and the database
  goes on.

## 137.5 Mutations
| Mutation | Caught by |
|---|---|
| The untyped `change` called uncaught | a change that panics |
| The typed `change` and conversions called uncaught | a typed change or conversion |
| `update_fields`' check of `T` called uncaught | a typed change or conversion |
| `caught` letting the panic through | both |
