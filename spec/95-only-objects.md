# 95. Only objects (`collection.rs`, `lib.rs`, `storage/file.rs`, `database.rs`, `data.rs`, `document.rs`, `check.rs`, `export.rs`)

A document could be any value: an object, but also a number, a string,
an array, a bool or null. `Collection<i64>` stored plain numbers. The
values that weren't objects cost more than they gave:

- **They had no `_id`.** Every other document carries its id as `_id`
  (§59). These couldn't, so:
  - `find` returned them without one (§74);
  - `Filter::id` never matched them, even though the primary index found
    them (§61.2);
  - an application, or the admin tool, couldn't update or delete one it
    found. It needed the id its insert returned.
- **They broke the order of `_id`.** As a missing field they sort as
  null, first, while the primary index held them by id among the
  objects. A walk of the primary index in `_id` order (§96) would have
  needed to know a collection held none of them.
- **Everything else passed them by.** No field condition matched them,
  an update operator failed on them (§68), and every index held them as
  nulls.

The gain was one convenience: a `Collection<i64>` without a struct
around the number. MongoDB and LiteDB don't allow such documents at all
(§59.4, §74.1). No application of trunkdb's used them, only its own
tests.

From §95 on, every document is an object. Before 1.0 that costs a
format version and some tests. After 1.0 it would be a breaking change
to the file and the API (ROADMAP, "Before 1.0").

## 95.1 Writes refuse them
A write whose document isn't an object fails:

```rust
/// A write gave collection `collection` a document that isn't an
/// object: a number, a string, an array, a bool or null (SPEC §95).
Error::NotAnObject { collection: String }
```

It is checked in one place, `apply_write_op`, for `Insert` and `Update`.
Every write goes through there:
- `insert`, `update` and `upsert`, typed and untyped;
- `update_many`, whose closure may return anything;
- a `Batch`;
- `import`.

A batch that meets one rolls back whole, as for any error in a batch.
`update_fields` can't make one: its paths can't be empty, so the
document stays an object.

`Error` is `#[non_exhaustive]` (§60), so the variant breaks no match.
The write itself, without the check, is `apply_unchecked`. Only tests
call it directly, to build the files older builds could write.

**Typed collections.** `Collection<T>` still takes any
`T: Serialize + DeserializeOwned`. Whether `T` serializes to an object
(a struct, a map) is known only at runtime, so `Collection<i64>`
compiles and its `insert` fails with `NotAnObject`, as does a
`Collection<Vec<i64>>`. The documentation of `Collection` says `T` is a
struct or a map, and that a plain value goes into a one-field struct.

## 95.2 Format 12: every document is an object
A file whose header says 12 holds only objects. Reads may rely on it;
§96 will.

**Why the version changes when no page changes shape.** It is the same
reason as format 10 (§62.3). 0.19.0 would open a format-12 file as its
own and could write a number into it. Every later open would then trust
the header and be wrong. 0.19.0 refuses format 12 as newer than itself
(§92), which is what the version is for (§21.2).

**Older files (6 to 11), checked and stamped at open.** These are
format-12 files if they happen to hold only objects.
`Database::open` makes sure of it in the batch it already runs
(`load_catalog`):

1. It reads the format before anything can stamp the header. §62's
   rebuild stamps it, for files before 10 with secondary indexes.
2. `only_objects_in_old_file` checks every document of every collection,
   by the name's order. It reads only the first byte of each document's
   encoding, its type tag (`Records::is_object`), without decoding
   anything. For a document in overflow pages, that is the chain's first
   page.
3. §62's check runs, for files before 10.
4. The header is stamped 12, so the check runs once. An empty file is
   stamped too, and so is a file before 10 without secondary indexes,
   which before §95 waited for its first write.

**One that isn't an object** fails the open with
`Error::Format(FormatError::NotAnObject { collection, id })`, the first
one found. The batch rolls back and the file stays as it was, byte for
byte. It is a format error: this build can't use the file as it is.
The admin tool shows it as an unsupported format, with the message.

**The way out** is an older build, as in §62.4. Open the file with
0.19.0, delete those documents or wrap each in an object, and open it
with this build again. The message says so. No real file is known to
hold any.

**A read-only open (§88)** checks too, and fails the same way. It keeps
the stamp in memory, as it keeps all of the open's changes, so the file
on disk still says 11. The next read-only open checks again, until an
open that writes stamps the file.

