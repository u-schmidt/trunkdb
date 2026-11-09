using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization.Metadata;

namespace TrunkDb;

/// <summary>
/// The reads of a collection, as the database is now or as a
/// <see cref="Snapshot"/> was taken, its documents as <typeparamref name="T"/>.
/// A filter is trunkdb's text, <c>status = :s order by due limit 10</c>,
/// null for every document; its parameters' values an object,
/// <c>new { s = "open" }</c>.
/// </summary>
public class CollectionView<T>
{
    private readonly SnapshotHandle at;

    internal CollectionView(Database database, SnapshotHandle at, string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        Database = database;
        this.at = at;
        Name = name;
    }

    /// <summary>The database the collection is in.</summary>
    public Database Database { get; }

    /// <summary>The collection's name.</summary>
    public string Name { get; }

    /// <summary>The document with <paramref name="id"/>, or null.</summary>
    public T? Get(DocId id)
    {
        Native.Check(Native.trunkdb_get(Database.Handle, at, Name, id.ToString(), out var document));
        return Database.Read<T>(Native.Take(document));
    }

    /// <summary>The documents <paramref name="filter"/> matches, in its order.</summary>
    public List<T> Find(string? filter = null, object? parameters = null)
    {
        Native.Check(Native.trunkdb_find(Database.Handle, at, Name, filter, Database.Params(parameters), out var documents));
        return Database.Read<List<T>>(Native.TakeText(documents))!;
    }

    /// <summary>The first document <paramref name="filter"/> matches, or null.</summary>
    public T? FindOne(string? filter = null, object? parameters = null)
    {
        Native.Check(Native.trunkdb_find_one(Database.Handle, at, Name, filter, Database.Params(parameters), out var document));
        return Database.Read<T>(Native.Take(document));
    }

    /// <summary>How many documents <paramref name="filter"/> matches; 0 if the collection isn't there.</summary>
    public ulong Count(string? filter = null, object? parameters = null)
    {
        Native.Check(Native.trunkdb_count(Database.Handle, at, Name, filter, Database.Params(parameters), out var count));
        return count;
    }

    /// <summary>
    /// The rows of <paramref name="aggregate"/>, <c>count() as n group by
    /// status</c>, over the documents <paramref name="filter"/> matches.
    /// The values are for both texts' parameters.
    /// </summary>
    public List<TRow> Aggregate<TRow>(string? filter, string aggregate, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(aggregate);
        Native.Check(Native.trunkdb_aggregate(
            Database.Handle, at, Name, filter, aggregate, Database.Params(parameters), out var rows));
        return Database.Read<List<TRow>>(Native.TakeText(rows))!;
    }

    /// <summary>How <paramref name="filter"/> would be read: its plan, as <c>trunkdb find --explain</c> prints it.</summary>
    public string Explain(string? filter = null, object? parameters = null)
    {
        Native.Check(Native.trunkdb_explain(Database.Handle, at, Name, filter, Database.Params(parameters), out var explained));
        return Native.TakeText(explained);
    }

    /// <summary>
    /// The documents <paramref name="filter"/> matches, one at a time, as
    /// they are when it opens: for more than fit in memory at once. Read
    /// once; a <c>foreach</c> to its end closes it, and so does
    /// <see cref="Cursor{T}.Dispose"/>.
    /// </summary>
    public Cursor<T> OpenCursor(string? filter = null, object? parameters = null)
    {
        Native.Check(Native.trunkdb_cursor_open(
            Database.Handle, at, Name, filter, Database.Params(parameters), out var cursor));
        return new Cursor<T>(cursor, Database.JsonOptions);
    }
}

/// <summary>
/// A collection of the database, its documents as <typeparamref name="T"/>
/// (SPEC §180): reads, writes and indexes. Made by
/// <see cref="Database.Collection{T}"/>, as cheap as a name; a collection
/// is there once a document is.
/// <code>
/// var tasks = db.Collection&lt;TaskItem&gt;("tasks");
/// var task = new TaskItem { Title = "Write it" };
/// tasks.Insert(task);            // task.Id is set
/// task.Status = "done";
/// tasks.Update(task);
/// </code>
/// A property <c>Id</c> of type <see cref="DocId"/> is the document's id.
/// </summary>
public sealed class Collection<T> : CollectionView<T>
{
    private readonly JsonPropertyInfo? id;

    internal Collection(Database database, string name) : base(database, SnapshotHandle.None, name)
    {
        var info = database.JsonOptions.GetTypeInfo(typeof(T));
        id = info.Kind == JsonTypeInfoKind.Object
            ? info.Properties.FirstOrDefault(p => p.Name == "_id" && (p.PropertyType == typeof(DocId) || p.PropertyType == typeof(DocId?)))
            : null;
    }

