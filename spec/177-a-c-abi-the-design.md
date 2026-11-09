# 177. A C ABI: the design (`trunkdb-ffi/`)

Written before the code, on 2026-11-08, to settle the shape first; the
sections that build it say what changed from here. trunkdb is a Rust
library, so only Rust programs can use it. Almost every language can
call C functions: C# through P/Invoke, Swift, Go, Java, Node, Dart,
Python through ctypes. A layer of plain C functions over trunkdb opens
it to all of them, with a thin wrapper per language on top, C# first
(ROADMAP.md, Planned).

```c
trunkdb_db *db;
if (trunkdb_open("app.trunkdb", NULL, &db) != TRUNKDB_OK) {
    fprintf(stderr, "%s\n", trunkdb_error_message());
    return 1;
}
char *id;
trunkdb_insert(db, "tasks", "{\"title\": \"Write it\", \"status\": \"open\"}", &id);
char *found;
trunkdb_find(db, NULL, "tasks", "status = :s order by _id limit 10",
             "{\"s\": \"open\"}", &found);
puts(found);                      /* [{"_id": {"$id": "…"}, "title": …}] */
trunkdb_string_free(found);
trunkdb_string_free(id);
trunkdb_close(db);
```

```csharp
using var db = TrunkDb.Open("app.trunkdb");
db.Insert("tasks", new { title = "Write it", status = "open" });
foreach (var task in db.Find<TaskItem>("tasks", "status = :s limit 10", new { s = "open" }))
    Console.WriteLine(task.Title);
```

## 177.1 The shape: text in, text out
- **Documents as JSON,** the tagged JSON an export writes (§30):
  `{"$id": …}`, `{"$date": …}`, `{"$time": …}`, `{"$binary": …}`,
  `{"$float": "NaN"}`. Every language reads and writes JSON already.
- **Filters and aggregates as text** (§111, §157), with parameters
  (§163) passed apart as one JSON object, `{"s": "open"}`, so a value
  never changes what the text says.
- **Updates as JSON operators,** `{"$inc": {"hits": 1}}`, as `trunkdb
  update` takes them (§156.2).
- **Batches as JSON lines,** one op a line, as `trunkdb batch` reads
  them (§168.3, §175.3).
- **Ids as their text,** `01a1bc72-c792-772e-a864-a359a7ffaeb6`.

So about 35 functions carry the whole query language, and a new
operator or aggregate reaches every language without a new function.
What has no text stays out: a custom condition, a sort by a key and
`update_many`'s change are closures (§136, §154, §38).

## 177.2 Handles
- **Real pointers to opaque types:** `trunkdb_db`, `trunkdb_snapshot`
  and `trunkdb_cursor` are declared in the header and never defined,
  so C can hold one but not look inside; each is a distinct type, so
  passing a cursor where a database goes fails to compile without a
  cast.
- **Checked on every call:** each handle starts with a tag of its own,
  a fixed 32-bit number. A null handle, or one whose tag isn't its
  type's, returns `TRUNKDB_INVALID_ARGUMENT` instead of reading what
  isn't there. Closing overwrites the tag before freeing, so a second
  close or a use after close is usually caught too. "Usually": freed
  memory may be reused; a forged pointer isn't caught at all. That is
  C's contract, as SQLite's and libgit2's.
- **Closing accepts null,** as C's `free` does.
- **No handle borrows from another:** `Database` is a clone of a shared
  handle (§27), and a cursor and a snapshot own what they hold. A
  cursor still open when its database is closed keeps reading; the
  file closes when the last of them closes.
- **No collection handle:** a collection is named in each call, as the
  command names one. Making a `Collection<Document>` is a clone of the
  database handle and a string.

## 177.3 Strings and memory
- **In:** `const char *`, UTF-8 ending in a zero byte, borrowed for the
  call only. Text that isn't UTF-8 is `TRUNKDB_INVALID_ARGUMENT`. JSON
  escapes a zero byte inside a string as `\u0000`, so documents fit.
- **Out:** `char **out`, a string trunkdb allocated, the caller's to
  give back with `trunkdb_string_free`, never C's `free`: Rust's
  allocator need not be C's. Freeing null does nothing.
- **Out parameters are written only on success;** on an error each is
  set to null or 0, so freeing one unconditionally is safe.
- **Numbers and flags** as `uint64_t` and `bool` (C99's, which Rust's
  `bool` matches).

