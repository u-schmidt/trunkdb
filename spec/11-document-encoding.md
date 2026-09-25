# 11. Document encoding (`document.rs`, `data.rs`)

*Updated by §26: lengths and counts are `u32` now, and the cell format is
`[u8 flags][DocId][payload]` (§20.5) with overflow cells (§26.2).*
Real and tested: `document::encode_document`/`decode_document` convert a
`Document` to and from bytes — one type tag, then a tag-shaped payload
(fixed-width for scalars, `u16` length + bytes for `String`/`Binary`, `u16`
count + that many encoded elements for `Array`, count + `(key length, key
bytes, encoded value)` triples for `Object`). `u16` is enough for every
length here since a whole document has to fit inside one 8192-byte page's
cell budget regardless (§7.1). `data::encode_record`/`decode_record` sit on
top: a document's own `DocId`, prefixed onto its encoded bytes — this is
the actual `Data` page cell format referenced in §7.3. The `DocId` has to
be stored explicitly rather than assumed to live inside the document's own
fields, because nothing else records it once persisted: the index maps
`id -> location`, never the reverse.

## 11.1 Recursive encode/decode: why `decode_document` returns a remainder
`Array`/`Object` nest arbitrarily, and neither the encoder nor decoder
knows ahead of time how many bytes a nested element occupies — only
`Array`/`Object` store an element *count*, never a total byte size. Encode
handles this for free (it just keeps appending to one growing buffer,
recursing into `write_document` for nested elements). Decode can't do the
equivalent so easily, since it's handed one flat slice and has to work out
where each value stops: `decode_document(bytes) -> (Document, &[u8])`
returns not just the parsed value but everything *after* it, and a
recursive caller (decoding an `Array`/`Object`'s elements) feeds that
remainder back in as the start of the next element. Every level of nesting
trusts its recursive call's remainder without needing to know anything
about what's inside it — this is what lets arbitrarily deep nesting work
without the format needing to record total byte sizes anywhere.

## 11.2 Errors: only for real corruption, not truncation
`decode_document` returns `Err` for exactly two things: an unrecognized
type tag, and invalid UTF-8 in a string or object key — both realistic
disk-corruption symptoms, mirroring how `PageType::from_u8` and the
catalog's name-decoding already behave. It does *not* guard against a
truncated buffer; that panics via ordinary out-of-bounds slicing instead
of a graceful error. Same trust level as the rest of the crate's cell
decoders: nothing ever reads a cell's bytes except code that wrote them,
so "the buffer is shorter than the format says it should be" isn't a
reachable state in practice.

## 11.3 Considered and rejected: two-pass size-then-write encoding
A first pass to compute the exact encoded size (so `encode_document` could
`Vec::with_capacity` the precise total up front, avoiding the buffer's
internal reallocations) was considered and rejected for v0. `Vec`'s growth
is already amortized (roughly doubling), so the total bytes ever copied
across every regrow is bounded by about twice the final size — for
anything capped at 8192 bytes, that's at most ~16KB of `memmove`, once,
per document; not a measured or plausible bottleneck next to the disk
write that follows it. A size-computing pre-pass would have to mirror
`write_document`'s entire recursive shape without writing anything — real
duplicated logic that has to stay in sync with the encoder forever, and an
`Vec::with_capacity` under-guess wouldn't even fail loudly (it's a hint,
not a hard limit, so it just silently falls back to normal reallocation),
making the two versions drifting apart a plausible, quiet bug. Worth
revisiting only if profiling ever shows this encoding path actually
dominating insert time in practice.

## 11.4 `Data` page management (`data.rs`): no chaining, one document per page
*Superseded by §20: documents are packed several per page now, updates
can move a document, and a page is freed only once it's empty.*
Real and tested: `insert_record`/`get_record`/`update_record`/
`delete_record`. Unlike `Catalog`/`IndexLeaf`, `Data` pages are never
chained — each document gets its own freshly-allocated page (the v0
simplification already recorded in §9: no `current_data_page`, revisit
only if this proves wasteful). One consequence worth naming: every live
`RecordLocation` this module hands out has `slot == 0`, since it's always
the first and only cell ever inserted into that page — asserted at the top
of `update_record`/`delete_record` rather than left implicit.

No chaining also means no page-walking loop, so this module is just four
plain functions, not a struct — unlike `Catalog`/`BTreeIndex`, there's no
root page or cache to hold between calls, so a wrapper type would hold
nothing.

`update_record` rebuilds the target page from scratch (`SlottedPage` has
no "replace a cell in place" operation, only append and tombstone) and
writes it back to the *same* page id — `loc.page` never changes on update,
so nothing that references the location (the index) needs touching for a
same-collection update. `delete_record` frees the whole page via
`store.free_page`, not just the one cell in it, since a page is never
shared between documents — freeing the page *is* freeing the document.

`insert_record`/`update_record` return a real `Err` (not `.expect()`) when
a document doesn't fit in one page — unlike every other `.expect()` in
this codebase, which is only ever safe because a `has_room_for` check
immediately before it guards an invariant the code itself controls, here
the document's size comes from the caller, so "too large" is a reachable,
legitimate outcome, not a bug. `insert_record` checks this against a
throwaway empty page *before* calling `allocate_page`, so a
too-large document never wastes (or has to roll back) a real allocation.
