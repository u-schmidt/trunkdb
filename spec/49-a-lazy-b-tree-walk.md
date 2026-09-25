# 49. A lazy B-tree walk (`index/btree.rs`, `collection.rs`)

Real and measured: reading an index in sort order now reads one leaf
page at a time and stops at the limit. "The oldest 20 queued tasks"
reads the pages those 20 are on, not every queued task's entry. In the
benchmark (§48) that query went from 19 ms to about 0.1 ms with
100,000 documents, and the descending one is as fast.

## 49.1 `BTreeIndex::walk`
`walk(store, range, backward)` returns a `Walk`, an iterator of
`io::Result<(key, location)>` that reads a leaf only when the previous
one is used up. An I/O error ends it after it's handed out.
- **Forward**, it descends to the leaf where `range.start` would be and
  follows the leaves' sibling links (§10) until a key reaches
  `range.end`.
- **Backward**, it descends towards `range.end` (or along the last
  children) and keeps the branch pages above the current leaf in
  memory: each as its list of children, with the one it's in. The
  previous leaf is one step left in the lowest branch that has a child
  to the left, then down the last children. That is one page read per
  leaf in the common case, and at most the tree's height where it
  crosses a branch boundary.
- A leaf left **empty** by removals (§10: leaves aren't merged) is
  skipped either way.
- Each leaf's entries in the range are copied out, so a `Walk` borrows
  only the store and holds no page past its leaf.

`range` is now the forward walk collected, so the eager callers
(`candidate_entries`, `check`, `compact`, ...) keep their `Vec` and
behave as before. The `Index` trait keeps `range` only; `InMemoryIndex`
has no pages to be lazy about.

Rejected:
- **Backward leaf links**, as the roadmap first said (§34.2). A `prev`
  pointer in every leaf would change the page layout and the file
  format, and every split would have to update a third page. The path
  of branches costs a few `Vec`s of page ids in memory and no format
  change.
- **Walking backward by searching again** for the key before the
  current leaf's first. An empty leaf has no first key to search from.
- **A walk that holds the read lock itself.** It borrows the store from
  the caller, which holds the lock for the whole `find` (§27), as
  before.

## 49.2 The sorted read on top of it
`read_in_index_order` consumes the walk: entries are grouped as they
come, by the values of the served sort keys (§47.3), and each finished
group is read and checked at once. When `limit` documents match, the
walk is dropped, and the rest of the range is never read. `Desc` walks
backward; the group sorting of §47.3 is gone.

Two things the old eager version got from sorting whole groups had to
be kept:
- **Ties in id order.** A backward walk hands out equal values last id
  first, so every group is sorted by id before it's read. It usually
  already is, and it's at most one group.
- **Unordered values last, in both directions.** In a compound index
  they carry `TAG_OTHER`, which sorts after every other tag (§43.2), so
  a forward walk has them last already. A backward walk has them
  *first* among the groups that share the values before them.
  `Held` keeps such a group back until the walk leaves those shared
  values, then hands it out after them, sorted as §47.3 sorted all
  groups. The stack holds one level per served key that has
  unordered values, so the innermost level goes first. Unordered
  values are rare (arrays, objects, NaN in an indexed field), and so
  are held groups. A one-field index holds no unordered values (§34.1)
  and never holds a group back.

## 49.3 Measured
With 100,000 documents, trunkdb alone (§48.3 has the others):

| | before | after, four runs |
|---|---:|---:|
| status == x, oldest 20 | 18,465–19,247 µs | 92–126 µs |
| status == x, newest 20 | — | 93–180 µs |

That's about 150–200× faster. At this size the spread between runs is
large; the same runs put SQLite, redb and sled at 15–23 µs. Everything
else stayed within the run-to-run spread, with one outlier: "created in
a range" once took 6.8 ms, then 4.5 and 4.8 ms again. `range` is now
the walk collected, so it was worth checking. The benchmark now
also measures "newest 20", for the backward walk; SQLite, redb and sled
take 15–23 µs for either. The rest of trunkdb's time is about 25 page
reads (the index's path and leaf, and 20 data pages), each a `pread`, a
checksum and an allocation without a page cache (§48.4).

## 49.4 Tests
- `index/btree.rs`:
  - 6,000 random keys of varying length, a third of them removed again
    including a run of 1,500 neighbours, so some leaves are empty. Then
    300 random ranges (bounded on both, one or no sides), walked
    forward and backward, must equal a `BTreeMap`'s entries in its
    order or the reverse; `range` too.
  - Page reads, counted by a wrapper around the store, on 20,000 keys:
    - the first 20 of everything, or of a middle range, either way: at
      most a descent plus one leaf;
    - a middle range of 1,000 keys walked whole: its leaves plus a
      descent, not the leaves before or after it;
    - the whole tree: every leaf.
- `collection.rs`: every randomized sorted-read test from §34, §43, §44
  and §47 now runs on the walk, `Desc` included, over arrays, NaN, huge
  numbers and cut strings in compound keys. They passed unchanged.
- Checked by breaking it on purpose, eleven ways. Each of these fails
  a test:
  - a backward walk starting from the last leaf instead of the one for
    `range.end` (correct, but reads everything after it);
  - a backward walk stopping at the first leaf of a branch instead of
    climbing further;
  - a walk not stopping at the far end of its range, either way (correct
    results, too many pages);
  - a forward walk handing out a leaf backward;
  - the path of branches not kept;
  - a `Desc` sort walking forward;
  - groups not put in id order;
  - held groups never held, released at once, or held until the end.

  One passed at first: stopping instead of climbing. Every test tree
  had one level of branches, where the two are the same. A test with
  600-byte keys builds a tree four levels tall and catches it now.

## 49.5 Limits
- **Only the sorted read is lazy.** An index range for a filter without
  a sort is still collected whole (`candidate_entries`), since every
  document in it is read anyway. A cursor still starts from that list.
- **Each leaf's entries are copied** before they're handed out, which
  costs an allocation per key.
