# 18. Merging `_id` into the untyped path (`collection.rs`)

Real and tested: a document read back from `Collection<Document>` now
reports its own `_id`, LiteDB/Mongo convention — not a bug fix, a
deliberate v0 gap (§13.5) closed on request once it was actually hit in
practice. `apply_write_op`'s `Insert`/`Update` branches run the document
through a new `with_id(doc, id)` before storing: if it's an `Object`, it
merges (overwriting any existing one) an `"_id"` key set to
`Document::Id(id)` — the real id `insert`/`update` were actually called
with, never whatever a caller may have already put there. Non-`Object`
documents (a bare `Document::Int`, `String`, ...) pass through unchanged
— there's no field to attach an id to.

Enforced in `apply_write_op`, not in `get`/`find`/`insert`/`update`
directly, for the same reason that function exists at all (§16.2): it's
the one path both live writes and crash-recovery replay share, so the
merge happens exactly once and the *stored bytes* are canonical from the
moment of creation. `get`/`find` needed no changes — they just return
whatever's on disk, which now already has the right `_id`.

The typed path (`Collection<T>`) is unaffected on purpose: `from_document`
deserializes permissively (unknown map keys ignored, same as
`serde_json`'s default), so the extra `_id` key now present in the
underlying `Document::Object` is silently dropped for a `T` that has no
matching field. Giving a typed struct its own populated id field is a
separate, bigger question (§13.5) — not part of this.
