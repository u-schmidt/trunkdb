# 31. Nested-field paths (`query.rs`, `collection.rs`)

Real and tested: a condition, a sort and an index can name a field
inside nested objects with a dotted path.

```rust
people.ensure_index("address.city")?;   // like LiteDB's "$.Address.City"
let in_berlin = Filter {
    conditions: vec![Condition {
        field: "address.city".into(),
        op: Op::Eq,
        value: Document::String("Berlin".into()),
    }],
    sort: Some(Sort { field: "address.zip".into(), order: SortOrder::Asc }),
    ..Filter::default()
};
people.find(in_berlin)?;                 // reads the index, sorts by zip
```

On the typed path a nested struct serializes to a nested object, so
`address.city` is simply the `city` field of a `Person`'s `address`.

## 31.1 One lookup for filters, sorts and index keys
`query::field_value` walks the path, one object per dot. It was already
the single place where conditions, sorts and index keys read a field;
now it follows a path instead of reading one key. So a filter and an
index can't disagree about where a document's value is — which matters
because every index candidate is rechecked against the filter (§28.3):
if the two looked in different places, documents would silently go
missing from indexed finds.

No file-format change: the catalog already stores an index's field as a
string (§28.5), and a path is just a string with dots in it. The key
encoding, the key-size cap and "one entry per document per index" stay
as they are.

## 31.2 A dot always separates
`a.b` always means "field `b` of the object in field `a`", never a key
literally named `a.b`. Such keys can still be stored and read back; a
path just can't reach them. That's MongoDB's rule too.

Rejected: trying the literal key first and falling back to the path. A
path would then have two possible answers, and which one counts would
depend on each document — adding an unrelated `"a.b"` key would
silently change what a document is indexed under. Rejected: an escape
syntax (`a\.b`). It adds a small language for a case that barely comes
up: serde field names can't contain dots unless renamed on purpose.

## 31.3 Arrays aren't walked into
A step that lands on an array stops the walk: `items.name` finds
nothing in `{"items": [{"name": …}]}`, and neither does `items.0.name`.

Rejected: numeric steps into arrays (`items.0.name`). They're rarely what
you want, and they'd suggest `items.name` should mean "any element",
which it doesn't. Deferred: that "any element" meaning (MongoDB's
multikey indexes). An index would need one entry per element, so one
document several entries — breaking the "one entry per document per
index" rule the write path relies on (§28.6). It belongs with array
conditions in filters, which don't exist yet either. *Done in §42, as
an explicit `[*]` step: `items[*].name`.*

## 31.4 What `ensure_index` rejects
A path with an empty part (`a..b`, `.a`, `a.`, the empty string), and
`_id` or anything below it — `_id` is the primary key, and an id has no
fields. Both mistakes would otherwise create an index where every
document sits under null (a missing field, §32). Filters don't check
paths: `matches` has no error to return, and a malformed path finds no
field, so it behaves like a missing one — null (§32).

## 31.5 Files from 0.3.0
In 0.3.0, a field name containing a dot meant a top-level key with that
literal name. An index created then on such a name was filled from those
keys. Now the name is a path, so writes look elsewhere: removing an old
entry finds nothing to remove (a no-op, §28.6), stale entries stay, and
indexed finds can miss documents. Nothing in the file tells the two
meanings apart. The fix is to rebuild the index — `drop_index` and
`ensure_index`, or an export and import (§30), which rebuilds every
index. Indexes on names without a dot, i.e. all normal ones, are
unaffected. (Since §32 the format version is 4, so a 0.3.0 file goes
through export and import anyway, and this can't happen.)

## 31.6 Tests
- `nested_path_indexes_match_full_scans_through_every_kind_of_write`:
  an index on `a.b.c` over documents that put the value at that path, one
  level short, inside an array, under keys with dots (`"a.b.c"`,
  `"b.c"`), or nowhere. It is built from existing documents, then kept
  through inserts, updates that move values in and out of the path's
  reach, and deletes; after all of them and again after a reopen, 300
  random filters must find through the index exactly what a full scan
  finds. Checked by breaking it on purpose: index keys read the old
  way (top-level key only) while filters walk the path — it fails.
- `typed_nested_fields_are_filtered_indexed_and_sorted_by_path`: nested
  structs, an index on `address.city`, a sort by `address.zip`, and an
  update that moves a document to another city and out of the result.
- Path semantics in `query.rs`: nested matches, missing and non-object
  steps, no deep search, arrays not entered, dotted keys not reached,
  malformed paths matching nothing, sort by a nested field.
- `ensure_index` rejects `_id`, `_id.x` and five malformed paths.
- The export round trip (§30.5) now includes an index on a nested path.

## 31.7 Cost and limits
- A lookup splits the path as it walks — no allocation — so a top-level
  field costs what it did before.
- No array traversal (§31.3; since §42 with `[*]`), no escaping of
  dots (§31.2).
- Still one field per index: compound indexes are still open (ROADMAP.md);
  unique ones came in §33.
