# 170. Field names stored once, numbers as varints (`names.rs`, `document.rs`, `catalog.rs`, `decode.rs`, `storage/file.rs`)

A stored document carried every one of its keys in full, each after a
4-byte length, and every int as 8 bytes, every string's and array's
length as 4 (§11). In a collection of tasks, that is the same
`"status"`, `"created"`, `"tenant"` and `"title"` in every document,
and a `tries` of 3 taking 9 bytes. Measured on a real program's later
file, 1,030,000 documents of about 217 bytes each: some 104 of those
bytes were field names and their lengths, and about 90 the values.

Now a file keeps **one dictionary of field names**, and a stored
document's keys are small numbers into it. Ints, lengths and counts
are **varints**. Nothing changes for a program: documents stay
schemaless, a new name just joins the dictionary, and every read
gives what was written.

```text
{"server": "web-1", "cpu": 42, "load": 0.7,
 "tags": ["prod", "eu"], "at": {"$date": "2026-11-06T22:15:36Z"}}

stored before: 106 bytes   now: 50 bytes   (69 with every name inline)
```

## 170.1 The dictionary
`names.rs`: `FieldNames`, the names by number and the numbers by name.
One for the whole file, not one per collection: collections share most
of their names (`name`, `created`, `status`), and one dictionary keeps
the catalog to one kind of cell and a read to one lookup.

- **Who joins:** a key of at most 64 bytes of UTF-8 (`MAX_LEN`), at any
  level of a document, numbered in the order first seen, depth first:
  a key, the keys inside its value, then the next key; each once.
- **How many:** at most 4,096 names (`MAX_NAMES`). Past that, and for a
  longer name, the key is stored as it is (170.2). So a document whose
  keys are data, ids, dates or paths as keys, can't grow the dictionary
  without end, and a program whose keys are such data loses nothing it
  had before: its keys cost what they cost.
- **Never forgotten,** but by `compact` (170.4): a name stays when the
  last document using it is deleted, as it would cost a read of every
  document to know.

## 170.2 The encoding
New type tags, in stored documents only (`encode_stored`):

| Tag | What follows |
|---|---|
| 10 `TAG_FIELDS`, an object | `[varint count]`, then each key and its value |
| 11 `TAG_VARINT`, an int | the value zigzag-encoded as a varint: `-1` is 1, `1` is 2 |
| 12 `TAG_TEXT`, a string | `[varint length][UTF-8]` |
| 13 `TAG_BYTES`, binary | `[varint length][bytes]` |
| 14 `TAG_LIST`, an array | `[varint count]`, then the items |

A key is one varint: a name's number times two, or, for a name not in
the dictionary, its length times two plus one and then its bytes. So
the low bit says which, and a number below 64 takes one byte.

A varint is unsigned LEB128 (`decode::take_varint`, `put_varint`): 7
bits a byte, the high bit set on every byte but the last; at most 10
bytes, and the tenth may hold only the 64th bit. One that runs past the
bytes, or past 64 bits, is `InvalidData` (§55). Floats, bools, nulls,
ids and date-times keep the encoding they had: they are fixed widths
that varints wouldn't shorten.

`encode_document` is unchanged: the canonical byte form that compares
two values for equality, groups an aggregate's values and keys a live
query's, which never reaches the file. The old tags 2, 4, 5, 6 and 7
still decode everywhere a stored document is read, so a document
written before this reads as it did (170.5).

## 170.3 The catalog
The dictionary lives in the catalog (page 1 and its chain, §9), in
cells of a new kind, `KIND_FIELD_NAMES` (4): `[u8 4][u32 first
number]`, then each name as `[u8 length][name]`, numbered on from the
first. A cell holds at most about 1,024 bytes of names; a batch adding
more writes several.

- **Written with the batch that brings the name** (`Catalog::learn`):
  before an insert or an update stores a document, its unknown names
  join the batch's copy of the catalog and their cell is appended. So
  a document is never stored with a number its file doesn't have, and
  a batch that fails takes its names with it: the next batch numbers
  its own from where the dictionary stood.
