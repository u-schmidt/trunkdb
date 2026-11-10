# 184. Writes that do what they say (`bindings/dotnet/`, `bin/trunkdb.rs`, `collection.rs`)

A review of updates, deletes, batches, upserts and the .NET and C entry
points found two defects and one hazard that wasn't documented. In each
case a write, or its preview, could do something other than what the
caller wrote or saw.

## 184.1 .NET text with a NUL is refused
Every `[LibraryImport]` of the C# wrapper (§180) that takes text used
`StringMarshalling.Utf8`, which keeps a NUL inside a string and ends it
with another. The library reads each argument as a C string,
`CStr::from_ptr`, up to the first NUL. So text with a NUL reached it
cut short, and silently:

- `DeleteMany("status = \"open\"\0 and title = \"a\"")` deleted every
  open document: the condition after the NUL never arrived.
- `Collection<T>("tasks\0 2")` wrote to `tasks`.
- `Database.Open("app.trunkdb\0x")`, `Export`, `Import`: another file.

A NUL in such text has to come from somewhere, usually input pasted
into a filter, which a parameter (§163) would have kept whole: JSON
escapes a NUL, and the library reads it back. But the wrapper had no
business passing text it knew the library would read differently.

Now every import that takes text, 21 of them, marshals it through
`NulFreeUtf8`: as `Utf8StringMarshaller` does, after refusing a string
that holds a NUL with `ArgumentException`, which says where the NUL is
and why. It's one place for every call, and a new import that takes
text gets it by being declared like the others.

Rejected:
- **A check in each public method:** a place in every method to keep
  in step, and a method added later without one is the same hole.
- **Passing a length with every string:** the C ABI (§179) takes C
  strings, as every C caller expects. A C caller can't pass a NUL in
  one, so the ABI itself has nothing to fix.

## 184.2 A dry run rehearses the write
`trunkdb update … --dry-run` and `delete … --dry-run` (§156.3) counted
the filter's matches with `count`. That was a guess at what the write
would do, and wrong in four ways:

- An update the run would fail, an `$inc` of a string, a unique index
  broken, was previewed as a success.
- A document the operators would leave as it was was counted as
  updated; the run neither writes nor counts it.
- `--upsert` with several matches previewed "would update 2
  documents"; the run fails with `Error::MultipleMatches`.
- `count` takes a filter's `limit` and `offset`, and `upsert_fields`
  ignores them. With `offset 1`, the preview said "would insert" and
  the run updated the document the filter found.

Now `update` and `delete` each make one op of a `Batch`
(`update_many`, `upsert_fields` or `delete_many`, §168) and either
`commit` it or, with `--dry-run`, `dry_run` it (§168.4). The rehearsal
runs the op under the writer's lock, writes nothing, and fails as the
run would. So the preview is the write, not a count. What it
prints follows `Written`: the documents changed, or for an upsert
whether it inserts, updates or leaves its document as it was ("would
leave 1 document as it was", "left 1 document as it was").

A batch's upsert refuses a filter with a sort, `limit` or `offset`
(§175), where `Collection::upsert_fields` ignores them. The command now
refuses such an upsert, in the preview and in the run alike, saying
why; before, the run quietly ignored them.

`--all` (§156.3) still counts: it asks whether the filter takes every
document, which the count answers.

## 184.3 The typed `update_many` drops what its type lacks
`Collection<T>::update_many` decodes each match into `T`, runs the
closure, and stores the `T`. A stored field `T` doesn't declare is
lost, even when the closure changes nothing. An older build of an
application, its `T` without a field a newer build added, erases that
field from every document it touches. This is the round trip by
design, as for the typed `update`, `upsert` and `insert`, which
GUIDE.md already warned of. `update_many` wasn't on that list, and it
is the one that looks like it changes only what the closure changes.

Now its documentation says so in bold, and GUIDE.md names it with the
others. Both point to `update_fields`, which changes only what its
operators name, or the untyped `update_many`.

Rejected:
- **`#[serde(flatten)]` into a map to carry unknown fields:** a
  `Document` isn't serde's, so the map would be of `serde_json::Value`,
  and a date or binary field would come back a string or an array of
  numbers. That changes the field instead of dropping it.
- **Merging the stored fields `T` lacks back in:** a field `T` has
  but leaves out with `skip_serializing_if`, or one the closure meant
  to clear, couldn't be told from one `T` never knew. The method would
  stop meaning "store this `T`".

## 184.4 Tests
- `.NET`: `Text_with_a_nul_is_refused_before_it_reaches_the_library`.
  A NUL in a filter, a collection name, a path, a batch's lines and a
  database path is each refused with `ArgumentException`, and nothing
  is deleted, updated, dropped or exported. A NUL in a parameter's
  value is kept.
- `tests/cli.rs`, `a_dry_run_rehearses_the_write`. A dry run and a run
  each fail alike on a unique index, an `$inc` of a string, an upsert
  of two matches and an upsert with an `offset`, and the file is byte
  for byte as before. A dry run counts only the documents that would
  change. An upsert that would leave its document says so, and leaves
  it.
- `collection.rs`, `a_typed_update_many_drops_the_fields_its_type_lacks`.
  The typed `update_many` of a smaller type with an empty closure drops
  a field and counts the document; `update_fields` keeps the field.

Mutations, each caught: the wrapper's imports back on
`StringMarshalling.Utf8`; a dry run that counts again.
