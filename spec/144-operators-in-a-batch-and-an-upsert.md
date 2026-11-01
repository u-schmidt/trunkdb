# 144. Operators in a batch and an upsert (`batch.rs`, `collection.rs`, `update.rs`)

§68.3 left the update operators out of a `Batch` and out of `upsert`:
`update_fields` was its own batch, and a batch or an upsert could only
write a whole document. Two common writes need the operators there. A
transfer moves an amount from one account to another, all or nothing,
without reading either first. A counter, a page's visits or a sensor's
readings, is one document per key that the first write makes and
later ones add to. No file format changed.

```rust
use trunkdb::query::{Filter, Update};

let mut batch = db.batch();
batch.update_fields(&accounts, &from, &Update::new().inc("balance", -25))?;
batch.update_fields(&accounts, &to, &Update::new().inc("balance", 25))?;
batch.commit()?;

visits.upsert_fields(
    Filter::new().eq("page", "/home"),
    &Update::new().inc("hits", 1).set_on_insert("first", DateTime::now()),
)?;   // { page: "/home", hits: 1, first: … } the first time, hits + 1 after
```

## 144.1 `Batch::update_fields(&collection, &id, &update)`
Operators on one document by id, as `Batch::update` replaces one:
- **Made at `commit`, in order,** to the document as the ops before
  them leave it: one inserted, replaced or changed earlier in the same
  batch is changed as it is by then. Nothing is read before `commit`,
  as for every other op (§60).
- **As `Collection::update_fields` makes them** (§68.2): an operator
  that can't be made fails the batch with `Error::Update`, naming the
  document; on a typed collection, a changed document that no longer
  converts to `T` fails it with the conversion's error; a document left
  as it was, judged by its encoding, isn't written, nor reported
  (§135.2).
- **A missing id fails the batch** with `Error::NotFound`, as for
  `Batch::update` and `Batch::delete`.
- **An update refused as written** (§68.1, §73) is refused when it's
  added, `ErrorKind::InvalidInput`, leaving the batch as it was, as a
  value that doesn't convert is (§60).
- **The update is cloned into the batch,** so the caller keeps it, as
  `Collection::update_fields` takes it by reference.

**Which `T`:** the typed check needs `T` to deserialize, which
`Batch::insert` doesn't. A second sealed trait, `FromDocument`, beside
`IntoDocument`, does it: any `DeserializeOwned` type, or `Document`,
which takes any document. The batch keeps a `fn` pointer to `T`'s
check with each op.

**How a batch commits:** each op under the writer's lock, in order, as
`upsert` and `update_many` do (§29.3, §38.2), so an op of operators
reads what the ops before it wrote. A batch used to go through the
`TransactionManager` (§17); `Database::write_batch` still does, for
single writes, imports and compaction, and its one lock was never
contended, since the writer's lock is taken first (§27).

## 144.2 `Collection::upsert_fields(filter, &update)`
`upsert` with operators instead of a document, typed and untyped:
- **One match:** the operators are made to it, as `update_fields`
  would make them; `Upserted::Updated(id)`. Left as it was, it isn't
  written, and is still `Updated`.
- **No match:** a new document is made of the filter's `eq`s, then the
  operators are made to it, `set_on_insert` included;
  `Upserted::Inserted(id)`. MongoDB's `upsert: true` does the same.
- **Several matches:** `Error::MultipleMatches`, as for `upsert`.
- **On the typed path,** the document written, inserted or updated,
  must convert to `T`. A `T` with a field that neither the filter nor
  the operators give fails as an insert, which is how a `set_on_insert`
  is found missing.
- **Atomic,** as `upsert` is: the lookup and the write under one lock,
  so threads counting the same key make one document between them.

**The new document's fields** (`update::seed`) come from each `eq` at
the top of the filter or inside an AND, on a path an update could name
(§68.1): objects on the way are made, as `set` makes them. An `eq` on
`_id` with an id gives the new document's id; any other is a new id.
Left out, since none says one value: ORs, NOTs, `ne`, the ranges,
`exists`, `size`, `elem_match`, a custom condition, and paths with
brackets. The rest of the filter still decides what matches; the
document inserted need not match it, as in MongoDB.

**Refused before anything is read,** `ErrorKind::InvalidInput`: an
update `update_fields` refuses, and a filter whose `eq`s can't be made
into one document: a field given two values that aren't equal (`1` and
`1.0` are equal, §34.1), a value given to a field and to one inside it
(`a` and `a.b`), or two ids. These are refused even where something
matches: the same filter would fail the day nothing does.

