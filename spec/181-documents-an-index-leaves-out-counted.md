# 181. Documents an index leaves out, counted (`catalog.rs`, `collection.rs`, `compact.rs`, `check.rs`)

A one-field index holds no entry for a value nothing orders: an array,
an object, NaN (§34.1). Those sort after every other value. A walk in
the index's order (§34.2) that runs out before its limit can't know
whether such documents exist, so it scanned every document of the
collection looking for them. Usually it found none.

This showed up in the admin app. Six recipes, an index on `category`:

```
category != "Salad" order by category limit 50
read: category, the whole index, 6 entries, its entries decide the filter
read: _id, the whole index, 6 entries
keys examined: 12
documents examined: 11
```

The walk read 5 documents, and then the scan read all 6 again for
nothing. The admin app always asks for a page, `limit 50`, so every
sorted list it shows that has fewer matches than a page did this. On a
large collection, a sorted page with few matches cost a read of the
whole collection.

Now each such index counts how many documents it leaves out, and the
walk scans only when the count isn't 0. The same query reads 6 entries
and 5 documents.

## 181.1 The count
- **`IndexMeta::left_out: Option<u64>`.** It counts the documents
  whose value at the index's field `sort_order` puts among the values
  nothing orders (`is_unordered`), a missing field counted as null,
  which is ordered. This is exactly what the scan looks for, so a count
  of 0 means the scan finds nothing.
- **Which indexes:** a one-field index whose path has no `[*]`
  (`counts_left_out`). Those are the only ones a walk in sort order
  scans after. A compound index holds every document (§43.2), and a
  multikey index never serves a sort (§42.3). Both keep `None`.
- **`None` means not known.** An index from a file before format 16
  has no count, and the walk scans as before.
- **Kept on every write:** `update_secondary_indexes` compares the old
  and the new document of each insert, update and delete. It returns
  the indexes where one went in or out of the count, with the change,
  and `Catalog::add_left_out` applies them once the indexes are no
  longer borrowed. A count can't go below 0: that would be damage, and
  is reported as such.
- **Counted at build:** `ensure_index` counts as it fills the index
  (`fill_index`). `compact` counts as it rebuilds each index, an older
  file's included, so a compaction is how an older index gets its count.
  A compaction that gains no pages leaves the file as it is (§41), and
  dropped the rebuilt catalog with its counts. Now it keeps them
  (`keep_counts`): each index without a count gets the one the rebuild
  made. Without that, a small file like the admin app's sample would
  never have been counted. A mutation found this (181.9).
- **In a batch:** the count is part of the catalog the batch changes,
  so a failed batch takes it back with everything else (§17, §78).

## 181.2 The catalog cell
A new cell kind, 5: `[u8 kind = 5][u64 root][u8 collection name length]
[collection name][u64 left_out][u8 flags][u8 count]`, then each field
as `[u8 length][field name]`. That is kind 3's layout with the count
after the collection name. The count sits at a fixed place, so a
change rewrites the cell in place (`set_left_out`), as
`set_current_data_page` does for a collection.

An older file's index has a cell of kind 1, 2 or 3, without a count.
When it gets its first count, that cell is deleted and a kind 5 cell
appended. Only a compaction does that (181.1).

The cell is written only when the count changes. That happens only
when a document with an array, object or NaN at an indexed field is
written, so ordinary writes don't touch the catalog.

A kind 5 cell on an index that can't count (several fields, or `[*]`)
is refused at load as damage.

## 181.3 The walk
`walk_in_index_order` scanned after the index when its range had no
bound, the index wasn't compound, and it wasn't a walk by id. Now it
also needs the index's count not to be `Some(0)`. Nothing else changed:
the plan, the order and the results stay the same. `explain` shows the
difference: no `read: _id` line, and fewer entries and documents.

A plan passed over (§133) runs the walk the same way, so the entries it
reports stay what that plan would read.

## 181.4 File format 16
Format 16 has the new cell kind. 0.36.0 and older refuse a format 16
file: they couldn't decode the cell, and if they could, they wouldn't
keep the count up to date. A format 15 file is a valid format 16 file
whose indexes have no count, so it opens as it is and is stamped 16.
Its indexes keep scanning until `compact` counts them, whether or not
it gains pages, or until they are dropped and built again.

## 181.5 `check`
`check` counts the documents left out as it reads them for the other
checks, and reports an index whose stored count differs:
`"recipes"'s index on "category" counts 0 documents left out of its
order, where the documents give 1`. A collection with an unreadable
document skips this check, since that document can't be counted.

## 181.6 Rejected
- **Holding those values in the index under a tag that sorts last**,
  the way a compound index does (§43.2). The walk would find them
  without any scan, even when there are some. But every reader of a
  one-field index would have to learn about the tag: ranges, counts
  from entries (§101.4), entries that decide the filter (§100), unique
  checks (§33), type tests (§159). Every existing index would also need
  rebuilding at open. That's a lot of risk for values that are rare in
  a field someone indexes.
