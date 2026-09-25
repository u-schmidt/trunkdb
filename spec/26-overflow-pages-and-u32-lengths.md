# 26. Overflow pages and `u32` lengths (`data.rs`, `document.rs`)

Real and tested: a document can be larger than a page — up to 4 GiB
encoded, in practice bounded by memory, since it's encoded, logged and
decoded as a whole. Documents with ~200 KB of
text (the large-document workload, §5.2) insert, update, turn up in a
`Contains` search and survive a reopen
(`documents_larger_than_a_page_work_end_to_end`). Documents that fit a page
are stored exactly as before; the sync workload's data never touches an overflow
page. Format version 2 (§21.2).

## 26.1 `u32` lengths in the document encoding
Every length and count in `encode_document` — `String`/`Binary` lengths,
`Array`/`Object` counts, object key lengths — went from `u16` to `u32`.
With documents capped at a page, `u16` could never wrap; past a page,
`as u16` would have truncated a 70 KB string's length silently and
misdecoded everything after it (§22.4 flagged this).

The `as u32` casts can't truncate: every inner length is at most the
encoding's total, and `data.rs` refuses a total past `u32::MAX` with
`InvalidInput` before writing anything — the one "too large" left. Keys
got `u32` too, for one rule instead of two; 64 KB keys are silly, but a
second width would mean a second error path in an encoder that has none.

Cost: two bytes per string, key, array and object — for the sync workload's
~900 B documents with ~20 fields, roughly 5–10 %.

Considered and rejected: **variable-length integers** (LEB128, as in
protobuf and SQLite): a length under 128 would take one byte, smaller
than even `u16`. But every length becomes a loop instead of a
`split_at(4)`, and "how long is this prefix" stops being a constant —
a real complexity cost in a format whose virtue so far is being readable
in a hex dump. The space it saves is small next to §20's packing; the
format version (§21.2) leaves the door open if measurements ever say
otherwise.

## 26.2 When a document overflows
Purely by size: a document stays inline if its cell (`[flags][id]
[document]`) fits on an empty data page — at most 8175 bytes — and
overflows otherwise. So:
- nothing changes for documents that fit, and there's no threshold to
  tune;
- the representation follows from the document alone, so an update
  switches it either way when the size crosses the line (inline → an
  overflow cell, and back, freeing the chain).

The cutoff sits at the edge, not lower as in SQLite (which overflows
cells past about a quarter page, to keep ≥ 4 cells per B-tree page):
trunkdb's data pages aren't B-tree nodes — nothing searches inside them,
so one large inline document on its own page costs nothing extra, while
overflowing it would waste the unused tail of its last overflow page.

## 26.3 The format
An overflow cell is `[u8 flags = 1][16-byte DocId][u32 length][u64 first
page]` — 29 bytes, packed onto the collection's data pages like any
other cell (§20.1). The encoded document lives in a chain of `Overflow`
pages:

```
[0]      page type tag (Overflow = 6)
[1..9)   next page id (0 = last)
[9..)    the next 8183 bytes of the document; on the last page only the
         remainder, the rest zeroed
```

No `SlottedPage` (one run of bytes needs no slot directory), and no
per-page length: the cell's total length says how much each page
holds — all of it, except on the last.

Rejected: keeping **"as much of the document as fits"** in the cell
itself, which §20.5 had sketched. It would save at most one page per
large document, but the cell would then fill its data page — no other
document could share it, and the cell size would depend on where it's
written. The pointer-only cell is fixed-size and always packs.

Also rejected: a **B-tree of blob pages** or extent allocation
(contiguous page runs, like SQL Server's LOB storage). Both are about
large-object random access and fragmentation; trunkdb reads and writes a
document as a whole, and a linked list is the simplest structure that
does that.

## 26.4 Writing, updating, deleting
- **Insert**: encode, write the chain (all pages allocated first, since
  each stores its successor), then place the 29-byte cell (§20.1).
- **Update**: free the old chain, write the new one, then update the cell
  — which, the cell being small, almost always stays in place. No
  in-place chain rewrite: the free list is LIFO, so the new chain gets
  the old pages back anyway, and one path is simpler than "reuse, then
  extend or trim".
- **Delete**: free the chain, then tombstone the cell (§20.4).

All of it is staged like any other page write (§19.2), so a batch that
fails rolls back the chain pages too; `a_failed_batch_leaves_no_trace`
now includes a large document and checks the file didn't grow.

## 26.5 Reading, and corrupt chains
`get_record` follows the chain, collecting the bytes, and decodes the
result. Scans (`find`) go through `get_record` for every document
already, so they need no change — and do read every chain, which is
fine for "search the prose" and wasteful for "filter on a small field
of a large document". Field-level lazy reading would need a different
encoding (offsets to fields); not before real use asks for it.

The walker trusts the cell's length, not the `next` pointers, to decide
where a chain ends, so corruption can't loop it forever: a chain that
ends early, one whose last page still links onwards, and a page in it
not tagged `Overflow` are all `InvalidData`. `delete`/`update` walk the
whole chain before freeing any of it, so a corrupt chain fails the batch
instead of freeing pages that belong to something else.

## 26.6 Cost
A large document is written twice like everything else (§19.9) — a
1 MB document is ~128 pages in the WAL and again in the main file — and
it is held in memory whole: as a `Document`, its encoding, and the
staged page images until commit. Fine for the large-document workload (tens to hundreds of KB);
a streaming blob API would be a different feature.
