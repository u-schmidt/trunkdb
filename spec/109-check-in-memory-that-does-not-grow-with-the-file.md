# 109. `check` in memory that doesn't grow with the file (`check.rs`)

§108.8 found it: `check` took 1.7–1.8 GB for a file of 1.1 GB, more
than the file. It held:
- every document of a collection, decoded, to check the indexes
  against: nearly all of it;
- for each index, two sets of all its keys, the ones it holds and the
  ones the documents call for, compared as sets;
- every primary-index entry in a list, a name for every page's owner, a
  set of data pages.

Now it holds a few megabytes besides what it reads through the page
cache, whatever the file's size. Its problems are what they were.

## 109.1 Documents one at a time
`check_collection` walks the primary index (`BTreeIndex::walk`, §49)
instead of collecting it, and reads each document, checks it, claims
its pages and lets it go. What it keeps of the document is a count per
index: how many entries the document calls for (`index_keys`).

The walk's order is checked as it goes (`Order`): the first key that
isn't after the one before is the problem it was, and the walk goes on.

## 109.2 An index entry by entry
An index can't be compared with its documents as two sets any more. So
`check_index` walks it and checks each entry on its own
(`entry_is_right`): the primary index has the entry's document where
the entry says it is, and the document there, under its own id, gives
the entry's key.

Right entries are distinct, as the index is in order and every key ends
with its document's id. Each is one the documents call for. So when as
many entries are right as the documents call for, the index holds
exactly those, and nothing is missing. A wrong entry is reported as it
is found, as before ("has an entry for document … that doesn't match
it").

**What's missing** can't be named by counting. When fewer entries are
right than the documents call for, or the index isn't in order, a
second pass (`name_missing`) walks the documents again and looks up
each key they give: each one the index lacks, or holds for another
place, is "lacks document …", as before. A consistent index never
needs it.

## 109.3 Unique values among neighbors
A unique index's equal values have equal value parts (§28.1), and keys
sort by value part first, so their entries are neighbors. The walk keeps
the run of right entries with one value part, ids and places only, and
compares the run's documents by value at its end, as compaction does
(§41, `refuse_duplicates`). A run is almost always one entry.

The run sees only what the index holds. A document whose value changed
past the index (its entry wrong, or missing) can share a value with
another without being in its run. The index is then not all it should
be, so the second pass runs, and it compares each document's values
with those of the entries under the same value part. A pair of
documents is reported once, whichever pass finds it.

## 109.4 Four bytes a page
Each page's owner is a number into a list of names, in a `Vec` with a
slot for every page: 4 bytes a page, a 2,048th of the file, where a
`BTreeMap` held a name for each. A data page is claimed by each
document on it, which a set of them did before: such a claim may come
again from the same owner (`claim_shared`), any other may not.

## 109.5 Measured
The compacted file of §108: 1,053 MiB, 258,500 documents of 2–6 KB, one
index. Release build, each run its own process, peak memory footprint
as macOS reports it.

| | Before | Now |
|---|---|---|
| `check`, default 256 MiB cache | 1.73–1.78 GB, 2.4–3.0 s | 316–321 MB, 2.2–2.7 s |
| `check`, no page cache | 1.32 GB, 2.5–2.8 s | 3 MB, 4.9–5.0 s |

With the cache, what's left is the cache itself, which a read of the
whole file fills. Without it, each document is read from the file once,
and once more for each index: twice as long here. The default keeps the
pages an index's entries lead back to, mostly, and is no slower than
before.

## 109.6 Rejected
- **Each index's expected keys sorted in memory**, and merged with the
  index's walk. A document read once in all, but memory growing with
  the largest index, as compaction's sort still does (§108.8).
- **Sorting them in a file**, as §108 builds an image there: bounded,
  but more code and more writes than reading documents again, for a
  diagnostic that blocks nobody (it reads a snapshot, §80).
- **A checksum per index**, summed over its entries and over the
  documents' keys, compared at the end. Order-free and cheap, but it
  names nothing: a second pass would still be needed for every problem.

## 109.7 Tests
`check.rs`, besides its earlier tests, which pass unchanged:
- A consistent file is checked reading each document once, and once
  more per index: 600 reads for 200 documents and two indexes. The
  second pass doesn't run.
- An entry with its document's key, but at another document's place:
  it doesn't match, and its document lacks an entry. Two problems,
  nothing else.
- The same, at a copy of the document the primary index doesn't have,
  with its id and its values: two problems, as before.
- An index entry changed on its leaf to sort after the next one: the
  index is out of order there, the entry doesn't match, and the
  document is named as lacking one.
- Two large documents of one collection on one chain of overflow pages:
  each of its pages belongs to both. A data page is claimed by the same
  owner many times; an overflow page may not be.
- A duplicate in a unique index that holds both documents rightly, and
  another document's entry removed so that the second pass runs too:
  the duplicate is reported once, the missing entry once, nothing else.

Every randomized test checks the whole file with `check` (§39.3), so
they all run through the new code too.

## 109.8 Mutations
Each caught: an entry's place not compared with the primary index's (1
test); its key not compared with its document's keys (2); the second
pass never run (3), always run (1), run only for a broken order (3),
and run without comparing unique values (1); runs of equal value parts
not compared (1), not ended at a new value part (1), and the last one
left out (1); a data page claimable once (9); an overflow page
claimable twice by its owner (1); leaks not looked for (1); the order
never found broken (1); no entries counted as called for (3); the
current data page not claimed (2), nor overflow pages (1); the entries
of unreadable documents checked (1); a pair of duplicates reported by
both passes (1).

One is hardly to be seen: an entry's document not compared by id. The
entry's place is compared with where the primary index has its
document, and the primary index's walk compares every place's document
with its id. Only where that walk has already reported a document at
the wrong place does an entry there pass without the comparison.

## 109.9 Limits
- **Slower without a cache:** each document is read once per index,
  and each entry costs a lookup in the primary index.
- **A snapshot kept for the whole check** (§80): on a busy database a
  long check keeps older pages in memory for it, up to
  `snapshot_memory` (§83), past which it ends with
  `Error::SnapshotTooOld`.
- **What `check` can't read** — documents on damaged pages, say — is
  kept as a set of ids, to skip their entries: as large as the damage.
- **An index's pages** are listed before they're claimed: 8 bytes for
  each, about a thousandth of the index.
