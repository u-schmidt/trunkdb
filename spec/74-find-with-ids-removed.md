# 74. `find_with_ids` removed, and a cursor of `T` (`collection.rs`, `cursor.rs`)

The first of the API questions for 1.0 (ROADMAP.md, "Before 1.0"): a
breaking change that costs little now and could not be made within 1.x.
`find_with_ids` and `find_one_with_id`, deprecated since 0.12.0 (§59.4),
are gone, typed and untyped, and a `cursor` hands out `T`, as `find`
does, instead of `(DocId, T)`:

```rust
for task in tasks.cursor(Filter::new().eq("done", false))? {
    let task = task?;
    println!("{} {}", task.id.unwrap(), task.title);
}
```

## 74.1 Where the id comes from now
As in MongoDB: every document has an id, from its insert, stored with
it, and every read puts it into the document's `_id` (§59). So the id
is in the document, and a type that wants it declares the field:

```rust
#[derive(Serialize, Deserialize)]
struct Task {
    #[serde(rename = "_id")]
    id: Option<DocId>,
    title: String,
}
```

What that leaves:
- **A type without the field** reads its documents as before; serde
  passes over the `_id` it has no place for, as it passes over any
  field the type doesn't declare. MongoDB's Rust driver does the same.
- **A type that can't get the field**, one from another crate, is
  wrapped, its own fields flattened into the wrapper's:

  ```rust
  #[derive(Serialize, Deserialize)]
  struct WithId {
      #[serde(rename = "_id")]
      id: Option<DocId>,
      #[serde(flatten)]
      user: other_crate::User,
  }
  ```

  The stored document is the same either way, so `Collection<WithId>`
  and `Collection<other_crate::User>` read one collection; filters,
  sorts and indexes name the flattened fields as they are (`"age"`, not
  `"user.age"`).
- **Documents that aren't objects** (`Collection<i64>`, or a
  `Collection<Document>` of strings) have no field to hold an id.
  `find` returns them as stored, and their ids are what `insert`
  returned, for `get`, `update` and `delete`. MongoDB and LiteDB don't
  store such documents at all.

## 74.2 What changed
- **`Cursor<T>`'s `Item`** is `crate::Result<T>`, no longer
  `crate::Result<(DocId, T)>`. It still holds the ids of its
  candidates, to read them one at a time (§29.4); it no longer hands
  them out. A sorted cursor keeps the found documents only, not their
  ids.
- **`find_one`** takes the cursor's first item as it is; it no longer
  unpacks a pair.
- **Tests** that need the ids of documents they didn't insert, or of
  documents that aren't objects, have `find_with_ids` and
  `find_one_with_id` still, as `#[cfg(test)]` and `pub(crate)`: in no
  build but the tests', and in no public API. The tests that allowed
  the deprecation warning don't need to.

Nothing changes in the file format, or in what `find` reads: it drops
the ids it reads with each document, as before, and `delete_many` and
`update_many` keep them to write the right documents.

Rejected:
- **`WithId<T>` in trunkdb**, the wrapper above as a type of its own:
  more API to keep stable, for five lines a user writes once. Purely
  additive, so possible after 1.0 if it's asked for.
- **Keeping `find_with_ids` for documents that aren't objects only:**
  a method for the one case MongoDB doesn't even allow, where `get`
  already serves.
- **A cursor of `(DocId, T)` beside a cursor of `T`:** the same answer
  as `find_with_ids`, under another name.

## 74.3 Migrating
- `find_with_ids(f)`, `find_one_with_id(f)`: give the type an `_id`
  field (74.1) and call `find(f)`, `find_one(f)`; the id is
  `doc.id.unwrap()`, never `None` on a read.
- `cursor(f)`: `for item in cursor { let (id, doc) = item?; ... }`
  becomes `let doc = item?;`, and the id `doc.id`.
- `Collection<Document>`: an object's id is its `_id` field,
  `Document::Id(id)`.

## 74.4 Tests
- `collection.rs`: the sync workload's find-then-update after a reopen
  (§5.3), through a struct carrying its id instead of
  `find_with_ids`; a struct wrapping a type without an id field, found
  through an index and a cursor, the bare type reading the same
  documents; documents that aren't objects found without ids, and
  read and updated by the ids their inserts returned. The cursor tests
  take `T` now: a streaming and a sorted cursor, a skipped one, one
  through ids, and the random nested filters (§36), whose streamed
  documents must carry the ids a scan finds.
- Checked by breaking it on purpose, five ways, each failing a test:
  a typed and an untyped `find_one` taking the second match; a sorted
  cursor reversed; a streaming cursor ignoring its skip, and yielding
  documents that don't match.
