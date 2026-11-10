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

public sealed class Note
{
    public string Text { get; set; } = "";
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
    private static readonly DocId Absent = DocId.Parse("01a1bc72-c792-772e-a864-a359a7ffaeb6");

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
    public void The_wrappers_file_is_not_the_c_librarys_on_windows()
    {
        // Windows doesn't tell TrunkDb.dll from trunkdb.dll: .NET loaded the
        // wrapper where it looked for the C library, on every call.
        var file = Path.GetFileName(typeof(Database).Assembly.Location);
        Assert.NotEqual("trunkdb.dll", file, StringComparer.OrdinalIgnoreCase);
    }

    [Fact]
    public void A_document_is_inserted_changed_and_updated_as_an_object()
    {
        using var db = Open();
        var tasks = db.Collection<TaskItem>("tasks");
        Assert.Equal("tasks", tasks.Name);
        Assert.Same(db, tasks.Database);

        var task = new TaskItem { Title = "Write it", Due = new DateOnly(2026, 11, 9) };
        var id = tasks.Insert(task);
        Assert.Equal(id, task.Id);

        var mine = tasks.Get(id)!;
        mine.Status = "done";
        Assert.True(tasks.Update(mine));
        Assert.Equal("done", tasks.Get(id)!.Status);
        Assert.Equal(new DateOnly(2026, 11, 9), tasks.Get(id)!.Due);
        Assert.Equal(id, tasks.Get(id)!.Id);

        Assert.True(tasks.Delete(mine));
        Assert.False(tasks.Delete(mine));
        Assert.False(tasks.Update(mine));
        Assert.Null(tasks.Get(id));

        // An id the document has is kept, and not overwritten.
        var given = new TaskItem { Id = Absent, Title = "Mine" };
        Assert.Equal(Absent, tasks.Insert(given));
        Assert.Equal(Absent, given.Id);
        Assert.Equal("Mine", tasks.Get(Absent)!.Title);

        // By id, for a document that has none of its own.
        var notes = db.Collection<Note>("notes");
        var noteId = notes.Insert(new Note { Text = "a" });
        Assert.True(notes.Update(noteId, new Note { Text = "b" }));
        Assert.Equal("b", notes.Get(noteId)!.Text);
        var noId = Assert.Throws<InvalidOperationException>(() => notes.Update(new Note()));
        Assert.Contains("Update(id, document)", noId.Message);
        var notYet = Assert.Throws<InvalidOperationException>(() => tasks.Update(new TaskItem()));
        Assert.Contains("insert it first", notYet.Message);
        Assert.True(notes.Delete(noteId));
    }

    [Fact]
    public void Documents_are_found_counted_and_inserted_many_at_once()
    {
        using var db = Open();
        var tasks = db.Collection<TaskItem>("tasks");
        var items = new List<TaskItem>
        {
            new() { Title = "Write it", Due = new DateOnly(2026, 11, 9), Hours = 2 },
            new() { Title = "Test it", Status = "done" },
        };
        var ids = tasks.InsertMany(items);
        Assert.Equal(ids, items.Select(t => t.Id));
        Assert.Empty(tasks.InsertMany([]));

        Assert.Equal(["Write it"], tasks.Find("status = :s", new { s = "open" }).Select(t => t.Title));
        Assert.Equal(2, tasks.Find().Count);
        Assert.Equal("Test it", tasks.FindOne("status = \"done\"")!.Title);
        Assert.Null(tasks.FindOne("hours > 10"));
        Assert.Equal(1ul, tasks.Count("due = :d", new { d = new DateOnly(2026, 11, 9) }));
        Assert.Equal(0ul, db.Collection<TaskItem>("nothing").Count());

        // All or none: an id the collection has stops every insert.
        var again = new List<TaskItem> { new() { Title = "new" }, new() { Id = ids[0], Title = "twice" } };
        Assert.Throws<DuplicateIdException>(() => tasks.InsertMany(again));
        Assert.Equal(2ul, tasks.Count());
        Assert.True(again[0].Id.IsEmpty);
    }

    /// <summary>Renames every property: <c>Id</c> is <c>f_Id</c>.</summary>
    private sealed class Prefixed : JsonNamingPolicy
    {
        public override string ConvertName(string name) => "f_" + name;
    }

