using System.Text.Json;
using System.Text.Json.Nodes;
using Xunit;

namespace TrunkDb.Tests;

public sealed class TaskItem
{
    public DocId Id { get; set; }
    public string Title { get; set; } = "";
    public string Status { get; set; } = "open";
    public DateOnly? Due { get; set; }
    public double Hours { get; set; }
}

public sealed record Kinds(
    DateTime At,
    DateTimeOffset Seen,
    DateOnly Day,
    TimeOnly Clock,
    byte[] Bytes,
    double Whole,
    double NotANumber,
    float Infinite,
    DocId Other,
    long Big);

public sealed record Group(string Status, ulong N, double Hours);

/// <summary>Every method of the wrapper, against the library cargo built (SPEC §180).</summary>
public sealed class DatabaseTests : IDisposable
{
    private readonly string directory = Directory.CreateTempSubdirectory("trunkdb-dotnet-").FullName;

    public void Dispose() => Directory.Delete(directory, recursive: true);

    private string File(string name) => Path.Combine(directory, name);

    private Database Open(string name = "app.trunkdb", OpenOptions? options = null) => Database.Open(File(name), options);

    [Fact]
    public void The_library_is_trunkdbs_release_and_speaks_its_abi()
    {
        Assert.Equal(1u, Database.AbiVersion);
        var version = typeof(Database).Assembly.GetName().Version!;
        Assert.Equal($"{version.Major}.{version.Minor}.{version.Build}", Database.LibraryVersion);
    }