## 144.3 `Update::set_on_insert(path, value)`
A `set` made only where `upsert_fields` inserts, MongoDB's
`$setOnInsert`: a `created` the first write gives and later ones leave
alone. Anywhere else, in `update_fields` or a batch, it changes
nothing. Its path is checked as `set`'s.

## 144.4 Rejected
- **A batch's `update_fields` by a filter,** as the collection's: a
  batch's ops name documents by id, and an op whose documents depend on
  the ops before it would need a count back from `commit`. The
  collection's `update_fields` is one batch of its own already.
- **`Batch::upsert`:** not asked for; a batch that needs one reads first.
- **An operator op in `WriteOp`:** `WriteOp` is what a commit records,
  for the report (§135) and the indexes; operators become an `Update`
  of the whole document when they're made, so the rest of the commit
  sees only the writes it knows.
- **Refusing an upsert's insert that doesn't match the filter:** MongoDB
  doesn't, and the filter's conditions besides its `eq`s say no values.
- **Seeding from `is_in` with one value:** said as an `eq` already.
- **Leaving out `set_on_insert`:** without it, an upsert's operators
  can't give a field once without changing it on every later write.

## 144.5 Tests
- `update.rs`: `set_on_insert` made only when inserting, checked as
  `set`, failing through a string; the seed taking `eq`s at the top and
  in nested ANDs, on nested paths, with null, an `_id`, and leaving out
  an OR, a NOT, `gt`, `ne`, a `[*]` path and an `_id` that isn't an id;
  two equal values kept as one; two values, a field and one inside it
  in either order, and two ids refused; `ab` not inside `a`.
- `collection.rs`:
  - visits: an insert from the filter's `eq`s, nested, and the
    operators; an update of it leaving `set_on_insert`'s field; the
    index following; an `_id` in the filter giving the id; a match left
    as it was neither written nor reported; refused as written (two
    values, a field and one inside it, an update of `_id`), several
    matches, an operator failing on the match and on the insert, none
    of them writing anything;
  - typed visits from four threads, 100 visits into one document per
    page; an insert without a field `T` needs, and a value of the wrong
    type, failing.
- `batch.rs`:
  - a transfer between two documents; operators on a document inserted
    earlier in the batch, twice, and on one replaced earlier; the index
    following;
  - a missing id, and one deleted earlier in the batch, failing it with
    `NotFound`; an operator that can't be made failing it with
    `Error::Update`, the insert before it not landing; a typed document
    that doesn't convert failing it, the untyped handle on the same
    collection taking it; an update refused when added, the batch
    left empty; a batch that changes nothing not reported.
- **The `queries` fuzz target** (§105): one upsert in three is an
  `upsert_fields`, its result as the model's matches say, the document
  then read back; one batch in three is one to four ops, inserts of new
  ids and the rest on ids it holds, ending with one to three
  `update_fields` ops, on an id it holds seven times in eight, failing
  as the model says or committing, the documents then read back; no
  duplicate in a unique index left either way. The updates draw
  `set_on_insert` too. 1,000 programs ran 967 `upsert_fields`, 350 of
  them inserting, and 3,017 batch ops of operators: 385 batches
  committed, 61 failed on a missing id, 1,123 on an operator, 3 on a
  duplicate, 239 on an op before them. 2,000 of seed 7 ran 1,822 and
  5,902: 680 committed. It draws differently, so a seed no longer gives
  the program it gave.

## 144.6 Mutations
| Mutation | Caught by |
|---|---|
| `set_on_insert` made on every update | `set_on_insert`, visits |
| `set_on_insert` never made | `set_on_insert`, visits, typed visits |
| The seed skipping ANDs | the seed |
| The seed taking every comparison | the seed |
| The seed ignoring `_id` | the seed, visits |
| Two values for a field let through | the seed, visits |
| A field and one inside it let through | the seed |
| A document left as it was written anyway | batch failing whole, visits |
| The changed document not checked | batch failing whole, typed visits |
| A batch's update not checked when added | batch failing whole |
| A batch's operators made after its other ops | batch on what it has so far |
| An insert made without `set_on_insert` | visits, typed visits |
| The filter's `_id` not used for the insert | visits |
| The typed `upsert_fields` not converted | typed visits |
| A typed batch op not converted | batch failing whole |
