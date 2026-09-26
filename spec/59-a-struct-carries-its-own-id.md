# 59. A struct carries its own id (`document.rs`, `serde_bridge.rs`, `data.rs`, `collection.rs`, `batch.rs`, `query.rs`, `storage/file.rs`)

A typed `find` handed back documents without their ids. The id was
stored with every document and read with it (§11, §18), then dropped,
because a `T` had no field to put it in: `DocId` had no serde support,
so a struct couldn't declare one (§13.5). `find_with_ids` (§23) was the
way around it, returning `(DocId, T)` pairs. No other document database
behaves like that by default: MongoDB's Rust driver and LiteDB both let
the struct carry its id.

Now it can, the way the MongoDB driver does it:

```rust
#[derive(Serialize, Deserialize)]
struct Task {
    #[serde(rename = "_id")]
    id: Option<DocId>,
    title: String,
}

let id = tasks.insert(Task { id: None, title: "write it down".into() })?;
let task = tasks.find_one(Filter::new().eq("title", "write it down"))?.unwrap();
assert_eq!(task.id, Some(id));
```

And the id is stored once: in the cell, not in the document as well
(file format 9, 59.3). `find_with_ids` and `find_one_with_id` are
deprecated, to be removed before 1.0 (59.4).

## 59.1 What happens where
- **Reading** fills the field in, in `get`, `find`, `find_one` and
  `cursor` alike: every read puts the cell's id into the document's
  `_id` (59.3), and a stored id deserializes into a `DocId`.
- **Insert** uses the struct's id if it's `Some`, and makes one if it's
  `None`, as MongoDB does. The same for a typed `Batch::insert` and for
  `upsert` when it inserts. An id already there is the existing
  `DuplicateId` error.
- **Update** keeps §18's rule: the id argument names the document; the
  field's value isn't stored. A struct can't move itself to another id
  by changing its field.
