# 166. Binary in indexes, in the order of its bytes (`index/key.rs`, `query.rs`, `collection.rs`, `select.rs`, `storage/file.rs`)

Binary data has been a kind of value since the v0 core, but a lesser
one: `=` and `!=` compared it by its contents (§124), `<`, `>` and a
sort didn't order it at all, and an index left it out (§28, §104), so
a filter on a binary field read every document. A program that keeps
something as bytes and wants to find it again had to store it as text
instead.

The case that brought it up: a program keeping file names exactly. A
Unix path needn't be UTF-8, and a `String` must be, so a lossy
conversion would make two different names look alike or lose one's
bytes. The program stored each name as text in hex, which works only
because hex sorts as its bytes do: an index on it serves `=`, ranges
and `starts with`. The price is twice the bytes, in the document and
again in every index entry. In the file it came from, two indexes on
hex paths took more pages than the documents themselves.

Now binary is ordered **byte by byte, as SQLite orders a BLOB**, with
every comparison, and has an index key:

```rust
files.ensure_index("name")?;
let name = Document::Binary(path.as_os_str().as_bytes().to_vec());
let found = files.find(Filter::new().eq("name", name))?;            // reads its range
let under = Condition::compare("name", Op::StartsWith, Document::Binary(dir));
let count = files.count(Filter::new().and(under))?;                  // reads no document
```

and in a filter's text, with binary written as everywhere else:

```text
name starts with {"$binary": "L1VzZXJz"} order by name desc limit 10
```

## 166.1 The key
A new type tag, `TAG_BINARY` (6), followed by the bytes escaped and
terminated exactly as a string's are (`encode_escaped`, §28.1): `0x00`
becomes `0x00 0xFF`, and `0x00 0x00` ends the value. So the encoding
is prefix-free (§43.2) and byte order is the bytes' order, `[0]` before
`[0, 0]` before `[0, 1]` before `[1]`, whatever id follows. A long value
is cut to the same budget a string is (`string_budget`), at the same
point for two values that agree up to it, so the cut keeps the order
and the document decides where it hid a difference, as for strings.

Everything an index does for a string it now does for binary:
- `eq`, `lt`, `lte`, `gt`, `gte` read a range of the index, and
  `starts with` the keys beginning with the prefix's escaped bytes
  (§158.2), alone or after a compound index's `=`s;
- an entry decides a condition from its bytes where the value fits the
  key (§104), so a `count` on a range reads no document, and a
  selection of the field is answered from the entry (§100);
- a unique index refuses the same bytes twice, where binary never
  counted before (§33, §124.5);
- `chunks[*]` holds a document once per binary element (§42), and a
  compound index orders by it in its place (§43), where it was one
  "other" value (`TAG_OTHER`) like an array.

A string and binary with the same bytes have different keys, by their
tags: text and bytes never compare (166.2).

## 166.2 The order
`compare` orders two binary values by their bytes, as `[u8]`'s `Ord`
does, so `Lt` to `Gte` hold between them, and `Eq` is what it was
since §124, byte for byte. Binary still never compares with another
kind: `name < "a"` holds of no binary name, and `!=` of every one.
`starts with` takes a binary prefix too and holds of the binary values
beginning with its bytes; a string prefix never holds of binary, nor a
binary one of a string. `contains` stays for text: it folds case, which
bytes don't have, and no index serves it.

A sort puts binary **after date-times**, by its bytes, reversed for
`Desc`, where it came last both ways, with arrays, objects and NaN,
as equals (§34.1). The key's tag follows: 6, after date-times' 5, so
reading an index in order gives what sorting in memory gives. Arrays,
objects and NaN still come last. That changes what a sort over mixed
values returns, and what `min` and `max` of an aggregate find among
binary values; no application is likely to have relied on binary
sorting as one value. A group by a binary field is unchanged: equal
bytes were one group already (§124.4).

## 166.3 Older files: rebuilt at open, stamped 13
An index built before this has no entry for binary in a one-field
index, or an "other" where a compound one now has the bytes. Read by
this build, it would answer wrong: `eq("name", bytes)` would find
nothing. So `Database::open` makes sure no such index is read, the way
§62.3 did for ids, by the same code, generalized
(`collection::rekey_old_indexes`):
- **When:** the file says a format before 13, and has secondary indexes.
- **What:** each collection with indexes is read once; an index a
  document holds binary in (or an id, for a format before 10:
  `keyed_since`) is emptied in place and filled again. The others stay.
