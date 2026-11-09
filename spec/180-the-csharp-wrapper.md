# 180. The C# wrapper (`bindings/dotnet/`)

Step 3 of §177.10: trunkdb from C#, over the C ABI of §179. A library,
`TrunkDb.Net.dll`, of plain classes: a database opened with `using`,
documents as a program's own classes, errors as exceptions. Nothing in
Rust changed; no file format changed.

```csharp
using TrunkDb;

using var db = Database.Open("app.trunkdb");
var tasks = db.Collection<TaskItem>("tasks");
tasks.EnsureIndex("status");

var task = new TaskItem { Title = "Write it", Due = new DateOnly(2026, 11, 9) };
tasks.Insert(task);                    // task.Id is set
task.Status = "done";
tasks.Update(task);                    // the stored document replaced

foreach (var open in tasks.Find("status = :s order by due limit 10", new { s = "open" }))
    Console.WriteLine($"{open.Id} {open.Title}");
tasks.UpdateMany("due < :today", """{"$set": {"late": true}}""", new { today = DateOnly.FromDateTime(DateTime.Today) });

class TaskItem
{
    public DocId Id { get; set; }          // the document's _id
    public string Title { get; set; } = "";
    public string Status { get; set; } = "open";
    public DateOnly? Due { get; set; }     // {"$date": "2026-11-09"}
}
```

## 180.1 As §177.9 has it, and what the code settled
- **`Database`, `Snapshot`, `Cursor<T>`,** each `IDisposable`, each
  holding a `SafeHandle` whose release calls the `_close` function: a
  handle forgotten is closed by the finalizer, and one disposed is
  refused by .NET itself, `ObjectDisposedException`, before any C is
  called. The design's `TrunkDb.Open` is `Database.Open`: a class named
  as its namespace makes every use `TrunkDb.TrunkDb`, and `Database` is
  Rust's name.
- **Documents through a `Collection<T>`,** as Rust's
  `db.collection::<T>(name)`: `db.Collection<TaskItem>("tasks")`, a
  name and the database, nothing to close. A document doesn't know its
  collection, so a write by the object alone needs one. Its reads,
  `Get`, `Find`, `FindOne`, `Count`, `Aggregate`, `Explain` and
  `OpenCursor`, are a `CollectionView<T>`'s, which a snapshot hands
  out, `snapshot.Collection<T>(name)`, for reads as it was taken; the
  writes and indexes are the collection's. A snapshot's reads need its
  database open: each call passes both handles. The database keeps
  what is the whole file's: collections listed and dropped, snapshots,
  batches, checkpoint, compact, check, stats, export and import.
- **A write by the object:** `Insert(task)` sets `task.Id` when it was
  empty; `Update(task)` replaces the stored document, found by its
  `Id`, and `Delete(task)` deletes it. A type without an `Id`, or an
  object not inserted yet, is `InvalidOperationException` naming
  `Update(id, document)`. `InsertMany` inserts in one batch, all or
  none, and sets each id. Each op is written as one line whatever the
  program's options: with `WriteIndented`, a document written as it
  came split its op over lines, and the batch refused it (found in
  review). Changing some fields of many documents is
  `UpdateMany(filter, update)`, with MongoDB's operators: `Update`
  replaces whole, as Rust's `Collection::update` does. The first cut
  had every call on the database with the collection's name and
  `Update(name, filter, operators)`: not how C# programs work with
  objects.
- **A cursor is an `IEnumerable<T>` read once;** a `foreach` to its end
  disposes it, `Dispose` when left before. A second enumeration is
  `InvalidOperationException`, not a silent empty sequence.
- **Every string copied and freed in the call it came back from;** no
  pointer reaches a C# program. Text going in is marshalled as UTF-8 by
  `[LibraryImport]`, its marshalling generated at compile time.
- **Filters and aggregates as trunkdb's text,** their parameters any
  object, `new { s = "open" }`, written as documents are; updates as
  the JSON operators `trunkdb update` takes, a string, since C# can't
  name an anonymous member `$inc`. A batch is its JSON lines.
