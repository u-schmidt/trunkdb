# 60. A smaller public API (`lib.rs`, and every module it no longer exports)

Twenty modules were public, so a second application could reach the
page layer, the B-tree, the WAL and the catalog, and all of it would
have been part of what 1.0 promises (ROADMAP.md, "Before 1.0"). The
applications using trunkdb needed five types: `Database`, `Collection`,
`DocId`, `Filter` and `Sort`, plus `Op` in one. Shrinking the surface to
what an application needs showed where the API itself was unclear, and
each of those got a decision.

## 60.1 What's public now

```
trunkdb::{Database, OpenOptions, Collection, Batch, Cursor, Upserted,
          Document, DocId, DocumentError, IndexOptions, IndexInfo,
          IndexFields, IntoDocument, CheckReport, FileInfo, Compacted,
          Summary, Error, Result}
trunkdb::query::{Filter, Condition, Op, SortOrder, QueryPlan}
trunkdb::fuzzing   (the `fuzzing` feature only, hidden: §55)
```

Everything else is `pub(crate)`, in private modules: `storage`, `index`,
`catalog`, `data`, `durability`, `txn`, `id`, `json`, `serde_bridge`,
and the modules whose types are re-exported at the root. Making them
private showed what was public only to be public: `NoopDurability`, the
"fake for tests" no test used, and `FileStore::sync`, used nowhere, are
gone; `InMemoryIndex`, `KeyRange::contains` and `key::is_exact` are
test-only now.

## 60.2 The decisions
**One batch (B1).** `db.batch()` was typed only; untyped documents went
through `Database::write_batch(Vec<WriteOp>)`, with collection names as
strings and ids the caller had to make up, and the whole `txn` module
public for it. Now `Batch::insert` and `update` take any `T:
IntoDocument`: a serde type, as `Collection<T>` converts it, or a
`Document` as it is. `IntoDocument` is sealed (below), and its two
impls can't overlap for the same reason the typed and untyped
`Collection` impls can't: `Document` isn't `Serialize`, and only this
crate could make it so. An untyped document's `_id` is its id (§59).
`write_batch` and `WriteOp` are internal, the engine under `Batch`,
`upsert` and import. A document that isn't an object (`Document::Int`,
`Binary`) has no field for an id, so a batch gives it a new one; the
benchmark now stores its bytes in an object with an `_id`.

**Conversions on `Document` (B2).** `serde_bridge::to_document` and
`from_document` became `Document::from_value(&t)` and
`doc.into_value::<T>()`, where a consumer holding a `Document` looks.
`into_` because it takes the document over: its content moves into the
value. A blanket `TryFrom` would collide with the standard library's.
`DocumentError` stays public as the payload of `Error::Document`.

**`Filter` only through its methods (B3).** Its fields were public, so
the representation (`Vec<Condition>`, `Vec<Sort>`, `Option<usize>`) was
API, and §47 had to break it once. The fields are `pub(crate)` now, and
`Sort` is internal; the builder (`eq`, `sort_desc`, `then_asc`, `limit`,
...) is the way in. `limit` takes `impl Into<Option<usize>>`, so a
caller's optional limit passes through: `limit(20)`, `limit(None)`.

**Limits on the types they limit (B4).** Not a `limits` module: as
`u32::MAX` sits on `u32`, `Document::MAX_NESTING` sits on `Document`,
and the limits with no better home on `Database`, the handle every
application holds: `MAX_COLLECTION_NAME_LEN`, `MAX_FIELD_NAME_LEN`,
`MAX_COMPOUND_FIELDS`. `Collection` is generic, and a constant on it
would read `Collection::<Car>::MAX_...`. `Database`'s documentation
lists all of them in one section. The index key's byte budget stays
internal: no caller passes a key.

**`IndexFields` sealed, and one index API (B5).** `IndexFields` has to
be public, since `ensure_index` takes it, but only trunkdb should
implement it; a private supertrait (`sealed::Sealed`) that outside code
can't name does that. Sealed means callers can't add forms, so trunkdb
gained the run-time ones: `Vec<String>`, `Vec<&str>`, `&[String]`.
The index methods had grown in pairs with every option; now there are
two ways to create one, `ensure_index(fields)` and
`ensure_index_with(fields, options)` (`ensure_unique_index` is gone:
`IndexOptions::new().unique()`), and one way to list them:
`indexes() -> Vec<IndexInfo>`, each with `fields()`, `name()`,
`options()`, `is_unique()`, `is_sparse()`. A new option adds a getter,
not two methods.

**Room to grow (B6).** The rule: types a caller gives trunkdb get
private fields and builders (`Filter`, `OpenOptions`, and now
`IndexOptions`: `new().unique().sparse()`, `is_unique()`,
`is_sparse()`); types trunkdb gives back keep public fields and get
`#[non_exhaustive]` (`CheckReport`, `FileInfo`, `Compacted`, `Summary`),
so a field added later breaks no one. Enums that will grow get it too:
`QueryPlan` (routing `eq("_id")` to the primary index is a new plan),
`Error` and its struct variants, and `Document`, since more value types
may come, not only a date. `Upserted` and `SortOrder` stay exhaustive:
they're complete by nature. `Error::Txn` is gone: its `TxnError` only
ever said a writer thread panicked holding the batch lock, which is
what `Error::Poisoned` already means (reopen, and the WAL restores what
was committed).

## 60.3 What it cost the applications
- The one that stores its data in trunkdb built a `Filter` as a literal
  with `Sort::desc`: one line, now
  `Filter::new().sort_desc("started_at").limit(limit)`.
- The learning app compiles as it was.
- The benchmark moved from `write_batch` to `Batch` on an untyped
  collection, with `Document::from_value`; the fuzz crate takes the page
  size from `fuzzing` and builds `IndexOptions` with its methods; the
  command line reads the page size from `FileInfo`.

## 60.4 Limits
- **`Document`'s variants are public**, including
  `Object(IndexMap<String, Document>)`: building an object needs the
  `indexmap` type, or `Document::Object(Default::default())` and
  `insert`. Hiding it behind methods is a bigger change, not needed yet.
- **`Error::Io` exposes `std::io::Error`**, with `InvalidInput` and
  `InvalidData` as the kinds a caller sees for limits and damage. A
  richer error type is open.
- **Sealing is a convention**, not a keyword: a private module outside
  code can't name.
- **Lock poisoning** maps to `Error::Poisoned` untested: it needs a
  thread to panic while holding the batch lock.

## 60.5 Tests
- `collection.rs`: `indexes` lists each index's name, fields and
  options, in creation order, from every form of field names
  (`&str`, `Vec<String>`, `Vec<&str>`, `&[String]`); `limit` takes a
  number, `Some` and `None`.
- `batch.rs`: one batch writes a typed and an untyped collection; an
  untyped document's `_id` is its id; a batch that fails rolls back both.
- Doctests: `IndexOptions`' builder, `Filter`'s, `ensure_index_with`,
  `Document::from_value`.
- The existing tests moved to the new API mechanically: 24 uses of
  `ensure_unique_index`, and `indexes()` where a test compared names
  (`index_names()`, a test-only helper now).
- Checked by breaking it on purpose, seven ways. Each of these fails a
  test:
  - a compound index named without parentheses;
  - `is_unique` reading the wrong option; `sparse()` setting it;
  - an untyped batch losing its document;
  - no limit becoming one;
  - a `Vec<&str>` of fields lost;
  - indexes listed out of creation order.