Measured on a format-11 file of 100,000 documents (14.6 MB), release
build, three rounds:

| | |
|---|---|
| first open, checked and stamped | 87 to 192 ms |
| every open after it | 24 to 37 ms |

## 95.3 What follows
- **`check`** reports a document that isn't an object as a problem.
  Every file says 12 once it is open, so such a document breaks what
  the format promises.
- **Export** writes no `$value` line any more, since no document needs
  one. The export format stays version 2. `document_line` keeps its
  branch for them, as there's nothing to gain from a panic there.
- **Import** refuses a `$value` line, and any line whose document isn't
  an object, with its line number, as for any bad line (§30). An export
  from 0.19.0 of a file without such documents imports as before.
- **`Document` stays as it is.** Values inside a document can still be
  anything, and `Filter::matches` and `apply` still take any
  `Document`.
- **README and DESIGN** lose the exceptions: the id of a document that
  isn't an object, the format history up to 11.

## 95.4 Rejected
- **Keep them and pay:** the learned "objects only" state the `_id`
  walk would have needed, and the admin tool's "inspect only". Every
  feature to come would have paid again.
- **Wrap them silently** (`Collection<i64>` storing `{"value": 1}` and
  unwrapping it on reads): filters would have to name a field nobody
  wrote, and the typed and untyped views of one collection would
  disagree.
- **Refuse new ones without a new format version:** an older build
  could still write them into a file that had been checked, and every
  read would keep its exceptions for the files it can't be sure of.
- **A compile-time bound on `T`:** serde has no trait for "serializes
  to a map". A trait of trunkdb's own would have to be implemented for
  every struct.
- **Wrapping on import** (`--wrap-values`): no real file needs it. It
  can come later if one does.

## 95.5 Tests
- **Refused writes** (`collection.rs`, `every_write_refuses_a_document_that_is_not_an_object`):
  - a number, a string, an array, null and a bool, each through
    untyped `insert`, `update` and `upsert`;
  - a typed `Collection<i64>` and `Collection<Vec<i64>>`;
  - a `Batch` with an object first and a number second, which writes
    neither;
  - an `update_many` whose closure returns a number for one document,
    which rolls back the change it made to the other;
  - `NotAnObject { collection }` every time, and only the documents
    from before left afterwards.
- **Older files** (`collection.rs`):
  - a format-11 file of 300 objects, one in an overflow chain, and an
    empty indexed collection: a read-only open passes and leaves 11 on
    disk; an open that writes stamps 12; an empty format-11 file is
    stamped too;
  - a format-11 file holding a number, a string in an overflow chain, or
    an array: both opens fail with `FormatError::NotAnObject`, naming
    the collection and the id, and the file is unchanged byte for byte.
    Before its version is set back, `check` reports the document. The
    collection holding it is checked first in one case, and after one
    holding only objects in the others.
- **Import:** a `$value` line is refused with its line number.
- **§62's test** now expects 12, and a file before 10 without secondary
  indexes stamped at open.
- **The tests that stored plain values** use objects:
  - `Num { n }` and `Text { s }` in `testing`, for the
    `Collection<i64>` and `Collection<String>` tests in `batch.rs`,
    `database.rs` and `snapshot.rs`;
  - `{"v": value}` for the untyped ones, through the `database.rs`
    helpers `insert`/`get`/`stored`;
  - a `Counter { count: u64 }` for the bridge's `u64` test;
  - the `Snapshot` doc example and the export round trip's random
    documents;
  - `tests/cli.rs`, which also expects format 12.
  
  `non_object_documents_are_found_without_ids` and
  `non_object_documents_are_unaffected_by_id_merging` are gone with the
  case.

Nine hand-made mutations, all caught:
- `Update` not checked;
- the open's check skipping a collection;
- an overflow chain, or an inline document, taken as an object without
  looking;
- the open stamping without the check;
- the format read after the stamp, so the check never runs;
- no stamp at open;
- `check` not reporting;
- import not refusing.

Full suite, clippy `-D warnings` with and without fuzzing, fmt,
rustdoc, the fuzz and bench crates, Rust 1.89.

## 95.6 Limits
- The first open of an older file reads every data page once.
- A file with documents that aren't objects needs an older build to
  move up.
- `Collection<T>` for a `T` that isn't a struct or a map compiles, and
  fails at its first write.