- **Counting at open,** so a format 15 file gets its counts at once.
  The first open after the upgrade would read every document of every
  collection with an index, and a read-only open couldn't save the
  result. `compact` already reads everything.
- **A flag, "leaves something out", instead of a count.** A delete
  couldn't clear it without reading the other documents.
- **Using the index only for `!=`.** The scan follows any walk that
  runs out before its limit: `order by category limit 50` alone did the
  same.

## 181.7 Tests
- `tests/left_out.rs`:
  - the recipes: 6 entries and 5 documents, in the right order;
  - an array, an object and NaN, each inserted: the scan comes back and
    the document comes last; each deleted: the scan goes;
  - an update into a value nothing orders, from one such value to
    another, back to a string, and a field unset;
  - the count read back after a reopen; a batch that would leave one
    out, failing on its precondition, leaves the count at 0, also after
    a reopen;
  - `ensure_index` on documents that already leave one out;
  - `compact`, rewriting the file, counting with one left out and with
    none;
  - the 0.35.0 fixture (format 15): it scans until compacted, then
    doesn't, with the same results;
  - 400 random inserts, updates and deletes of values of every kind,
    nothing orders among them: every 20 steps two sorted pages through
    the index are compared with a scan's, and `check` passes.
- `catalog.rs`: kind 5 cells read back with each option and the
  highest counts, the same length for any count; refused on a compound
  or `[*]` index; `add_left_out` adds, leaves an index without a count
  alone, refuses going below 0, and the count survives a reopen.
- `check.rs`: a count set wrong by hand is reported.
- `compact.rs`: two indexes made into an older file's by hand, one
  with a document left out; a compaction that gains no pages counts
  both, 0 and 1, the walk in the first's order stops scanning, and the
  counts are there after a reopen.
- Every earlier test of sorting through an index (§34, §47, §176) and
  the fixtures (§120), and the `queries` fuzz target (181.9), which run
  the walk with and without the scan now.

## 181.8 Measured
Release build of the `trunkdb` command, an M1 Pro; 100,000 recipes, `category`
one of eight strings, indexed; the mean of 20 runs of `find --explain`
each. The same file twice: first with no document left out, then with
one recipe whose `category` is `[1]`, which makes the walk scan as
every walk did before this section.

| | scanned, as before | spared, now |
|---|---:|---:|
| `name = "r77777" order by category limit 50` | 109 ms, 200,001 entries, 200,001 documents | 58 ms, 100,000 entries, 100,000 documents |
| `category != "Salad" order by category limit 100000` | 58 ms, 200,001 entries, 187,411 documents | 18 ms, 100,000 entries, 87,410 documents |
| `category != "Salad" order by category limit 50` | 0.1 ms, 50 entries, 50 documents | the same |

A page the walk fills never scanned, so it is unchanged. One it
doesn't fill reads half as much. With `!=` the walk's own entries
decide the filter (§100) and read no document for the excluded ones,
but the scan read every document, so the gain is larger than half.

Writes weren't measured. A write looks up the indexed field's value
once more per index, which it already looks up for the keys, and
writes the catalog only when a document moves in or out of a count.

## 181.9 Mutated
Twelve mutations by hand (a script in the scratchpad, each run against
`tests/left_out.rs` and the `catalog`, `check` and `compact` unit
tests): never counted; an update's change reversed; not known taken as
0; `ensure_index` not counting; a compaction not counting; a delete not
counting; the count kept in memory but not written to its cell; an off
by one at build; `check` silent; a compaction that gains nothing
dropping its counts; an older cell left as it was where it can't grow
in place; and the scan always skipped. All twelve fail a test.

Two of them first stood, and each found something:
- **A compaction not counting** stood because the test's compaction
  gained no pages, and such a compaction left the file as it was,
  dropping the rebuilt catalog and its counts. That was a gap, not only
  in the test: an older file that is already compact would never have
  been counted. `keep_counts` closes it (181.1), and the test now
  deletes many documents first so its compaction rewrites the file.
- **An older cell left as it was** stood because `update_cell` grows a
  cell in place where its page has room, or by compacting the page
  (§153), so the replacing branch runs only on a full catalog page. A
  test now fills page 1 to under 11 free bytes, with nothing deleted
  before that compacting could reclaim.

**Fuzzed:** the `queries` target (§105), whose documents have arrays
in indexed fields and whose reads sort with limits, checked against
its model: 1,000 programs of seed 11, in 240 s, with 2,836 indexes
made, 2,788 dropped and 2,624 compactions among them. Nothing found.

## 181.10 Limits
- An older file's indexes keep scanning until compacted. A compaction
  reads every document, which can take a while on a large file.
- A collection that really has documents left out still scans. Holding
  those values in the index (181.6) would fix that, if a program ever
  needs it.
- The count is kept for one-field indexes only. A compound index never
  needed the scan.