- **In one batch:** the one `open` runs, so a crash in the middle
  leaves the file as it was, and an error rolls it back. A read-only
  open does the same in memory (§88), at every open, and writes
  nothing.
- **Stamped 13** afterwards, so the check runs once.
- **Not stamped by a read-only open.** Before, the open stamped the
  header in its batch whatever the open, and a read-only one kept the
  stamp in memory: `trunkdb info`, which opens read-only, would have
  said "format 13, written by" this build of a file that says 12 and
  0.31.0 on disk. It went unseen as long as every file the tests open
  read-only had the current format; the 0.23.0 fixture, format 12, is
  the first that didn't. Now `load_catalog` stamps only for an open
  that writes, so `info` and `file_info` say what the file says. A
  read-only open that does rebuild an index in memory still reports
  13: writing a page in a batch stamps the header (§59), in memory too
  (166.5).

**Why format 13.** A build before it computes no key for binary. If it
deleted a document holding some in an indexed field, it would leave the
new entry behind, pointing to a document that no longer exists. A file
with binary keys has to be refused by such a build, which is what the
format version is for (§21.2): 0.31.0 reads 13 as newer than itself and
says so. This build still opens 6 to 12.

## 166.4 Measured
Release build, 100,000 documents of about 50-byte file paths and a
number, indexes on both, the first open of a file written by 0.31.0
(format 12), three runs each:

| First open of a format-12 file | |
|---|---|
| the paths as text: checked, nothing rebuilt | 91–157 ms |
| the paths as binary: that index rebuilt | 320–398 ms |
| every open after that | 4–6 ms |

The same documents written by this build, the paths as hex text (as
the program above had to) and as binary:

| | hex text | binary |
|---|---|---|
| file | 42.6 MiB | 31.4 MiB |
| documents | 13.6 MiB | 8.4 MiB |
| index on the path | 19.0 MiB, 2,430 pages | 13.1 MiB, 1,676 pages |
| a count by a 30-byte prefix, 2,000 found | 0.39 ms | 0.22 ms |

A quarter of the file, a third of the index, and the prefix count
faster with keys half as long.

## 166.5 Limits
- **A unique index an older file has two documents with the same
  bytes in** fails the open with `DuplicateValue`, and the file is left
  as it was, as §62.4 has it for ids: open it with 0.31.0, drop the
  index or change one of them, and open it again.
- **The first open of an older file is slower**, by a read of every
  document of every indexed collection (166.4), even where no document
  holds binary; once.
- **A read-only open that rebuilds an index reports format 13** and
  this build as the last writer, of a file whose bytes it leaves as
  they were: the header in memory is stamped by the first page the
  rebuild writes (166.3). Only an older file with indexes holding
  binary, or ids before 10, opened read-only; `check` and every read
  are right, only `file_info` says what the open made of it.
- **No `contains` on bytes** (166.2), and no pattern beyond a prefix.
- **A range on binary reads its bound too,** as every range does
  (`key::range_for`): different long values can share a key.
- **Binary in a filter's text is base64,** `{"$binary": "…"}`, as in
  exports: readable for a program, not for a person. A program passes
  it as a parameter (§163) instead.

## 166.6 Tests
- `index/key.rs`: binary keys in byte order whatever the ids after
  them, `[0]` before `[0, 0]` before `[0, 1]`; after the latest
  date-time and before "other"; exact, and read back whole; cut as a
  string is, then inexact and not read back; a part of a compound key
  split off whole; a terminator inside read as damage; text and bytes
  with different keys. A binary prefix's range, at one field and at
  eight, holds every value beginning with its bytes, and others only
  where the cut hides the difference; an entry decides it from its
  bytes where exact; no string is in the range. Binary moved out of the
  values without a key.
- `query.rs`: binary compares by its bytes with every operator, never
  with a string, number or null; `starts with` holds of bytes beginning
  with a binary prefix, never across text and bytes; no `contains`;
  sorts after date-times by its bytes, reversed for `Desc`, and arrays
  and objects still last; filters as text with `{"$binary": …}` in a
  range and a `starts with` sorted descending, read back as written.