## 177.4 Errors
- **Every function but the frees returns an `int32_t` code:**
  `TRUNKDB_OK` is 0, and each kind of error has its own number, never
  reused: invalid argument, parse (a filter, document, update,
  parameters, aggregate or batch line that doesn't read), I/O, format
  (§21), read-only, poisoned, duplicate id, duplicate value, not found,
  multiple matches, update, precondition failed, not an object, hint,
  import, snapshot open, snapshot too old, panic, and other, for a kind
  added later (`Error` is `#[non_exhaustive]`).
- **The message:** `trunkdb_error_message()`, the text of the last error
  on this thread, as `Display` prints it; owned by trunkdb, valid
  until the next failing call on the thread. Thread-local, as `errno`
  and `GetLastError`, so two threads never see each other's.
- **The details:** `trunkdb_error_detail()`, the same error as JSON:
  `{"code": 8, "kind": "duplicate_value", "message": …, "collection":
  "users", "field": "email", "id": …, "existing": …}`, every field the
  `Error` variant has, and for a parse error its offset in the text.
  A wrapper makes typed exceptions from it.

## 177.5 The functions
Reads take an optional snapshot: null reads the database as it is now,
a snapshot reads as it was taken (§82). One function a read, instead of
each twice.

| Group | Functions |
|---|---|
| General | `trunkdb_abi_version`, `trunkdb_version`, `trunkdb_error_message`, `trunkdb_error_detail`, `trunkdb_string_free` |
| Database | `trunkdb_open(path, options_json, out)`, `trunkdb_close`, `trunkdb_checkpoint`, `trunkdb_compact`, `trunkdb_check`, `trunkdb_stats`, `trunkdb_collections`, `trunkdb_drop_collection`, `trunkdb_export(path)`, `trunkdb_import(path, options_json)` |
| Indexes | `trunkdb_index_ensure(fields_json, options_json)`, `trunkdb_index_drop`, `trunkdb_indexes` |
| Reads | `trunkdb_get(id)`, `trunkdb_find`, `trunkdb_find_one`, `trunkdb_count`, `trunkdb_aggregate`, `trunkdb_explain`; each `(db, snapshot, collection, filter, params, out)` |
| Writes | `trunkdb_insert(doc)`, `trunkdb_replace(id, doc)`, `trunkdb_delete(id)`, `trunkdb_update(filter, params, update)`, `trunkdb_delete_many(filter, params)`, `trunkdb_upsert(filter, params, update, out id, out inserted)` |
| Batch | `trunkdb_batch(db, lines, params, dry_run, out written)` |
| Snapshot | `trunkdb_snapshot_open`, `trunkdb_snapshot_close` |
| Cursor | `trunkdb_cursor_open(db, snapshot, collection, filter, params, out)`, `trunkdb_cursor_next(cursor, out doc)`, null at the end, `trunkdb_cursor_close` |

36 in all. The names follow the command's (§156): `trunkdb_update`
writes operators by a filter, as `trunkdb update` does, which is
`update_fields` in Rust; replacing a document by id is
`trunkdb_replace`. Results that are more than a document or a count,
`Written`, `Stats`, a check's report, an export's `Summary`, come as
JSON, the shape `--json` prints where the command has one (§155).

## 177.6 Threads and panics
- **A database handle from any number of threads at once,** as
  `Database` is `Send` and `Sync` (§27). A snapshot too.
- **A cursor from one thread at a time,** by contract; its handle holds
  it behind a lock anyway, so two threads calling `next` at once each
  get a document instead of undefined behavior.
- **No panic crosses the boundary:** every function runs inside
  `catch_unwind`, and a panic returns `TRUNKDB_PANIC` with its message.
  Unwinding into C is undefined behavior; the library is built with
  `panic = "unwind"` so there is something to catch. A panic inside a
  write may leave the database poisoned (§27), as in Rust.

## 177.7 The embedding API inside trunkdb
What the command reads and writes as text lives in `bin/trunkdb.rs`
today: updates as JSON operators (`parse_update`, `update_from`),
batch lines (`batch_of`), parameters (`param_value`), and the `--json`
reports. The C ABI needs each of them, and so will the Python bindings,
so they move into the library first, public, and the command uses them
from there: one code path, the command's tests covering it.
- `Update` from a document of operators, `TryFrom<Document>`.
- `Params` from a JSON object, `TryFrom<Document>`.
- A batch from JSON lines: `Database::batch_from_lines(text, &params)`,
  each line's mistake naming its line, as now.
- Reports as documents: `Written`, `Upserted`, `Stats`, `CheckReport`,
  `FileInfo`, `Summary`, `Compacted`, `IndexInfo`, each `to_document`,
  so `Display` gives its JSON.

No behaviour changes; the step is its own section, with its own tests,
before any C.

## 177.8 The crate, the header and the build
- **`trunkdb-ffi/`,** a member of the workspace beside `trunkdb-derive`,
  `publish = false`, built as `cdylib` and `staticlib`: `trunkdb.dll`,
  `libtrunkdb.dylib`, `libtrunkdb.so`, and a `.a`/`.lib` to link in.
  Nothing of it in the `trunkdb` crate a Rust program depends on.
- **`trunkdb-ffi/include/trunkdb.h`,** made by cbindgen from the Rust
  and checked in, so a C program needs no Rust to read it. A test
  regenerates it and fails when it differs, as CI's fmt check does;
  cbindgen stays a dev-dependency, out of every build.
- **The ABI's version:** `TRUNKDB_ABI_VERSION` in the header and
  `trunkdb_abi_version()` in the library, so a wrapper loading a
  library of another release can refuse it. A function, once out, keeps
  its signature; a change is a new function, as SQLite's
  `sqlite3_prepare_v2`.
- **CI:** the crate built, linted and tested on the three systems; a C
  program using every function compiled against the header and run,
  under AddressSanitizer on Linux.

## 177.9 The C# wrapper
In this repository, `bindings/dotnet/`, at first: while the ABI still
changes, a function changes in the Rust, the header and the wrapper in
one commit, and one CI run tests them together. A repository of its own
once the ABI holds still and the wrapper has users and releases of its
own, as Wasmtime's wrappers moved out.

- **Thin:** `TrunkDb : IDisposable`, its handle a `SafeHandle` whose
  release calls `trunkdb_close`, so a database is closed by `using` or,
  at the latest, by the finalizer; `Snapshot` and `Cursor` alike, a
  cursor an `IEnumerable<T>`.
- **Strings copied at once:** each `char*` read into a .NET string and
  freed in the same call, so no C# user holds a pointer.
- **Documents through `System.Text.Json`,** with converters for the
  tags: `DateTime`/`DateTimeOffset` as `$date` with a time, `DateOnly`
  as `$date` without, `TimeOnly` as `$time`, `byte[]` as `$binary`, a
  `DocId` struct as `$id`. Typed `Find<T>` and `Insert<T>`, and
  `JsonDocument` for the untyped.
- **Exceptions from the details:** a `TrunkDbException` with the code,
  and a subclass for the ones a program catches by kind:
  `DuplicateValueException` with `Field`, `PreconditionFailedException`
  with `Op`, `SnapshotTooOldException`.
- **The P/Invoke declarations** may be generated from the header; the
  wrapper over them is written by hand.

## 177.10 Steps
1. **The embedding API** of 177.7, in the library, the command moved
   onto it.
2. **`trunkdb-ffi`:** the 36 functions, the header, the C test program.
3. **The C# wrapper** and its tests, against the library built by CI.
4. **Later, each when asked:** changes and live queries (§135, §138) as
   handles polled with a timeout, `trunkdb_changes_next(h, ms, out)`;
   the Python bindings (ROADMAP.md, Planned) on the same embedding API.

## 177.11 Rejected
- **Numbers in a table instead of pointers,** as file descriptors or
  Windows `HANDLE`s: a wrong or stale number always an error, never a
  crash, but a lock and a lookup on every call, for a protection the C#
  wrapper already gives its users. A tag catches the common mistakes.
- **Callbacks** for live queries and changes: a Rust thread calling into
  C#'s runtime, or Python's under its lock, is where bindings break.
  A handle the program polls from its own thread first.
- **A document as a tree of C structs,** with a function per kind of
  value: dozens of functions, and every wrapper turns it into its own
  objects anyway, as it does JSON.
- **A binary encoding,** CBOR or MessagePack, instead of JSON: every
  language has a JSON reader, the tags are defined already (§30), and
  no measurement shows JSON's cost yet.
- **Buffers the caller passes in,** as many Windows functions take them:
  no allocation crosses the boundary, but every call twice, once for
  the size, for results whose size is not known in advance.
- **The C# wrapper in a repository of its own** from the start, as most
  .NET wrappers live: every change to a function two pull requests,
  kept in step by hand.
- **A NuGet package from the start:** strangers depending on an ABI
  that still changes, and a package's metadata kept for no one yet.
- **A collection handle:** one more type to close for nothing a name
  doesn't do (177.2).
- **Closures through function pointers:** a C function pointer for a
  custom condition can be made, but every document would cross the
  boundary as JSON to be judged; a filter's text is the way.

## 177.12 How the library reaches a program
- **Files on each GitHub release** first: the library for Windows x64,
  Linux x64 and macOS arm64 that CI builds anyway, with the header,
  attached by one more job. A C# program copies the one for its system
  beside its executable, or has its project file do it.
- **A NuGet package later,** when the ABI holds still and programs
  other than the author's use it: the same files under
  `runtimes/<rid>/native/`, with the wrapper. Nothing of the first
  step changes for it. Apart from whether trunkdb goes on crates.io
  (ROADMAP.md, Open): the C library and the Rust crate reach programs
  by different roads.

## 177.13 Tests, planned
- Rust tests calling each function as C would, through raw pointers:
  every function, every error code, the details' JSON for each.
- Misuse: null for each argument, text that isn't UTF-8, a cursor
  passed as a database, a second close, a use after close, freeing
  null; each an error code, never a crash, where 177.2 says it can be
  caught.
- A panic forced inside a call: `TRUNKDB_PANIC`, the process alive.
- Threads: one database handle used by eight at once; two calling a
  cursor's `next` together.
- The C program, under AddressSanitizer: nothing leaked, nothing read
  after free.
- The header regenerated and compared.
- The usual hand-made mutations on the boundary: a tag not checked, a
  string not freed, an out parameter left set on an error.
