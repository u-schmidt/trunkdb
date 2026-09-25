# 32. Null and missing fields (`query.rs`, `index/key.rs`, `collection.rs`)

Real and tested: a condition against `Null` finds documents where the
field holds null *and* documents that don't have the field at all.

```rust
#[derive(Serialize, Deserialize)]
struct Member { name: String, nick: Option<String> }   // C#: string? Nick

members.ensure_index("nick")?;
members.find(Filter { conditions: vec![Condition {
    field: "nick".into(), op: Op::Eq, value: Document::Null,
}], ..Filter::default() })?;   // every Member whose nick is None — via the index
```

Storing null always worked: `Document::Null`, and `Option::None` on the
typed path. Querying for it didn't: `Null` compared as "not
comparable", so `nick == null` matched nothing, and `nick != null`
matched every document that had the field — including the ones where
it was null.

## 32.1 Missing reads as null
A condition or a sort that looks at a field the document doesn't have
sees `Null` (`query::value_or_null`). That includes a path that can't be
followed (`address.city` when `address` is missing or a string, §31),
and a document that isn't an object. And `Null` compares equal to
`Null`. Together:

| condition | matches |
|---|---|
| `x == null`, `x <= null`, `x >= null` | `x` is null or missing |
| `x != null` | `x` is there and not null |
| `x < null`, `x > null` | nothing |
| `x == 5`, `x > 5`, `x contains "a"` | as before; never a null or missing `x` |
| `x != 5` | also a null or missing `x` |

The last row is the one behavior change beyond null itself: `x != 5`
always matched a *stored* null, but not a missing field. Now both.

Why equate them: on the typed path both come back as the same `None`
— serde fills a missing `Option` field with `None` — so a query that
told them apart would disagree with what the app reads back. And a field
added to a struct later is missing from every older document; if
`== null` skipped those, the query would silently return too few
results, the classic schema-less bug. SQL has no "missing" at all, and
LiteDB (a missing field reads as `BsonValue.Null`) and MongoDB (`{x:
null}` matches missing) equate them too.

Rejected: keeping them apart. What that buys — telling "cleared" from
"never set" — is rarely needed, and an `Exists` operator can add it
later without changing anything here.

## 32.2 Indexes hold null, and missing as null
`Null` became an indexed type with the lowest tag (`0`), one value,
and a missing field is indexed as null. So `== null`, `<= null` and
`>= null` read a range of the index, like any other value (§28.4), and
the existing range logic needed no change: a tag of its own keeps every
other type's range free of nulls.

The cost: every document now has an entry in every index — before, a
document without the field had none. For an index on a field most
documents lack, that's an entry per document that used to be free.
MongoDB makes the same trade by default. Rejected for now (added
since, §44): *sparse*
indexes (skip missing fields, as an option). Such an index couldn't
answer `== null`, so it would need its own planner rule; it's worth
adding only when an index on a rare field turns out to be too big.

## 32.3 Format version 4
An index built by format 3 has no entry for a null or missing field, so
it would silently miss documents for `== null`. That's a change in what
an index contains, so the format version is now 4 (§21.2), and a
format-3 file is refused. The error now says how to move it: export
with the trunkdb version that wrote it, import with this one (§30) —
which rebuilds every index.

Rejected: upgrading format-3 files in place, by rebuilding their
indexes on open. It would be trunkdb's first automatic migration — an
open that writes, plus a header change in the same batch — for files
that only exist in development so far. Export and import is the
documented path (§21.2) and already tested.

## 32.4 Tests
- `a_missing_field_is_null` (`query.rs`): every operator against null
  and against a value, on a null field, a missing field, a document
  that isn't an object, and a set field; plus a path whose parent is
  missing.
- `null_and_missing_fields_are_found_alike_through_the_index`: typed,
  with `Option` fields stored as null, left out by
  `skip_serializing_if`, and missing because the document was written
  with an older struct. `== null` and `!= null` find the right ones,
  `== null` through the index, and an update that clears a field moves
  its entry.
- The two index-versus-scan tests (§28.7, §31.6) already had null
  values in their random filters and documents without the field;
  they now check those through the index too. Checked by breaking it on
  purpose: with missing fields left out of the index again, both fail,
  and so does the typed test.
- Key encoding: null's key, and that `Eq null` covers it while number
  ranges don't.

## 32.5 Limits
- No `Exists` operator: "null or missing" is one state for queries.
  (§45 added one, next to the comparisons, which still don't tell them
  apart.)
- A sort still leaves null and missing where they are relative to other
  values (not first or last): `compare` orders only values of one kind.
  (§34.1 fixed that: they sort first.)
- Every index grows by one entry per document without the field (§32.2).
