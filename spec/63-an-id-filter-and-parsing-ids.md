# 63. A filter on the id, and ids parsed from text (`query.rs`, `document.rs`, `lib.rs`)

Two small gaps around ids, both found by an application built on
trunkdb:

- **Filtering by id meant spelling `"_id"`.** Filters see a field by
  its stored name, after serde's `rename` (§59), so a struct's
  `#[serde(rename = "_id")] id` field is `"_id"` to a filter, and
  `eq("id", id)` finds nothing, without an error. Since §61, a filter
  on the id is also the fast way to fetch documents by id, so it's worth
  a name of its own.
- **An id could be written as text but not read back.** `DocId` had
  `Display` (a UUID) but no `FromStr`, so a command line taking an id
  had to go through the `uuid` crate and `DocId(*uuid.as_bytes())`.

```rust
let id: DocId = args[1].parse()?;                        // "0192…-…" → DocId
let task = tasks.find_one(Filter::new().id(id))?;        // eq("_id", id), a lookup
let some = tasks.find(Filter::new().any_of(ids.into_iter().map(Condition::id)))?;
```

## 63.1 `Filter::id` and `Condition::id`
`Filter::new().id(id)` adds `eq("_id", id)`; `Condition::id(id)` is
that condition, for an OR or any other group. They build exactly what
`eq("_id", id)` builds, so the planner names the documents by id and
looks them up in the primary index (§61.1): `explain` shows `ById`.
They take a `DocId`, not anything that converts into a `Document`: an
id's string never matches an id (§59.5), and now it doesn't compile
either.

Rejected:
- **A public `ID_FIELD` constant** (or `DocId::FIELD`). A filter was the
  one place the name mattered and could go wrong silently; `id` covers
  it. Sorting by id is rare, and documents with equal sort keys already
  come in id order (§47). A constant can still be added later without
  breaking anything.
- **A `Filter::ids(ids)` for several at once**, an `in` on the id:
  `any_of(ids.into_iter().map(Condition::id))` says it in one line, as
  §61.3 decided for `eq`. The name is also the planner's own
  (`Filter::ids`, the ids a filter names).
- **Warning when a filter names `"id"`:** a filter doesn't know what
  type its collection holds, let alone its serde names; `"id"` is a
  field like any other in an untyped collection.

## 63.2 `DocId: FromStr`
`"…".parse::<DocId>()` reads what `Display` writes, and the other ways
UUIDs are written: without hyphens, in braces, as `urn:uuid:…`, in
upper case. `id.to_string().parse()` gives `id` back. The serde impl and
the serde bridge, which parsed ids through a private `DocId::parse`
before, now use it too.

A string that isn't an id is a `ParseIdError`, exported from the crate
root. Its message names the string and why:
`"abc" is not a document id: invalid length: expected length 32 for
simple format, found 3`. It implements `std::error::Error`, so `?` turns
it into a `Box<dyn Error>` or an application's own error.

Rejected:
- **`uuid::Error` as the error type:** trunkdb's API would then change
  whenever `uuid`'s did. `ParseIdError` keeps it in a private field.
- **A variant of `trunkdb::Error`:** parsing touches no database; std's
  `ParseIntError` is a type of its own for the same reason.
- **`TryFrom<&str>` instead:** `FromStr` is what `str::parse` needs, and
  `parse` is what Rust code reaches for.

## 63.3 Limits
- **`eq("id", id)` still finds nothing, silently**, for code that
  doesn't use `id`. The `#[trunkdb::document]` attribute, open in
  ROADMAP.md, would take on the same problem from the struct's side.

## 63.4 Tests
- `document.rs`: an id and its string go back and forth, `NIL` too;
  five ways of writing the same UUID parse to it; an empty, a short, a
  one-too-short and a one-too-long string don't, and the message names
  the string.
- `query.rs`: `Filter::id` and `Condition::id` name their ids as
  `eq("_id", ...)` does, alone, next to another condition and in an OR;
  they match on `_id`, not on a field called `id`.
- `collection.rs`: a typed struct found through `Filter::id` from an id
  parsed out of its string, by `ById`; `eq("id", ...)` finds nothing; an
  OR of two `Condition::id`s finds both, by `ById`.
- Checked by breaking it on purpose, six ways. Each of these fails a
  test:
  - `Condition::id` on the field `id`, or as `ne`; `Filter::id` adding
    nothing;
  - parsing that ignores the text; an error message without the string,
    or with an empty one.
