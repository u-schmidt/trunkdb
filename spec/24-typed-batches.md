# 24. Typed batches (`batch.rs`)

Real and tested: `Database::batch()` returns a `Batch`, which collects
typed inserts, updates and deletes against any of the database's
collections and `commit`s them as one `write_batch` — atomic and
durable, all or nothing (§19.3). Until now only the untyped
`write_batch(Vec<WriteOp>)` could do that, so a typed caller had to
build `Document`s by hand. The sync workload (§5.3) is the
target: one batch per run, find-then-update via `find_with_ids` (§23).

```rust
let entries = db.collection::<Entry>("entries");
let mut batch = db.batch();
let id = batch.insert(&entries, entry)?;   // id generated now
batch.update(&entries, &known_id, changed)?;
batch.delete(&entries, &gone_id);
batch.commit()?;
```

## 24.1 Ops take collection handles
Each op takes the `Collection<T>` it targets: `T` comes from the handle
(no turbofish, no mismatched types), and so does the name (no string
repeated per op). One batch can still span collections — the point of
batches since §4.4. A handle from a *different* `Database` would write
to a same-named collection of this one; `Batch` compares the handle's
database pointer and panics on a mismatch — a caller bug, not a runtime
condition. That needed `Collection::name()` (public) and a crate-internal
`db()` accessor.

Considered and rejected:
- **Collection names as strings** (`batch.insert::<Entry>("entries",
  x)`): no link between the name and `T`, so a typo or a wrong type only
  shows up as bad data.
- **A closure API** (`db.transaction(|tx| { ... })`): would allow reads
  inside a transaction later, but nothing reads mid-batch yet, and a
  builder is easier to fill from a loop and to inspect (`len`).

## 24.2 When errors surface
- **Adding an op** converts the value to a `Document` right away, so a
  value the serde bridge can't represent (e.g. `u64` above `i64::MAX`)
  fails there, and the batch is left as it was.
- **`commit`** reports what depends on stored state — `DuplicateId`,
  `NotFound` (§22.2), a too-large document — and rolls back everything.
  A missing id is an error here even for `update`/`delete`, where
  `Collection::update`/`delete` return `Ok(false)`: in a batch, a missing
  id means the batch's assumptions are wrong, and applying the rest
  would be a partial sync.

## 24.3 Ids, ordering, and what the batch doesn't see
`insert` generates the id when the op is added and returns it, so later
ops in the same batch can target it (insert, then update or delete — ops
apply in order, against staged pages). The id names a stored document
only once `commit` succeeds. Nothing is read before `commit`: reads in
the meantime (`get`, `find`) see the state before the batch — no
read-your-own-writes, which would need reads through the staged pages
and is out of scope until something needs it. A `Batch` is
`#[must_use]`; dropping one discards its ops.

## 24.4 Untyped callers
`Batch` covers `Collection<T>` for `T: Serialize`. `Document` doesn't
implement `Serialize` (§12.1), so `insert`/`update` don't accept a
`Collection<Document>` — untyped callers keep `write_batch(Vec<WriteOp>)`.
(`delete` needs no conversion and works with either.) A shared
conversion trait, implemented for every `T: Serialize` and for
`Document`, would lift that; not needed by anything yet.