    public sealed class Named
    {
        [System.Text.Json.Serialization.JsonPropertyName("ident")]
        public DocId Id { get; set; }
        public string Title { get; set; } = "";
    }

    [Fact]
    public void Options_of_the_programs_own_change_neither_ids_nor_batches()
    {
        // Indented: a document over several lines, which a batch's line can't be.
        var json = new JsonSerializerOptions(JsonSerializerDefaults.Web) { WriteIndented = true, PropertyNamingPolicy = new Prefixed() };
        using var db = Open(options: new OpenOptions { Json = json });
        var tasks = db.Collection<TaskItem>("tasks");
        var items = new List<TaskItem> { new() { Title = "a" }, new() { Title = "b" } };
        var ids = tasks.InsertMany(items);
        Assert.Equal(ids, items.Select(t => t.Id));

        // A policy renaming Id leaves it the document's id.
        var task = new TaskItem { Title = "c" };
        var id = tasks.Insert(task);
        Assert.Equal(id, task.Id);
        task.Status = "done";
        Assert.True(tasks.Update(task));
        var raw = db.Collection<JsonObject>("tasks").Get(id)!;
        Assert.Equal("done", raw["f_Status"]!.GetValue<string>());
        Assert.False(raw.ContainsKey("f_Id"));
        Assert.Equal(id, tasks.Get(id)!.Id);

        // A name of its own is kept: an ordinary field, no id to update by.
        var named = db.Collection<Named>("named");
        var made = named.Insert(new Named { Id = Absent, Title = "n" });
        Assert.NotEqual(Absent, made);
        Assert.Equal(Absent, named.Get(made)!.Id);
        Assert.Throws<InvalidOperationException>(() => named.Update(new Named { Id = Absent }));
    }

    [Fact]
    public void Every_tagged_kind_goes_through_the_database_and_back()
    {
        using var db = Open();
        var collection = db.Collection<Kinds>("kinds");
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
            Absent,
            long.MaxValue);
        var id = collection.Insert(kinds);