- **Parameters are written without the `_id` rule of 180.2:** `new {
  id }` is the value of `:id`. Written with the documents' options, it
  was `_id`, and `_id = :id` had no value: found by the test of an
  update by id.
- **Reports as records:** `CheckReport`, `CompactReport`,
  `TransferSummary` (an export's or import's), `IndexInfo`,
  `DatabaseStats`, `BatchResult` with its `BatchOp`s, `Upserted`.
- **Open checks the ABI:** a library speaking another than 1 is
  refused with `InvalidOperationException` naming both, before a call
  could misread an argument.
- **Its version is trunkdb's** (`Directory.Build.props`); a test holds
  it equal to the library's, so a release bumps both.
- **.NET 8,** the oldest supported release with `DateOnly`, `TimeOnly`
  and `[LibraryImport]`.

## 180.2 Documents through System.Text.Json
`TrunkJson.Options()` makes the options every document is written and
read with: the web's defaults, camelCase names read in any case, or a
program's own (`OpenOptions.Json`), with a converter for each tag:

| C# | Tagged JSON |
|---|---|
| `DocId` | `{"$id": "01a1bc72-…"}` |
| `DateTime`, `DateTimeOffset` | `{"$date": "2026-11-09T14:05:00Z"}` |
| `DateOnly` | `{"$date": "2026-11-09"}` |
| `TimeOnly` | `{"$time": "14:05:00"}` |
| `byte[]` | `{"$binary": "AQID"}` |
| `double`, `float` | `3.0`; `{"$float": "NaN"}` |

- **A property `Id` of type `DocId` is `_id`,** by the member's own
  name, whatever a naming policy calls it, unless it has a
  `[JsonPropertyName]`. Matched by the name the policy made, a policy
  renaming `Id` left it an ordinary field: `Insert` didn't set it and
  `Update(task)` found none (found in review). left out when empty, `default`, so trunkdb
  chooses one. A `DocId` sorts as its text, as trunkdb orders ids.
- **A `DateTime` is an instant:** local time converted to UTC,
  unspecified taken as UTC, read back as UTC. .NET keeps 100 ns, trunkdb
  nanoseconds: the rest is cut on the way in. A `$date` without a time
  read into a `DateTime` is refused, naming `DateOnly`.
- **A `double` is written with its fraction,** `3.0`, never `3`:
  System.Text.Json writes `3`, which trunkdb reads as an integer. NaN
  and the infinities, which JSON has no number for, are tagged.
- **Untyped,** a `JsonNode` or a `JsonElement` holds the document as
  trunkdb gives it, tags and all.

## 180.3 Exceptions
Every error is a `TrunkDbException` with its `Code`, an `ErrorCode`,
its message, and `Detail`, the JSON of `trunkdb_error_detail()`. The
kinds a program catches by name have their fields as properties:
`ParseException` (`What`, `Offset`, `Line`, `Parameter`),
`DuplicateIdException`, `DuplicateValueException` (`Field`,
`Existing`), `NotFoundException`, `PreconditionFailedException` (`Op`,
`Line`, `Matched`), `SnapshotTooOldException`. A null where none is
taken is .NET's `ArgumentNullException`, before any C.

## 180.4 Where the library comes from
The wrapper doesn't carry the C library: a program puts the one for its
system beside its executable (§177.12), `trunkdb.dll`,
`libtrunkdb.dylib` or `libtrunkdb.so`, and .NET finds it by the name
`trunkdb`. The wrapper's own file is `TrunkDb.Net.dll`, not
`TrunkDb.dll`: Windows doesn't tell that from `trunkdb.dll`, and with
both in one directory .NET loaded the wrapper where it looked for the C
library, every call "no entry point `trunkdb_abi_version`". CI's first
Windows run failed so; macOS and Linux name the library
`libtrunkdb.*`. A test refuses the name on every system. The tests take it from `target/debug/`, where `cargo build
-p trunkdb-ffi` puts it, or from `-p:TrunkDbNative=<directory>`; the
build fails naming the command when it isn't there.