    /// <summary>
    /// Inserts <paramref name="document"/>; its id, the one it has or a new
    /// one, also set in its <c>Id</c> if that was empty.
    /// </summary>
    /// <exception cref="DuplicateIdException">The collection has its id.</exception>
    /// <exception cref="DuplicateValueException">A unique index has one of its values.</exception>
    public DocId Insert(T document)
    {
        Native.Check(Native.trunkdb_insert(Database.Handle, Name, Database.Write(document), out var made));
        var given = DocId.Parse(Native.TakeText(made));
        if (id?.Set is { } set && !typeof(T).IsValueType && IdOf(document).IsEmpty)
        {
            set(document!, given);
        }
        return given;
    }

    /// <summary>Inserts each of <paramref name="documents"/>, in one batch: all of them or none.</summary>
    public IReadOnlyList<DocId> InsertMany(IEnumerable<T> documents)
    {
        ArgumentNullException.ThrowIfNull(documents);
        var all = documents.ToList();
        if (all.Count == 0)
        {
            return [];
        }
        // One op a line, whatever the program's options indent.
        var lines = string.Join('\n', all.Select(d => new JsonObject
        {
            ["op"] = "insert",
            ["collection"] = Name,
            ["document"] = JsonNode.Parse(Database.Write(d)),
        }.ToJsonString()));
        var done = Database.Batch(lines);
        var ids = done.Ops.Select(op => op.Id!.Value).ToList();
        if (id?.Set is { } set && !typeof(T).IsValueType)
        {
            for (var i = 0; i < all.Count; i++)
            {
                if (IdOf(all[i]).IsEmpty)
                {
                    set(all[i]!, ids[i]);
                }
            }
        }
        return ids;
    }

    /// <summary>Replaces the stored document with <paramref name="document"/>, found by its <c>Id</c>; false if there is none.</summary>
    /// <exception cref="InvalidOperationException"><typeparamref name="T"/> has no <c>Id</c>, or it is empty.</exception>
    public bool Update(T document) => Update(RequireId(document), document);

    /// <summary>Replaces the document with <paramref name="id"/> by <paramref name="document"/> whole; false if there is none.</summary>
    public bool Update(DocId id, T document)
    {
        Native.Check(Native.trunkdb_replace(Database.Handle, Name, id.ToString(), Database.Write(document), out var found));
        return found;
    }

    /// <summary>Deletes the document with <paramref name="id"/>; false if there is none.</summary>
    public bool Delete(DocId id)
    {
        Native.Check(Native.trunkdb_delete(Database.Handle, Name, id.ToString(), out var found));
        return found;
    }

    /// <summary>Deletes <paramref name="document"/>, found by its <c>Id</c>; false if it isn't there.</summary>
    /// <exception cref="InvalidOperationException"><typeparamref name="T"/> has no <c>Id</c>, or it is empty.</exception>
    public bool Delete(T document) => Delete(RequireId(document));

    /// <summary>
    /// Applies <paramref name="update"/>, MongoDB's operators as JSON,
    /// <c>{"$set": {"status": "done"}}</c>, to every document
    /// <paramref name="filter"/> matches, null for all; how many changed.
    /// </summary>
    public ulong UpdateMany(string? filter, string update, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(update);
        Native.Check(Native.trunkdb_update(Database.Handle, Name, filter, Database.Params(parameters), update, out var count));
        return count;
    }

    /// <summary>Deletes every document <paramref name="filter"/> matches, null for all; how many.</summary>
    public ulong DeleteMany(string? filter, object? parameters = null)
    {
        Native.Check(Native.trunkdb_delete_many(Database.Handle, Name, filter, Database.Params(parameters), out var count));
        return count;
    }

    /// <summary>
    /// Applies <paramref name="update"/> to the one document
    /// <paramref name="filter"/> matches, or inserts one made from the
    /// filter's equalities and the update.
    /// </summary>
    /// <exception cref="TrunkDbException">More than one matches (<see cref="ErrorCode.MultipleMatches"/>).</exception>
    public Upserted Upsert(string filter, string update, object? parameters = null)
    {
        ArgumentNullException.ThrowIfNull(filter);
        ArgumentNullException.ThrowIfNull(update);
        Native.Check(Native.trunkdb_upsert(
            Database.Handle, Name, filter, Database.Params(parameters), update, out var made, out var inserted));
        return new Upserted(DocId.Parse(Native.TakeText(made)), inserted);
    }

    /// <summary>Makes an index on <paramref name="field"/> if there is none; true if it made one.</summary>
    public bool EnsureIndex(string field, bool unique = false, bool sparse = false) =>
        EnsureIndex([field], unique, sparse);

