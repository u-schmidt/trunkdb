# 30. Export and import as JSON Lines (`json.rs`, `export.rs`)

Real and tested:

```rust
db.export(File::create("backup.jsonl")?)?;       // Summary { collections, documents }
new_db.import(File::open("backup.jsonl")?)?;     // same ids, same indexes
db.collections()?;                               // ["pings", "users"]
```

The whole database becomes one text file that doesn't depend on the page
layout. That makes it the migration path between file format versions
(§21.2): export with the old trunkdb, import with the new one, and
trunkdb never has to read an old format itself — which 1.0.0 needs
(ROADMAP.md). It's also a backup that can be read and `diff`ed, and a way to
bring data in from elsewhere.

## 30.1 Tagged JSON (`json.rs`)
`serde_json::Value` via the serde bridge (§13) loses information:
`Binary` would come back as an array of numbers, an `Id` as a string,
and JSON has no `NaN` or infinity. So `json.rs` converts `Document`
directly, and marks what plain JSON can't hold with a one-key object
whose key is a tag, like MongoDB's Extended JSON:
- `Id` → `{"$id": "<uuid>"}`;
- `Binary` → `{"$binary": "<base64>"}` (standard alphabet, padded; a
  dozen lines in `json.rs` rather than a dependency);
- `NaN`/`±inf` → `{"$float": "NaN"}`, `"Infinity"`, `"-Infinity"`;
- an `Object` that *is* a one-key object with a tag name →
  `{"$object": {...}}`, so it can't be mistaken for a tag.

Everything else is plain JSON. `Int` and `Float` stay apart because
`serde_json` always writes a float with a fraction or exponent (`3.0`,
`1e300`) and reads such a number back as a float. So plain JSON is
tagged JSON: a hand-written file needs no tags, and an object with an
unknown `$` key (MongoDB's `$date`) is just an object. Integers outside
`i64` are an error, not a rounded float.

Two `serde_json` features are needed:
- `preserve_order`: a `Value` object otherwise sorts its keys, and
  field order is part of a document;
- `float_roundtrip`: the default float parser is faster but can be one
  bit off. The round-trip test found it: `4.1946076254797075e17` came
  back as `4.194607625479707e17`.

`serde_json` is now a regular dependency (it was only used by tests).
A Cargo feature to make it optional was rejected for now: it's small,
nearly every Rust app already has it, and a feature flag would split
every build and test run in two. Easy to add if someone minds.

## 30.2 The file
```text
{"$trunkdb_export":1}
{"$collection":"users","$indexes":["age"]}
{"name":"Ada","age":36,"_id":{"$id":"0199…"}}
{"$collection":"pings","$indexes":[]}
…
```
- **Header line** with the export format's own version — independent
  of the file format version, which is the point.
- **A collection line** per collection (name, indexed fields — a unique
  index as `{"field": ..., "unique": true}`, §33.5), then its
  documents, one per line. Collections come in name order, documents in
  id order, so exporting the same data twice gives the same bytes.
- **A document line** is the document itself: an `Object` already
  carries its `_id` (§18). Any other document (a bare `Int`, an array)
  has nowhere to put an id, so it's wrapped:
  `{"_id": {"$id": ...}, "$value": 5}`. An `Object` whose only field
  besides `_id` has a tag name goes whole into `$object`, with the
  `_id` repeated outside — so the wrapper stays unambiguous, and field
  order is kept.
- A collection line is recognized by `$collection` **and no `_id`**:
  every exported document has an `_id`, so a document with a
  `$collection` field isn't misread.

One file per database, not one per collection: a backup or migration is
one thing to move, and the collection lines already separate the parts.

## 30.3 Import: ids kept, chunked, not atomic
Every document keeps its id — documents that refer to others by id
would otherwise point at nothing. This needed no new internal step:
`WriteOp::Insert` has always carried the id, and `write_batch` is
public. A document line without `_id` (hand-written) gets a new one;
an `_id` that isn't `{"$id": ...}` is an error, not a guess.

Documents are written in batches of 1000, or about 8 MB of JSON,
whichever comes first: a batch holds every page it changes in memory
until it commits (§19.9), so one batch for a whole import would need
the database's size in memory. The price is that **an import isn't
atomic**. It stops at the first bad line (`Error::Import { line,
message }`) or existing id (`Error::DuplicateId`), and every batch
before that stays. The documented safe way: import into a new file and
switch over only if it succeeds.

Each collection's indexes are built with `ensure_index` once its
documents are in — one pass over the collection, instead of maintaining
every index on every insert. A collection line with no documents still
creates the collection, so empty collections survive the round trip.

## 30.4 Export: one snapshot under the read lock
The export holds the read lock from the first line to the last. So it
is a consistent snapshot of the whole database: a batch lands entirely
before or entirely after it, and documents in different collections
that refer to each other agree. Readers go on in parallel; writers wait.

That's the opposite of `cursor` (§29.4), which holds no lock between
items, and the first roadmap sketch had planned to export through a
cursor.
Rejected, because an export is a backup: read-committed per document
would let a batch that updates two collections appear half-applied. The
reasons the cursor avoids the lock don't apply here — the export is one
function call, so there's no forgotten guard and no lifetime problem,
and nobody writes to the database from inside it. The one rule: the
`Write` the export writes to must not write to the same database, which
would deadlock. Memory stays at one document plus the ids of one
collection.

## 30.5 Tests
- `export_then_import_reproduces_every_document_id_and_index`: 2500
  documents of random shapes — every value type nested three deep, keys
  that look like tags (`$id`, `$value`, `$object`, `$collection`, `_id`
  inside nested objects), top-level documents that aren't objects — plus
  a 100 KB document in overflow pages, an empty collection and an index
  on an empty one. The import must reproduce every collection, index,
  id and document byte for byte, also after a reopen; the rebuilt index
  must answer a query; and exporting the copy must give the same text.
- `floats_come_back_bit_for_bit` (`json.rs`): 200,000 random floats
  through text and back. Fails without `float_roundtrip` — checked.
- Tagged JSON: every value kind, `Int`/`Float` separation, plain JSON
  as input, malformed tags rejected, document lines with and without
  ids, base64 against RFC 4648's test vectors.
- A hand-written file (no ids, no `$indexes` on one collection) imports.
- Ten kinds of bad input each report their line number.
- An existing id in the second batch: the first batch stays, the second
  is rolled back entirely.
- `export_is_a_snapshot_writers_wait_for`: halfway through an export,
  another thread updates the last document. The export must contain the
  old version, and the update must land afterwards.

## 30.6 Limits
- Not atomic on import (§30.3).
- Writers wait for the whole export. For a large database that's
  seconds; a snapshot that doesn't block writers needs MVCC (ROADMAP.md).
- Reading `mongoexport` output directly (`$oid` is 12 bytes, not 16;
  `$date`, `$numberLong`) is left out: its `$oid` values aren't
  `DocId`s, so a migration from MongoDB needs decisions only the app
  can make. A small program using `import`'s format can do it.
