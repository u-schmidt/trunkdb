# 10. Index (`index/btree.rs`, `index/branch.rs`, `index/leaf.rs`, `index/in_memory.rs`)

*Updated by §28: keys are byte strings of any length up to 1024 bytes,
not `DocId`s, and pages split by bytes, not entry count (§28.2). The
primary index's pages are byte for byte what they were.*

Real and tested: `BTreeIndex` is the disk-backed `Index` implementation
for the primary `_id` index — a real B-tree with linked leaves, replacing
the earlier `LinearIndex` (a flat, unordered chain of `IndexLeaf` pages,
O(n) for everything; deleted, fully superseded). `insert`/`lookup`/
`remove` are O(log n): a page holds ~270 leaf entries or ~290 branch
entries at 8192 bytes, so even a few hundred thousand documents keep the
tree at 3-4 levels. `InMemoryIndex` is untouched — still the fast,
non-durable fake for tests that don't want real I/O.

## 10.1 Entry format (`leaf.rs`): fixed 26 bytes — unchanged, as forecast
A leaf entry is still `[DocId: 16 bytes][RecordLocation: 8-byte page +
2-byte slot]`, exactly as `LinearIndex` used it. §7.4 predicted this
format would survive a real B-tree unchanged, since only the node
hierarchy above leaves was expected to change — that held.

## 10.2 Branch pages: a new page type, no changes to `SlottedPage`
`PageType::IndexBranch` is new; branch cells (`branch.rs`) are
`(separator_key: DocId, child: PageId)` — 24 fixed bytes, the same
fixed-size property leaf entries have (see §10.4 for why that matters).
No change to `SlottedPage` itself was needed — branch pages are just
slotted pages whose cells mean something else, the same reuse `Catalog`/
`Data`/`IndexLeaf` already share.

One field does double duty: `SlottedPage::next_page` means "next leaf
sibling" (the scan chain) on an `IndexLeaf` page, but "rightmost child
pointer" on an `IndexBranch` page — the classic "n separator keys route
to n+1 children" scheme, where a branch's cells `[(k0,c0), (k1,c1)]` plus
rightmost `R` means `c0` handles keys `< k0`, `c1` handles `k0 <= keys <
k1`, and `R` handles `keys >= k1`. No new page field, just a
page-type-dependent meaning for an existing one.

## 10.3 The root page id is permanent; its role isn't
`CollectionMeta.index_root` is handed out once and never reassigned. A
tree still needs to grow taller over time — root starts as a leaf,
becomes a branch, later a taller branch. When the root overflows and
splits, its current content (already rewritten in place as the "left
half" by the split that bubbled up to it) is relocated to a *freshly
allocated* page, and the root's own page id is overwritten with a new
branch page pointing at (separator → relocated page) with rightmost = the
split's new sibling (`grow_new_root`). Every other split just allocates a
fresh page for the new sibling and leaves the original id as "left" — the
root split is the one case where the *original content* has to move,
because its id is the one thing that can't.

## 10.4 Splitting: fixed-size entries make it foolproof
Leaf and branch entries are always exactly 26 or 24 bytes. So if `n+1`
entries overflow a page that held `n` just fine, splitting those `n+1`
into two roughly-equal halves always leaves each half fitting — a
guarantee from the arithmetic (`(n+1)/2 <= n` for `n >= 1`), not something
asserted defensively per split. Leaf and branch splits differ in one way,
matching standard B-tree/B+-tree semantics: a **leaf split** copies the
smallest key of the right half up as the separator (leaves hold real
data, so nothing is removed); a **branch split** promotes *and removes*
the middle entry's key (branch keys are pure routing information, never
duplicated).

## 10.5 Deliberately not built: rebalancing on delete
`remove` descends to the right leaf and tombstones the cell — no merging
or redistributing underfull nodes afterward. This isn't a new gap the
B-tree opened; it matches the rest of the codebase's existing stance
(tombstones never reclaim space anywhere, no compaction pass exists yet).

## 10.6 A side effect: `scan` is now sorted
Leaves chain left-to-right in key order, so `scan()` — used by every
`Collection::find` — returns documents in ascending `_id` order for free,
where `LinearIndex` returned arbitrary insertion order. Tested
(`scan_after_many_inserts_is_sorted_and_complete`, 2000 entries inserted
in shuffled order) but not yet promised as part of the `Index` trait's
contract — worth revisiting if something later wants to rely on it.

## 10.7 Two bugs surfaced while building the original `LinearIndex`
(kept for history — both fixes carried forward unchanged into
`BTreeIndex`, since they live in `SlottedPage`/`Catalog`, not the index
itself.)
- `SlottedPage::has_room_for(data_len)` replaces a hand-rolled
  `free_space() < data_len` check that forgot to account for the 4-byte
  slot-directory entry every cell also costs. `catalog.rs`'s
  `create_collection` had the identical bug already (never triggered,
  since no test happened to land in that 4-byte gap). Both now use
  `has_room_for`, which encapsulates the correct comparison instead of
  requiring every caller to know about `SlottedPage`'s internal
  `SLOT_LEN`.
- `create_collection` allocated `index_root` but never wrote anything to
  it (see updated §9.5) — the index's first read of `root` would have
  misread whatever bytes were already there as a bogus page. Fixed by
  initializing it as an empty `IndexLeaf` page at allocation time.
