# 114. `#[trunkdb::document]` (`trunkdb-derive/`, `src/lib.rs`)

A struct takes its id with `#[serde(rename = "_id")] id: Option<DocId>`
(§59). That works, but it asks a consumer to know two things that are
trunkdb's business: that the stored name is `_id`, and that serde's
`rename` is how a field gets it. Now a struct can say what it means:

```rust
#[trunkdb::document]
#[derive(Serialize, Deserialize)]
struct Task {
    #[id]
    id: Option<DocId>,
    title: String,
}
```

with the `derive` feature. Purely additive: without the feature nothing
is compiled, and the rename written by hand still works.

## 114.1 What it does
An attribute macro on the struct, `#[proc_macro_attribute]`: it is
handed the struct as tokens and returns the tokens compiled in its
place. It takes `#[id]` off the field it marks and puts
`#[serde(rename = "_id")]` there, and returns the rest as it came.
Attributes are expanded from the top down, so serde's derive, written
below it, sees the rename and never `#[id]`, which would be an unknown
attribute to the compiler if it were still there.

It refuses, as compile errors pointing at the code at fault:
- **an id that isn't `DocId` or `Option<DocId>`**, the type underlined;
- **`#[id]` on a second field**, the second `#[id]` underlined;
- **arguments**, `#[id(x)]`, `#[id = "x"]` or `#[trunkdb::document(x)]`;
- **anything but a struct**, by `syn`'s parse of an `ItemStruct`.

A struct without `#[id]` is a document without an id field, as one
without the rename is (§74): allowed.

## 114.2 The crate
A procedural macro is a program the compiler runs while it compiles its
caller, so Rust wants it in a crate of its own, `proc-macro = true`:
`trunkdb-derive/`, a member of trunkdb's workspace (`bench/` and
`fuzz/` stay workspaces of their own). trunkdb re-exports it behind the
`derive` feature, `#[cfg(feature = "derive")] pub use
trunkdb_derive::document`, as serde re-exports `serde_derive` behind
its `derive`. So a consumer depends on trunkdb alone and writes
`#[trunkdb::document]`; the documentation, with an example run as a
doctest, sits on that re-export, and docs.rs builds with the feature.

It parses with `syn` and prints with `quote`, as nearly every macro
does: `syn` 3, the version serde's derive already brings in, so one
`syn` is compiled, not two. The work is in a function returning
`syn::Result`, each check a `?`, and the macro turns an error into
`compile_error!` tokens with the code's span.

## 114.3 The type, as written
A macro sees tokens, not types: the compiler resolves names after it
runs. So the check is on the type's path, its last segment: `DocId`
without generic arguments, or `Option` with one argument that is that.
`trunkdb::DocId` and `std::option::Option<DocId>` pass; `String`,
`Vec<DocId>`, `Option<Option<DocId>>` and `&DocId` don't.

Two consequences, the same as for serde's own derive and the MongoDB
driver's: an alias, `type Key = DocId`, is refused though it would
work, and an unrelated type named `DocId` passes, to fail later in the
derive or at the first read. Rejected for that: a check by the
compiler, generated code like `fn check<T: IdField>()` over the field's
type with a trait trunkdb implements for both. It sees aliases, but its
error names a trait the consumer never wrote, in code they can't see;
the plain message at the type is worth more than aliases.

## 114.4 Rejected
- **The derive added by the macro**, `#[derive(Serialize,
  Deserialize)]` written for the consumer. A derive written by habit as
  well would implement the traits twice, an error in code nobody can
  see; the consumer's crate needs `serde` regardless, since the derive's
  code names it; and macros that change a detail (the MongoDB driver's,
  `sqlx::FromRow`, `serde_with`) leave the derives to the struct, where
  they can be read.
- **A derive, `#[derive(Document)]`**: a derive can only add items
  beside the struct, not change its fields, and the rename has to be on
  the field before serde's derive reads it.
- **`eq("id", …)` made to find the id.** Filters see stored names
  (§63.3); the struct's field name is gone after serialization.
  `Filter::id` stays the way to name it.

## 114.5 Tests
`tests/document_macro.rs`, with the feature (CI runs `cargo test
--features derive` on every platform and on 1.89):
- `#[id]` on an `Option<DocId>`: an insert with `None` makes the id, and
  `get` fills it in; without the rename `task.id` would be `None`.
- A plain `DocId`, given by the caller, read back.
- The id after other fields.
- Compile failures with `trybuild`, each against its `.stderr` in
  `tests/ui/`: a `String` id, `Vec<DocId>`, `Option<Option<DocId>>`,
  `#[id]` twice, `#[id(x)]`, `#[trunkdb::document(x)]`. The files name
  serde's traits in full, so no unused import adds a warning to the
  expected output.

## 114.6 Mutations
Each caught: the rename left out (2 tests), or given as `id` (2);
arguments to the struct's attribute (1) or to `#[id]` (1) accepted; a
second `#[id]` accepted (1), or the first never remembered (1); any
type accepted (1); the error spanned on the field instead of its type
(1); the field loop stopped at the first field without `#[id]` (build);
any wrapper of `DocId` accepted (1); `Option` checked again inside
`Option` (1); a bare `DocId` refused (build); any last segment taken
for `DocId` (1). Three survived at first, each until a test was added
for it: the loop stopped early (the id after other fields), any wrapper
(`Vec<DocId>`), `Option` inside `Option` (`Option<Option<DocId>>`).
Two survive, as equivalents: `Option` with two arguments and `DocId`
with arguments accepted, types the compiler refuses on its own.

## 114.7 Limits
- An alias of `DocId` is refused, and another type named `DocId`
  passes (114.3).
- Only structs with named fields carry an id: a tuple struct is
  serialized as an array, which trunkdb refuses (§95).
- `#[id]` is only `#[id]` inside `#[trunkdb::document]`; elsewhere the
  compiler doesn't know it.