- **A struct without the field** works as before.
- **A plain `id: DocId`** (LiteDB's style) works too, with `DocId::NIL`
  (all zeros, which no generator makes) meaning "make one". `Option` is
  the documented way: `None` says "not stored yet" without a special
  value.
- **A `DocId` anywhere else** in a struct, as a reference to another
  document, is stored as an id too, and a filter on it finds it (59.5).

## 59.2 How `DocId` goes through serde
`DocId` serializes as a newtype with a marker name,
`$trunkdb::DocId`, around its string form (a UUID). The serde bridge
sees the name and stores a `Document::Id`; any other format, JSON for
one, sees a newtype around a string and writes the string. The technique
is `serde_bytes`'s, which §13.5 named as the way to do this. Reading needs
no marker: the bridge hands a stored id over as its string, as it always
did (§13.5), and `DocId`'s `Deserialize` parses it; from a format like
JSON it reads the string inside the newtype.

**The untyped path follows the same rule on insert.** An object whose
`_id` is a `Document::Id` other than `NIL` keeps it; any other `_id` (a
string, null, the nil id) is replaced with a new one, as before.
**This changes §18**, where insert always made a new id: an object read
from one collection and inserted as it is now keeps its id, and is a
`DuplicateId` in the same collection. That is what MongoDB does, and it
is what a struct's `Some(id)` has to mean.

Rejected:
- **Returning ids always** (`find` giving `(DocId, T)`, or a
  `Found<T>`): breaking, and clumsy for code that doesn't need them.
  §23 already rejected the wrapper struct.
- **A public `FromStr` for `DocId`**: it would put the `uuid` crate's
  error type into the API. Parsing stays internal; serde and `Display`
  are the public ways in and out.
- **Refusing an `_id` that isn't an id** on insert, instead of replacing
  it: stricter, but it would break structs that used a `String` field
  named `_id`, which §13.5 made possible.

## 59.3 One id: in the cell (file format 9)
Since §18 an object held its id twice: in the cell, `[flags][id][...]`,
where the storage layer needs it (a scan, `check` and compaction learn
whose document a cell is without decoding it, and a document that isn't
an object has no field for it), and as `_id` inside the encoded
document, where a read found it. Every write set the second from the
first, so they couldn't differ through the API; but nothing checked
that, and reads, `find` on `_id` and export all trusted the copy inside
the document, while the primary index, `get`, `update` and `delete`
used the cell's. If the two had ever differed, the user would have been
handed an id the database didn't know it by, and an export would have
moved the document to it. §59 made the copy far more visible: a typed
read now hands it to the caller.

So the id is stored once, in the cell:
- **Writing** (`data::write_cell`) encodes an object without its `_id`
  (`document::encode_stored`). 24 bytes less per document: the
  benchmark's file is 42.4 MB instead of 44.9, 31.0 instead of 33.2
  after compaction.
- **Reading** (`data::Records::get`, which every read goes through: find,
  cursor, export, `check`, compaction, `update_many`) puts the cell's id
  into `_id`, first, where MongoDB shows it (`document::with_id`).
- **The decoder reserves room for it.** A top-level object's map is
  sized for its stored fields plus one; without that, adding `_id`
  reallocated the map on every read, and a full scan was 3% slower.
  With it, a scan is as fast as before (154 ms against 157–158), and a
  million lookups take the same time (4.7–5.0 µs each, before and
  after).

**Format 9.** A build before it would read these documents without
`_id`. Format-8 files open as they are: their documents hold an `_id`,
which a read replaces with the cell's, so even an older file has only
one id that counts. The first page a batch writes stamps the header with
9 (`FileStore::write_page`), in the same batch, and a rollback takes the
stamp back with the rest; before, only an allocation stamped it, which
was enough while every format change came with new pages. Compaction
rewrites every document, so it also drops the stored `_id`s of an older
file.

Rejected:
- **Keeping both copies, and correcting on read.** It closes the "which
  id is true" question as well, without a format change, but keeps the
  24 bytes and a copy that means nothing.
- **Dropping the cell's id instead.** The storage layer needs it without
  decoding, and non-object documents have nowhere else to keep it.

## 59.4 `find_with_ids` and `find_one_with_id`: deprecated
With the id in the struct, `find` gives what `find_with_ids` gave. Both
`find_with_ids` and `find_one_with_id` (typed and untyped) are
`#[deprecated(since = "0.12.0")]`, with a note naming the field; they
work as before. `find`, `find_one` and a sorted `cursor` don't call
them. The tests that cover them allow the warning explicitly.

**To be removed before 1.0** (ROADMAP.md, "Before 1.0"), and `cursor`
changed then to hand out `T` instead of `(DocId, T)`, like `find`. What
still needs them until then: a type without an id field that can't get
one (from another crate: wrap it in a struct with the id and
`#[serde(flatten)]`), and documents that aren't objects (`Collection<i64>`),
which have no field to hold an id: those have `get` by id, and MongoDB
and LiteDB don't allow them at all.

## 59.5 Ids in filters, found on the way
A test filtering on a reference found nothing, and not because of this
section: `query::compare` had no case for two ids, so they were never
equal. No filter on an id had ever matched, not even `eq("_id", id)`,
although `From<DocId> for Document` exists so that filters can take ids
(§35.2). Ids now compare by their bytes, which for UUIDv7 ids is the
order they were made in, so `lt` and `gt` mean "made before" and "made
after". Sorting is unchanged: ids still rank as unordered values, after
everything else (§34.1).

## 59.6 Limits
- **Ids have no index key.** A secondary index leaves id values out
  (§34.1, `key::encode_value`), so a filter on a reference scans. Giving
  them a key is an index-only change, but indexes built before it lack
  entries for id values and would give wrong answers until rebuilt: it
  needs a format bump, or a rebuild at open. Open (ROADMAP.md).
- **`eq("_id", id)` scans** too; `get` uses the primary index. The
  planner could send one to the other.
- **`Binary` values still don't compare** with each other, the same gap
  as ids had; nothing has asked for it.
- **`DocId::NIL` is public**, new API: it's needed for the plain-field
  style, and a named constant reads better than `DocId([0; 16])`.

## 59.7 Tests
- `serde_bridge.rs`: a `DocId` is a `Document::Id` through the bridge,
  and a UUID string through serde_json, both ways; a string that isn't
  an id is an error. An `Option<DocId>` field is null for `None` and an
  id for `Some`.
- `collection.rs`:
  - a struct with an optional id field: `insert` with `None` makes one,
    with `Some` uses it, twice is `DuplicateId`; `get`, `find`,
    `find_one`, `find_with_ids` and `cursor` all fill it in; an update's
    argument wins over the field;
  - `upsert` inserting, and a typed batch, use the given id; a `DocId`
    field is stored as an id, and a filter on it finds the document;
  - a plain `DocId` field with `NIL` gets a new id;
  - on the untyped path, an id in `_id` is kept, and a string, `null` or
    the nil id is replaced (this replaces §18's test of the opposite).
- `data.rs`: a stored cell holds the id once, and neither `_id` nor the
  id the written document had in it; a read puts the cell's id first. A
  cell written by format 8, with a different `_id` inside, reads back
  with the cell's.
- `storage/file.rs`: a format-8 file is stamped 9 by the first page a
  batch writes, a rollback takes the stamp back, and the stamp is
  remembered for the rest of the batch.
- `query.rs`: ids compare with ids (`eq`, `ne`, `lt`), and not with their
  string form.
- A doctest on `Collection` shows the field.
- Checked by breaking it on purpose, fourteen ways. Each of these fails
  a test:
  - the bridge ignoring the marker; no newtype on read;
  - insert ignoring a given id; the nil id used as given;
  - batches, or upserts, ignoring a given id;
  - ids not comparing;
  - the stored document keeping its `_id`; inline, or overflow, reads
    not adding it; the id not first;
  - no stamp on a page write; the stamp not remembered; format 8
    refused.
