# 12. Wiring `Collection<Document>` (`collection.rs`)

Real and tested: `insert`/`get`/`update`/`delete`/`find` all now do real
work for `Collection<Document>` — the untyped path. Every method follows
the same shape: look up the collection's `CollectionMeta` via a shared
`meta` helper (creating it, via `Catalog::create_collection`, only on
`insert`'s first call), build a `BTreeIndex` from `meta.index_root` (free
— it's one `PageId`, nothing to load), then combine the index and `data.rs`:
`insert` writes the record then indexes it; `get`/`find` look the location
up (or scan) then read it; `update` rewrites the data page, and
re-points the index only if the document had to move (§20.3); `delete`
removes the data cell *and* the index entry — both, since either alone
would leave the other side pointing at nothing.

Two additions to `Database` were needed to make this possible:
`catalog`'s field visibility widened from private to `pub(crate)` (same
reasoning as `store`'s — `Collection` needs direct access from its method
bodies), and a new `pub(crate) id_gen: UuidV7Generator` field — plain, no
`RefCell`, since generating an id never mutates the generator's own state.
(Since §27 these live in `State` behind the lock, `id_gen` outside it.)

## 12.1 A second `impl` block, not a filled-in generic one
At the time this was built, `Collection<T>`'s existing generic methods
(bounded on `T: Serialize + DeserializeOwned`) were still `todo!()` — they
needed the serde bridge (§13) to convert an arbitrary `T` to/from
`Document`, which didn't exist yet. `Document` needs no such conversion;
it already *is* the storage representation. So the real implementation
went into a second, concrete `impl<'db> Collection<'db, Document>` block
instead of the generic one. Rust allowed both blocks to coexist without
ambiguity, since `Document` didn't (at that point) derive
`Serialize`/`DeserializeOwned`, so the generic bound never applied to it —
the two blocks never competed for the same call. This turned out to be
more than a stopgap: once the serde bridge did land, `Collection<T>`'s
generic methods (§13.3) were wired as a thin layer that converts and then
*calls this same concrete impl* rather than duplicating its logic — so
this "second impl block" is the permanent shape, not a temporary one.
