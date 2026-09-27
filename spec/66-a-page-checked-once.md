# 66. A page checked once (`storage/page.rs`, `storage/slotted.rs`, `storage/file.rs`)

Every `SlottedPage::from_bytes` checked the page's layout (§54): the
type, the directory against the cells, every slot inside the page. A
lookup reads three or four pages, and checked each one every time,
though a page in the cache hadn't changed since it was last checked. In
the profile of four readers after §64 the check cost as much as
reading the cells (§64.3).

Now a page is checked once, when it goes into the cache or among the
committed pages, and every read of it after carries the result.

## 66.1 The mark
`Page` carries `layout_checked`, a plain `bool` next to its `Arc`:

- **`Page::checked`** checks the layout (`SlottedPage::check_layout`,
  the checks `from_bytes` made, split out) and marks the page if it
  passes. A page that isn't slotted (overflow, free, the header) or
  that fails stays unmarked; `from_bytes` then checks it and reports
  what's wrong, as before. A page already marked isn't checked again.
- **`FileStore` marks pages where they enter memory for good:** a page
  read from the file, as it goes into the cache; a batch's pages when
  it commits, as they become the committed pages that reads use until
  the checkpoint (§51), which then moves them to the cache marked; and
  a page written outside a batch, which only tests do.
- **Every clone the cache hands out copies the mark**, and
  `from_bytes` skips the check for a marked page.
- **`make_mut` takes the mark away**, from the page it changes: the
  bytes are someone else's now, or changed. A batch's own pages, read
  back while it runs, are checked on every read, as before.

The mark sits in the handle, not with the shared bytes. It's set before
the page is shared (as it goes into the cache), so no one needs to see
it change, and reading it touches no memory another core writes.

Rejected:
- **The mark with the shared bytes, an `AtomicBool` in the `Arc`**, set
  by the first `from_bytes` that checks a page: the first version. It
  made one reader 19% faster, but eight 18% slower, in three runs of
  each. The bytes then sat behind a second pointer, in the same
  allocation as the reference count every reader changes, so every
  read of a page's bytes went through a cache line other cores kept
  taking. Aligning that allocation to a cache line of its own didn't
  help.
- **Checking only as a page comes in from the file:** after a load, the
  pages the last batches wrote are read from the committed pages until
  the next checkpoint, often the B-tree's inner pages. Without the mark
  at commit, one reader gained 8% instead of 14%.
- **Trusting `SlottedPage`'s own changes and keeping the mark through
  `make_mut`:** the methods keep the layout valid, but `make_mut` also
  serves code that writes raw bytes; a check at commit costs one pass
  per changed page, next to the page image the WAL writes for it.

## 66.2 Measured
M1 Pro, 3 s per case, this change against §65, run one after the other,
twice. `reader_wait` (§58), reads in 3 s, no writer:

| readers | §65 | §66 |
|---:|---:|---:|
| 1 | 999,623; 959,613 | 1,137,104; 1,103,747 |
| 4 | 3,136,621; 3,043,301 | 3,406,844; 3,436,209 |
| 8 | 2,865,608; 3,126,001 | 3,061,469; 3,112,922 |

One reader 14% faster, four 11%, eight the same. The main benchmark
(§48), one thread: `get` by id 5.2 and 5.3 µs against 6.1 and 6.5 µs;
finds, the scan and the writes unchanged. A first comparison showed
the scan 6% slower; with the order of the two builds reversed, it was
the same, 157 ms both: whichever ran second was slower.

## 66.3 Limits
- **A damaged page in memory isn't found again.** A page is checked
  when it comes in; memory that changes under it afterwards (a bad RAM
  bit) isn't caught by the next read, as a page damaged on disk after
  it was cached isn't (§50.3). `check` still reads the file.
- **A batch's own pages are checked on every read** until it commits.
- **Overlapping cells still aren't detected** (§54.2).

## 66.4 Tests
- `page.rs`: a clone keeps the mark; a change, copied or in place,
  takes it from the page changed and leaves the original's.
- `slotted.rs`: a page marked by `checked` isn't checked again, and
  loses the mark when it changes; a damaged page isn't marked, and
  marked anyway (only a test can), `from_bytes` takes it as it is; a
  page that isn't slotted isn't marked; cells starting at the page's
  end are an empty page, one byte past it damage.
- `file.rs`: a slotted page is marked once committed, once cached by
  the checkpoint, when written outside a batch, and when read from the
  file after a reopen; a staged page and a page that isn't slotted are
  not.
- Checked by breaking it on purpose, eight ways. Each of these fails a
  test:
  - the mark ignored; a page marked without a check; a change keeping
    the mark;
  - pages not marked at commit, when read from the file, or when
    written outside a batch;
  - cells starting past the page's end, or inside the directory,
    passing the check.
