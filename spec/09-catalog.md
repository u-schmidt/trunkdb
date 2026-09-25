# 9. Catalog (`catalog.rs`)

*Updated by §28: every catalog cell starts with a kind byte, and the
catalog also records secondary indexes (§28.5).*

Real and tested: `Catalog` maps collection name → `CollectionMeta { index_root:
PageId }`, backed by a chain of `SlottedPage(Catalog)` pages starting at the
fixed, well-known page 1. `CollectionMeta` first held just the one
field — no `current_data_page` (deferred: v0 allocated a fresh data page
per document; it did prove wasteful, and §20 added the field) — and no
`name` (redundant — it's already the `HashMap` key, and only needs to
exist in the *on-disk cell*, not the in-memory struct).

## 9.1 `PageStore::try_read_page`: `Option`, not `Err`, for "not allocated yet"
Added because "this page hasn't been created yet" is an expected, ordinary
outcome (a fresh database) — using `Err` for it conflated *failure* with
*absence*. `try_read_page` returns `Ok(None)` for a never-allocated page id
and `Ok(Some(bytes))` otherwise; genuine I/O errors still surface as `Err`.
It only distinguishes "never allocated" from "allocated" — it does not
check free-list membership, so it isn't safe in general for a page that
*could* have been freed. Fine for the catalog root specifically, since
nothing ever frees it.

## 9.2 Corruption check: page-type tag, not slot count
A freed page's bytes coincidentally read as "0 slots" at the same offset a
genuinely-empty `SlottedPage(Catalog)` would — `free_page` only writes the
type tag and free-list-next pointer, leaving the rest zeroed, same as a
fresh page. Slot count alone can't tell them apart. The page's type tag
can: `Catalog::load` checks `page.page_type() == PageType::Catalog`
after a successful read, and treats a mismatch as corruption (a real
`Err`), not as "must be empty." This reuses the page-type-tag mechanism
built in §7.3, rather than requiring new free-list-walking machinery.

## 9.3 Bootstrap-ordering assumption: an `assert`, not a stronger type
`Catalog::load` assumes the catalog page is the *first* page ever
allocated in a fresh file (so `allocate_page()` is guaranteed to hand back
id 1). This is enforced with `assert_eq!`, not a typestate-style API that
would make violating it a compile error. Typestate was considered and
rejected: the assumption's entire blast radius is a few lines inside one
function (`Catalog::load` itself), never touched by external callers — the
cost of a second type and a consuming transition method wasn't worth it
for a risk that's local, not spread across a broad API surface. The assert
still turns a silent, corrupting violation into a loud, immediate panic.

## 9.4 Naming: `load`, not `open`
Considered `open` (to signal "creates if missing, reads if present,"
matching `FileStore::open`'s own behavior) but kept `load` — a deliberate
judgment call, not an oversight: "open" fits establishing a live handle to
an external resource (a database, a file); "load" fits populating a
structure from a source, which is what a catalog fundamentally is, even
though this one instance also happens to create-on-miss. Compensated by
being explicit in `load`'s doc comment that it creates the catalog page
when necessary, since the name alone hints at that less strongly than
"open" would have.

## 9.5 `create_collection`
Allocates a page to serve as the new collection's `index_root`, encodes
`(name, CollectionMeta)` into cell bytes (fixed 8-byte `index_root` first,
then the name's raw UTF-8 bytes last — no length prefix needed, since
`SlottedPage`'s own slot directory already tracks each cell's length),
walks the catalog page chain for room (extending it via `next_page` if
every page is full), writes the cell, and updates the in-memory map to
match. The allocated `index_root` page is also written immediately as an
empty `SlottedPage(IndexLeaf)` — added after the index (§10) turned out
to assume a readable, correctly-typed page at `root` from its very first
call, which a merely-*allocated*-but-never-written page isn't. Names are
limited to 255 bytes since §22.3.

`SlottedPage` gained `#[derive(Clone)]` for this: writing a page's bytes to
disk via `into_bytes(self)` consumes it, but the in-memory value is
sometimes still needed afterward (e.g. a freshly-allocated chained page,
written once immediately, then kept as the page subsequent code continues
operating on) — cloning (a cheap `Vec<u8>` copy) resolves that without
resorting to re-parsing bytes back into a second `SlottedPage`.