        var back = collection.Get(id)!;
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
        var raw = db.Collection<JsonElement>("kinds").Get(id);
        Assert.Equal("3.0", raw.GetProperty("whole").GetRawText());
        Assert.Equal("""{"$date":"2026-11-09"}""", raw.GetProperty("day").GetRawText());
        Assert.Equal(1ul, collection.Count("day = :d and clock > :t", new { d = kinds.Day, t = new TimeOnly(14, 0) }));
        Assert.Equal(1ul, collection.Count("at < :now", new { now = DateTime.UtcNow.AddYears(1) }));
    }

    [Fact]
    public void Untyped_documents_are_json_nodes_tags_and_all()
    {
        using var db = Open();
        var notes = db.Collection<JsonObject>("notes");
        var id = notes.Insert(JsonNode.Parse("""{"text": "hi", "when": {"$date": "2026-11-09"}}""")!.AsObject());
        var note = notes.Get(id)!;
        Assert.Equal(id.ToString(), note["_id"]!["$id"]!.GetValue<string>());
        Assert.Equal("2026-11-09", note["when"]!["$date"]!.GetValue<string>());
    }

    [Fact]
    public void Many_are_updated_and_deleted_by_a_filter_and_upserted()
    {
        using var db = Open();
        var tasks = db.Collection<TaskItem>("tasks");
        var a = new TaskItem { Title = "a", Hours = 1 };
        tasks.InsertMany([a, new TaskItem { Title = "b", Hours = 1 }]);

        Assert.Equal(2ul, tasks.UpdateMany(null, """{"$inc": {"hours": 1}}"""));
        Assert.Equal(2.0, tasks.Get(a.Id)!.Hours);
        Assert.Equal(1ul, tasks.UpdateMany("title = :t", """{"$set": {"status": "done"}}""", new { t = "b" }));
        Assert.Equal(1ul, tasks.UpdateMany("_id = :id", """{"$set": {"title": "a2"}}""", new { id = a.Id }));
        Assert.Equal("a2", tasks.Get(a.Id)!.Title);

        var visits = db.Collection<JsonObject>("visits");
        var first = visits.Upsert("page = :p", """{"$inc": {"hits": 1}}""", new { p = "/" });
        Assert.True(first.Inserted);
        var second = visits.Upsert("page = :p", """{"$inc": {"hits": 1}}""", new { p = "/" });
        Assert.False(second.Inserted);
        Assert.Equal(first.Id, second.Id);
        Assert.Equal(2, visits.Get(first.Id)!["hits"]!.GetValue<int>());

        Assert.Equal(1ul, tasks.DeleteMany("status = \"done\""));
        Assert.Equal(1ul, tasks.DeleteMany(null));
        Assert.Equal(0ul, tasks.Count());
    }

    [Fact]
    public void Aggregates_come_back_as_rows_and_explain_as_lines()
    {
        using var db = Open();
        var tasks = db.Collection<TaskItem>("tasks");
        tasks.InsertMany([
            new TaskItem { Title = "a", Hours = 1 },
            new TaskItem { Title = "b", Hours = 2 },
            new TaskItem { Title = "c", Status = "done", Hours = 4 },
        ]);

        var rows = tasks.Aggregate<Group>("hours >= :min", "count() as n, sum(hours) as hours group by status", new { min = 1 });
        Assert.Equal([new Group("done", 1, 4), new Group("open", 2, 3)], rows.OrderBy(r => r.Status));

        tasks.EnsureIndex("status");
        Assert.Contains("plan: index on", tasks.Explain("status = :s", new { s = "open" }));
    }

    [Fact]
    public void Indexes_are_made_listed_enforced_and_dropped()
    {
        using var db = Open();
        var users = db.Collection<JsonObject>("users");
        Assert.True(users.EnsureIndex("email", unique: true));
        Assert.False(users.EnsureIndex("email", unique: true));
        Assert.True(users.EnsureIndex(["team", "name"], sparse: true));
        var indexes = users.Indexes().OrderBy(i => i.Fields[0]).ToList();
        Assert.Equal(2, indexes.Count);
        Assert.Equal(["email"], indexes[0].Fields);
        Assert.True(indexes[0].Unique);
        Assert.Equal(["team", "name"], indexes[1].Fields);
        Assert.True(indexes[1].Sparse);

        var first = users.Insert(new JsonObject { ["email"] = "a@example.com" });
        var refused = Assert.Throws<DuplicateValueException>(() => users.Insert(new JsonObject { ["email"] = "a@example.com" }));
        Assert.Equal(ErrorCode.DuplicateValue, refused.Code);
        Assert.Equal("users", refused.Collection);
        Assert.Equal("email", refused.Field);
        Assert.Equal(first, refused.Existing);

        var other = Assert.Throws<TrunkDbException>(() => users.EnsureIndex("email"));
        Assert.Equal(ErrorCode.InvalidArgument, other.Code);

        Assert.True(users.DropIndex(["team", "name"]));
        Assert.False(users.DropIndex(["team", "name"]));
        Assert.Single(users.Indexes());
    }

    [Fact]
    public void A_batch_writes_all_or_nothing_and_names_the_line_that_failed()
    {
        using var db = Open();
        var log = db.Collection<JsonObject>("log");
        const string lines = """
            {"op": "insert", "collection": "log", "document": {"n": 1}}

            {"op": "update", "collection": "log", "filter": "n = :n", "update": {"$set": {"seen": true}}}
            """;
        var dry = db.Batch(lines, new { n = 1 }, dryRun: true);
        Assert.True(dry.DryRun);
        Assert.Equal(0ul, log.Count());
        Assert.All(dry.Ops, op => Assert.Null(op.Id));

        var done = db.Batch(lines, new { n = 1 });
        Assert.Equal(2ul, done.Total);
        Assert.Equal([1ul, 3ul], done.Ops.Select(op => op.Line));
        Assert.Equal(["insert", "update"], done.Ops.Select(op => op.Op));
        Assert.True(log.Get(done.Ops[0].Id!.Value)!["seen"]!.GetValue<bool>());

        var failed = Assert.Throws<PreconditionFailedException>(() => db.Batch("""
            {"op": "insert", "collection": "log", "document": {"n": 2}}
            {"op": "require_none", "collection": "log", "filter": "n = 1"}
            """));
        Assert.Equal(2, failed.Line);
        Assert.Equal(1, failed.Op);
        Assert.True(failed.Matched);
        Assert.Equal("log", failed.Collection);
        Assert.Equal(1ul, log.Count());

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
        var tasks = db.Collection<TaskItem>("tasks");
        tasks.InsertMany(Enumerable.Range(0, 300).Select(i => new TaskItem { Title = $"t{i}", Hours = i }));
        tasks.DeleteMany("hours >= 10");

        using (var snapshot = db.OpenSnapshot())
        {
            var then = snapshot.Collection<TaskItem>("tasks");
            tasks.DeleteMany("hours >= 5");
            Assert.Equal(5ul, tasks.Count());
            Assert.Equal(10ul, then.Count());
            Assert.Equal(10, then.Find().Count);
            Assert.Equal(9.0, then.FindOne("hours = 9")!.Hours);
            Assert.Equal("t9", then.Get(then.FindOne("hours = 9")!.Id)!.Title);
            Assert.Equal(10, then.Aggregate<JsonObject>(null, "count() as n").Single()["n"]!.GetValue<int>());
            Assert.Contains("plan:", then.Explain());

            using var cursor = then.OpenCursor("hours >= :h order by hours", new { h = 3 });
            Assert.Equal([3.0, 4, 5, 6, 7, 8, 9], cursor.Select(t => t.Hours));
            Assert.Throws<InvalidOperationException>(() => cursor.First());

            var refused = Assert.Throws<TrunkDbException>(() => db.Compact());
            Assert.Equal(ErrorCode.SnapshotOpen, refused.Code);
        }
        var compacted = db.Compact();
        Assert.True(compacted.PagesAfter < compacted.PagesBefore);

        // Left before its end, a cursor is closed by Dispose.
        using var live = tasks.OpenCursor();
        Assert.Equal("t0", live.First().Title);
    }

    [Fact]
    public void The_database_is_checked_measured_exported_and_imported()
    {
        using var db = Open();
        var tasks = db.Collection<TaskItem>("tasks");
        tasks.EnsureIndex("status");
        tasks.Insert(new TaskItem { Title = "a" });
        db.Collection<Note>("notes").Insert(new Note { Text = "n" });
        db.Checkpoint();

        var check = db.Check();
        Assert.True(check.Ok);
        Assert.Empty(check.Problems);
        Assert.Equal(2ul, check.Documents);

        var stats = db.Stats();
        Assert.True(stats.File.Pages > 0);
        Assert.Equal(Database.LibraryVersion, stats.File.WrittenBy);
        var counted = Assert.Single(stats.Collections, c => c.Name == "tasks");
        Assert.Equal(1ul, counted.Documents);
        Assert.Equal(["status"], Assert.Single(counted.Indexes).Fields);

        Assert.Equal(["notes", "tasks"], db.Collections().Order());
        Assert.Equal(new TransferSummary(2, 2, true), db.Export(File("backup.jsonl")));

        using var copy = Open("copy.trunkdb");
        Assert.Equal(new TransferSummary(2, 2, true), copy.Import(File("backup.jsonl")));
        Assert.Equal("a", copy.Collection<TaskItem>("tasks").FindOne()!.Title);
        Assert.True(copy.Collection<Note>("notes").Drop());
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
            var tasks = db.Collection<TaskItem>("tasks");
            var filter = Assert.Throws<ParseException>(() => tasks.Count("hours = = 1"));
            Assert.Equal(ErrorCode.Parse, filter.Code);
            Assert.Equal("filter", filter.What);
            Assert.Equal(8, filter.Offset);
            Assert.Equal("parse", filter.Detail.GetProperty("kind").GetString());

            var unused = Assert.Throws<ParseException>(() => tasks.Find("status = :s", new { s = "open", t = 1 }));
            Assert.Equal("t", unused.Parameter);
            var missing = Assert.Throws<ParseException>(() => tasks.Find("status = :s"));
            Assert.Equal("s", missing.Parameter);
            Assert.Throws<ParseException>(() => tasks.UpdateMany(null, """{"$bump": {"n": 1}}"""));

            var id = tasks.Insert(new TaskItem { Title = "a" });
            var twice = Assert.Throws<DuplicateIdException>(() => tasks.Insert(new TaskItem { Id = id }));
            Assert.Equal(id, twice.DocumentId);
            Assert.Equal("tasks", twice.Collection);

            tasks.Insert(new TaskItem { Title = "b" });
            var many = Assert.Throws<TrunkDbException>(() => tasks.Upsert("status = \"open\"", """{"$set": {"x": 1}}"""));
            Assert.Equal(ErrorCode.MultipleMatches, many.Code);
            Assert.Throws<ArgumentNullException>(() => db.Collection<TaskItem>(null!));
            Assert.Throws<ArgumentNullException>(() => tasks.Insert(null!));
        }

        using (var reader = Open(options: new OpenOptions { ReadOnly = true, CacheSize = 1 << 20 }))
        {
            var tasks = reader.Collection<TaskItem>("tasks");
            var refused = Assert.Throws<TrunkDbException>(() => tasks.Insert(new TaskItem()));
            Assert.Equal(ErrorCode.ReadOnly, refused.Code);
            Assert.Equal(2ul, tasks.Count());
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
        var tasks = db.Collection<TaskItem>("tasks");
        var snapshot = db.OpenSnapshot();
        var then = snapshot.Collection<TaskItem>("tasks");
        snapshot.Dispose();
        Assert.Throws<ObjectDisposedException>(() => then.Count());
        db.Dispose();
        db.Dispose();
        Assert.Throws<ObjectDisposedException>(() => tasks.Count());
    }

    [Fact]
    public void One_database_is_used_by_eight_threads_at_once()
    {
        using var db = Open();
        var tasks = db.Collection<JsonObject>("tasks");
        Parallel.For(0, 8, thread =>
        {
            for (var i = 0; i < 25; i++)
            {
                tasks.Insert(new JsonObject { ["thread"] = thread, ["i"] = i });
                Assert.True(tasks.Count("thread = :t", new { t = thread }) >= (ulong)i + 1);
            }
        });
        Assert.Equal(200ul, tasks.Count());
        // Each thread's errors are its own: the offset its text had.
        Parallel.For(0, 8, thread =>
        {
            var e = Assert.Throws<ParseException>(() => tasks.Count(new string(' ', thread) + "thread = = 1"));
            Assert.Equal(thread + 9, e.Offset);
        });
    }

    [Fact]
    public void Text_with_a_nul_is_refused_before_it_reaches_the_library()
    {
        // The library reads up to the first NUL: "status = \"open\"" alone
        // would delete every open task, and "tasks" would be another
        // collection's name (SPEC §184.1).
        using var db = Open();
        var tasks = db.Collection<TaskItem>("tasks");
        tasks.Insert(new TaskItem { Title = "a" });
        tasks.Insert(new TaskItem { Title = "b" });
        db.Collection<TaskItem>("tasks2").Insert(new TaskItem { Title = "c" });

        void Refused(Action call)
        {
            var e = Assert.Throws<ArgumentException>(call);
            Assert.Contains("NUL", e.Message);
        }
        Refused(() => tasks.DeleteMany("status = \"open\"\0 and title = \"a\""));
        Refused(() => tasks.UpdateMany("title = \"a\"\0 or true", """{"$set": {"status": "done"}}"""));
        Refused(() => db.Collection<TaskItem>("tasks\0 2").DeleteMany(null));
        Refused(() => db.DropCollection("tasks2\0"));
        Refused(() => db.Export(File("backup.jsonl\0.trunkdb")));
        Refused(() => Database.Open(File("app.trunkdb\0x")));
        Refused(() => db.Batch("{\"op\": \"delete_many\", \"collection\": \"tasks\"}\0"));

        Assert.Equal(2ul, tasks.Count());
        Assert.Equal(0ul, tasks.Count("status = \"done\""));
        Assert.Equal(1ul, db.Collection<TaskItem>("tasks2").Count());
        Assert.False(System.IO.File.Exists(File("backup.jsonl")));
        // A NUL inside a value given as a parameter is kept, as JSON escapes it.
        Assert.Equal(0ul, tasks.Count("title = :t", new { t = "a\0" }));
    }
}
