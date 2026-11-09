# 178. Text a program passes, read by the library (`update.rs`, `query_text.rs`, `batch_lines.rs`, `bin/trunkdb.rs`)

The first step of the C ABI (§177.10). A program that speaks JSON, a C#
one through the C ABI, Python later, a Rust one taking requests, passes
updates as MongoDB's operators, values for a template's parameters, and
a batch's ops as lines. Only the command read them: its `update_from`,
`batch_line`, `batch_of` and `failed_line` lived in `bin/trunkdb.rs`.
They move into the library, public, and the command reads through them,
so one code path serves every caller and the command's tests cover it.
No file format changed; what the command prints is as it was, but for
one corner of 178.4.

```rust
use trunkdb::BatchLines;
use trunkdb::query::{Params, Update};

let params: Params = r#"{"page": "/home"}"#.parse()?;
let ops = BatchLines::read(lines, &params)?;
let (batch, ids) = ops.batch(&db)?;
let written = batch.commit()?;

let update: Update = r#"{"$inc": {"hits": 1}}"#.parse()?;
```

## 178.1 `Update` from operators
`TryFrom<Document>` and `FromStr` for `Update`: an object of operators,
each an object of paths and what to do there, as `trunkdb update`
takes one (§156.2). `$set`, `$setOnInsert`, `$unset` (an object or an
array of paths), `$inc`, `$mul`, `$min`, `$max`, `$rename`, `$push` and
`$addToSet` (each with `$each`), `$pull`, `$pop` (1 or -1). A field
that isn't an operator is refused: a whole document isn't replaced. A
mistake is a `ParseUpdateError`, its text what the command printed.

## 178.2 `Params` from an object
- **`TryFrom<Document>` and `FromStr`:** each field's name a
  parameter's, without its `:`; the values are documents, so a tagged
  `{"$date": …}` is a date-time, as everywhere JSON is read (§30). Not
  an object, or not JSON, is a `ParseParamsError`.
- **`names()`,** the names with a value, and **`only(names)`,** the
  values of those names: a filter and an aggregate passed with one set
  of values, or a batch's lines, each bind their own, as `bind` refuses
  a value its text has no parameter of (§163.4).
- The command's `--param name=value` keeps its own forms, `id("…")` and
  bare strings (§163.6): text a shell passes, not JSON.

## 178.3 `BatchLines`
- **`BatchLines::read(text, &params)`:** one op a line as §168.3 and
  §175.3 have them, blank lines passed over and counted, each filter
  bound to the values it has parameters for. Every line is read and
  checked before anything is written; the first mistake is the error,
  or else a value no filter has a parameter of.
- **`ops()`,** each a `LineOp` with its `line()`, `collection()` and
  `kind()`, an `OpKind`; `len()`, `is_empty()`.
- **`batch(&db)`:** the `Batch` to commit or dry-run, and each insert's
  new id, `None` for the other ops; an upsert's comes from `Written`
  (§175.2). An op the batch refuses when added, a precondition with a
  limit, is the error, with its line.
- **`failed_line(&db, &error)`:** the line a failed `commit` or
  `dry_run` came from: a precondition's own, any other's by dry runs of
  ever shorter beginnings (§168.5).
- **`BatchLineError`,** each kind with what the command prints:
  `Line` (not an op as written, with its message), `Filter` (the
  filter's text and its `ParseFilterError`, so the place can be
  marked), `Bind` (the text and its `BindError`), `Unused` (a value's
  name), `Refused` (the line and the `Error`). `line()` for each but
  `Unused`.

## 178.4 What changed in moving
- **`failed_line` returns `None` when the whole batch runs.** The
  command called it only after a failure, under a file lock no other
  process gets past (§27), so it assumed one; a program sharing a
  handle among threads can have another batch come between the failure
  and the search. Before, the last line was blamed; now none is. One
  dry run more, only after a failure.
- **Several unused `--param`s:** the command named the first given; it
  names the first by name now, as `Params` keeps them. One unused value,
  the case anyone meets, is named as before.
- **The command reads all of its input before checking it,** where it
  checked line by line. The first mistake is the same; a file of ops is
  held in memory once, as the batch holds its ops anyway.
- **Not moved yet:** the reports `--json` prints (§155), and JSON for
  `Written`, `Stats` and the rest (§177.7). The C ABI fixes their shape
  and tests it; they move with it, the command's onto them.

## 178.5 Rejected
- **`impl FromStr for Batch`:** a batch belongs to a database, which a
  text alone doesn't name, and its line numbers would be lost after.
- **The command's `--param` forms in `Params`'s `FromStr`:** JSON is what
  a program has; `id("…")` and a bare string are for a shell, and
  would make `"5"` and `5` ambiguous.
- **An error type for all three:** each reader says what it read,
  as `ParseIdError` and `ParseDateTimeError` do.

## 178.6 Tests
- `tests/text_for_programs.rs`:
  - each operator as JSON, both ways in, does what the builder's method
    does, on the same document: `$set` of a date and a nested path,
    `$setOnInsert`, `$unset` in both forms, `$inc`, `$mul`, `$min`,
    `$max`, `$rename`, `$push` one, with `$each` and an object,
    `$addToSet` one and with `$each`, `$pull`, `$pop` both ends;
  - eleven updates that aren't, each saying why in the command's words;
  - parameters from an object, tags read as their kinds, `names` in
    order, a filter bound to `only` its own after `bind` refused them
    all, values that aren't an object;
  - every op of a counter's batch read, lines counted with the blank
    ones, committed, the insert's and the upsert's ids; read again, the
    upsert finds its document;
  - eighteen lines that aren't ops, each named at its line, the first of
    two; a filter that doesn't read, with its offset; a missing value;
    unused values, the first by name;
  - values shared by two lines; none used by an empty text; blank lines
    only;
  - a precondition with a limit refused at its line; a unique index
    failing the third of five, found from `commit` and `dry_run`; a
    precondition's own line; `None` when nothing fails, and for no ops;
  - ids in line order, an upsert's from `Written`.
- `tests/cli.rs` passes as it was, every message `trunkdb update` and
  `trunkdb batch` print, and reads one more: the hint a batch line's
  missing value gets, `give it with --param a=…`.

## 178.7 Mutated
Fourteen ways by hand: `$pop`'s ends swapped; `$each` ignored by
`$push`; `$unset`'s array form ignored; `only` keeping every value; no
check for unused values; the values a filter took not recorded; lines
counted without the blank ones; an empty filter let through without
`all`; `require` and `require_none` swapped in the batch, or in
`kind`; a precondition's line read one op off; no `None` when the
whole batch runs; `Refused` without its line; and in the command, a
batch line's missing value printed without its place and hint. The
first thirteen fail `tests/text_for_programs.rs` alone, the last
`tests/cli.rs`. Two of them stood at first: the test of a batch that
runs passed a precondition's error, which never reaches that check,
and no test read the hint a missing value gets in a batch line; both
tests now do. Nothing in storage or in reading changed, so nothing was
fuzzed.
