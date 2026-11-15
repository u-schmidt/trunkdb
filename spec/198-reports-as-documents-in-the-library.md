# 198. Reports as documents, in the library (`reports.rs`, `query_text.rs`, `trunkdb-ffi/`, `bin/trunkdb.rs`)

Step 1 of the Python bindings (§197.11). §179 made the reports' JSON in
`trunkdb-ffi`, "until a second user comes", and the command made the
same shapes again for `--json` (§155). The Python bindings are the
second user: the shapes move into the library, once, as documents, and
the C ABI and the command hand on what it makes. Nothing a program sees
changes.

## 198.1 What moved
- **`to_document`** on `CheckReport`, `Compacted`, `Summary`,
  `IndexInfo`, `FileInfo`, `IndexStats` and `Stats`: each the object the
  C ABI returned (§179), fields in the same order. Counts and sizes are
  `Int`; one past `i64`, which no count of pages or bytes reaches, would
  be its largest.
- **`BatchLines::report(written, ids, dry_run)`:** a batch's `{"ops":
  […], "total": …, "dry_run": …}`, each op's line, `op`, `count`, an
  insert's or an upsert's `id` and an upsert's `inserted`; no ids on a
  dry run. The id is its text, a `String`, as the C ABI hands every id
  out, not an `Id`, which its JSON would write as `{"$id": …}`.
- **`OpKind::name`,** the op as a batch's line names it, which the C ABI
  had as a function of its own.
- **`Params::unused(names)`:** the first value none of a call's texts
  has a parameter of, where each text is bound to `only` its own (§178);
  the call refuses it, as `bind` refuses one (§163.4).

`reports.rs` holds the reports' `to_document`s, beside each other as
the shapes one reader reads.

## 198.2 Who hands them on
- **The C ABI** writes each document as its JSON, `Display` (§71),
  where it built a `serde_json::Value`; `report.rs` is gone, and so is
  its op names' function.
- **The command:** `check --json` prints `CheckReport::to_document`;
  `info --json`'s index entries are `IndexInfo::to_document`, read into
  the object the command makes. The rest of `info --json` is the
  command's own.
- **The Python bindings** will turn them into dicts (§197.3).

## 198.3 Checked unchanged
- **Byte for byte against `main`:** the C ABI's `check`, `stats`,
  `compact`, `indexes`, `export` and `batch` on the same calls, through
  the library built from `main` and from this change, side by side
  (Python's `ctypes`, a script outside the repository); and the
  command's `check --json` and `info --json` on a copy of the 0.40.0
  fixture. All the same; the batch's new upserted id blanked in both.
- **The C ABI's tests and the C program,** whose report assertions are
  unchanged, and the command's.

## 198.4 Rejected
- **The reports as `serde_json::Value`** in the library: `serde_json`
  would be in its public API, which §116's promise keeps to `serde`
  alone (ROADMAP.md, "A settled API and format").
- **`Serialize` on each report:** each binding would still need the
  shape's names in a serializer of its own, and a field renamed for one
  would change for all; a document is what every binding already reads.
- **The batch's ids as `Id`:** for Python a `DocId` is nicer, but the
  C ABI's JSON would change. The Python bindings turn an op's `id` into
  a `DocId` themselves.
- **The command's whole `info --json` as a library report:** it is what
  the command gathers from several calls, not one report.

## 198.5 Tests
- `reports.rs`, `reports_are_documents_of_their_shapes`: `check`'s
  report whole; an index's; `stats`'s keys at the top, of its file and
  of an index's statistics; `compact`'s whole, after documents written
  and deleted, so that its two counts differ; an export's summary
  whole.
- `reports.rs`, `a_batchs_report_names_each_ops_line`: lines 1, 3 and
  4, a blank line between, and 5; each op's name, `require_none`
  among them; an upsert's `inserted`; a `require`'s count 0; ids as
  text, the inserted one `batch`'s, none on a dry run; the total and
  `dry_run`.
- `reports.rs`, `a_value_no_text_takes_is_found`.
- `trunkdb-ffi`'s 21 tests, the C program, `tests/cli.rs`.

## 198.6 Mutations
Each alone, its tests run, the file restored:

| Mutation | Test |
|---|---|
| `check`'s `ok` always true | caught: the report's test, `trunkdb-ffi`'s, the command's |
| a dry run's ops with ids | caught: the batch's test, `trunkdb-ffi`'s |
| an op's id an `Id`, not its text | caught: the same |
| an index's `sparse` its `unique` | caught: the index's report, `trunkdb-ffi`'s, the command's |
| a summary never `ended` | caught: the summary's, `trunkdb-ffi`'s |
| `compact`'s pages after as before | passed at first, the counts equal in the test; caught once documents were deleted before it |
| an index's `depth` left out | caught: `stats`'s keys |
| `unused` finding a value a text takes | caught: its test, `trunkdb-ffi`'s |
| `require_none` named `require` | passed at first, no such op in the test; caught once one was added |
