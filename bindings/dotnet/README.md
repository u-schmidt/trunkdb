# trunkdb for C#

A thin wrapper over trunkdb's C ABI ([trunkdb.h](../../trunkdb-ffi/include/trunkdb.h)),
for .NET 8 and later. The design is in [SPEC §177.9](../../spec/177-a-c-abi-the-design.md),
what was built in [§180](../../spec/180-the-csharp-wrapper.md).

```csharp
using TrunkDb;

using var db = Database.Open("app.trunkdb");
db.EnsureIndex("tasks", "status");
var id = db.Insert("tasks", new TaskItem { Title = "Write it", Due = new DateOnly(2026, 11, 9) });

foreach (var task in db.Find<TaskItem>("tasks", "status = :s order by due limit 10", new { s = "open" }))
    Console.WriteLine($"{task.Id} {task.Title}");

db.Update("tasks", "_id = :id", """{"$set": {"status": "done"}}""", new { id });

using (var snapshot = db.OpenSnapshot())
using (var cursor = snapshot.OpenCursor<TaskItem>("tasks"))
    foreach (var task in cursor)
        Console.WriteLine(task.Title);

class TaskItem
{
    public DocId Id { get; set; }          // the document's _id; empty: trunkdb chooses one
    public string Title { get; set; } = "";
    public string Status { get; set; } = "open";
    public DateOnly? Due { get; set; }     // {"$date": "2026-11-09"}
}
```

- **Filters and aggregates** are trunkdb's text, as the `trunkdb`
  command takes it; parameters an object, `new { s = "open" }`.
- **Updates** are MongoDB's operators as JSON, `{"$inc": {"hits": 1}}`.
- **Documents** go through System.Text.Json, camelCase by default; pass
  your own options as `OpenOptions.Json`. `DocId`, `DateTime`,
  `DateOnly`, `TimeOnly` and `byte[]` are written as trunkdb's tags.
- **Errors** are a `TrunkDbException` with its `Code` and `Detail`; catch
  `DuplicateValueException`, `ParseException` and the others by kind.
- **Threads:** one `Database` for the whole program, from any thread.

## The library

The wrapper calls `trunkdb.dll`, `libtrunkdb.dylib` or `libtrunkdb.so`,
which it doesn't carry: put the one for your system beside your
executable. Build it from the repository's root:

```bash
cargo build --release -p trunkdb-ffi
```

## Tests

```bash
cargo build -p trunkdb-ffi
```

```bash
dotnet test bindings/dotnet/tests/TrunkDb.Tests
```

They take the library from `target/debug/`; `-p:TrunkDbNative=<directory>`
names another.