- **Shared cheaply:** the catalog is copied for every batch and every
  snapshot (§78), so it holds the dictionary as an `Arc`, copied only
  when a batch adds a name: nearly never, once a program's documents
  have all their shapes.
- **Read at open:** the cells, in any order on the catalog's pages,
  must number the names from 0 without a gap or an overlap. Two cells
  from the same number, a cell starting past the names before it, and
  a name too long for the dictionary or in it twice are damage: the
  open fails with `InvalidData`, saying which.

Every read gets the dictionary of the catalog it reads through: a
snapshot the one of its moment. A name only ever joins, so a newer
dictionary reads an older document too.

## 170.4 `compact`
`compact` stores every document anew into a new file (§41), whose
dictionary learns the names as the documents come: the names still in
use, numbered in the order they are met. So a dictionary that filled up
with names no document has any more starts over, and every document
written before this release is stored in the new encoding.

## 170.5 Older files: format 14
The file format is 14. A build before it would read a document's new
tags as damage, so 0.33.0 and older refuse the file as newer than
themselves (§21.2). This build opens formats 6 to 13 and stamps them 14
as before, with nothing to convert: the dictionary of an older file is
empty, its documents read under their old tags, and each is stored in
the new encoding the next time it is written. `compact` stores them all
so (170.4).

## 170.6 Measured
Release build, the bench's 100,000 tasks (status, created, tenant, a
title of 20 to 60 bytes, tries, up to two tags) with three indexes,
inserted in batches of 1,000; pages counted by `stats`:

| | before | now |
|---|---|---|
| data pages | 2,223 | 1,233 |
| file after inserting | 40.7 MB | 32.6 MB |
| file after `compact` | 30.6 MB | 23.1 MB |

The index pages are the same 2,742: keys are encoded as before (§28).
Updates went from 16k to 21k a second and inserts from 44k to 45k:
fewer bytes to write and to fit. Reads are the same or a little faster.

## 170.7 Rejected
- **A dictionary per collection,** as the roadmap had it: it would
  number `created` once per collection, put the collection into every
  read's lookup, and need cells per collection, for little gain. Names
  are shared more than they differ.
- **Shapes,** a whole set of keys stored once and named by a number:
  a document of a known shape would save its key varints too, a byte
  each. It costs a second structure, and a shape per combination of
  optional fields. The numbers don't call for it yet.
- **Compressing documents** (LZ4, zstd): fast, but a document of 100
  bytes barely compresses on its own, and a page compressed whole
  can't be changed in place (§52).
- **Varints for floats, ids and date-times:** fixed-width values that a
  varint would make longer as often as shorter.

## 170.8 Limits
- **A key of more than 64 bytes,** and every key after the first 4,096
  names, is stored inline, as before but for its varint length.
- **The dictionary only grows** until a `compact` (170.1).
- **An older release can't open the file** once this one has opened it
  for writing (170.5): an export, or a copy from before, goes back.

## 170.9 Tests
- `decode.rs`: varints of every width read back; one cut short, one of
  eleven bytes and one past 64 bits are refused.
- `names.rs`: names numbered in the order first seen, at every level,
  each once; a long name and names past the limit stay out.
- `document.rs`: every kind of value stored and read back, with keys
  numbered and inline, and a count or length past the bytes refused; the
  example above, 106 bytes before, 50 now, 69 inline; a key numbered past
  the dictionary is damage, naming the number; decoding some fields is
  damaged exactly when the whole is (§99.2), over the old tags and the
  new.
- `tests/field_names.rs`:
  - names survive a reopen, and the names of a failed batch go with
    it: the next batch numbers its own from where the dictionary stood;
    a second collection shares the names;
  - a document of 5,000 keys and a long one: the first 4,096 numbered,
    the rest inline, every key read back, found and selected;
  - a snapshot reads its documents while later batches add names, and
    typed reads, selections, sorts and counts read numbered keys;
  - `compact` keeps what the documents say, in a dictionary of the names
    still in use, and after a reopen.
- The old-file fixtures (§120) open, check, read and write as before,
  their documents in the old tags.