- `collection.rs`:
  - an index on a binary field of file names with zeros and `0xFF` in
    them: an `eq` reads its one document, a range its ten and at most
    its bound, a count of it none, a selection of the field none; a
    prefix reads just its 44; a sort walks the index and reads 3 for a
    limit of 3; a compound `(size, name)` gives one size's names in
    order reading 2; a unique index refuses the same bytes twice; the
    file checks out;
  - a format-12 file whose indexes leave binary out and hold it as
    "other", made by a test helper: this build answers wrong before,
    and `check` sees it; exactly the two indexes holding binary are
    stale; opening it rebuilds them, keeps the index list's order,
    stamps 13, checks out and answers through both; the next open reads
    no document;
  - the same file opened read-only first: an `eq` finds its document
    through the index keyed in memory, and the file's bytes stay as
    they were; a format-12 file whose index holds no binary, opened
    read-only, reports format 12 and stays 12 on disk, and an open that
    writes stamps it 13;
  - a unique index on a format-12 file with the same bytes twice fails
    the open, twice, and leaves the file's bytes as they were;
  - the random values of the planner tests (each plan against a scan,
    updates against a model) and of the entry-decision tests now hold
    binary, short and with zeros, and long enough to be cut.
- `tests/equal_whole_values.rs` (§124): `bin > {"$binary": "AA=="}`
  finds the three documents with binary, where it found none, and
  `bin < …` and `bin starts with …` find theirs, through every way of
  reading, with and without indexes.
- `tests/cli.rs`: `trunkdb info` says format 13; a file of format 14
  is the one newer than this build; the 0.23.0 fixture, opened by
  `info`, still says it was written by an unknown release.
- The tests that pinned the old behaviour changed: format 13 where they
  said 12, binary among the ordered kinds in the sort test and out of
  the unindexed ones, `<` holding between binary values. The update
  model test runs 50 rounds instead of 40, to see enough failing
  updates with the new values among them.

## 166.7 Mutations
Checked by breaking it on purpose, twenty-two ways, the whole suite
run each time. Each of these fails a test, but one:
- **binary without a key,** **its zeros not escaped,** **tagged as a
  string,** **a cut value taken as exact,** **not read back from a
  key,** **an escaped zero read back as two bytes,** **no prefix range
  for binary,** **a prefix of the other kind decided on bytes:** caught
  by the key tests, the end-to-end index test and the random planner
  and entry-decision tests;
- **binary not compared,** **compared backwards,** **sorting last
  again,** **sorting before date-times,** **no `starts with` on binary
  documents,** **binary taken as a whole value again,** **not handed
  out from a key:** caught by the sort, compare and selection tests, the
  random tests against a scan and `tests/equal_whole_values.rs`;
- **format-12 files not rekeyed,** **binary not taken for stale,**
  **taken for stale only before format 10,** **every index rebuilt,**
  **format 12 refused,** **a read-only open stamping the format:**
  caught by the old-file tests, the fixtures' and `info`'s.
- The one that passes: the tag 0xFE instead of 6, which orders binary
  the same way, after date-times and before "other", as 0xFE did for
  ids in §62.5.

The first run of these counted every mutation as caught, wrongly: two
tests of `tests/cli.rs` failed without any mutation, as they still said
format 12, and the runs before had stopped at the library's tests. One
of them, the fixture's `info`, is what found the read-only stamp
(166.3).

## 166.8 Rejected
- **Binary ordered by length first, then bytes,** as some databases
  order blobs: no shorter prefix could be read as a range, and
  `starts with` is what a path needs.
- **A new kind for paths,** text that needn't be UTF-8: binary already
  is that, and an application knows what its bytes mean; a second kind
  would have to be taught to every part that knows the first.
- **Rebuilding every index of an older file,** **export and import as
  the way up,** and **a flag per index:** rejected for §62.3, for the
  same reasons.
- **Keeping binary last in a sort,** with keys only for `=`: an index's
  order has to be a sort's order (§34.1), or reading an index in order
  would give something else than sorting in memory, and a range needs
  an order anyway.
