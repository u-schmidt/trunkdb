# 129. A slot directory past the page's end (`storage/slotted.rs`)

A fix to §54 and §55. `SlottedPage::check_layout` checks a page's
layout before anything reads its cells. It made sure that the slot
directory ends before the cells start. Only after reading every slot
did it check that the cells start inside the page.

So a page with both past its end got through the first check and was
then read past its buffer: a panic, not `InvalidData`. The review that
found it (2026-10-26) used a data page with 2,048 slots, all empty, and
the cells said to start at 9,000:
- The directory ends at 13 + 2,048 × 4 = 8,205, before 9,000, so the
  first check passed.
- The page is 8,188 bytes, so the loop over the slots reached the
  buffer's end at slot 2,043 and panicked.

Any page with 2,044 slots or more, and the cells said to start after
its directory, does the same.

A bit flip on disk fails the page's checksum first (§40), so this
takes a page whose checksum matches its bad layout. That is the kind
of page §55 fuzzes for: a bug, a tool writing the file, or someone
building one on purpose. `open_raw`, which seals any bytes with valid
checksums, could have found it but hadn't. An application should get
`InvalidData` from such a file, never a panic.

## 129.1 The fix
The check that the cells start inside the page (55's) moves before
the loops over the slots. Then:
- the cells start at most at the page's end;
- the directory ends at most where the cells start;
- so every slot the loops read is inside the page.

Nothing else changes. A page that passed before passes now, and one
refused before is still refused. Where a page has two faults, the
message may name another one: the cells' start, before the directory or
a slot. `from_bytes_rejects_offsets_past_page_end` gave its page a cell
start of 60,000 only to reach its slot; it now starts the cells at the
page's end, so its slot is still what's refused.

## 129.2 Tests
- `a_directory_past_the_page_is_refused_before_it_is_read`
  (`slotted.rs`):
  - The most slots that fit, 2,043 empty ones, before cells at the
    page's end: still valid.
  - One slot more, with the cells at the end, one byte past it, or
    four past it: refused.
  - The review's page, and both values at `u16::MAX`: refused.
- `a_page_whose_directory_runs_past_its_end_fails_the_open`
  (`database.rs`): a database whose catalog page has the review's
  shape, its checksum matching. Both a read-only open and a writable
  open fail with `InvalidData`. The directory is emptied first, as in
  the review's page: a live slot is refused before the loop gets far
  enough to panic.
- **The fuzz corpus:** `seed_corpus` writes the same file as an
  `open_raw` input, `directory_past_the_end`.

The test-only `checksum` export, behind `fuzzing` until now, is also
there for tests.

## 129.3 Mutations
| Mutation | Caught by |
|---|---|
| The check back after the loops: the code before | both new tests |
| The check removed | both, and §66's `a_checked_page_is_not_checked_again_until_it_changes` |
| `>=` for `>`: an empty page refused | all three |

## 129.4 Limits
Overlapping cells are still not detected (§54).
