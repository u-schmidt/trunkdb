# 179. The C ABI (`trunkdb-ffi/`, `explain.rs`, `export.rs`, `bin/trunkdb.rs`)

Step 2 of §177.10: the 36 functions of §177.5 in a crate of their own,
`trunkdb-ffi`, built as `trunkdb.dll`, `libtrunkdb.dylib` or
`libtrunkdb.so`, and `libtrunkdb.a` to link in; `include/trunkdb.h`
declares them. A C program uses trunkdb with nothing else; the C#
wrapper (step 3) builds on it. Nothing in the `trunkdb` crate a Rust
program depends on changed but two additions, 179.4; no file format
changed.

```c
#include "trunkdb.h"
/* cc app.c -I trunkdb-ffi/include -L target/release -ltrunkdb */

trunkdb_db *db;
if (trunkdb_open("app.trunkdb", NULL, &db) != TRUNKDB_OK) {
    fprintf(stderr, "%s\n", trunkdb_error_message());
    return 1;
}
char *found;
trunkdb_find(db, NULL, "tasks", "status = :s limit 10", "{\"s\": \"open\"}", &found);
puts(found);
trunkdb_string_free(found);
trunkdb_close(db);
```

## 179.1 As §177 has it
Handles (177.2), strings (177.3), error codes, the thread's last error
as text and JSON (177.4), the functions (177.5), threads and panics
(177.6), the crate and its header (177.8), each as designed. What the
code settled that the design didn't:
- **A handle's tag is read as four bytes before anything else,** so a
  cursor passed as a database is never read as one; a `repr(C)` box
  with the tag first. Closing writes the closed tag with a volatile
  store, which the compiler can't drop as one to memory freed next.
- **What the library refuses as asked is `TRUNKDB_INVALID_ARGUMENT`,**
  not `TRUNKDB_IO`: an index on fields that have one of other options,
  a batch's precondition with a limit, an export over the database's
  own file. The library gives them as `Error::Io` of kind
  `InvalidInput`; to a C program they are the call's mistake. The
  details keep `"io": "InvalidInput"`.
- **A filter's text may be null or empty,** every document, as
  `Filter::new()`; for `trunkdb_update` and `trunkdb_delete_many` too,
  as in Rust. The command's `--all` guard (§156.3) is the command's.
- **A value no parameter takes is refused** by every call with a
  filter, as `bind` refuses one (§163.4); an aggregate's call binds the
  filter's and the aggregate's own, and refuses one neither takes.
- **An index's fields:** a bare name, `status`, a JSON string, or an
  array, `["team", "email"]`.
- **The reports' JSON** (`check`, `stats`, `compact`, an export's or
  import's summary, the indexes, a batch's ops) is made in the crate:
  §177.7 and §178.4 had them in the library, for the Python bindings
  too. The C ABI is their only user; they move when a second comes.
  `check`'s and the indexes' are the shapes `--json` prints (§155).