    /// <summary>Makes an index on <paramref name="fields"/>, in their order, if there is none; true if it made one.</summary>
    /// <exception cref="TrunkDbException">There is one on them of other options (<see cref="ErrorCode.InvalidArgument"/>).</exception>
    /// <exception cref="DuplicateValueException">It is unique, and two documents have one value.</exception>
    public bool EnsureIndex(IReadOnlyList<string> fields, bool unique = false, bool sparse = false)
    {
        ArgumentNullException.ThrowIfNull(fields);
        var options = $$"""{"unique":{{(unique ? "true" : "false")}},"sparse":{{(sparse ? "true" : "false")}}}""";
        Native.Check(Native.trunkdb_index_ensure(Database.Handle, Name, Fields(fields), options, out var created));
        return created;
    }

    /// <summary>Drops the index on <paramref name="field"/>; false if there is none.</summary>
    public bool DropIndex(string field) => DropIndex([field]);

    /// <summary>Drops the index on <paramref name="fields"/>; false if there is none.</summary>
    public bool DropIndex(IReadOnlyList<string> fields)
    {
        ArgumentNullException.ThrowIfNull(fields);
        Native.Check(Native.trunkdb_index_drop(Database.Handle, Name, Fields(fields), out var dropped));
        return dropped;
    }

    /// <summary>The collection's indexes.</summary>
    public IReadOnlyList<IndexInfo> Indexes()
    {
        Native.Check(Native.trunkdb_indexes(Database.Handle, Name, out var indexes));
        return Database.Report<List<IndexInfo>>(indexes);
    }

    /// <summary>Drops the collection, its documents and indexes; false if it isn't there.</summary>
    public bool Drop() => Database.DropCollection(Name);

    private static string Fields(IReadOnlyList<string> fields) => JsonSerializer.Serialize(fields);

    private DocId IdOf(T document) => id?.Get?.Invoke(document!) is DocId given ? given : default;

    private DocId RequireId(T document)
    {
        ArgumentNullException.ThrowIfNull(document);
        if (id is null)
        {
            throw new InvalidOperationException(
                $"{typeof(T).Name} has no property Id of type DocId: pass the id, Update(id, document)");
        }
        var given = IdOf(document);
        return given.IsEmpty
            ? throw new InvalidOperationException($"the {typeof(T).Name} has no id yet: insert it first")
            : given;
    }
}

/// <summary>
/// The database as it was when the snapshot was taken (SPEC §77): every
/// read of it sees that, whatever is written since. Dispose it when done:
/// an open snapshot holds what was overwritten since, and a compaction
/// waits for it. Its database must stay open while it is read.
/// </summary>
public sealed class Snapshot : IDisposable
{
    private readonly Database database;
    private readonly SnapshotHandle handle;

    internal Snapshot(Database database, SnapshotHandle handle)
    {
        this.database = database;
        this.handle = handle;
    }

    /// <summary>The collection <paramref name="name"/> as it was, its documents as <typeparamref name="T"/>.</summary>
    public CollectionView<T> Collection<T>(string name) => new(database, handle, name);

    /// <summary>Closes the snapshot.</summary>
    public void Dispose() => handle.Dispose();
}

/// <summary>
/// Documents read one at a time (SPEC §80), as they were when the cursor
/// opened. Read once; disposed at the end of a <c>foreach</c>, or by
/// <see cref="Dispose"/> when left before it.
/// </summary>
public sealed class Cursor<T> : IEnumerable<T>, IDisposable
{
    private readonly CursorHandle handle;
    private readonly JsonSerializerOptions json;
    private bool started;

    internal Cursor(CursorHandle handle, JsonSerializerOptions json)
    {
        this.handle = handle;
        this.json = json;
    }

    /// <summary>The documents; a second enumeration is refused.</summary>
    /// <exception cref="InvalidOperationException">The cursor was read already.</exception>
    public IEnumerator<T> GetEnumerator()
    {
        if (started)
        {
            throw new InvalidOperationException("a cursor is read once: open another");
        }
        started = true;
        return Documents();
    }

    System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();

    private IEnumerator<T> Documents()
    {
        try
        {
            while (true)
            {
                Native.Check(Native.trunkdb_cursor_next(handle, out var document));
                if (Native.Take(document) is not { } text)
                {
                    yield break;
                }
                yield return JsonSerializer.Deserialize<T>(text, json)!;
            }
        }
        finally
        {
            handle.Dispose();
        }
    }

    /// <summary>Closes the cursor.</summary>
    public void Dispose() => handle.Dispose();
}
