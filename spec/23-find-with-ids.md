# 23. `find_with_ids` (`collection.rs`, `query.rs`)

Real and tested: `Collection<T>::find_with_ids(filter) -> Vec<(DocId,
T)>`, and the same on `Collection<Document>`. It closes the sync
workload prototype's one real blocker (§5.3): a typed caller could get a
document's id only from `insert`, since a `T` has no field to carry it
(§13.5) — so after a restart there was no way to find a document by a
field and then `update`/`delete` it. Tested as exactly that
(`typed_find_with_ids_finds_updatable_ids_after_reopen`).

The ids come from the data cells, which store them anyway (§11) —
`data::get_record` already returned `(DocId, Document)`, and `find` used
to drop the id. To keep each id paired with its document through
filter, sort and limit, `Filter` gained `apply_to(items, doc_of)`: the
same pipeline as `apply`, for any item that carries a document; `apply`
now delegates to it. `find` is `find_with_ids` minus the ids, on both
paths, so there's one query path, not two.

On the untyped path, an `Object` document already carries its id as
`_id` (§18); `find_with_ids` also covers non-`Object` documents, and it
is what the typed version builds on.

Considered and rejected: a `Found<T> { id, doc }` struct instead of a
tuple — more self-describing, but a pair destructures directly (`for
(id, entry) in ...`) and matches the roadmap's signature; a struct can
come later if more per-result metadata appears.