    [Fact]
    public void Documents_go_in_as_objects_and_come_back_as_them()
    {
        using var db = Open();
        var id = db.Insert("tasks", new TaskItem { Title = "Write it", Due = new DateOnly(2026, 11, 9), Hours = 2 });
        db.Insert("tasks", new TaskItem { Title = "Test it", Status = "done" });

        var task = db.Get<TaskItem>("tasks", id)!;
        Assert.Equal(id, task.Id);
        Assert.Equal("Write it", task.Title);
        Assert.Equal(new DateOnly(2026, 11, 9), task.Due);
        Assert.Null(db.Get<TaskItem>("tasks", DocId.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6")));

        var open = db.Find<TaskItem>("tasks", "status = :s", new { s = "open" });
        Assert.Equal(["Write it"], open.Select(t => t.Title));
        Assert.Equal(2, db.Find<TaskItem>("tasks").Count);
        Assert.Equal("Test it", db.FindOne<TaskItem>("tasks", "status = \"done\"")!.Title);
        Assert.Null(db.FindOne<TaskItem>("tasks", "hours > 10"));
        Assert.Equal(1ul, db.Count("tasks", "due = :d", new { d = new DateOnly(2026, 11, 9) }));
        Assert.Equal(0ul, db.Count("nothing"));

        // The id a document has is kept.
        var mine = new DocId(Guid.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6"));
        Assert.Equal(mine, db.Insert("tasks", new TaskItem { Id = mine, Title = "Mine" }));
        Assert.Equal("Mine", db.Get<TaskItem>("tasks", mine)!.Title);
    }

    [Fact]
    public void Every_tagged_kind_goes_through_the_database_and_back()
    {
        using var db = Open();
        var at = new DateTime(2026, 11, 9, 14, 5, 0, DateTimeKind.Utc).AddTicks(1234567);
        var kinds = new Kinds(
            at,
            new DateTimeOffset(at),
            new DateOnly(2026, 11, 9),
            new TimeOnly(14, 5, 0, 250),
            [0, 1, 255],
            3.0,
            double.NaN,
            float.PositiveInfinity,
            DocId.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6"),
            long.MaxValue);
        var id = db.Insert("kinds", kinds);

        var back = db.Get<Kinds>("kinds", id)!;
        Assert.Equal(kinds.At, back.At);
        Assert.Equal(kinds.Seen, back.Seen);
        Assert.Equal(kinds.Day, back.Day);
        Assert.Equal(kinds.Clock, back.Clock);
        Assert.Equal(kinds.Bytes, back.Bytes);
        Assert.Equal(3.0, back.Whole);
        Assert.True(double.IsNaN(back.NotANumber));
        Assert.True(float.IsPositiveInfinity(back.Infinite));
        Assert.Equal(kinds.Other, back.Other);
        Assert.Equal(long.MaxValue, back.Big);

        // As trunkdb holds it: a float stays a float, a date a date.
        var raw = db.Get<JsonElement>("kinds", id);
        Assert.Equal("3.0", raw.GetProperty("whole").GetRawText());
        Assert.Equal("""{"$date":"2026-11-09"}""", raw.GetProperty("day").GetRawText());
        Assert.Equal(1ul, db.Count("kinds", "day = :d and clock > :t", new { d = kinds.Day, t = new TimeOnly(14, 0) }));
        Assert.Equal(1ul, db.Count("kinds", "at < :now", new { now = DateTime.UtcNow.AddYears(1) }));
    }

    [Fact]
    public void Untyped_documents_are_json_nodes_tags_and_all()
    {
        using var db = Open();
        var id = db.Insert("notes", JsonNode.Parse("""{"text": "hi", "when": {"$date": "2026-11-09"}}""")!);
        var note = db.Get<JsonObject>("notes", id)!;
        Assert.Equal(id.ToString(), note["_id"]!["$id"]!.GetValue<string>());
        Assert.Equal("2026-11-09", note["when"]!["$date"]!.GetValue<string>());
    }

    [Fact]
    public void Writes_replace_delete_update_and_upsert()
    {
        using var db = Open();
        var id = db.Insert("tasks", new TaskItem { Title = "a", Hours = 1 });
        db.Insert("tasks", new TaskItem { Title = "b", Hours = 1 });

        Assert.True(db.Replace("tasks", id, new TaskItem { Title = "a2", Hours = 5 }));
        Assert.Equal("a2", db.Get<TaskItem>("tasks", id)!.Title);
        Assert.Equal(id, db.Get<TaskItem>("tasks", id)!.Id);
        Assert.False(db.Replace("tasks", DocId.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6"), new TaskItem()));

        Assert.Equal(2ul, db.Update("tasks", null, """{"$inc": {"hours": 1}}"""));
        Assert.Equal(6.0, db.Get<TaskItem>("tasks", id)!.Hours);
        Assert.Equal(1ul, db.Update("tasks", "title = :t", """{"$set": {"status": "done"}}""", new { t = "b" }));
        Assert.Equal(1ul, db.Update("tasks", "_id = :id", """{"$set": {"title": "a3"}}""", new { id }));
        Assert.Equal("a3", db.Get<TaskItem>("tasks", id)!.Title);

        var first = db.Upsert("visits", "page = :p", """{"$inc": {"hits": 1}}""", new { p = "/" });
        Assert.True(first.Inserted);
        var second = db.Upsert("visits", "page = :p", """{"$inc": {"hits": 1}}""", new { p = "/" });
        Assert.False(second.Inserted);
        Assert.Equal(first.Id, second.Id);
        Assert.Equal(2, db.Get<JsonElement>("visits", first.Id).GetProperty("hits").GetInt32());

        Assert.True(db.Delete("tasks", id));
        Assert.False(db.Delete("tasks", id));
        Assert.Equal(1ul, db.DeleteMany("tasks", null));
        Assert.Equal(0ul, db.Count("tasks"));
    }

    [Fact]
    public void Aggregates_come_back_as_rows_and_explain_as_lines()
    {
        using var db = Open();
        db.Insert("tasks", new TaskItem { Title = "a", Hours = 1 });
        db.Insert("tasks", new TaskItem { Title = "b", Hours = 2 });
        db.Insert("tasks", new TaskItem { Title = "c", Status = "done", Hours = 4 });

        var rows = db.Aggregate<Group>("tasks", "hours >= :min", "count() as n, sum(hours) as hours group by status", new { min = 1 });
        Assert.Equal([new Group("done", 1, 4), new Group("open", 2, 3)], rows.OrderBy(r => r.Status));

        db.EnsureIndex("tasks", "status");
        Assert.Contains("plan: index on", db.Explain("tasks", "status = :s", new { s = "open" }));
    }

    [Fact]
    public void Indexes_are_made_listed_enforced_and_dropped()
    {
        using var db = Open();
        Assert.True(db.EnsureIndex("users", "email", unique: true));
        Assert.False(db.EnsureIndex("users", "email", unique: true));
        Assert.True(db.EnsureIndex("users", ["team", "name"], sparse: true));
        Assert.Equal(
            [new IndexInfo(["email"], true, false), new IndexInfo(["team", "name"], false, true)],
            db.Indexes("users").Select(i => i with { Fields = [.. i.Fields] }).OrderBy(i => i.Fields[0]).ToList(),
            new IndexComparer());

        var first = db.Insert("users", new { email = "a@example.com" });
        var refused = Assert.Throws<DuplicateValueException>(() => db.Insert("users", new { email = "a@example.com" }));
        Assert.Equal(ErrorCode.DuplicateValue, refused.Code);
        Assert.Equal("users", refused.Collection);
        Assert.Equal("email", refused.Field);
        Assert.Equal(first, refused.Existing);

        var other = Assert.Throws<TrunkDbException>(() => db.EnsureIndex("users", "email"));
        Assert.Equal(ErrorCode.InvalidArgument, other.Code);

        Assert.True(db.DropIndex("users", ["team", "name"]));
        Assert.False(db.DropIndex("users", ["team", "name"]));
        Assert.Single(db.Indexes("users"));
    }

    private sealed class IndexComparer : IEqualityComparer<IndexInfo>
    {
        public bool Equals(IndexInfo? x, IndexInfo? y) =>
            x!.Fields.SequenceEqual(y!.Fields) && x.Unique == y.Unique && x.Sparse == y.Sparse;

        public int GetHashCode(IndexInfo obj) => obj.Fields.Count;
    }

    [Fact]
    public void A_batch_writes_all_or_nothing_and_names_the_line_that_failed()
    {
        using var db = Open();
        const string lines = """
            {"op": "insert", "collection": "log", "document": {"n": 1}}

            {"op": "update", "collection": "log", "filter": "n = :n", "update": {"$set": {"seen": true}}}
            """;
        var dry = db.Batch(lines, new { n = 1 }, dryRun: true);
        Assert.True(dry.DryRun);
        Assert.Equal(0ul, db.Count("log"));
        Assert.All(dry.Ops, op => Assert.Null(op.Id));

        var done = db.Batch(lines, new { n = 1 });
        Assert.Equal(2ul, done.Total);
        Assert.Equal([1ul, 3ul], done.Ops.Select(op => op.Line));
        Assert.Equal(["insert", "update"], done.Ops.Select(op => op.Op));
        Assert.Equal(done.Ops[0].Id, db.FindOne<JsonObject>("log") is { } log ? DocId.Parse(log["_id"]!["$id"]!.GetValue<string>()) : null);

        var failed = Assert.Throws<PreconditionFailedException>(() => db.Batch("""
            {"op": "insert", "collection": "log", "document": {"n": 2}}
            {"op": "require_none", "collection": "log", "filter": "n = 1"}
            """));
        Assert.Equal(2, failed.Line);
        Assert.Equal(1, failed.Op);
        Assert.True(failed.Matched);
        Assert.Equal("log", failed.Collection);
        Assert.Equal(1ul, db.Count("log"));

        var unread = Assert.Throws<ParseException>(() => db.Batch("""
            {"op": "insert", "collection": "log", "document": {"n": 3}}
            {"op": "delete", "collection": "log", "filter": "n = = 3"}
            """));
        Assert.Equal("batch", unread.What);
        Assert.Equal(2, unread.Line);
    }

    [Fact]
    public void A_snapshot_and_its_cursor_read_the_database_as_it_was()
    {
        using var db = Open();
        for (var i = 0; i < 300; i++)
        {
            db.Insert("tasks", new TaskItem { Title = $"t{i}", Hours = i });
        }
        db.DeleteMany("tasks", "hours >= 10");

        using (var snapshot = db.OpenSnapshot())
        {
            db.DeleteMany("tasks", "hours >= 5");
            Assert.Equal(5ul, db.Count("tasks"));
            Assert.Equal(10ul, snapshot.Count("tasks"));
            Assert.Equal(10, snapshot.Find<TaskItem>("tasks").Count);
            Assert.Equal(9.0, snapshot.FindOne<TaskItem>("tasks", "hours = 9")!.Hours);

            using var cursor = snapshot.OpenCursor<TaskItem>("tasks", "hours >= :h order by hours", new { h = 3 });
            Assert.Equal([3.0, 4, 5, 6, 7, 8, 9], cursor.Select(t => t.Hours));
            Assert.Throws<InvalidOperationException>(() => cursor.First());

            var refused = Assert.Throws<TrunkDbException>(() => db.Compact());
            Assert.Equal(ErrorCode.SnapshotOpen, refused.Code);
        }
        var compacted = db.Compact();
        Assert.True(compacted.PagesAfter < compacted.PagesBefore);

        // Left before its end, a cursor is closed by Dispose.
        using var live = db.OpenCursor<TaskItem>("tasks");
        Assert.Equal("t0", live.First().Title);
    }

    [Fact]
    public void The_database_is_checked_measured_exported_and_imported()
    {
        using var db = Open();
        db.EnsureIndex("tasks", "status");
        db.Insert("tasks", new TaskItem { Title = "a" });
        db.Insert("notes", new { text = "n" });
        db.Checkpoint();

        var check = db.Check();
        Assert.True(check.Ok);
        Assert.Empty(check.Problems);
        Assert.Equal(2ul, check.Documents);

        var stats = db.Stats();
        Assert.True(stats.File.Pages > 0);
        Assert.Equal(Database.LibraryVersion, stats.File.WrittenBy);
        var tasks = Assert.Single(stats.Collections, c => c.Name == "tasks");
        Assert.Equal(1ul, tasks.Documents);
        Assert.Equal(["status"], Assert.Single(tasks.Indexes).Fields);

        Assert.Equal(["notes", "tasks"], db.Collections().Order());
        var exported = db.Export(File("backup.jsonl"));
        Assert.Equal(new TransferSummary(2, 2, true), exported);

        using var copy = Open("copy.trunkdb");
        Assert.Equal(new TransferSummary(2, 2, true), copy.Import(File("backup.jsonl")));
        Assert.Equal("a", copy.FindOne<TaskItem>("tasks")!.Title);
        Assert.True(copy.DropCollection("notes"));
        Assert.False(copy.DropCollection("notes"));
        Assert.Equal(["tasks"], copy.Collections());

        using var third = Open("third.trunkdb");
        var tooLong = Assert.Throws<TrunkDbException>(() => third.Import(File("backup.jsonl"), maxLineBytes: 10));
        Assert.Equal(ErrorCode.Import, tooLong.Code);
    }

    [Fact]
    public void Errors_are_exceptions_with_their_details()
    {
        using (var db = Open())
        {
            var filter = Assert.Throws<ParseException>(() => db.Count("tasks", "hours = = 1"));
            Assert.Equal(ErrorCode.Parse, filter.Code);
            Assert.Equal("filter", filter.What);
            Assert.NotNull(filter.Offset);
            Assert.Equal("parse", filter.Detail.GetProperty("kind").GetString());

            var unused = Assert.Throws<ParseException>(() => db.Find<TaskItem>("tasks", "status = :s", new { s = "open", t = 1 }));
            Assert.Equal("t", unused.Parameter);
            var missing = Assert.Throws<ParseException>(() => db.Find<TaskItem>("tasks", "status = :s"));
            Assert.Equal("s", missing.Parameter);
            Assert.Throws<ParseException>(() => db.Update("tasks", null, """{"$bump": {"n": 1}}"""));

            var id = db.Insert("tasks", new TaskItem { Title = "a" });
            var twice = Assert.Throws<DuplicateIdException>(() => db.Insert("tasks", new TaskItem { Id = id }));
            Assert.Equal(id, twice.DocumentId);
            Assert.Equal("tasks", twice.Collection);

            var many = Assert.Throws<TrunkDbException>(() =>
            {
                db.Insert("tasks", new TaskItem { Title = "b" });
                db.Upsert("tasks", "status = \"open\"", """{"$set": {"x": 1}}""");
            });
            Assert.Equal(ErrorCode.MultipleMatches, many.Code);
            Assert.Throws<ArgumentNullException>(() => db.Count(null!));
        }

        using (var reader = Open(options: new OpenOptions { ReadOnly = true, CacheSize = 1 << 20 }))
        {
            var refused = Assert.Throws<TrunkDbException>(() => reader.Insert("tasks", new TaskItem()));
            Assert.Equal(ErrorCode.ReadOnly, refused.Code);
            Assert.Equal(2ul, reader.Count("tasks"));
        }

        var missingFile = Assert.Throws<TrunkDbException>(() => Open("none.trunkdb", new OpenOptions { ReadOnly = true }));
        Assert.Equal(ErrorCode.Io, missingFile.Code);
        System.IO.File.WriteAllText(File("text.trunkdb"), new string('x', 10000));
        Assert.Equal(ErrorCode.Format, Assert.Throws<TrunkDbException>(() => Open("text.trunkdb")).Code);
    }

    [Fact]
    public void A_closed_database_or_snapshot_is_refused_by_dotnet()
    {
        var db = Open();
        var snapshot = db.OpenSnapshot();
        snapshot.Dispose();
        Assert.Throws<ObjectDisposedException>(() => snapshot.Count("tasks"));
        db.Dispose();
        db.Dispose();
        Assert.Throws<ObjectDisposedException>(() => db.Count("tasks"));
    }

    [Fact]
    public void One_database_is_used_by_eight_threads_at_once()
    {
        using var db = Open();
        Parallel.For(0, 8, thread =>
        {
            for (var i = 0; i < 25; i++)
            {
                db.Insert("tasks", new { thread, i });
                Assert.True(db.Count("tasks", "thread = :t", new { t = thread }) >= (ulong)i + 1);
            }
        });
        Assert.Equal(200ul, db.Count("tasks"));
        // Each thread's errors are its own: the offset its text had.
        Parallel.For(0, 8, thread =>
        {
            var e = Assert.Throws<ParseException>(() => db.Count("tasks", new string(' ', thread) + "thread = = 1"));
            Assert.Equal(thread + 9, e.Offset);
        });
    }
}
