# 64. Shared pages (`storage/page.rs`, `storage/cache.rs`, `storage/file.rs`, `storage/slotted.rs`)

Readers didn't run in parallel (§58.3): every page read took the page
cache's mutex and, holding it, allocated a `Vec` and copied 8 KB into
it. Four readers spent four times as long waiting for that lock as
working. The copy was there because `PageStore::read_page` handed out
an owned page that `SlottedPage` could change, and it was the biggest
part of a lookup even with one reader (§50.2).

Now the cache hands out the page it holds, shared, and a page is
copied only when someone changes it.

```rust
let page = store.read_page(id)?;          // an Arc cloned: no allocation, no copy
let mut leaf = SlottedPage::from_bytes(page)?;
leaf.insert_cell_at(slot, &entry);        // copies the page first: the cache's stays as it was
store.write_page(id, &leaf.into_page())?;
```

## 64.1 `Page`
A new type, `Page`: the page's bytes behind an `Arc<[u8]>`, readable as
a `&[u8]` (`Deref`). Cloning it is an atomic increment. Changing it
goes through `Page::make_mut`, which is `Arc::make_mut`: if anyone else
holds the page, the bytes are copied first and the change goes to the
copy. So a page behaves as the copy a read used to make: whoever holds
it sees it as it was when they got it, whatever happens to the cache's
page or anyone else's afterwards.

- **`PageStore::read_page` and `try_read_page` return a `Page`**
  instead of a `Vec<u8>`. `write_page` still takes a `&[u8]`.
- **The cache holds `Page`s.** `get` clones one, under the lock; the
  lock is now held for a hash lookup, a flag and an atomic increment.
  `put` replaces the slot's page instead of copying into it, so a
  reader still holding the old one keeps it.
- **Staged and committed pages are `Page`s too.** A batch reading what
  it wrote copies nothing, and a checkpoint moves the committed pages
  into the cache instead of copying each one.
- **`SlottedPage` holds a `Page`.** Reading its cells copies nothing;
  every method that changes it goes through `bytes_mut`, which is
  `make_mut`. `into_bytes` became `into_page`.
- **`MemoryStore`** (compaction's image, §41) still copies on read: it
  has no cache to share with, and holds its pages as `Vec`s.

The cost stays where it was for writes: a change used to copy on the
read and again on `write_page`; now it copies on the first change and
on `write_page`. A `write_page` taking the `Page` itself would save the
second copy; left for when writes are measured again.

Rejected:
- **`DerefMut` on `Page`, doing `make_mut` behind `page[i] = x`:**
  every write to a page would then hide a possible 8 KB copy behind an
  index expression. `make_mut` names it, as `Cow::to_mut` does.
- **Handing out a guard that borrows from the cache** (`&[u8]` tied to
  the lock): the lock would be held for as long as the caller reads
  the page, which is what made readers wait in the first place.
- **Sharding the cache first**, several mutexes chosen by page id:
  every lookup starts at the same root and inner pages, so readers
  would still queue on the shards that hold them, each still copying
  8 KB under the lock.

## 64.2 Measured
M1 Pro, `reader_wait` (§58) with trunkdb alone, 3 s per case, the
build before this change and after it, run one after the other. With no
writer:

| readers | before: reads in 3 s | p50 | after: reads in 3 s | p50 |
|---:|---:|---:|---:|---:|
| 1 | 732,869 | 3.9 µs | 947,211 | 2.9 µs |
| 2 | 702,943 | 7.0 µs | 1,579,400 | 3.0 µs |
| 4 | 751,869 | 11.7 µs | 1,748,917 | 4.0 µs |

- **One reader is 30% faster:** the copy was most of a lookup (§50.2).
- **Two readers now do 1.7 times the work of one**, where before they
  did less than one did alone.
- **Four do only 10% more than two.** A profile of four (macOS
  `sample`, as in §58.3) still has more samples waiting in
  `__psynch_mutexwait` (6,651) than in any work, and every one of them
  under `FileStore::read_current`: the cache's mutex again. It is held
  for a hash lookup now, but four threads take it millions of times a
  second, and each time two meet, macOS puts one to sleep in the
  kernel. §65 takes that on.
- **Readers next to a writer** gain the same: batches of 1,000 back to
  back, 963,018 reads with four readers before, 2,713,505 after. Single
  inserts back to back still starve them (§58.3): the write lock, not
  the cache.

The main benchmark (§48), one thread, trunkdb before and after, two
runs each: finds through an index 10–20% faster (`tenant == x`, 2.41
and 2.34 ms before, 2.16 and 1.99 ms after; "oldest 20", 47–48 µs
before, 42–45 µs after); `get` by id within the noise (7.9 and 5.9 µs,
6.2 and 6.5 µs); the unindexed scan and the writes unchanged.

## 64.3 Limits
- **The cache's mutex is still the limit past two readers** (64.2):
  the next step, a read lock for lookups, is §65.
- **`SlottedPage::from_bytes` checks a page's layout on every read**
  (§54), and in the profile of four readers it costs as much as
  reading the cells. A page could be checked once, when it comes into
  the cache; not done here.
- **A page held outlives its eviction.** A reader holding a `Page` the
  cache has since replaced or evicted keeps it in memory until it lets
  go. `cache_size` bounds the cache, not those; a read holds its pages
  for one call.
- **Writes still copy twice** (64.1).
- **Snapshots (§57) fit this.** A page held by a reader stays as it was
  whatever a writer does next, which is what an in-memory snapshot
  needs per page. What's missing is the other half: finding, for a
  given snapshot, which version of a page to read.

## 64.4 Tests
- `page.rs`: a clone shares the bytes until one side changes them,
  then only that side has the change; a page no one else holds is
  changed where it is.
- `slotted.rs`: a `SlottedPage` made from another's bytes, then changed
  (a cell added, one deleted, the next page set), leaves those bytes as
  they were.
- `file.rs`:
  - two reads of a cached page share it; changing one read changes no
    later read; a page staged, committed and checkpointed is the same
    bytes in memory at each step, and a reader holding the page from
    before keeps its old contents;
  - a page committed by one batch and cut off by the next is not cached
    by the checkpoint that writes both (a gap from §51 the mutations
    below found).
- Every other test reads pages through the new type, unchanged.
- Checked by breaking it on purpose, six ways. Each of these fails a
  test:
  - the cache handing out a copy; `put` copying into the slot;
  - a checkpoint copying the pages it caches; committed pages copied
    on read;
  - a page read from the file not cached; a page a later batch cut off
    cached by the checkpoint.
