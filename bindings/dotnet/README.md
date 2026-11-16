# trunkdb for C#

A thin wrapper over trunkdb's C ABI ([trunkdb.h](../../trunkdb-ffi/include/trunkdb.h)),
for .NET 8 and later. The design is in [SPEC §177.9](../../spec/177-a-c-abi-the-design.md),
what was built in [§180](../../spec/180-the-csharp-wrapper.md).

```csharp
using TrunkDb;

using var db = Database.Open("app.trunkdb");
var tasks = db.Collection<TaskItem>("tasks");
tasks.EnsureIndex("status");

var task = new TaskItem { Title = "Write it", Due = new DateOnly(2026, 11, 9) };
tasks.Insert(task);                   // task.Id is set
task.Status = "done";
tasks.Update(task);                   // replaced, found by task.Id

foreach (var open in tasks.Find("status = :s order by due limit 10", new { s = "open" }))
    Console.WriteLine($"{open.Id} {open.Title}");

using (var snapshot = db.OpenSnapshot())
using (var cursor = snapshot.Collection<TaskItem>("tasks").OpenCursor())
    foreach (var then in cursor)
        Console.WriteLine(then.Title);

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
- **A collection** is a name and the database: `db.Collection<T>(name)`,
  as cheap as that, nothing to close. `Update(task)` replaces the
  document whole, found by its `Id`.
- **Changing many at once,** `UpdateMany(filter, update)`, takes
  MongoDB's operators as JSON, `{"$inc": {"hits": 1}}`.
- **Documents** go through System.Text.Json, camelCase by default; pass
  your own options as `OpenOptions.Json`. `DocId`, `DateTime`,
  `DateOnly`, `TimeOnly` and `byte[]` are written as trunkdb's tags.
- **Errors** are a `TrunkDbException` with its `Code` and `Detail`; catch
  `DuplicateValueException`, `ParseException` and the others by kind.
- **Threads:** one `Database` for the whole program, from any thread.

## Changes as they happen

A filter kept live tells you what comes to match it, what leaves, and
what is written and still matches, as each commit commits ([SPEC
§194](../../spec/194-changes-as-they-happen-through-the-c-abi.md)):

```csharp
using var inDepot = vans.Watch("zone = :z", new { z = "depot" });
await foreach (var e in inDepot.ReadAllAsync(token))
{
    switch (e)
    {
        case LiveEvent<Van>.Entered entered: Console.WriteLine($"{entered.Document.Name} arrived"); break;
        case LiveEvent<Van>.Left left: Console.WriteLine($"{left.Id} left"); break;
        case LiveEvent<Van>.Changed changed: Console.WriteLine($"{changed.Document.Name} moved within"); break;
    }
}
```

- **First what matches now,** as `Entered`, then the changes. With a
  sort, `Watch("order by speed desc limit 10")` is a list: each event's
  `From` and `At` say where to take a document out and put it in.
- **`WatchAggregate<TRow>(filter, aggregate)`** keeps an aggregate live,
  its rows `Entered`, `Left` or `Changed`; **`db.Subscribe()`** hears of
  every commit, each document it inserted, updated or deleted.
- **Read** with `Next(timeout)`, `foreach` over `ReadAll()` on a thread
  of its own, or `await foreach` over `ReadAllAsync(token)`. Nothing is
  missed between two reads; events wait until read.
- **Dispose** the feed to stop it. Disposing the database ends it too,
  once the events before are read: `Ended` is then true.
- **Opened read-only beside the application** (`ReadOnly = true`, while
  another process writes the file), `Watch` and `WatchAggregate` report
  that process's commits too, read again every 100 ms: several commits
  between two looks come as one. `Subscribe` reports nothing there.
- **Against the clock,** `Watch("lastSeen < now - 10m")` is read again
  as time passes, with nothing written: a device that stops reporting
  is `Entered` ten minutes after its last reading, `Left` when it
  reports again. An index on `lastSeen` finds the moment; without one,
  it is read again every second.

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