## 180.5 Built and tested in CI
The C ABI's job (§179.3) builds the library, then builds the wrapper
and runs its tests, on Linux, macOS and Windows, warnings as errors.

## 180.6 Rejected
- **Updates as objects:** an anonymous type can't name `$set`, and a
  dictionary of dictionaries says less than the JSON.
- **A collection named by an attribute on the class,**
  `db.Collection<TaskItem>()`: one class may live in two collections,
  and a name in the call is what Rust and the command use.
- **`Find` as a lazy `IEnumerable`:** `trunkdb_find` returns the whole
  array at once anyway; `OpenCursor` is the lazy one.
- **A NuGet package now:** §177.12.

## 180.7 Tests
- The wrapper's file not the C library's, in any case.
- A program's own options, indented, with a naming policy renaming
  every property: `InsertMany` and its ids, `Id` still the document's
  id, inserted, updated and read back; a `[JsonPropertyName]` on `Id`
  kept, an ordinary field.
- `JsonTests`, without the library: each tag written and read; a
  `DateTime` local, unspecified and with a fraction to the tick; nine
  digits of fraction cut to seven; a date refused as a `DateTime` and
  an instant as a `DateOnly`; floats `3.0`, `-0.0`, `1E+300`, NaN and
  the infinities; bytes; an empty id left out, a `JsonPropertyName`
  kept, a record's `Id` read from `_id`; ids in their text's order.
- `DatabaseTests`, against the library:
  - the library's version the wrapper's, its ABI 1;
  - an object inserted, its id set, changed, updated and deleted by
    itself; an id it has kept; by id for a type without one; a type
    without an id and an object not inserted refused by name;
  - many inserted at once, their ids set; all refused for one
    duplicate id, none set; find with parameters, find one, count, a
    date as a parameter;
  - every tagged kind through the database and back, a float stored as
    a float and a date as a date, compared in filters;
  - a `JsonObject` in and out with its tags;
  - many updated with a filter and without, one by `_id` with the id
    as a parameter; upsert inserting then updating; delete many;
  - an aggregate's rows as records; explain naming its index;
  - indexes on one field and on two, unique and sparse, listed; a
    duplicate value's field and existing id; another kind refused;
    dropped twice;
  - a batch dry-run without ids, committed with them, a blank line
    counted; a precondition failing at line 2, nothing written; a line
    that doesn't read;
  - a snapshot's get, count, find, find one, aggregate, explain and
    cursor as they were; a
    cursor read twice refused; a compaction refused while a snapshot
    holds the last commit, done after;
  - check, stats, checkpoint, collections, export, import into another
    database, drop collection twice; an import's line too long;
  - errors: a filter's place, an unused and a missing parameter, an
    update that doesn't read, a duplicate id, an upsert matching two,
    a null collection; a read-only database's write refused and its
    reads done; a missing file opened read-only; a file that isn't
    trunkdb's;
  - a disposed database and snapshot refused;
  - one database used by eight threads at once, each thread's error its
    own.

## 180.8 Mutated
Twenty-one ways by hand: a batch's op written indented; `Id` matched
by the name the policy made; an empty id written; parameters with the `_id`
rule; `Id` not mapped to `_id`; an inserted object's id not set, or
each of many's; `Update(task)` and `Delete(task)` by an empty id; an
object not inserted taken as one; a snapshot's collection reading the
database now; a float without `.0`; the fraction not cut; local time
not converted; `ReadOnly` not passed; a dry run committed; an upsert's
`Inserted` inverted; `unique` dropped; a duplicate value as the plain
exception; a cursor read twice allowed. Each failed a test but one: an
id set again when the object had one, which trunkdb returns as it was,
so the same. Untested: the ABI check, which needs a library of another
ABI, and the free of each string, which no test can see.

## 180.9 Left for later
- **The files on a GitHub release:** the library for each system and
  the header, attached by CI (§177.12), with the wrapper.
- **A NuGet package,** when the ABI holds still.
- **Changes and live queries,** when the C ABI has them (§177.10).
