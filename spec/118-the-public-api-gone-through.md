# 118. The public API, gone through (`database.rs`, `export.rs`, `catalog.rs`, `explain.rs`, `stats.rs`, `check.rs`, `compact.rs`, `snapshot.rs`, `query.rs`, `document.rs`, `serde_bridge.rs`, `fuzzing.rs`)

Everything public is a promise at 1.0 (ROADMAP.md, "Before 1.0"). So
the whole of it was listed once, from the compiler's own account:
`rustdoc`'s JSON output, walked from the crate's root through every
public module and re-export. It came to 55 types, traits and aliases,
with about 200 methods. §116 and §117 came from a first look; this is
the rest.

Most of it was as it should be:
- the traits (`IntoDocument`, `IndexFields`) are sealed;
- the reports are `#[non_exhaustive]`;
- the options and query types keep their fields private and are built
  with methods;
- the errors' variants with fields are marked.

What wasn't is below.

## 118.1 Derives that close doors
Options and reports derived `Copy` and `Eq`. Each is a promise about
every field the type will ever have:
- `Copy` rules out a `String`, a `Vec`, a callback;
- `Eq` rules out an `f64`, or a `Condition`, which holds floats.

Plausible fields that would have broken them:
- `IndexOptions`: a partial index, "only where `status = "open"`",
  holds a `Condition`;
- `OpenOptions`, `ImportOptions`: a folder for compaction's scratch
  file, a callback for progress;
- the reports: a ratio, a fill factor, a list.

Taking `Copy` or `Eq` away after 1.0 breaks every program that relied
on it. So the options and reports now derive `Clone`, `Debug` and
`PartialEq` (and `Default` where they had it), and neither:
- `OpenOptions`, `ImportOptions`, `IndexOptions`;
- `IndexInfo`, `ExecutionStats`, `SortWork`, `QueryPlan`, `Compacted`,
  `SnapshotInfo`, `PageUse`, `Recovered`, `Summary`, `CheckReport`,
  `FileInfo`, `Stats`, `CollectionStats`, `IndexStats`.

**Values keep both:** `DocId`, `DateTime`, `SortOrder`, `Upserted`. What
they hold is fixed, by the file format or by what they are.

**The errors keep `Eq`** where they had it: their fields are messages
and numbers.

What it costs a program: an option or a report reused after it's
passed on is `.clone()`d. The options' builder methods take `self`, so
`options.checkpoint_pages(0)` used twice is `options.clone()` once. A
match on a report's `Option` borrows it, `if let Some(sort) =
&explained.sort`. In trunkdb, the command took two such borrows, and
the tests about twenty clones.

## 118.2 Public by accident
- **`Document::nests_too_deep()`:** the check every write makes,
  described in terms of how an export nests its tags. A write refuses
  a document too deep anyway, with an error that says so.
- **`Filter::apply_to(items, doc_of)`:** `apply` for items that carry a
  document, written for `delete_many`'s ids. Nothing outside the
  library used it. `Filter::apply`, the filter run over documents in
  memory, stays: it's how a program checks a filter without a
  database.

Both are `pub(crate)` now.

## 118.3 `Document` is no `Deserializer`
`Document` implemented serde's `Deserializer` and `IntoDeserializer`.
That's how `into_value::<T>()` turned it into a `T`, but it was a second,
public way to do the same — `T::deserialize(doc)` — and every detail of
the impl a promise: how a date-time reads as a string, an id as text,
an enum from an object of one field.

The impl now belongs to `Reader`, a wrapper private to the crate, and
`into_value` is the one way in. The reads from stored bytes
(`serde_stored.rs`, §99) fall back to it the same way.

Seen in passing: `Document` implements neither `Serialize` nor
`Deserialize`, so a program's struct can't hold a `Document` field.
That's an addition, for 1.x if asked for.

## 118.4 The `fuzzing` feature
`trunkdb::fuzzing`, behind the `fuzzing` feature and hidden from the
docs, is the fuzz targets' hook (§55). A feature is part of a crate's
API, so `Cargo.toml` and the module's docs now say it's outside the
semver promise: it changes with the fuzz targets, in any release.

## 118.5 Kept, on purpose
- **`DocId(pub [u8; 16])`:** the file format fixes an id at 16 bytes,
  so there's nothing for a private field to protect.
- **`Upserted` and `SortOrder` exhaustive:** a match with exactly these
  arms is what they're for.
- **`Filter::apply`, `Condition::matches`, `Aggregate::apply`:** the
  meaning of a filter or an aggregate, run in memory; the fuzz target's
  model rests on them (§105).
- **The constants** (`Database::MAX_*`, `Document::MAX_NESTING`): limits
  a program checks against; raising one later breaks nothing.
- **The `map` module and `Map` at the root:** the iterators need a path
  (§116).

## 118.6 Tests
Compile-fail tests in `tests/ui`, run by `tests/document_macro.rs` with
the `derive` feature on stable and on the MSRV:
- `options_and_reports_are_not_copy`: each of the 17 types asked for as
  `Copy`, one error each;
- `options_and_reports_are_not_eq`: each asked for as `Eq`, one error
  each;
- `internal_helpers_are_private`: `nests_too_deep` and `apply_to`
  called from outside, one error each.

A `compile_fail,E0277` doc test on `into_value`: `String::deserialize`
of a `Document` doesn't compile. A doc test checks only the error's
code. The message lists the types that are `Deserializer`s, `serde_json`'s
among them, and would change with any update of those crates.

The calls in the first two tests start past line 9, and the helper
they bound is below them. Rust 1.89 and stable lay out a message quoting
lines of one and of two digits differently (§117.4).

Every other test runs as before: `Reader` reads what `Document` did, and
the serde bridge's tests and every typed read go through it.

## 118.7 Mutations
Each caught:
- `Copy` put back on `OpenOptions` (the first test), and on `SortWork`
  (the same);
- `Eq` put back on `IndexStats`, and on `QueryPlan` (the second);
- `nests_too_deep`, and `apply_to`, public again (the third);
- `Document` a `Deserializer` again, forwarding to `Reader` (the doc
  test);
- an `Option`'s `Some` read as the value itself (the library's tests,
  four of them first).