- **A batch's result:** `{"ops": [{"line": 1, "op": "insert", "count":
  1, "id": …}, …], "total": …, "dry_run": …}`; an upsert's `inserted`;
  no ids on a dry run, as `trunkdb batch` prints none, since the next
  call would choose others. A failure's details have its `line`.
- **The library's name is `trunkdb`,** the dependency on the `trunkdb`
  crate renamed `db` inside it; `doc = false`, as `cargo doc
  --workspace` refuses two libraries of one name, and its
  documentation is the header.
- **`ABI_VERSION` 1,** `trunkdb_version()` trunkdb's release: the
  crate's version follows trunkdb's, a test holds them equal.

## 179.2 The header
`include/trunkdb.h`, made by cbindgen from the crate (`cbindgen.toml`)
and checked in: the rules every function keeps, in its opening comment,
then the codes, the three opaque types and the functions, each with the
comment its Rust has. `header_is_current` makes it again and fails when
it differs; `TRUNKDB_WRITE_HEADER=1` writes it. cbindgen is a
dev-dependency, in no build.

## 179.3 Built and tested in CI
A job on Linux, macOS and Windows runs the crate's tests, the header's
among them; on Linux and macOS, `tests/c/run.sh` builds
`tests/c/every_function.c` against the header with `-Wall -Wextra
-Werror` and AddressSanitizer, links it to the library and runs it.
The minimum Rust version job builds the library. The release library
is 4.5 MB on macOS arm64, with the `regex` feature.

## 179.4 Moved into the library
- **`Display` for `Explained` and `QueryPlan`:** the lines `trunkdb find
  --explain` prints, which `trunkdb_explain` hands out. The command
  printed them from its own function.
- **`Database::export_to_file(path)`:** an export written beside the
  file and renamed over it once complete, refusing the database's own
  file and its WAL (`InvalidInput`), as `trunkdb export` did itself.

## 179.5 Rejected
- **The reports in the library now:** a shape fixed for one user;
  179.1.
- **A `trunkdb_collections` with a snapshot:** not asked for; a
  snapshot's reads name their collection.
- **Leaving the refusals as `TRUNKDB_IO`:** a C# wrapper would raise an
  I/O exception for an index asked for wrong.

## 179.6 Tests
- `trunkdb-ffi/src/tests.rs`, each function called through raw pointers:
  - documents in and out as JSON, a date kept a date; `get` and
    `find_one` null for none; `find` with parameters; `count` of a
    collection not there; `update` with a null filter; `upsert`
    inserting then updating; `replace` whole, `false` for none;
    `delete` and `delete_many`; `collections`; `drop_collection` twice;
  - indexes by a bare name, a string and an array, unique and sparse;
    another kind refused; listed; a duplicate's details: collection,
    field, existing id; dropped twice; four bad field lists;
  - an aggregate whose filter and text share the values; a value
    neither takes; one that doesn't read; `explain`'s lines;
  - a batch dry-run without ids, committed with them, lines counted with
    a blank one, an upsert's id the document's; a precondition failing
    at line 2, nothing written; a filter's place at its line; a
    precondition with a limit; an unused value;
  - a snapshot read by `count`, `find` and a cursor as it was; a cursor
    to its end and past; a compaction refused while a snapshot holds
    the last commit, done after;
  - `check`, `stats`, `checkpoint`, an export and its import into
    another database; an export over the database itself refused; an
    import of a file not there;
  - every kind of text that doesn't read, `what` and `offset`; an out
    parameter reset by a failure; bad options;
  - a file that isn't trunkdb's, a read-only open of a missing file, a
    duplicate id, a write to a read-only database;
  - misuse: null db, collection, out parameter and document; text that
    isn't UTF-8; a cursor passed as a database and as a snapshot, and
    closed as a database; a snapshot closed twice; null closed and
    freed;
  - the last error kept through a success, another thread's empty;
  - a panic caught as `TRUNKDB_PANIC`;
  - one database handle used by eight threads at once, 200 inserts and
    counts; one cursor read by two threads, each document once;
  - every call with a filter refusing a value no parameter takes;
  - the header current; the version trunkdb's.
- `tests/c/every_function.c`: all 36 functions from C, each at least
  once, an error's code, message and details; under AddressSanitizer.
- `export.rs`: an export to a file replacing one there whole, no
  temporary file left; refused over the database, its WAL and a symlink
  to it, each left as it was; a directory not there an I/O error.
- `tests/cli.rs`, as it was: `find --explain` and `export` through the
  library's.

## 179.7 Mutated
Seventeen ways by hand: the tag not checked (a crash, caught); an out
parameter not reset; the closed tag not written; a panic not caught;
the refusals left as I/O; `find`, and `count`, taking unused values;
an aggregate's unused value not checked; a dry run handing out ids; a
failure's line not named; the snapshot ignored; a duplicate's field
not in its details; the export's guard gone; a cursor skipping a
document; an upsert's `inserted` inverted; `read_only` ignored; a
success keeping an error. Fifteen failed a test at once; `find` taking
unused values stood until every call with a filter was tested for it.
One stands: the closed tag not written. The allocator writes over a
freed block's first bytes, so a second close is refused without it;
another allocator might not, which is why the store is there. Nothing
in storage or reading changed, so nothing was fuzzed.

## 179.8 Left for later
- **The C# wrapper,** step 3 (§177.9).
- **Changes and live queries,** polled handles, when asked (§177.10).
- **A leak check:** AddressSanitizer finds leaks on Linux only; the C
  program frees every string it is handed, and CI's Linux run checks.
