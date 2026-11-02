# 152. A page changed through `SlottedPage` keeps its mark (`storage/slotted.rs`, `storage/page.rs`)

§66 checks a page's layout once, as it goes into the cache, and marks
it, so a read of a cached page isn't checked again. A change took the
mark away (`Page::make_mut`), and the page stayed unchecked until its
batch committed. So every read of a page the batch had changed checked
it again: the slot directory, every slot's bounds, and for an index
page every cell's length. §150's profile gave that 7% of a batched
insert's CPU; with §151 it was about a tenth of the samples.

The check exists for bytes from the file, which may be damaged (§55).
A page changed through `SlottedPage` was valid when it was read or
built, and every change it makes keeps it so: a cell goes only where
`free_space` says it fits, a slot is rewritten only with a cell's own
offset and length, a compaction repacks the cells it has. The one
thing the check refuses that a change could do is a cell too short for
an index entry on an index page (§101), which no caller writes but
`insert_cell` would take.

## 152.1 The change
- **`SlottedPage::valid`:** whether the bytes pass `check_layout`. True
  from `from_bytes`, which checked them or found them marked, and from
  `new` with a slotted page type; set false by `write_slot`, which every
  cell's slot is written through, for a cell shorter than the page's
  type allows. The shortest cell per type is `min_cell`, which
  `check_layout` now uses too.
- **`into_page` marks a valid page** (`Page::known_checked`), so it
  goes into the batch, and to its readers, marked: a read of it skips
  the check as a read of the cache does.
- **A debug build checks it all the same:** `into_page` asserts that a
  page it marks passes `check_layout`, so the tests, run in debug, see
  every change's result checked, as every read checked it before.
- **Changed bytes still lose the mark:** `Page::make_mut` outside
  `SlottedPage` (`check`'s repairs, the header, overflow pages) leaves
  a page unchecked, and the commit checks it, as before.

No file format, no API, no change to what any call returns.

Counted with the benchmark's inserts (§48): 207 layout checks in
100,000 documents, the pages read from the file, where every read of a
page the batch had changed was one before.

## 152.2 Measured
On the machine of §123, the build before the change (`5957621`) and
after, one after the other.

Inserts alone with flushes skipped (§150.2's program), two rounds of
100,000 in each of three runs:

| | before | after |
|---|---:|---:|
| inserts | 60.4–68.6k/s, median 67.1k/s | 70.1–74.9k/s, median 70.8k/s |

1.06×, less than the tenth of the samples the check had. Likely why:
the check was the first to read a page's slot directory into the CPU's
cache, and what reads the page next, the binary search (§150), now
pays for that instead.

The §48 benchmark, trunkdb alone, two rounds of three runs each, the
first on a busier machine:

| | before | after |
|---|---|---|
| insert, 1000 per commit | 36k/s, 27k/s, 38k/s; 36k/s, 41k/s, 39k/s | 35k/s, 39k/s, 41k/s; 39k/s, 43k/s, 43k/s |
| update, 1000 per commit | 12k/s, 13k/s, 12k/s; 14k/s, 14k/s, 14k/s | 12k/s, 13k/s, 13k/s; 15k/s, 14k/s, 15k/s |
| delete, 1000 per commit | 28k/s, 27k/s, 28k/s; 28k/s, 28k/s, 30k/s | 28k/s, 26k/s, 29k/s; 30k/s, 31k/s, 30k/s |
| compact | 0.85 s, 0.90 s, 0.77 s; 0.74 s, 0.73 s, 0.74 s | 0.73 s, 0.63 s, 0.65 s; 0.64 s, 0.83 s, 0.65 s |
| get by id | 1.7–3.6 µs | 1.8–2.7 µs |

- **Inserts about 1.1×,** 37.5k/s against 41k/s by the medians of all
  six; the quieter round alone, 39k/s against 43k/s.
- **Compaction 1.15×,** 0.75 s against 0.65 s: it reads back the
  leaves and data pages it holds in memory (§108) as it fills them,
  and each read checked the page again.
- **Deletes about 1.05×, updates within noise,** reads unchanged.

The full benchmark, all four stores, three runs, on the quieter
machine: inserts 43k/s against SQLite's 39k/s, 1.1× (0.95× in §151.2);
updates 14k/s against 15k/s; deletes 30k/s against 20k/s; compaction
0.63 s against 0.16 s. SQLite's own inserts were 41k/s, 40k/s and 39k/s
on the days of §150 to §152, so "level or a little ahead" is what
these runs say, not more. The README's and DESIGN's tables are these
runs.

## 152.3 Rejected
- **Checking a batch's pages once, at commit, instead of on every
  read:** what the roadmap had. The commit's check costs one per page
  a batch changed, about 800 a batch here, and guards against nothing
  `SlottedPage` lets happen; a debug build checks every change instead.
- **Trusting every page in the batch:** a page the batch wrote as raw
  bytes, an overflow page or one just allocated, may be read as a
  slotted page through a damaged link (§55). Checked, it is refused as
  damage; trusted, an entry decoder could panic on it.
- **Refusing a short cell in `insert_cell`:** the B-tree never writes
  one, but the page's own tests use short cells on index page types;
  noting it keeps every caller as it is.

## 152.4 Tests
- `a_checked_page_is_not_checked_again_until_it_changes`: a page built
  through `SlottedPage` comes marked; one read, changed (an insert, a
  delete, a compaction) and written back stays marked; a change to its
  bytes takes the mark away; on a leaf and on a branch, a cell of the
  shortest length is marked, one byte shorter isn't, and its page is
  refused when read; a page of a type that isn't slotted isn't marked.
- `cached_pages_come_with_their_layout_checked`: a page built through
  `SlottedPage` is marked while staged; one written as raw bytes is
  not, until its batch commits, as before.
- The full test suite, run in debug, where every page `into_page`
  marks is checked: nothing failed it.

## 152.5 Mutations
Each run against the library's tests:

- a short index cell not noted, or a tombstone taken for one: caught;
- a cell of exactly the shortest length taken as too short: caught;
- a new page valid whatever its type: caught;
- a page read taken as not valid: caught (nothing is marked);
- `into_page` never marking, or marking an invalid page: caught;
- the shortest cell of a leaf, or of a branch, not known: caught,
  `a_cell_too_short_for_its_entry_is_an_error` (§55) among them.

Not run: the debug build's check left out. It changes nothing a test
can see unless a change through `SlottedPage` breaks a layout, which is
what it is there to find.
