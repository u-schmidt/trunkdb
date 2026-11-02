# 161. The public API, gone through again (`query.rs`, `changes.rs`)

§118 went through the whole public API once and left a second pass to
when what came after it had settled (ROADMAP.md, "A settled API and
format"): the hook after each commit (§135), live queries and
aggregates (§137, §139, §145), the window (§139), `Condition::custom`
(§136), `Document`'s readers, rows of the application's own (§148),
and since then a sort by a key (§154), aggregates as text (§157),
`starts with` (§158) and a value's kind (§159).

As in §118, the API was listed from `rustdoc`'s JSON (nightly, `-Z
unstable-options --output-format json`), walked from the crate's root
through every public module and re-export, at §118's commit and now,
and the two lists compared: 52 types, traits and aliases then, 66 now;
262 items listed, types and methods and constants, then, 338 now. The
comparison was read whole, and the list as it is now, since what came
later changes how older items read.

## 161.1 `Kind`, before it was released
- **`Kind::ALL` was an array, `[Kind; 10]`.** `Kind` is
  `#[non_exhaustive]` so a kind can be added, a date without a time
  (ROADMAP.md); but the array's length is part of its type, so adding
  one would break every program that names the type or matches its
  length. It is a slice now, `&'static [Kind]`, as long as it needs to
  be.
- **`Kind::Date` tested for a `Document::DateTime`.** A date without a
  time, when it comes, is `Document::Date` by every name trunkdb uses,
  and its kind `Kind::Date`, which would then have been taken. The
  variant is `Kind::DateTime` now, named for what it tests. Its name in
  the text stays `date`, as date-times are written `date("…")` there and
  `trunkdb fields` prints them; what a date without a time is called in
  the text is decided with it.

Both came with §159, after 0.29.0, so no release has them: nothing to
adapt.

## 161.2 The hook's events aren't `Eq`
`Commit` and `Change` (§135) derived `Eq`, and §118.1 took `Eq` from
every report for the reason that holds for them too: it promises that
no field ever added holds a float. Fields a commit may plausibly get —
the time it was written, the fields an update changed, the document
itself for a subscriber that wants it without reading — and a
`Document` holds floats. The live queries' events, `Live` and
`LiveRow`, never had it, for that reason. Both are `PartialEq` alone
now; `==` works as before.

What it costs a program: a `Commit` or `Change` used as a key of a
`HashMap` or `HashSet`, or in a type that derives `Eq`. A set of the ids
changed is `HashSet<DocId>`, from `Change::id`.

## 161.3 Kept, on purpose
- **Values keep `Copy` and `Eq`:** `Kind`, `EntryCount` and `Release`,
  as `DocId`, `DateTime` and `SortOrder` do (§118.1): what they hold is
  fixed by what they are.
- **`Op` without `Copy`:** a pattern operator (ROADMAP.md, Planned) may
  carry its compiled pattern. Adding `Copy` later breaks nothing.
- **`Send`, `Sync` and unwind safety,** which §118 didn't list: every
  public type is `Send`, and all but six `Sync` and unwind-safe.
  `Changes`, `LiveQuery` and `LiveAggregate` aren't `Sync`, a channel's
  receiving end each, one reader at a time (`Sync` can come later,
  breaking nothing); `Error` isn't unwind-safe, as `std::io::Error`
  isn't `RefUnwindSafe`, nor are `map::IterMut` and `map::ValuesMut`, as
  no `&mut` is. `Database`, `Collection`, `Snapshot`, `View`,
  `Filter` (closures in it are `Send + Sync`), `Cursor` and the rest
  are all three. Losing one later would break programs; nothing here
  is likely to lose one.
- **`UnnamedColumn::by` on every column,** refused as the aggregate
  runs on any but `first` and `last`: a type per kind of column would
  double the builder's types for one method.
- **`explain(&Filter)` beside `explain_run(Filter)`:** as §118 left it;
  `explain` only plans, and is called with the filter a `find` takes
  next.
- **`Document::get` giving `None` for a path with `[*]`,** which reaches
  any number of values: `as_array` reads them.
- **`Changes::recv` with no error,** where `LiveQuery::recv` has one: a
  subscriber is never cut off (§135), while a live query ends on a
  panic in a closure of its filter.

## 161.4 Left for additions
Each adds to the API and breaks nothing, so none is needed before 1.0:
`PartialEq` for `Filter`, `Aggregate` and `Update` (tests compare their
`Debug` text instead); `Live::doc`; `Serialize` and `Deserialize` for
`Document` (§118.3).

## 161.5 Tests
- `options_and_reports_are_not_eq` (`tests/ui`), §118's compile-fail
  test, asks for `Commit` and `Change` as `Eq` too, one error each, on
  stable and on Rust 1.89.
- `the_names_are_trunkdb_fields_names` (`tests/kinds.rs`) binds
  `Kind::ALL` as a `&'static [Kind]`, which an array isn't, and checks `Kind::DateTime` is written `date`.
- The full test suite, clippy, the MSRV build (1.89).

## 161.6 Mutations
- `Eq` put back on `Change`, and on both: caught by the compile-fail
  test, its errors no longer as expected. On `Commit` alone it doesn't
  build, as a `Commit` holds `Change`s.
- `Kind::ALL` an array again: the library builds, and `tests/kinds.rs`
  doesn't.
